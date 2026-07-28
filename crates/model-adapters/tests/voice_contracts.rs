use conversation_model_adapters::{
    AudioCapture, AudioFrame, CaptureEvent, ContinuousAudioOutput, GenerationLanguageModel,
    GenerationLanguageRequest, MockAudioCapture, MockContinuousAudioOutput,
    MockGenerationLanguageModel, MockSpeechRecognizer, MockStreamingSpeechSynthesizer,
    MockVoiceInput, MockVoiceIoFactory, PcmFormat, PcmSampleFormat, RecognitionEvent,
    RecognitionHypothesis, SpeechRecognizer, StreamingSpeechRequest, StreamingSpeechSynthesizer,
    VoiceInput, VoiceInputEvent, VoiceIoFactory,
};
use conversation_protocol::{GenerationId, SessionId, TurnId, UtteranceId};
use tokio_util::sync::CancellationToken;

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
async fn capture_and_recognition_close_their_streams_when_cancelled() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let mut capture = MockAudioCapture::new([CaptureEvent::Frame(frame(0))])
        .start(SessionId::new(1), cancellation.child_token())
        .await
        .unwrap();
    assert!(capture.recv().await.is_none());

    let mut recognition = MockSpeechRecognizer::new([RecognitionEvent::Hypothesis(
        RecognitionHypothesis::partial(1, "hello"),
    )])
    .start(SessionId::new(1), cancellation)
    .await
    .unwrap();
    assert!(recognition.recv().await.is_none());
}

#[tokio::test]
async fn streaming_mocks_close_their_streams_when_cancelled() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let language = MockGenerationLanguageModel::new(["hello"]);
    let mut deltas = language.stream(
        GenerationLanguageRequest::new(TurnId::new(2), GenerationId::new(3), "hi"),
        cancellation.child_token(),
    );
    assert!(deltas.recv().await.is_none());
    assert_eq!(
        language.requests(),
        vec![GenerationLanguageRequest::new(
            TurnId::new(2),
            GenerationId::new(3),
            "hi"
        )]
    );

    let speech = MockStreamingSpeechSynthesizer::new([frame(0)]);
    let mut frames = speech.stream(
        StreamingSpeechRequest::new(
            TurnId::new(2),
            GenerationId::new(3),
            UtteranceId::new(4),
            "hello",
        ),
        cancellation,
    );
    assert!(frames.recv().await.is_none());
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
