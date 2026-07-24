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
            "speech_completed",
            "turn_completed",
        ]
    );
}
