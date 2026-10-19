//! Recording must remain independent of recognition capacity and recognition consumer failure.
use conversation_model_adapters::{
    frame_for_network_capture, AdapterError, AdapterFuture, AudioCapture, AudioFrame,
    CaptureDiscontinuityReason, CaptureEvent, CaptureMetadata, NetworkMediaHub,
    NetworkVoiceIoFactory, OpenAiSpeechRecognitionConfig, OpenAiSpeechRecognizer, PcmFormat,
    PcmSampleFormat, VoiceInputEvent, VoiceIoFactory, VoiceIoSession, MAX_PCM_FRAME_BYTES,
};
use conversation_protocol::SessionId;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const SOURCE: SessionId = SessionId::new(41);
const PROMPT: Duration = Duration::from_millis(500);

type Recording = mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>;
type Recognition = mpsc::Receiver<Result<CaptureEvent, AdapterError>>;

async fn ports(
    operation_timeout: Duration,
    cancellation: &CancellationToken,
) -> (NetworkMediaHub, VoiceIoSession, Recording, Recognition) {
    let hub = NetworkMediaHub::with_limits(16 * MAX_PCM_FRAME_BYTES, operation_timeout);
    hub.reserve(SOURCE, 10).await.unwrap();
    let factory = NetworkVoiceIoFactory::new(hub.clone(), SOURCE);
    let io = factory.start(SOURCE, cancellation.clone()).await.unwrap();
    let recording = io.input.start(SOURCE, cancellation.clone()).await.unwrap();
    let recognition = factory
        .recognition_capture()
        .start(SOURCE, cancellation.clone())
        .await
        .unwrap();
    (hub, io, recording, recognition)
}

fn frame(sequence: u64) -> AudioFrame {
    frame_for_network_capture(
        sequence,
        PcmFormat::new(24000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        vec![0, 0],
    )
    .unwrap()
}

fn recorded_sequence(event: VoiceInputEvent) -> u64 {
    match event {
        VoiceInputEvent::Capture(CaptureEvent::Frame(frame)) => frame.sequence(),
        other => panic!("expected a recording frame, got {other:?}"),
    }
}

/// Fills the recognition queue while the recording consumer keeps up.
async fn stall_recognition(
    hub: &NetworkMediaHub,
    recording: &mut Recording,
    recognition: &Recognition,
) -> u64 {
    let capacity = recognition.max_capacity() as u64;
    for sequence in 1..=capacity {
        hub.deliver_capture(SOURCE, frame(sequence)).await.unwrap();
        assert_eq!(
            recorded_sequence(recording.recv().await.unwrap().unwrap()),
            sequence
        );
    }
    assert_eq!(recognition.len(), capacity as usize);
    capacity
}

async fn finish(hub: NetworkMediaHub, io: VoiceIoSession, cancellation: CancellationToken) {
    cancellation.cancel();
    io.completion.await.unwrap().unwrap();
    hub.shutdown().await;
}

#[tokio::test]
async fn recording_frames_are_not_blocked_by_a_stalled_recognition_consumer() {
    let cancellation = CancellationToken::new();
    // Long operation timeout: any recognition coupling shows up as a blocked recording frame.
    let (hub, io, mut recording, recognition) = ports(Duration::from_secs(10), &cancellation).await;
    let filled = stall_recognition(&hub, &mut recording, &recognition).await;
    for sequence in filled + 1..=filled + 4 {
        let delivered = timeout(PROMPT, hub.deliver_capture(SOURCE, frame(sequence))).await;
        assert!(
            matches!(delivered, Ok(Ok(()))),
            "recording frame {sequence} was blocked or refused by recognition: {delivered:?}"
        );
        let recorded = timeout(PROMPT, recording.recv()).await;
        assert_eq!(
            recorded_sequence(
                recorded
                    .expect("recording frame was not delivered")
                    .unwrap()
                    .unwrap()
            ),
            sequence
        );
    }
    assert_eq!(recognition.len(), filled as usize);
    drop(recognition);
    finish(hub, io, cancellation).await;
}

#[tokio::test]
async fn recording_disconnect_and_reconnect_metadata_are_not_dropped_by_a_stalled_recognition_consumer(
) {
    let cancellation = CancellationToken::new();
    // Short operation timeout: the old joint reservation drops both recorder events with an error.
    let (hub, io, mut recording, recognition) =
        ports(Duration::from_millis(200), &cancellation).await;
    stall_recognition(&hub, &mut recording, &recognition).await;
    let delivered = timeout(
        PROMPT,
        hub.deliver_disconnect_for_reservation(SOURCE, 10, 1_300, Some(3)),
    )
    .await;
    assert!(
        matches!(delivered, Ok(Ok(()))),
        "recording disconnect was blocked or dropped by recognition: {delivered:?}"
    );
    assert_eq!(
        recording.try_recv().ok().map(Result::unwrap),
        Some(VoiceInputEvent::Capture(CaptureEvent::Discontinuity {
            at_ms: 1_300
        })),
        "recording gap was dropped"
    );
    assert_eq!(
        recording.try_recv().ok().map(Result::unwrap),
        Some(VoiceInputEvent::CaptureMetadata(
            CaptureMetadata::Discontinuity {
                capture_epoch: 3,
                reason: CaptureDiscontinuityReason::Reconnect,
            }
        )),
        "recording Reconnect metadata was dropped"
    );
    drop(recognition);
    finish(hub, io, cancellation).await;
}

#[tokio::test]
async fn recognition_overflow_is_accounted_as_one_discontinuity_before_its_next_frame() {
    let cancellation = CancellationToken::new();
    let (hub, io, mut recording, mut recognition) =
        ports(Duration::from_secs(10), &cancellation).await;
    let filled = stall_recognition(&hub, &mut recording, &recognition).await;
    // Two frames and one live discontinuity are lost to recognition, never to recording.
    for sequence in [filled + 1, filled + 2] {
        timeout(PROMPT, hub.deliver_capture(SOURCE, frame(sequence)))
            .await
            .expect("recording frame was blocked")
            .unwrap();
        assert_eq!(
            recorded_sequence(recording.recv().await.unwrap().unwrap()),
            sequence
        );
    }
    timeout(PROMPT, hub.deliver_discontinuity(SOURCE, 77))
        .await
        .expect("recording discontinuity was blocked")
        .unwrap();
    assert_eq!(
        recording.recv().await.unwrap().unwrap(),
        VoiceInputEvent::Capture(CaptureEvent::Discontinuity { at_ms: 77 })
    );
    for sequence in 1..=filled {
        match recognition.recv().await.unwrap().unwrap() {
            CaptureEvent::Frame(frame) => assert_eq!(frame.sequence(), sequence),
            other => panic!("expected queued recognition frame, got {other:?}"),
        }
    }
    let resumed = filled + 3;
    hub.deliver_capture(SOURCE, frame(resumed)).await.unwrap();
    assert_eq!(
        recorded_sequence(recording.recv().await.unwrap().unwrap()),
        resumed
    );
    // The earliest loss was a frame (position unknown, at_ms 0); all losses collapse into one gap.
    assert_eq!(
        recognition.recv().await.unwrap().unwrap(),
        CaptureEvent::Discontinuity { at_ms: 0 }
    );
    match recognition.recv().await.unwrap().unwrap() {
        CaptureEvent::Frame(frame) => assert_eq!(frame.sequence(), resumed),
        other => panic!("expected resumed recognition frame, got {other:?}"),
    }
    assert!(recognition.try_recv().is_err());
    hub.deliver_capture(SOURCE, frame(resumed + 1))
        .await
        .unwrap();
    match recognition.recv().await.unwrap().unwrap() {
        CaptureEvent::Frame(frame) => assert_eq!(frame.sequence(), resumed + 1),
        other => panic!("gap was reported twice: {other:?}"),
    }
    finish(hub, io, cancellation).await;
}

/// Unchanged contract: a closed recognition receiver (media lifetime ended) still fails capture
/// closed before recording is touched, so no one-sided frame is published.
#[tokio::test]
async fn closed_recognition_consumer_still_fails_closed_without_a_one_sided_recording_frame() {
    let cancellation = CancellationToken::new();
    let (hub, io, mut recording, recognition) = ports(Duration::from_secs(10), &cancellation).await;
    drop(recognition);
    let delivered = timeout(PROMPT, hub.deliver_capture(SOURCE, frame(1)))
        .await
        .expect("closed recognizer check blocked");
    assert!(delivered.is_err());
    assert!(
        recording.try_recv().is_err(),
        "recording received a refused frame"
    );
    finish(hub, io, cancellation).await;
}

#[tokio::test]
async fn full_recording_queue_still_fails_closed_without_a_one_sided_recognition_frame() {
    let cancellation = CancellationToken::new();
    let (hub, io, recording, mut recognition) =
        ports(Duration::from_millis(100), &cancellation).await;
    let capacity = recording.max_capacity() as u64;
    for sequence in 1..=capacity {
        hub.deliver_capture(SOURCE, frame(sequence)).await.unwrap();
        recognition.recv().await.unwrap().unwrap();
    }
    assert!(hub
        .deliver_capture(SOURCE, frame(capacity + 1))
        .await
        .is_err());
    assert!(
        recognition.try_recv().is_err(),
        "recognition received a frame recording refused"
    );
    drop(recording);
    finish(hub, io, cancellation).await;
}

/// Catches a gap owed at the wrong time: with one free recognition slot and an owed gap, neither
/// the gap alone nor the frame alone may be sent; both wait for the next frame that fits.
#[tokio::test]
async fn one_free_recognition_slot_with_an_owed_gap_sends_neither_the_gap_nor_the_frame_alone() {
    let cancellation = CancellationToken::new();
    let (hub, io, mut recording, mut recognition) =
        ports(Duration::from_secs(10), &cancellation).await;
    let filled = stall_recognition(&hub, &mut recording, &recognition).await;
    for sequence in [filled + 1, filled + 2] {
        if sequence == filled + 2 {
            // Exactly one slot frees while the gap from `filled + 1` is owed.
            match recognition.recv().await.unwrap().unwrap() {
                CaptureEvent::Frame(frame) => assert_eq!(frame.sequence(), 1),
                other => panic!("expected the first queued frame, got {other:?}"),
            }
        }
        timeout(PROMPT, hub.deliver_capture(SOURCE, frame(sequence)))
            .await
            .expect("recording frame was blocked")
            .unwrap();
        assert_eq!(
            recorded_sequence(recording.recv().await.unwrap().unwrap()),
            sequence
        );
    }
    assert_eq!(recognition.len(), filled as usize - 1);
    for sequence in 2..=filled {
        match recognition.recv().await.unwrap().unwrap() {
            CaptureEvent::Frame(frame) => assert_eq!(frame.sequence(), sequence),
            other => panic!("a lone gap or frame entered the last free slot: {other:?}"),
        }
    }
    let resumed = filled + 3;
    hub.deliver_capture(SOURCE, frame(resumed)).await.unwrap();
    recording.recv().await.unwrap().unwrap();
    assert_eq!(
        recognition.recv().await.unwrap().unwrap(),
        CaptureEvent::Discontinuity { at_ms: 0 }
    );
    match recognition.recv().await.unwrap().unwrap() {
        CaptureEvent::Frame(frame) => assert_eq!(frame.sequence(), resumed),
        other => panic!("expected resumed recognition frame, got {other:?}"),
    }
    finish(hub, io, cancellation).await;
}

/// Catches a doubled gap: an owed gap and a live discontinuity that fits collapse into one
/// recognition discontinuity (carrying the earliest loss), and the next frame follows directly.
#[tokio::test]
async fn an_owed_gap_and_a_fitting_live_discontinuity_collapse_into_one() {
    let cancellation = CancellationToken::new();
    let (hub, io, mut recording, mut recognition) =
        ports(Duration::from_secs(10), &cancellation).await;
    let filled = stall_recognition(&hub, &mut recording, &recognition).await;
    hub.deliver_capture(SOURCE, frame(filled + 1))
        .await
        .unwrap();
    recording.recv().await.unwrap().unwrap();
    recognition.recv().await.unwrap().unwrap();
    timeout(PROMPT, hub.deliver_discontinuity(SOURCE, 77))
        .await
        .expect("recording discontinuity was blocked")
        .unwrap();
    assert_eq!(
        recording.recv().await.unwrap().unwrap(),
        VoiceInputEvent::Capture(CaptureEvent::Discontinuity { at_ms: 77 })
    );
    for sequence in 2..=filled {
        match recognition.recv().await.unwrap().unwrap() {
            CaptureEvent::Frame(frame) => assert_eq!(frame.sequence(), sequence),
            other => panic!("expected queued recognition frame, got {other:?}"),
        }
    }
    assert_eq!(
        recognition.recv().await.unwrap().unwrap(),
        CaptureEvent::Discontinuity { at_ms: 0 }
    );
    hub.deliver_capture(SOURCE, frame(filled + 2))
        .await
        .unwrap();
    recording.recv().await.unwrap().unwrap();
    match recognition.recv().await.unwrap().unwrap() {
        CaptureEvent::Frame(frame) => assert_eq!(frame.sequence(), filled + 2),
        other => panic!("the collapsed gap was reported twice: {other:?}"),
    }
    assert!(recognition.try_recv().is_err());
    finish(hub, io, cancellation).await;
}

// ---- Provider startup: recognition must survive a slow connect; recording never waits. ----

/// Fixed non-secret test value; no test reaches a real provider.
const ASR_CREDENTIAL_ENV: &str = "CONVERSATION_TEST_NETWORK_ASR_CREDENTIAL";
/// 20 ms of 24 kHz mono s16le, the bridge's capture frame.
const FRAME_20MS_BYTES: usize = 960;

struct HeldCapture(std::sync::Mutex<Option<Recognition>>);

impl AudioCapture for HeldCapture {
    fn start<'a>(
        &'a self,
        _session: SessionId,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, Recognition> {
        Box::pin(async move { Ok(self.0.lock().unwrap().take().unwrap()) })
    }
}

fn frame_20ms(sequence: u64) -> AudioFrame {
    frame_for_network_capture(
        sequence,
        PcmFormat::new(24000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        vec![0; FRAME_20MS_BYTES],
    )
    .unwrap()
}

/// A transcription provider that holds the WebSocket upgrade for `delay` (TCP is accepted at
/// once), then completes the session handshake and counts audio appends.
struct SlowProvider {
    endpoint: String,
    connected: tokio::sync::oneshot::Receiver<()>,
    appends: Arc<AtomicUsize>,
    worker: tokio::task::JoinHandle<()>,
}

async fn slow_provider(delay: Duration) -> SlowProvider {
    use tokio_tungstenite::tungstenite::Message;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "ws://{}/v1/realtime?intent=transcription",
        listener.local_addr().unwrap()
    );
    let (connected_tx, connected) = tokio::sync::oneshot::channel();
    let appends = Arc::new(AtomicUsize::new(0));
    let counted = appends.clone();
    let worker = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(delay).await;
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                json!({"type":"session.created","session":{"type":"transcription"}})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let update = socket.next().await.unwrap().unwrap();
        let update: Value = serde_json::from_str(update.to_text().unwrap()).unwrap();
        socket
            .send(Message::Text(
                json!({"type":"session.updated","session":update["session"]})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let _ = connected_tx.send(());
        while let Some(Ok(message)) = socket.next().await {
            if let Message::Text(text) = message {
                if text.contains("input_audio_buffer.append") {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    });
    SlowProvider {
        endpoint,
        connected,
        appends,
        worker,
    }
}

fn slow_recognizer(
    endpoint: &str,
    response_start_timeout: Duration,
    recognition: Recognition,
) -> OpenAiSpeechRecognizer {
    std::env::set_var(ASR_CREDENTIAL_ENV, "fake-not-a-real-credential-0000");
    let config =
        OpenAiSpeechRecognitionConfig::new(endpoint, "configured-model", ASR_CREDENTIAL_ENV)
            .unwrap()
            .with_server_vad(0.5, 300, 500)
            .unwrap()
            .with_response_start_timeout(response_start_timeout)
            .unwrap();
    OpenAiSpeechRecognizer::new(
        config,
        Arc::new(HeldCapture(std::sync::Mutex::new(Some(recognition)))),
    )
}

async fn deliver_and_record(hub: &NetworkMediaHub, recording: &mut Recording, sequence: u64) {
    timeout(PROMPT, hub.deliver_capture(SOURCE, frame_20ms(sequence)))
        .await
        .expect("recording frame was blocked by recognition startup")
        .unwrap();
    let recorded = timeout(PROMPT, recording.recv())
        .await
        .expect("recording frame was not delivered");
    assert_eq!(recorded_sequence(recorded.unwrap().unwrap()), sequence);
    // Real capture arrives every 20 ms; on this single-threaded runtime, yield so the
    // recognizer task gets the turn it would have between real frames.
    tokio::task::yield_now().await;
}

/// Catches recognition dying at provider startup: 2 s of capture (100 frames, far beyond the
/// hub's 16-frame queue) arrives while the provider holds its WebSocket upgrade for 1 s. Every
/// frame must reach the provider in order with no gap, and recording must never wait.
#[tokio::test]
async fn recognition_survives_a_slow_provider_connect_while_recording_continues() {
    let cancellation = CancellationToken::new();
    let (hub, io, mut recording, recognition) = ports(Duration::from_secs(10), &cancellation).await;
    let mut provider = slow_provider(Duration::from_secs(1)).await;
    let recognizer = slow_recognizer(&provider.endpoint, Duration::from_secs(10), recognition);
    let mut stream = recognizer
        .start_segments(SOURCE, cancellation.clone())
        .await
        .unwrap();
    for sequence in 1..=100 {
        deliver_and_record(&hub, &mut recording, sequence).await;
    }
    timeout(Duration::from_secs(5), &mut provider.connected)
        .await
        .expect("provider handshake did not finish")
        .unwrap();
    deliver_and_record(&hub, &mut recording, 101).await;
    let all_appended = timeout(Duration::from_secs(5), async {
        while provider.appends.load(Ordering::SeqCst) < 101 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let appended = provider.appends.load(Ordering::SeqCst);
    let failure = stream.events.try_recv();
    assert!(
        all_appended.is_ok() && appended == 101,
        "recognition lost startup capture: {appended} of 101 frames appended, stream {failure:?}"
    );
    assert!(failure.is_err(), "recognition failed: {failure:?}");
    cancellation.cancel();
    drop(stream.events);
    let actor = timeout(Duration::from_secs(2), stream.completion).await;
    assert!(matches!(actor, Ok(Ok(Ok(())))), "{actor:?}");
    provider.worker.abort();
    io.completion.await.unwrap().unwrap();
    hub.shutdown().await;
}

/// Catches an unbounded or silently lossy startup buffer: with a 1 s startup window the
/// recognizer buffers at most 1 s + 2 s margin = 3 s (150 frames). 400 frames arrive during
/// connect, so recognition overflows into an owed gap, which the recognizer reports as an error
/// (existing contract), while recording receives every frame without waiting.
#[tokio::test]
async fn startup_buffer_overflow_is_accounted_as_a_gap_and_never_blocks_recording() {
    let cancellation = CancellationToken::new();
    let (hub, io, mut recording, recognition) = ports(Duration::from_secs(10), &cancellation).await;
    let mut provider = slow_provider(Duration::from_millis(300)).await;
    let recognizer = slow_recognizer(&provider.endpoint, Duration::from_secs(1), recognition);
    let mut stream = recognizer
        .start_segments(SOURCE, cancellation.clone())
        .await
        .unwrap();
    for sequence in 1..=400 {
        deliver_and_record(&hub, &mut recording, sequence).await;
    }
    timeout(Duration::from_secs(5), &mut provider.connected)
        .await
        .expect("provider handshake did not finish")
        .unwrap();
    let mut failed = None;
    for sequence in 401..=500 {
        deliver_and_record(&hub, &mut recording, sequence).await;
        if let Ok(Some(event)) = timeout(Duration::from_millis(20), stream.recv()).await {
            failed = Some(event);
            break;
        }
    }
    let failed = failed.expect("the owed recognition gap was never reported");
    assert!(
        failed
            .as_ref()
            .is_err_and(|error| error.to_string().contains("capture discontinuity")),
        "{failed:?}"
    );
    // 150 buffered by the recognizer + 16 queued in the hub; nothing after the gap.
    let appended = provider.appends.load(Ordering::SeqCst);
    assert!(
        appended <= 166,
        "{appended} frames appended past the startup budget"
    );
    cancellation.cancel();
    drop(stream.events);
    let _ = timeout(Duration::from_secs(2), stream.completion).await;
    provider.worker.abort();
    io.completion.await.unwrap().unwrap();
    hub.shutdown().await;
}
