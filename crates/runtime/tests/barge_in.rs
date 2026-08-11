use std::collections::BTreeSet;
use std::future::pending;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioFrame, ContinuousAudioOutput, GenerationLanguageModel,
    GenerationLanguageRequest, GenerationTextDelta, MockVoiceCaptureControl, PcmFormat,
    PcmSampleFormat, PlaybackReceipt, RecognitionEvent, RecognitionHypothesis,
    StreamingSpeechRequest, StreamingSpeechSynthesizer, VoiceInput, VoiceInputEvent,
    VoiceIoFactory, VoiceIoSession,
};
use conversation_protocol::{
    ComponentDescriptor, ComponentKind, ConversationMode, ExecutionLocation, GenerationId,
    PersonaProfile, PlaybackState, PrivacyMode, RecoveryDisposition, ResponseControls,
    RuntimeErrorKind, RuntimeEvent, RuntimeStage, SessionId, TurnId, VoiceActivity,
    VoiceSessionEvent, VoiceSessionPolicy,
};
use conversation_runtime::{
    ConversationContext, ConversationQualityController, VoiceSessionAdapters,
    VoiceSessionEventStream, VoiceSessionRuntime,
};
use tokio::sync::{mpsc, Notify};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const SESSION_ID: SessionId = SessionId::new(1);

fn context() -> ConversationContext {
    ConversationContext::new(ConversationQualityController::new(
        PersonaProfile::default(),
        ResponseControls::default(),
        ConversationMode::DirectAnswer,
    ))
}

#[tokio::test(start_paused = true)]
async fn sidecar_barge_in_flushes_and_cancels_all_generation_work() {
    let harness = VoiceSessionHarness::speaking_generations([GenerationId::new(4)]);
    let mut events = harness.start().await;
    harness
        .start_speaking_generation(&mut events, GenerationId::new(4))
        .await;

    harness
        .emit_barge_in(TurnId::new(4), GenerationId::new(4))
        .await;
    let observed = drain_until_turn_terminal(&mut events).await;

    assert!(observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::BargeIn { generation_id, .. }
            if *generation_id == GenerationId::new(4)
    )));
    assert_eq!(harness.output.flushed(), vec![GenerationId::new(4)]);
    assert!(harness.language.cleanup_finished(GenerationId::new(4)));
    assert!(harness.speech.cleanup_finished(GenerationId::new(4)));
    assert!(harness.output.queued_frames().is_empty());
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(
                event,
                VoiceSessionEvent::Turn {
                    event: RuntimeEvent::TurnCancelled { .. },
                    ..
                }
            ))
            .count(),
        1
    );
    assert_no_late_generation_work(&observed);
    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn flush_waking_pending_enqueue_still_yields_exact_cancellation() {
    let harness = VoiceSessionHarness::speaking_generations([GenerationId::new(1)])
        .with_flush_waking_enqueue_failure();
    let mut events = harness.start().await;
    harness
        .start_speaking_generation(&mut events, GenerationId::new(1))
        .await;

    harness
        .emit_barge_in(TurnId::new(1), GenerationId::new(1))
        .await;
    let observed = drain_until_turn_terminal(&mut events).await;

    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(
                event,
                VoiceSessionEvent::Turn {
                    event: RuntimeEvent::TurnCancelled {
                        turn_id,
                    },
                    ..
                } if *turn_id == TurnId::new(1)
            ))
            .count(),
        1
    );
    assert!(!observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::Turn {
            event: RuntimeEvent::TurnCompleted { .. } | RuntimeEvent::TurnFailed { .. },
            ..
        }
    )));
    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn repeated_barge_in_publishes_one_cancellation_and_one_flush() {
    let harness = VoiceSessionHarness::speaking_generations([GenerationId::new(1)]);
    let mut events = harness.start().await;
    harness
        .start_speaking_generation(&mut events, GenerationId::new(1))
        .await;

    harness
        .emit_barge_in(TurnId::new(1), GenerationId::new(1))
        .await;
    harness
        .emit_barge_in(TurnId::new(1), GenerationId::new(1))
        .await;
    let observed = drain_until_turn_terminal(&mut events).await;
    tokio::task::yield_now().await;

    assert_eq!(harness.output.flushed(), vec![GenerationId::new(1)]);
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(
                event,
                VoiceSessionEvent::Turn {
                    event: RuntimeEvent::TurnCancelled { turn_id },
                    ..
                } if *turn_id == TurnId::new(1)
            ))
            .count(),
        1
    );
    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn stale_barge_in_does_not_cancel_replacement_generation() {
    let harness =
        VoiceSessionHarness::speaking_generations([GenerationId::new(1), GenerationId::new(2)]);
    let mut events = harness.start().await;
    harness
        .start_speaking_generation(&mut events, GenerationId::new(1))
        .await;
    harness
        .runtime
        .barge_in(TurnId::new(1), GenerationId::new(1))
        .await
        .unwrap();
    drain_until_turn_terminal(&mut events).await;

    harness
        .start_speaking_generation(&mut events, GenerationId::new(2))
        .await;
    harness
        .runtime
        .barge_in(TurnId::new(1), GenerationId::new(1))
        .await
        .unwrap();

    assert!(!harness.language.cleanup_finished(GenerationId::new(2)));
    assert!(!harness.speech.cleanup_finished(GenerationId::new(2)));
    assert_eq!(harness.output.flushed(), vec![GenerationId::new(1)]);

    harness
        .runtime
        .barge_in(TurnId::new(2), GenerationId::new(2))
        .await
        .unwrap();
    let observed = drain_until_turn_terminal(&mut events).await;
    assert!(observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::BargeIn {
            turn_id,
            generation_id,
            ..
        } if *turn_id == TurnId::new(2) && *generation_id == GenerationId::new(2)
    )));
    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn stale_rendered_is_discarded_while_replacement_generation_responds() {
    let harness =
        VoiceSessionHarness::speaking_generations([GenerationId::new(1), GenerationId::new(2)]);
    let mut events = harness.start().await;
    harness
        .start_speaking_generation(&mut events, GenerationId::new(1))
        .await;
    harness
        .runtime
        .barge_in(TurnId::new(1), GenerationId::new(1))
        .await
        .unwrap();
    drain_until_turn_terminal(&mut events).await;

    harness
        .start_speaking_generation(&mut events, GenerationId::new(2))
        .await;
    loop {
        tokio::select! {
            event = events.recv() => {
                assert!(event.is_some(), "replacement generation event stream closed");
            }
            _ = tokio::time::sleep(Duration::from_millis(1)) => break,
        }
    }
    harness
        .emit_playback(PlaybackReceipt::new(
            GenerationId::new(1),
            PlaybackState::Rendered,
        ))
        .await;
    tokio::select! {
        event = events.recv() => {
            assert!(!matches!(
                event,
                Some(VoiceSessionEvent::Playback {
                    generation_id,
                    state: PlaybackState::Rendered,
                    ..
                }) if generation_id == GenerationId::new(1)
            ));
        }
        _ = tokio::time::sleep(Duration::from_millis(1)) => {}
    }
    harness
        .runtime
        .barge_in(TurnId::new(2), GenerationId::new(2))
        .await
        .unwrap();
    let observed = drain_until_turn_terminal(&mut events).await;

    assert!(!observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::Playback {
            generation_id,
            state: PlaybackState::Rendered,
            ..
        } if *generation_id == GenerationId::new(1)
    )));
    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn barge_in_cleanup_is_not_blocked_by_full_lifecycle_output() {
    let harness = VoiceSessionHarness::speaking_generations([GenerationId::new(1)]);
    let mut events = harness.start().await;
    harness
        .start_speaking_generation_without_draining(&mut events, GenerationId::new(1))
        .await;

    timeout(
        Duration::from_secs(1),
        harness
            .runtime
            .barge_in(TurnId::new(1), GenerationId::new(1)),
    )
    .await
    .expect("barge-in blocked behind lifecycle output")
    .unwrap();

    assert!(harness.language.cleanup_finished(GenerationId::new(1)));
    assert!(harness.speech.cleanup_finished(GenerationId::new(1)));
    let observed = drain_until_turn_terminal(&mut events).await;
    assert!(observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::BargeIn {
            generation_id,
            ..
        } if *generation_id == GenerationId::new(1)
    )));
    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn barge_in_purges_deferred_partials_before_cancelled_terminal() {
    let harness = VoiceSessionHarness::speaking_generations([GenerationId::new(1)]);
    let mut events = harness.start().await;
    harness
        .start_speaking_generation_without_draining(&mut events, GenerationId::new(1))
        .await;

    for segment_id in 100..196 {
        harness
            .input
            .send(Ok(VoiceInputEvent::Recognition(
                RecognitionEvent::Hypothesis(RecognitionHypothesis::partial(
                    segment_id,
                    format!("deferred-{segment_id}"),
                )),
            )))
            .await
            .unwrap();
        tokio::task::yield_now().await;
    }

    harness
        .runtime
        .barge_in(TurnId::new(1), GenerationId::new(1))
        .await
        .unwrap();
    let observed = drain_until_turn_terminal(&mut events).await;

    let barge_in_index = observed
        .iter()
        .position(|event| {
            matches!(
                event,
                VoiceSessionEvent::BargeIn { generation_id, .. }
                    if *generation_id == GenerationId::new(1)
            )
        })
        .unwrap();
    let cancellation_index = observed
        .iter()
        .position(|event| {
            matches!(
                event,
                VoiceSessionEvent::Turn {
                    event: RuntimeEvent::TurnCancelled { turn_id },
                    ..
                } if *turn_id == TurnId::new(1)
            )
        })
        .unwrap();
    assert!(barge_in_index < cancellation_index);

    tokio::task::yield_now().await;
    tokio::select! {
        event = events.recv() => panic!("cancelled turn emitted deferred event after terminal: {event:?}"),
        _ = tokio::time::sleep(Duration::from_millis(1)) => {}
    }

    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn dropped_event_consumer_cleans_turn_and_sidecar_work() {
    let harness = VoiceSessionHarness::speaking_generations([GenerationId::new(1)]);
    let mut events = harness.start().await;
    harness
        .start_speaking_generation_without_draining(&mut events, GenerationId::new(1))
        .await;

    drop(events);

    timeout(
        Duration::from_secs(1),
        harness.factory.wait_for_completion(),
    )
    .await
    .expect("dropped consumer did not finish sidecar cleanup");
    assert!(harness.language.cleanup_finished(GenerationId::new(1)));
    assert!(harness.speech.cleanup_finished(GenerationId::new(1)));
    assert!(harness.output.queued_frames().is_empty());
}

#[tokio::test(start_paused = true)]
async fn flush_failure_still_cleans_the_turn_then_ends_the_session() {
    let harness =
        VoiceSessionHarness::speaking_generations([GenerationId::new(1)]).with_flush_failure();
    let mut events = harness.start().await;
    harness
        .start_speaking_generation(&mut events, GenerationId::new(1))
        .await;

    harness
        .runtime
        .barge_in(TurnId::new(1), GenerationId::new(1))
        .await
        .unwrap();
    let observed = drain_until_session_terminal(&mut events).await;

    assert!(harness.language.cleanup_finished(GenerationId::new(1)));
    assert!(harness.speech.cleanup_finished(GenerationId::new(1)));
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(
                event,
                VoiceSessionEvent::Turn {
                    event: RuntimeEvent::TurnCancelled { .. },
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::SessionFailed {
            error,
            recovery: RecoveryDisposition::NewSession,
            ..
        } if error.stage() == RuntimeStage::ContinuousAudioOutput
    )));
}

#[tokio::test(start_paused = true)]
async fn stalled_flush_times_out_to_one_new_session_terminal() {
    let harness =
        VoiceSessionHarness::speaking_generations([GenerationId::new(1)]).with_stalled_flush();
    let mut events = harness.start().await;
    harness
        .start_speaking_generation(&mut events, GenerationId::new(1))
        .await;

    harness
        .emit_barge_in(TurnId::new(1), GenerationId::new(1))
        .await;
    tokio::time::advance(Duration::from_secs(2)).await;
    let observed = drain_until_session_terminal(&mut events).await;

    assert_new_session_cleanup_timeout(&observed);
    assert!(harness.language.cleanup_finished(GenerationId::new(1)));
    assert!(harness.speech.cleanup_finished(GenerationId::new(1)));
}

#[tokio::test(start_paused = true)]
async fn stalled_turn_drain_times_out_to_one_new_session_terminal() {
    let harness = VoiceSessionHarness::speaking_generations([GenerationId::new(1)])
        .with_stalled_turn_cleanup();
    let mut events = harness.start().await;
    harness
        .start_speaking_generation(&mut events, GenerationId::new(1))
        .await;

    harness
        .emit_barge_in(TurnId::new(1), GenerationId::new(1))
        .await;
    tokio::time::advance(Duration::from_secs(2)).await;
    let observed = drain_until_session_terminal(&mut events).await;

    assert_new_session_cleanup_timeout(&observed);
    assert!(
        harness.output.stalled_work_stopped(),
        "session terminal published before stalled turn work was aborted"
    );
}

#[tokio::test(start_paused = true)]
async fn stalled_sidecar_completion_is_aborted_after_cleanup_timeout() {
    let harness = VoiceSessionHarness::speaking_generations([]).with_stalled_completion();
    let mut events = harness.start().await;
    let runtime = harness.runtime.clone();
    let shutdown = tokio::spawn(async move { runtime.shutdown().await });
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_secs(2)).await;
    timeout(Duration::from_secs(1), shutdown)
        .await
        .expect("sidecar completion cleanup did not time out")
        .expect("shutdown task panicked")
        .expect("shutdown command failed");
    let observed = drain_until_session_terminal(&mut events).await;

    assert_new_session_cleanup_timeout(&observed);
    assert!(harness.factory.completion_finished.load(Ordering::Acquire));
}

#[tokio::test(start_paused = true)]
async fn recognition_failure_recovers_to_listening() {
    let harness = VoiceSessionHarness::speaking_generations([]);
    let mut events = harness.start().await;

    harness
        .input
        .send(Err(AdapterError::new("voice sidecar recognition failed")
            .with_stage(RuntimeStage::SpeechRecognizer)))
        .await
        .unwrap();
    let failure = events
        .recv()
        .await
        .expect("recognition failure was dropped");
    assert!(matches!(
        failure,
        VoiceSessionEvent::SessionFailed {
            error,
            recovery: RecoveryDisposition::ContinueSession,
            ..
        } if error.stage() == RuntimeStage::SpeechRecognizer
            && error.kind() == RuntimeErrorKind::Adapter
    ));

    harness.finalize_utterance(1, "recovered", 0).await;
    let observed = drain_until_turn_terminal(&mut events).await;
    assert!(observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::Turn {
            event: RuntimeEvent::TurnCompleted { .. },
            ..
        }
    )));
    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn recognition_failure_during_response_cleans_the_turn_before_recovery() {
    let harness = VoiceSessionHarness::speaking_generations([GenerationId::new(1)]);
    let mut events = harness.start().await;
    harness
        .start_speaking_generation(&mut events, GenerationId::new(1))
        .await;

    harness
        .input
        .send(Err(AdapterError::new("voice sidecar recognition failed")
            .with_stage(RuntimeStage::SpeechRecognizer)))
        .await
        .unwrap();
    let observed = drain_until_turn_recovery(&mut events).await;

    assert!(harness.language.cleanup_finished(GenerationId::new(1)));
    assert!(harness.speech.cleanup_finished(GenerationId::new(1)));
    assert!(observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::SessionFailed {
            error,
            recovery: RecoveryDisposition::ContinueSession,
            ..
        } if error.stage() == RuntimeStage::SpeechRecognizer
    )));
    assert!(observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::Turn {
            event: RuntimeEvent::TurnCancelled {
                turn_id,
            },
            ..
        } if *turn_id == TurnId::new(1)
    )));
    harness.shutdown(&mut events).await;
}

#[tokio::test]
async fn sidecar_failure_is_session_fatal() {
    let harness = VoiceSessionHarness::speaking_generations([]);
    let mut events = harness.start().await;

    harness.factory.fail_sidecar().await;
    let observed = drain_until_session_terminal(&mut events).await;

    assert_eq!(
        observed
            .iter()
            .filter(|event| event.is_session_terminal())
            .count(),
        1
    );
    assert!(observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::SessionFailed {
            error,
            recovery: RecoveryDisposition::NewSession,
            ..
        } if error.stage() == RuntimeStage::VoiceSidecar
    )));
    assert!(harness.factory.completion_finished.load(Ordering::Acquire));
}

fn assert_no_late_generation_work(observed: &[VoiceSessionEvent]) {
    assert!(!observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::Turn {
            event: RuntimeEvent::TextDelta { delta, .. },
            ..
        } if delta == "late-cancelled-generation"
    )));
    assert!(!observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::Playback {
            state: PlaybackState::Accepted,
            ..
        }
    )));
}

fn assert_new_session_cleanup_timeout(observed: &[VoiceSessionEvent]) {
    assert_eq!(
        observed
            .iter()
            .filter(|event| event.is_session_terminal())
            .count(),
        1
    );
    assert!(observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::SessionFailed {
            error,
            recovery: RecoveryDisposition::NewSession,
            ..
        } if error.stage() == RuntimeStage::Runtime
            && error.message().contains("cleanup timed out")
    )));
}

struct VoiceSessionHarness {
    runtime: VoiceSessionRuntime,
    factory: Arc<TestVoiceIoFactory>,
    input: mpsc::Sender<Result<VoiceInputEvent, AdapterError>>,
    language: Arc<CancellableLanguage>,
    speech: Arc<CancellableSpeech>,
    output: Arc<CancellableOutput>,
    elapsed_ms: AtomicUsize,
}

impl VoiceSessionHarness {
    fn speaking_generations<I>(active_generations: I) -> Self
    where
        I: IntoIterator<Item = GenerationId>,
    {
        Self::configured(active_generations, FlushBehavior::Succeed, false, false)
    }

    fn configured<I>(
        active_generations: I,
        flush_behavior: FlushBehavior,
        stall_turn_cleanup: bool,
        stall_completion: bool,
    ) -> Self
    where
        I: IntoIterator<Item = GenerationId>,
    {
        let active_generations: BTreeSet<_> = active_generations.into_iter().collect();
        let context = context();
        let (input, input_receiver) = mpsc::channel(64);
        let output = Arc::new(CancellableOutput::new(flush_behavior, stall_turn_cleanup));
        let language = Arc::new(CancellableLanguage::new(active_generations.clone()));
        let speech = Arc::new(CancellableSpeech::new(active_generations));
        let factory = Arc::new(TestVoiceIoFactory::new(
            input_receiver,
            output.clone(),
            stall_completion,
        ));
        let runtime = VoiceSessionRuntime::new(
            context,
            VoiceSessionAdapters::new(factory.clone(), language.clone(), speech.clone()),
        );
        Self {
            runtime,
            factory,
            input,
            language,
            speech,
            output,
            elapsed_ms: AtomicUsize::new(0),
        }
    }

    fn with_flush_failure(self) -> Self {
        Self::configured(
            self.language.active_generations.iter().copied(),
            FlushBehavior::Fail,
            false,
            false,
        )
    }

    fn with_flush_waking_enqueue_failure(self) -> Self {
        Self::configured(
            self.language.active_generations.iter().copied(),
            FlushBehavior::WakeEnqueueFailure,
            false,
            false,
        )
    }

    fn with_stalled_flush(self) -> Self {
        Self::configured(
            self.language.active_generations.iter().copied(),
            FlushBehavior::Stall,
            false,
            false,
        )
    }

    fn with_stalled_turn_cleanup(self) -> Self {
        Self::configured(
            self.language.active_generations.iter().copied(),
            FlushBehavior::Succeed,
            true,
            false,
        )
    }

    fn with_stalled_completion(self) -> Self {
        Self::configured(
            self.language.active_generations.iter().copied(),
            FlushBehavior::Succeed,
            false,
            true,
        )
    }

    async fn start(&self) -> VoiceSessionEventStream {
        let mut events = self.runtime.start(policy()).await.unwrap();
        timeout(Duration::from_secs(1), self.factory.wait_for_input_start())
            .await
            .expect("voice input did not start");
        assert!(matches!(
            events.recv().await,
            Some(VoiceSessionEvent::SessionStarted {
                session_id: SESSION_ID,
                ..
            })
        ));
        events
    }

    async fn start_speaking_generation(
        &self,
        events: &mut VoiceSessionEventStream,
        target: GenerationId,
    ) {
        let target_value = target.get();
        let first_generation = self.elapsed_ms.load(Ordering::Acquire) as u64 / 600 + 1;
        for generation_value in first_generation..=target_value {
            let generation_id = GenerationId::new(generation_value);
            self.finalize_utterance(
                generation_value,
                &format!("utterance {generation_value}"),
                self.elapsed_ms.load(Ordering::Acquire) as u64,
            )
            .await;
            if generation_id == target {
                let expected_enqueues = self
                    .language
                    .active_generations
                    .range(..=generation_id)
                    .count();
                self.output.wait_for_enqueue_count(expected_enqueues).await;
            } else {
                let observed = drain_until_turn_terminal(events).await;
                assert!(observed.iter().any(|event| matches!(
                    event,
                    VoiceSessionEvent::Turn {
                        event: RuntimeEvent::TurnCompleted { turn_id },
                        ..
                    } if *turn_id == TurnId::new(generation_value)
                )));
            }
        }
    }

    async fn start_speaking_generation_without_draining(
        &self,
        _events: &mut VoiceSessionEventStream,
        generation_id: GenerationId,
    ) {
        assert_eq!(generation_id, GenerationId::new(1));
        self.finalize_utterance(1, "saturated", 0).await;
        self.output.wait_for_enqueue_count(1).await;
    }

    async fn finalize_utterance(&self, segment_id: u64, text: &str, at_ms: u64) {
        self.input
            .send(Ok(VoiceInputEvent::Activity(
                VoiceActivity::SpeechStarted { at_ms },
            )))
            .await
            .unwrap();
        self.input
            .send(Ok(VoiceInputEvent::Recognition(
                RecognitionEvent::Hypothesis(RecognitionHypothesis::engine_final(segment_id, text)),
            )))
            .await
            .unwrap();
        self.input
            .send(Ok(VoiceInputEvent::Activity(VoiceActivity::SpeechEnded {
                at_ms,
            })))
            .await
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(600)).await;
        self.elapsed_ms.fetch_add(600, Ordering::AcqRel);
        tokio::task::yield_now().await;
    }

    async fn emit_barge_in(&self, turn_id: TurnId, generation_id: GenerationId) {
        assert_eq!(turn_id.get(), generation_id.get());
        self.input
            .send(Ok(VoiceInputEvent::Activity(
                VoiceActivity::SpeechStarted {
                    at_ms: self.elapsed_ms.load(Ordering::Acquire) as u64,
                },
            )))
            .await
            .unwrap();
        tokio::task::yield_now().await;
    }

    async fn emit_playback(&self, receipt: PlaybackReceipt) {
        self.input
            .send(Ok(VoiceInputEvent::Playback(receipt)))
            .await
            .unwrap();
        tokio::task::yield_now().await;
    }

    async fn shutdown(&self, events: &mut VoiceSessionEventStream) {
        self.runtime.shutdown().await.unwrap();
        let terminal = drain_until_session_terminal(events).await;
        assert!(terminal.iter().any(|event| matches!(
            event,
            VoiceSessionEvent::SessionEnded {
                session_id: SESSION_ID
            }
        )));
    }
}

struct CancellableLanguage {
    active_generations: BTreeSet<GenerationId>,
    cleaned: Arc<Mutex<BTreeSet<GenerationId>>>,
}

impl CancellableLanguage {
    fn new(active_generations: BTreeSet<GenerationId>) -> Self {
        Self {
            active_generations,
            cleaned: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    fn cleanup_finished(&self, generation_id: GenerationId) -> bool {
        self.cleaned
            .lock()
            .expect("language cleanup lock poisoned")
            .contains(&generation_id)
    }
}

impl GenerationLanguageModel for CancellableLanguage {
    fn stream(
        &self,
        request: GenerationLanguageRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        if !self.active_generations.contains(&request.generation_id()) {
            return receiver;
        }

        let cleaned = Arc::clone(&self.cleaned);
        tokio::spawn(async move {
            for _ in 0..128 {
                if sender
                    .send(Ok(GenerationTextDelta::new(
                        request.turn_id(),
                        request.generation_id(),
                        "xxxxxxxxxx",
                    )))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            cancellation.cancelled().await;
            let _ = sender
                .send(Ok(GenerationTextDelta::new(
                    request.turn_id(),
                    request.generation_id(),
                    "late-cancelled-generation",
                )))
                .await;
            cleaned
                .lock()
                .expect("language cleanup lock poisoned")
                .insert(request.generation_id());
        });
        receiver
    }
}

struct CancellableSpeech {
    active_generations: BTreeSet<GenerationId>,
    cleaned: Arc<Mutex<BTreeSet<GenerationId>>>,
}

impl CancellableSpeech {
    fn new(active_generations: BTreeSet<GenerationId>) -> Self {
        Self {
            active_generations,
            cleaned: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    fn cleanup_finished(&self, generation_id: GenerationId) -> bool {
        self.cleaned
            .lock()
            .expect("speech cleanup lock poisoned")
            .contains(&generation_id)
    }
}

impl StreamingSpeechSynthesizer for CancellableSpeech {
    fn stream(
        &self,
        request: StreamingSpeechRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<AudioFrame, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        if !self.active_generations.contains(&request.generation_id()) {
            return receiver;
        }

        let cleaned = Arc::clone(&self.cleaned);
        tokio::spawn(async move {
            if sender.send(Ok(frame(&request, 0))).await.is_err() {
                return;
            }
            cancellation.cancelled().await;
            let _ = sender.send(Ok(frame(&request, 1))).await;
            cleaned
                .lock()
                .expect("speech cleanup lock poisoned")
                .insert(request.generation_id());
        });
        receiver
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlushBehavior {
    Succeed,
    Fail,
    WakeEnqueueFailure,
    Stall,
}

struct CancellableOutput {
    flush_behavior: FlushBehavior,
    stall_turn_cleanup: bool,
    stalled_work_stopped: Arc<AtomicBool>,
    frames: Mutex<Vec<AudioFrame>>,
    flushes: Mutex<Vec<GenerationId>>,
    enqueue_count: AtomicUsize,
    enqueue_notify: Notify,
    flush_wakeup: Notify,
}

impl CancellableOutput {
    fn new(flush_behavior: FlushBehavior, stall_turn_cleanup: bool) -> Self {
        Self {
            flush_behavior,
            stall_turn_cleanup,
            stalled_work_stopped: Arc::new(AtomicBool::new(false)),
            frames: Mutex::new(Vec::new()),
            flushes: Mutex::new(Vec::new()),
            enqueue_count: AtomicUsize::new(0),
            enqueue_notify: Notify::new(),
            flush_wakeup: Notify::new(),
        }
    }

    fn flushed(&self) -> Vec<GenerationId> {
        self.flushes.lock().expect("flush lock poisoned").clone()
    }

    fn queued_frames(&self) -> Vec<AudioFrame> {
        self.frames.lock().expect("frame lock poisoned").clone()
    }

    fn stalled_work_stopped(&self) -> bool {
        self.stalled_work_stopped.load(Ordering::Acquire)
    }

    async fn wait_for_enqueue_count(&self, expected: usize) {
        while self.enqueue_count.load(Ordering::Acquire) < expected {
            self.enqueue_notify.notified().await;
        }
    }
}

impl ContinuousAudioOutput for CancellableOutput {
    fn enqueue<'a>(
        &'a self,
        frame: AudioFrame,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, PlaybackReceipt> {
        Box::pin(async move {
            let _stalled_work = StalledWorkGuard {
                stopped: self
                    .stall_turn_cleanup
                    .then(|| Arc::clone(&self.stalled_work_stopped)),
            };
            self.frames
                .lock()
                .expect("frame lock poisoned")
                .push(frame.clone());
            self.enqueue_count.fetch_add(1, Ordering::AcqRel);
            self.enqueue_notify.notify_waiters();
            if self.flush_behavior == FlushBehavior::WakeEnqueueFailure {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {}
                    _ = self.flush_wakeup.notified() => {
                        return Err(AdapterError::new("media enqueue failed during flush"));
                    }
                }
            } else {
                cancellation.cancelled().await;
            }
            if self.stall_turn_cleanup {
                pending::<()>().await;
            }
            self.frames
                .lock()
                .expect("frame lock poisoned")
                .retain(|queued| queued.generation_id() != frame.generation_id());
            Err(AdapterError::new("media enqueue cancelled"))
        })
    }

    fn flush<'a>(
        &'a self,
        _session_id: SessionId,
        generation_id: GenerationId,
    ) -> AdapterFuture<'a, PlaybackReceipt> {
        Box::pin(async move {
            self.flushes
                .lock()
                .expect("flush lock poisoned")
                .push(generation_id);
            self.frames
                .lock()
                .expect("frame lock poisoned")
                .retain(|frame| frame.generation_id() != generation_id);
            match self.flush_behavior {
                FlushBehavior::Succeed => {
                    Ok(PlaybackReceipt::new(generation_id, PlaybackState::Flushed))
                }
                FlushBehavior::Fail => Err(AdapterError::new("output flush failed")),
                FlushBehavior::WakeEnqueueFailure => {
                    self.flush_wakeup.notify_one();
                    for _ in 0..128 {
                        tokio::task::yield_now().await;
                    }
                    Ok(PlaybackReceipt::new(generation_id, PlaybackState::Flushed))
                }
                FlushBehavior::Stall => pending().await,
            }
        })
    }
}

struct StalledWorkGuard {
    stopped: Option<Arc<AtomicBool>>,
}

impl Drop for StalledWorkGuard {
    fn drop(&mut self) {
        if let Some(stopped) = &self.stopped {
            stopped.store(true, Ordering::Release);
        }
    }
}

struct TestVoiceIoFactory {
    input: Arc<TestVoiceInput>,
    output: Arc<CancellableOutput>,
    completion_failure: mpsc::Sender<AdapterError>,
    completion_receiver: Mutex<Option<mpsc::Receiver<AdapterError>>>,
    completion_finished: Arc<AtomicBool>,
    stall_completion: bool,
}

impl TestVoiceIoFactory {
    fn new(
        input: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
        output: Arc<CancellableOutput>,
        stall_completion: bool,
    ) -> Self {
        let (completion_failure, completion_receiver) = mpsc::channel(1);
        Self {
            input: Arc::new(TestVoiceInput {
                receiver: Mutex::new(Some(input)),
                started: AtomicBool::new(false),
                started_notify: Notify::new(),
            }),
            output,
            completion_failure,
            completion_receiver: Mutex::new(Some(completion_receiver)),
            completion_finished: Arc::new(AtomicBool::new(false)),
            stall_completion,
        }
    }

    async fn wait_for_input_start(&self) {
        while !self.input.started.load(Ordering::Acquire) {
            self.input.started_notify.notified().await;
        }
    }

    async fn wait_for_completion(&self) {
        while !self.completion_finished.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    }

    async fn fail_sidecar(&self) {
        self.completion_failure
            .send(AdapterError::new(
                "voice sidecar process exited unexpectedly",
            ))
            .await
            .unwrap();
    }
}

impl VoiceIoFactory for TestVoiceIoFactory {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, VoiceIoSession> {
        Box::pin(async move {
            let mut completion_failure = self
                .completion_receiver
                .lock()
                .expect("completion receiver lock poisoned")
                .take()
                .ok_or_else(|| AdapterError::new("voice factory already started"))?;
            let completion_finished = Arc::clone(&self.completion_finished);
            let stall_completion = self.stall_completion;
            Ok(VoiceIoSession {
                input: self.input.clone(),
                capture: Arc::new(MockVoiceCaptureControl::new()),
                output: self.output.clone(),
                completion: tokio::spawn(async move {
                    let _finished = CompletionFinished(completion_finished);
                    if stall_completion {
                        pending::<()>().await;
                    }
                    let outcome = tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => Ok(()),
                        error = completion_failure.recv() => {
                            Err(error.unwrap_or_else(|| {
                                AdapterError::new("voice sidecar completion channel closed")
                            }))
                        }
                    };
                    outcome
                }),
            })
        })
    }
}

struct CompletionFinished(Arc<AtomicBool>);

impl Drop for CompletionFinished {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct TestVoiceInput {
    receiver: Mutex<Option<mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>>>,
    started: AtomicBool,
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

fn frame(request: &StreamingSpeechRequest, sequence: u64) -> AudioFrame {
    AudioFrame::new(
        request.turn_id(),
        request.generation_id(),
        request.utterance_id(),
        sequence,
        PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        vec![0; 960],
    )
    .unwrap()
}

async fn drain_until_turn_terminal(events: &mut VoiceSessionEventStream) -> Vec<VoiceSessionEvent> {
    timeout(Duration::from_secs(1), async {
        let mut observed = Vec::new();
        while let Some(event) = events.recv().await {
            let terminal = matches!(
                event,
                VoiceSessionEvent::Turn {
                    event: RuntimeEvent::TurnCompleted { .. }
                        | RuntimeEvent::TurnCancelled { .. }
                        | RuntimeEvent::TurnFailed { .. },
                    ..
                }
            );
            observed.push(event);
            if terminal {
                return observed;
            }
        }
        panic!("session ended before a turn terminal");
    })
    .await
    .expect("turn terminal timed out")
}

async fn drain_until_session_terminal(
    events: &mut VoiceSessionEventStream,
) -> Vec<VoiceSessionEvent> {
    timeout(Duration::from_secs(1), async {
        let mut observed = Vec::new();
        while let Some(event) = events.recv().await {
            let terminal = event.is_session_terminal();
            observed.push(event);
            if terminal {
                return observed;
            }
        }
        panic!("session ended without a terminal event");
    })
    .await
    .expect("session terminal timed out")
}

async fn drain_until_turn_recovery(events: &mut VoiceSessionEventStream) -> Vec<VoiceSessionEvent> {
    timeout(Duration::from_secs(1), async {
        let mut observed = Vec::new();
        let mut saw_turn_terminal = false;
        let mut saw_recovery = false;
        while let Some(event) = events.recv().await {
            saw_turn_terminal |= matches!(
                event,
                VoiceSessionEvent::Turn {
                    event: RuntimeEvent::TurnCompleted { .. }
                        | RuntimeEvent::TurnCancelled { .. }
                        | RuntimeEvent::TurnFailed { .. },
                    ..
                }
            );
            saw_recovery |= matches!(
                event,
                VoiceSessionEvent::SessionFailed {
                    recovery: RecoveryDisposition::ContinueSession,
                    ..
                }
            );
            observed.push(event);
            if saw_turn_terminal && saw_recovery {
                return observed;
            }
        }
        panic!("session ended before turn recovery completed");
    })
    .await
    .expect("turn recovery timed out")
}
