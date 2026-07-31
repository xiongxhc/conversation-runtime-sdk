use std::net::IpAddr;
use std::path::Path;

use conversation_model_adapters::{
    MacOsAfplayAudioOutput, MacOsAfplayConfig, OllamaConfig, OllamaLanguageModel,
    OpenAiCompatibleSpeechConfig, OpenAiCompatibleSpeechSynthesizer,
};
use serde::Deserialize;

use crate::config_file::load_toml;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VoiceConfig {
    schema_version: u32,
    language: LanguageConfig,
    speech: SpeechConfig,
    audio: AudioConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LanguageConfig {
    endpoint: String,
    model: String,
    thinking: bool,
    temperature: f32,
    seed: u64,
    num_predict: usize,
    num_ctx: usize,
    max_assistant_content_bytes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpeechConfig {
    endpoint: String,
    model: String,
    voice: String,
    speed: f32,
    language: String,
    instructions: String,
    max_tokens: usize,
    repetition_penalty: f32,
    max_text_bytes: usize,
    max_audio_bytes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioConfig {
    backend: AudioBackend,
    executable: std::path::PathBuf,
    temp_directory: std::path::PathBuf,
    max_audio_bytes: usize,
    max_error_bytes: usize,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum AudioBackend {
    MacosAfplay,
}

impl VoiceConfig {
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let config: Self = load_toml(path)?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn language_model(&self) -> Result<OllamaLanguageModel, String> {
        let mut config = OllamaConfig::new(&self.language.model).map_err(adapter_message)?;
        config = config
            .with_endpoint(&self.language.endpoint)
            .map_err(adapter_message)?
            .with_thinking(self.language.thinking)
            .with_temperature(self.language.temperature)
            .with_seed(self.language.seed)
            .with_num_predict(self.language.num_predict)
            .map_err(adapter_message)?
            .with_num_ctx(self.language.num_ctx)
            .map_err(adapter_message)?
            .with_max_assistant_content_bytes(self.language.max_assistant_content_bytes)
            .map_err(adapter_message)?;
        Ok(OllamaLanguageModel::new(config))
    }

    pub(crate) fn speech_synthesizer(&self) -> Result<OpenAiCompatibleSpeechSynthesizer, String> {
        let mut config =
            OpenAiCompatibleSpeechConfig::new(&self.speech.model).map_err(adapter_message)?;
        config = config
            .with_endpoint(&self.speech.endpoint)
            .map_err(adapter_message)?
            .with_voice(&self.speech.voice)
            .map_err(adapter_message)?
            .with_speed(self.speech.speed)
            .map_err(adapter_message)?
            .with_language(&self.speech.language)
            .map_err(adapter_message)?
            .with_instructions(&self.speech.instructions)
            .map_err(adapter_message)?
            .with_max_tokens(self.speech.max_tokens)
            .map_err(adapter_message)?
            .with_repetition_penalty(self.speech.repetition_penalty)
            .map_err(adapter_message)?
            .with_max_text_bytes(self.speech.max_text_bytes)
            .map_err(adapter_message)?
            .with_max_audio_bytes(self.speech.max_audio_bytes)
            .map_err(adapter_message)?;
        Ok(OpenAiCompatibleSpeechSynthesizer::new(config))
    }

    pub(crate) fn audio_output(&self) -> Result<MacOsAfplayAudioOutput, String> {
        match self.audio.backend {
            AudioBackend::MacosAfplay => {}
        }
        let config = MacOsAfplayConfig::new(&self.audio.executable)
            .map_err(adapter_message)?
            .with_temp_directory(&self.audio.temp_directory)
            .map_err(adapter_message)?
            .with_max_audio_bytes(self.audio.max_audio_bytes)
            .map_err(adapter_message)?
            .with_max_stderr_bytes(self.audio.max_error_bytes)
            .map_err(adapter_message)?;
        Ok(MacOsAfplayAudioOutput::new(config))
    }

    pub(crate) const fn max_response_bytes(&self) -> usize {
        self.language.max_assistant_content_bytes
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err("voice configuration schema_version must be 1".to_owned());
        }
        validate_loopback_http_endpoint(&self.language.endpoint, "language")?;
        validate_loopback_http_endpoint(&self.speech.endpoint, "speech")?;
        if !self.language.temperature.is_finite() || self.language.temperature < 0.0 {
            return Err("language temperature must be finite and non-negative".to_owned());
        }
        self.language_model()?;
        self.speech_synthesizer()?;
        self.audio_output()?;
        Ok(())
    }
}

fn validate_loopback_http_endpoint(endpoint: &str, name: &str) -> Result<(), String> {
    let endpoint = reqwest::Url::parse(endpoint)
        .map_err(|_| format!("{name} endpoint must be a valid URL"))?;
    if endpoint.scheme() != "http" {
        return Err(format!("{name} endpoint must use plain HTTP"));
    }
    if !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(format!(
            "{name} endpoint must not contain credentials, a query, or a fragment"
        ));
    }
    let address = endpoint
        .host_str()
        .and_then(|host| host.parse::<IpAddr>().ok())
        .ok_or_else(|| format!("{name} endpoint must use a loopback IP address"))?;
    if !address.is_loopback() {
        return Err(format!("{name} endpoint must use a loopback IP address"));
    }
    Ok(())
}

fn adapter_message(error: conversation_model_adapters::AdapterError) -> String {
    error.message().to_owned()
}
