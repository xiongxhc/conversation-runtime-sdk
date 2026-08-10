use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use conversation_protocol::{decode_client_command, MAX_CLIENT_FRAME_BYTES};
use conversation_runtime_gateway::{FrameError, FrameReader, FrameWriter};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;

#[tokio::test]
async fn reads_fragmented_headers_and_payloads() {
    let (reader, mut peer) = tokio::io::duplex(32);
    let writer = tokio::spawn(async move {
        for chunk in [&[0, 0][..], &[0, 3], b"a", b"bc"] {
            peer.write_all(chunk).await.unwrap();
        }
    });
    let mut reader = FrameReader::new(reader);

    assert_eq!(reader.read_frame().await.unwrap(), Some(b"abc".to_vec()));
    writer.await.unwrap();
}

#[tokio::test]
async fn reads_coalesced_frames() {
    let (reader, mut peer) = tokio::io::duplex(32);
    peer.write_all(&[0, 0, 0, 1, b'a', 0, 0, 0, 2, b'b', b'c'])
        .await
        .unwrap();
    let mut reader = FrameReader::new(reader);

    assert_eq!(reader.read_frame().await.unwrap(), Some(b"a".to_vec()));
    assert_eq!(reader.read_frame().await.unwrap(), Some(b"bc".to_vec()));
}

#[tokio::test]
async fn rejects_a_zero_length_frame() {
    let (reader, mut peer) = tokio::io::duplex(8);
    peer.write_all(&0_u32.to_be_bytes()).await.unwrap();
    let mut reader = FrameReader::new(reader);

    assert!(matches!(
        reader.read_frame().await,
        Err(FrameError::InvalidLength(0))
    ));
}

#[tokio::test]
async fn accepts_a_frame_at_the_exact_512_kib_limit() {
    let payload = vec![b'x'; MAX_CLIENT_FRAME_BYTES];
    let (reader, mut peer) = tokio::io::duplex(MAX_CLIENT_FRAME_BYTES + 4);
    let payload_for_writer = payload.clone();
    let writer = tokio::spawn(async move {
        peer.write_all(&(payload_for_writer.len() as u32).to_be_bytes())
            .await
            .unwrap();
        peer.write_all(&payload_for_writer).await.unwrap();
    });
    let mut reader = FrameReader::new(reader);

    assert_eq!(reader.read_frame().await.unwrap(), Some(payload));
    writer.await.unwrap();
}

#[tokio::test]
async fn rejects_an_oversized_header_before_reading_a_payload() {
    let (reader, mut peer) = tokio::io::duplex(8);
    peer.write_all(&((MAX_CLIENT_FRAME_BYTES + 1) as u32).to_be_bytes())
        .await
        .unwrap();
    let mut reader = FrameReader::new(reader);

    assert!(matches!(
        reader.read_frame().await,
        Err(FrameError::InvalidLength(length)) if length == MAX_CLIENT_FRAME_BYTES + 1
    ));
}

#[tokio::test]
async fn distinguishes_clean_eof_from_a_truncated_frame() {
    let (reader, peer) = tokio::io::duplex(16);
    drop(peer);
    let mut reader = FrameReader::new(reader);
    assert_eq!(reader.read_frame().await.unwrap(), None);

    let (reader, mut peer) = tokio::io::duplex(16);
    peer.write_all(&[0, 0]).await.unwrap();
    drop(peer);
    let mut reader = FrameReader::new(reader);
    assert!(matches!(
        reader.read_frame().await,
        Err(FrameError::Truncated)
    ));

    let (reader, mut peer) = tokio::io::duplex(16);
    peer.write_all(&[0, 0, 0, 3, b'a']).await.unwrap();
    drop(peer);
    let mut reader = FrameReader::new(reader);
    assert!(matches!(
        reader.read_frame().await,
        Err(FrameError::Truncated)
    ));
}

#[tokio::test]
async fn returns_invalid_utf8_payloads_for_client_wire_validation() {
    let (reader, mut peer) = tokio::io::duplex(8);
    peer.write_all(&[0, 0, 0, 1, 0xff]).await.unwrap();
    let mut reader = FrameReader::new(reader);

    let frame = reader.read_frame().await.unwrap().unwrap();
    assert!(decode_client_command(&frame).is_err());
}

#[tokio::test]
async fn writes_one_big_endian_length_and_payload_for_each_frame() {
    let (peer, reader) = tokio::io::duplex(32);
    let mut writer = FrameWriter::new(peer);

    writer.write_frame(b"abc").await.unwrap();

    let mut reader = reader;
    let mut bytes = [0_u8; 7];
    reader.read_exact(&mut bytes).await.unwrap();
    assert_eq!(bytes, [0, 0, 0, 3, b'a', b'b', b'c']);
}

#[tokio::test]
async fn writer_rejects_an_oversized_payload() {
    let (writer, _) = tokio::io::duplex(8);
    let mut writer = FrameWriter::new(writer);
    let payload = vec![b'x'; MAX_CLIENT_FRAME_BYTES + 1];

    assert!(matches!(
        writer.write_frame(&payload).await,
        Err(FrameError::InvalidLength(length)) if length == MAX_CLIENT_FRAME_BYTES + 1
    ));
}

#[tokio::test]
async fn acknowledges_a_complete_frame_before_a_blocked_flush() {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let (flush_polled, mut flush_polled_receiver) = watch::channel(false);
    let writer = FlushBlockingWriter {
        bytes: Arc::clone(&bytes),
        flush_polled,
    };
    let acknowledged = Arc::new(AtomicBool::new(false));
    let task_acknowledged = Arc::clone(&acknowledged);
    let task = tokio::spawn(async move {
        FrameWriter::new(writer)
            .write_frame_with_ack(b"abc", || {
                task_acknowledged.store(true, Ordering::SeqCst);
            })
            .await
    });

    if !*flush_polled_receiver.borrow() {
        flush_polled_receiver.changed().await.unwrap();
    }
    assert!(acknowledged.load(Ordering::SeqCst));
    assert_eq!(*bytes.lock().unwrap(), [0, 0, 0, 3, b'a', b'b', b'c']);

    task.abort();
    let _ = task.await;
}

struct FlushBlockingWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
    flush_polled: watch::Sender<bool>,
}

impl AsyncWrite for FlushBlockingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.bytes.lock().unwrap().extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.flush_polled.send_replace(true);
        Poll::Pending
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
