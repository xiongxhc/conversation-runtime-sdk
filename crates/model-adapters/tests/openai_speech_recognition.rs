use std::sync::Arc;

use conversation_model_adapters::{
    AdapterError, AudioFrame, CaptureEvent, MockAudioCapture, OpenAiSpeechRecognitionConfig,
    OpenAiSpeechRecognizer, PcmFormat, PcmSampleFormat, RecognitionEvent, RecognitionStream,
    SpeechRecognizer,
};
use conversation_protocol::{GenerationId, RuntimeStage, SessionId, TurnId, UtteranceId};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;

/// The value of the credential the tests place in the process environment. It
/// is a fixed non-secret string; no test ever reaches a real provider.
const CREDENTIAL_VALUE: &str = "fake-not-a-real-credential-0000";
const CREDENTIAL_ENV: &str = "CONVERSATION_TEST_ONLINE_ASR_CREDENTIAL";
const ABSENT_CREDENTIAL_ENV: &str = "CONVERSATION_TEST_ONLINE_ASR_CREDENTIAL_ABSENT";
const PRIVATE_TEXT: &str = "private meeting transcript";
const SESSION: SessionId = SessionId::new(7);

struct HeldCapture(std::sync::Mutex<Option<mpsc::Receiver<Result<CaptureEvent, AdapterError>>>>);

impl conversation_model_adapters::AudioCapture for HeldCapture {
    fn start<'a>(
        &'a self,
        _session: SessionId,
        _cancellation: CancellationToken,
    ) -> conversation_model_adapters::AdapterFuture<
        'a,
        mpsc::Receiver<Result<CaptureEvent, AdapterError>>,
    > {
        Box::pin(async move { Ok(self.0.lock().unwrap().take().unwrap()) })
    }
}

#[tokio::test]
async fn transcription_orders_committed_items_and_completed_text_replaces_deltas() {
    use tokio_tungstenite::tungstenite::Message;
    set_credential();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/v1/realtime?intent=transcription",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        timeout(Duration::from_secs(2), async move {
            let (stream, _) = listener.accept().await.map_err(|_| "accept")?;
            let mut socket = tokio_tungstenite::accept_async(stream).await.map_err(|_| "upgrade")?;
            socket.send(Message::Text(json!({"type":"session.created","session":{"type":"transcription"}}).to_string().into())).await.map_err(|_| "created")?;
            let update = socket.next().await.ok_or("update absent")?.map_err(|_| "update read")?;
            let update: Value = serde_json::from_str(update.to_text().map_err(|_| "update text")?).map_err(|_| "update JSON")?;
            socket.send(Message::Text(json!({"type":"session.updated","session":update["session"]}).to_string().into())).await.map_err(|_| "updated")?;
            let append = socket.next().await.ok_or("append absent")?.map_err(|_| "append read")?;
            let append: Value = serde_json::from_str(append.to_text().map_err(|_| "append text")?).map_err(|_| "append JSON")?;
            assert_eq!(append["type"], "input_audio_buffer.append");
            for event in [
                json!({"type":"input_audio_buffer.speech_started","item_id":"A","audio_start_ms":0}),
                json!({"type":"input_audio_buffer.speech_stopped","item_id":"A","audio_end_ms":20}),
                json!({"type":"input_audio_buffer.committed","item_id":"A","previous_item_id":null}),
                json!({"type":"conversation.item.input_audio_transcription.delta","event_id":"delta-A-1","item_id":"A","content_index":0,"delta":"discard this candidate"}),
                json!({"type":"input_audio_buffer.speech_started","item_id":"B","audio_start_ms":20}),
                json!({"type":"input_audio_buffer.speech_stopped","item_id":"B","audio_end_ms":40}),
                json!({"type":"input_audio_buffer.committed","item_id":"B","previous_item_id":"A"}),
                json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"B","content_index":0,"transcript":"second"}),
                json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"A","content_index":0,"transcript":"first"}),
            ] {
                socket.send(Message::Text(event.to_string().into())).await.map_err(|_| "item send")?;
            }
            // Keep the provider alive until the client explicitly ends the epoch.
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) { break; }
            }
            Ok::<_, &'static str>(())
        }).await.map_err(|_| "script deadline")?
    });
    let (capture_sender, capture_receiver) = mpsc::channel(1);
    let capture = Arc::new(HeldCapture(std::sync::Mutex::new(Some(capture_receiver))));
    let config = OpenAiSpeechRecognitionConfig::new(endpoint, "configured-model", CREDENTIAL_ENV)
        .unwrap()
        .with_server_vad(0.5, 300, 500)
        .unwrap();
    let format = config.input_format();
    let recognizer = OpenAiSpeechRecognizer::new(config, capture);
    let cancellation = CancellationToken::new();
    let mut segments = recognizer
        .start_segments(SESSION, cancellation.clone())
        .await
        .unwrap();
    let frame = AudioFrame::new(
        TurnId::new(1), GenerationId::new(1), UtteranceId::new(1), 1, format, vec![0;1920],
    ).unwrap().with_capture_timing(conversation_model_adapters::TimedCapture {
        epoch: conversation_model_adapters::CaptureEpoch {bridge_session_id:"bridge".into(),clock_domain_id:"capturer".into(),capture_epoch:1},
        frame: conversation_model_adapters::FrameProvenance {
            capture_epoch:1,sequence:1,output_start_sample:1000,sample_count:960,
            source_start_numerator:48000000,source_end_numerator:94080000,source_denominator:24000,source_rate_hz:48000,
            quality:conversation_model_adapters::CaptureMappingQuality::NominalFrameEndpointInterpolation,
            span:Some(conversation_model_adapters::NominalCaptureSpan {host_start_ns:1000000000,host_end_ns:1040000000,rounding_envelope_ns:1,capture_uncertainty_ns:None}),
        },
    }).unwrap();
    capture_sender
        .send(Ok(CaptureEvent::Frame(frame)))
        .await
        .unwrap();
    let result = timeout(Duration::from_secs(2), async {
        let first = segments
            .recv()
            .await
            .ok_or("first absent")?
            .map_err(|_| "first errored")?;
        let second = segments
            .recv()
            .await
            .ok_or("second absent")?
            .map_err(|_| "second errored")?;
        Ok::<_, &'static str>((first, second))
    })
    .await;
    cancellation.cancel();
    drop(capture_sender);
    drop(segments.events);
    let completion = timeout(Duration::from_secs(2), segments.completion)
        .await
        .unwrap()
        .unwrap();
    let server_result = server.await.unwrap();
    let (first, second) = result.expect("final deadline").expect("two ordered finals");
    assert_eq!(
        (first.segment_id(), first.text(), first.is_final()),
        ("s7-seg1", "first", true)
    );
    assert_eq!(
        (second.segment_id(), second.text(), second.is_final()),
        ("s7-seg2", "second", true)
    );
    assert_eq!(
        (first.provider_start_ms(), first.provider_end_ms()),
        (Some(0), Some(20))
    );
    assert_eq!(
        (second.provider_start_ms(), second.provider_end_ms()),
        (Some(20), Some(40))
    );
    let first_capture = first
        .capture_evidence()
        .capture
        .as_ref()
        .expect("submitted first item address");
    let second_capture = second
        .capture_evidence()
        .capture
        .as_ref()
        .expect("submitted second item address");
    assert_eq!(
        (
            first_capture.source_start_sample,
            first_capture.source_end_sample
        ),
        (1000, 1480)
    );
    assert_eq!(
        (
            second_capture.source_start_sample,
            second_capture.source_end_sample
        ),
        (1480, 1960)
    );
    assert_eq!(
        first
            .capture_evidence()
            .live_coverage
            .as_ref()
            .unwrap()
            .host_end_ns,
        1040000000
    );
    assert_eq!(server_result, Ok(()));
    assert_eq!(completion, Ok(()));
}

#[test]
fn transcription_configuration_cannot_widen_absolute_startup_and_send_budgets() {
    let configuration = OpenAiSpeechRecognitionConfig::new(
        "wss://api.example.invalid/realtime?intent=transcription",
        "configured-model",
        CREDENTIAL_ENV,
    )
    .unwrap();
    assert!(configuration
        .clone()
        .with_response_start_timeout(Duration::from_millis(30001))
        .is_err());
    assert!(configuration
        .with_stall_timeout(Duration::from_millis(5001))
        .is_err());
}

#[tokio::test(start_paused = true)]
async fn transcription_startup_bound_includes_capture_start_and_cancels_its_child() {
    struct PendingCapture(Arc<std::sync::Mutex<Option<CancellationToken>>>);
    impl conversation_model_adapters::AudioCapture for PendingCapture {
        fn start<'a>(
            &'a self,
            _: SessionId,
            cancellation: CancellationToken,
        ) -> conversation_model_adapters::AdapterFuture<
            'a,
            mpsc::Receiver<Result<CaptureEvent, AdapterError>>,
        > {
            Box::pin(async move {
                *self.0.lock().unwrap() = Some(cancellation);
                std::future::pending().await
            })
        }
    }
    set_credential();
    let capture_token = Arc::new(std::sync::Mutex::new(None));
    let configuration = OpenAiSpeechRecognitionConfig::new(
        "ws://127.0.0.1:1/realtime?intent=transcription",
        "configured-model",
        CREDENTIAL_ENV,
    )
    .unwrap()
    .with_server_vad(0.5, 300, 500)
    .unwrap()
    .with_response_start_timeout(Duration::from_millis(100))
    .unwrap();
    let recognizer = OpenAiSpeechRecognizer::new(
        configuration,
        Arc::new(PendingCapture(capture_token.clone())),
    );
    let parent = CancellationToken::new();
    let result = timeout(
        Duration::from_millis(200),
        recognizer.start_segments(SESSION, parent.clone()),
    )
    .await;
    let child_cancelled = capture_token
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .is_cancelled();
    let parent_cancelled = parent.is_cancelled();
    parent.cancel();
    assert!(
        matches!(result, Ok(Err(_))),
        "capture start must consume the same absolute startup budget"
    );
    assert!(
        child_cancelled,
        "failed startup must cancel its capture child"
    );
    assert!(
        !parent_cancelled,
        "recognizer startup cannot cancel independent recording"
    );
}

#[test]
fn transcription_configuration_preserves_the_complete_websocket_target() {
    // Rewriting the configured query or rejecting WSS prevents the documented
    // transcription handshake before any provider interaction is possible.
    let endpoint = "wss://example.test/v1/realtime?intent=transcription&route=a%2Fb";
    let config = OpenAiSpeechRecognitionConfig::new(endpoint, "configured-model", CREDENTIAL_ENV)
        .expect("the explicitly configured transcription WebSocket must be admitted");
    assert_eq!(config.endpoint().as_str(), endpoint);
    assert_eq!(config.model(), "configured-model");
    assert_eq!(config.input_format().sample_rate_hz(), 24_000);
    assert_eq!(config.input_format().channels(), 1);
    assert_eq!(
        config.input_format().sample_format(),
        PcmSampleFormat::Signed16LittleEndian
    );
}

#[test]
fn transcription_configuration_rejects_fragments_instead_of_erasing_them() {
    assert!(OpenAiSpeechRecognitionConfig::new(
        "https://example.test/v1#private-fragment",
        "configured-model",
        CREDENTIAL_ENV,
    )
    .is_err());
    assert!(OpenAiSpeechRecognitionConfig::new(
        "wss://example.test/v1/realtime#private-fragment",
        "configured-model",
        CREDENTIAL_ENV,
    )
    .is_err());
}

#[tokio::test]
async fn transcription_requires_explicit_server_vad_before_starting_capture() {
    set_credential();
    let config = OpenAiSpeechRecognitionConfig::new(
        "ws://127.0.0.1:9/v1/realtime?intent=transcription",
        "configured-model",
        CREDENTIAL_ENV,
    )
    .unwrap();
    let recognizer = OpenAiSpeechRecognizer::new(config, Arc::new(MockAudioCapture::new([])));
    let cancellation = CancellationToken::new();
    let result = recognizer
        .start_segments(SESSION, cancellation.clone())
        .await;
    cancellation.cancel();
    assert!(
        result.is_err(),
        "missing VAD must fail before opening a session"
    );
    assert!(result.err().unwrap().to_string().contains("server VAD"));
}

#[tokio::test]
async fn transcription_refuses_non_profile_pcm_before_opening_a_session() {
    set_credential();
    for format in [
        PcmFormat::new(16000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        PcmFormat::new(24000, 2, PcmSampleFormat::Signed16LittleEndian).unwrap(),
    ] {
        let config = OpenAiSpeechRecognitionConfig::new(
            "ws://127.0.0.1:9/v1/realtime?intent=transcription",
            "configured-model",
            CREDENTIAL_ENV,
        )
        .unwrap()
        .with_server_vad(0.5, 300, 500)
        .unwrap()
        .with_input_format(format);
        let recognizer = OpenAiSpeechRecognizer::new(config, Arc::new(MockAudioCapture::new([])));
        let cancellation = CancellationToken::new();
        let result = recognizer
            .start_segments(SESSION, cancellation.clone())
            .await;
        cancellation.cancel();
        assert!(
            result.is_err(),
            "capture bytes cannot be relabelled as 24kHz mono"
        );
    }
}

#[test]
fn transcription_refuses_a_model_documented_as_incompatible_with_server_vad() {
    assert!(OpenAiSpeechRecognitionConfig::new(
        "wss://example.test/v1/realtime?intent=transcription",
        "gpt-realtime-whisper",
        CREDENTIAL_ENV,
    )
    .unwrap()
    .with_server_vad(0.5, 300, 500)
    .is_err());
}

#[tokio::test]
async fn transcription_websocket_verifies_session_before_raw_pcm_append() {
    use tokio_tungstenite::tungstenite::{
        handshake::server::{Callback, ErrorResponse, Request, Response},
        Message,
    };
    struct VerifyHandshake;
    impl Callback for VerifyHandshake {
        fn on_request(
            self,
            request: &Request,
            response: Response,
        ) -> Result<Response, ErrorResponse> {
            assert_eq!(
                request.uri().path_and_query().unwrap().as_str(),
                "/v1/realtime?intent=transcription&route=a%2Fb"
            );
            assert_eq!(
                request.headers()["authorization"],
                format!("Bearer {CREDENTIAL_VALUE}")
            );
            Ok(response)
        }
    }
    set_credential();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/v1/realtime?intent=transcription&route=a%2Fb",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        timeout(Duration::from_secs(2), async move {
            let (stream, _) = listener.accept().await.map_err(|_| "accept failed")?;
            let mut socket = tokio_tungstenite::accept_hdr_async(stream, VerifyHandshake).await.map_err(|_| "WebSocket handshake failed")?;
            socket.send(Message::Text(json!({"type":"session.created","session":{"type":"transcription"}}).to_string().into())).await.map_err(|_| "created send failed")?;
            let update = socket.next().await.ok_or("missing update")?.map_err(|_| "update read failed")?;
            let update: Value = serde_json::from_str(update.to_text().map_err(|_| "update not text")?).map_err(|_| "update not JSON")?;
            assert_eq!(update["type"], "session.update");
            assert_eq!(update["session"]["type"], "transcription");
            assert_eq!(update["session"]["audio"]["input"]["format"], json!({"type":"audio/pcm","rate":24000}));
            assert_eq!(update["session"]["audio"]["input"]["transcription"]["model"], "configured-model");
            assert_eq!(update["session"]["audio"]["input"]["turn_detection"], json!({"type":"server_vad","threshold":0.5,"prefix_padding_ms":300,"silence_duration_ms":500}));
            // A control barrier proves the next client message cannot be PCM:
            // audio is queued, but session.updated has not yet been sent.
            socket.send(Message::Ping(vec![7].into())).await.map_err(|_| "ping failed")?;
            assert!(matches!(socket.next().await.ok_or("missing pong")?.map_err(|_| "pong failed")?, Message::Pong(_)));
            socket.send(Message::Text(json!({"type":"session.updated","session":update["session"]}).to_string().into())).await.map_err(|_| "updated failed")?;
            let append = socket.next().await.ok_or("missing append")?.map_err(|_| "append read failed")?;
            let append: Value = serde_json::from_str(append.to_text().map_err(|_| "append not text")?).map_err(|_| "append not JSON")?;
            assert_eq!(append, json!({"type":"input_audio_buffer.append","audio":"EQAiADMA"}));
            Ok::<_, &'static str>(())
        }).await.map_err(|_| "script deadline")?
    });
    let format = PcmFormat::new(24000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    let frame = AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        1,
        format,
        vec![0x11, 0, 0x22, 0, 0x33, 0],
    )
    .unwrap();
    let config = OpenAiSpeechRecognitionConfig::new(endpoint, "configured-model", CREDENTIAL_ENV)
        .unwrap()
        .with_server_vad(0.5, 300, 500)
        .unwrap();
    let recognizer = OpenAiSpeechRecognizer::new(
        config,
        Arc::new(MockAudioCapture::new([CaptureEvent::Frame(frame)])),
    );
    let cancellation = CancellationToken::new();
    let events = recognizer
        .start_segments(SESSION, cancellation.clone())
        .await
        .unwrap();
    let outcome = server.await.expect("owned server task must be reaped");
    cancellation.cancel();
    drop(events.events);
    let completion = join_owned(events.completion).await;
    assert_eq!(outcome, Ok(()));
    assert!(completion.is_ok(), "actual recognizer task must be reaped");
}

#[derive(Clone, Copy)]
enum ScriptEnd {
    Hold,
    Close,
    BarrierHold,
    WrongUpdated,
}

struct ScriptedProvider {
    endpoint: String,
    sent: tokio::sync::oneshot::Receiver<()>,
    cancellation: CancellationToken,
    worker: JoinHandle<Result<(), &'static str>>,
    appends: Arc<std::sync::atomic::AtomicUsize>,
}

impl ScriptedProvider {
    async fn start(events: Vec<tokio_tungstenite::tungstenite::Message>, end: ScriptEnd) -> Self {
        use tokio_tungstenite::tungstenite::Message;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "ws://{}/v1/realtime?intent=transcription",
            listener.local_addr().unwrap()
        );
        let cancellation = CancellationToken::new();
        let cancelled = cancellation.clone();
        let (sent, received) = tokio::sync::oneshot::channel();
        let appends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_appends = appends.clone();
        let worker = tokio::spawn(async move {
            let mut socket = timeout(Duration::from_secs(2), async {
                let (stream, _) = listener.accept().await.map_err(|_| "accept")?;
                let mut socket = tokio_tungstenite::accept_async(stream)
                    .await
                    .map_err(|_| "upgrade")?;
                socket
                    .send(Message::Text(
                        json!({"type":"session.created","session":{"type":"transcription"}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .map_err(|_| "created")?;
                let update = socket
                    .next()
                    .await
                    .ok_or("update absent")?
                    .map_err(|_| "update")?;
                let update: Value =
                    serde_json::from_str(update.to_text().map_err(|_| "update text")?)
                        .map_err(|_| "update JSON")?;
                let mut session = update["session"].clone();
                if matches!(end, ScriptEnd::WrongUpdated) {
                    session["audio"]["input"]["transcription"]["model"] = json!("unexpected-model");
                }
                socket
                    .send(Message::Text(
                        json!({"type":"session.updated","session":session})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .map_err(|_| "updated")?;
                Ok::<_, &'static str>(socket)
            })
            .await
            .map_err(|_| "startup deadline")??;
            for event in events {
                socket.send(event).await.map_err(|_| "script send")?;
            }
            if matches!(end, ScriptEnd::BarrierHold) {
                socket
                    .send(Message::Ping(b"script-consumed".as_slice().into()))
                    .await
                    .map_err(|_| "ping")?;
                timeout(Duration::from_secs(2), async {
                    loop {
                        match socket.next().await {
                            Some(Ok(Message::Pong(bytes)))
                                if bytes.as_ref() == b"script-consumed" =>
                            {
                                return Ok(())
                            }
                            Some(Ok(Message::Text(_))) => {
                                observed_appends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            }
                            _ => return Err("barrier closed"),
                        }
                    }
                })
                .await
                .map_err(|_| "barrier deadline")??;
            }
            let _ = sent.send(());
            if matches!(end, ScriptEnd::Close) {
                socket.close(None).await.map_err(|_| "close")?;
                return Ok(());
            }
            loop {
                tokio::select! {
                    biased;
                    _ = cancelled.cancelled() => return Ok(()),
                    event = socket.next() => match event {
                        Some(Ok(Message::Ping(_))) => {socket.flush().await.map_err(|_|"pong")?;},
                        Some(Ok(Message::Text(_))) => {observed_appends.fetch_add(1,std::sync::atomic::Ordering::SeqCst);},
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return Ok(()),
                        _ => {},
                    },
                }
            }
        });
        Self {
            endpoint,
            sent: received,
            cancellation,
            worker,
            appends,
        }
    }
}

fn provider_message(value: Value) -> tokio_tungstenite::tungstenite::Message {
    tokio_tungstenite::tungstenite::Message::Text(value.to_string().into())
}

fn item_events(
    id: &str,
    previous: Option<&str>,
    text: &str,
) -> Vec<tokio_tungstenite::tungstenite::Message> {
    [
        json!({"type":"input_audio_buffer.speech_started","item_id":id,"audio_start_ms":0}),
        json!({"type":"input_audio_buffer.speech_stopped","item_id":id,"audio_end_ms":20}),
        json!({"type":"input_audio_buffer.committed","item_id":id,"previous_item_id":previous}),
        json!({"type":"conversation.item.input_audio_transcription.completed","item_id":id,"content_index":0,"transcript":text}),
    ].into_iter().map(provider_message).collect()
}

fn set_credential() {
    std::env::set_var(CREDENTIAL_ENV, CREDENTIAL_VALUE);
}

fn configured(endpoint: &str) -> OpenAiSpeechRecognitionConfig {
    OpenAiSpeechRecognitionConfig::new(endpoint, "configured-model", CREDENTIAL_ENV)
        .unwrap()
        .with_server_vad(0.5, 300, 500)
        .unwrap()
}

fn held_recognizer(
    endpoint: &str,
) -> (
    OpenAiSpeechRecognizer,
    mpsc::Sender<Result<CaptureEvent, AdapterError>>,
) {
    let (sender, receiver) = mpsc::channel(1);
    (
        OpenAiSpeechRecognizer::new(
            configured(endpoint),
            Arc::new(HeldCapture(std::sync::Mutex::new(Some(receiver)))),
        ),
        sender,
    )
}

async fn join_owned<T>(mut worker: JoinHandle<T>) -> Result<T, &'static str> {
    match timeout(Duration::from_secs(2), &mut worker).await {
        Ok(result) => result.map_err(|_| "owned task panicked"),
        Err(_) => {
            worker.abort();
            let _ = worker.await;
            Err("owned task failed to reap within two seconds")
        }
    }
}

async fn finish<T>(
    stream: RecognitionStream<T>,
    cancellation: CancellationToken,
    provider: ScriptedProvider,
) -> (
    Result<Result<(), AdapterError>, &'static str>,
    Result<Result<(), &'static str>, &'static str>,
) {
    cancellation.cancel();
    drop(stream.events);
    let actor = join_owned(stream.completion).await;
    provider.cancellation.cancel();
    let server = join_owned(provider.worker).await;
    (actor, server)
}

#[tokio::test]
async fn documented_error_malformed_oversized_and_unsupported_profile_events_fail_closed_without_echo(
) {
    use tokio_tungstenite::tungstenite::Message;
    set_credential();
    for event in [
        provider_message(
            json!({"type":"error","error":{"message":format!("{PRIVATE_TEXT} {CREDENTIAL_VALUE}")}}),
        ),
        provider_message(
            json!({"type":"conversation.item.input_audio_transcription.failed","item_id":"A","content_index":0,"error":{"message":PRIVATE_TEXT}}),
        ),
        provider_message(
            json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"A","content_index":1,"transcript":PRIVATE_TEXT}),
        ),
        provider_message(
            json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"A","transcript":PRIVATE_TEXT}),
        ),
        provider_message(
            json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"A","content_index":0,"transcript":"x".repeat(16385)}),
        ),
        provider_message(
            json!({"type":"input_audio_buffer.committed","item_id":"A","previous_item_id":"A"}),
        ),
        provider_message(json!({"type":"input_audio_buffer.committed","item_id":"A"})),
        provider_message(json!({"type":"input_audio_buffer.unknown","private":PRIVATE_TEXT})),
        Message::Text(format!("malformed {PRIVATE_TEXT} {CREDENTIAL_VALUE}").into()),
        Message::Text("x".repeat(128 * 1024 + 1).into()),
        Message::Binary(vec![1, 2, 3].into()),
    ] {
        let provider = ScriptedProvider::start(vec![event], ScriptEnd::Hold).await;
        let (recognizer, _capture) = held_recognizer(&provider.endpoint);
        let cancellation = CancellationToken::new();
        let mut stream = recognizer
            .start_segments(SESSION, cancellation.clone())
            .await
            .unwrap();
        let observed = timeout(Duration::from_secs(2), stream.recv()).await;
        let (actor, server) = finish(stream, cancellation, provider).await;
        let error = observed
            .expect("terminal error deadline")
            .expect("explicit error")
            .expect_err("invalid event cannot publish wording");
        assert_eq!(error.stage(), Some(RuntimeStage::SpeechRecognizer));
        assert!(!error.message().contains(PRIVATE_TEXT));
        assert!(!error.message().contains(CREDENTIAL_VALUE));
        assert!(actor.unwrap().is_err());
        assert_eq!(server, Ok(Ok(())));
    }
}

#[tokio::test]
async fn wrong_updated_and_clean_or_abnormal_socket_loss_end_one_epoch_without_replay() {
    set_credential();
    for end in [ScriptEnd::WrongUpdated, ScriptEnd::Close, ScriptEnd::Hold] {
        let provider = ScriptedProvider::start(vec![], end).await;
        let (recognizer, _capture) = held_recognizer(&provider.endpoint);
        let cancellation = CancellationToken::new();
        let mut stream = recognizer
            .start_segments(SESSION, cancellation.clone())
            .await
            .unwrap();
        if matches!(end, ScriptEnd::Hold) {
            provider.cancellation.cancel();
        }
        let observed = timeout(Duration::from_secs(2), stream.recv()).await;
        let ended = timeout(Duration::from_secs(2), stream.recv()).await;
        let (actor, server) = finish(stream, cancellation, provider).await;
        assert!(
            matches!(observed, Ok(Some(Err(_)))),
            "loss must emit explicit incomplete error"
        );
        assert!(
            matches!(ended, Ok(None)),
            "loss must close the epoch, not retry/replay"
        );
        assert!(actor.unwrap().is_err());
        assert_eq!(server, Ok(Ok(())));
    }
}

#[tokio::test]
async fn recognition_port_emits_finalized_utterances_not_local_silence_hypotheses() {
    set_credential();
    let mut script = item_events("A", None, "first");
    script.extend(item_events("B", Some("A"), "second"));
    let provider = ScriptedProvider::start(script, ScriptEnd::Hold).await;
    let (recognizer, _capture) = held_recognizer(&provider.endpoint);
    let cancellation = CancellationToken::new();
    let mut stream = recognizer
        .start(SESSION, cancellation.clone())
        .await
        .unwrap();
    let observed = timeout(Duration::from_secs(2), async {
        let mut values = Vec::new();
        for _ in 0..2 {
            values.push(stream.recv().await);
        }
        values
    })
    .await;
    let (actor, server) = finish(stream, cancellation, provider).await;
    let values = observed.unwrap();
    for (index, value) in values.into_iter().enumerate() {
        let RecognitionEvent::FinalizedUtterance(utterance) = value.unwrap().unwrap() else {
            panic!("provider wording must bypass hypotheses")
        };
        assert_eq!(utterance.segment_id(), index as u64 + 1);
        assert_eq!(utterance.text(), ["first", "second"][index]);
        assert!(utterance.capture_evidence().unwrap().capture.is_none());
    }
    assert_eq!(actor, Ok(Ok(())));
    assert_eq!(server, Ok(Ok(())));
}

#[tokio::test]
async fn quiet_or_active_speech_without_a_commit_does_not_invent_a_completion_timeout() {
    set_credential();
    for events in [
        vec![],
        vec![provider_message(
            json!({"type":"input_audio_buffer.speech_started","item_id":"A","audio_start_ms":0}),
        )],
    ] {
        let mut provider = ScriptedProvider::start(events, ScriptEnd::BarrierHold).await;
        let (recognizer, _capture) = held_recognizer(&provider.endpoint);
        let cancellation = CancellationToken::new();
        let stream = recognizer
            .start_segments(SESSION, cancellation.clone())
            .await
            .unwrap();
        timeout(Duration::from_secs(2), &mut provider.sent)
            .await
            .unwrap()
            .unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(3600)).await;
        let prematurely_ended = stream.completion.is_finished();
        let (actor, server) = finish(stream, cancellation, provider).await;
        tokio::time::resume();
        assert!(!prematurely_ended);
        assert_eq!(actor, Ok(Ok(())));
        assert_eq!(server, Ok(Ok(())));
    }
}

#[tokio::test]
async fn capture_discontinuity_and_capture_eof_end_asr_with_owned_completion() {
    set_credential();
    for gap in [false, true] {
        let mut provider = ScriptedProvider::start(vec![], ScriptEnd::BarrierHold).await;
        let (recognizer, capture) = held_recognizer(&provider.endpoint);
        let cancellation = CancellationToken::new();
        let mut stream = recognizer
            .start_segments(SESSION, cancellation.clone())
            .await
            .unwrap();
        timeout(Duration::from_secs(2), &mut provider.sent)
            .await
            .unwrap()
            .unwrap();
        if gap {
            capture
                .send(Ok(CaptureEvent::Discontinuity { at_ms: 42 }))
                .await
                .unwrap();
        }
        drop(capture);
        let observed = timeout(Duration::from_secs(2), stream.recv()).await;
        let (actor, server) = finish(stream, cancellation, provider).await;
        assert!(matches!(observed, Ok(Some(Err(_)))));
        assert!(actor.unwrap().is_err());
        assert_eq!(server, Ok(Ok(())));
    }
}

#[tokio::test]
async fn pending_wire_items_obey_fixed_commit_predecessor_and_missing_stop_deadlines() {
    set_credential();
    let completed = json!({"type":"conversation.item.input_audio_transcription.completed","item_id":"A","content_index":0,"transcript":"authoritative"});
    for (events, seconds, final_without_timing) in [
        (
            vec![
                json!({"type":"input_audio_buffer.speech_stopped","item_id":"A","audio_end_ms":20}),
                completed.clone(),
            ],
            5,
            false,
        ),
        (
            vec![
                json!({"type":"input_audio_buffer.committed","item_id":"A","previous_item_id":"never-seen"}),
                json!({"type":"input_audio_buffer.speech_stopped","item_id":"A","audio_end_ms":20}),
                completed.clone(),
            ],
            30,
            false,
        ),
        (
            vec![
                json!({"type":"conversation.item.input_audio_transcription.delta","event_id":"delta-1","item_id":"A","content_index":0,"delta":"candidate only"}),
            ],
            5,
            false,
        ),
        (
            vec![
                json!({"type":"input_audio_buffer.committed","item_id":"A","previous_item_id":null}),
                completed,
            ],
            5,
            true,
        ),
    ] {
        let mut provider = ScriptedProvider::start(
            events.into_iter().map(provider_message).collect(),
            ScriptEnd::BarrierHold,
        )
        .await;
        let (recognizer, _capture) = held_recognizer(&provider.endpoint);
        let cancellation = CancellationToken::new();
        let mut stream = recognizer
            .start_segments(SESSION, cancellation.clone())
            .await
            .unwrap();
        timeout(Duration::from_secs(2), &mut provider.sent)
            .await
            .unwrap()
            .unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(seconds)).await;
        let observed = timeout(Duration::from_secs(1), stream.recv()).await;
        let (actor, server) = finish(stream, cancellation, provider).await;
        tokio::time::resume();
        let result = observed
            .expect("fixed item deadline must resolve")
            .expect("item must not silently disappear");
        if final_without_timing {
            let utterance = result.unwrap();
            assert_eq!(utterance.text(), "authoritative");
            assert!(utterance.capture_evidence().capture.is_none());
            assert_eq!(actor, Ok(Ok(())));
        } else {
            assert!(
                result.is_err(),
                "unresolved ordering must not publish candidate text"
            );
            assert!(actor.unwrap().is_err());
        }
        assert_eq!(server, Ok(Ok(())));
    }
}

#[tokio::test]
async fn cancellation_reaps_actor_and_recognition_relay_with_a_full_final_consumer() {
    set_credential();
    let mut script = item_events("A", None, "first");
    script.extend(item_events("B", Some("A"), "second"));
    script.extend(item_events("C", Some("B"), "third"));
    let mut provider = ScriptedProvider::start(script, ScriptEnd::Hold).await;
    let (recognizer, _capture) = held_recognizer(&provider.endpoint);
    let cancellation = CancellationToken::new();
    let stream = recognizer
        .start(SESSION, cancellation.clone())
        .await
        .unwrap();
    timeout(Duration::from_secs(2), &mut provider.sent)
        .await
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(2), async {
        while stream.events.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (actor, server) = finish(stream, cancellation, provider).await;
    assert_eq!(actor, Ok(Ok(())));
    assert_eq!(server, Ok(Ok(())));
}

// Catches cancellation/receiver-drop paths that leave a handshake socket or its
// sole capture receiver alive before either created or updated has arrived.
#[tokio::test]
async fn startup_cancellation_and_consumer_drop_reap_before_created_and_updated() {
    for after_created in [false, true] {
        for drop_consumer in [false, true] {
            set_credential();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!(
                "ws://{}/v1/realtime?intent=transcription",
                listener.local_addr().unwrap()
            );
            let (ready, reached_phase) = tokio::sync::oneshot::channel();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                if after_created {
                    socket
                        .send(provider_message(
                            json!({"type":"session.created","session":{"type":"transcription"}}),
                        ))
                        .await
                        .unwrap();
                    let update = socket.next().await.unwrap().unwrap();
                    let update: Value = serde_json::from_str(update.to_text().unwrap()).unwrap();
                    assert_eq!(update["type"], "session.update");
                }
                ready.send(()).unwrap();
                while let Some(Ok(message)) = socket.next().await {
                    assert!(
                        !matches!(message, tokio_tungstenite::tungstenite::Message::Text(_)),
                        "unverified startup uploaded audio"
                    );
                    if matches!(message, tokio_tungstenite::tungstenite::Message::Close(_)) {
                        break;
                    }
                }
            });
            let (source, source_receiver) = mpsc::channel(1);
            let recognizer = OpenAiSpeechRecognizer::new(
                configured(&endpoint),
                Arc::new(HeldCapture(std::sync::Mutex::new(Some(source_receiver)))),
            );
            let cancellation = CancellationToken::new();
            let stream = recognizer
                .start(SESSION, cancellation.clone())
                .await
                .unwrap();
            timeout(Duration::from_secs(2), reached_phase)
                .await
                .unwrap()
                .unwrap();
            if !drop_consumer {
                cancellation.cancel();
            }
            drop(stream.events);
            assert!(join_owned(stream.completion).await.unwrap().is_ok());
            join_owned(server).await.unwrap();
            assert!(
                source.is_closed(),
                "completed actor retained its capture receiver"
            );
        }
    }
}

#[tokio::test]
async fn missing_credential_and_bad_pcm_fail_before_any_audio_upload() {
    set_credential();
    let capture = Arc::new(MockAudioCapture::new([]));
    let recognizer = OpenAiSpeechRecognizer::new(
        configured("ws://127.0.0.1:1/realtime")
            .with_credential_env(ABSENT_CREDENTIAL_ENV)
            .unwrap(),
        capture,
    );
    let error = recognizer
        .start_segments(SESSION, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        error.message(),
        "online speech recognition credential is not available"
    );
    let mut provider = ScriptedProvider::start(vec![], ScriptEnd::BarrierHold).await;
    let (recognizer, capture) = held_recognizer(&provider.endpoint);
    let cancellation = CancellationToken::new();
    let mut stream = recognizer
        .start_segments(SESSION, cancellation.clone())
        .await
        .unwrap();
    timeout(Duration::from_secs(2), &mut provider.sent)
        .await
        .unwrap()
        .unwrap();
    capture
        .send(Ok(CaptureEvent::Frame(
            AudioFrame::new(
                TurnId::new(1),
                GenerationId::new(1),
                UtteranceId::new(1),
                1,
                PcmFormat::new(16000, 1, PcmSampleFormat::Float32LittleEndian).unwrap(),
                vec![0; 320],
            )
            .unwrap(),
        )))
        .await
        .unwrap();
    let observed = timeout(Duration::from_secs(2), stream.recv()).await;
    let appends = provider.appends.clone();
    let (actor, server) = finish(stream, cancellation, provider).await;
    assert!(matches!(observed, Ok(Some(Err(_)))));
    assert!(actor.unwrap().is_err());
    assert_eq!(server, Ok(Ok(())));
    assert_eq!(
        appends.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "bad PCM must not be uploaded before the error"
    );
}

#[test]
fn unusable_endpoint_and_credential_fields_are_independently_rejected() {
    for endpoint in [
        "https://example.test/v1",
        "ws://example.test/realtime",
        "wss://user:secret@example.test/realtime",
        "wss://example.test/realtime#fragment",
    ] {
        assert!(OpenAiSpeechRecognitionConfig::new(endpoint, "model", CREDENTIAL_ENV).is_err());
    }
    for name in ["lowercase", "9LEADING_DIGIT", ""] {
        assert!(
            OpenAiSpeechRecognitionConfig::new("wss://example.test/realtime", "model", name)
                .is_err()
        );
    }
}
