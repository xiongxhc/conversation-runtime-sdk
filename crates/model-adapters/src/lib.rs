mod audio_frame;
mod audio_output;
mod base64;
mod buffered_streaming_speech;
mod capture;
mod capture_timing;
mod continuous_audio_output;
mod credential;
mod diarization;
mod generation_language;
mod language_model;
mod macos_afplay;
mod macos_system_speech;
mod macos_voice_sidecar;
mod mock;
mod network_voice_io;
mod ollama;
mod openai_compatible_speech;
mod openai_compatible_streaming_speech;
mod openai_diarization;
mod openai_language;
mod openai_speech_recognition;
mod recognition;
mod speech;
#[cfg(unix)]
mod speechbrain_diarization;
mod streaming_speech;
mod voice_capture_control;
mod voice_input;
mod voice_io;
mod voice_mock;
mod wav_pcm;

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use conversation_protocol::RuntimeStage;

pub use audio_frame::{AudioFrame, PcmFormat, PcmSampleFormat, MAX_PCM_FRAME_BYTES};
pub use audio_output::{AudioOutput, AudioOutputRequest, DiscardAudioOutput};
pub use buffered_streaming_speech::BufferedStreamingSpeechSynthesizer;
pub use capture::{AudioCapture, CaptureEvent};
pub use capture_timing::{
    validate_capture_identifier, CaptureClockPair, CaptureDiscontinuityReason,
    CaptureDomainHistory, CaptureEpoch, CaptureFinishBoundary, CaptureMappingQuality,
    CaptureMetadata, FrameProvenance, NominalCaptureSpan, TimedCapture, CAPTURE_TIMING_BYTES,
    CLOCK_PAIR_CADENCE_NS, MAX_CAPTURE_CLOCK_DOMAINS, MAX_CAPTURE_IDENTIFIER_BYTES,
    MAX_CAPTURE_TIMING_BYTES, MAX_CLOCK_BRACKET_NS, MAX_CLOCK_PAIR_GAP_NS, MAX_CLOCK_RESIDUAL_NS,
    MAX_RECORDING_TIMING_BYTES, MAX_RECORDING_TIMING_RECORDS,
};
pub use continuous_audio_output::{ContinuousAudioOutput, PlaybackReceipt};
pub use diarization::{
    DiarizationResult, DiarizationWarning, DiarizedMediaSampleRange, DiarizedSegment,
    MediaSampleOrigin, MeetingAudioWindow, MeetingAudioWindows, TranscriptionDiarizer,
    MAX_DIARIZED_SEGMENTS, MAX_DIARIZED_SEGMENT_ID_BYTES, MAX_DIARIZED_SEGMENT_TEXT_BYTES,
    MAX_DIARIZED_TEXT_BYTES, MAX_SPEAKER_CLUSTERS,
};
pub use generation_language::{
    GenerationLanguageModel, GenerationLanguageRequest, GenerationTextDelta,
};
pub use language_model::{
    LanguageModel, LanguageModelInput, LanguageModelRequest, MAX_LANGUAGE_MODEL_INPUT_BYTES,
    MAX_RESPONSE_SCHEMA_BYTES, MAX_RUNTIME_GUIDANCE_BYTES,
};
pub use macos_afplay::{MacOsAfplayAudioOutput, MacOsAfplayConfig};
pub use macos_system_speech::{MacOsSystemSpeechConfig, MacOsSystemSpeechSynthesizer};
#[cfg(unix)]
pub use macos_voice_sidecar::{
    MacOsVoiceSidecar, MacOsVoiceSidecarConfig, MacOsVoiceSidecarSession, SidecarAsrBackend,
    SystemDevice,
};
pub use mock::{MockAudioOutput, MockLanguageModel, MockSpeechSynthesizer};
pub use network_voice_io::{
    frame_for_network_capture, NetworkBridgeRequest, NetworkFlushRequest, NetworkMediaHub,
    NetworkPlaybackAck, NetworkPlaybackRequest, NetworkVoiceIoFactory,
};
pub use ollama::{
    OllamaChatMetrics, OllamaChatStream, OllamaConfig, OllamaLanguageModel, OllamaThinkingLevel,
};
pub use openai_compatible_speech::{
    OpenAiCompatibleSpeechConfig, OpenAiCompatibleSpeechSynthesizer,
};
pub use openai_compatible_streaming_speech::{
    OpenAiCompatibleStreamingSpeechConfig, OpenAiCompatibleStreamingSpeechSynthesizer,
};
pub use openai_diarization::{OpenAiDiarizationConfig, OpenAiTranscriptionDiarizer};
pub use openai_language::{OpenAiLanguageConfig, OpenAiLanguageModel};
pub use openai_speech_recognition::{
    OnlineRecognitionSegment, OpenAiSpeechRecognitionConfig, OpenAiSpeechRecognizer,
};
pub use recognition::{
    FinalizedUtterance, RecognitionEvent, RecognitionHypothesis, RecognitionStream,
    SpeechRecognizer,
};
pub use speech::{AudioFormat, SpeechRequest, SpeechSynthesizer, SynthesizedAudio};
#[cfg(unix)]
pub use speechbrain_diarization::{SpeechbrainDiarizationConfig, SpeechbrainTranscriptionDiarizer};
pub use streaming_speech::{StreamingSpeechRequest, StreamingSpeechSynthesizer};
pub use voice_capture_control::VoiceCaptureControl;
pub use voice_input::{AudioDeviceStatus, CaptureFinish, VoiceInput, VoiceInputEvent};
pub use voice_io::{VoiceIoFactory, VoiceIoSession};
pub use voice_mock::{
    MockAudioCapture, MockContinuousAudioOutput, MockGenerationLanguageModel, MockSpeechRecognizer,
    MockStreamingSpeechSynthesizer, MockVoiceCaptureControl, MockVoiceInput, MockVoiceIoFactory,
};
pub use wav_pcm::WavPcmDecoder;

pub type AdapterFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AdapterError>> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterError {
    message: String,
    stage: Option<RuntimeStage>,
}

impl AdapterError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            stage: None,
        }
    }

    pub fn with_stage(mut self, stage: RuntimeStage) -> Self {
        self.stage = Some(stage);
        self
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub const fn stage(&self) -> Option<RuntimeStage> {
        self.stage
    }
}

impl fmt::Display for AdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl Error for AdapterError {}
