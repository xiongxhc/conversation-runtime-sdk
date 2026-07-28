use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioFormat, BufferedStreamingSpeechSynthesizer, SpeechRequest,
    SpeechSynthesizer, StreamingSpeechRequest, StreamingSpeechSynthesizer, SynthesizedAudio,
};
use conversation_protocol::{GenerationId, TurnId, UtteranceId};
use tokio::sync::Notify;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn buffered_adapter_emits_ordered_identity_tagged_pcm_frames() {
    let inner = Arc::new(MockSpeechSynthesizer::new(pcm16_wav(24_000, 1, 1_200)));
    let adapter = BufferedStreamingSpeechSynthesizer::new(inner.clone());
    let request = StreamingSpeechRequest::new(
        TurnId::new(2),
        GenerationId::new(3),
        UtteranceId::new(4),
        "hello",
    );

    let mut receiver = adapter.stream(request.clone(), CancellationToken::new());
    let mut frames = Vec::new();
    while let Some(frame) = receiver.recv().await {
        frames.push(frame.unwrap());
    }

    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].sequence(), 0);
    assert_eq!(frames[1].sequence(), 1);
    assert_eq!(frames[2].bytes().len(), 480);
    assert!(frames.iter().all(|frame| {
        frame.turn_id() == request.turn_id()
            && frame.generation_id() == request.generation_id()
            && frame.utterance_id() == request.utterance_id()
    }));
    assert_eq!(inner.request_count(), 1);
}

#[tokio::test]
async fn buffered_adapter_cancellation_before_synthesis_closes_without_starting_work() {
    let inner = Arc::new(MockSpeechSynthesizer::new(pcm16_wav(24_000, 1, 480)));
    let adapter = BufferedStreamingSpeechSynthesizer::new(inner.clone());
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let mut receiver = adapter.stream(request(), cancellation);

    assert!(timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .is_none());
    assert_eq!(inner.request_count(), 0);
}

#[tokio::test]
async fn buffered_adapter_cancellation_during_synthesis_closes_after_inner_cleanup() {
    let inner = Arc::new(MockSpeechSynthesizer::delayed(
        pcm16_wav(24_000, 1, 480),
        Duration::from_secs(1),
    ));
    let adapter = BufferedStreamingSpeechSynthesizer::new(inner.clone());
    let cancellation = CancellationToken::new();
    let mut receiver = adapter.stream(request(), cancellation.clone());

    timeout(Duration::from_secs(1), inner.wait_for_request())
        .await
        .unwrap();
    cancellation.cancel();

    assert!(timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn buffered_adapter_cancellation_breaks_backpressured_frame_emission() {
    let inner = Arc::new(MockSpeechSynthesizer::new(pcm16_wav(24_000, 1, 1_440)));
    let adapter = BufferedStreamingSpeechSynthesizer::new(inner.clone());
    let cancellation = CancellationToken::new();
    let mut receiver = adapter.stream(request(), cancellation.clone());

    timeout(Duration::from_secs(1), inner.wait_for_request())
        .await
        .unwrap();
    sleep(Duration::from_millis(20)).await;
    cancellation.cancel();

    let first_frame = timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first_frame.sequence(), 0);
    assert!(timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .is_none());
}

#[derive(Debug)]
struct MockSpeechSynthesizer {
    audio: SynthesizedAudio,
    delay: Duration,
    request_count: AtomicUsize,
    requested: Notify,
}

impl MockSpeechSynthesizer {
    fn new(audio: SynthesizedAudio) -> Self {
        Self::delayed(audio, Duration::ZERO)
    }

    fn delayed(audio: SynthesizedAudio, delay: Duration) -> Self {
        Self {
            audio,
            delay,
            request_count: AtomicUsize::new(0),
            requested: Notify::new(),
        }
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }

    async fn wait_for_request(&self) {
        self.requested.notified().await;
    }
}

impl SpeechSynthesizer for MockSpeechSynthesizer {
    fn synthesize<'a>(
        &'a self,
        _request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        Box::pin(async move {
            self.request_count.fetch_add(1, Ordering::SeqCst);
            self.requested.notify_waiters();

            if !self.delay.is_zero() {
                tokio::select! {
                    _ = cancellation.cancelled() => return Err(AdapterError::new("speech synthesis cancelled")),
                    _ = sleep(self.delay) => {}
                }
            }

            if cancellation.is_cancelled() {
                return Err(AdapterError::new("speech synthesis cancelled"));
            }
            Ok(self.audio.clone())
        })
    }
}

fn request() -> StreamingSpeechRequest {
    StreamingSpeechRequest::new(
        TurnId::new(2),
        GenerationId::new(3),
        UtteranceId::new(4),
        "hello",
    )
}

fn pcm16_wav(sample_rate_hz: u32, channels: u16, samples: usize) -> SynthesizedAudio {
    let block_align = channels * 2;
    let mut bytes = Vec::from(&b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0"[..]);
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&sample_rate_hz.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate_hz * u32::from(block_align)).to_le_bytes());
    bytes.extend_from_slice(&block_align.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&u32::try_from(samples * 2).unwrap().to_le_bytes());
    bytes.resize(bytes.len() + samples * 2, 0);
    let riff_size = u32::try_from(bytes.len() - 8).unwrap();
    bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
    SynthesizedAudio::new(bytes, AudioFormat::Wav)
}
