use std::fmt;
use std::fs;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use conversation_memory::{SqliteMemoryContextProvider, SqliteMemoryStore, SystemMemoryClock};
use conversation_model_adapters::{
    BufferedStreamingSpeechSynthesizer, GenerationLanguageModel, MacOsVoiceSidecar,
    MacOsVoiceSidecarConfig, OllamaConfig, OllamaLanguageModel, OpenAiCompatibleSpeechConfig,
    OpenAiCompatibleSpeechSynthesizer, OpenAiCompatibleStreamingSpeechConfig,
    OpenAiCompatibleStreamingSpeechSynthesizer, SidecarAsrBackend, SpeechSynthesizer,
    StreamingSpeechSynthesizer, SystemDevice,
};
use conversation_protocol::{
    ClientComponentDescriptor, ComponentDescriptor, ComponentKind, ConversationMode,
    ExecutionLocation, FollowUpPolicy, PersonaLevel, PersonaProfile,
    PrivacyMode as ProtocolPrivacyMode, ResponseControls, RuntimeStatus, SilencePolicy, SpeechPace,
    MAX_CLIENT_PROVIDER_LABEL_BYTES,
};
use conversation_runtime::{ConversationContext, ConversationQualityController};
use serde::Deserialize;

use crate::voice_adapters::{GatewayVoiceAdapters, VoicePolicyTemplate};

const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_MODEL_ID_BYTES: usize = 256;
const MAX_SYSTEM_PROMPT_BYTES: usize = 4 * 1024;

#[derive(Debug)]
pub struct GatewayConfigError(String);

impl fmt::Display for GatewayConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for GatewayConfigError {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    schema_version: u32,
    privacy_mode: GatewayPrivacyMode,
    language: LanguageConfig,
    persona: PersonaConfig,
    memory: Option<MemoryConfig>,
    voice: Option<VoiceConfig>,
}

pub struct GatewayAdapters {
    pub context: ConversationContext,
    pub language: Arc<dyn GenerationLanguageModel>,
    pub voice: Option<GatewayVoiceAdapters>,
    pub memory_store: Option<SqliteMemoryStore>,
    pub status: RuntimeStatus,
}

impl fmt::Debug for GatewayAdapters {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GatewayAdapters")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl GatewayAdapters {
    pub fn text_only_status(&self) -> RuntimeStatus {
        let mut status = self.status.clone();
        status
            .capabilities
            .retain(|capability| capability != "voice_session");
        status.components.retain(|component| {
            !matches!(
                component.kind.as_str(),
                "speech_recognition" | "speech_synthesis" | "audio_io"
            )
        });
        status
    }
}

impl GatewayConfig {
    pub fn load(path: &Path) -> Result<GatewayAdapters, GatewayConfigError> {
        load_toml(path)?.build_adapters()
    }

    fn build_adapters(&self) -> Result<GatewayAdapters, GatewayConfigError> {
        if self.schema_version != 1 {
            return Err(config_error(
                "gateway configuration schema_version must be 1",
            ));
        }
        if !matches!(self.privacy_mode, GatewayPrivacyMode::LocalOnly) {
            return Err(config_error("gateway privacy_mode must be local-only"));
        }
        if !matches!(self.language.backend, LanguageBackend::OllamaCompatible) {
            return Err(config_error("language backend must be ollama-compatible"));
        }
        require_local_execution(self.language.execution, "language")?;
        validate_provider_label(&self.language.provider, "language")?;
        validate_loopback_http_endpoint(&self.language.endpoint, "language")?;

        let language: Arc<dyn GenerationLanguageModel> =
            Arc::new(OllamaLanguageModel::new_direct(self.language_config()?));
        let quality = self.quality_controller()?;
        let memory = self.memory.as_ref().map(memory_adapters).transpose()?;
        let (memory_provider, memory_store) = match memory {
            Some((provider, store)) => (Some(provider), Some(store)),
            None => (None, None),
        };
        let mut context = ConversationContext::new(quality);
        if let Some(provider) = memory_provider {
            context = context
                .with_memory_provider(Arc::new(provider), ExecutionLocation::Local)
                .map_err(runtime_error)?;
        }

        let language_component = local_component(
            ComponentKind::LanguageModel,
            &self.language.provider,
            "language",
        )?;
        let memory_component = memory_store.as_ref().map(|_| {
            ComponentDescriptor::new(ComponentKind::Memory, "sqlite", ExecutionLocation::Local)
        });
        let voice = self
            .voice
            .as_ref()
            .map(|voice| voice.build(language_component.clone(), memory_component.clone()))
            .transpose()?;

        let component_descriptors = if let Some(voice) = voice.as_ref() {
            voice.policy.components().to_vec()
        } else {
            let mut components = vec![language_component];
            components.extend(memory_component);
            components
        };
        let components = component_descriptors
            .iter()
            .map(ClientComponentDescriptor::from)
            .collect();
        let memory_enabled = memory_store.is_some();
        let mut capabilities = vec!["text".to_owned()];
        if memory_enabled {
            capabilities.push("memory_inspection".to_owned());
        }
        if voice.is_some() {
            capabilities.push("voice_session".to_owned());
        }
        let status = RuntimeStatus {
            transport: "stdio".to_owned(),
            privacy_mode: "local_only".to_owned(),
            language_location: "local".to_owned(),
            model_id: self.language.model.clone(),
            memory_enabled,
            memory_location: memory_enabled.then(|| "local".to_owned()),
            telemetry_enabled: false,
            capabilities,
            components,
        };

        Ok(GatewayAdapters {
            context,
            language,
            voice,
            memory_store,
            status,
        })
    }

    fn language_config(&self) -> Result<OllamaConfig, GatewayConfigError> {
        if self.language.model.len() > MAX_MODEL_ID_BYTES {
            return Err(config_error("language model identifier exceeded 256 bytes"));
        }
        let mut language = OllamaConfig::new(&self.language.model)
            .map_err(adapter_error)?
            .with_endpoint(&self.language.endpoint)
            .map_err(adapter_error)?
            .with_thinking(self.language.thinking)
            .with_temperature(self.language.temperature)
            .with_seed(self.language.seed)
            .with_num_predict(self.language.num_predict)
            .map_err(adapter_error)?
            .with_num_ctx(self.language.num_ctx)
            .map_err(adapter_error)?
            .with_max_assistant_content_bytes(self.language.max_assistant_content_bytes)
            .map_err(adapter_error)?;
        if let Some(system_prompt) = self.language.system_prompt.as_deref() {
            if system_prompt.trim().is_empty() {
                return Err(config_error("language system_prompt cannot be empty"));
            }
            if system_prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
                return Err(config_error("language system_prompt exceeded 4 KiB"));
            }
            language = language.with_system_prompt(system_prompt);
        }
        if !self.language.temperature.is_finite() || self.language.temperature < 0.0 {
            return Err(config_error(
                "language temperature must be finite and non-negative",
            ));
        }
        Ok(language)
    }

    fn quality_controller(&self) -> Result<ConversationQualityController, GatewayConfigError> {
        let persona = PersonaProfile::new(
            persona_level(self.persona.warmth)?,
            persona_level(self.persona.humor)?,
            persona_level(self.persona.teasing)?,
            persona_level(self.persona.initiative)?,
            persona_level(self.persona.directness)?,
            persona_level(self.persona.intimacy)?,
            persona_level(self.persona.verbosity)?,
            persona_level(self.persona.follow_up_frequency)?,
        );
        let controls = ResponseControls::new(
            20,
            persona.directness(),
            SpeechPace::Natural,
            FollowUpPolicy::Contextual,
            SilencePolicy::AllowWithoutFiller,
        )
        .map_err(|error| config_error(error.message()))?;
        Ok(ConversationQualityController::new(
            persona,
            controls,
            self.persona.mode.into(),
        ))
    }
}

impl VoiceConfig {
    fn build(
        &self,
        language: ComponentDescriptor,
        memory: Option<ComponentDescriptor>,
    ) -> Result<GatewayVoiceAdapters, GatewayConfigError> {
        match (
            self.capture.device,
            self.asr.backend,
            self.speech.backend,
            self.audio.backend,
        ) {
            (
                VoiceCaptureDevice::SystemDefault,
                VoiceAsrBackend::Whisperkit | VoiceAsrBackend::Sensevoice,
                VoiceSpeechBackend::OpenaiCompatible,
                VoiceAudioBackend::ManagedSidecar,
            ) => {}
        }
        require_local_execution(self.asr.execution, "voice ASR")?;
        require_local_execution(self.speech.execution, "voice speech")?;
        require_local_execution(self.audio.execution, "voice audio")?;
        if self.asr.download {
            return Err(config_error("voice ASR model download must be disabled"));
        }
        if !self.asr.model_path.is_absolute() {
            return Err(config_error("voice ASR model path must be absolute"));
        }
        if !self.asr.model_path.is_dir() {
            return Err(config_error(
                "voice ASR model path must be an existing directory",
            ));
        }
        validate_loopback_http_endpoint(&self.speech.endpoint, "voice speech")?;

        let recognition = local_component(
            ComponentKind::SpeechRecognition,
            &self.asr.provider,
            "voice ASR",
        )?;
        let speech_component = local_component(
            ComponentKind::SpeechSynthesis,
            &self.speech.provider,
            "voice speech",
        )?;
        let audio = local_component(ComponentKind::AudioIo, &self.audio.provider, "voice audio")?;
        let mut components = vec![recognition, language, speech_component, audio];
        components.extend(memory);
        let policy = VoicePolicyTemplate::new(
            ProtocolPrivacyMode::LocalOnly,
            self.turn.speech_start_ms,
            self.turn.final_silence_ms,
            components,
        )
        .map_err(runtime_error)?;

        let mut sidecar = MacOsVoiceSidecarConfig::new(
            &self.audio.sidecar_executable,
            &self.asr.model_path,
            SystemDevice::SystemDefault,
            self.asr.download,
            self.turn.speech_start_ms,
            self.turn.final_silence_ms,
        )
        .map_err(adapter_error)?
        .with_max_stderr_bytes(self.audio.max_error_bytes)
        .map_err(adapter_error)?;
        if let Some(language) = &self.asr.language {
            sidecar = sidecar.with_language(language).map_err(adapter_error)?;
        }
        if matches!(self.asr.backend, VoiceAsrBackend::Sensevoice) {
            sidecar = sidecar.with_asr_backend(SidecarAsrBackend::Sensevoice);
        }

        Ok(GatewayVoiceAdapters {
            io: Arc::new(MacOsVoiceSidecar::new(sidecar)),
            speech: self.speech.synthesizer()?,
            policy,
        })
    }
}

impl VoiceSpeechConfig {
    fn synthesizer(&self) -> Result<Arc<dyn StreamingSpeechSynthesizer>, GatewayConfigError> {
        let speech = OpenAiCompatibleSpeechConfig::new(&self.model)
            .map_err(adapter_error)?
            .with_endpoint(&self.endpoint)
            .map_err(adapter_error)?
            .with_voice(&self.voice)
            .map_err(adapter_error)?
            .with_speed(self.speed)
            .map_err(adapter_error)?
            .with_language(&self.language)
            .map_err(adapter_error)?
            .with_instructions(&self.instructions)
            .map_err(adapter_error)?
            .with_max_tokens(self.max_tokens)
            .map_err(adapter_error)?
            .with_repetition_penalty(self.repetition_penalty)
            .map_err(adapter_error)?
            .with_max_text_bytes(self.max_text_bytes)
            .map_err(adapter_error)?
            .with_max_audio_bytes(self.max_audio_bytes)
            .map_err(adapter_error)?;

        match (self.mode, self.streaming_interval) {
            (VoiceSpeechMode::Buffered, None) => {
                let buffered: Arc<dyn SpeechSynthesizer> =
                    Arc::new(OpenAiCompatibleSpeechSynthesizer::new(speech));
                Ok(Arc::new(BufferedStreamingSpeechSynthesizer::new(buffered)))
            }
            (VoiceSpeechMode::Buffered, Some(_)) => Err(config_error(
                "voice speech streaming_interval is only valid in streaming mode",
            )),
            (VoiceSpeechMode::Streaming, None) => Err(config_error(
                "voice streaming speech requires streaming_interval",
            )),
            (VoiceSpeechMode::Streaming, Some(interval)) => {
                let streaming = OpenAiCompatibleStreamingSpeechConfig::new(speech, interval)
                    .map_err(adapter_error)?;
                Ok(Arc::new(OpenAiCompatibleStreamingSpeechSynthesizer::new(
                    streaming,
                )))
            }
        }
    }
}

fn load_toml(path: &Path) -> Result<GatewayConfig, GatewayConfigError> {
    if !path.is_absolute() {
        return Err(config_error("gateway configuration path must be absolute"));
    }
    let file = open_config_file(path)?;
    let mut contents = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|_| config_error("gateway configuration file could not be read"))?;
    if contents.len() as u64 > MAX_CONFIG_BYTES {
        return Err(config_error("gateway configuration file exceeded 64 KiB"));
    }
    let contents = std::str::from_utf8(&contents)
        .map_err(|_| config_error("gateway configuration file was not valid UTF-8"))?;
    toml::from_str(contents)
        .map_err(|_| config_error("gateway configuration file was not valid TOML"))
}

#[cfg(unix)]
fn open_config_file(path: &Path) -> Result<fs::File, GatewayConfigError> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| config_error("gateway configuration file could not be opened"))?;
    if !file
        .metadata()
        .map_err(|_| config_error("gateway configuration file could not be opened"))?
        .file_type()
        .is_file()
    {
        return Err(config_error(
            "gateway configuration file could not be opened",
        ));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_config_file(_path: &Path) -> Result<fs::File, GatewayConfigError> {
    Err(config_error(
        "gateway configuration file could not be opened",
    ))
}

fn validate_loopback_http_endpoint(
    endpoint: &str,
    component: &str,
) -> Result<(), GatewayConfigError> {
    let endpoint = reqwest::Url::parse(endpoint)
        .map_err(|_| config_error(format!("{component} endpoint must be a valid URL")))?;
    if endpoint.scheme() != "http" {
        return Err(config_error(format!(
            "{component} endpoint must use plain HTTP"
        )));
    }
    if !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(config_error(format!(
            "{component} endpoint must not contain credentials, a query, or a fragment"
        )));
    }
    let address = endpoint
        .host_str()
        .and_then(|host| host.parse::<IpAddr>().ok())
        .ok_or_else(|| {
            config_error(format!(
                "{component} endpoint must use a loopback IP address"
            ))
        })?;
    if !address.is_loopback() {
        return Err(config_error(format!(
            "{component} endpoint must use a loopback IP address"
        )));
    }
    Ok(())
}

fn require_local_execution(
    execution: ExecutionConfig,
    component: &str,
) -> Result<(), GatewayConfigError> {
    if matches!(execution, ExecutionConfig::Local) {
        Ok(())
    } else {
        Err(config_error(format!("{component} execution must be local")))
    }
}

fn validate_provider_label(label: &str, component: &str) -> Result<(), GatewayConfigError> {
    if label.is_empty() || label.trim() != label || label.len() > MAX_CLIENT_PROVIDER_LABEL_BYTES {
        return Err(config_error(format!(
            "{component} provider label must be trimmed and within 1..=128 bytes"
        )));
    }
    Ok(())
}

fn local_component(
    kind: ComponentKind,
    provider: &str,
    name: &str,
) -> Result<ComponentDescriptor, GatewayConfigError> {
    validate_provider_label(provider, name)?;
    Ok(ComponentDescriptor::new(
        kind,
        provider,
        ExecutionLocation::Local,
    ))
}

fn memory_adapters(
    config: &MemoryConfig,
) -> Result<(SqliteMemoryContextProvider, SqliteMemoryStore), GatewayConfigError> {
    if !config.database.is_absolute() {
        return Err(config_error("memory database path must be absolute"));
    }
    let store = SqliteMemoryStore::open(&config.database).map_err(memory_error)?;
    let provider = SqliteMemoryContextProvider::new(store.clone(), Arc::new(SystemMemoryClock))
        .with_limits(config.maximum_items, config.maximum_bytes)
        .map_err(memory_error)?;
    Ok((provider, store))
}

fn persona_level(value: u8) -> Result<PersonaLevel, GatewayConfigError> {
    PersonaLevel::new(value).map_err(|error| config_error(error.message()))
}

fn adapter_error(error: conversation_model_adapters::AdapterError) -> GatewayConfigError {
    config_error(error.message())
}

fn memory_error(error: conversation_memory::MemoryStoreError) -> GatewayConfigError {
    config_error(error.to_string())
}

fn runtime_error(error: conversation_protocol::RuntimeError) -> GatewayConfigError {
    config_error(error.message())
}

fn config_error(message: impl Into<String>) -> GatewayConfigError {
    GatewayConfigError(message.into())
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum GatewayPrivacyMode {
    LocalOnly,
    Hybrid,
    Cloud,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LanguageConfig {
    backend: LanguageBackend,
    execution: ExecutionConfig,
    provider: String,
    endpoint: String,
    model: String,
    #[serde(default)]
    system_prompt: Option<String>,
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
    OllamaCompatible,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonaConfig {
    mode: GatewayConversationMode,
    warmth: u8,
    humor: u8,
    teasing: u8,
    initiative: u8,
    directness: u8,
    intimacy: u8,
    verbosity: u8,
    follow_up_frequency: u8,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum GatewayConversationMode {
    DirectAnswer,
    Companionship,
    Brainstorming,
    Reflective,
}

impl From<GatewayConversationMode> for ConversationMode {
    fn from(value: GatewayConversationMode) -> Self {
        match value {
            GatewayConversationMode::DirectAnswer => Self::DirectAnswer,
            GatewayConversationMode::Companionship => Self::Companionship,
            GatewayConversationMode::Brainstorming => Self::Brainstorming,
            GatewayConversationMode::Reflective => Self::Reflective,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryConfig {
    database: PathBuf,
    maximum_items: usize,
    maximum_bytes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VoiceConfig {
    capture: VoiceCaptureConfig,
    turn: VoiceTurnConfig,
    asr: VoiceAsrConfig,
    speech: VoiceSpeechConfig,
    audio: VoiceAudioConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VoiceCaptureConfig {
    device: VoiceCaptureDevice,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum VoiceCaptureDevice {
    SystemDefault,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VoiceTurnConfig {
    speech_start_ms: u64,
    final_silence_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VoiceAsrConfig {
    backend: VoiceAsrBackend,
    execution: ExecutionConfig,
    provider: String,
    model_path: PathBuf,
    download: bool,
    language: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum VoiceAsrBackend {
    Whisperkit,
    Sensevoice,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VoiceSpeechConfig {
    backend: VoiceSpeechBackend,
    execution: ExecutionConfig,
    provider: String,
    mode: VoiceSpeechMode,
    streaming_interval: Option<f32>,
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
enum VoiceSpeechBackend {
    OpenaiCompatible,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum VoiceSpeechMode {
    Buffered,
    Streaming,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VoiceAudioConfig {
    backend: VoiceAudioBackend,
    execution: ExecutionConfig,
    provider: String,
    sidecar_executable: PathBuf,
    max_error_bytes: usize,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum VoiceAudioBackend {
    ManagedSidecar,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ExecutionConfig {
    Local,
    Remote,
}
