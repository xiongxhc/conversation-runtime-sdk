use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use conversation_model_adapters::{
    AdapterError, BufferedStreamingSpeechSynthesizer, GenerationLanguageModel,
    GenerationLanguageRequest, GenerationTextDelta, LanguageModel, LanguageModelRequest,
    MacOsVoiceSidecar, MacOsVoiceSidecarConfig, OllamaConfig, OllamaLanguageModel,
    OpenAiCompatibleSpeechConfig, OpenAiCompatibleSpeechSynthesizer, SpeechSynthesizer,
    SystemDevice,
};
use conversation_protocol::{
    ComponentDescriptor, ComponentKind, ExecutionLocation, PrivacyMode, RuntimeError, SessionId,
    VoiceSessionPolicy,
};
use conversation_runtime::VoiceSessionAdapters;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config_file::load_toml;

const SPEECH_START_MS: std::ops::RangeInclusive<u64> = 100..=1_000;
const FINAL_SILENCE_MS: std::ops::RangeInclusive<u64> = 200..=3_000;
const GENERATION_BUFFER_SIZE: usize = 16;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    schema_version: u32,
    privacy: PrivacyConfig,
    capture: CaptureConfig,
    turn: TurnConfig,
    asr: AsrConfig,
    language: LanguageConfig,
    speech: SpeechConfig,
    audio: AudioConfig,
    #[serde(default)]
    tools: Vec<OptionalComponentConfig>,
    #[serde(default)]
    memory: Vec<OptionalComponentConfig>,
    #[serde(default)]
    telemetry: Vec<OptionalComponentConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrivacyConfig {
    mode: PrivacyModeConfig,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum PrivacyModeConfig {
    LocalOnly,
    Hybrid,
    Cloud,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureConfig {
    device: CaptureDevice,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum CaptureDevice {
    SystemDefault,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TurnConfig {
    speech_start_ms: u64,
    final_silence_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AsrConfig {
    backend: AsrBackend,
    execution: ExecutionConfig,
    provider: String,
    model_path: PathBuf,
    download: bool,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum AsrBackend {
    Whisperkit,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LanguageConfig {
    backend: LanguageBackend,
    execution: ExecutionConfig,
    provider: String,
    endpoint: String,
    model: String,
    thinking: bool,
    temperature: f32,
    seed: u64,
    num_predict: usize,
    num_ctx: usize,
    max_assistant_content_bytes: usize,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum LanguageBackend {
    Ollama,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpeechConfig {
    backend: SpeechBackend,
    execution: ExecutionConfig,
    provider: String,
    mode: SpeechMode,
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

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum SpeechBackend {
    OpenaiCompatible,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum SpeechMode {
    Buffered,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioConfig {
    backend: AudioBackend,
    execution: ExecutionConfig,
    provider: String,
    sidecar_executable: PathBuf,
    max_error_bytes: usize,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum AudioBackend {
    ManagedSidecar,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionalComponentConfig {
    provider: String,
    execution: ExecutionConfig,
    enabled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ExecutionConfig {
    Local,
    Remote,
}

impl SessionConfig {
    pub fn load(path: &Path) -> Result<Self, String> {
        load_toml(path)
    }

    pub fn descriptors(&self) -> Vec<ComponentDescriptor> {
        let mut descriptors = vec![
            descriptor(
                ComponentKind::SpeechRecognition,
                &self.asr.provider,
                self.asr.execution,
            ),
            descriptor(
                ComponentKind::LanguageModel,
                &self.language.provider,
                self.language.execution,
            ),
            descriptor(
                ComponentKind::SpeechSynthesis,
                &self.speech.provider,
                self.speech.execution,
            ),
            descriptor(
                ComponentKind::AudioIo,
                &self.audio.provider,
                self.audio.execution,
            ),
        ];
        descriptors.extend(optional_descriptors(ComponentKind::Tool, &self.tools));
        descriptors.extend(optional_descriptors(ComponentKind::Memory, &self.memory));
        descriptors.extend(optional_descriptors(
            ComponentKind::Telemetry,
            &self.telemetry,
        ));
        descriptors
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 2 {
            return Err("voice session configuration schema_version must be 2".to_owned());
        }
        self.require_local_execution_adapters()?;
        if !SPEECH_START_MS.contains(&self.turn.speech_start_ms) {
            return Err("speech start threshold is outside the supported range".to_owned());
        }
        if !FINAL_SILENCE_MS.contains(&self.turn.final_silence_ms) {
            return Err("final silence threshold is outside the supported range".to_owned());
        }
        if !self.asr.model_path.is_absolute() {
            return Err("ASR model path must be absolute".to_owned());
        }
        if !self.asr.model_path.is_dir() {
            return Err("ASR model path must be an existing directory".to_owned());
        }
        if !self.audio.sidecar_executable.is_absolute() {
            return Err("sidecar executable must be absolute".to_owned());
        }
        if !self.language.temperature.is_finite() || self.language.temperature < 0.0 {
            return Err("language temperature must be finite and non-negative".to_owned());
        }
        validate_http_endpoint(&self.language.endpoint, "language", self.language.execution)?;
        validate_http_endpoint(&self.speech.endpoint, "speech", self.speech.execution)?;

        match (
            self.capture.device,
            self.asr.backend,
            self.language.backend,
            self.speech.backend,
            self.speech.mode,
            self.audio.backend,
        ) {
            (
                CaptureDevice::SystemDefault,
                AsrBackend::Whisperkit,
                LanguageBackend::Ollama,
                SpeechBackend::OpenaiCompatible,
                SpeechMode::Buffered,
                AudioBackend::ManagedSidecar,
            ) => {}
        }

        Ok(())
    }

    pub fn policy(
        &self,
        descriptors: Vec<ComponentDescriptor>,
    ) -> Result<VoiceSessionPolicy, RuntimeError> {
        VoiceSessionPolicy::new(
            SessionId::new(1),
            self.privacy.mode.into(),
            self.turn.speech_start_ms,
            self.turn.final_silence_ms,
            descriptors,
        )
    }

    pub fn adapters(&self) -> Result<VoiceSessionAdapters, String> {
        self.require_local_execution_adapters()?;
        let language_model = Arc::new(IdentityTaggedLanguageModel::new(self.language_model()?));
        let speech_synthesizer: Arc<dyn SpeechSynthesizer> = Arc::new(self.speech_synthesizer()?);
        let speech_synthesizer =
            Arc::new(BufferedStreamingSpeechSynthesizer::new(speech_synthesizer));
        let voice_io = Arc::new(self.voice_io()?);
        Ok(VoiceSessionAdapters::new(
            voice_io,
            language_model,
            speech_synthesizer,
        ))
    }

    fn require_local_execution_adapters(&self) -> Result<(), String> {
        if matches!(self.privacy.mode, PrivacyModeConfig::LocalOnly) {
            Ok(())
        } else {
            Err("privacy mode requires unavailable execution-specific adapters".to_owned())
        }
    }

    fn language_model(&self) -> Result<OllamaLanguageModel, String> {
        let config = OllamaConfig::new(&self.language.model)
            .map_err(adapter_message)?
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

    fn speech_synthesizer(&self) -> Result<OpenAiCompatibleSpeechSynthesizer, String> {
        let config = OpenAiCompatibleSpeechConfig::new(&self.speech.model)
            .map_err(adapter_message)?
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

    fn voice_io(&self) -> Result<MacOsVoiceSidecar, String> {
        let config = MacOsVoiceSidecarConfig::new(
            &self.audio.sidecar_executable,
            &self.asr.model_path,
            match self.capture.device {
                CaptureDevice::SystemDefault => SystemDevice::SystemDefault,
            },
            self.asr.download,
            self.turn.speech_start_ms,
            self.turn.final_silence_ms,
        )
        .map_err(adapter_message)?
        .with_max_stderr_bytes(self.audio.max_error_bytes)
        .map_err(adapter_message)?;
        Ok(MacOsVoiceSidecar::new(config))
    }
}

impl From<PrivacyModeConfig> for PrivacyMode {
    fn from(value: PrivacyModeConfig) -> Self {
        match value {
            PrivacyModeConfig::LocalOnly => Self::LocalOnly,
            PrivacyModeConfig::Hybrid => Self::Hybrid,
            PrivacyModeConfig::Cloud => Self::Cloud,
        }
    }
}

impl From<ExecutionConfig> for ExecutionLocation {
    fn from(value: ExecutionConfig) -> Self {
        match value {
            ExecutionConfig::Local => Self::Local,
            ExecutionConfig::Remote => Self::Remote,
        }
    }
}

fn descriptor(
    kind: ComponentKind,
    provider: &str,
    execution: ExecutionConfig,
) -> ComponentDescriptor {
    ComponentDescriptor::new(kind, provider, execution.into())
}

fn optional_descriptors(
    kind: ComponentKind,
    components: &[OptionalComponentConfig],
) -> impl Iterator<Item = ComponentDescriptor> + '_ {
    components
        .iter()
        .filter(|component| component.enabled)
        .map(move |component| descriptor(kind, &component.provider, component.execution))
}

fn adapter_message(error: AdapterError) -> String {
    error.message().to_owned()
}

fn validate_http_endpoint(
    endpoint: &str,
    name: &str,
    execution: ExecutionConfig,
) -> Result<(), String> {
    let endpoint = reqwest::Url::parse(endpoint)
        .map_err(|_| format!("{name} endpoint must be a valid URL"))?;
    if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
        return Err(format!("{name} endpoint must be a valid HTTP(S) URL"));
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
    if matches!(execution, ExecutionConfig::Local) {
        if endpoint.scheme() != "http" {
            return Err(format!("{name} local endpoint must use plain HTTP"));
        }
        let address = endpoint
            .host_str()
            .and_then(|host| host.parse::<IpAddr>().ok())
            .ok_or_else(|| format!("{name} local endpoint must use a loopback IP address"))?;
        if !address.is_loopback() {
            return Err(format!(
                "{name} local endpoint must use a loopback IP address"
            ));
        }
    }
    Ok(())
}

#[derive(Clone)]
struct IdentityTaggedLanguageModel {
    inner: OllamaLanguageModel,
}

impl IdentityTaggedLanguageModel {
    const fn new(inner: OllamaLanguageModel) -> Self {
        Self { inner }
    }
}

impl GenerationLanguageModel for IdentityTaggedLanguageModel {
    fn stream(
        &self,
        request: GenerationLanguageRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
        let (sender, receiver) = mpsc::channel(GENERATION_BUFFER_SIZE);
        let turn_id = request.turn_id();
        let generation_id = request.generation_id();
        let mut deltas = self.inner.stream(
            LanguageModelRequest::new(turn_id, request.transcript()),
            cancellation.clone(),
        );

        tokio::spawn(async move {
            loop {
                let delta = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return,
                    delta = deltas.recv() => delta,
                };
                let Some(delta) = delta else {
                    return;
                };
                let tagged =
                    delta.map(|delta| GenerationTextDelta::new(turn_id, generation_id, delta));
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return,
                    result = sender.send(tagged) => {
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
