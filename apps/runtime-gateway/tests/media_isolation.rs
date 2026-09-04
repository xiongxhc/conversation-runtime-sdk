//! Loopback-only media-bridge behaviour tests. These exercise the binary listener rather than
//! mock calls, so removing either capture fan-out branch changes an observed result.

use std::sync::Arc;
use std::time::Duration;

use conversation_model_adapters::{
    AdapterError, AudioFrame, CaptureEvent, NetworkVoiceIoFactory, PcmFormat, PcmSampleFormat,
    VoiceInputEvent, VoiceIoFactory,
};
use conversation_protocol::{GenerationId, PlaybackState, SessionId, TurnId, UtteranceId};
use conversation_runtime_gateway::{MediaServer, MediaServerConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const RECEIVE_TIMEOUT: Duration = Duration::from_secs(2);

/// Catches either production regression: deleting the `VoiceInputEvent::Capture` delivery leaves
/// the recording path empty, while deleting the independent `AudioCapture` delivery starves the
/// recognizer. It also catches a registry keyed by connection rather than authoritative session.
#[tokio::test]
async fn each_accepted_capture_frame_reaches_recording_and_recognition_without_cross_session_leakage(
) {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let first = server.reserve(SessionId::new(41)).await.unwrap();
    let second = server.reserve(SessionId::new(42)).await.unwrap();

    let first_factory = Arc::new(NetworkVoiceIoFactory::new(server.hub(), first.session_id()));
    let second_factory = Arc::new(NetworkVoiceIoFactory::new(
        server.hub(),
        second.session_id(),
    ));
    let cancellation = CancellationToken::new();
    let first_io = first_factory
        .start(first.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let second_io = second_factory
        .start(second.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut first_recording = first_io
        .input
        .start(first.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut second_recording = second_io
        .input
        .start(second.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut first_recognition = first_factory
        .recognition_capture()
        .start(first.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut second_recognition = second_factory
        .recognition_capture()
        .start(second.session_id(), cancellation.child_token())
        .await
        .unwrap();

    let mut bridge = authenticated_bridge(&first).await;
    send_capture(
        &mut bridge,
        first.session_id(),
        7,
        1_000,
        PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        b"first-session-frame!",
    )
    .await;

    let recording = timeout(RECEIVE_TIMEOUT, first_recording.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let recognition = timeout(RECEIVE_TIMEOUT, first_recognition.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_recording_capture_payload(recording, b"first-session-frame!", 7);
    assert_capture_payload(recognition, b"first-session-frame!", 7);
    assert!(timeout(Duration::from_millis(30), second_recording.recv())
        .await
        .is_err());
    assert!(
        timeout(Duration::from_millis(30), second_recognition.recv())
            .await
            .is_err()
    );

    cancellation.cancel();
    server.shutdown().await;
}

/// Catches `reserve` replacing a live hub entry before reporting the duplicate. The first lease's
/// already-attached capture consumers must stay operational after the second reservation fails.
#[tokio::test]
async fn duplicate_reservation_keeps_the_original_live_capture_session_operational() {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let lease = server.reserve(SessionId::new(43)).await.unwrap();
    let cancellation = CancellationToken::new();
    let (_factory, mut recording, mut recognition) =
        attached_ports(&server, &lease, cancellation.clone()).await;
    assert!(server.reserve(lease.session_id()).await.is_err());

    let mut bridge = authenticated_bridge(&lease).await;
    send_capture(
        &mut bridge,
        lease.session_id(),
        1,
        1_000,
        PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        b"still-live",
    )
    .await;
    assert_recording_capture_payload(recording.recv().await.unwrap().unwrap(), b"still-live", 1);
    assert_capture_payload(recognition.recv().await.unwrap().unwrap(), b"still-live", 1);

    cancellation.cancel();
    server.shutdown().await;
}

/// Catches an unbounded bridge sender: a single session cannot turn a short burst into an
/// unbounded resident queue or exceed the explicitly configured ingress rate.
#[tokio::test]
async fn capture_ingress_refuses_a_second_frame_above_the_configured_rate() {
    let server = MediaServer::bind(MediaServerConfig::loopback().with_max_frames_per_second(1))
        .await
        .unwrap();
    let lease = server.reserve(SessionId::new(51)).await.unwrap();
    let factory = Arc::new(NetworkVoiceIoFactory::new(server.hub(), lease.session_id()));
    let cancellation = CancellationToken::new();
    let io = factory
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut recording = io
        .input
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let _recognition = factory
        .recognition_capture()
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let format = PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    let mut bridge = authenticated_bridge(&lease).await;
    send_capture(&mut bridge, lease.session_id(), 1, 1_000, format, b"one!").await;
    let first = timeout(RECEIVE_TIMEOUT, recording.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_recording_capture_payload(first, b"one!", 1);
    send_capture(&mut bridge, lease.session_id(), 2, 1_001, format, b"two!").await;
    assert_discontinuity(
        timeout(RECEIVE_TIMEOUT, recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        1_000,
    );
    cancellation.cancel();
    server.shutdown().await;
}

/// Catches sequential fan-out: when either bounded consumer is full, the other must not receive
/// an extra frame. The protocol closes the overflowing bridge rather than publishing one-sided
/// capture or fabricated silence.
#[tokio::test]
async fn full_capture_consumer_prevents_partial_fanout() {
    let server = MediaServer::bind(
        MediaServerConfig::loopback()
            .with_lifetimes(Duration::from_secs(1), Duration::from_secs(1))
            .with_max_playback_queue_duration(Duration::from_millis(50))
            .with_packet_read_timeout(Duration::from_millis(50)),
    )
    .await
    .unwrap();
    let lease = server.reserve(SessionId::new(53)).await.unwrap();
    let cancellation = CancellationToken::new();
    let (_factory, mut recording, mut recognition) =
        attached_ports(&server, &lease, cancellation.clone()).await;
    let format = PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    let mut bridge = authenticated_bridge(&lease).await;
    for sequence in 1..=17 {
        send_capture(
            &mut bridge,
            lease.session_id(),
            sequence,
            1_000 + sequence,
            format,
            b"fill",
        )
        .await;
    }
    let mut byte = [0_u8; 1];
    assert_eq!(
        timeout(RECEIVE_TIMEOUT, bridge.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    for sequence in 1..=16 {
        assert_recording_capture_payload(
            timeout(RECEIVE_TIMEOUT, recording.recv())
                .await
                .expect("recording fan-out stalled")
                .unwrap()
                .unwrap(),
            b"fill",
            sequence,
        );
        assert_capture_payload(
            timeout(RECEIVE_TIMEOUT, recognition.recv())
                .await
                .expect("recognition fan-out stalled")
                .unwrap()
                .unwrap(),
            b"fill",
            sequence,
        );
    }
    assert!(timeout(Duration::from_millis(30), recording.recv())
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(30), recognition.recv())
        .await
        .is_err());

    cancellation.cancel();
    server.shutdown().await;
}

/// Catches a bridge that rewinds capture time while keeping a superficially valid sequence.
#[tokio::test]
async fn capture_timestamp_must_be_monotonic_within_a_session() {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let lease = server.reserve(SessionId::new(61)).await.unwrap();
    let factory = Arc::new(NetworkVoiceIoFactory::new(server.hub(), lease.session_id()));
    let cancellation = CancellationToken::new();
    let io = factory
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut recording = io
        .input
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let _recognition = factory
        .recognition_capture()
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let format = PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    let mut bridge = authenticated_bridge(&lease).await;
    send_capture(&mut bridge, lease.session_id(), 1, 1_000, format, b"one!").await;
    let _ = timeout(RECEIVE_TIMEOUT, recording.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    send_capture(&mut bridge, lease.session_id(), 2, 999, format, b"two!").await;
    assert_discontinuity(
        timeout(RECEIVE_TIMEOUT, recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        1_000,
    );
    cancellation.cancel();
    server.shutdown().await;
}

/// A bridge close is missing media, not invented silence: both independent consumers must see
/// the same discontinuity after the last accepted capture frame.
#[tokio::test]
async fn bridge_disconnect_emits_a_capture_discontinuity_to_recording_and_recognition() {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let lease = server.reserve(SessionId::new(71)).await.unwrap();
    let factory = Arc::new(NetworkVoiceIoFactory::new(server.hub(), lease.session_id()));
    let cancellation = CancellationToken::new();
    let io = factory
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut recording = io
        .input
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut recognition = factory
        .recognition_capture()
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let format = PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    let mut bridge = authenticated_bridge(&lease).await;
    send_capture(&mut bridge, lease.session_id(), 1, 1_000, format, b"one!").await;
    let _ = timeout(RECEIVE_TIMEOUT, recording.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let _ = timeout(RECEIVE_TIMEOUT, recognition.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(bridge);
    assert_discontinuity(
        timeout(RECEIVE_TIMEOUT, recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        1_000,
    );
    assert_capture_discontinuity(
        timeout(RECEIVE_TIMEOUT, recognition.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        1_000,
    );
    cancellation.cancel();
    server.shutdown().await;
}

/// A disconnected bridge may resume only while its configured lease window is live; it never
/// obtains a fresh sequence authority merely by reconnecting.
#[tokio::test]
async fn reconnect_after_the_configured_window_is_rejected() {
    let server = MediaServer::bind(
        MediaServerConfig::loopback()
            .with_lifetimes(Duration::from_secs(1), Duration::from_millis(20)),
    )
    .await
    .unwrap();
    let lease = server.reserve(SessionId::new(81)).await.unwrap();
    let factory = Arc::new(NetworkVoiceIoFactory::new(server.hub(), lease.session_id()));
    let cancellation = CancellationToken::new();
    let io = factory
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut recording = io
        .input
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let _recognition = factory
        .recognition_capture()
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let format = PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    let mut bridge = authenticated_bridge(&lease).await;
    send_capture(&mut bridge, lease.session_id(), 1, 1_000, format, b"one!").await;
    let _ = timeout(RECEIVE_TIMEOUT, recording.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(bridge);
    let _ = timeout(RECEIVE_TIMEOUT, recording.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut stale = authenticated_bridge(&lease).await;
    send_capture(&mut stale, lease.session_id(), 2, 1_001, format, b"two!").await;
    assert!(timeout(Duration::from_millis(50), recording.recv())
        .await
        .is_err());
    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn credentials_and_sequence_authority_stay_bound_to_their_one_session_across_reconnect() {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let first = server.reserve(SessionId::new(91)).await.unwrap();
    let second = server.reserve(SessionId::new(92)).await.unwrap();
    let cancellation = CancellationToken::new();
    let (first_factory, mut first_recording, _first_recognition) =
        attached_ports(&server, &first, cancellation.clone()).await;
    let (_second_factory, mut second_recording, _second_recognition) =
        attached_ports(&server, &second, cancellation.clone()).await;
    let format = PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();

    // A credential authenticates exactly its own session, never a header-selected neighbour.
    let mut wrong_session =
        authenticated_bridge_as(first.endpoint(), second.session_id(), first.credential()).await;
    send_capture(
        &mut wrong_session,
        second.session_id(),
        1,
        1_000,
        format,
        b"bad!",
    )
    .await;
    assert!(timeout(Duration::from_millis(50), first_recording.recv())
        .await
        .is_err());
    assert!(!matches!(
        timeout(Duration::from_millis(50), second_recording.recv()).await,
        Ok(Some(Ok(VoiceInputEvent::Capture(CaptureEvent::Frame(_)))))
    ));

    let mut bridge = authenticated_bridge(&first).await;
    send_capture(&mut bridge, first.session_id(), 5, 1_000, format, b"one!").await;
    assert_recording_capture_payload(
        timeout(RECEIVE_TIMEOUT, first_recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        b"one!",
        5,
    );
    // A second connection is rejected while the valid credential is active.
    let mut concurrent = authenticated_bridge(&first).await;
    send_capture(
        &mut concurrent,
        first.session_id(),
        6,
        1_001,
        format,
        b"two!",
    )
    .await;
    assert!(timeout(Duration::from_millis(50), first_recording.recv())
        .await
        .is_err());

    drop(bridge);
    assert_discontinuity(
        timeout(RECEIVE_TIMEOUT, first_recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        1_000,
    );
    let mut reconnected = authenticated_bridge(&first).await;
    send_capture(
        &mut reconnected,
        first.session_id(),
        6,
        1_001,
        format,
        b"two!",
    )
    .await;
    assert_recording_capture_payload(
        timeout(RECEIVE_TIMEOUT, first_recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        b"two!",
        6,
    );
    drop(reconnected);
    assert_discontinuity(
        timeout(RECEIVE_TIMEOUT, first_recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        1_001,
    );
    let mut reset = authenticated_bridge(&first).await;
    send_capture(&mut reset, first.session_id(), 1, 1_002, format, b"old!").await;
    assert_discontinuity(
        timeout(RECEIVE_TIMEOUT, first_recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        1_001,
    );

    server.revoke(second.session_id()).await;
    let mut revoked = authenticated_bridge(&second).await;
    send_capture(&mut revoked, second.session_id(), 1, 1_000, format, b"gone").await;
    assert!(!matches!(
        timeout(Duration::from_millis(50), second_recording.recv()).await,
        Ok(Some(Ok(VoiceInputEvent::Capture(CaptureEvent::Frame(_)))))
    ));
    cancellation.cancel();
    server.shutdown().await;
    drop(first_factory);
}

#[tokio::test]
async fn expired_credential_rejects_even_while_its_reconnect_window_is_open() {
    let server = MediaServer::bind(
        MediaServerConfig::loopback()
            .with_lifetimes(Duration::from_millis(20), Duration::from_secs(1)),
    )
    .await
    .unwrap();
    let lease = server.reserve(SessionId::new(101)).await.unwrap();
    let cancellation = CancellationToken::new();
    let (_factory, mut recording, _recognition) =
        attached_ports(&server, &lease, cancellation.clone()).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let mut stale = authenticated_bridge(&lease).await;
    let format = PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    send_capture(&mut stale, lease.session_id(), 1, 1_000, format, b"late").await;
    assert!(timeout(Duration::from_millis(50), recording.recv())
        .await
        .is_err());
    cancellation.cancel();
    server.shutdown().await;
}

#[tokio::test]
async fn malformed_authentication_and_frame_headers_fail_closed_without_delivery() {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let lease = server.reserve(SessionId::new(102)).await.unwrap();
    let cancellation = CancellationToken::new();
    let (_factory, mut recording, _recognition) =
        attached_ports(&server, &lease, cancellation.clone()).await;
    let mutations: &[&[u8]] = &[
        b"MED1\x02\x01", // unsupported auth version
        b"MED1\x01\x03", // unsupported auth kind
        b"BAD!\x01\x01", // malformed auth magic
    ];
    for prefix in mutations {
        let mut stream = TcpStream::connect(lease.endpoint()).await.unwrap();
        stream.write_all(prefix).await.unwrap();
        stream.flush().await.unwrap();
        drop(stream);
    }
    let mut frame = authenticated_bridge(&lease).await;
    // Valid authentication followed by a literal unsupported frame direction. The parser must
    // close before it can deliver the bytes to either capture consumer.
    frame.write_all(b"MED1\x01\x02\x09").await.unwrap();
    frame
        .write_all(&lease.session_id().get().to_be_bytes())
        .await
        .unwrap();
    frame.write_all(&1_u64.to_be_bytes()).await.unwrap();
    frame.write_all(&1_000_u64.to_be_bytes()).await.unwrap();
    frame.write_u8(1).await.unwrap();
    frame.write_all(&16_000_u32.to_be_bytes()).await.unwrap();
    frame.write_all(&1_u16.to_be_bytes()).await.unwrap();
    frame.write_all(&4_u32.to_be_bytes()).await.unwrap();
    frame.write_all(b"bad!").await.unwrap();
    frame.flush().await.unwrap();
    assert!(timeout(Duration::from_millis(50), recording.recv())
        .await
        .is_err());
    cancellation.cancel();
    server.shutdown().await;
}

/// Catches a parser that advances capture authority or leaks to either consumer after a malformed
/// authenticated frame. Every packet below is written as bridge bytes, not through a production
/// test API; a later valid reconnect proves the authoritative session remained usable.
#[tokio::test]
async fn authenticated_capture_validation_rejects_bad_versions_codecs_lengths_and_authority_edges()
{
    let server = MediaServer::bind(MediaServerConfig::loopback().with_max_payload_bytes(4))
        .await
        .unwrap();
    let lease = server.reserve(SessionId::new(103)).await.unwrap();
    let cancellation = CancellationToken::new();
    let (_factory, mut recording, mut recognition) =
        attached_ports(&server, &lease, cancellation.clone()).await;

    let mut packets = vec![
        b"MED1\x02\x02".to_vec(), // unsupported frame protocol version
        raw_capture_header(lease.session_id(), 1, 1_000, 9, 4), // invalid codec
        raw_capture_header(lease.session_id(), 1, 1_000, 1, 0), // zero length
        raw_capture_header(lease.session_id(), 1, 1_000, 1, 5), // configured maximum
    ];
    let mut wrong_session = raw_capture_header(SessionId::new(104), 1, 1_000, 1, 4);
    wrong_session.extend_from_slice(b"nope");
    packets.push(wrong_session); // header session mismatch
    for packet in packets {
        let mut bridge = authenticated_bridge(&lease).await;
        bridge.write_all(&packet).await.unwrap();
        bridge.flush().await.unwrap();
        let mut byte = [0_u8; 1];
        assert_eq!(
            timeout(RECEIVE_TIMEOUT, bridge.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(timeout(Duration::from_millis(30), recording.recv())
            .await
            .is_err());
        assert!(timeout(Duration::from_millis(30), recognition.recv())
            .await
            .is_err());
    }
    let mut truncated = authenticated_bridge(&lease).await;
    truncated.write_all(b"MED1\x01\x02\x01").await.unwrap();
    truncated.flush().await.unwrap();
    drop(truncated);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(timeout(Duration::from_millis(30), recording.recv())
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(30), recognition.recv())
        .await
        .is_err());

    let mut bridge = authenticated_bridge(&lease).await;
    send_capture(
        &mut bridge,
        lease.session_id(),
        1,
        1_000,
        PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        b"good",
    )
    .await;
    assert_recording_capture_payload(recording.recv().await.unwrap().unwrap(), b"good", 1);
    assert_capture_payload(recognition.recv().await.unwrap().unwrap(), b"good", 1);

    // A forward gap is explicit on the capture timeline, then the accepted frame reaches both
    // ports. `u64::MAX` must fail before it can reset sequence authority to None.
    send_capture(
        &mut bridge,
        lease.session_id(),
        3,
        1_300,
        PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        b"next",
    )
    .await;
    assert_discontinuity(recording.recv().await.unwrap().unwrap(), 1_300);
    assert_capture_discontinuity(recognition.recv().await.unwrap().unwrap(), 1_300);
    assert_recording_capture_payload(recording.recv().await.unwrap().unwrap(), b"next", 3);
    assert_capture_payload(recognition.recv().await.unwrap().unwrap(), b"next", 3);
    bridge
        .write_all(&raw_capture_header(
            lease.session_id(),
            u64::MAX,
            2_000,
            1,
            4,
        ))
        .await
        .unwrap();
    bridge.write_all(b"last").await.unwrap();
    bridge.flush().await.unwrap();
    let mut byte = [0_u8; 1];
    assert_eq!(
        timeout(RECEIVE_TIMEOUT, bridge.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert_discontinuity(recording.recv().await.unwrap().unwrap(), 1_300);
    assert_capture_discontinuity(recognition.recv().await.unwrap().unwrap(), 1_300);
    assert!(timeout(Duration::from_millis(30), recording.recv())
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(30), recognition.recv())
        .await
        .is_err());

    let mut reconnect = authenticated_bridge(&lease).await;
    send_capture(
        &mut reconnect,
        lease.session_id(),
        4,
        1_400,
        PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        b"kept",
    )
    .await;
    assert_recording_capture_payload(recording.recv().await.unwrap().unwrap(), b"kept", 4);
    assert_capture_payload(recognition.recv().await.unwrap().unwrap(), b"kept", 4);

    cancellation.cancel();
    server.shutdown().await;
}

/// Catches a peer holding an authenticated connection permit with a complete capture header and
/// incomplete payload. The timed-out packet must not publish, advance capture authority, or block
/// the next authorized bridge from starting at its original sequence.
#[tokio::test]
async fn authenticated_partial_capture_payload_times_out_without_advancing_authority() {
    let server = MediaServer::bind(
        MediaServerConfig::loopback()
            .with_lifetimes(Duration::from_secs(1), Duration::from_secs(1))
            .with_max_playback_queue_duration(Duration::from_secs(1))
            .with_packet_read_timeout(Duration::from_millis(50)),
    )
    .await
    .unwrap();
    let lease = server.reserve(SessionId::new(105)).await.unwrap();
    let cancellation = CancellationToken::new();
    let (_factory, mut recording, mut recognition) =
        attached_ports(&server, &lease, cancellation.clone()).await;

    let mut truncated = authenticated_bridge(&lease).await;
    let mut packet = raw_capture_header(lease.session_id(), 1, 1_000, 1, 4);
    packet.extend_from_slice(b"sh");
    truncated.write_all(&packet).await.unwrap();
    truncated.flush().await.unwrap();
    let mut byte = [0_u8; 1];
    let closed = timeout(Duration::from_millis(250), truncated.read(&mut byte)).await;
    drop(truncated);

    let mut reconnect = authenticated_bridge(&lease).await;
    send_capture(
        &mut reconnect,
        lease.session_id(),
        1,
        1_000,
        PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        b"kept",
    )
    .await;
    let recovered_recording = timeout(RECEIVE_TIMEOUT, recording.recv()).await;
    let recovered_recognition = timeout(RECEIVE_TIMEOUT, recognition.recv()).await;

    cancellation.cancel();
    server.shutdown().await;

    assert_eq!(closed.unwrap().unwrap(), 0);
    assert_recording_capture_payload(recovered_recording.unwrap().unwrap().unwrap(), b"kept", 1);
    assert_capture_payload(recovered_recognition.unwrap().unwrap().unwrap(), b"kept", 1);
}

/// An authenticated bridge may intentionally be capture-idle or playback-only. The packet
/// completion deadline begins with a packet byte, not when the connection becomes idle.
#[tokio::test]
async fn authenticated_idle_bridge_survives_packet_timeout_then_plays_and_acknowledges() {
    let server = MediaServer::bind(
        MediaServerConfig::loopback()
            .with_lifetimes(Duration::from_secs(1), Duration::from_secs(1))
            .with_max_playback_queue_duration(Duration::from_secs(1))
            .with_packet_read_timeout(Duration::from_millis(50)),
    )
    .await
    .unwrap();
    let lease = server.reserve(SessionId::new(106)).await.unwrap();
    let cancellation = CancellationToken::new();
    let (factory, mut recording, _recognition) =
        attached_ports(&server, &lease, cancellation.clone()).await;
    let output = Arc::clone(
        &factory
            .start(lease.session_id(), cancellation.child_token())
            .await
            .unwrap()
            .output,
    );
    let mut bridge = authenticated_bridge(&lease).await;

    // The legacy outbound queue timer currently closes this idle authenticated peer here.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let enqueue = tokio::spawn({
        let cancellation = cancellation.child_token();
        async move {
            output
                .enqueue(playback_test_frame(31, 31), cancellation)
                .await
        }
    });
    let sequence = timeout(
        Duration::from_millis(250),
        read_playback_frame(
            &mut bridge,
            lease.session_id(),
            TurnId::new(62),
            GenerationId::new(31),
            UtteranceId::new(52),
            31,
            b"accepted-only!",
        ),
    )
    .await;

    let sequence = sequence.expect("an idle authenticated bridge was closed before later playback");
    send_playback_ack(&mut bridge, lease.session_id(), sequence, 1).await;
    let accepted = timeout(RECEIVE_TIMEOUT, enqueue)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    send_playback_ack(&mut bridge, lease.session_id(), sequence, 2).await;
    let rendered = timeout(RECEIVE_TIMEOUT, recording.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    cancellation.cancel();
    server.shutdown().await;

    assert_eq!(accepted.state(), PlaybackState::Accepted);
    assert_playback_rendered(rendered, GenerationId::new(31));
}

/// Catches a bridge path that treats a successful TCP write as audible playback or echoes capture
/// ingress back as output. The output must remain pending until this exact authenticated socket
/// accepts the exact outbound sequence, and only its later rendered acknowledgement may enter the
/// owning voice-input stream.
#[tokio::test]
async fn playback_receipts_require_authenticated_accepted_then_rendered_acknowledgements() {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let lease = server.reserve(SessionId::new(111)).await.unwrap();
    let factory = Arc::new(NetworkVoiceIoFactory::new(server.hub(), lease.session_id()));
    let cancellation = CancellationToken::new();
    let io = factory
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut recording = io
        .input
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut recognition = factory
        .recognition_capture()
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut bridge = authenticated_bridge(&lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let capture_format = PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    send_capture(
        &mut bridge,
        lease.session_id(),
        1,
        1_000,
        capture_format,
        b"capture-only",
    )
    .await;
    assert_recording_capture_payload(
        timeout(RECEIVE_TIMEOUT, recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        b"capture-only",
        1,
    );
    assert_capture_payload(
        timeout(RECEIVE_TIMEOUT, recognition.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        b"capture-only",
        1,
    );
    assert!(timeout(Duration::from_millis(30), bridge.read_u8())
        .await
        .is_err());
    let output = Arc::clone(&io.output);
    let frame = AudioFrame::new(
        TurnId::new(13),
        GenerationId::new(23),
        UtteranceId::new(33),
        43,
        PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        b"playback!!".to_vec(),
    )
    .unwrap();
    let receipt_cancellation = cancellation.child_token();
    let mut receipt =
        tokio::spawn(async move { output.enqueue(frame, receipt_cancellation).await });

    let outbound_sequence = timeout(
        RECEIVE_TIMEOUT,
        read_playback_frame(
            &mut bridge,
            lease.session_id(),
            TurnId::new(13),
            GenerationId::new(23),
            UtteranceId::new(33),
            43,
            b"playback!!",
        ),
    )
    .await
    .expect("bridge did not receive playback frame");
    assert!(timeout(Duration::from_millis(50), &mut receipt)
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(30), recording.recv())
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(30), recognition.recv())
        .await
        .is_err());

    send_playback_ack(&mut bridge, lease.session_id(), outbound_sequence, 1).await;
    let accepted = timeout(RECEIVE_TIMEOUT, receipt)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(accepted.generation_id(), GenerationId::new(23));
    assert_eq!(accepted.state(), PlaybackState::Accepted);
    assert!(timeout(Duration::from_millis(30), recording.recv())
        .await
        .is_err());

    send_playback_ack(&mut bridge, lease.session_id(), outbound_sequence, 2).await;
    assert_playback_rendered(
        timeout(RECEIVE_TIMEOUT, recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        GenerationId::new(23),
    );
    send_playback_ack(&mut bridge, lease.session_id(), outbound_sequence, 2).await;
    assert!(timeout(Duration::from_millis(50), recording.recv())
        .await
        .is_err());

    cancellation.cancel();
    server.shutdown().await;
}

/// Catches an ACK parser that trusts a frame-selected session or sequence, or lets a render ACK
/// skip acceptance. Each packet is written to the real authenticated loopback socket; it must
/// neither complete the waiting enqueue nor publish a playback event.
#[tokio::test]
async fn wrong_unknown_and_render_before_accept_playback_acknowledgements_resolve_nothing() {
    assert_rejected_playback_ack(121, RejectedPlaybackAck::WrongSession).await;
    assert_rejected_playback_ack(122, RejectedPlaybackAck::UnknownSequence).await;
    assert_rejected_playback_ack(123, RejectedPlaybackAck::RenderedBeforeAccepted).await;
}

/// Catches a reconnect path that lets a replacement bridge revive bytes whose audible state is
/// unknown. A stale ACK stays ignored after the old pending enqueue has been truthfully flushed.
#[tokio::test]
async fn playback_acknowledgement_from_a_reconnected_bridge_resolves_nothing() {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let lease = server.reserve(SessionId::new(131)).await.unwrap();
    let factory = Arc::new(NetworkVoiceIoFactory::new(server.hub(), lease.session_id()));
    let cancellation = CancellationToken::new();
    let io = factory
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut recording = io
        .input
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let _recognition = factory
        .recognition_capture()
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut first_bridge = authenticated_bridge(&lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let output = Arc::clone(&io.output);
    let receipt_cancellation = cancellation.child_token();
    let receipt = tokio::spawn(async move {
        output
            .enqueue(
                AudioFrame::new(
                    TurnId::new(15),
                    GenerationId::new(25),
                    UtteranceId::new(35),
                    45,
                    PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
                    b"old-bridge".to_vec(),
                )
                .unwrap(),
                receipt_cancellation,
            )
            .await
    });
    let outbound_sequence = timeout(
        RECEIVE_TIMEOUT,
        read_playback_frame(
            &mut first_bridge,
            lease.session_id(),
            TurnId::new(15),
            GenerationId::new(25),
            UtteranceId::new(35),
            45,
            b"old-bridge",
        ),
    )
    .await
    .expect("first bridge did not receive playback frame");
    drop(first_bridge);
    assert_playback_flushed(
        timeout(RECEIVE_TIMEOUT, recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        GenerationId::new(25),
    );
    let disconnected = timeout(RECEIVE_TIMEOUT, receipt).await.unwrap().unwrap();
    assert!(disconnected.is_err());
    tokio::time::sleep(Duration::from_millis(10)).await;
    let mut reconnected_bridge = authenticated_bridge(&lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    send_playback_ack(
        &mut reconnected_bridge,
        lease.session_id(),
        outbound_sequence,
        1,
    )
    .await;
    assert!(timeout(Duration::from_millis(50), recording.recv())
        .await
        .is_err());

    cancellation.cancel();
    server.shutdown().await;
}

/// Catches a flush implementation that reports completion locally without the owning bridge
/// acknowledging the exact session and generation request.
#[tokio::test]
async fn flush_receipt_requires_an_authenticated_exact_bridge_acknowledgement() {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let lease = server.reserve(SessionId::new(141)).await.unwrap();
    let factory = Arc::new(NetworkVoiceIoFactory::new(server.hub(), lease.session_id()));
    let cancellation = CancellationToken::new();
    let io = factory
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let _recording = io
        .input
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let _recognition = factory
        .recognition_capture()
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut bridge = authenticated_bridge(&lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let output = Arc::clone(&io.output);
    let mut receipt = tokio::spawn(async move {
        output
            .flush(SessionId::new(141), GenerationId::new(51))
            .await
    });
    timeout(
        RECEIVE_TIMEOUT,
        read_flush_request(&mut bridge, lease.session_id(), GenerationId::new(51)),
    )
    .await
    .expect("bridge did not receive flush request");
    assert!(timeout(Duration::from_millis(50), &mut receipt)
        .await
        .is_err());
    send_flush_ack(&mut bridge, lease.session_id(), GenerationId::new(51)).await;
    let flushed = timeout(RECEIVE_TIMEOUT, receipt)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(flushed.generation_id(), GenerationId::new(51));
    assert_eq!(flushed.state(), PlaybackState::Flushed);

    cancellation.cancel();
    server.shutdown().await;
}

/// Catches a connected-but-stalled bridge permanently occupying playback or flush bookkeeping.
/// The same configured queue-time budget bounds both truthful ACK waits; late ACKs cannot revive
/// a timed-out operation or publish an event.
#[tokio::test]
async fn connected_playback_and_flush_acknowledgements_obey_the_configured_time_bound() {
    let server = MediaServer::bind(
        MediaServerConfig::loopback()
            .with_lifetimes(Duration::from_secs(1), Duration::from_millis(50)),
    )
    .await
    .unwrap();
    let lease = server.reserve(SessionId::new(145)).await.unwrap();
    let cancellation = CancellationToken::new();
    let (factory, mut recording, _recognition) =
        attached_ports(&server, &lease, cancellation.clone()).await;
    let io = factory
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let output = Arc::clone(&io.output);
    let flush_output = Arc::clone(&io.output);
    let mut bridge = authenticated_bridge(&lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let enqueue_cancellation = cancellation.child_token();
    let pending = tokio::spawn(async move {
        output
            .enqueue(
                AudioFrame::new(
                    TurnId::new(31),
                    GenerationId::new(71),
                    UtteranceId::new(51),
                    61,
                    PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
                    b"wait".to_vec(),
                )
                .unwrap(),
                enqueue_cancellation,
            )
            .await
    });
    let sequence = timeout(
        RECEIVE_TIMEOUT,
        read_playback_frame(
            &mut bridge,
            lease.session_id(),
            TurnId::new(31),
            GenerationId::new(71),
            UtteranceId::new(51),
            61,
            b"wait",
        ),
    )
    .await
    .expect("bridge did not receive bounded playback");
    for _ in 0..2 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        send_playback_ack(&mut bridge, lease.session_id(), sequence + 1, 1).await;
    }
    assert!(timeout(RECEIVE_TIMEOUT, pending)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    send_playback_ack(&mut bridge, lease.session_id(), sequence, 1).await;
    assert!(timeout(Duration::from_millis(30), recording.recv())
        .await
        .is_err());

    let pending_flush = tokio::spawn(async move {
        flush_output
            .flush(SessionId::new(145), GenerationId::new(72))
            .await
    });
    read_flush_request(&mut bridge, lease.session_id(), GenerationId::new(72)).await;
    for _ in 0..2 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        send_playback_ack(&mut bridge, lease.session_id(), sequence + 1, 1).await;
    }
    assert!(timeout(RECEIVE_TIMEOUT, pending_flush)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    send_flush_ack(&mut bridge, lease.session_id(), GenerationId::new(72)).await;

    cancellation.cancel();
    server.shutdown().await;
}

/// Catches Accepted-only playback permanently retaining every bounded pending slot. Each expired
/// generation must produce one truthful Flushed event, free its slot, and reject a late Rendered.
#[tokio::test]
async fn accepted_only_playback_expires_as_flushed_and_frees_capacity() {
    let server = MediaServer::bind(
        MediaServerConfig::loopback()
            .with_lifetimes(Duration::from_secs(1), Duration::from_millis(50)),
    )
    .await
    .unwrap();
    let lease = server.reserve(SessionId::new(146)).await.unwrap();
    let cancellation = CancellationToken::new();
    let (factory, mut recording, _recognition) =
        attached_ports(&server, &lease, cancellation.clone()).await;
    let io = factory
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let output = Arc::clone(&io.output);
    let mut bridge = authenticated_bridge(&lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let mut sequences = Vec::new();

    for source_sequence in 1..=16 {
        let enqueue = tokio::spawn({
            let output = Arc::clone(&output);
            let cancellation = cancellation.child_token();
            async move {
                output
                    .enqueue(playback_test_frame(1, source_sequence), cancellation)
                    .await
            }
        });
        let sequence = timeout(
            RECEIVE_TIMEOUT,
            read_playback_frame(
                &mut bridge,
                lease.session_id(),
                TurnId::new(62),
                GenerationId::new(1),
                UtteranceId::new(52),
                source_sequence,
                b"accepted-only!",
            ),
        )
        .await
        .expect("bridge did not receive accepted-only playback");
        send_playback_ack(&mut bridge, lease.session_id(), sequence, 1).await;
        let accepted = timeout(RECEIVE_TIMEOUT, enqueue)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(accepted.state(), PlaybackState::Accepted);
        sequences.push(sequence);
    }

    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        // A legal packet keeps the authenticated bridge alive while its Accepted entries expire;
        // the duplicate state itself remains rejected by the hub.
        send_playback_ack(&mut bridge, lease.session_id(), sequences[0], 1).await;
    }
    let recovered = tokio::spawn({
        let output = Arc::clone(&output);
        let cancellation = cancellation.child_token();
        async move {
            output
                .enqueue(playback_test_frame(17, 17), cancellation)
                .await
        }
    });
    let recovered_frame = timeout(
        Duration::from_millis(250),
        read_playback_frame(
            &mut bridge,
            lease.session_id(),
            TurnId::new(62),
            GenerationId::new(17),
            UtteranceId::new(52),
            17,
            b"accepted-only!",
        ),
    )
    .await;
    let expired = timeout(Duration::from_millis(30), recording.recv()).await;
    let duplicate_expiry = timeout(Duration::from_millis(30), recording.recv()).await;
    send_playback_ack(&mut bridge, lease.session_id(), sequences[0], 2).await;
    let late_rendered = timeout(Duration::from_millis(30), recording.recv()).await;

    cancellation.cancel();
    server.shutdown().await;

    assert!(
        recovered_frame.is_ok(),
        "expired playback did not free capacity"
    );
    assert_playback_flushed(expired.unwrap().unwrap().unwrap(), GenerationId::new(1));
    assert!(
        duplicate_expiry.is_err(),
        "expiry flushed a generation twice"
    );
    assert!(
        late_rendered.is_err(),
        "late Rendered published after expiry"
    );
    assert!(recovered.await.unwrap().is_err());
}

/// Rapid Accepted -> Rendered generations must leave no delayed expiry behind. This exercises
/// more than the pending-map capacity without exposing an implementation-only task counter.
#[tokio::test]
async fn rapid_rendered_playback_cycles_leave_no_delayed_flush_or_capacity_loss() {
    let server = MediaServer::bind(
        MediaServerConfig::loopback()
            .with_lifetimes(Duration::from_secs(1), Duration::from_secs(1))
            .with_max_playback_queue_duration(Duration::from_millis(50)),
    )
    .await
    .unwrap();
    let lease = server.reserve(SessionId::new(148)).await.unwrap();
    let cancellation = CancellationToken::new();
    let (factory, mut recording, _recognition) =
        attached_ports(&server, &lease, cancellation.clone()).await;
    let output = Arc::clone(
        &factory
            .start(lease.session_id(), cancellation.child_token())
            .await
            .unwrap()
            .output,
    );
    let mut bridge = authenticated_bridge(&lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    for generation in 1..=20 {
        let enqueue = tokio::spawn({
            let output = Arc::clone(&output);
            let cancellation = cancellation.child_token();
            async move {
                output
                    .enqueue(playback_test_frame(generation, generation), cancellation)
                    .await
            }
        });
        let sequence = read_playback_frame(
            &mut bridge,
            lease.session_id(),
            TurnId::new(62),
            GenerationId::new(generation),
            UtteranceId::new(52),
            generation,
            b"accepted-only!",
        )
        .await;
        send_playback_ack(&mut bridge, lease.session_id(), sequence, 1).await;
        assert_eq!(
            timeout(RECEIVE_TIMEOUT, enqueue)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .state(),
            PlaybackState::Accepted
        );
        send_playback_ack(&mut bridge, lease.session_id(), sequence, 2).await;
        assert_playback_rendered(
            timeout(RECEIVE_TIMEOUT, recording.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            GenerationId::new(generation),
        );
    }

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        timeout(Duration::from_millis(30), recording.recv())
            .await
            .is_err(),
        "a completed generation emitted a delayed expiry flush"
    );

    let enqueue = tokio::spawn({
        let cancellation = cancellation.child_token();
        async move {
            output
                .enqueue(playback_test_frame(21, 21), cancellation)
                .await
        }
    });
    let sequence = read_playback_frame(
        &mut bridge,
        lease.session_id(),
        TurnId::new(62),
        GenerationId::new(21),
        UtteranceId::new(52),
        21,
        b"accepted-only!",
    )
    .await;
    send_playback_ack(&mut bridge, lease.session_id(), sequence, 1).await;
    let accepted = timeout(RECEIVE_TIMEOUT, enqueue)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    send_playback_ack(&mut bridge, lease.session_id(), sequence, 2).await;
    let rendered = timeout(RECEIVE_TIMEOUT, recording.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    cancellation.cancel();
    server.shutdown().await;

    assert_eq!(accepted.state(), PlaybackState::Accepted);
    assert_playback_rendered(rendered, GenerationId::new(21));
}

/// Catches a retired reservation's expiry matching a newly reserved session after every visible
/// counter restarts. The old Accepted deadline must not flush the new reservation's sequence one.
#[tokio::test]
async fn revoked_reservation_expiry_cannot_flush_a_matching_rereservation() {
    let server = MediaServer::bind(
        MediaServerConfig::loopback()
            .with_lifetimes(Duration::from_secs(2), Duration::from_millis(200)),
    )
    .await
    .unwrap();
    let session_id = SessionId::new(147);
    let old_lease = server.reserve(session_id).await.unwrap();
    let old_cancellation = CancellationToken::new();
    let (old_factory, _old_recording, _old_recognition) =
        attached_ports(&server, &old_lease, old_cancellation.clone()).await;
    let old_output = Arc::clone(
        &old_factory
            .start(session_id, old_cancellation.child_token())
            .await
            .unwrap()
            .output,
    );
    let mut old_bridge = authenticated_bridge(&old_lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let old_enqueue = tokio::spawn({
        let cancellation = old_cancellation.child_token();
        async move {
            old_output
                .enqueue(playback_test_frame(9, 1), cancellation)
                .await
        }
    });
    let old_sequence = read_playback_frame(
        &mut old_bridge,
        session_id,
        TurnId::new(62),
        GenerationId::new(9),
        UtteranceId::new(52),
        1,
        b"accepted-only!",
    )
    .await;
    send_playback_ack(&mut old_bridge, session_id, old_sequence, 1).await;
    assert_eq!(
        timeout(RECEIVE_TIMEOUT, old_enqueue)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .state(),
        PlaybackState::Accepted
    );

    tokio::time::sleep(Duration::from_millis(130)).await;
    server.revoke(session_id).await;
    let new_lease = server.reserve(session_id).await.unwrap();
    let new_cancellation = CancellationToken::new();
    let (new_factory, mut new_recording, _new_recognition) =
        attached_ports(&server, &new_lease, new_cancellation.clone()).await;
    let new_output = Arc::clone(
        &new_factory
            .start(session_id, new_cancellation.child_token())
            .await
            .unwrap()
            .output,
    );
    let mut new_bridge = authenticated_bridge(&new_lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let new_enqueue = tokio::spawn({
        let cancellation = new_cancellation.child_token();
        async move {
            new_output
                .enqueue(playback_test_frame(9, 1), cancellation)
                .await
        }
    });
    let new_sequence = read_playback_frame(
        &mut new_bridge,
        session_id,
        TurnId::new(62),
        GenerationId::new(9),
        UtteranceId::new(52),
        1,
        b"accepted-only!",
    )
    .await;
    assert_eq!(new_sequence, 1);
    send_playback_ack(&mut new_bridge, session_id, new_sequence, 1).await;
    assert_eq!(
        timeout(RECEIVE_TIMEOUT, new_enqueue)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .state(),
        PlaybackState::Accepted
    );

    let old_deadline_event = timeout(Duration::from_millis(100), new_recording.recv()).await;
    send_playback_ack(&mut new_bridge, session_id, new_sequence, 2).await;
    let rendered = timeout(RECEIVE_TIMEOUT, new_recording.recv()).await;

    old_cancellation.cancel();
    new_cancellation.cancel();
    server.shutdown().await;

    assert!(
        old_deadline_event.is_err(),
        "an old reservation expiry affected the new reservation"
    );
    assert_playback_rendered(rendered.unwrap().unwrap().unwrap(), GenerationId::new(9));
}

/// Catches reconnect code that replays uncertain audio or leaves its caller blocked forever.
/// Bytes already written to a lost bridge are not known to be audible, so they fail typed and
/// produce one truthful Flushed input event instead of Accepted or Rendered.
#[tokio::test]
async fn disconnecting_before_playback_ack_flushes_once_without_replaying_written_audio() {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let lease = server.reserve(SessionId::new(151)).await.unwrap();
    let factory = Arc::new(NetworkVoiceIoFactory::new(server.hub(), lease.session_id()));
    let cancellation = CancellationToken::new();
    let io = factory
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut recording = io
        .input
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let _recognition = factory
        .recognition_capture()
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut bridge = authenticated_bridge(&lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let output = Arc::clone(&io.output);
    let receipt_cancellation = cancellation.child_token();
    let receipt = tokio::spawn(async move {
        output
            .enqueue(
                AudioFrame::new(
                    TurnId::new(16),
                    GenerationId::new(61),
                    UtteranceId::new(36),
                    46,
                    PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
                    b"uncertain!".to_vec(),
                )
                .unwrap(),
                receipt_cancellation,
            )
            .await
    });
    let _ = timeout(
        RECEIVE_TIMEOUT,
        read_playback_frame(
            &mut bridge,
            lease.session_id(),
            TurnId::new(16),
            GenerationId::new(61),
            UtteranceId::new(36),
            46,
            b"uncertain!",
        ),
    )
    .await
    .expect("bridge did not receive playback frame");
    drop(bridge);
    assert_playback_flushed(
        timeout(RECEIVE_TIMEOUT, recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        GenerationId::new(61),
    );
    let disconnected = timeout(RECEIVE_TIMEOUT, receipt).await.unwrap().unwrap();
    assert!(disconnected.is_err());

    let mut reconnected = authenticated_bridge(&lease).await;
    assert!(timeout(Duration::from_millis(50), reconnected.read_u8())
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(50), recording.recv())
        .await
        .is_err());

    cancellation.cancel();
    server.shutdown().await;
}

/// Catches a reconnect path that either drops new disconnected output immediately, replays old
/// audio, resets capture sequence authority, or leaks the recovered session into another lease.
#[tokio::test]
async fn disconnected_output_waits_only_for_reconnect_and_keeps_capture_and_session_authority() {
    let server = MediaServer::bind(
        MediaServerConfig::loopback()
            .with_lifetimes(Duration::from_secs(1), Duration::from_millis(200))
            .with_max_playback_queue_bytes(64),
    )
    .await
    .unwrap();
    let first = server.reserve(SessionId::new(161)).await.unwrap();
    let second = server.reserve(SessionId::new(162)).await.unwrap();
    let first_factory = Arc::new(NetworkVoiceIoFactory::new(server.hub(), first.session_id()));
    let second_factory = Arc::new(NetworkVoiceIoFactory::new(
        server.hub(),
        second.session_id(),
    ));
    let cancellation = CancellationToken::new();
    let first_io = first_factory
        .start(first.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut first_recording = first_io
        .input
        .start(first.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let _first_recognition = first_factory
        .recognition_capture()
        .start(first.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let second_io = second_factory
        .start(second.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut second_recording = second_io
        .input
        .start(second.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let _second_recognition = second_factory
        .recognition_capture()
        .start(second.session_id(), cancellation.child_token())
        .await
        .unwrap();

    let format = PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    let mut first_bridge = authenticated_bridge(&first).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    send_capture(
        &mut first_bridge,
        first.session_id(),
        7,
        1_000,
        format,
        b"capture-seven!",
    )
    .await;
    assert_recording_capture_payload(
        timeout(RECEIVE_TIMEOUT, first_recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        b"capture-seven!",
        7,
    );
    drop(first_bridge);
    assert_discontinuity(
        timeout(RECEIVE_TIMEOUT, first_recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        1_000,
    );

    let output = Arc::clone(&first_io.output);
    let receipt_cancellation = cancellation.child_token();
    let mut receipt = tokio::spawn(async move {
        output
            .enqueue(
                AudioFrame::new(
                    TurnId::new(17),
                    GenerationId::new(71),
                    UtteranceId::new(37),
                    47,
                    format,
                    b"reconnect!".to_vec(),
                )
                .unwrap(),
                receipt_cancellation,
            )
            .await
    });
    assert!(timeout(Duration::from_millis(50), &mut receipt)
        .await
        .is_err());

    let mut reconnected = authenticated_bridge(&first).await;
    let sequence = timeout(
        RECEIVE_TIMEOUT,
        read_playback_frame(
            &mut reconnected,
            first.session_id(),
            TurnId::new(17),
            GenerationId::new(71),
            UtteranceId::new(37),
            47,
            b"reconnect!",
        ),
    )
    .await
    .expect("replacement bridge did not receive queued playback");
    assert_eq!(sequence, 1);
    send_playback_ack(&mut reconnected, first.session_id(), sequence, 1).await;
    assert_eq!(
        timeout(RECEIVE_TIMEOUT, receipt)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .state(),
        PlaybackState::Accepted
    );
    send_capture(
        &mut reconnected,
        first.session_id(),
        8,
        1_001,
        format,
        b"capture-eight!",
    )
    .await;
    assert_recording_capture_payload(
        timeout(RECEIVE_TIMEOUT, first_recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        b"capture-eight!",
        8,
    );
    assert!(timeout(Duration::from_millis(50), second_recording.recv())
        .await
        .is_err());

    cancellation.cancel();
    server.shutdown().await;
}

/// Catches a disconnected-output queue that can retain bytes past its configured reconnect time
/// or byte budget. Neither case may grow storage or leave an enqueue pending indefinitely.
#[tokio::test]
async fn disconnected_playback_queue_expires_and_refuses_byte_overflow() {
    let expiry_server = MediaServer::bind(
        MediaServerConfig::loopback()
            .with_lifetimes(Duration::from_secs(1), Duration::from_millis(20))
            .with_max_playback_queue_bytes(64),
    )
    .await
    .unwrap();
    let expiry_lease = expiry_server.reserve(SessionId::new(171)).await.unwrap();
    let expiry_factory = Arc::new(NetworkVoiceIoFactory::new(
        expiry_server.hub(),
        expiry_lease.session_id(),
    ));
    let cancellation = CancellationToken::new();
    let expiry_io = expiry_factory
        .start(expiry_lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let _expiry_recording = expiry_io
        .input
        .start(expiry_lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let _expiry_recognition = expiry_factory
        .recognition_capture()
        .start(expiry_lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let expiry_bridge = authenticated_bridge(&expiry_lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    drop(expiry_bridge);
    tokio::time::sleep(Duration::from_millis(10)).await;
    let expiry_output = Arc::clone(&expiry_io.output);
    let expiry_cancellation = cancellation.child_token();
    let mut expired = tokio::spawn(async move {
        expiry_output
            .enqueue(
                AudioFrame::new(
                    TurnId::new(18),
                    GenerationId::new(81),
                    UtteranceId::new(38),
                    48,
                    PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
                    b"wait!!".to_vec(),
                )
                .unwrap(),
                expiry_cancellation,
            )
            .await
    });
    assert!(timeout(Duration::from_millis(5), &mut expired)
        .await
        .is_err());
    assert!(timeout(RECEIVE_TIMEOUT, expired)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    cancellation.cancel();
    expiry_server.shutdown().await;

    let overflow_server = MediaServer::bind(
        MediaServerConfig::loopback()
            .with_lifetimes(Duration::from_secs(1), Duration::from_millis(100))
            .with_max_playback_queue_bytes(4),
    )
    .await
    .unwrap();
    let overflow_lease = overflow_server.reserve(SessionId::new(172)).await.unwrap();
    let overflow_factory = Arc::new(NetworkVoiceIoFactory::new(
        overflow_server.hub(),
        overflow_lease.session_id(),
    ));
    let overflow_cancellation = CancellationToken::new();
    let overflow_io = overflow_factory
        .start(
            overflow_lease.session_id(),
            overflow_cancellation.child_token(),
        )
        .await
        .unwrap();
    let _overflow_recording = overflow_io
        .input
        .start(
            overflow_lease.session_id(),
            overflow_cancellation.child_token(),
        )
        .await
        .unwrap();
    let _overflow_recognition = overflow_factory
        .recognition_capture()
        .start(
            overflow_lease.session_id(),
            overflow_cancellation.child_token(),
        )
        .await
        .unwrap();
    let overflow_bridge = authenticated_bridge(&overflow_lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    drop(overflow_bridge);
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(overflow_io
        .output
        .enqueue(
            AudioFrame::new(
                TurnId::new(19),
                GenerationId::new(82),
                UtteranceId::new(39),
                49,
                PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
                b"overflow!!".to_vec(),
            )
            .unwrap(),
            overflow_cancellation.child_token(),
        )
        .await
        .is_err());
    overflow_cancellation.cancel();
    overflow_server.shutdown().await;
}

/// Catches flush ACK handling that trusts a supplied session/generation, accepts duplicates, or
/// lets a replacement connection complete a flush request made by the lost bridge.
#[tokio::test]
async fn wrong_unknown_duplicate_and_stale_flush_acknowledgements_resolve_nothing() {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let lease = server.reserve(SessionId::new(181)).await.unwrap();
    let factory = Arc::new(NetworkVoiceIoFactory::new(server.hub(), lease.session_id()));
    let cancellation = CancellationToken::new();
    let io = factory
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut recording = io
        .input
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let _recognition = factory
        .recognition_capture()
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut bridge = authenticated_bridge(&lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    send_flush_ack(&mut bridge, lease.session_id(), GenerationId::new(999)).await;
    let output = Arc::clone(&io.output);
    let mut first = tokio::spawn(async move {
        output
            .flush(SessionId::new(181), GenerationId::new(91))
            .await
    });
    read_flush_request(&mut bridge, lease.session_id(), GenerationId::new(91)).await;
    send_flush_ack(&mut bridge, SessionId::new(182), GenerationId::new(91)).await;
    assert!(timeout(Duration::from_millis(50), &mut first)
        .await
        .is_err());
    send_flush_ack(&mut bridge, lease.session_id(), GenerationId::new(91)).await;
    assert_eq!(
        timeout(RECEIVE_TIMEOUT, first)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .state(),
        PlaybackState::Flushed
    );
    send_flush_ack(&mut bridge, lease.session_id(), GenerationId::new(91)).await;
    assert!(timeout(Duration::from_millis(50), recording.recv())
        .await
        .is_err());

    let stale_output = Arc::clone(&io.output);
    let stale = tokio::spawn(async move {
        stale_output
            .flush(SessionId::new(181), GenerationId::new(92))
            .await
    });
    read_flush_request(&mut bridge, lease.session_id(), GenerationId::new(92)).await;
    drop(bridge);
    assert!(timeout(RECEIVE_TIMEOUT, stale)
        .await
        .unwrap()
        .unwrap()
        .is_err());
    let mut reconnected = authenticated_bridge(&lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    send_flush_ack(&mut reconnected, lease.session_id(), GenerationId::new(92)).await;
    assert!(timeout(Duration::from_millis(50), recording.recv())
        .await
        .is_err());

    cancellation.cancel();
    server.shutdown().await;
}

/// Catches a media registry, outbound queue, pending ACK map, or connection lookup accidentally
/// keyed globally instead of by authoritative session. Both bridges are real loopback sockets and
/// their capture, playback, ACK, invalid-traffic, and disconnect operations are interleaved.
#[tokio::test]
async fn simultaneous_bridges_keep_capture_playback_and_acknowledgements_session_isolated() {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let first = server.reserve(SessionId::new(191)).await.unwrap();
    let second = server.reserve(SessionId::new(192)).await.unwrap();
    let first_factory = Arc::new(NetworkVoiceIoFactory::new(server.hub(), first.session_id()));
    let second_factory = Arc::new(NetworkVoiceIoFactory::new(
        server.hub(),
        second.session_id(),
    ));
    let cancellation = CancellationToken::new();
    let first_io = first_factory
        .start(first.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut first_recording = first_io
        .input
        .start(first.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut first_recognition = first_factory
        .recognition_capture()
        .start(first.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let second_io = second_factory
        .start(second.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut second_recording = second_io
        .input
        .start(second.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut second_recognition = second_factory
        .recognition_capture()
        .start(second.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let (mut first_bridge, mut second_bridge) =
        tokio::join!(authenticated_bridge(&first), authenticated_bridge(&second));
    tokio::time::sleep(Duration::from_millis(10)).await;

    let format = PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
    let ((), ()) = tokio::join!(
        send_capture(
            &mut first_bridge,
            first.session_id(),
            31,
            1_000,
            format,
            b"capture-alpha!",
        ),
        send_capture(
            &mut second_bridge,
            second.session_id(),
            41,
            2_000,
            format,
            b"capture-beta!!",
        ),
    );
    assert_recording_capture_payload_with_checksum(
        timeout(RECEIVE_TIMEOUT, first_recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        b"capture-alpha!",
        31,
        1_352,
    );
    assert_capture_payload_with_checksum(
        timeout(RECEIVE_TIMEOUT, first_recognition.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        b"capture-alpha!",
        31,
        1_352,
    );
    assert_recording_capture_payload_with_checksum(
        timeout(RECEIVE_TIMEOUT, second_recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        b"capture-beta!!",
        41,
        1_279,
    );
    assert_capture_payload_with_checksum(
        timeout(RECEIVE_TIMEOUT, second_recognition.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        b"capture-beta!!",
        41,
        1_279,
    );

    let first_output = Arc::clone(&first_io.output);
    let second_output = Arc::clone(&second_io.output);
    let first_cancellation = cancellation.child_token();
    let second_cancellation = cancellation.child_token();
    let first_receipt = tokio::spawn(async move {
        first_output
            .enqueue(
                AudioFrame::new(
                    TurnId::new(21),
                    GenerationId::new(101),
                    UtteranceId::new(41),
                    51,
                    format,
                    b"play-alpha!!".to_vec(),
                )
                .unwrap(),
                first_cancellation,
            )
            .await
    });
    let second_receipt = tokio::spawn(async move {
        second_output
            .enqueue(
                AudioFrame::new(
                    TurnId::new(22),
                    GenerationId::new(102),
                    UtteranceId::new(42),
                    52,
                    format,
                    b"play-beta!!!".to_vec(),
                )
                .unwrap(),
                second_cancellation,
            )
            .await
    });
    let (first_sequence, second_sequence) = tokio::join!(
        read_playback_frame_with_checksum(
            &mut first_bridge,
            PlaybackFrameExpectation {
                session_id: first.session_id(),
                turn_id: TurnId::new(21),
                generation_id: GenerationId::new(101),
                utterance_id: UtteranceId::new(41),
                source_sequence: 51,
                payload: b"play-alpha!!",
                expected_checksum: Some(1_067),
            },
        ),
        read_playback_frame_with_checksum(
            &mut second_bridge,
            PlaybackFrameExpectation {
                session_id: second.session_id(),
                turn_id: TurnId::new(22),
                generation_id: GenerationId::new(102),
                utterance_id: UtteranceId::new(42),
                source_sequence: 52,
                payload: b"play-beta!!!",
                expected_checksum: Some(994),
            },
        ),
    );
    assert_eq!(first_sequence, 1);
    assert_eq!(second_sequence, 1);
    let ((), ()) = tokio::join!(
        send_playback_ack(&mut second_bridge, second.session_id(), second_sequence, 1),
        send_playback_ack(&mut first_bridge, first.session_id(), first_sequence, 1),
    );
    assert_eq!(
        timeout(RECEIVE_TIMEOUT, first_receipt)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .generation_id(),
        GenerationId::new(101)
    );
    assert_eq!(
        timeout(RECEIVE_TIMEOUT, second_receipt)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .generation_id(),
        GenerationId::new(102)
    );
    let ((), ()) = tokio::join!(
        send_playback_ack(&mut first_bridge, first.session_id(), first_sequence, 2),
        send_playback_ack(&mut second_bridge, second.session_id(), second_sequence, 2),
    );
    assert_playback_rendered(
        timeout(RECEIVE_TIMEOUT, first_recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        GenerationId::new(101),
    );
    assert_playback_rendered(
        timeout(RECEIVE_TIMEOUT, second_recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        GenerationId::new(102),
    );

    let isolated_output = Arc::clone(&second_io.output);
    let isolated_cancellation = cancellation.child_token();
    let mut isolated_receipt = tokio::spawn(async move {
        isolated_output
            .enqueue(
                AudioFrame::new(
                    TurnId::new(23),
                    GenerationId::new(103),
                    UtteranceId::new(43),
                    53,
                    format,
                    b"second-only!".to_vec(),
                )
                .unwrap(),
                isolated_cancellation,
            )
            .await
    });
    let isolated_sequence = read_playback_frame_with_checksum(
        &mut second_bridge,
        PlaybackFrameExpectation {
            session_id: second.session_id(),
            turn_id: TurnId::new(23),
            generation_id: GenerationId::new(103),
            utterance_id: UtteranceId::new(43),
            source_sequence: 53,
            payload: b"second-only!",
            expected_checksum: Some(1_164),
        },
    )
    .await;
    send_playback_ack(&mut first_bridge, first.session_id(), 999, 1).await;
    drop(first_bridge);
    assert_discontinuity(
        timeout(RECEIVE_TIMEOUT, first_recording.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        1_000,
    );
    assert!(timeout(Duration::from_millis(50), &mut isolated_receipt)
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(50), second_recording.recv())
        .await
        .is_err());
    send_playback_ack(
        &mut second_bridge,
        second.session_id(),
        isolated_sequence,
        1,
    )
    .await;
    assert_eq!(
        timeout(RECEIVE_TIMEOUT, isolated_receipt)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .generation_id(),
        GenerationId::new(103)
    );
    assert!(timeout(Duration::from_millis(50), first_recording.recv())
        .await
        .is_err());

    cancellation.cancel();
    server.shutdown().await;
}

fn assert_capture_payload(event: CaptureEvent, expected: &[u8], sequence: u64) {
    assert_capture_payload_inner(event, expected, sequence, None);
}

fn assert_capture_payload_with_checksum(
    event: CaptureEvent,
    expected: &[u8],
    sequence: u64,
    expected_checksum: u32,
) {
    assert_capture_payload_inner(event, expected, sequence, Some(expected_checksum));
}

fn assert_capture_payload_inner(
    event: CaptureEvent,
    expected: &[u8],
    sequence: u64,
    expected_checksum: Option<u32>,
) {
    match event {
        CaptureEvent::Frame(frame) => {
            assert_eq!(frame.bytes(), expected);
            if let Some(expected_checksum) = expected_checksum {
                assert_eq!(test_checksum(frame.bytes()), expected_checksum);
            }
            assert_eq!(frame.sequence(), sequence);
            assert_eq!(frame.turn_id(), TurnId::new(1));
            assert_eq!(frame.generation_id(), GenerationId::new(1));
            assert_eq!(frame.utterance_id(), UtteranceId::new(1));
        }
        CaptureEvent::Discontinuity { at_ms } => {
            panic!("expected a capture frame, got gap at {at_ms}")
        }
        _ => panic!("unexpected capture event"),
    }
}

fn test_checksum(bytes: &[u8]) -> u32 {
    bytes.iter().map(|byte| u32::from(*byte)).sum()
}

fn assert_recording_capture_payload(event: VoiceInputEvent, expected: &[u8], sequence: u64) {
    match event {
        VoiceInputEvent::Capture(event) => assert_capture_payload(event, expected, sequence),
        _ => panic!("expected a runtime recording capture event"),
    }
}

fn assert_recording_capture_payload_with_checksum(
    event: VoiceInputEvent,
    expected: &[u8],
    sequence: u64,
    expected_checksum: u32,
) {
    match event {
        VoiceInputEvent::Capture(event) => {
            assert_capture_payload_with_checksum(event, expected, sequence, expected_checksum)
        }
        _ => panic!("expected a runtime recording capture event"),
    }
}

fn assert_discontinuity(event: VoiceInputEvent, at_ms: u64) {
    match event {
        VoiceInputEvent::Capture(CaptureEvent::Discontinuity { at_ms: actual }) => {
            assert_eq!(actual, at_ms)
        }
        _ => panic!("expected a runtime recording discontinuity"),
    }
}

fn assert_capture_discontinuity(event: CaptureEvent, at_ms: u64) {
    match event {
        CaptureEvent::Discontinuity { at_ms: actual } => assert_eq!(actual, at_ms),
        _ => panic!("expected a recognition discontinuity"),
    }
}

fn assert_playback_rendered(event: VoiceInputEvent, generation_id: GenerationId) {
    match event {
        VoiceInputEvent::Playback(receipt) => {
            assert_eq!(receipt.generation_id(), generation_id);
            assert_eq!(receipt.state(), PlaybackState::Rendered);
        }
        _ => panic!("expected a rendered playback receipt"),
    }
}

fn assert_playback_flushed(event: VoiceInputEvent, generation_id: GenerationId) {
    match event {
        VoiceInputEvent::Playback(receipt) => {
            assert_eq!(receipt.generation_id(), generation_id);
            assert_eq!(receipt.state(), PlaybackState::Flushed);
        }
        _ => panic!("expected a flushed playback receipt"),
    }
}

enum RejectedPlaybackAck {
    WrongSession,
    UnknownSequence,
    RenderedBeforeAccepted,
}

async fn assert_rejected_playback_ack(session: u64, rejection: RejectedPlaybackAck) {
    let server = MediaServer::bind(MediaServerConfig::loopback())
        .await
        .unwrap();
    let lease = server.reserve(SessionId::new(session)).await.unwrap();
    let factory = Arc::new(NetworkVoiceIoFactory::new(server.hub(), lease.session_id()));
    let cancellation = CancellationToken::new();
    let io = factory
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut recording = io
        .input
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let _recognition = factory
        .recognition_capture()
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let mut bridge = authenticated_bridge(&lease).await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let output = Arc::clone(&io.output);
    let receipt_cancellation = cancellation.child_token();
    let mut receipt = tokio::spawn(async move {
        output
            .enqueue(
                AudioFrame::new(
                    TurnId::new(14),
                    GenerationId::new(24),
                    UtteranceId::new(34),
                    44,
                    PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
                    b"reject-me!".to_vec(),
                )
                .unwrap(),
                receipt_cancellation,
            )
            .await
    });
    let outbound_sequence = timeout(
        RECEIVE_TIMEOUT,
        read_playback_frame(
            &mut bridge,
            lease.session_id(),
            TurnId::new(14),
            GenerationId::new(24),
            UtteranceId::new(34),
            44,
            b"reject-me!",
        ),
    )
    .await
    .expect("bridge did not receive playback frame");
    let (acknowledgement_session, acknowledgement_sequence, acknowledgement_state) = match rejection
    {
        RejectedPlaybackAck::WrongSession => (SessionId::new(session + 1), outbound_sequence, 1),
        RejectedPlaybackAck::UnknownSequence => (lease.session_id(), outbound_sequence + 1, 1),
        RejectedPlaybackAck::RenderedBeforeAccepted => (lease.session_id(), outbound_sequence, 2),
    };
    send_playback_ack(
        &mut bridge,
        acknowledgement_session,
        acknowledgement_sequence,
        acknowledgement_state,
    )
    .await;
    assert!(timeout(Duration::from_millis(50), &mut receipt)
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(50), recording.recv())
        .await
        .is_err());

    cancellation.cancel();
    let cancelled = timeout(RECEIVE_TIMEOUT, receipt).await.unwrap().unwrap();
    assert!(cancelled.is_err());
    server.shutdown().await;
}

async fn authenticated_bridge(lease: &conversation_runtime_gateway::MediaLease) -> TcpStream {
    authenticated_bridge_as(lease.endpoint(), lease.session_id(), lease.credential()).await
}

async fn authenticated_bridge_as(
    endpoint: std::net::SocketAddr,
    session_id: SessionId,
    credential: &str,
) -> TcpStream {
    let mut stream = TcpStream::connect(endpoint).await.unwrap();
    stream.write_all(b"MED1\x01\x01").await.unwrap();
    stream
        .write_all(&session_id.get().to_be_bytes())
        .await
        .unwrap();
    stream.write_u8(credential.len() as u8).await.unwrap();
    stream.write_all(credential.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    stream
}

async fn attached_ports(
    server: &MediaServer,
    lease: &conversation_runtime_gateway::MediaLease,
    cancellation: CancellationToken,
) -> (
    Arc<NetworkVoiceIoFactory>,
    mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
    mpsc::Receiver<Result<CaptureEvent, AdapterError>>,
) {
    let factory = Arc::new(NetworkVoiceIoFactory::new(server.hub(), lease.session_id()));
    let io = factory
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let recording = io
        .input
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    let recognition = factory
        .recognition_capture()
        .start(lease.session_id(), cancellation.child_token())
        .await
        .unwrap();
    (factory, recording, recognition)
}

fn playback_test_frame(generation: u64, source_sequence: u64) -> AudioFrame {
    AudioFrame::new(
        TurnId::new(62),
        GenerationId::new(generation),
        UtteranceId::new(52),
        source_sequence,
        PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        b"accepted-only!".to_vec(),
    )
    .unwrap()
}

async fn send_capture(
    stream: &mut TcpStream,
    session_id: SessionId,
    sequence: u64,
    captured_at_ms: u64,
    format: PcmFormat,
    payload: &[u8],
) {
    stream.write_all(b"MED1\x01\x02\x01").await.unwrap();
    stream
        .write_all(&session_id.get().to_be_bytes())
        .await
        .unwrap();
    stream.write_all(&sequence.to_be_bytes()).await.unwrap();
    stream
        .write_all(&captured_at_ms.to_be_bytes())
        .await
        .unwrap();
    stream
        .write_u8(match format.sample_format() {
            PcmSampleFormat::Signed16LittleEndian => 1,
            PcmSampleFormat::Float32LittleEndian => 2,
            _ => unreachable!(),
        })
        .await
        .unwrap();
    stream
        .write_all(&format.sample_rate_hz().to_be_bytes())
        .await
        .unwrap();
    stream
        .write_all(&format.channels().to_be_bytes())
        .await
        .unwrap();
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .unwrap();
    stream.write_all(payload).await.unwrap();
    stream.flush().await.unwrap();
}

fn raw_capture_header(
    session_id: SessionId,
    sequence: u64,
    captured_at_ms: u64,
    sample_format: u8,
    payload_length: u32,
) -> Vec<u8> {
    let mut bytes = b"MED1\x01\x02\x01".to_vec();
    bytes.extend_from_slice(&session_id.get().to_be_bytes());
    bytes.extend_from_slice(&sequence.to_be_bytes());
    bytes.extend_from_slice(&captured_at_ms.to_be_bytes());
    bytes.push(sample_format);
    bytes.extend_from_slice(&16_000_u32.to_be_bytes());
    bytes.extend_from_slice(&1_u16.to_be_bytes());
    bytes.extend_from_slice(&payload_length.to_be_bytes());
    bytes
}

async fn read_playback_frame(
    stream: &mut TcpStream,
    session_id: SessionId,
    turn_id: TurnId,
    generation_id: GenerationId,
    utterance_id: UtteranceId,
    source_sequence: u64,
    payload: &[u8],
) -> u64 {
    read_playback_frame_inner(
        stream,
        PlaybackFrameExpectation {
            session_id,
            turn_id,
            generation_id,
            utterance_id,
            source_sequence,
            payload,
            expected_checksum: None,
        },
    )
    .await
}

struct PlaybackFrameExpectation<'a> {
    session_id: SessionId,
    turn_id: TurnId,
    generation_id: GenerationId,
    utterance_id: UtteranceId,
    source_sequence: u64,
    payload: &'a [u8],
    expected_checksum: Option<u32>,
}

async fn read_playback_frame_with_checksum(
    stream: &mut TcpStream,
    expected: PlaybackFrameExpectation<'_>,
) -> u64 {
    read_playback_frame_inner(stream, expected).await
}

async fn read_playback_frame_inner(
    stream: &mut TcpStream,
    expected: PlaybackFrameExpectation<'_>,
) -> u64 {
    let PlaybackFrameExpectation {
        session_id,
        turn_id,
        generation_id,
        utterance_id,
        source_sequence,
        payload,
        expected_checksum,
    } = expected;
    let mut header = [0_u8; 66];
    stream.read_exact(&mut header).await.unwrap();
    assert_eq!(&header[..7], b"MED1\x01\x02\x02");
    assert_eq!(
        u64::from_be_bytes(header[7..15].try_into().unwrap()),
        session_id.get()
    );
    let outbound_sequence = u64::from_be_bytes(header[15..23].try_into().unwrap());
    assert_eq!(
        u64::from_be_bytes(header[23..31].try_into().unwrap()),
        turn_id.get()
    );
    assert_eq!(
        u64::from_be_bytes(header[31..39].try_into().unwrap()),
        generation_id.get()
    );
    assert_eq!(
        u64::from_be_bytes(header[39..47].try_into().unwrap()),
        utterance_id.get()
    );
    assert_eq!(
        u64::from_be_bytes(header[47..55].try_into().unwrap()),
        source_sequence
    );
    assert_eq!(header[55], 1);
    assert_eq!(
        u32::from_be_bytes(header[56..60].try_into().unwrap()),
        16_000
    );
    assert_eq!(u16::from_be_bytes(header[60..62].try_into().unwrap()), 1);
    assert_eq!(
        u32::from_be_bytes(header[62..66].try_into().unwrap()) as usize,
        payload.len()
    );
    let mut actual_payload = vec![0_u8; payload.len()];
    stream.read_exact(&mut actual_payload).await.unwrap();
    assert_eq!(actual_payload, payload);
    if let Some(expected_checksum) = expected_checksum {
        assert_eq!(test_checksum(&actual_payload), expected_checksum);
    }
    outbound_sequence
}

async fn send_playback_ack(
    stream: &mut TcpStream,
    session_id: SessionId,
    outbound_sequence: u64,
    state: u8,
) {
    stream.write_all(b"MED1\x01\x03").await.unwrap();
    stream
        .write_all(&session_id.get().to_be_bytes())
        .await
        .unwrap();
    stream
        .write_all(&outbound_sequence.to_be_bytes())
        .await
        .unwrap();
    stream.write_u8(state).await.unwrap();
    stream.flush().await.unwrap();
}

async fn read_flush_request(
    stream: &mut TcpStream,
    session_id: SessionId,
    generation_id: GenerationId,
) {
    let mut bytes = [0_u8; 22];
    stream.read_exact(&mut bytes).await.unwrap();
    assert_eq!(&bytes[..6], b"MED1\x01\x04");
    assert_eq!(
        u64::from_be_bytes(bytes[6..14].try_into().unwrap()),
        session_id.get()
    );
    assert_eq!(
        u64::from_be_bytes(bytes[14..22].try_into().unwrap()),
        generation_id.get()
    );
}

async fn send_flush_ack(
    stream: &mut TcpStream,
    session_id: SessionId,
    generation_id: GenerationId,
) {
    stream.write_all(b"MED1\x01\x05").await.unwrap();
    stream
        .write_all(&session_id.get().to_be_bytes())
        .await
        .unwrap();
    stream
        .write_all(&generation_id.get().to_be_bytes())
        .await
        .unwrap();
    stream.flush().await.unwrap();
}
