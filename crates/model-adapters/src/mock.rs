use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::{
    AdapterError, AdapterFuture, AudioFormat, AudioOutput, AudioOutputRequest, LanguageModel,
    LanguageModelRequest, SpeechRequest, SpeechSynthesizer, SynthesizedAudio,
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
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(AdapterError::new("speech synthesis cancelled"));
            }

            if self.delay.is_zero() {
                return self.synthesized_audio();
            }

            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    Err(AdapterError::new("speech synthesis cancelled"))
                }
                _ = sleep(self.delay) => self.synthesized_audio(),
            }
        })
    }
}

impl MockSpeechSynthesizer {
    fn synthesized_audio(&self) -> Result<SynthesizedAudio, AdapterError> {
        let audio = SynthesizedAudio::new(self.audio.clone(), AudioFormat::Aiff);
        audio.validate()?;
        Ok(audio)
    }
}

#[derive(Clone, Debug)]
pub struct MockAudioOutput {
    requests: Arc<Mutex<Vec<AudioOutputRequest>>>,
    delay: Duration,
}

impl MockAudioOutput {
    pub fn new() -> Self {
        Self::delayed(Duration::ZERO)
    }

    pub fn delayed(delay: Duration) -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            delay,
        }
    }

    pub fn requests(&self) -> Vec<AudioOutputRequest> {
        self.requests
            .lock()
            .expect("mock audio output requests lock poisoned")
            .clone()
    }
}

impl Default for MockAudioOutput {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioOutput for MockAudioOutput {
    fn play<'a>(
        &'a self,
        request: AudioOutputRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(AdapterError::new("audio output cancelled"));
            }

            self.requests
                .lock()
                .expect("mock audio output requests lock poisoned")
                .push(request.clone());

            if self.delay.is_zero() {
                return Ok(());
            }

            tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(AdapterError::new("audio output cancelled")),
                _ = sleep(self.delay) => Ok(()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use conversation_protocol::TurnId;
    use tokio_util::sync::CancellationToken;

    use super::{MockLanguageModel, MockSpeechSynthesizer};
    use crate::{
        AudioFormat, AudioOutput, AudioOutputRequest, DiscardAudioOutput, LanguageModel,
        LanguageModelRequest, MockAudioOutput, SpeechRequest, SpeechSynthesizer, SynthesizedAudio,
    };

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

    #[tokio::test]
    async fn mock_speech_synthesizer_returns_typed_aiff_audio() {
        let expected_audio = minimal_aiff();
        let speech = MockSpeechSynthesizer::new(expected_audio.clone());
        let audio = speech
            .synthesize(
                SpeechRequest::new(TurnId::new(1), "hello"),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(audio.bytes(), expected_audio);
        assert_eq!(audio.format(), AudioFormat::Aiff);
    }

    #[test]
    fn audio_output_request_exposes_its_typed_fields() {
        let audio = SynthesizedAudio::new([1, 2, 3], AudioFormat::Aiff);
        let request = AudioOutputRequest::new(TurnId::new(1), 7, audio.clone());

        assert_eq!(request.turn_id(), TurnId::new(1));
        assert_eq!(request.segment_index(), 7);
        assert_eq!(request.audio(), &audio);
    }

    #[tokio::test]
    async fn discard_output_accepts_typed_audio() {
        let output = DiscardAudioOutput;
        output
            .play(
                AudioOutputRequest::new(
                    TurnId::new(1),
                    7,
                    SynthesizedAudio::new([1, 2, 3], AudioFormat::Aiff),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn pre_cancelled_mock_output_returns_cancellation() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = MockAudioOutput::new()
            .play(
                AudioOutputRequest::new(
                    TurnId::new(1),
                    0,
                    SynthesizedAudio::new([1], AudioFormat::Aiff),
                ),
                cancellation,
            )
            .await
            .unwrap_err();
        assert_eq!(error.message(), "audio output cancelled");
    }

    #[tokio::test]
    async fn mock_audio_output_records_a_snapshot_of_played_requests() {
        let output = MockAudioOutput::new();
        let request = AudioOutputRequest::new(
            TurnId::new(1),
            2,
            SynthesizedAudio::new([1, 2, 3], AudioFormat::Aiff),
        );

        output
            .play(request.clone(), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(output.requests(), vec![request]);
    }

    fn minimal_aiff() -> Vec<u8> {
        let mut bytes = Vec::from(&b"FORM"[..]);
        bytes.extend_from_slice(&48_u32.to_be_bytes());
        bytes.extend_from_slice(b"AIFFCOMM");
        bytes.extend_from_slice(&18_u32.to_be_bytes());
        bytes.extend_from_slice(&[0; 18]);
        bytes.extend_from_slice(b"SSND");
        bytes.extend_from_slice(&9_u32.to_be_bytes());
        bytes.extend_from_slice(&[0; 8]);
        bytes.extend_from_slice(&[0x80, 0]);
        bytes
    }
}
