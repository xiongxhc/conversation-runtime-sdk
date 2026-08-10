use std::time::Duration;

use conversation_model_adapters::RecognitionHypothesis;
use conversation_protocol::{RuntimeStage, VoiceActivity};
use conversation_runtime::{SessionClock, TurnFinalizationDeadline, TurnFinalizer};

#[test]
fn engine_final_waits_for_rust_silence_deadline() {
    let mut finalizer = TurnFinalizer::new(600).unwrap();
    finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(8, "hello"), 1_000);
    finalizer.observe_activity(VoiceActivity::SpeechEnded { at_ms: 1_020 });

    assert_eq!(finalizer.finalize_ready(1_619), None);
    assert_eq!(
        finalizer.finalize_ready(1_620).map(|item| item.text),
        Some("hello".to_owned())
    );
    assert_eq!(finalizer.finalize_ready(1_621), None);
}

#[test]
fn later_partial_replaces_display_candidate_without_appending() {
    let mut finalizer = TurnFinalizer::new(600).unwrap();
    finalizer.observe_hypothesis(RecognitionHypothesis::partial(3, "hel"), 10);
    finalizer.observe_hypothesis(RecognitionHypothesis::partial(3, "hello"), 20);

    assert_eq!(finalizer.display_text(), Some("hello"));
}

#[test]
fn whitespace_engine_final_never_becomes_a_finalized_transcript() {
    let mut finalizer = TurnFinalizer::new(600).unwrap();
    finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(4, " \t\n "), 10);
    finalizer.observe_activity(VoiceActivity::SpeechEnded { at_ms: 20 });

    assert_eq!(finalizer.finalize_ready(620), None);
    assert_eq!(finalizer.finalize_ready(u64::MAX), None);
}

#[test]
fn whitespace_segments_do_not_replace_or_reset_a_valid_pending_segment() {
    let mut finalizer = TurnFinalizer::new(600).unwrap();
    finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(20, "hello"), 10);
    finalizer.observe_activity(VoiceActivity::SpeechEnded { at_ms: 20 });

    finalizer.observe_hypothesis(RecognitionHypothesis::partial(21, " \t"), 30);
    finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(22, "\n "), 40);

    assert_eq!(finalizer.display_text(), Some("hello"));
    assert_eq!(
        finalizer.finalize_ready(620),
        Some(conversation_runtime::FinalizedTranscript {
            segment_id: 20,
            text: "hello".to_owned(),
        })
    );
    assert_eq!(finalizer.finalize_ready(u64::MAX), None);
}

#[test]
fn speech_resume_cancels_the_silence_deadline() {
    let mut finalizer = TurnFinalizer::new(600).unwrap();
    finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(5, "hello"), 10);
    finalizer.observe_activity(VoiceActivity::SpeechEnded { at_ms: 20 });
    finalizer.observe_activity(VoiceActivity::SpeechStarted { at_ms: 300 });

    assert_eq!(finalizer.finalize_ready(620), None);

    finalizer.observe_activity(VoiceActivity::SpeechEnded { at_ms: 700 });
    assert_eq!(finalizer.finalize_ready(1_299), None);
    assert_eq!(
        finalizer.finalize_ready(1_300).map(|item| item.text),
        Some("hello".to_owned())
    );
}

#[test]
fn a_new_segment_keeps_prior_engine_final_text_in_the_same_turn() {
    let mut finalizer = TurnFinalizer::new(600).unwrap();
    finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(10, "old"), 10);
    finalizer.observe_activity(VoiceActivity::SpeechEnded { at_ms: 20 });
    finalizer.observe_hypothesis(RecognitionHypothesis::partial(11, " new"), 30);

    assert_eq!(finalizer.display_text(), Some(" new"));
    assert_eq!(finalizer.finalize_ready(620), None);

    finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(11, " new text"), 700);
    finalizer.observe_activity(VoiceActivity::SpeechEnded { at_ms: 720 });
    let finalized = finalizer
        .finalize_ready(1_320)
        .expect("new segment should finalize");

    assert_eq!(finalized.segment_id, 11);
    assert_eq!(finalized.text, "old new text");
}

#[test]
fn one_segment_can_finalize_only_once() {
    let mut finalizer = TurnFinalizer::new(600).unwrap();
    finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(12, "hello"), 10);
    finalizer.observe_activity(VoiceActivity::SpeechEnded { at_ms: 20 });

    assert!(finalizer.finalize_ready(620).is_some());
    assert_eq!(finalizer.finalize_ready(621), None);

    finalizer.observe_hypothesis(RecognitionHypothesis::engine_final(12, "hello again"), 700);
    finalizer.observe_activity(VoiceActivity::SpeechEnded { at_ms: 710 });
    assert_eq!(finalizer.finalize_ready(1_310), None);
}

#[test]
fn finalizer_rejects_a_zero_silence_duration() {
    let error = TurnFinalizer::new(0).unwrap_err();

    assert_eq!(error.stage(), RuntimeStage::Runtime);
}

#[tokio::test(start_paused = true)]
async fn session_clock_uses_one_paused_monotonic_origin() {
    let clock = SessionClock::new();

    assert_eq!(clock.now_ms(), 0);
    tokio::time::advance(Duration::from_millis(1_234)).await;
    assert_eq!(clock.now_ms(), 1_234);
}

#[tokio::test(start_paused = true)]
async fn finalization_deadline_can_be_replaced_and_fires_without_input() {
    let mut deadline = TurnFinalizationDeadline::new();
    deadline.arm_after(Duration::from_millis(100));

    tokio::select! {
        _ = deadline.wait() => panic!("original deadline fired too early"),
        _ = tokio::time::advance(Duration::from_millis(50)) => {}
    }

    deadline.arm_after(Duration::from_millis(200));
    tokio::select! {
        _ = deadline.wait() => panic!("replacement deadline fired too early"),
        _ = tokio::time::advance(Duration::from_millis(199)) => {}
    }

    tokio::time::advance(Duration::from_millis(1)).await;
    deadline.wait().await;
}

#[tokio::test(start_paused = true)]
async fn finalization_deadline_wait_is_cancellation_safe_and_disarmable() {
    let mut deadline = TurnFinalizationDeadline::new();
    deadline.arm_after(Duration::from_millis(100));

    tokio::select! {
        _ = deadline.wait() => panic!("deadline fired too early"),
        _ = tokio::time::advance(Duration::from_millis(50)) => {}
    }

    tokio::time::advance(Duration::from_millis(50)).await;
    deadline.wait().await;

    deadline.arm_after(Duration::from_millis(10));
    deadline.disarm();
    tokio::select! {
        _ = deadline.wait() => panic!("disarmed deadline fired"),
        _ = tokio::time::advance(Duration::from_secs(1)) => {}
    }
}
