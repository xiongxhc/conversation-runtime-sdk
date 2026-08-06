use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conversation_memory::{
    MemoryClock, MemoryContextProvider, MemoryProviderFuture, MemoryStore, MemoryStoreResult,
    SqliteMemoryContextProvider, SqliteMemoryStore,
};
use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioFrame, ContinuousAudioOutput, MockContinuousAudioOutput,
    MockGenerationLanguageModel, MockStreamingSpeechSynthesizer, MockVoiceCaptureControl,
    PcmFormat, PcmSampleFormat, PlaybackReceipt, VoiceCaptureControl, VoiceInput, VoiceInputEvent,
    VoiceIoFactory, VoiceIoSession,
};
use conversation_protocol::{
    ComponentDescriptor, ComponentKind, ExecutionLocation, GenerationId, MemoryConfidence,
    MemoryDraft, MemoryKind, MemoryProvenance, MemoryProvenanceKind, MemoryRetention,
    PersonaProfile, PlaybackState, PrivacyMode, RecoveryDisposition, ResponseControls,
    RuntimeEvent, RuntimeStage, SessionId, TurnId, UnixTimestampMillis, UtteranceId, VoiceActivity,
    VoiceSessionEvent, VoiceSessionPolicy, VoiceTimingMilestone,
};
use conversation_runtime::{
    ConversationContext, ConversationQualityController, TextTurnEventStream, TextTurnRuntime,
    VoiceSessionAdapters, VoiceSessionEventStream, VoiceSessionRuntime,
};
use tempfile::TempDir;
use tokio::sync::{mpsc, Notify};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const SESSION_ID: SessionId = SessionId::new(1);

#[tokio::test(start_paused = true)]
async fn typed_voice_typed_turns_share_history_and_monotonic_ids() {
    let context = conversation_context();
    let first_language = Arc::new(MockGenerationLanguageModel::new(["answer one"]));
    let text = TextTurnRuntime::new(context.clone(), first_language);

    let first_text = text.start_turn("typed one").await.unwrap();
    assert_eq!(first_text.identity().turn_id(), TurnId::new(1));
    drain_text_turn(first_text.into_events()).await;

    let voice = VoiceSessionHarness::with_context(
        context.clone(),
        Arc::new(MockGenerationLanguageModel::new(["answer two"])),
    );
    let mut voice_events = voice.start().await;
    assert_session_started(voice_events.recv().await);
    voice.engine_final(1, "spoken").await;
    voice.speech_ended(0).await;
    tokio::time::advance(Duration::from_millis(600)).await;
    let observed = drain_until_turn_terminal(&mut voice_events).await;
    assert_final_and_completed(&observed, TurnId::new(2), "spoken");
    voice.shutdown(&mut voice_events).await;

    let second_language = Arc::new(MockGenerationLanguageModel::new(["answer three"]));
    let second_text = TextTurnRuntime::new(context, second_language.clone())
        .start_turn("typed two")
        .await
        .unwrap();
    assert_eq!(second_text.identity().turn_id(), TurnId::new(3));
    drain_text_turn(second_text.into_events()).await;

    let requests = second_language.requests();
    assert_eq!(
        requests[0]
            .input()
            .recent_messages()
            .iter()
            .map(|message| message.text())
            .collect::<Vec<_>>(),
        ["typed one", "answer one", "spoken", "answer two"]
    );
}

#[tokio::test(start_paused = true)]
async fn restarted_voice_sessions_continue_context_identities() {
    let context = conversation_context();
    let first = VoiceSessionHarness::with_context_for_turn(
        context.clone(),
        Arc::new(MockGenerationLanguageModel::new(["first answer"])),
        TurnId::new(1),
    );
    let mut first_events = first.start().await;
    assert_session_started(first_events.recv().await);
    first.engine_final(1, "first spoken turn").await;
    first.speech_ended(0).await;
    tokio::time::advance(Duration::from_millis(600)).await;
    assert_final_and_completed(
        &drain_until_turn_terminal(&mut first_events).await,
        TurnId::new(1),
        "first spoken turn",
    );
    first.shutdown(&mut first_events).await;

    let second = VoiceSessionHarness::with_context_for_turn(
        context,
        Arc::new(MockGenerationLanguageModel::new(["second answer"])),
        TurnId::new(2),
    );
    let mut second_events = second.start().await;
    assert_session_started(second_events.recv().await);
    second.engine_final(1, "second spoken turn").await;
    second.speech_ended(600).await;
    tokio::time::advance(Duration::from_millis(600)).await;
    assert_final_and_completed(
        &drain_until_turn_terminal(&mut second_events).await,
        TurnId::new(2),
        "second spoken turn",
    );
    second.shutdown(&mut second_events).await;
}

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
async fn recognizer_final_arriving_after_elapsed_silence_starts_the_turn() {
    let harness = VoiceSessionHarness::new();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    harness.partial(4, "late final").await;
    assert_partial(events.recv().await, 4, "late final");
    harness.speech_ended(0).await;
    tokio::time::advance(Duration::from_millis(600)).await;
    tokio::task::yield_now().await;
    assert!(harness.language.requests().is_empty());

    harness.engine_final(4, "late final").await;
    harness.wait_for_request_count(1).await;

    let observed = drain_until_turn_terminal(&mut events).await;
    assert_final_and_completed(&observed, TurnId::new(1), "late final");
    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn sidecar_timestamp_skew_does_not_move_the_session_deadline() {
    let harness = VoiceSessionHarness::new();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    tokio::time::advance(Duration::from_millis(250)).await;
    harness.engine_final(4, "clock domain").await;
    harness.speech_ended(9_000_000).await;
    tokio::time::advance(Duration::from_millis(600)).await;

    let observed = drain_until_turn_terminal(&mut events).await;
    assert_final_and_completed(&observed, TurnId::new(1), "clock domain");
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

#[tokio::test]
async fn session_started_waits_until_capture_start_completes() {
    let input = Arc::new(DelayedVoiceInput::new());
    let factory = Arc::new(DelayedVoiceIoFactory {
        input: input.clone(),
    });
    let runtime = VoiceSessionRuntime::new(
        conversation_context(),
        VoiceSessionAdapters::new(
            factory,
            Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new())),
            Arc::new(MockStreamingSpeechSynthesizer::new([])),
        ),
    );
    let mut events = runtime.start(policy()).await.unwrap();
    timeout(Duration::from_secs(1), input.wait_until_starting())
        .await
        .expect("voice input start was not attempted");

    assert!(timeout(Duration::from_millis(20), events.recv())
        .await
        .is_err());

    input.release();
    assert_session_started(
        timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("session start acknowledgement was not published"),
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_during_capture_start_cancels_start_and_completes() {
    let input = Arc::new(DelayedVoiceInput::new());
    let factory = Arc::new(DelayedVoiceIoFactory {
        input: input.clone(),
    });
    let runtime = VoiceSessionRuntime::new(
        conversation_context(),
        VoiceSessionAdapters::new(
            factory,
            Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new())),
            Arc::new(MockStreamingSpeechSynthesizer::new([])),
        ),
    );
    let mut events = runtime.start(policy()).await.unwrap();
    timeout(Duration::from_secs(1), input.wait_until_starting())
        .await
        .expect("voice input start was not attempted");

    timeout(Duration::from_secs(1), runtime.shutdown())
        .await
        .expect("shutdown blocked behind capture-start acknowledgement")
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("shutdown did not publish a terminal event"),
        Some(VoiceSessionEvent::SessionEnded {
            session_id: SESSION_ID,
        })
    );
    assert_eq!(
        timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("shutdown stream did not reach EOF"),
        None
    );

    let replacement_id = SessionId::new(2);
    let mut replacement = runtime.start(policy_for(replacement_id)).await.unwrap();
    input.release();
    assert!(matches!(
        timeout(Duration::from_secs(1), replacement.recv())
            .await
            .expect("replacement session did not start"),
        Some(VoiceSessionEvent::SessionStarted { session_id, .. })
            if session_id == replacement_id
    ));
    runtime.shutdown().await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), replacement.recv())
            .await
            .expect("replacement session did not terminate"),
        Some(VoiceSessionEvent::SessionEnded {
            session_id: replacement_id,
        })
    );
    assert_eq!(
        timeout(Duration::from_secs(1), replacement.recv())
            .await
            .expect("replacement stream did not reach EOF"),
        None
    );
}

#[tokio::test]
async fn pause_and_resume_publish_only_after_adapter_acknowledgement() {
    let harness = VoiceSessionHarness::new();
    let capture = harness.factory.capture();
    capture.gate_pause.store(true, Ordering::Release);
    capture.gate_resume.store(true, Ordering::Release);
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    let runtime = harness.runtime.clone();
    let pause = tokio::spawn(async move { runtime.pause_capture().await });
    capture.wait_for_pause_calls(1).await;
    assert!(timeout(Duration::from_millis(20), events.recv())
        .await
        .is_err());
    capture.pause_release.notify_one();
    pause.await.unwrap().unwrap();
    assert_eq!(
        events.recv().await,
        Some(VoiceSessionEvent::CapturePaused {
            session_id: SESSION_ID,
        })
    );

    let runtime = harness.runtime.clone();
    let resume = tokio::spawn(async move { runtime.resume_capture().await });
    capture.wait_for_resume_calls(1).await;
    assert!(timeout(Duration::from_millis(20), events.recv())
        .await
        .is_err());
    capture.resume_release.notify_one();
    resume.await.unwrap().unwrap();
    assert_eq!(
        events.recv().await,
        Some(VoiceSessionEvent::CaptureResumed {
            session_id: SESSION_ID,
        })
    );

    harness.shutdown(&mut events).await;
}

#[tokio::test]
async fn repeated_capture_controls_are_rejected_without_state_change() {
    let harness = VoiceSessionHarness::new();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    harness.runtime.pause_capture().await.unwrap();
    assert!(harness.runtime.pause_capture().await.is_err());
    assert_eq!(
        events.recv().await,
        Some(VoiceSessionEvent::CapturePaused {
            session_id: SESSION_ID,
        })
    );
    assert!(timeout(Duration::from_millis(20), events.recv())
        .await
        .is_err());

    harness.runtime.resume_capture().await.unwrap();
    assert!(harness.runtime.resume_capture().await.is_err());
    assert_eq!(
        events.recv().await,
        Some(VoiceSessionEvent::CaptureResumed {
            session_id: SESSION_ID,
        })
    );
    harness.shutdown(&mut events).await;
}

#[tokio::test]
async fn shutdown_from_paused_state_completes_once() {
    let harness = VoiceSessionHarness::new();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);
    harness.runtime.pause_capture().await.unwrap();
    assert!(matches!(
        events.recv().await,
        Some(VoiceSessionEvent::CapturePaused { .. })
    ));

    harness.shutdown(&mut events).await;
}

#[tokio::test]
async fn dropping_the_session_cancels_a_pending_capture_control() {
    let harness = VoiceSessionHarness::new();
    let capture = harness.factory.capture();
    capture.gate_pause.store(true, Ordering::Release);
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    let runtime = harness.runtime.clone();
    let pause = tokio::spawn(async move { runtime.pause_capture().await });
    capture.wait_for_pause_calls(1).await;
    drop(events);

    assert!(timeout(Duration::from_secs(1), pause)
        .await
        .expect("pause command did not finish")
        .unwrap()
        .is_err());
    timeout(
        Duration::from_secs(1),
        harness.factory.wait_for_completion(),
    )
    .await
    .expect("voice I/O completion did not finish");
}

#[tokio::test(start_paused = true)]
async fn pause_during_an_active_turn_does_not_interrupt_the_turn() {
    let harness = VoiceSessionHarness::with_playback();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);
    harness.engine_final(1, "hello").await;
    harness.speech_ended(0).await;
    tokio::time::advance(Duration::from_millis(600)).await;
    harness.wait_for_frame_count(1).await;

    harness.runtime.pause_capture().await.unwrap();
    let mut saw_paused = false;
    while !saw_paused {
        saw_paused = matches!(
            timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap(),
            Some(VoiceSessionEvent::CapturePaused { .. })
        );
    }
    harness.release_playback_acceptance();
    assert!(drain_until_turn_terminal(&mut events)
        .await
        .iter()
        .any(|event| matches!(
            event,
            VoiceSessionEvent::Turn {
                event: RuntimeEvent::TurnCompleted { .. },
                ..
            }
        )));
    harness.shutdown(&mut events).await;
}

#[tokio::test]
async fn capture_resumes_after_a_typed_turn_uses_the_shared_context() {
    let context = conversation_context();
    let harness = VoiceSessionHarness::with_context_for_turn(
        context.clone(),
        Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new())),
        TurnId::new(2),
    );
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);
    harness.runtime.pause_capture().await.unwrap();
    assert!(matches!(
        events.recv().await,
        Some(VoiceSessionEvent::CapturePaused { .. })
    ));

    let typed = TextTurnRuntime::new(
        context,
        Arc::new(MockGenerationLanguageModel::new(["typed answer"])),
    )
    .start_turn("typed while paused")
    .await
    .unwrap();
    assert_eq!(typed.identity().turn_id(), TurnId::new(1));
    drain_text_turn(typed.into_events()).await;

    harness.runtime.resume_capture().await.unwrap();
    assert!(matches!(
        events.recv().await,
        Some(VoiceSessionEvent::CaptureResumed { .. })
    ));
    harness.shutdown(&mut events).await;
}

#[tokio::test]
async fn shutdown_cancels_an_in_flight_pause() {
    let harness = VoiceSessionHarness::new();
    let capture = harness.factory.capture();
    capture.gate_pause.store(true, Ordering::Release);
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    let runtime = harness.runtime.clone();
    let pause = tokio::spawn(async move { runtime.pause_capture().await });
    capture.wait_for_pause_calls(1).await;
    let runtime = harness.runtime.clone();
    let shutdown = tokio::spawn(async move { runtime.shutdown().await });

    assert!(timeout(Duration::from_secs(1), pause)
        .await
        .expect("pause did not cancel")
        .unwrap()
        .is_err());
    timeout(Duration::from_secs(1), shutdown)
        .await
        .expect("shutdown did not complete")
        .unwrap()
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(VoiceSessionEvent::SessionEnded { .. })
    ));
}

struct RemoteMemoryProvider;

impl MemoryContextProvider for RemoteMemoryProvider {
    fn execution_location(&self) -> ExecutionLocation {
        ExecutionLocation::Remote
    }

    fn retrieve(
        &self,
        _turn_id: TurnId,
        _query: String,
        _cancellation: CancellationToken,
    ) -> MemoryProviderFuture<'_> {
        unreachable!()
    }
}

#[test]
fn remote_memory_is_rejected_before_a_voice_session_can_start() {
    let harness = VoiceSessionHarness::new();
    let error = conversation_context()
        .with_memory_provider(Arc::new(RemoteMemoryProvider), ExecutionLocation::Local)
        .err()
        .expect("remote memory should be rejected");

    assert_eq!(error.stage(), RuntimeStage::Memory);
    assert_eq!(harness.factory.start_count(), 0);
}

struct FixedMemoryClock(UnixTimestampMillis);

impl MemoryClock for FixedMemoryClock {
    fn now(&self) -> MemoryStoreResult<UnixTimestampMillis> {
        Ok(self.0)
    }
}

#[tokio::test(start_paused = true)]
async fn configured_local_memory_reaches_the_voice_language_request() {
    let harness = VoiceSessionHarness::with_memory();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    harness.engine_final(1, "local project").await;
    harness.speech_ended(0).await;
    tokio::time::advance(Duration::from_millis(600)).await;

    let observed = drain_until_turn_terminal(&mut events).await;
    assert!(observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::Turn {
            event: RuntimeEvent::MemoryRetrieved { trace },
            ..
        } if trace.selected_items() == 1
    )));
    assert_eq!(harness.language.requests().len(), 1);
    assert_eq!(
        harness.language.requests()[0].input().memory_items()[0].content(),
        "Local project context"
    );
    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn unique_partial_coalescing_remains_bounded_under_stalled_delivery() {
    let harness = VoiceSessionHarness::new();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    for segment_id in 1..=96 {
        harness
            .partial(segment_id, &format!("unique-{segment_id}"))
            .await;
    }

    for _ in 0..60 {
        assert!(matches!(
            events.recv().await,
            Some(VoiceSessionEvent::TranscriptPartial { .. })
        ));
    }
    tokio::select! {
        event = events.recv() => panic!("partial pending state exceeded its bound: {event:?}"),
        _ = tokio::time::sleep(Duration::from_millis(1)) => {}
    }

    harness.shutdown(&mut events).await;
}

#[tokio::test(start_paused = true)]
async fn stalled_consumer_backpressures_sustained_multi_turn_reliable_events() {
    let harness = VoiceSessionHarness::new();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    for segment_id in 1..=4 {
        harness
            .partial(segment_id, &format!("partial-{segment_id}"))
            .await;
        harness
            .engine_final(segment_id, &format!("final-{segment_id}"))
            .await;
        harness.speech_ended((segment_id - 1) * 600).await;
        tokio::time::advance(Duration::from_millis(600)).await;
        harness.wait_for_request_count(segment_id as usize).await;
    }

    for segment_id in 100..196 {
        harness
            .input
            .send(Ok(VoiceInputEvent::Recognition(
                conversation_model_adapters::RecognitionEvent::Hypothesis(
                    conversation_model_adapters::RecognitionHypothesis::partial(
                        segment_id,
                        format!("unique-{segment_id}"),
                    ),
                ),
            )))
            .await
            .unwrap();
        tokio::task::yield_now().await;
    }

    let mut saw_backpressure = false;
    for segment_id in 1_000..2_000 {
        let input = if segment_id % 2 == 0 {
            Err(AdapterError::new("voice sidecar recognition failed")
                .with_stage(RuntimeStage::SpeechRecognizer))
        } else {
            Ok(VoiceInputEvent::Recognition(
                conversation_model_adapters::RecognitionEvent::Hypothesis(
                    conversation_model_adapters::RecognitionHypothesis::partial(
                        segment_id,
                        format!("saturated-{segment_id}"),
                    ),
                ),
            ))
        };
        match harness.input.try_send(input) {
            Ok(()) => tokio::task::yield_now().await,
            Err(mpsc::error::TrySendError::Full(_)) => {
                saw_backpressure = true;
                break;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                panic!("voice input closed before saturation")
            }
        }
    }

    assert!(
        saw_backpressure,
        "stalled event delivery did not backpressure reliable producers"
    );
    drop(events);
    timeout(
        Duration::from_secs(1),
        harness.factory.wait_for_completion(),
    )
    .await
    .expect("saturated session did not clean up");
}

#[tokio::test]
async fn input_side_acceptance_is_rejected_without_publication() {
    let harness = VoiceSessionHarness::new();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    harness
        .send(VoiceInputEvent::Playback(PlaybackReceipt::new(
            GenerationId::new(1),
            PlaybackState::Accepted,
        )))
        .await;
    let observed = drain_until_session_terminal(&mut events).await;

    assert!(!observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::Playback {
            state: PlaybackState::Accepted,
            ..
        }
    )));
    assert!(observed.iter().any(|event| matches!(
        event,
        VoiceSessionEvent::SessionFailed {
            error,
            recovery: RecoveryDisposition::NewSession,
            ..
        } if error.stage() == RuntimeStage::VoiceSidecar
    )));
}

#[tokio::test(start_paused = true)]
async fn matching_playback_lifecycle_is_reliable_under_saturated_output() {
    let harness = VoiceSessionHarness::with_playback();
    let mut events = harness.start().await;
    assert_session_started(events.recv().await);

    for segment_id in 1..=96 {
        harness
            .partial(segment_id, &format!("saturated-{segment_id}"))
            .await;
    }
    harness.engine_final(100, "final question").await;
    harness.speech_ended(0).await;
    tokio::time::advance(Duration::from_millis(600)).await;
    harness.wait_for_request_count(1).await;
    harness.wait_for_frame_count(1).await;
    for _ in 0..96 {
        harness
            .send(VoiceInputEvent::Playback(PlaybackReceipt::new(
                GenerationId::new(1),
                PlaybackState::Rendered,
            )))
            .await;
    }
    harness.release_playback_acceptance();

    let observed = drain_until_turn_terminal(&mut events).await;
    let accepted = observed
        .iter()
        .position(|event| {
            matches!(
                event,
                VoiceSessionEvent::Playback {
                    generation_id,
                    state: PlaybackState::Accepted,
                    ..
                } if *generation_id == GenerationId::new(1)
            )
        })
        .expect("matching runtime acceptance was not delivered");
    let rendered = observed
        .iter()
        .position(|event| {
            matches!(
                event,
                VoiceSessionEvent::Playback {
                    generation_id,
                    state: PlaybackState::Rendered,
                    ..
                } if *generation_id == GenerationId::new(1)
            )
        })
        .expect("matching rendered acknowledgement was not delivered");
    let completed = observed
        .iter()
        .position(|event| {
            matches!(
                event,
                VoiceSessionEvent::Turn {
                    generation_id,
                    event: RuntimeEvent::TurnCompleted { .. },
                    ..
                } if *generation_id == GenerationId::new(1)
            )
        })
        .expect("matching turn completion was not delivered");

    assert!(accepted < rendered);
    assert!(rendered < completed);
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(
                event,
                VoiceSessionEvent::Playback {
                    generation_id,
                    state: PlaybackState::Accepted | PlaybackState::Rendered,
                    ..
                } if *generation_id == GenerationId::new(1)
            ))
            .count(),
        2
    );
    harness.shutdown(&mut events).await;
}

struct VoiceSessionHarness {
    runtime: VoiceSessionRuntime,
    factory: Arc<TestVoiceIoFactory>,
    input: mpsc::Sender<Result<VoiceInputEvent, AdapterError>>,
    language: Arc<MockGenerationLanguageModel>,
    playback_gate: Option<Arc<GatedAcceptOutput>>,
    _memory_directory: Option<TempDir>,
}

impl VoiceSessionHarness {
    fn new() -> Self {
        let (input, input_receiver) = mpsc::channel(32);
        let output = Arc::new(MockContinuousAudioOutput::new());
        let factory = Arc::new(TestVoiceIoFactory::new(input_receiver, output));
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let speech = Arc::new(MockStreamingSpeechSynthesizer::new([]));
        let runtime = VoiceSessionRuntime::new(
            conversation_context(),
            VoiceSessionAdapters::new(factory.clone(), language.clone(), speech),
        );
        Self {
            runtime,
            factory,
            input,
            language,
            playback_gate: None,
            _memory_directory: None,
        }
    }

    fn with_context(
        context: ConversationContext,
        language: Arc<MockGenerationLanguageModel>,
    ) -> Self {
        Self::with_context_for_turn(context, language, TurnId::new(2))
    }

    fn with_context_for_turn(
        context: ConversationContext,
        language: Arc<MockGenerationLanguageModel>,
        turn_id: TurnId,
    ) -> Self {
        let (input, input_receiver) = mpsc::channel(32);
        let output = Arc::new(MockContinuousAudioOutput::new());
        let factory = Arc::new(TestVoiceIoFactory::new(input_receiver, output));
        let frame = AudioFrame::new(
            turn_id,
            GenerationId::new(turn_id.get()),
            UtteranceId::new(1),
            0,
            PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
            vec![0; 960],
        )
        .unwrap();
        let speech = Arc::new(MockStreamingSpeechSynthesizer::new([frame]));
        let runtime = VoiceSessionRuntime::new(
            context,
            VoiceSessionAdapters::new(factory.clone(), language.clone(), speech),
        );
        Self {
            runtime,
            factory,
            input,
            language,
            playback_gate: None,
            _memory_directory: None,
        }
    }

    fn with_memory() -> Self {
        let memory_directory = tempfile::tempdir().unwrap();
        let store =
            SqliteMemoryStore::initialize(memory_directory.path().join("memory.sqlite3")).unwrap();
        let created_at = UnixTimestampMillis::new(1_000).unwrap();
        store
            .create(
                MemoryDraft::new(
                    MemoryKind::Semantic,
                    "Local project context",
                    MemoryProvenance::new(
                        MemoryProvenanceKind::UserProvided,
                        "settings",
                        created_at,
                        "local-user",
                        None,
                    )
                    .unwrap(),
                    MemoryConfidence::new(900).unwrap(),
                    created_at,
                    MemoryRetention::UntilDeleted,
                )
                .unwrap(),
            )
            .unwrap();
        let provider = Arc::new(SqliteMemoryContextProvider::new(
            store,
            Arc::new(FixedMemoryClock(UnixTimestampMillis::new(2_000).unwrap())),
        ));
        let (input, input_receiver) = mpsc::channel(32);
        let output = Arc::new(MockContinuousAudioOutput::new());
        let factory = Arc::new(TestVoiceIoFactory::new(input_receiver, output));
        let language = Arc::new(MockGenerationLanguageModel::new(Vec::<String>::new()));
        let speech = Arc::new(MockStreamingSpeechSynthesizer::new([]));
        let context = conversation_context()
            .with_memory_provider(provider, ExecutionLocation::Local)
            .unwrap();
        let adapters = VoiceSessionAdapters::new(factory.clone(), language.clone(), speech);
        Self {
            runtime: VoiceSessionRuntime::new(context, adapters),
            factory,
            input,
            language,
            playback_gate: None,
            _memory_directory: Some(memory_directory),
        }
    }

    fn with_playback() -> Self {
        let turn_id = TurnId::new(1);
        let generation_id = GenerationId::new(1);
        let format = PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
        let frame = AudioFrame::new(
            turn_id,
            generation_id,
            UtteranceId::new(1),
            0,
            format,
            vec![0; 960],
        )
        .unwrap();
        let (input, input_receiver) = mpsc::channel(32);
        let playback_gate = Arc::new(GatedAcceptOutput::new());
        let factory = Arc::new(TestVoiceIoFactory::new(
            input_receiver,
            playback_gate.clone(),
        ));
        let language = Arc::new(MockGenerationLanguageModel::new(["Fixture response."]));
        let speech = Arc::new(MockStreamingSpeechSynthesizer::new([frame]));
        let runtime = VoiceSessionRuntime::new(
            conversation_context(),
            VoiceSessionAdapters::new(factory.clone(), language.clone(), speech),
        );
        Self {
            runtime,
            factory,
            input,
            language,
            playback_gate: Some(playback_gate),
            _memory_directory: None,
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

    async fn wait_for_request_count(&self, expected: usize) {
        for _ in 0..128 {
            if self.language.requests().len() >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("language request {expected} did not start");
    }

    async fn wait_for_frame_count(&self, expected: usize) {
        let playback_gate = self
            .playback_gate
            .as_ref()
            .expect("playback harness has a gated output");
        for _ in 0..128 {
            if playback_gate.frames().len() >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("audio frame {expected} was not enqueued");
    }

    fn release_playback_acceptance(&self) {
        self.playback_gate
            .as_ref()
            .expect("playback harness has a gated output")
            .release();
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
    output: Arc<dyn ContinuousAudioOutput>,
    capture: Arc<TestVoiceCaptureControl>,
    start_count: AtomicUsize,
    input_started: Arc<AtomicBool>,
    completion_finished: Arc<AtomicBool>,
}

struct DelayedVoiceIoFactory {
    input: Arc<DelayedVoiceInput>,
}

impl VoiceIoFactory for DelayedVoiceIoFactory {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, VoiceIoSession> {
        Box::pin(async move {
            Ok(VoiceIoSession {
                input: self.input.clone(),
                capture: Arc::new(MockVoiceCaptureControl::new()),
                output: Arc::new(MockContinuousAudioOutput::new()),
                completion: tokio::spawn(async move {
                    cancellation.cancelled().await;
                    Ok(())
                }),
            })
        })
    }
}

struct DelayedVoiceInput {
    starting: AtomicBool,
    starting_notify: Notify,
    release: Notify,
    sender: Mutex<Option<mpsc::Sender<Result<VoiceInputEvent, AdapterError>>>>,
}

impl DelayedVoiceInput {
    fn new() -> Self {
        Self {
            starting: AtomicBool::new(false),
            starting_notify: Notify::new(),
            release: Notify::new(),
            sender: Mutex::new(None),
        }
    }

    async fn wait_until_starting(&self) {
        while !self.starting.load(Ordering::Acquire) {
            self.starting_notify.notified().await;
        }
    }

    fn release(&self) {
        self.release.notify_one();
    }
}

impl VoiceInput for DelayedVoiceInput {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>> {
        Box::pin(async move {
            self.starting.store(true, Ordering::Release);
            self.starting_notify.notify_waiters();
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    return Err(AdapterError::new("voice input cancelled"));
                }
                _ = self.release.notified() => {}
            }
            let (sender, receiver) = mpsc::channel(1);
            *self
                .sender
                .lock()
                .expect("delayed input sender lock poisoned") = Some(sender);
            Ok(receiver)
        })
    }
}

impl TestVoiceIoFactory {
    fn new(
        input: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
        output: Arc<dyn ContinuousAudioOutput>,
    ) -> Self {
        let input_started = Arc::new(AtomicBool::new(false));
        Self {
            input: Arc::new(TestVoiceInput {
                receiver: Mutex::new(Some(input)),
                started: Arc::clone(&input_started),
                started_notify: Notify::new(),
            }),
            output,
            capture: Arc::new(TestVoiceCaptureControl::new()),
            start_count: AtomicUsize::new(0),
            input_started,
            completion_finished: Arc::new(AtomicBool::new(false)),
        }
    }

    fn start_count(&self) -> usize {
        self.start_count.load(Ordering::Acquire)
    }

    fn capture(&self) -> Arc<TestVoiceCaptureControl> {
        self.capture.clone()
    }

    async fn wait_for_input_start(&self) {
        while !self.input_started.load(Ordering::Acquire) {
            self.input.started_notify.notified().await;
        }
    }

    async fn wait_for_completion(&self) {
        while !self.completion_finished.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
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
                capture: self.capture.clone(),
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum TestCaptureState {
    Active,
    Paused,
}

struct TestVoiceCaptureControl {
    state: Mutex<TestCaptureState>,
    pause_calls: AtomicUsize,
    resume_calls: AtomicUsize,
    pause_started: Notify,
    resume_started: Notify,
    pause_release: Notify,
    resume_release: Notify,
    gate_pause: AtomicBool,
    gate_resume: AtomicBool,
}

impl TestVoiceCaptureControl {
    fn new() -> Self {
        Self {
            state: Mutex::new(TestCaptureState::Active),
            pause_calls: AtomicUsize::new(0),
            resume_calls: AtomicUsize::new(0),
            pause_started: Notify::new(),
            resume_started: Notify::new(),
            pause_release: Notify::new(),
            resume_release: Notify::new(),
            gate_pause: AtomicBool::new(false),
            gate_resume: AtomicBool::new(false),
        }
    }

    async fn wait_for_pause_calls(&self, expected: usize) {
        while self.pause_calls.load(Ordering::Acquire) < expected {
            self.pause_started.notified().await;
        }
    }

    async fn wait_for_resume_calls(&self, expected: usize) {
        while self.resume_calls.load(Ordering::Acquire) < expected {
            self.resume_started.notified().await;
        }
    }
}

impl VoiceCaptureControl for TestVoiceCaptureControl {
    fn pause<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()> {
        Box::pin(async move {
            self.pause_calls.fetch_add(1, Ordering::AcqRel);
            self.pause_started.notify_waiters();
            if self.gate_pause.load(Ordering::Acquire) {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        return Err(AdapterError::new("capture pause cancelled"));
                    }
                    _ = self.pause_release.notified() => {}
                }
            }
            let mut state = self.state.lock().expect("capture state lock poisoned");
            if *state != TestCaptureState::Active {
                return Err(AdapterError::new("capture is not active"));
            }
            *state = TestCaptureState::Paused;
            Ok(())
        })
    }

    fn resume<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()> {
        Box::pin(async move {
            self.resume_calls.fetch_add(1, Ordering::AcqRel);
            self.resume_started.notify_waiters();
            if self.gate_resume.load(Ordering::Acquire) {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        return Err(AdapterError::new("capture resume cancelled"));
                    }
                    _ = self.resume_release.notified() => {}
                }
            }
            let mut state = self.state.lock().expect("capture state lock poisoned");
            if *state != TestCaptureState::Paused {
                return Err(AdapterError::new("capture is not paused"));
            }
            *state = TestCaptureState::Active;
            Ok(())
        })
    }
}

struct GatedAcceptOutput {
    frames: Mutex<Vec<AudioFrame>>,
    release: Notify,
}

impl GatedAcceptOutput {
    fn new() -> Self {
        Self {
            frames: Mutex::new(Vec::new()),
            release: Notify::new(),
        }
    }

    fn frames(&self) -> Vec<AudioFrame> {
        self.frames.lock().expect("frame lock poisoned").clone()
    }

    fn release(&self) {
        self.release.notify_one();
    }
}

impl ContinuousAudioOutput for GatedAcceptOutput {
    fn enqueue<'a>(
        &'a self,
        frame: AudioFrame,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, PlaybackReceipt> {
        Box::pin(async move {
            let generation_id = frame.generation_id();
            self.frames.lock().expect("frame lock poisoned").push(frame);
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    Err(AdapterError::new("media enqueue cancelled"))
                }
                _ = self.release.notified() => {
                    Ok(PlaybackReceipt::new(generation_id, PlaybackState::Accepted))
                }
            }
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
    policy_for(SESSION_ID)
}

fn policy_for(session_id: SessionId) -> VoiceSessionPolicy {
    VoiceSessionPolicy::new(
        session_id,
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

fn conversation_context() -> ConversationContext {
    ConversationContext::new(ConversationQualityController::new(
        PersonaProfile::default(),
        ResponseControls::default(),
        conversation_protocol::ConversationMode::DirectAnswer,
    ))
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
        generation_id: GenerationId::new(turn_id.get()),
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

async fn drain_text_turn(events: TextTurnEventStream) -> Vec<RuntimeEvent> {
    let mut events = events;
    let mut observed = Vec::new();
    while let Some(event) = events.recv().await {
        observed.push(event);
    }
    assert!(observed
        .iter()
        .any(|event| matches!(event, RuntimeEvent::TurnCompleted { .. })));
    observed
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
        panic!("voice session ended before the session terminal");
    })
    .await
    .expect("session terminal timed out")
}
