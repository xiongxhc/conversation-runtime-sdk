use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioFormat, AudioOutput, AudioOutputRequest, DiscardAudioOutput,
    LanguageModel, LanguageModelRequest, MockLanguageModel, MockSpeechSynthesizer, SpeechRequest,
    SpeechSynthesizer, SynthesizedAudio,
};
use conversation_protocol::{
    RuntimeCommand, RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStage, TurnId,
};
use conversation_runtime::{ConversationRuntime, RuntimeCommandResult, TurnEventStream};
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

struct CompletionSignallingSpeech {
    completed: Arc<AtomicBool>,
}

struct CleanupAwareSpeech {
    started: Arc<AtomicBool>,
    cleanup_completed: Arc<AtomicBool>,
}

struct CancellationSignallingLanguageModel {
    started: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}

struct ControlledLanguageModel {
    receiver: Mutex<Option<mpsc::Receiver<Result<String, AdapterError>>>>,
}

struct CancellationCleanupLanguageModel {
    text: String,
    cleanup_completed: Arc<AtomicBool>,
}

struct CleanupBlockingSpeech {
    started: mpsc::UnboundedSender<String>,
    cleanup_completed: Arc<AtomicBool>,
}

struct FailingSpeech {
    started: mpsc::UnboundedSender<()>,
}

struct CleanupBlockingOutput {
    started: mpsc::UnboundedSender<()>,
    cleanup_completed: Arc<AtomicBool>,
}

struct GatedFailingOutput {
    started: mpsc::UnboundedSender<()>,
    fail: Mutex<Option<oneshot::Receiver<()>>>,
    cleanup_completed: Arc<AtomicBool>,
}

struct SaturatingLanguageModel {
    progress: mpsc::UnboundedSender<usize>,
    cleanup_completed: Arc<AtomicBool>,
}

struct DropAwareLanguageModel {
    started: mpsc::UnboundedSender<()>,
    cleanup_completed: mpsc::UnboundedSender<()>,
}

struct DropAwareSpeech {
    started: mpsc::UnboundedSender<()>,
    cleanup_completed: mpsc::UnboundedSender<()>,
}

struct DropAwareOutput {
    started: mpsc::UnboundedSender<()>,
    cleanup_completed: mpsc::UnboundedSender<()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReuseOutcome {
    Completion,
    ExternalCancellation,
    LanguageFailure,
    SynthesisFailure,
    OutputFailure,
}

struct ReusableLanguageModel {
    first_outcome: ReuseOutcome,
}

struct ReusableSpeech {
    first_outcome: ReuseOutcome,
}

struct ReusableOutput {
    first_outcome: ReuseOutcome,
}

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

impl LanguageModel for CancellationCleanupLanguageModel {
    fn stream(
        &self,
        _request: LanguageModelRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        let text = self.text.clone();
        let cleanup_completed = Arc::clone(&self.cleanup_completed);

        tokio::spawn(async move {
            if sender.send(Ok(text)).await.is_err() {
                return;
            }
            cancellation.cancelled().await;
            cleanup_completed.store(true, Ordering::Release);
            drop(sender);
        });

        receiver
    }
}

impl LanguageModel for SaturatingLanguageModel {
    fn stream(
        &self,
        _request: LanguageModelRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        let progress = self.progress.clone();
        let cleanup_completed = Arc::clone(&self.cleanup_completed);

        tokio::spawn(async move {
            for (index, delta) in std::iter::once("Speak.".to_owned())
                .chain((0..40).map(|_| "x".to_owned()))
                .enumerate()
            {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    result = sender.send(Ok(delta)) => {
                        if result.is_err() {
                            break;
                        }
                        let _ = progress.send(index + 1);
                    }
                }
            }
            cancellation.cancelled().await;
            cleanup_completed.store(true, Ordering::Release);
        });

        receiver
    }
}

impl LanguageModel for DropAwareLanguageModel {
    fn stream(
        &self,
        request: LanguageModelRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>> {
        if request.turn_id() != TurnId::new(30) {
            return MockLanguageModel::new(["recovered."]).stream(request, cancellation);
        }

        let (sender, receiver) = mpsc::channel(1);
        let _ = self.started.send(());
        let cleanup_completed = self.cleanup_completed.clone();
        tokio::spawn(async move {
            cancellation.cancelled().await;
            let _ = cleanup_completed.send(());
            drop(sender);
        });
        receiver
    }
}

impl LanguageModel for ReusableLanguageModel {
    fn stream(
        &self,
        request: LanguageModelRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        let first_outcome = self.first_outcome;

        tokio::spawn(async move {
            if request.turn_id() == TurnId::new(1) {
                match first_outcome {
                    ReuseOutcome::ExternalCancellation => {
                        cancellation.cancelled().await;
                        return;
                    }
                    ReuseOutcome::LanguageFailure => {
                        let _ = sender
                            .send(Err(AdapterError::new("language model unavailable")))
                            .await;
                        return;
                    }
                    _ => {}
                }
            }
            let _ = sender.send(Ok("response.".into())).await;
        });

        receiver
    }
}

impl SpeechSynthesizer for CleanupBlockingSpeech {
    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        let _ = self.started.send(request.text().to_owned());
        Box::pin(async move {
            cancellation.cancelled().await;
            self.cleanup_completed.store(true, Ordering::Release);
            Err(AdapterError::new("speech synthesis cancelled"))
        })
    }
}

impl SpeechSynthesizer for FailingSpeech {
    fn synthesize<'a>(
        &'a self,
        _request: SpeechRequest,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        let _ = self.started.send(());
        Box::pin(async { Err(AdapterError::new("speech synthesizer unavailable")) })
    }
}

impl SpeechSynthesizer for ReusableSpeech {
    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        let should_fail = request.turn_id() == TurnId::new(1)
            && self.first_outcome == ReuseOutcome::SynthesisFailure;
        Box::pin(async move {
            if should_fail {
                Err(AdapterError::new("speech synthesizer unavailable"))
            } else {
                Ok(SynthesizedAudio::new(minimal_aiff(), AudioFormat::Aiff))
            }
        })
    }
}

impl SpeechSynthesizer for DropAwareSpeech {
    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        if request.turn_id() != TurnId::new(30) {
            return Box::pin(async {
                Ok(SynthesizedAudio::new(minimal_aiff(), AudioFormat::Aiff))
            });
        }

        let _ = self.started.send(());
        Box::pin(async move {
            cancellation.cancelled().await;
            let _ = self.cleanup_completed.send(());
            Err(AdapterError::new("speech synthesis cancelled"))
        })
    }
}

impl AudioOutput for CleanupBlockingOutput {
    fn play<'a>(
        &'a self,
        _request: AudioOutputRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()> {
        let _ = self.started.send(());
        Box::pin(async move {
            cancellation.cancelled().await;
            self.cleanup_completed.store(true, Ordering::Release);
            Err(AdapterError::new("audio output cancelled"))
        })
    }
}

impl AudioOutput for GatedFailingOutput {
    fn play<'a>(
        &'a self,
        _request: AudioOutputRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()> {
        let _ = self.started.send(());
        let fail = self
            .fail
            .lock()
            .expect("output failure gate lock poisoned")
            .take()
            .expect("gated output used more than once");

        Box::pin(async move {
            let message = tokio::select! {
                biased;
                _ = cancellation.cancelled() => "audio output cancelled",
                _ = fail => "audio output unavailable",
            };
            self.cleanup_completed.store(true, Ordering::Release);
            Err(AdapterError::new(message))
        })
    }
}

impl AudioOutput for ReusableOutput {
    fn play<'a>(
        &'a self,
        request: AudioOutputRequest,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()> {
        let should_fail = request.turn_id() == TurnId::new(1)
            && self.first_outcome == ReuseOutcome::OutputFailure;
        Box::pin(async move {
            if should_fail {
                Err(AdapterError::new("audio output unavailable"))
            } else {
                Ok(())
            }
        })
    }
}

impl AudioOutput for DropAwareOutput {
    fn play<'a>(
        &'a self,
        request: AudioOutputRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()> {
        if request.turn_id() != TurnId::new(30) {
            return Box::pin(async { Ok(()) });
        }

        let _ = self.started.send(());
        Box::pin(async move {
            cancellation.cancelled().await;
            let _ = self.cleanup_completed.send(());
            Err(AdapterError::new("audio output cancelled"))
        })
    }
}

impl LanguageModel for CancellationSignallingLanguageModel {
    fn stream(
        &self,
        _request: LanguageModelRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        self.started.store(true, Ordering::Release);
        let cancelled = Arc::clone(&self.cancelled);

        tokio::spawn(async move {
            cancellation.cancelled().await;
            cancelled.store(true, Ordering::Release);
            drop(sender);
        });

        receiver
    }
}

struct InvocationTrackingSpeech {
    invoked: Arc<AtomicBool>,
}

impl SpeechSynthesizer for InvocationTrackingSpeech {
    fn synthesize<'a>(
        &'a self,
        _request: SpeechRequest,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        self.invoked.store(true, Ordering::Release);
        Box::pin(async { Ok(SynthesizedAudio::new(minimal_aiff(), AudioFormat::Aiff)) })
    }
}

impl SpeechSynthesizer for CompletionSignallingSpeech {
    fn synthesize<'a>(
        &'a self,
        _request: SpeechRequest,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        Box::pin(async move {
            self.completed.store(true, Ordering::Release);
            Ok(SynthesizedAudio::new(minimal_aiff(), AudioFormat::Aiff))
        })
    }
}

impl SpeechSynthesizer for CleanupAwareSpeech {
    fn synthesize<'a>(
        &'a self,
        _request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        Box::pin(async move {
            self.started.store(true, Ordering::Release);
            cancellation.cancelled().await;
            tokio::task::yield_now().await;
            self.cleanup_completed.store(true, Ordering::Release);
            Err(AdapterError::new("speech synthesis cancelled"))
        })
    }
}

#[tokio::test]
async fn interruption_reaches_generation_and_skips_synthesis() {
    let generation_started = Arc::new(AtomicBool::new(false));
    let generation_cancelled = Arc::new(AtomicBool::new(false));
    let synthesis_invoked = Arc::new(AtomicBool::new(false));
    let runtime = ConversationRuntime::new(
        Arc::new(CancellationSignallingLanguageModel {
            started: Arc::clone(&generation_started),
            cancelled: Arc::clone(&generation_cancelled),
        }),
        Arc::new(InvocationTrackingSpeech {
            invoked: Arc::clone(&synthesis_invoked),
        }),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(6);
    let mut events = start_turn(&runtime, turn_id, "stop generation").await;

    assert_eq!(
        events.recv().await,
        Some(RuntimeEvent::TurnStarted { turn_id })
    );
    timeout(Duration::from_secs(1), async {
        while !generation_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("generation did not start");
    interrupt(&runtime, turn_id).await.unwrap();

    while events.recv().await.is_some() {}

    timeout(Duration::from_secs(1), async {
        while !generation_cancelled.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("generation did not observe cancellation");
    assert!(!synthesis_invoked.load(Ordering::Acquire));
}

#[tokio::test(flavor = "current_thread")]
async fn immediate_interruption_preserves_the_started_event() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::delayed(["late"], Duration::from_secs(5))),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(8);
    let mut events = start_turn(&runtime, turn_id, "interrupt immediately").await;

    interrupt(&runtime, turn_id).await.unwrap();

    assert_eq!(
        events.recv().await,
        Some(RuntimeEvent::TurnStarted { turn_id })
    );
    assert_eq!(
        events.recv().await,
        Some(RuntimeEvent::TurnCancelled { turn_id })
    );
}

#[tokio::test]
async fn interruption_emits_one_cancelled_terminal_event() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::delayed(["late"], Duration::from_secs(5))),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(7);
    let mut events = start_turn(&runtime, turn_id, "stop").await;

    assert_eq!(
        events.recv().await,
        Some(RuntimeEvent::TurnStarted { turn_id })
    );
    interrupt(&runtime, turn_id).await.unwrap();

    let mut terminal_events = Vec::new();
    while let Some(event) = events.recv().await {
        if event.is_terminal() {
            terminal_events.push(event);
        }
    }

    assert_eq!(
        terminal_events,
        vec![RuntimeEvent::TurnCancelled { turn_id }]
    );
}

#[tokio::test]
async fn waits_for_speech_cleanup_before_cancellation_completes() {
    let synthesis_started = Arc::new(AtomicBool::new(false));
    let cleanup_completed = Arc::new(AtomicBool::new(false));
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["response"])),
        Arc::new(CleanupAwareSpeech {
            started: Arc::clone(&synthesis_started),
            cleanup_completed: Arc::clone(&cleanup_completed),
        }),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(14);
    let mut events = start_turn(&runtime, turn_id, "cancel during speech").await;

    timeout(Duration::from_secs(1), async {
        while !synthesis_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("speech synthesis did not start");

    interrupt(&runtime, turn_id).await.unwrap();

    let mut terminal_events = Vec::new();
    while let Some(event) = events.recv().await {
        if event.is_terminal() {
            terminal_events.push(event);
        }
    }

    assert!(cleanup_completed.load(Ordering::Acquire));
    assert_eq!(
        terminal_events,
        vec![RuntimeEvent::TurnCancelled { turn_id }]
    );
}

#[tokio::test]
async fn dropped_event_stream_cleans_stalled_language_and_allows_reuse() {
    let (started, mut started_receiver) = mpsc::unbounded_channel();
    let (cleanup_completed, mut cleanup_receiver) = mpsc::unbounded_channel();
    let runtime = ConversationRuntime::new(
        Arc::new(DropAwareLanguageModel {
            started,
            cleanup_completed,
        }),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DiscardAudioOutput),
    );
    let events = start_turn(&runtime, TurnId::new(30), "drop language").await;

    timeout(Duration::from_secs(1), started_receiver.recv())
        .await
        .expect("language model did not start")
        .expect("language start channel closed");
    drop(events);

    timeout(Duration::from_secs(1), cleanup_receiver.recv())
        .await
        .expect("dropped stream did not clean language work")
        .expect("language cleanup channel closed");
    assert_reusable_after_stream_drop(&runtime).await;
}

#[tokio::test]
async fn dropped_event_stream_cleans_stalled_synthesis_and_allows_reuse() {
    let (started, mut started_receiver) = mpsc::unbounded_channel();
    let (cleanup_completed, mut cleanup_receiver) = mpsc::unbounded_channel();
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["Speak."])),
        Arc::new(DropAwareSpeech {
            started,
            cleanup_completed,
        }),
        Arc::new(DiscardAudioOutput),
    );
    let events = start_turn(&runtime, TurnId::new(30), "drop synthesis").await;

    timeout(Duration::from_secs(1), started_receiver.recv())
        .await
        .expect("speech synthesizer did not start")
        .expect("speech start channel closed");
    drop(events);

    timeout(Duration::from_secs(1), cleanup_receiver.recv())
        .await
        .expect("dropped stream did not clean synthesis work")
        .expect("speech cleanup channel closed");
    assert_reusable_after_stream_drop(&runtime).await;
}

#[tokio::test]
async fn dropped_event_stream_cleans_stalled_output_and_allows_reuse() {
    let (started, mut started_receiver) = mpsc::unbounded_channel();
    let (cleanup_completed, mut cleanup_receiver) = mpsc::unbounded_channel();
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["Speak."])),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DropAwareOutput {
            started,
            cleanup_completed,
        }),
    );
    let events = start_turn(&runtime, TurnId::new(30), "drop output").await;

    timeout(Duration::from_secs(1), started_receiver.recv())
        .await
        .expect("audio output did not start")
        .expect("output start channel closed");
    drop(events);

    timeout(Duration::from_secs(1), cleanup_receiver.recv())
        .await
        .expect("dropped stream did not clean output work")
        .expect("output cleanup channel closed");
    assert_reusable_after_stream_drop(&runtime).await;
}

#[tokio::test(flavor = "current_thread")]
async fn interruption_discards_queued_synthesis_after_active_cleanup() {
    let (delta_sender, delta_receiver) = mpsc::channel(1);
    delta_sender.send(Ok("First.".into())).await.unwrap();
    let (speech_started, mut speech_started_receiver) = mpsc::unbounded_channel();
    let cleanup_completed = Arc::new(AtomicBool::new(false));
    let runtime = ConversationRuntime::new(
        Arc::new(ControlledLanguageModel::new(delta_receiver)),
        Arc::new(CleanupBlockingSpeech {
            started: speech_started,
            cleanup_completed: Arc::clone(&cleanup_completed),
        }),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(15);
    let mut events = start_turn(&runtime, turn_id, "cancel queue").await;

    assert_eq!(
        timeout(Duration::from_secs(1), speech_started_receiver.recv())
            .await
            .expect("speech synthesis did not start")
            .expect("speech start channel closed"),
        "First."
    );
    for phrase in ["Second.", "Third.", "Fourth.", "Fifth."] {
        delta_sender.send(Ok(phrase.into())).await.unwrap();
    }

    interrupt(&runtime, turn_id).await.unwrap();
    drop(delta_sender);
    let observed = drain_events(&mut events).await;

    assert!(cleanup_completed.load(Ordering::Acquire));
    assert!(speech_started_receiver.try_recv().is_err());
    assert_eq!(
        observed
            .into_iter()
            .filter(RuntimeEvent::is_terminal)
            .collect::<Vec<_>>(),
        [RuntimeEvent::TurnCancelled { turn_id }]
    );
}

#[tokio::test]
async fn interruption_waits_for_active_output_cleanup() {
    let (output_started, mut output_started_receiver) = mpsc::unbounded_channel();
    let cleanup_completed = Arc::new(AtomicBool::new(false));
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["Speak."])),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(CleanupBlockingOutput {
            started: output_started,
            cleanup_completed: Arc::clone(&cleanup_completed),
        }),
    );
    let turn_id = TurnId::new(16);
    let mut events = start_turn(&runtime, turn_id, "cancel playback").await;

    timeout(Duration::from_secs(1), output_started_receiver.recv())
        .await
        .expect("audio output did not start")
        .expect("audio output start channel closed");
    interrupt(&runtime, turn_id).await.unwrap();
    let observed = drain_events(&mut events).await;

    assert!(cleanup_completed.load(Ordering::Acquire));
    assert_eq!(
        observed
            .into_iter()
            .filter(RuntimeEvent::is_terminal)
            .collect::<Vec<_>>(),
        [RuntimeEvent::TurnCancelled { turn_id }]
    );
}

#[tokio::test]
async fn language_failure_retains_its_stage_after_active_speech_cleanup() {
    let (delta_sender, delta_receiver) = mpsc::channel(2);
    delta_sender
        .send(Ok("First. Second. Third.".into()))
        .await
        .unwrap();
    let (speech_started, mut speech_started_receiver) = mpsc::unbounded_channel();
    let cleanup_completed = Arc::new(AtomicBool::new(false));
    let runtime = ConversationRuntime::new(
        Arc::new(ControlledLanguageModel::new(delta_receiver)),
        Arc::new(CleanupBlockingSpeech {
            started: speech_started,
            cleanup_completed: Arc::clone(&cleanup_completed),
        }),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(17);
    let mut events = start_turn(&runtime, turn_id, "language failure").await;

    assert_eq!(
        timeout(Duration::from_secs(1), speech_started_receiver.recv())
            .await
            .expect("speech synthesis did not start")
            .expect("speech start channel closed"),
        "First."
    );
    delta_sender
        .send(Err(AdapterError::new("language model unavailable")))
        .await
        .unwrap();
    delta_sender
        .send(Ok("cleanup sentinel".into()))
        .await
        .unwrap();
    drop(delta_sender);

    let observed = drain_events(&mut events).await;

    assert!(cleanup_completed.load(Ordering::Acquire));
    assert!(speech_started_receiver.try_recv().is_err());
    assert_eq!(
        observed
            .into_iter()
            .filter(RuntimeEvent::is_terminal)
            .collect::<Vec<_>>(),
        [RuntimeEvent::TurnFailed {
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
async fn synthesis_failure_cancels_and_cleans_active_generation() {
    let generation_cleanup = Arc::new(AtomicBool::new(false));
    let (speech_started, mut speech_started_receiver) = mpsc::unbounded_channel();
    let runtime = ConversationRuntime::new(
        Arc::new(CancellationCleanupLanguageModel {
            text: "Speak.".into(),
            cleanup_completed: Arc::clone(&generation_cleanup),
        }),
        Arc::new(FailingSpeech {
            started: speech_started,
        }),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(18);
    let mut events = start_turn(&runtime, turn_id, "synthesis failure").await;

    timeout(Duration::from_secs(1), speech_started_receiver.recv())
        .await
        .expect("speech synthesis did not start")
        .expect("speech start channel closed");
    let observed = drain_events(&mut events).await;

    assert!(generation_cleanup.load(Ordering::Acquire));
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
                "speech synthesizer unavailable",
            ),
        }]
    );
}

#[tokio::test]
async fn output_failure_cancels_generation_discards_queue_and_waits_for_cleanup() {
    let generation_cleanup = Arc::new(AtomicBool::new(false));
    let output_cleanup = Arc::new(AtomicBool::new(false));
    let (output_started, mut output_started_receiver) = mpsc::unbounded_channel();
    let (fail_output, output_failure) = oneshot::channel();
    let runtime = ConversationRuntime::new(
        Arc::new(CancellationCleanupLanguageModel {
            text: "First. Second. Third.".into(),
            cleanup_completed: Arc::clone(&generation_cleanup),
        }),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(GatedFailingOutput {
            started: output_started,
            fail: Mutex::new(Some(output_failure)),
            cleanup_completed: Arc::clone(&output_cleanup),
        }),
    );
    let turn_id = TurnId::new(19);
    let mut events = start_turn(&runtime, turn_id, "output failure").await;

    timeout(Duration::from_secs(1), output_started_receiver.recv())
        .await
        .expect("audio output did not start")
        .expect("audio output start channel closed");
    fail_output.send(()).unwrap();

    let observed = drain_events(&mut events).await;

    assert!(generation_cleanup.load(Ordering::Acquire));
    assert!(output_cleanup.load(Ordering::Acquire));
    assert!(output_started_receiver.try_recv().is_err());
    assert_eq!(
        observed
            .into_iter()
            .filter(RuntimeEvent::is_terminal)
            .collect::<Vec<_>>(),
        [RuntimeEvent::TurnFailed {
            turn_id,
            error: RuntimeError::new(
                RuntimeErrorKind::Adapter,
                RuntimeStage::AudioOutput,
                "audio output unavailable",
            ),
        }]
    );
}

#[tokio::test]
async fn output_failure_resolves_when_lifecycle_events_are_saturated() {
    let generation_cleanup = Arc::new(AtomicBool::new(false));
    let output_cleanup = Arc::new(AtomicBool::new(false));
    let (progress, mut progress_receiver) = mpsc::unbounded_channel();
    let (output_started, mut output_started_receiver) = mpsc::unbounded_channel();
    let (fail_output, output_failure) = oneshot::channel();
    let runtime = ConversationRuntime::new(
        Arc::new(SaturatingLanguageModel {
            progress,
            cleanup_completed: Arc::clone(&generation_cleanup),
        }),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(GatedFailingOutput {
            started: output_started,
            fail: Mutex::new(Some(output_failure)),
            cleanup_completed: Arc::clone(&output_cleanup),
        }),
    );
    let turn_id = TurnId::new(20);
    let mut events = start_turn(&runtime, turn_id, "saturate events").await;

    timeout(Duration::from_secs(1), output_started_receiver.recv())
        .await
        .expect("audio output did not start")
        .expect("audio output start channel closed");
    for expected in 1..=28 {
        assert_eq!(
            timeout(Duration::from_secs(1), progress_receiver.recv())
                .await
                .expect("language model did not saturate the event channel")
                .expect("language progress channel closed"),
            expected
        );
    }
    fail_output.send(()).unwrap();

    let observed = drain_events(&mut events).await;

    assert!(generation_cleanup.load(Ordering::Acquire));
    assert!(output_cleanup.load(Ordering::Acquire));
    assert_eq!(
        observed
            .into_iter()
            .filter(RuntimeEvent::is_terminal)
            .collect::<Vec<_>>(),
        [RuntimeEvent::TurnFailed {
            turn_id,
            error: RuntimeError::new(
                RuntimeErrorKind::Adapter,
                RuntimeStage::AudioOutput,
                "audio output unavailable",
            ),
        }]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn interruption_unblocks_worker_lifecycle_send_on_a_saturated_event_channel() {
    let (delta_sender, delta_receiver) = mpsc::channel(1);
    delta_sender.send(Ok("x".into())).await.unwrap();
    let synthesis_invoked = Arc::new(AtomicBool::new(false));
    let runtime = ConversationRuntime::new(
        Arc::new(ControlledLanguageModel::new(delta_receiver)),
        Arc::new(InvocationTrackingSpeech {
            invoked: Arc::clone(&synthesis_invoked),
        }),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(21);
    let mut events = start_turn(&runtime, turn_id, "blocked lifecycle").await;

    for _ in 0..27 {
        delta_sender.send(Ok("x".into())).await.unwrap();
    }
    delta_sender.send(Ok(".".into())).await.unwrap();
    delta_sender.send(Ok("tail".into())).await.unwrap();

    interrupt(&runtime, turn_id).await.unwrap();
    drop(delta_sender);
    let observed = drain_events(&mut events).await;

    assert!(!synthesis_invoked.load(Ordering::Acquire));
    assert_eq!(
        observed
            .into_iter()
            .filter(RuntimeEvent::is_terminal)
            .collect::<Vec<_>>(),
        [RuntimeEvent::TurnCancelled { turn_id }]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn language_failure_unblocks_worker_lifecycle_send_on_a_saturated_event_channel() {
    let (delta_sender, delta_receiver) = mpsc::channel(1);
    delta_sender.send(Ok("x".into())).await.unwrap();
    let synthesis_invoked = Arc::new(AtomicBool::new(false));
    let runtime = ConversationRuntime::new(
        Arc::new(ControlledLanguageModel::new(delta_receiver)),
        Arc::new(InvocationTrackingSpeech {
            invoked: Arc::clone(&synthesis_invoked),
        }),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(22);
    let mut events = start_turn(&runtime, turn_id, "blocked failure").await;

    for _ in 0..27 {
        delta_sender.send(Ok("x".into())).await.unwrap();
    }
    delta_sender.send(Ok(".".into())).await.unwrap();
    delta_sender
        .send(Err(AdapterError::new("language model unavailable")))
        .await
        .unwrap();
    delta_sender
        .send(Ok("cleanup sentinel".into()))
        .await
        .unwrap();
    drop(delta_sender);

    let observed = drain_events(&mut events).await;

    assert!(!synthesis_invoked.load(Ordering::Acquire));
    assert_eq!(
        observed
            .into_iter()
            .filter(RuntimeEvent::is_terminal)
            .collect::<Vec<_>>(),
        [RuntimeEvent::TurnFailed {
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
async fn runtime_reuses_after_every_terminal_outcome() {
    for first_outcome in [
        ReuseOutcome::Completion,
        ReuseOutcome::ExternalCancellation,
        ReuseOutcome::LanguageFailure,
        ReuseOutcome::SynthesisFailure,
        ReuseOutcome::OutputFailure,
    ] {
        let runtime = ConversationRuntime::new(
            Arc::new(ReusableLanguageModel { first_outcome }),
            Arc::new(ReusableSpeech { first_outcome }),
            Arc::new(ReusableOutput { first_outcome }),
        );
        let first_turn = TurnId::new(1);
        let mut first_events = start_turn(&runtime, first_turn, "first").await;

        if first_outcome == ReuseOutcome::ExternalCancellation {
            assert_eq!(
                first_events.recv().await,
                Some(RuntimeEvent::TurnStarted {
                    turn_id: first_turn
                })
            );
            interrupt(&runtime, first_turn).await.unwrap();
        }
        let first_observed = drain_events(&mut first_events).await;
        let first_terminal = first_observed
            .iter()
            .find(|event| event.is_terminal())
            .expect("first turn did not emit a terminal event");
        match first_outcome {
            ReuseOutcome::Completion => {
                assert_eq!(
                    first_terminal,
                    &RuntimeEvent::TurnCompleted {
                        turn_id: first_turn
                    }
                );
            }
            ReuseOutcome::ExternalCancellation => {
                assert_eq!(
                    first_terminal,
                    &RuntimeEvent::TurnCancelled {
                        turn_id: first_turn
                    }
                );
            }
            ReuseOutcome::LanguageFailure => assert!(matches!(
                first_terminal,
                RuntimeEvent::TurnFailed { error, .. }
                    if error.stage() == RuntimeStage::LanguageModel
            )),
            ReuseOutcome::SynthesisFailure => assert!(matches!(
                first_terminal,
                RuntimeEvent::TurnFailed { error, .. }
                    if error.stage() == RuntimeStage::SpeechSynthesizer
            )),
            ReuseOutcome::OutputFailure => assert!(matches!(
                first_terminal,
                RuntimeEvent::TurnFailed { error, .. }
                    if error.stage() == RuntimeStage::AudioOutput
            )),
        }

        let second_turn = TurnId::new(2);
        let mut second_events = start_turn(&runtime, second_turn, "second").await;
        let second_observed = drain_events(&mut second_events).await;
        assert_eq!(
            second_observed
                .iter()
                .filter(|event| event.is_terminal())
                .collect::<Vec<_>>(),
            [&RuntimeEvent::TurnCompleted {
                turn_id: second_turn
            }]
        );
    }
}

#[tokio::test]
async fn rejects_a_reused_turn_id() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["response"])),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(13);
    let mut events = start_turn(&runtime, turn_id, "first").await;

    while events.recv().await.is_some() {}

    let error = match runtime
        .execute(RuntimeCommand::StartTurn {
            turn_id,
            transcript: "reused".into(),
        })
        .await
    {
        Ok(_) => panic!("a reused turn identifier must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), RuntimeErrorKind::InvalidState);
}

#[tokio::test]
async fn rejects_a_second_active_turn() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::delayed(["late"], Duration::from_secs(5))),
        Arc::new(MockSpeechSynthesizer::new(minimal_aiff())),
        Arc::new(DiscardAudioOutput),
    );
    let first_turn = TurnId::new(1);
    let second_turn = TurnId::new(2);
    let _events = start_turn(&runtime, first_turn, "first").await;

    let error = match runtime
        .execute(RuntimeCommand::StartTurn {
            turn_id: second_turn,
            transcript: "second".into(),
        })
        .await
    {
        Ok(_) => panic!("a second active turn must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), RuntimeErrorKind::InvalidState);
    interrupt(&runtime, first_turn).await.unwrap();
}

#[tokio::test]
async fn interruption_result_matches_the_terminal_event_at_synthesis_boundary() {
    let synthesis_completed = Arc::new(AtomicBool::new(false));
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(vec!["x"; 27])),
        Arc::new(CompletionSignallingSpeech {
            completed: Arc::clone(&synthesis_completed),
        }),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(9);
    let mut events = start_turn(&runtime, turn_id, "fill the event buffer").await;

    while !synthesis_completed.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }

    let interruption = interrupt(&runtime, turn_id).await;

    let mut terminal_events = Vec::new();
    while let Some(event) = events.recv().await {
        if event.is_terminal() {
            terminal_events.push(event);
        }
    }

    match interruption {
        Ok(()) => assert_eq!(
            terminal_events,
            vec![RuntimeEvent::TurnCancelled { turn_id }]
        ),
        Err(_) => assert_eq!(
            terminal_events,
            vec![RuntimeEvent::TurnCompleted { turn_id }]
        ),
    }
}

#[tokio::test]
async fn interruption_finalizes_when_the_event_consumer_is_backpressured() {
    let synthesis_completed = Arc::new(AtomicBool::new(false));
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(vec!["x"; 27])),
        Arc::new(CompletionSignallingSpeech {
            completed: Arc::clone(&synthesis_completed),
        }),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(12);
    let mut events = start_turn(&runtime, turn_id, "fill the event buffer").await;

    while !synthesis_completed.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }

    interrupt(&runtime, turn_id).await.unwrap();

    let mut terminal_events = Vec::new();
    while let Some(event) = events.recv().await {
        if event.is_terminal() {
            terminal_events.push(event);
        }
    }

    assert_eq!(
        terminal_events,
        vec![RuntimeEvent::TurnCancelled { turn_id }]
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

async fn interrupt(
    runtime: &ConversationRuntime,
    turn_id: TurnId,
) -> Result<(), conversation_protocol::RuntimeError> {
    match runtime
        .execute(RuntimeCommand::Interrupt { turn_id })
        .await?
    {
        RuntimeCommandResult::InterruptAccepted => Ok(()),
        RuntimeCommandResult::TurnStarted { .. } => {
            panic!("interrupt command must not return a turn event stream")
        }
        _ => panic!("interrupt command returned an unknown result"),
    }
}

async fn drain_events(events: &mut TurnEventStream) -> Vec<RuntimeEvent> {
    let mut observed = Vec::new();
    while let Some(event) = events.recv().await {
        observed.push(event);
    }
    observed
}

async fn assert_reusable_after_stream_drop(runtime: &ConversationRuntime) {
    let mut events = timeout(Duration::from_secs(1), async {
        loop {
            match runtime
                .execute(RuntimeCommand::StartTurn {
                    turn_id: TurnId::new(31),
                    transcript: "reuse".into(),
                })
                .await
            {
                Ok(RuntimeCommandResult::TurnStarted { events }) => break events,
                Err(error) if error.kind() == RuntimeErrorKind::InvalidState => {
                    tokio::task::yield_now().await;
                }
                Ok(RuntimeCommandResult::InterruptAccepted) => {
                    panic!("start command returned interrupt result")
                }
                Ok(_) => panic!("start command returned unknown result"),
                Err(error) => panic!("reuse failed with unexpected error: {error}"),
            }
        }
    })
    .await
    .expect("runtime active turn was not cleared after stream drop");

    assert_eq!(
        drain_events(&mut events)
            .await
            .into_iter()
            .filter(RuntimeEvent::is_terminal)
            .collect::<Vec<_>>(),
        [RuntimeEvent::TurnCompleted {
            turn_id: TurnId::new(31)
        }]
    );
}
