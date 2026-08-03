use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use conversation_memory::{SqliteMemoryContextProvider, SqliteMemoryStore, SystemMemoryClock};
use conversation_model_adapters::{
    AdapterError, BufferedStreamingSpeechSynthesizer, GenerationLanguageModel,
    GenerationLanguageRequest, GenerationTextDelta, LanguageModel, LanguageModelRequest,
    MacOsVoiceSidecar, MacOsVoiceSidecarConfig, OllamaConfig, OllamaLanguageModel,
    OpenAiCompatibleSpeechConfig, OpenAiCompatibleSpeechSynthesizer,
    OpenAiCompatibleStreamingSpeechConfig, OpenAiCompatibleStreamingSpeechSynthesizer,
    SpeechSynthesizer, StreamingSpeechSynthesizer, SystemDevice,
};
use conversation_protocol::{
    ComponentDescriptor, ComponentKind, ConversationMode, ExecutionLocation, FollowUpPolicy,
    PersonaLevel, PersonaProfile, PrivacyMode, ResponseControls, RuntimeError, SessionId,
    SilencePolicy, SpeechPace, VoiceSessionPolicy, MAX_MEMORY_RETRIEVAL_BYTES,
    MAX_MEMORY_RETRIEVAL_ITEMS,
};
use conversation_runtime::{ConversationQualityController, VoiceSessionAdapters};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config_file::load_toml;

const SPEECH_START_MS: std::ops::RangeInclusive<u64> = 100..=1_000;
const FINAL_SILENCE_MS: std::ops::RangeInclusive<u64> = 200..=3_000;
const GENERATION_BUFFER_SIZE: usize = 16;
const BUNDLED_SIDECAR_NAME: &str = "conversation-voice-sidecar";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    schema_version: u32,
    privacy: PrivacyConfig,
    capture: CaptureConfig,
    turn: TurnConfig,
    #[serde(default)]
    persona: PersonaConfig,
    #[serde(default)]
    response: ResponseConfig,
    #[serde(default)]
    quality_metrics: QualityMetricsConfig,
    asr: AsrConfig,
    language: LanguageConfig,
    speech: SpeechConfig,
    audio: AudioConfig,
    #[serde(default)]
    tools: Vec<OptionalComponentConfig>,
    #[serde(default)]
    memory: Vec<OptionalComponentConfig>,
    memory_store: Option<MemoryStoreConfig>,
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
#[serde(default, deny_unknown_fields)]
struct PersonaConfig {
    warmth: f32,
    humor: f32,
    teasing: f32,
    initiative: f32,
    directness: f32,
    intimacy: f32,
    verbosity: f32,
    follow_up_frequency: f32,
}

impl Default for PersonaConfig {
    fn default() -> Self {
        Self {
            warmth: 0.8,
            humor: 0.6,
            teasing: 0.4,
            initiative: 0.35,
            directness: 0.8,
            intimacy: 0.3,
            verbosity: 0.2,
            follow_up_frequency: 0.25,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ResponseConfig {
    mode: ConversationModeConfig,
    maximum_spoken_seconds: u16,
    pace: SpeechPaceConfig,
    allow_silence: bool,
    ask_follow_up_by_default: bool,
}

impl Default for ResponseConfig {
    fn default() -> Self {
        Self {
            mode: ConversationModeConfig::DirectAnswer,
            maximum_spoken_seconds: 20,
            pace: SpeechPaceConfig::Natural,
            allow_silence: true,
            ask_follow_up_by_default: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ConversationModeConfig {
    DirectAnswer,
    Companionship,
    Brainstorming,
    Reflective,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum SpeechPaceConfig {
    Measured,
    Natural,
    Brisk,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct QualityMetricsConfig {
    enabled: bool,
    record_content: bool,
}

impl Default for QualityMetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            record_content: false,
        }
    }
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
enum SpeechBackend {
    OpenaiCompatible,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum SpeechMode {
    Buffered,
    Streaming,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AudioConfig {
    backend: AudioBackend,
    execution: ExecutionConfig,
    provider: String,
    sidecar_executable: Option<PathBuf>,
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryStoreConfig {
    database_path: PathBuf,
    max_items: usize,
    max_bytes: usize,
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
        self.validate_memory_configuration()?;
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
        if self
            .audio
            .sidecar_executable
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err("sidecar executable override must be absolute".to_owned());
        }
        if !self.language.temperature.is_finite() || self.language.temperature < 0.0 {
            return Err("language temperature must be finite and non-negative".to_owned());
        }
        self.persona_profile()?;
        self.response_controls()?;
        if self.quality_metrics.record_content {
            return Err("quality metrics cannot record transcript content".to_owned());
        }
        validate_http_endpoint(&self.language.endpoint, "language", self.language.execution)?;
        validate_http_endpoint(&self.speech.endpoint, "speech", self.speech.execution)?;
        self.validate_speech_mode()?;

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
                SpeechMode::Buffered | SpeechMode::Streaming,
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
        let memory_provider = self.memory_provider()?;
        self.require_local_execution_adapters()?;
        let language_model = Arc::new(IdentityTaggedLanguageModel::new(self.language_model()?));
        self.validate_speech_mode()?;
        let speech_synthesizer: Arc<dyn StreamingSpeechSynthesizer> = match self.speech.mode {
            SpeechMode::Buffered => {
                let synthesizer: Arc<dyn SpeechSynthesizer> = Arc::new(
                    OpenAiCompatibleSpeechSynthesizer::new(self.speech_config()?),
                );
                Arc::new(BufferedStreamingSpeechSynthesizer::new(synthesizer))
            }
            SpeechMode::Streaming => {
                let interval = self.speech.streaming_interval.ok_or_else(|| {
                    "streaming speech mode requires streaming_interval".to_owned()
                })?;
                let config =
                    OpenAiCompatibleStreamingSpeechConfig::new(self.speech_config()?, interval)
                        .map_err(adapter_message)?;
                Arc::new(OpenAiCompatibleStreamingSpeechSynthesizer::new(config))
            }
        };
        let voice_io = Arc::new(self.voice_io()?);
        let mut adapters = VoiceSessionAdapters::new(voice_io, language_model, speech_synthesizer)
            .with_quality_controller(self.quality_controller()?);
        if let Some(provider) = memory_provider {
            adapters = adapters
                .with_memory_provider(Arc::new(provider), self.language.execution.into())
                .map_err(runtime_message)?;
        }
        Ok(adapters)
    }

    pub const fn quality_metrics_enabled(&self) -> bool {
        self.quality_metrics.enabled
    }

    fn require_local_execution_adapters(&self) -> Result<(), String> {
        if matches!(self.privacy.mode, PrivacyModeConfig::LocalOnly) {
            Ok(())
        } else {
            Err("privacy mode requires unavailable execution-specific adapters".to_owned())
        }
    }

    fn validate_memory_configuration(&self) -> Result<(), String> {
        let enabled = self
            .memory
            .iter()
            .filter(|component| component.enabled)
            .collect::<Vec<_>>();
        let Some(store) = &self.memory_store else {
            if enabled.is_empty() {
                return Ok(());
            }
            if enabled
                .iter()
                .any(|component| matches!(component.execution, ExecutionConfig::Remote))
            {
                return Ok(());
            }
            return Err("enabled memory descriptor requires memory_store configuration".to_owned());
        };
        if enabled.len() != 1 {
            return Err(
                "memory_store configuration requires exactly one enabled memory descriptor"
                    .to_owned(),
            );
        }
        let descriptor = enabled[0];
        if descriptor.provider != "sqlite" {
            return Err("memory descriptor provider must match the sqlite store".to_owned());
        }
        if !matches!(descriptor.execution, ExecutionConfig::Local) {
            return Err("memory descriptor execution must be local".to_owned());
        }
        if !matches!(self.language.execution, ExecutionConfig::Local) {
            return Err("memory-enabled language execution must be local".to_owned());
        }
        if !store.database_path.is_absolute() {
            return Err("memory database path must be absolute".to_owned());
        }
        if !(1..=MAX_MEMORY_RETRIEVAL_ITEMS).contains(&store.max_items) {
            return Err("memory max_items must be within 1..=8".to_owned());
        }
        if !(1..=MAX_MEMORY_RETRIEVAL_BYTES).contains(&store.max_bytes) {
            return Err("memory max_bytes must be within 1..=8192".to_owned());
        }
        Ok(())
    }

    fn memory_provider(&self) -> Result<Option<SqliteMemoryContextProvider>, String> {
        self.validate_memory_configuration()?;
        let Some(config) = &self.memory_store else {
            return Ok(None);
        };
        let store =
            SqliteMemoryStore::open(&config.database_path).map_err(|error| error.to_string())?;
        let provider = SqliteMemoryContextProvider::new(store, Arc::new(SystemMemoryClock))
            .with_limits(config.max_items, config.max_bytes)
            .map_err(|error| error.to_string())?;
        Ok(Some(provider))
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

    fn quality_controller(&self) -> Result<ConversationQualityController, String> {
        Ok(ConversationQualityController::new(
            self.persona_profile()?,
            self.response_controls()?,
            self.response.mode.into(),
        ))
    }

    fn persona_profile(&self) -> Result<PersonaProfile, String> {
        Ok(PersonaProfile::new(
            persona_level(self.persona.warmth, "warmth")?,
            persona_level(self.persona.humor, "humor")?,
            persona_level(self.persona.teasing, "teasing")?,
            persona_level(self.persona.initiative, "initiative")?,
            persona_level(self.persona.directness, "directness")?,
            persona_level(self.persona.intimacy, "intimacy")?,
            persona_level(self.persona.verbosity, "verbosity")?,
            persona_level(self.persona.follow_up_frequency, "follow_up_frequency")?,
        ))
    }

    fn response_controls(&self) -> Result<ResponseControls, String> {
        if !self.response.allow_silence {
            return Err("response allow_silence must remain true".to_owned());
        }
        ResponseControls::new(
            self.response.maximum_spoken_seconds,
            persona_level(self.persona.directness, "directness")?,
            self.response.pace.into(),
            if self.response.ask_follow_up_by_default {
                FollowUpPolicy::Allowed
            } else {
                FollowUpPolicy::Contextual
            },
            SilencePolicy::AllowWithoutFiller,
        )
        .map_err(|error| error.message().to_owned())
    }

    fn validate_speech_mode(&self) -> Result<(), String> {
        match (self.speech.mode, self.speech.streaming_interval) {
            (SpeechMode::Buffered, None) => Ok(()),
            (SpeechMode::Buffered, Some(_)) => {
                Err("streaming_interval is only valid for streaming speech mode".to_owned())
            }
            (SpeechMode::Streaming, None) => {
                Err("streaming speech mode requires streaming_interval".to_owned())
            }
            (SpeechMode::Streaming, Some(interval))
                if interval.is_finite() && (0.10..=2.00).contains(&interval) =>
            {
                Ok(())
            }
            (SpeechMode::Streaming, Some(_)) => {
                Err("streaming_interval must be within 0.10..=2.00".to_owned())
            }
        }
    }

    fn speech_config(&self) -> Result<OpenAiCompatibleSpeechConfig, String> {
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
        Ok(config)
    }

    fn voice_io(&self) -> Result<MacOsVoiceSidecar, String> {
        let sidecar_executable = self.sidecar_executable()?;
        let config = MacOsVoiceSidecarConfig::new(
            sidecar_executable,
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

    fn sidecar_executable(&self) -> Result<PathBuf, String> {
        if let Some(executable) = &self.audio.sidecar_executable {
            return Ok(executable.clone());
        }
        let executable = std::env::current_exe()
            .map_err(|_| "failed to resolve the running voice-loop executable".to_owned())?;
        let directory = executable.parent().ok_or_else(|| {
            "failed to resolve the running voice-loop executable directory".to_owned()
        })?;
        Ok(directory.join(BUNDLED_SIDECAR_NAME))
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

impl From<ConversationModeConfig> for ConversationMode {
    fn from(value: ConversationModeConfig) -> Self {
        match value {
            ConversationModeConfig::DirectAnswer => Self::DirectAnswer,
            ConversationModeConfig::Companionship => Self::Companionship,
            ConversationModeConfig::Brainstorming => Self::Brainstorming,
            ConversationModeConfig::Reflective => Self::Reflective,
        }
    }
}

impl From<SpeechPaceConfig> for SpeechPace {
    fn from(value: SpeechPaceConfig) -> Self {
        match value {
            SpeechPaceConfig::Measured => Self::Measured,
            SpeechPaceConfig::Natural => Self::Natural,
            SpeechPaceConfig::Brisk => Self::Brisk,
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

fn runtime_message(error: RuntimeError) -> String {
    error.message().to_owned()
}

fn persona_level(value: f32, name: &str) -> Result<PersonaLevel, String> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(format!(
            "persona {name} must be finite and within 0.0..=1.0"
        ));
    }
    PersonaLevel::new((value * 100.0).round() as u8).map_err(|error| error.message().to_owned())
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
        let language_request =
            match LanguageModelRequest::from_input(turn_id, request.input().clone()) {
                Ok(request) => request,
                Err(error) => {
                    let _ = sender.try_send(Err(error));
                    return receiver;
                }
            };
        let mut deltas = self.inner.stream(language_request, cancellation.clone());

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
