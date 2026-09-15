//! Reservation reuse must not let an old connection touch replacement sinks.
use conversation_model_adapters::{
    frame_for_network_capture, CaptureEvent, NetworkMediaHub, NetworkVoiceIoFactory, PcmFormat,
    PcmSampleFormat, VoiceInputEvent, VoiceIoFactory, VoiceIoSession,
};
use conversation_protocol::SessionId;
use std::{
    future::Future,
    task::Poll,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const SOURCE: SessionId = SessionId::new(41);

async fn ports(
    hub: NetworkMediaHub,
    cancellation: &CancellationToken,
) -> (
    VoiceIoSession,
    mpsc::Receiver<Result<VoiceInputEvent, conversation_model_adapters::AdapterError>>,
    mpsc::Receiver<Result<CaptureEvent, conversation_model_adapters::AdapterError>>,
) {
    let factory = NetworkVoiceIoFactory::new(hub, SOURCE);
    let io = factory.start(SOURCE, cancellation.clone()).await.unwrap();
    let recording = io.input.start(SOURCE, cancellation.clone()).await.unwrap();
    let recognition = factory
        .recognition_capture()
        .start(SOURCE, cancellation.clone())
        .await
        .unwrap();
    (io, recording, recognition)
}

#[tokio::test]
async fn stale_disconnect_cannot_detach_replacement_with_reused_connection_number() {
    let hub = NetworkMediaHub::default();
    hub.reserve(SOURCE, 10).await.unwrap();
    let old_transport = hub.attach_playback_transport(SOURCE, 1).await.unwrap();
    hub.revoke(SOURCE).await;
    drop(old_transport);
    hub.reserve(SOURCE, 11).await.unwrap();
    let cancellation = CancellationToken::new();
    let (io, _recording, _recognition) = ports(hub.clone(), &cancellation).await;
    let mut replacement = hub.attach_playback_transport(SOURCE, 1).await.unwrap();
    hub.disconnect_playback_transport_for_reservation(
        SOURCE,
        10,
        1,
        Instant::now() + Duration::from_secs(1),
    )
    .await
    .unwrap();
    let frame = frame_for_network_capture(
        1,
        PcmFormat::new(24000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        vec![0, 0],
    )
    .unwrap();
    let mut pending = Box::pin(hub.enqueue_playback(SOURCE, frame, cancellation.clone()));
    std::future::poll_fn(|cx| {
        assert!(pending.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let delivered = replacement.try_recv();
    cancellation.cancel();
    let _ = pending.await;
    io.completion.await.unwrap().unwrap();
    hub.shutdown().await;
    assert!(
        delivered.is_ok(),
        "old cleanup detached new playback: {delivered:?}"
    );
}

#[tokio::test]
async fn stale_disconnect_notifications_cannot_select_replacement_recording_or_recognizer() {
    let hub = NetworkMediaHub::default();
    hub.reserve(SOURCE, 10).await.unwrap();
    hub.revoke(SOURCE).await;
    hub.reserve(SOURCE, 11).await.unwrap();
    let cancellation = CancellationToken::new();
    let (io, mut recording, mut recognition) = ports(hub.clone(), &cancellation).await;
    // This is the old owner's cleanup after gateway disconnect/re-reserve.
    hub.deliver_disconnect_for_reservation(SOURCE, 10, 0, Some(3))
        .await
        .unwrap();
    let recorded = recording.try_recv();
    let recognized = recognition.try_recv();
    cancellation.cancel();
    io.completion.await.unwrap().unwrap();
    hub.shutdown().await;
    assert!(
        recorded.is_err(),
        "old notification selected replacement recorder: {recorded:?}"
    );
    assert!(
        recognized.is_err(),
        "old notification selected replacement recognizer: {recognized:?}"
    );
}
