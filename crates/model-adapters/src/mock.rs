use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::{
    AdapterError, AdapterFuture, LanguageModel, LanguageModelRequest, SpeechRequest,
    SpeechSynthesizer,
};

#[derive(Clone, Debug)]
pub struct MockLanguageModel {
    deltas: Vec<String>,
    delay: Duration,
}

impl MockLanguageModel {
    pub fn new<I, S>(deltas: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::delayed(deltas, Duration::ZERO)
    }

    pub fn delayed<I, S>(deltas: I, delay: Duration) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            deltas: deltas.into_iter().map(Into::into).collect(),
            delay,
        }
    }
}

impl LanguageModel for MockLanguageModel {
    fn stream(
        &self,
        _request: LanguageModelRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>> {
        let (sender, receiver) = mpsc::channel(self.deltas.len().max(1));
        let deltas = self.deltas.clone();
        let delay = self.delay;

        tokio::spawn(async move {
            for delta in deltas {
                if !delay.is_zero() {
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => return,
                        _ = sleep(delay) => {}
                    }
                }

                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return,
                    result = sender.send(Ok(delta)) => {
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

#[derive(Clone, Debug)]
pub struct MockSpeechSynthesizer {
    audio: Vec<u8>,
    delay: Duration,
}

impl MockSpeechSynthesizer {
    pub fn new<I>(audio: I) -> Self
    where
        I: IntoIterator<Item = u8>,
    {
        Self::delayed(audio, Duration::ZERO)
    }

    pub fn delayed<I>(audio: I, delay: Duration) -> Self
    where
        I: IntoIterator<Item = u8>,
    {
        Self {
            audio: audio.into_iter().collect(),
            delay,
        }
    }
}

impl SpeechSynthesizer for MockSpeechSynthesizer {
    fn synthesize<'a>(
        &'a self,
        _request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, Vec<u8>> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(AdapterError::new("speech synthesis cancelled"));
            }

            if self.delay.is_zero() {
                return Ok(self.audio.clone());
            }

            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    Err(AdapterError::new("speech synthesis cancelled"))
                }
                _ = sleep(self.delay) => Ok(self.audio.clone()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use conversation_protocol::TurnId;
    use tokio_util::sync::CancellationToken;

    use super::MockLanguageModel;
    use crate::{LanguageModel, LanguageModelRequest};

    #[tokio::test]
    async fn mock_language_model_streams_configured_deltas() {
        let model = MockLanguageModel::new(["hello", " there"]);
        let mut stream = model.stream(
            LanguageModelRequest::new(TurnId::new(1), "hi"),
            CancellationToken::new(),
        );

        assert_eq!(stream.recv().await.unwrap().unwrap(), "hello");
        assert_eq!(stream.recv().await.unwrap().unwrap(), " there");
        assert!(stream.recv().await.is_none());
    }
}
