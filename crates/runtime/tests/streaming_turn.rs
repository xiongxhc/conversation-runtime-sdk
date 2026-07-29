use std::sync::{Arc, Mutex};
use std::time::Duration;

use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioFrame, ContinuousAudioOutput, MockContinuousAudioOutput,
    MockGenerationLanguageModel, MockStreamingSpeechSynthesizer, PcmFormat, PcmSampleFormat,
    PlaybackReceipt, StreamingSpeechRequest, StreamingSpeechSynthesizer,
};
use conversation_protocol::{
    GenerationId, PlaybackState, RuntimeEvent, RuntimeTimingMilestone, SessionId, TurnId,
    UtteranceId,
};
use conversation_runtime::{StreamingTurnEventStream, StreamingTurnRuntime, UtteranceAssembler};
use tokio::sync::{mpsc, watch, Notify};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[test]
fn short_answer_is_one_utterance() {
    let mut assembler = UtteranceAssembler::default();

    assert!(assembler.push_delta("第一句。第二句。").is_empty());
    assert_eq!(assembler.finish().as_deref(), Some("第一句。第二句。"));
}

#[test]
fn long_answer_prefers_paragraph_boundaries_before_hard_limit() {
    let mut assembler = UtteranceAssembler::new(24, 48).unwrap();
    let emitted = assembler.push_delta("第一段足够长。\n\n第二段也足够长。\n\n第三段");

    assert_eq!(emitted, vec!["第一段足够长。\n\n"]);
    assert!(emitted[0].len() <= 48);
}

#[test]
fn hard_limit_splits_only_at_utf8_boundaries() {
    let mut assembler = UtteranceAssembler::new(6, 9).unwrap();

    assert_eq!(assembler.push_delta("你好世界"), vec!["你好世"]);
    assert_eq!(assembler.finish().as_deref(), Some("界"));
}

#[test]
fn safe_phrase_boundary_is_preferred_to_a_hard_split() {
    let mut assembler = UtteranceAssembler::new(8, 16).unwrap();

    assert_eq!(
        assembler.push_delta("alpha, beta gamma"),
        vec!["alpha, beta "]
    );
    assert_eq!(assembler.finish().as_deref(), Some("gamma"));
}

#[test]
fn default_assembler_uses_r3_limits() {
    let assembler = UtteranceAssembler::default();

    assert_eq!(assembler.soft_limit_bytes(), 384);
    assert_eq!(assembler.hard_limit_bytes(), 1_024);
}

#[tokio::test]
async fn streaming_turn_publishes_original_text_and_enqueues_ordered_frames() {
    let turn_id = TurnId::new(7);
    let generation_id = GenerationId::new(8);
    let utterance_id = UtteranceId::new(1);
    let format = PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    let expected_frames = vec![
        frame(turn_id, generation_id, utterance_id, 0, format),
        frame(turn_id, generation_id, utterance_id, 1, format),
    ];
    let language = Arc::new(MockGenerationLanguageModel::new(["hello", " world"]));
    let speech = Arc::new(MockStreamingSpeechSynthesizer::new(expected_frames.clone()));
    let output = Arc::new(MockContinuousAudioOutput::new());
    let runtime = StreamingTurnRuntime::new(language.clone(), speech.clone(), output.clone());

    let mut stream = runtime
        .start_turn(turn_id, generation_id, "question")
        .await
        .unwrap();
    let observed = drain(&mut stream).await;

    assert_eq!(
        observed
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::TextDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec!["hello", " world"]
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    RuntimeEvent::Timing {
                        milestone: RuntimeTimingMilestone::FirstPlayableAudio,
                        ..
                    }
                )
            })
            .count(),
        1
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| event.is_terminal())
            .collect::<Vec<_>>(),
        vec![&RuntimeEvent::TurnCompleted { turn_id }]
    );
    assert_eq!(output.frames(), expected_frames);
    assert_eq!(language.requests().len(), 1);
    assert_eq!(speech.requests().len(), 1);
    assert_eq!(speech.requests()[0].utterance_id(), utterance_id);
    assert_eq!(speech.requests()[0].text(), "hello world");
}

#[tokio::test]
async fn speech_normalization_runs_after_utterance_boundary_selection() {
    let turn_id = TurnId::new(2);
    let generation_id = GenerationId::new(3);
    let language = Arc::new(MockGenerationLanguageModel::new([
        "# Heading\n\n",
        "This is **important**.",
    ]));
    let speech = Arc::new(MockStreamingSpeechSynthesizer::new([frame(
        turn_id,
        generation_id,
        UtteranceId::new(1),
        0,
        PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
    )]));
    let output = Arc::new(MockContinuousAudioOutput::new());
    let runtime = StreamingTurnRuntime::new(language, speech.clone(), output);

    let mut stream = runtime
        .start_turn(turn_id, generation_id, "question")
        .await
        .unwrap();
    let observed = drain(&mut stream).await;

    assert!(observed.contains(&RuntimeEvent::TextDelta {
        turn_id,
        delta: "# Heading\n\n".into(),
    }));
    assert!(observed.contains(&RuntimeEvent::TextDelta {
        turn_id,
        delta: "This is **important**.".into(),
    }));
    assert_eq!(speech.requests().len(), 1);
    assert_eq!(speech.requests()[0].text(), "Heading. This is important.");
}

#[tokio::test(flavor = "current_thread")]
async fn first_playable_publication_precedes_frame_enqueue_under_backpressure() {
    let turn_id = TurnId::new(9);
    let generation_id = GenerationId::new(10);
    let format = PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    let expected_frames = vec![
        frame(turn_id, generation_id, UtteranceId::new(1), 0, format),
        frame(turn_id, generation_id, UtteranceId::new(1), 1, format),
    ];
    let language = Arc::new(MockGenerationLanguageModel::new(std::iter::repeat_n(
        "x", 27,
    )));
    let first_frame_validated = Arc::new(Notify::new());
    let speech = Arc::new(FirstPlayableProbeSpeech {
        frames: expected_frames.clone(),
        first_frame_validated: Arc::clone(&first_frame_validated),
    });
    let (enqueue_started_sender, mut enqueue_started_receiver) = mpsc::unbounded_channel();
    let (release_sender, release_receiver) = watch::channel(false);
    let output = Arc::new(GatedOrderingOutput {
        enqueue_started: enqueue_started_sender,
        first_enqueue_release: release_receiver,
        accepted_sequences: Mutex::new(Vec::new()),
    });
    let runtime = StreamingTurnRuntime::new(language, speech, output.clone());
    let mut stream = runtime
        .start_turn(turn_id, generation_id, "question")
        .await
        .unwrap();

    timeout(Duration::from_secs(1), first_frame_validated.notified())
        .await
        .expect("worker never validated the first frame");
    assert!(
        enqueue_started_receiver.try_recv().is_err(),
        "frame enqueue ran while FirstPlayableAudio publication was blocked"
    );

    let mut observed = vec![stream.recv().await.expect("lifecycle stream closed early")];
    let first_enqueued_sequence = timeout(Duration::from_secs(1), enqueue_started_receiver.recv())
        .await
        .expect("enqueue did not start after lifecycle capacity was released")
        .expect("enqueue probe closed");
    assert_eq!(first_enqueued_sequence, 0);

    while !observed.iter().any(|event| {
        matches!(
            event,
            RuntimeEvent::Timing {
                milestone: RuntimeTimingMilestone::FirstPlayableAudio,
                ..
            }
        )
    }) {
        observed.push(
            stream
                .recv()
                .await
                .expect("stream closed before FirstPlayableAudio was observed"),
        );
    }
    assert!(
        output
            .accepted_sequences
            .lock()
            .expect("accepted sequence lock poisoned")
            .is_empty(),
        "first enqueue completed before the retained release"
    );

    release_sender
        .send(true)
        .expect("first enqueue release receiver dropped");
    observed.extend(drain(&mut stream).await);

    assert_eq!(
        output
            .accepted_sequences
            .lock()
            .expect("accepted sequence lock poisoned")
            .as_slice(),
        &[0, 1]
    );
    assert_eq!(
        observed
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    RuntimeEvent::Timing {
                        milestone: RuntimeTimingMilestone::FirstPlayableAudio,
                        ..
                    }
                )
            })
            .count(),
        1
    );
}

struct FirstPlayableProbeSpeech {
    frames: Vec<AudioFrame>,
    first_frame_validated: Arc<Notify>,
}

impl StreamingSpeechSynthesizer for FirstPlayableProbeSpeech {
    fn stream(
        &self,
        _request: StreamingSpeechRequest,
        _cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<AudioFrame, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        sender
            .try_send(Ok(self.frames[0].clone()))
            .expect("first frame fits the empty probe channel");
        let second_frame = self.frames[1].clone();
        let first_frame_validated = Arc::clone(&self.first_frame_validated);
        tokio::spawn(async move {
            if sender.send(Ok(second_frame)).await.is_ok() {
                first_frame_validated.notify_one();
            }
        });
        receiver
    }
}

struct GatedOrderingOutput {
    enqueue_started: mpsc::UnboundedSender<u64>,
    first_enqueue_release: watch::Receiver<bool>,
    accepted_sequences: Mutex<Vec<u64>>,
}

impl ContinuousAudioOutput for GatedOrderingOutput {
    fn enqueue<'a>(
        &'a self,
        frame: AudioFrame,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, PlaybackReceipt> {
        Box::pin(async move {
            self.enqueue_started
                .send(frame.sequence())
                .map_err(|_| AdapterError::new("enqueue probe closed"))?;
            if frame.sequence() == 0 {
                let mut release = self.first_enqueue_release.clone();
                while !*release.borrow() {
                    release
                        .changed()
                        .await
                        .map_err(|_| AdapterError::new("enqueue release dropped"))?;
                }
            }
            self.accepted_sequences
                .lock()
                .expect("accepted sequence lock poisoned")
                .push(frame.sequence());
            Ok(PlaybackReceipt::new(
                frame.generation_id(),
                PlaybackState::Accepted,
            ))
        })
    }

    fn flush<'a>(
        &'a self,
        _session_id: SessionId,
        generation_id: GenerationId,
    ) -> AdapterFuture<'a, PlaybackReceipt> {
        Box::pin(async move { Ok(PlaybackReceipt::new(generation_id, PlaybackState::Flushed)) })
    }
}

fn frame(
    turn_id: TurnId,
    generation_id: GenerationId,
    utterance_id: UtteranceId,
    sequence: u64,
    format: PcmFormat,
) -> AudioFrame {
    AudioFrame::new(
        turn_id,
        generation_id,
        utterance_id,
        sequence,
        format,
        vec![0; 960],
    )
    .unwrap()
}

async fn drain(stream: &mut StreamingTurnEventStream) -> Vec<RuntimeEvent> {
    let mut observed = Vec::new();
    while let Some(event) = stream.recv().await {
        observed.push(event);
    }
    observed
}
