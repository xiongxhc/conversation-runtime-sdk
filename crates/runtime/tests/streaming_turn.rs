use std::sync::Arc;
use std::time::Duration;

use conversation_model_adapters::{
    AudioFrame, MockContinuousAudioOutput, MockGenerationLanguageModel,
    MockStreamingSpeechSynthesizer, PcmFormat, PcmSampleFormat,
};
use conversation_protocol::{
    GenerationId, RuntimeEvent, RuntimeTimingMilestone, TurnId, UtteranceId,
};
use conversation_runtime::{StreamingTurnEventStream, StreamingTurnRuntime, UtteranceAssembler};
use tokio::time::timeout;

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

#[tokio::test]
async fn first_playable_publication_precedes_frame_enqueue_under_backpressure() {
    let turn_id = TurnId::new(9);
    let generation_id = GenerationId::new(10);
    let expected_frame = frame(
        turn_id,
        generation_id,
        UtteranceId::new(1),
        0,
        PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
    );
    let language = Arc::new(MockGenerationLanguageModel::new(std::iter::repeat_n(
        "x", 27,
    )));
    let speech = Arc::new(MockStreamingSpeechSynthesizer::new(
        [expected_frame.clone()],
    ));
    let output = Arc::new(MockContinuousAudioOutput::new());
    let runtime = StreamingTurnRuntime::new(language, speech.clone(), output.clone());
    let mut stream = runtime
        .start_turn(turn_id, generation_id, "question")
        .await
        .unwrap();

    timeout(Duration::from_secs(1), async {
        while speech.requests().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("speech request never started");
    tokio::task::yield_now().await;
    assert!(
        output.frames().is_empty(),
        "frame enqueue ran while FirstPlayableAudio publication was blocked"
    );

    let observed = drain(&mut stream).await;

    assert_eq!(output.frames(), vec![expected_frame]);
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
