use conversation_model_adapters::{
    AdapterError, AudioCapture, AudioFrame, CaptureEvent, ContinuousAudioOutput,
    GenerationLanguageModel, GenerationLanguageRequest, LanguageModelInput, MockAudioCapture,
    MockContinuousAudioOutput, MockGenerationLanguageModel, MockSpeechRecognizer,
    MockStreamingSpeechSynthesizer, MockVoiceInput, MockVoiceIoFactory, PcmFormat, PcmSampleFormat,
    RecognitionEvent, RecognitionHypothesis, SpeechRecognizer, StreamingSpeechRequest,
    StreamingSpeechSynthesizer, VoiceInput, VoiceInputEvent, VoiceIoFactory,
};
use conversation_protocol::{
    ContextSource, ConversationMessage, ConversationMode, ConversationRole, GenerationId,
    MemoryContextItem, MemoryId, MemoryKind, MemoryRetrievalReason, PlaybackState, QualityDecision,
    ResponseControls, RuntimeStage, SessionId, TurnId, UtteranceId,
};
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;

#[test]
fn adapter_errors_keep_new_untyped_and_retain_explicit_stage_provenance() {
    let untyped = AdapterError::new("generic adapter failure");
    let staged =
        AdapterError::new("output permission denied").with_stage(RuntimeStage::AudioOutput);

    assert_eq!(untyped.stage(), None);
    assert_eq!(staged.stage(), Some(RuntimeStage::AudioOutput));
    assert_eq!(staged.message(), "output permission denied");
}

#[tokio::test]
async fn mock_voice_input_emits_partial_without_marking_it_final() {
    let input = MockVoiceInput::new([
        VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(
            RecognitionHypothesis::partial(4, "hel"),
        )),
        VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(
            RecognitionHypothesis::engine_final(4, "hello"),
        )),
    ]);
    let mut events = input
        .start(SessionId::new(1), CancellationToken::new())
        .await
        .unwrap();

    assert!(matches!(
        events.recv().await.unwrap().unwrap(),
        VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(value))
            if !value.is_engine_final()
    ));
}

#[tokio::test]
async fn receiver_mocks_cancel_blocked_sends_and_close_their_streams() {
    let capture =
        MockAudioCapture::new([CaptureEvent::Frame(frame(0)), CaptureEvent::Frame(frame(1))]);
    let cancellation = CancellationToken::new();
    let mut events = capture
        .start(SessionId::new(1), cancellation.clone())
        .await
        .unwrap();
    timeout(Duration::from_secs(1), capture.wait_for_blocked_send())
        .await
        .unwrap();
    cancellation.cancel();
    assert!(events.recv().await.unwrap().is_ok());
    assert!(events.recv().await.is_none());

    let recognition = MockSpeechRecognizer::new([
        RecognitionEvent::Hypothesis(RecognitionHypothesis::partial(1, "hello")),
        RecognitionEvent::Hypothesis(RecognitionHypothesis::engine_final(1, "hello")),
    ]);
    let cancellation = CancellationToken::new();
    let mut events = recognition
        .start(SessionId::new(1), cancellation.clone())
        .await
        .unwrap();
    timeout(Duration::from_secs(1), recognition.wait_for_blocked_send())
        .await
        .unwrap();
    cancellation.cancel();
    assert!(events.recv().await.unwrap().is_ok());
    assert!(events.recv().await.is_none());

    let input = MockVoiceInput::new([
        VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(
            RecognitionHypothesis::partial(1, "hello"),
        )),
        VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(
            RecognitionHypothesis::engine_final(1, "hello"),
        )),
    ]);
    let cancellation = CancellationToken::new();
    let mut events = input
        .start(SessionId::new(1), cancellation.clone())
        .await
        .unwrap();
    timeout(Duration::from_secs(1), input.wait_for_blocked_send())
        .await
        .unwrap();
    cancellation.cancel();
    assert!(events.recv().await.unwrap().is_ok());
    assert!(events.recv().await.is_none());

    let language = MockGenerationLanguageModel::new(["hello", " world"]);
    let cancellation = CancellationToken::new();
    let mut deltas = language.stream(
        GenerationLanguageRequest::new(TurnId::new(2), GenerationId::new(3), "hi"),
        cancellation.clone(),
    );
    timeout(Duration::from_secs(1), language.wait_for_blocked_send())
        .await
        .unwrap();
    cancellation.cancel();
    assert!(deltas.recv().await.unwrap().is_ok());
    assert!(deltas.recv().await.is_none());

    let speech = MockStreamingSpeechSynthesizer::new([frame(0), frame(1)]);
    let cancellation = CancellationToken::new();
    let mut frames = speech.stream(
        StreamingSpeechRequest::new(
            TurnId::new(2),
            GenerationId::new(3),
            UtteranceId::new(4),
            "hello",
        ),
        cancellation.clone(),
    );
    timeout(Duration::from_secs(1), speech.wait_for_blocked_send())
        .await
        .unwrap();
    cancellation.cancel();
    assert!(frames.recv().await.unwrap().is_ok());
    assert!(frames.recv().await.is_none());
}

#[tokio::test]
async fn generation_language_preserves_request_and_delta_identities() {
    let language = MockGenerationLanguageModel::new(["hello"]);
    let request = GenerationLanguageRequest::new(TurnId::new(5), GenerationId::new(6), "hi");

    let mut deltas = language.stream(request.clone(), CancellationToken::new());
    let delta = deltas.recv().await.unwrap().unwrap();

    assert_eq!(request.turn_id(), TurnId::new(5));
    assert_eq!(request.generation_id(), GenerationId::new(6));
    assert_eq!(delta.turn_id(), request.turn_id());
    assert_eq!(delta.generation_id(), request.generation_id());
    assert_eq!(delta.delta(), "hello");
    assert_eq!(language.requests(), vec![request]);
}

#[test]
fn generation_language_carries_bounded_typed_quality_input() {
    let turn_id = TurnId::new(5);
    let decision = QualityDecision::new(
        turn_id,
        ConversationMode::DirectAnswer,
        ResponseControls::default(),
        [],
        2,
        [ContextSource::SavedPersona, ContextSource::RecentHistory],
    )
    .unwrap();
    let history = [
        ConversationMessage::new(ConversationRole::User, "earlier question").unwrap(),
        ConversationMessage::new(ConversationRole::Assistant, "earlier answer").unwrap(),
    ];
    let input = LanguageModelInput::with_quality(
        "current question",
        history.clone(),
        decision.clone(),
        "Answer directly within the resolved spoken-duration limit.",
    )
    .unwrap();
    let request =
        GenerationLanguageRequest::from_input(turn_id, GenerationId::new(6), input).unwrap();

    assert_eq!(request.transcript(), "current question");
    assert_eq!(request.input().recent_messages(), &history);
    assert_eq!(request.input().quality_decision(), Some(&decision));
    assert_eq!(
        request.input().runtime_guidance(),
        Some("Answer directly within the resolved spoken-duration limit.")
    );
}

#[test]
fn language_input_enforces_a_forty_kib_aggregate_with_typed_memory() {
    let turn_id = TurnId::new(5);
    let decision = QualityDecision::new(
        turn_id,
        ConversationMode::DirectAnswer,
        ResponseControls::default(),
        [],
        0,
        [],
    )
    .unwrap();
    let transcript = "t".repeat(16 * 1024);
    let history =
        [ConversationMessage::new(ConversationRole::User, "h".repeat(16 * 1024)).unwrap()];
    let guidance = "g".repeat(4 * 1024);
    let full_item = MemoryContextItem::new(
        MemoryId::new(1).unwrap(),
        MemoryKind::Semantic,
        "m".repeat(4 * 1024),
        MemoryRetrievalReason::ExactPhrase,
    )
    .unwrap();

    let exact = LanguageModelInput::with_quality_and_memory(
        transcript.clone(),
        history.clone(),
        decision.clone(),
        guidance.clone(),
        [full_item.clone()],
    )
    .unwrap();
    assert_eq!(exact.memory_items(), std::slice::from_ref(&full_item));

    let extra_item = MemoryContextItem::new(
        MemoryId::new(2).unwrap(),
        MemoryKind::Episodic,
        "x",
        MemoryRetrievalReason::SharedTerm,
    )
    .unwrap();
    let error = LanguageModelInput::with_quality_and_memory(
        transcript,
        history,
        decision,
        guidance,
        [full_item, extra_item],
    )
    .unwrap_err();
    assert_eq!(
        error.message(),
        "language-model aggregate input exceeds 40 KiB"
    );
}

#[tokio::test]
async fn streaming_speech_preserves_request_frame_and_snapshot_identities() {
    let request = StreamingSpeechRequest::new(
        TurnId::new(7),
        GenerationId::new(8),
        UtteranceId::new(9),
        "hello",
    );
    let expected_frame = AudioFrame::new(
        request.turn_id(),
        request.generation_id(),
        request.utterance_id(),
        0,
        PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        vec![0; 960],
    )
    .unwrap();
    let speech = MockStreamingSpeechSynthesizer::new([expected_frame.clone()]);

    let mut frames = speech.stream(request.clone(), CancellationToken::new());
    let frame = frames.recv().await.unwrap().unwrap();

    assert_eq!(frame.turn_id(), request.turn_id());
    assert_eq!(frame.generation_id(), request.generation_id());
    assert_eq!(frame.utterance_id(), request.utterance_id());
    assert_eq!(frame, expected_frame);
    assert_eq!(speech.requests(), vec![request]);
}

#[tokio::test]
async fn continuous_output_rejects_cancelled_enqueue_and_records_flushes() {
    let output = MockContinuousAudioOutput::new();
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert!(output.enqueue(frame(0), cancellation).await.is_err());
    output
        .flush(SessionId::new(1), GenerationId::new(3))
        .await
        .unwrap();

    assert!(output.frames().is_empty());
    assert_eq!(
        output.flushed_generations(),
        vec![(SessionId::new(1), GenerationId::new(3))]
    );
}

#[tokio::test]
async fn continuous_output_records_frames_and_returns_accepted_generation_receipts() {
    let output = MockContinuousAudioOutput::new();
    let expected_frame = frame(0);

    let receipt = output
        .enqueue(expected_frame.clone(), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(receipt.generation_id(), expected_frame.generation_id());
    assert_eq!(receipt.state(), PlaybackState::Accepted);
    assert_eq!(output.frames(), vec![expected_frame]);
}

#[tokio::test]
async fn voice_io_factory_is_inert_until_start_and_joins_owned_completion() {
    let factory = MockVoiceIoFactory::new([VoiceInputEvent::Recognition(
        RecognitionEvent::Hypothesis(RecognitionHypothesis::partial(1, "hello")),
    )]);
    assert_eq!(factory.start_count(), 0);

    let cancellation = CancellationToken::new();
    let session = factory
        .start(SessionId::new(1), cancellation.clone())
        .await
        .unwrap();
    assert_eq!(factory.start_count(), 1);

    cancellation.cancel();
    assert!(session.completion.await.unwrap().is_ok());
}

fn frame(sequence: u64) -> AudioFrame {
    AudioFrame::new(
        TurnId::new(2),
        GenerationId::new(3),
        UtteranceId::new(4),
        sequence,
        PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        vec![0; 960],
    )
    .unwrap()
}
