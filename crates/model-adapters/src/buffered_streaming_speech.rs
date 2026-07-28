use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    AdapterError, SpeechRequest, SpeechSynthesizer, StreamingSpeechRequest,
    StreamingSpeechSynthesizer, WavPcmDecoder,
};

pub struct BufferedStreamingSpeechSynthesizer {
    inner: Arc<dyn SpeechSynthesizer>,
    decoder: WavPcmDecoder,
}

impl BufferedStreamingSpeechSynthesizer {
    pub fn new(inner: Arc<dyn SpeechSynthesizer>) -> Self {
        Self {
            inner,
            decoder: WavPcmDecoder::default(),
        }
    }
}

impl StreamingSpeechSynthesizer for BufferedStreamingSpeechSynthesizer {
    fn stream(
        &self,
        request: StreamingSpeechRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<crate::AudioFrame, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        let inner = Arc::clone(&self.inner);
        let decoder = self.decoder;

        tokio::spawn(async move {
            if cancellation.is_cancelled() {
                return;
            }

            let synthesis = inner
                .synthesize(
                    SpeechRequest::new(request.turn_id(), request.text()),
                    cancellation.clone(),
                )
                .await;
            if cancellation.is_cancelled() {
                return;
            }

            let audio = match synthesis {
                Ok(audio) => audio,
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                    return;
                }
            };

            let frames = match decoder.decode(
                request.turn_id(),
                request.generation_id(),
                request.utterance_id(),
                &audio,
            ) {
                Ok(frames) => frames,
                Err(error) => {
                    if !cancellation.is_cancelled() {
                        let _ = sender.send(Err(error)).await;
                    }
                    return;
                }
            };

            for frame in frames {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return,
                    result = sender.send(Ok(frame)) => {
                        if result.is_err() {
                            return;
                        }
                    }
                }
            }
        });

        receiver
    }
}
