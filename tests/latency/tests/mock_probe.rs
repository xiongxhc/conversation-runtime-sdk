use conversation_latency_harness::measure_mock_turn;

#[tokio::test]
async fn records_the_expected_mock_turn_checkpoints() {
    let samples = measure_mock_turn("hello").await.unwrap();
    let labels: Vec<_> = samples.iter().map(|sample| sample.label()).collect();

    assert_eq!(
        labels,
        [
            "turn_started",
            "transcript_final",
            "first_text_delta",
            "speech_started",
            "first_synthesis_request",
            "first_playable_audio",
            "speech_completed",
            "turn_completed",
        ]
    );
    let milestone_samples: Vec<_> = samples
        .iter()
        .filter(|sample| {
            matches!(
                sample.label(),
                "first_text_delta" | "first_synthesis_request" | "first_playable_audio"
            )
        })
        .collect();
    assert!(milestone_samples
        .windows(2)
        .all(|pair| pair[0].elapsed() <= pair[1].elapsed()));
}
