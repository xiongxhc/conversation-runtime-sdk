use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioFrame, ContinuousAudioOutput, GenerationLanguageModel,
    GenerationLanguageRequest, GenerationTextDelta, MockContinuousAudioOutput,
    MockGenerationLanguageModel, MockStreamingSpeechSynthesizer, PcmFormat, PcmSampleFormat,
    PlaybackReceipt, StreamingSpeechRequest, StreamingSpeechSynthesizer,
};
use conversation_protocol::{
    GenerationId, PlaybackState, RuntimeEvent, RuntimeStage, SessionId, TurnId, UtteranceId,
};
use conversation_runtime::{StreamingTurnEventStream, StreamingTurnRuntime};
use tokio::sync::{mpsc, watch, Notify};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn wrong_tagged_language_delta_fails_before_text_publication() {
    let turn_id = TurnId::new(1);
    let generation_id = GenerationId::new(2);
    let language = Arc::new(TaggedLanguage::new([GenerationTextDelta::new(
        turn_id,
        GenerationId::new(99),
        "wrong generation",
    )]));
    let runtime = StreamingTurnRuntime::new(
        language,
        Arc::new(MockStreamingSpeechSynthesizer::new([])),
        Arc::new(MockContinuousAudioOutput::new()),
    );

    let mut stream = runtime
        .start_turn(turn_id, generation_id, "question")
        .await
        .unwrap();
    let observed = drain(&mut stream).await;

    assert!(!observed.iter().any(|event| {
        matches!(
            event,
            RuntimeEvent::TextDelta { delta, .. } if delta == "wrong generation"
        )
    }));
    assert_failed_at(&observed, RuntimeStage::LanguageModel);
}

#[tokio::test]
async fn wrong_frame_turn_generation_or_utterance_fails_before_enqueue() {
    let turn_id = TurnId::new(10);
    let generation_id = GenerationId::new(20);
    let utterance_id = UtteranceId::new(1);
    let format = pcm_format();
    let cases = [
        frame(TurnId::new(11), generation_id, utterance_id, 0, format),
        frame(turn_id, GenerationId::new(21), utterance_id, 0, format),
        frame(turn_id, generation_id, UtteranceId::new(2), 0, format),
    ];

    for invalid_frame in cases {
        let output = Arc::new(MockContinuousAudioOutput::new());
        let runtime = StreamingTurnRuntime::new(
            Arc::new(MockGenerationLanguageModel::new(["answer"])),
            Arc::new(MockStreamingSpeechSynthesizer::new([invalid_frame])),
            output.clone(),
        );

        let mut stream = runtime
            .start_turn(turn_id, generation_id, "question")
            .await
            .unwrap();
        let observed = drain(&mut stream).await;

        assert!(output.frames().is_empty());
        assert!(!observed.iter().any(|event| {
            matches!(
                event,
                RuntimeEvent::Timing {
                    milestone: conversation_protocol::RuntimeTimingMilestone::FirstPlayableAudio,
                    ..
                }
            )
        }));
        assert_failed_at(&observed, RuntimeStage::SpeechSynthesizer);
    }
}

#[tokio::test]
async fn sequence_gap_fails_before_out_of_order_frame_enqueue() {
    let turn_id = TurnId::new(1);
    let generation_id = GenerationId::new(1);
    let utterance_id = UtteranceId::new(1);
    let output = Arc::new(MockContinuousAudioOutput::new());
    let first = frame(turn_id, generation_id, utterance_id, 0, pcm_format());
    let gap = frame(turn_id, generation_id, utterance_id, 2, pcm_format());
    let runtime = StreamingTurnRuntime::new(
        Arc::new(MockGenerationLanguageModel::new(["answer"])),
        Arc::new(MockStreamingSpeechSynthesizer::new([first.clone(), gap])),
        output.clone(),
    );

    let mut stream = runtime
        .start_turn(turn_id, generation_id, "question")
        .await
        .unwrap();
    let observed = drain(&mut stream).await;

    assert_eq!(output.frames(), vec![first]);
    assert_failed_at(&observed, RuntimeStage::SpeechSynthesizer);
}

#[tokio::test]
async fn format_change_fails_before_changed_frame_enqueue() {
    let turn_id = TurnId::new(1);
    let generation_id = GenerationId::new(1);
    let utterance_id = UtteranceId::new(1);
    let first_format = pcm_format();
    let changed_format = PcmFormat::new(48_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    let first = frame(turn_id, generation_id, utterance_id, 0, first_format);
    let changed = frame(turn_id, generation_id, utterance_id, 1, changed_format);
    let output = Arc::new(MockContinuousAudioOutput::new());
    let runtime = StreamingTurnRuntime::new(
        Arc::new(MockGenerationLanguageModel::new(["answer"])),
        Arc::new(MockStreamingSpeechSynthesizer::new([
            first.clone(),
            changed,
        ])),
        output.clone(),
    );

    let mut stream = runtime
        .start_turn(turn_id, generation_id, "question")
        .await
        .unwrap();
    let observed = drain(&mut stream).await;

    assert_eq!(output.frames(), vec![first]);
    assert_failed_at(&observed, RuntimeStage::SpeechSynthesizer);
}

#[tokio::test]
async fn interruption_unblocks_a_saturated_lifecycle_receiver() {
    let turn_id = TurnId::new(1);
    let generation_id = GenerationId::new(1);
    let language = Arc::new(MockGenerationLanguageModel::new(std::iter::repeat_n(
        "x", 64,
    )));
    let runtime = StreamingTurnRuntime::new(
        language.clone(),
        Arc::new(MockStreamingSpeechSynthesizer::new([])),
        Arc::new(MockContinuousAudioOutput::new()),
    );
    let mut stream = runtime
        .start_turn(turn_id, generation_id, "question")
        .await
        .unwrap();

    timeout(Duration::from_secs(1), language.wait_for_blocked_send())
        .await
        .expect("language producer never backpressured");
    timeout(
        Duration::from_secs(1),
        runtime.interrupt(turn_id, generation_id),
    )
    .await
    .expect("interrupt blocked behind lifecycle backpressure")
    .unwrap();
    let observed = timeout(Duration::from_secs(1), drain(&mut stream))
        .await
        .expect("terminal event did not escape saturated lifecycle receiver");

    assert_eq!(
        observed
            .iter()
            .filter(|event| event.is_terminal())
            .collect::<Vec<_>>(),
        vec![&RuntimeEvent::TurnCancelled { turn_id }]
    );
}

#[tokio::test]
async fn interruption_unblocks_full_media_enqueue_and_waits_for_cleanup() {
    let turn_id = TurnId::new(2);
    let generation_id = GenerationId::new(3);
    let output = Arc::new(BlockingOutput::new(false));
    let speech = Arc::new(MockStreamingSpeechSynthesizer::new([frame(
        turn_id,
        generation_id,
        UtteranceId::new(1),
        0,
        pcm_format(),
    )]));
    let runtime = StreamingTurnRuntime::new(
        Arc::new(MockGenerationLanguageModel::new(["answer"])),
        speech,
        output.clone(),
    );
    let mut stream = runtime
        .start_turn(turn_id, generation_id, "question")
        .await
        .unwrap();

    timeout(Duration::from_secs(1), output.started.notified())
        .await
        .expect("media enqueue never started");
    runtime.interrupt(turn_id, generation_id).await.unwrap();
    let observed = drain(&mut stream).await;

    assert!(output.cleanup_finished.load(Ordering::Acquire));
    assert_eq!(
        observed
            .iter()
            .filter(|event| event.is_terminal())
            .collect::<Vec<_>>(),
        vec![&RuntimeEvent::TurnCancelled { turn_id }]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn queue_interruption_awaits_speech_cleanup_before_terminal_and_reuse() {
    let first_turn = TurnId::new(20);
    let first_generation = GenerationId::new(30);
    let speech_started = Arc::new(Notify::new());
    let cleanup_started = Arc::new(Notify::new());
    let cleanup_finished = Arc::new(AtomicBool::new(false));
    let (cleanup_release_sender, cleanup_release_receiver) = watch::channel(false);
    let runtime = StreamingTurnRuntime::new(
        Arc::new(QueuePressureLanguage),
        Arc::new(LatchedCleanupSpeech {
            speech_started: Arc::clone(&speech_started),
            cleanup_started: Arc::clone(&cleanup_started),
            cleanup_finished: Arc::clone(&cleanup_finished),
            cleanup_release: cleanup_release_receiver,
        }),
        Arc::new(MockContinuousAudioOutput::new()),
    );
    let mut first = runtime
        .start_turn(first_turn, first_generation, "first")
        .await
        .unwrap();

    timeout(Duration::from_secs(1), speech_started.notified())
        .await
        .expect("speech worker never occupied the utterance queue");
    runtime
        .interrupt(first_turn, first_generation)
        .await
        .unwrap();
    timeout(Duration::from_secs(1), cleanup_started.notified())
        .await
        .expect("speech cleanup never started");

    let second_turn = TurnId::new(21);
    let second_generation = GenerationId::new(31);
    let premature_reuse = timeout(Duration::from_millis(50), async {
        loop {
            if let Ok(stream) = runtime
                .start_turn(second_turn, second_generation, "second")
                .await
            {
                break stream;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        premature_reuse.is_err(),
        "runtime became reusable before speech cleanup completed"
    );
    assert!(
        timeout(Duration::from_millis(50), drain(&mut first))
            .await
            .is_err(),
        "terminal published before speech cleanup completed"
    );
    assert!(!cleanup_finished.load(Ordering::Acquire));

    cleanup_release_sender
        .send(true)
        .expect("speech cleanup release receiver dropped");
    let observed = timeout(Duration::from_secs(1), drain(&mut first))
        .await
        .expect("terminal did not publish after speech cleanup");
    assert!(cleanup_finished.load(Ordering::Acquire));
    assert_eq!(
        observed
            .iter()
            .filter(|event| event.is_terminal())
            .collect::<Vec<_>>(),
        vec![&RuntimeEvent::TurnCancelled {
            turn_id: first_turn
        }]
    );

    let mut second = runtime
        .start_turn(second_turn, second_generation, "second")
        .await
        .expect("terminal observation must imply runtime reuse");
    runtime
        .interrupt(second_turn, second_generation)
        .await
        .unwrap();
    timeout(Duration::from_secs(1), drain(&mut second))
        .await
        .expect("second turn did not clean up");
}

#[tokio::test]
async fn dropped_consumer_cancels_owned_work_and_runtime_reuses() {
    let first_turn = TurnId::new(1);
    let first_generation = GenerationId::new(1);
    let output = Arc::new(BlockingOutput::new(false));
    let runtime = StreamingTurnRuntime::new(
        Arc::new(MockGenerationLanguageModel::new(["answer"])),
        Arc::new(MockStreamingSpeechSynthesizer::new([frame(
            first_turn,
            first_generation,
            UtteranceId::new(1),
            0,
            pcm_format(),
        )])),
        output.clone(),
    );
    let stream = runtime
        .start_turn(first_turn, first_generation, "first")
        .await
        .unwrap();

    timeout(Duration::from_secs(1), output.started.notified())
        .await
        .expect("media enqueue never started");
    drop(stream);
    timeout(Duration::from_secs(1), async {
        while !output.cleanup_finished.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropped consumer did not finish owned cleanup");

    let mut next = timeout(Duration::from_secs(1), async {
        loop {
            match runtime
                .start_turn(TurnId::new(2), GenerationId::new(2), "second")
                .await
            {
                Ok(stream) => break stream,
                Err(_) => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("runtime did not clear the dropped turn");
    runtime
        .interrupt(TurnId::new(2), GenerationId::new(2))
        .await
        .unwrap();
    let observed = drain(&mut next).await;

    assert!(observed.iter().any(RuntimeEvent::is_terminal));
}

#[tokio::test]
async fn output_flush_failure_still_cancels_and_cleans_the_turn() {
    let turn_id = TurnId::new(4);
    let generation_id = GenerationId::new(5);
    let output = Arc::new(BlockingOutput::new(true));
    let runtime = StreamingTurnRuntime::new(
        Arc::new(MockGenerationLanguageModel::new(["answer"])),
        Arc::new(MockStreamingSpeechSynthesizer::new([frame(
            turn_id,
            generation_id,
            UtteranceId::new(1),
            0,
            pcm_format(),
        )])),
        output.clone(),
    )
    .with_session_id(SessionId::new(6));
    let mut stream = runtime
        .start_turn(turn_id, generation_id, "question")
        .await
        .unwrap();

    timeout(Duration::from_secs(1), output.started.notified())
        .await
        .expect("media enqueue never started");
    let error = runtime.interrupt(turn_id, generation_id).await.unwrap_err();
    let observed = drain(&mut stream).await;

    assert_eq!(error.stage(), RuntimeStage::ContinuousAudioOutput);
    assert_eq!(output.flush_count(), 1);
    assert_eq!(
        output
            .flushes
            .lock()
            .expect("flush lock poisoned")
            .as_slice(),
        &[(SessionId::new(6), generation_id)]
    );
    assert!(output.cleanup_finished.load(Ordering::Acquire));
    assert_eq!(
        observed
            .iter()
            .filter(|event| event.is_terminal())
            .collect::<Vec<_>>(),
        vec![&RuntimeEvent::TurnCancelled { turn_id }]
    );
}

#[tokio::test]
async fn cancelled_generation_cannot_publish_late_text_or_audio() {
    let language = Arc::new(LateLanguage::default());
    let speech = Arc::new(LateSpeech::default());
    let output = Arc::new(MockContinuousAudioOutput::new());
    let runtime = StreamingTurnRuntime::new(language.clone(), speech.clone(), output.clone());
    let first_turn = TurnId::new(1);
    let first_generation = GenerationId::new(1);
    let mut first = runtime
        .start_turn(first_turn, first_generation, "first")
        .await
        .unwrap();

    timeout(Duration::from_secs(1), speech.first_started.notified())
        .await
        .expect("first speech stream never started");
    runtime
        .interrupt(first_turn, first_generation)
        .await
        .unwrap();
    let first_events = drain(&mut first).await;
    let second_turn = TurnId::new(2);
    let second_generation = GenerationId::new(2);
    let mut second = runtime
        .start_turn(second_turn, second_generation, "second")
        .await
        .unwrap();
    let second_events = drain(&mut second).await;

    assert!(first_events.iter().any(RuntimeEvent::is_terminal));
    assert!(!first_events.iter().chain(&second_events).any(|event| {
        matches!(event, RuntimeEvent::TextDelta { delta, .. } if delta == "late-first")
    }));
    assert_eq!(
        output
            .frames()
            .iter()
            .map(AudioFrame::generation_id)
            .collect::<Vec<_>>(),
        vec![second_generation]
    );
    assert!(language.first_cleanup.load(Ordering::Acquire));
    assert!(speech.first_cleanup.load(Ordering::Acquire));
}

struct TaggedLanguage {
    deltas: Vec<GenerationTextDelta>,
}

struct QueuePressureLanguage;

impl GenerationLanguageModel for QueuePressureLanguage {
    fn stream(
        &self,
        request: GenerationLanguageRequest,
        _cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            let _ = sender
                .send(Ok(GenerationTextDelta::new(
                    request.turn_id(),
                    request.generation_id(),
                    "a".repeat(4_096),
                )))
                .await;
        });
        receiver
    }
}

struct LatchedCleanupSpeech {
    speech_started: Arc<Notify>,
    cleanup_started: Arc<Notify>,
    cleanup_finished: Arc<AtomicBool>,
    cleanup_release: watch::Receiver<bool>,
}

impl StreamingSpeechSynthesizer for LatchedCleanupSpeech {
    fn stream(
        &self,
        _request: StreamingSpeechRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<AudioFrame, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        self.speech_started.notify_one();
        let cleanup_started = Arc::clone(&self.cleanup_started);
        let cleanup_finished = Arc::clone(&self.cleanup_finished);
        let mut cleanup_release = self.cleanup_release.clone();
        tokio::spawn(async move {
            cancellation.cancelled().await;
            cleanup_started.notify_one();
            while !*cleanup_release.borrow() {
                if cleanup_release.changed().await.is_err() {
                    return;
                }
            }
            cleanup_finished.store(true, Ordering::Release);
            drop(sender);
        });
        receiver
    }
}

impl TaggedLanguage {
    fn new<I>(deltas: I) -> Self
    where
        I: IntoIterator<Item = GenerationTextDelta>,
    {
        Self {
            deltas: deltas.into_iter().collect(),
        }
    }
}

impl GenerationLanguageModel for TaggedLanguage {
    fn stream(
        &self,
        _request: GenerationLanguageRequest,
        _cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        let deltas = self.deltas.clone();
        tokio::spawn(async move {
            for delta in deltas {
                if sender.send(Ok(delta)).await.is_err() {
                    return;
                }
            }
        });
        receiver
    }
}

struct BlockingOutput {
    started: Notify,
    cleanup_finished: AtomicBool,
    fail_flush: bool,
    flushes: Mutex<Vec<(SessionId, GenerationId)>>,
}

impl BlockingOutput {
    fn new(fail_flush: bool) -> Self {
        Self {
            started: Notify::new(),
            cleanup_finished: AtomicBool::new(false),
            fail_flush,
            flushes: Mutex::new(Vec::new()),
        }
    }

    fn flush_count(&self) -> usize {
        self.flushes.lock().expect("flush lock poisoned").len()
    }
}

impl ContinuousAudioOutput for BlockingOutput {
    fn enqueue<'a>(
        &'a self,
        _frame: AudioFrame,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, PlaybackReceipt> {
        Box::pin(async move {
            self.started.notify_waiters();
            cancellation.cancelled().await;
            self.cleanup_finished.store(true, Ordering::Release);
            Err(AdapterError::new("media enqueue cancelled"))
        })
    }

    fn flush<'a>(
        &'a self,
        session_id: SessionId,
        generation_id: GenerationId,
    ) -> AdapterFuture<'a, PlaybackReceipt> {
        Box::pin(async move {
            self.flushes
                .lock()
                .expect("flush lock poisoned")
                .push((session_id, generation_id));
            if self.fail_flush {
                Err(AdapterError::new("output flush failed"))
            } else {
                Ok(PlaybackReceipt::new(generation_id, PlaybackState::Flushed))
            }
        })
    }
}

#[derive(Default)]
struct LateLanguage {
    first_cleanup: Arc<AtomicBool>,
}

impl GenerationLanguageModel for LateLanguage {
    fn stream(
        &self,
        request: GenerationLanguageRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        let first_cleanup = Arc::clone(&self.first_cleanup);
        tokio::spawn(async move {
            if request.generation_id() == GenerationId::new(1) {
                let initial = format!("{}\n\n", "a".repeat(384));
                if sender
                    .send(Ok(GenerationTextDelta::new(
                        request.turn_id(),
                        request.generation_id(),
                        initial,
                    )))
                    .await
                    .is_err()
                {
                    return;
                }
                cancellation.cancelled().await;
                let _ = sender
                    .send(Ok(GenerationTextDelta::new(
                        request.turn_id(),
                        request.generation_id(),
                        "late-first",
                    )))
                    .await;
                first_cleanup.store(true, Ordering::Release);
            } else {
                let _ = sender
                    .send(Ok(GenerationTextDelta::new(
                        request.turn_id(),
                        request.generation_id(),
                        "second",
                    )))
                    .await;
            }
        });
        receiver
    }
}

#[derive(Default)]
struct LateSpeech {
    first_started: Notify,
    first_cleanup: Arc<AtomicBool>,
}

impl StreamingSpeechSynthesizer for LateSpeech {
    fn stream(
        &self,
        request: StreamingSpeechRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<AudioFrame, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        if request.generation_id() == GenerationId::new(1) {
            self.first_started.notify_one();
        }
        let first_cleanup = Arc::clone(&self.first_cleanup);
        tokio::spawn(async move {
            if request.generation_id() == GenerationId::new(1) {
                cancellation.cancelled().await;
                let _ = sender
                    .send(Ok(frame(
                        request.turn_id(),
                        request.generation_id(),
                        request.utterance_id(),
                        0,
                        pcm_format(),
                    )))
                    .await;
                first_cleanup.store(true, Ordering::Release);
            } else {
                let _ = sender
                    .send(Ok(frame(
                        request.turn_id(),
                        request.generation_id(),
                        request.utterance_id(),
                        0,
                        pcm_format(),
                    )))
                    .await;
            }
        });
        receiver
    }
}

fn assert_failed_at(observed: &[RuntimeEvent], expected_stage: RuntimeStage) {
    let failures = observed
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::TurnFailed { error, .. } => Some(error),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].stage(), expected_stage);
    assert_eq!(
        observed.iter().filter(|event| event.is_terminal()).count(),
        1
    );
}

fn pcm_format() -> PcmFormat {
    PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap()
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
