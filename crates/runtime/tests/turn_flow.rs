use std::future::ready;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioFormat, AudioOutput, AudioOutputRequest, DiscardAudioOutput,
    LanguageModel, LanguageModelRequest, MockAudioOutput, MockLanguageModel, MockSpeechSynthesizer,
    SpeechRequest, SpeechSynthesizer, SynthesizedAudio,
};
use conversation_protocol::{
    RuntimeCommand, RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStage,
    RuntimeTimingMilestone, TurnId,
};
use conversation_runtime::{
    ConversationRuntime, PhraseChunkingConfig, RuntimeCommandResult, TurnEventStream,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

fn minimal_aiff() -> Vec<u8> {
    let mut bytes = Vec::from(&b"FORM"[..]);
    bytes.extend_from_slice(&48_u32.to_be_bytes());
    bytes.extend_from_slice(b"AIFFCOMM");
    bytes.extend_from_slice(&18_u32.to_be_bytes());
    bytes.extend_from_slice(&[0; 18]);
    bytes.extend_from_slice(b"SSND");
    bytes.extend_from_slice(&9_u32.to_be_bytes());
    bytes.extend_from_slice(&[0; 8]);
    bytes.extend_from_slice(&[0x80, 0]);
    bytes
}

struct FailingLanguageModel;
struct FailingSpeechSynthesizer;

struct OverflowingLanguageModel {
    cancellation_observed: Arc<AtomicBool>,
}

struct ControlledLanguageModel {
    receiver: Mutex<Option<mpsc::Receiver<Result<String, AdapterError>>>>,
}

struct RecordingSpeechSynthesizer {
    audio: SynthesizedAudio,
    calls: mpsc::UnboundedSender<String>,
}

struct BlockingSpeechSynthesizer {
    started: mpsc::UnboundedSender<String>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

struct PanickingLanguageModel;
struct PanickingSpeechSynthesizer;
struct PanickingAudioOutput;

impl ControlledLanguageModel {
    fn new(receiver: mpsc::Receiver<Result<String, AdapterError>>) -> Self {
        Self {
            receiver: Mutex::new(Some(receiver)),
        }
    }
}

impl LanguageModel for ControlledLanguageModel {
    fn stream(
        &self,
        _request: LanguageModelRequest,
        _cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>> {
        self.receiver
            .lock()
            .expect("controlled language receiver lock poisoned")
            .take()
            .expect("controlled language model used more than once")
    }
}

impl SpeechSynthesizer for RecordingSpeechSynthesizer {
    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        let _ = self.calls.send(request.text().to_owned());
        let audio = self.audio.clone();
        Box::pin(async move { Ok(audio) })
    }
}

impl SpeechSynthesizer for BlockingSpeechSynthesizer {
    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        let _ = self.started.send(request.text().to_owned());
        let release = self
            .release
            .lock()
            .expect("blocking speech release lock poisoned")
            .take()
            .expect("blocking speech synthesizer used more than once");

        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    Err(AdapterError::new("speech synthesis cancelled"))
                }
                _ = release => {
                    Ok(SynthesizedAudio::new(minimal_aiff(), AudioFormat::Aiff))
                }
            }
        })
    }
}

impl LanguageModel for PanickingLanguageModel {
    fn stream(
        &self,
        request: LanguageModelRequest,
        _cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>> {
        assert_ne!(request.turn_id(), TurnId::new(30), "language model panic");
        MockLanguageModel::new(["recovered."]).stream(request, CancellationToken::new())
    }
}

impl SpeechSynthesizer for PanickingSpeechSynthesizer {
    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        Box::pin(async move {
            assert_ne!(
                request.turn_id(),
                TurnId::new(30),
                "speech synthesizer panic"
            );
            Ok(SynthesizedAudio::new(minimal_aiff(), AudioFormat::Aiff))
        })
    }
}

impl AudioOutput for PanickingAudioOutput {
    fn play<'a>(
        &'a self,
        request: AudioOutputRequest,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()> {
        Box::pin(async move {
            assert_ne!(request.turn_id(), TurnId::new(30), "audio output panic");
            Ok(())
        })
    }
}

impl LanguageModel for FailingLanguageModel {
    fn stream(
        &self,
        _request: LanguageModelRequest,
        _cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            let _ = sender
                .send(Err(AdapterError::new("language model unavailable")))
                .await;
        });
        receiver
    }
}

impl LanguageModel for OverflowingLanguageModel {
    fn stream(
        &self,
        _request: LanguageModelRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>> {
        let (sender, receiver) = mpsc::channel(2);
        let cancellation_observed = Arc::clone(&self.cancellation_observed);

        tokio::spawn(async move {
            let _ = sender.send(Ok("abc".into())).await;
            let _ = sender.send(Ok("de".into())).await;
            cancellation.cancelled().await;
            cancellation_observed.store(true, Ordering::Release);
        });

        receiver
    }
}

impl SpeechSynthesizer for FailingSpeechSynthesizer {
    fn synthesize<'a>(
        &'a self,
        _request: SpeechRequest,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        Box::pin(async { Err(AdapterError::new("speech synthesizer unavailable")) })
    }
}

#[tokio::test]
async fn emits_an_ordered_completed_turn() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["hello", " there"])),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(1);
    let mut events = start_turn(&runtime, turn_id, "hi").await;
    let mut observed = Vec::new();

    while let Some(event) = events.recv().await {
        observed.push(event);
    }

    assert_eq!(observed[0], RuntimeEvent::TurnStarted { turn_id });
    assert_eq!(
        observed[1],
        RuntimeEvent::TranscriptFinal {
            turn_id,
            text: "hi".into(),
        }
    );
    assert!(matches!(
        observed[2],
        RuntimeEvent::Timing {
            milestone: RuntimeTimingMilestone::FirstTextDelta,
            ..
        }
    ));
    assert_eq!(
        observed[3],
        RuntimeEvent::TextDelta {
            turn_id,
            delta: "hello".into(),
        }
    );
    assert_eq!(
        observed[4],
        RuntimeEvent::TextDelta {
            turn_id,
            delta: " there".into(),
        }
    );
    assert_eq!(observed[5], RuntimeEvent::SpeechStarted { turn_id });
    assert!(matches!(
        observed[6],
        RuntimeEvent::Timing {
            milestone: RuntimeTimingMilestone::FirstSynthesisRequest,
            ..
        }
    ));
    assert!(matches!(
        observed[7],
        RuntimeEvent::Timing {
            milestone: RuntimeTimingMilestone::FirstPlayableAudio,
            ..
        }
    ));
    assert_eq!(observed[8], RuntimeEvent::SpeechCompleted { turn_id });
    assert_eq!(observed[9], RuntimeEvent::TurnCompleted { turn_id });
    assert_eq!(
        observed.iter().filter(|event| event.is_terminal()).count(),
        1
    );
}

#[tokio::test]
async fn streams_ordered_phrases_to_audio_before_generation_completes() {
    let (delta_sender, delta_receiver) = mpsc::channel(2);
    delta_sender
        .send(Ok("First sentence.".into()))
        .await
        .unwrap();
    let (speech_calls, mut synthesized_text) = mpsc::unbounded_channel();
    let speech = Arc::new(RecordingSpeechSynthesizer {
        audio: SynthesizedAudio::new(minimal_aiff(), AudioFormat::Aiff),
        calls: speech_calls,
    });
    let output = Arc::new(MockAudioOutput::new());
    let runtime = ConversationRuntime::new(
        Arc::new(ControlledLanguageModel::new(delta_receiver)),
        speech,
        output.clone(),
    )
    .with_phrase_chunking(PhraseChunkingConfig::new(15, 24).unwrap());
    let turn_id = TurnId::new(20);
    let mut events = start_turn(&runtime, turn_id, "overlap").await;

    let first_synthesis = timeout(Duration::from_secs(1), synthesized_text.recv())
        .await
        .expect("first synthesis did not start before generation release")
        .expect("speech call channel closed");
    let first_synthesis_started_before_model_release = true;
    assert_eq!(first_synthesis, "First sentence.");

    delta_sender
        .send(Ok("Third sentence.".into()))
        .await
        .unwrap();
    drop(delta_sender);

    let observed = drain_events(&mut events).await;
    let second_synthesis = synthesized_text
        .recv()
        .await
        .expect("second phrase was not synthesized");
    let synthesized_text = vec![first_synthesis, second_synthesis];
    assert_eq!(
        synthesized_text,
        vec!["First sentence.".to_owned(), "Third sentence.".to_owned()]
    );
    assert!(first_synthesis_started_before_model_release);

    let first_text_delta_index = observed
        .iter()
        .position(|event| matches!(event, RuntimeEvent::TextDelta { .. }))
        .unwrap();
    assert!(matches!(
        observed[first_text_delta_index - 1],
        RuntimeEvent::Timing {
            milestone: RuntimeTimingMilestone::FirstTextDelta,
            ..
        }
    ));
    let first_speech_index = observed
        .iter()
        .position(|event| matches!(event, RuntimeEvent::SpeechStarted { .. }))
        .unwrap();
    assert!(matches!(
        observed[first_speech_index],
        RuntimeEvent::SpeechStarted { .. }
    ));
    assert!(matches!(
        observed[first_speech_index + 1],
        RuntimeEvent::Timing {
            milestone: RuntimeTimingMilestone::FirstSynthesisRequest,
            ..
        }
    ));

    for milestone in [
        RuntimeTimingMilestone::FirstTextDelta,
        RuntimeTimingMilestone::FirstSynthesisRequest,
        RuntimeTimingMilestone::FirstPlayableAudio,
    ] {
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(
                    event,
                    RuntimeEvent::Timing {
                        milestone: event_milestone,
                        ..
                    } if *event_milestone == milestone
                ))
                .count(),
            1
        );
    }
    let first_playable_index = observed
        .iter()
        .position(|event| {
            matches!(
                event,
                RuntimeEvent::Timing {
                    milestone: RuntimeTimingMilestone::FirstPlayableAudio,
                    ..
                }
            )
        })
        .unwrap();
    let speech_completed_index = observed
        .iter()
        .position(|event| matches!(event, RuntimeEvent::SpeechCompleted { .. }))
        .unwrap();
    assert!(first_speech_index < first_playable_index);
    assert!(first_playable_index < speech_completed_index);

    let output_requests = output.requests();
    assert_eq!(
        output_requests
            .iter()
            .map(|request| request.segment_index())
            .collect::<Vec<_>>(),
        [0, 1]
    );
    assert!(output_requests
        .iter()
        .all(|request| request.audio().validate().is_ok()));
    let terminal_events: Vec<_> = observed
        .iter()
        .filter(|event| event.is_terminal())
        .collect();
    assert_eq!(terminal_events.len(), 1);
}

#[tokio::test]
async fn short_sentences_are_one_speech_request_without_changing_text_deltas() {
    let original = "# 问候\n你好。今天很好！*保持自然*";
    let (speech_calls, mut synthesized_text) = mpsc::unbounded_channel();
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new([original])),
        Arc::new(RecordingSpeechSynthesizer {
            audio: SynthesizedAudio::new(minimal_aiff(), AudioFormat::Aiff),
            calls: speech_calls,
        }),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(60);
    let mut events = start_turn(&runtime, turn_id, "coalesce").await;

    let observed = drain_events(&mut events).await;
    let deltas = observed
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect::<String>();

    assert_eq!(deltas, original);
    assert_eq!(synthesized_text.recv().await.as_deref(), Some(original));
    assert!(synthesized_text.try_recv().is_err());
}

#[tokio::test]
async fn configured_phrase_limits_control_runtime_synthesis_segments() {
    let (speech_calls, mut synthesized_text) = mpsc::unbounded_channel();
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["你好世界"])),
        Arc::new(RecordingSpeechSynthesizer {
            audio: SynthesizedAudio::new(minimal_aiff(), AudioFormat::Aiff),
            calls: speech_calls,
        }),
        Arc::new(DiscardAudioOutput),
    )
    .with_phrase_chunking(PhraseChunkingConfig::new(6, 9).unwrap());
    let mut events = start_turn(&runtime, TurnId::new(21), "segment").await;

    let observed = drain_events(&mut events).await;
    let mut phrases = Vec::new();
    while let Ok(phrase) = synthesized_text.try_recv() {
        phrases.push(phrase);
    }

    assert_eq!(phrases, ["你好世", "界"]);
    assert!(matches!(
        observed.last(),
        Some(RuntimeEvent::TurnCompleted { .. })
    ));
}

#[tokio::test]
async fn runtime_never_synthesizes_a_phrase_above_multibyte_boundaries() {
    let cases = [
        ("aaaaaaa。", vec!["aaaaaaa", "。"]),
        ("aaaaaaa，", vec!["aaaaaaa", "，"]),
        ("aaaaaaa\u{2003}", vec!["aaaaaaa"]),
    ];

    for (case_index, (input, expected)) in cases.into_iter().enumerate() {
        let (speech_calls, mut synthesized_text) = mpsc::unbounded_channel();
        let runtime = ConversationRuntime::new(
            Arc::new(MockLanguageModel::new([input])),
            Arc::new(RecordingSpeechSynthesizer {
                audio: SynthesizedAudio::new(minimal_aiff(), AudioFormat::Aiff),
                calls: speech_calls,
            }),
            Arc::new(DiscardAudioOutput),
        )
        .with_phrase_chunking(PhraseChunkingConfig::new(6, 9).unwrap());
        let turn_id = TurnId::new(40 + case_index as u64);
        let mut events = start_turn(&runtime, turn_id, "segment").await;

        let observed = drain_events(&mut events).await;
        let mut phrases = Vec::new();
        while let Ok(phrase) = synthesized_text.try_recv() {
            phrases.push(phrase);
        }

        assert_eq!(phrases, expected, "input: {input:?}");
        assert!(
            phrases.iter().all(|phrase| phrase.len() <= 9),
            "input: {input:?}, phrases: {phrases:?}"
        );
        assert!(matches!(
            observed.last(),
            Some(RuntimeEvent::TurnCompleted {
                turn_id: completed_turn
            }) if *completed_turn == turn_id
        ));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn two_item_phrase_queue_backpressures_language_consumption() {
    let (delta_sender, delta_receiver) = mpsc::channel(1);
    delta_sender.send(Ok("First.".into())).await.unwrap();
    let (speech_started, mut speech_started_receiver) = mpsc::unbounded_channel();
    let (_release_speech, speech_release) = oneshot::channel();
    let runtime = ConversationRuntime::new(
        Arc::new(ControlledLanguageModel::new(delta_receiver)),
        Arc::new(BlockingSpeechSynthesizer {
            started: speech_started,
            release: Mutex::new(Some(speech_release)),
        }),
        Arc::new(DiscardAudioOutput),
    )
    .with_phrase_chunking(PhraseChunkingConfig::new(6, 12).unwrap());
    let turn_id = TurnId::new(22);
    let mut events = start_turn(&runtime, turn_id, "queue").await;

    assert_eq!(
        timeout(Duration::from_secs(1), speech_started_receiver.recv())
            .await
            .expect("first synthesis did not start")
            .expect("speech start channel closed"),
        "First."
    );
    for phrase in ["Second.", "Third.", "Fourth.", "Fifth."] {
        delta_sender.send(Ok(phrase.into())).await.unwrap();
    }

    let mut sixth_send = Box::pin(delta_sender.send(Ok("Sixth.".into())));
    tokio::select! {
        biased;
        result = &mut sixth_send => {
            panic!("language consumption was not backpressured: {result:?}");
        }
        _ = ready(()) => {}
    }
    drop(sixth_send);
    drop(delta_sender);

    interrupt(&runtime, turn_id).await;
    let observed = drain_events(&mut events).await;
    assert_eq!(
        observed
            .into_iter()
            .filter(RuntimeEvent::is_terminal)
            .collect::<Vec<_>>(),
        [RuntimeEvent::TurnCancelled { turn_id }]
    );
}

#[tokio::test]
async fn rejects_invalid_typed_audio_before_playback() {
    let (speech_calls, _synthesized_text) = mpsc::unbounded_channel();
    let output = Arc::new(MockAudioOutput::new());
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["Speak."])),
        Arc::new(RecordingSpeechSynthesizer {
            audio: SynthesizedAudio::new(b"not-an-aiff".to_vec(), AudioFormat::Aiff),
            calls: speech_calls,
        }),
        output.clone(),
    );
    let turn_id = TurnId::new(23);
    let mut events = start_turn(&runtime, turn_id, "validate").await;

    let observed = drain_events(&mut events).await;

    assert!(output.requests().is_empty());
    assert!(!observed.iter().any(|event| matches!(
        event,
        RuntimeEvent::Timing {
            milestone: RuntimeTimingMilestone::FirstPlayableAudio,
            ..
        }
    )));
    assert_eq!(
        observed
            .into_iter()
            .filter(RuntimeEvent::is_terminal)
            .collect::<Vec<_>>(),
        [RuntimeEvent::TurnFailed {
            turn_id,
            error: RuntimeError::new(
                RuntimeErrorKind::Adapter,
                RuntimeStage::SpeechSynthesizer,
                "synthesized audio was not a valid encoded container",
            ),
        }]
    );
}

#[tokio::test]
async fn whitespace_only_output_skips_speech_lifecycle_and_timing() {
    let (speech_calls, mut synthesized_text) = mpsc::unbounded_channel();
    let output = Arc::new(MockAudioOutput::new());
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new([" \t", "\n  "])),
        Arc::new(RecordingSpeechSynthesizer {
            audio: SynthesizedAudio::new(minimal_aiff(), AudioFormat::Aiff),
            calls: speech_calls,
        }),
        output.clone(),
    );
    let turn_id = TurnId::new(24);
    let mut events = start_turn(&runtime, turn_id, "blank").await;

    let observed = drain_events(&mut events).await;

    assert!(synthesized_text.try_recv().is_err());
    assert!(output.requests().is_empty());
    assert!(!observed.iter().any(|event| matches!(
        event,
        RuntimeEvent::SpeechStarted { .. }
            | RuntimeEvent::SpeechCompleted { .. }
            | RuntimeEvent::Timing {
                milestone: RuntimeTimingMilestone::FirstSynthesisRequest
                    | RuntimeTimingMilestone::FirstPlayableAudio,
                ..
            }
    )));
    assert_eq!(
        observed
            .iter()
            .filter(|event| event.is_terminal())
            .collect::<Vec<_>>(),
        [&RuntimeEvent::TurnCompleted { turn_id }]
    );
}

#[tokio::test]
async fn reports_language_model_failure_as_the_only_terminal_event() {
    let runtime = ConversationRuntime::new(
        Arc::new(FailingLanguageModel),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(2);
    let mut events = start_turn(&runtime, turn_id, "fail").await;
    let mut observed = Vec::new();

    while let Some(event) = events.recv().await {
        observed.push(event);
    }

    let terminal_events: Vec<_> = observed
        .into_iter()
        .filter(RuntimeEvent::is_terminal)
        .collect();
    assert_eq!(
        terminal_events,
        vec![RuntimeEvent::TurnFailed {
            turn_id,
            error: RuntimeError::new(
                RuntimeErrorKind::Adapter,
                RuntimeStage::LanguageModel,
                "language model unavailable",
            ),
        }]
    );
}

#[tokio::test]
async fn bounds_language_model_responses_and_cancels_the_model_child_token() {
    let cancellation_observed = Arc::new(AtomicBool::new(false));
    let runtime = ConversationRuntime::new(
        Arc::new(OverflowingLanguageModel {
            cancellation_observed: Arc::clone(&cancellation_observed),
        }),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DiscardAudioOutput),
    )
    .with_max_response_bytes(4)
    .unwrap();
    let turn_id = TurnId::new(5);
    let mut events = start_turn(&runtime, turn_id, "bound this").await;
    let mut observed = Vec::new();

    while let Some(event) = events.recv().await {
        observed.push(event);
    }

    assert!(observed.contains(&RuntimeEvent::TextDelta {
        turn_id,
        delta: "abc".into(),
    }));
    assert!(!observed.contains(&RuntimeEvent::TextDelta {
        turn_id,
        delta: "de".into(),
    }));
    assert_eq!(
        observed
            .iter()
            .filter(|event| event.is_terminal())
            .collect::<Vec<_>>(),
        vec![&RuntimeEvent::TurnFailed {
            turn_id,
            error: RuntimeError::new(
                RuntimeErrorKind::Adapter,
                RuntimeStage::LanguageModel,
                "language model response exceeds the maximum size of 4 bytes",
            ),
        }]
    );
    timeout(Duration::from_secs(1), async {
        while !cancellation_observed.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("language model child token was not cancelled");
}

#[tokio::test]
async fn accepts_exactly_the_default_runtime_response_limit() {
    let delta = "a".repeat(64 * 1024);
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new([delta])),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(6);
    let mut events = start_turn(&runtime, turn_id, "bound this").await;
    let mut observed = Vec::new();

    while let Some(event) = events.recv().await {
        observed.push(event);
    }

    assert!(observed.contains(&RuntimeEvent::TextDelta {
        turn_id,
        delta: "a".repeat(64 * 1024),
    }));
    assert!(observed.contains(&RuntimeEvent::TurnCompleted { turn_id }));
}

#[tokio::test]
async fn rejects_one_byte_over_the_default_runtime_response_limit() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["a".repeat(64 * 1024 + 1)])),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(7);
    let mut events = start_turn(&runtime, turn_id, "bound this").await;
    let mut observed = Vec::new();

    while let Some(event) = events.recv().await {
        observed.push(event);
    }

    assert!(!observed.iter().any(|event| matches!(
        event,
        RuntimeEvent::TextDelta { turn_id: event_turn_id, .. } if *event_turn_id == turn_id
    )));
    assert_eq!(
        observed
            .into_iter()
            .filter(RuntimeEvent::is_terminal)
            .collect::<Vec<_>>(),
        vec![RuntimeEvent::TurnFailed {
            turn_id,
            error: RuntimeError::new(
                RuntimeErrorKind::Adapter,
                RuntimeStage::LanguageModel,
                "language model response exceeds the maximum size of 65536 bytes",
            ),
        }]
    );
}

#[test]
fn rejects_a_zero_runtime_response_limit() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["response"])),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DiscardAudioOutput),
    );

    assert!(runtime.with_max_response_bytes(0).is_err());
}

#[tokio::test]
async fn reports_speech_failure_with_the_synthesis_stage() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["response"])),
        Arc::new(FailingSpeechSynthesizer),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(4);
    let mut events = start_turn(&runtime, turn_id, "fail speech").await;
    let mut terminal_events = Vec::new();

    while let Some(event) = events.recv().await {
        if event.is_terminal() {
            terminal_events.push(event);
        }
    }

    assert_eq!(
        terminal_events,
        vec![RuntimeEvent::TurnFailed {
            turn_id,
            error: RuntimeError::new(
                RuntimeErrorKind::Adapter,
                RuntimeStage::SpeechSynthesizer,
                "speech synthesizer unavailable",
            ),
        }]
    );
}

#[tokio::test]
async fn cancels_during_speech_synthesis() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["response"])),
        Arc::new(MockSpeechSynthesizer::delayed(
            minimal_aiff(),
            Duration::from_secs(5),
        )),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(3);
    let mut events = start_turn(&runtime, turn_id, "speak").await;
    let mut observed = Vec::new();

    while let Some(event) = events.recv().await {
        let speech_started = matches!(event, RuntimeEvent::SpeechStarted { .. });
        observed.push(event);
        if speech_started {
            interrupt(&runtime, turn_id).await;
        }
    }

    let terminal_events: Vec<_> = observed
        .into_iter()
        .filter(RuntimeEvent::is_terminal)
        .collect();
    assert_eq!(
        terminal_events,
        vec![RuntimeEvent::TurnCancelled { turn_id }]
    );
}

#[tokio::test]
async fn reuses_runtime_after_a_completed_turn() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["response"])),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DiscardAudioOutput),
    );

    for turn_number in [10, 11] {
        let turn_id = TurnId::new(turn_number);
        let mut events = start_turn(&runtime, turn_id, "again").await;
        let mut terminal_events = Vec::new();

        while let Some(event) = events.recv().await {
            if event.is_terminal() {
                terminal_events.push(event);
            }
        }

        assert_eq!(
            terminal_events,
            vec![RuntimeEvent::TurnCompleted { turn_id }]
        );
    }
}

#[tokio::test]
async fn language_model_stream_panic_fails_at_language_stage_and_allows_reuse() {
    let runtime = ConversationRuntime::new(
        Arc::new(PanickingLanguageModel),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DiscardAudioOutput),
    );

    assert_panicking_turn_and_reuse(
        &runtime,
        RuntimeStage::LanguageModel,
        "language model adapter panicked",
    )
    .await;
}

#[tokio::test]
async fn speech_panic_fails_at_synthesis_stage_and_allows_reuse() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["Speak."])),
        Arc::new(PanickingSpeechSynthesizer),
        Arc::new(DiscardAudioOutput),
    );

    assert_panicking_turn_and_reuse(
        &runtime,
        RuntimeStage::SpeechSynthesizer,
        "speech synthesizer adapter panicked",
    )
    .await;
}

#[tokio::test]
async fn output_panic_fails_at_output_stage_and_allows_reuse() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["Speak."])),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(PanickingAudioOutput),
    );

    assert_panicking_turn_and_reuse(
        &runtime,
        RuntimeStage::AudioOutput,
        "audio output adapter panicked",
    )
    .await;
}

async fn assert_panicking_turn_and_reuse(
    runtime: &ConversationRuntime,
    expected_stage: RuntimeStage,
    expected_message: &str,
) {
    let first_turn = TurnId::new(30);
    let mut first_events = start_turn(runtime, first_turn, "panic").await;
    let first_observed = drain_events(&mut first_events).await;

    assert_eq!(
        first_observed
            .into_iter()
            .filter(RuntimeEvent::is_terminal)
            .collect::<Vec<_>>(),
        [RuntimeEvent::TurnFailed {
            turn_id: first_turn,
            error: RuntimeError::new(RuntimeErrorKind::Adapter, expected_stage, expected_message,),
        }]
    );

    let second_turn = TurnId::new(31);
    let mut second_events = start_turn(runtime, second_turn, "reuse").await;
    assert_eq!(
        drain_events(&mut second_events)
            .await
            .into_iter()
            .filter(RuntimeEvent::is_terminal)
            .collect::<Vec<_>>(),
        [RuntimeEvent::TurnCompleted {
            turn_id: second_turn
        }]
    );
}

async fn start_turn(
    runtime: &ConversationRuntime,
    turn_id: TurnId,
    transcript: &str,
) -> TurnEventStream {
    match runtime
        .execute(RuntimeCommand::StartTurn {
            turn_id,
            transcript: transcript.into(),
        })
        .await
        .unwrap()
    {
        RuntimeCommandResult::TurnStarted { events } => events,
        RuntimeCommandResult::InterruptAccepted => {
            panic!("start command must return a turn event stream")
        }
        _ => panic!("start command returned an unknown result"),
    }
}

async fn interrupt(runtime: &ConversationRuntime, turn_id: TurnId) {
    assert!(matches!(
        runtime
            .execute(RuntimeCommand::Interrupt { turn_id })
            .await
            .unwrap(),
        RuntimeCommandResult::InterruptAccepted
    ));
}

async fn drain_events(events: &mut TurnEventStream) -> Vec<RuntimeEvent> {
    let mut observed = Vec::new();
    while let Some(event) = events.recv().await {
        observed.push(event);
    }
    observed
}
