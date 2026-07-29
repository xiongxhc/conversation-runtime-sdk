use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conversation_model_adapters::{
    AdapterError, AdapterFuture, MockContinuousAudioOutput, MockGenerationLanguageModel,
    MockStreamingSpeechSynthesizer, VoiceInput, VoiceInputEvent, VoiceIoFactory, VoiceIoSession,
};
use conversation_protocol::{
    ComponentDescriptor, ComponentKind, ExecutionLocation, GenerationId, PrivacyMode,
    RecoveryDisposition, RuntimeEvent, SessionId, TurnId, VoiceActivity, VoiceSessionEvent,
    VoiceSessionPolicy, VoiceTimingMilestone,
};
use conversation_runtime::{VoiceSessionAdapters, VoiceSessionEventStream, VoiceSessionRuntime};
use tokio::sync::{mpsc, Notify};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const SESSION_ID: SessionId = SessionId::new(1);

#[tokio::test(start_paused = true)]
async fn finalizes_each_utterance_from_the_silence_timer_with_increasing_identities() {
    let harness = VoiceSessionHarness::new();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    harness.partial(1, "hel").await;
    harness.partial(1, "hello").await;
    assert_partial(events.recv().await, 1, "hel");
    assert_partial(events.recv().await, 1, "hello");
    harness.engine_final(1, "hello").await;
    harness.speech_ended(0).await;

    tokio::time::advance(Duration::from_millis(599)).await;
    tokio::task::yield_now().await;
    assert!(harness.language.requests().is_empty());

    tokio::time::advance(Duration::from_millis(1)).await;
    let first = drain_until_turn_terminal(&mut events).await;
    assert_final_and_completed(&first, TurnId::new(1), "hello");
    assert_eq!(harness.language.requests().len(), 1);
    assert_eq!(harness.language.requests()[0].turn_id(), TurnId::new(1));
    assert_eq!(
        harness.language.requests()[0].generation_id(),
        GenerationId::new(1)
    );

    harness.partial(2, "again").await;
    assert_partial(events.recv().await, 2, "again");
    harness.engine_final(2, "again").await;
    harness.speech_ended(600).await;
    tokio::time::advance(Duration::from_millis(600)).await;

    let second = drain_until_turn_terminal(&mut events).await;
    assert_final_and_completed(&second, TurnId::new(2), "again");
    assert_eq!(harness.language.requests().len(), 2);
    assert_eq!(harness.language.requests()[1].turn_id(), TurnId::new(2));
    assert_eq!(
        harness.language.requests()[1].generation_id(),
        GenerationId::new(2)
    );

    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn deadline_fires_without_any_subsequent_input_event() {
    let harness = VoiceSessionHarness::new();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    harness.engine_final(4, "timer only").await;
    harness.speech_ended(0).await;
    tokio::time::advance(Duration::from_millis(600)).await;

    let observed = drain_until_turn_terminal(&mut events).await;
    assert_final_and_completed(&observed, TurnId::new(1), "timer only");
    assert_eq!(harness.language.requests().len(), 1);

    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn resumed_speech_disarms_and_rearms_the_deadline() {
    let harness = VoiceSessionHarness::new();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    harness.engine_final(5, "keep listening").await;
    harness.speech_ended(0).await;
    assert_voice_activity(events.recv().await, VoiceActivity::SpeechEnded { at_ms: 0 });
    assert_speech_end_timing(events.recv().await);
    tokio::time::advance(Duration::from_millis(599)).await;
    harness.speech_started(599).await;
    assert_voice_activity(
        events.recv().await,
        VoiceActivity::SpeechStarted { at_ms: 599 },
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(harness.language.requests().is_empty());

    harness.speech_ended(600).await;
    tokio::time::advance(Duration::from_millis(599)).await;
    tokio::task::yield_now().await;
    assert!(harness.language.requests().is_empty());
    tokio::time::advance(Duration::from_millis(1)).await;

    let observed = drain_until_turn_terminal(&mut events).await;
    assert_final_and_completed(&observed, TurnId::new(1), "keep listening");
    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn newer_same_segment_hypothesis_replaces_text_without_moving_deadline() {
    let harness = VoiceSessionHarness::new();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    harness.engine_final(6, "old").await;
    harness.speech_ended(0).await;
    tokio::time::advance(Duration::from_millis(300)).await;
    harness.engine_final(6, "replacement").await;
    tokio::time::advance(Duration::from_millis(300)).await;

    let observed = drain_until_turn_terminal(&mut events).await;
    assert_final_and_completed(&observed, TurnId::new(1), "replacement");
    assert_eq!(harness.language.requests()[0].transcript(), "replacement");
    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn replacement_segment_disarms_the_previous_segments_deadline() {
    let harness = VoiceSessionHarness::new();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    harness.engine_final(7, "old segment").await;
    harness.speech_ended(0).await;
    assert_voice_activity(events.recv().await, VoiceActivity::SpeechEnded { at_ms: 0 });
    assert_speech_end_timing(events.recv().await);
    tokio::time::advance(Duration::from_millis(300)).await;
    harness.partial(8, "new").await;
    assert_partial(events.recv().await, 8, "new");
    tokio::time::advance(Duration::from_millis(300)).await;
    tokio::task::yield_now().await;
    assert!(harness.language.requests().is_empty());

    harness.engine_final(8, "new segment").await;
    harness.speech_ended(600).await;
    tokio::time::advance(Duration::from_millis(600)).await;
    let observed = drain_until_turn_terminal(&mut events).await;
    assert_final_and_completed(&observed, TurnId::new(1), "new segment");
    harness.shutdown(&mut events).await;
}

#[tokio::test]
async fn rejected_policy_never_starts_the_voice_factory() {
    let harness = VoiceSessionHarness::new();
    let invalid_policy = VoiceSessionPolicy::new(
        SESSION_ID,
        PrivacyMode::LocalOnly,
        200,
        600,
        [component(
            ComponentKind::SpeechRecognition,
            "local-recognition",
        )],
    )
    .unwrap();

    let error = harness.runtime.start(invalid_policy).await.unwrap_err();

    assert_eq!(
        error.stage(),
        conversation_protocol::RuntimeStage::PrivacyPolicy
    );
    assert_eq!(harness.factory.start_count(), 0);
    assert!(!harness.factory.input_started.load(Ordering::Acquire));
}

struct VoiceSessionHarness {
    runtime: VoiceSessionRuntime,
    factory: Arc<TestVoiceIoFactory>,
    input: mpsc::Sender<Result<VoiceInputEvent, AdapterError>>,
    language: Arc<MockGenerationLanguageModel>,
}

impl VoiceSessionHarness {
    fn new() -> Self {
        let (input, input_receiver) = mpsc::channel(32);
        let output = Arc::new(MockContinuousAudioOutput::new());
        let factory = Arc::new(TestVoiceIoFactory::new(input_receiver, output));
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let speech = Arc::new(MockStreamingSpeechSynthesizer::new([]));
        let runtime = VoiceSessionRuntime::new(VoiceSessionAdapters::new(
            factory.clone(),
            language.clone(),
            speech,
        ));
        Self {
            runtime,
            factory,
            input,
            language,
        }
    }

    async fn start(&self) -> VoiceSessionEventStream {
        let events = self.runtime.start(policy()).await.unwrap();
        timeout(Duration::from_secs(1), self.factory.wait_for_input_start())
            .await
            .expect("voice input did not start");
        events
    }

    async fn partial(&self, segment_id: u64, text: &str) {
        self.send(VoiceInputEvent::Recognition(
            conversation_model_adapters::RecognitionEvent::Hypothesis(
                conversation_model_adapters::RecognitionHypothesis::partial(segment_id, text),
            ),
        ))
        .await;
    }

    async fn engine_final(&self, segment_id: u64, text: &str) {
        self.send(VoiceInputEvent::Recognition(
            conversation_model_adapters::RecognitionEvent::Hypothesis(
                conversation_model_adapters::RecognitionHypothesis::engine_final(segment_id, text),
            ),
        ))
        .await;
    }

    async fn speech_started(&self, at_ms: u64) {
        self.send(VoiceInputEvent::Activity(VoiceActivity::SpeechStarted {
            at_ms,
        }))
        .await;
    }

    async fn speech_ended(&self, at_ms: u64) {
        self.send(VoiceInputEvent::Activity(VoiceActivity::SpeechEnded {
            at_ms,
        }))
        .await;
    }

    async fn send(&self, event: VoiceInputEvent) {
        self.input.send(Ok(event)).await.unwrap();
        tokio::task::yield_now().await;
    }

    async fn shutdown(&self, events: &mut VoiceSessionEventStream) {
        self.runtime.shutdown().await.unwrap();
        let terminal = timeout(Duration::from_secs(1), async {
            while let Some(event) = events.recv().await {
                if event.is_session_terminal() {
                    return event;
                }
            }
            panic!("voice session ended without a terminal event");
        })
        .await
        .expect("voice session did not shut down");
        assert_eq!(
            terminal,
            VoiceSessionEvent::SessionEnded {
                session_id: SESSION_ID
            }
        );
        assert!(self.factory.completion_finished.load(Ordering::Acquire));
    }
}

struct TestVoiceIoFactory {
    input: Arc<TestVoiceInput>,
    output: Arc<MockContinuousAudioOutput>,
    start_count: AtomicUsize,
    input_started: Arc<AtomicBool>,
    completion_finished: Arc<AtomicBool>,
}

impl TestVoiceIoFactory {
    fn new(
        input: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
        output: Arc<MockContinuousAudioOutput>,
    ) -> Self {
        let input_started = Arc::new(AtomicBool::new(false));
        Self {
            input: Arc::new(TestVoiceInput {
                receiver: Mutex::new(Some(input)),
                started: Arc::clone(&input_started),
                started_notify: Notify::new(),
            }),
            output,
            start_count: AtomicUsize::new(0),
            input_started,
            completion_finished: Arc::new(AtomicBool::new(false)),
        }
    }

    fn start_count(&self) -> usize {
        self.start_count.load(Ordering::Acquire)
    }

    async fn wait_for_input_start(&self) {
        while !self.input_started.load(Ordering::Acquire) {
            self.input.started_notify.notified().await;
        }
    }
}

impl VoiceIoFactory for TestVoiceIoFactory {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, VoiceIoSession> {
        Box::pin(async move {
            self.start_count.fetch_add(1, Ordering::AcqRel);
            let completion_finished = Arc::clone(&self.completion_finished);
            Ok(VoiceIoSession {
                input: self.input.clone(),
                output: self.output.clone(),
                completion: tokio::spawn(async move {
                    cancellation.cancelled().await;
                    completion_finished.store(true, Ordering::Release);
                    Ok(())
                }),
            })
        })
    }
}

struct TestVoiceInput {
    receiver: Mutex<Option<mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>>>,
    started: Arc<AtomicBool>,
    started_notify: Notify,
}

impl VoiceInput for TestVoiceInput {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>> {
        Box::pin(async move {
            self.started.store(true, Ordering::Release);
            self.started_notify.notify_waiters();
            self.receiver
                .lock()
                .expect("voice input receiver lock poisoned")
                .take()
                .ok_or_else(|| AdapterError::new("voice input already started"))
        })
    }
}

fn policy() -> VoiceSessionPolicy {
    VoiceSessionPolicy::new(
        SESSION_ID,
        PrivacyMode::LocalOnly,
        200,
        600,
        [
            component(ComponentKind::SpeechRecognition, "local-recognition"),
            component(ComponentKind::LanguageModel, "local-language"),
            component(ComponentKind::SpeechSynthesis, "local-speech"),
            component(ComponentKind::AudioIo, "local-audio"),
        ],
    )
    .unwrap()
}

fn component(kind: ComponentKind, provider: &str) -> ComponentDescriptor {
    ComponentDescriptor::new(kind, provider, ExecutionLocation::Local)
}

fn assert_session_started(event: Option<VoiceSessionEvent>) {
    assert!(matches!(
        event,
        Some(VoiceSessionEvent::SessionStarted {
            session_id: SESSION_ID,
            ..
        })
    ));
}

fn assert_partial(event: Option<VoiceSessionEvent>, segment_id: u64, expected: &str) {
    assert_eq!(
        event,
        Some(VoiceSessionEvent::TranscriptPartial {
            session_id: SESSION_ID,
            segment_id,
            text: expected.to_owned(),
        })
    );
}

fn assert_voice_activity(event: Option<VoiceSessionEvent>, expected: VoiceActivity) {
    assert_eq!(
        event,
        Some(VoiceSessionEvent::VoiceActivity {
            session_id: SESSION_ID,
            activity: expected,
        })
    );
}

fn assert_speech_end_timing(event: Option<VoiceSessionEvent>) {
    assert!(matches!(
        event,
        Some(VoiceSessionEvent::Timing {
            session_id: SESSION_ID,
            milestone: VoiceTimingMilestone::SpeechEnd,
            ..
        })
    ));
}

fn assert_final_and_completed(observed: &[VoiceSessionEvent], turn_id: TurnId, text: &str) {
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event, VoiceSessionEvent::TranscriptFinal { .. }))
            .count(),
        1
    );
    assert!(observed.contains(&VoiceSessionEvent::TranscriptFinal {
        session_id: SESSION_ID,
        turn_id,
        text: text.to_owned(),
    }));
    assert!(observed.contains(&VoiceSessionEvent::Turn {
        session_id: SESSION_ID,
        event: RuntimeEvent::TurnStarted { turn_id },
    }));
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(
                event,
                VoiceSessionEvent::Turn {
                    event: RuntimeEvent::TurnCompleted {
                        turn_id: completed,
                    },
                    ..
                } if *completed == turn_id
            ))
            .count(),
        1
    );
    assert!(!observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::SessionFailed {
            recovery: RecoveryDisposition::NewSession,
            ..
        }
    )));
}

async fn drain_until_turn_terminal(events: &mut VoiceSessionEventStream) -> Vec<VoiceSessionEvent> {
    timeout(Duration::from_secs(1), async {
        let mut observed = Vec::new();
        while let Some(event) = events.recv().await {
            let is_terminal = matches!(
                event,
                VoiceSessionEvent::Turn {
                    event: RuntimeEvent::TurnCompleted { .. }
                        | RuntimeEvent::TurnCancelled { .. }
                        | RuntimeEvent::TurnFailed { .. },
                    ..
                }
            );
            observed.push(event);
            if is_terminal {
                return observed;
            }
        }
        panic!("voice session ended before the turn terminal");
    })
    .await
    .expect("turn terminal timed out")
}
