use std::collections::BTreeSet;
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
use serde::{Deserialize, Serialize};

use crate::memory_extraction::MemoryExtractionSettings;
use crate::voice_adapters::{GatewayVoiceAdapters, VoicePolicyTemplate};

const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_MODEL_ID_BYTES: usize = 256;
const MAX_SYSTEM_PROMPT_BYTES: usize = 4 * 1024;
const MAX_PROVIDER_HOST_ID_BYTES: usize = 128;
/// Maximum UTF-8 byte length accepted for a provider readiness URL.
const MAX_PROVIDER_READINESS_URL_BYTES: usize = 2_048;
const MAX_PROVIDER_ARG_COUNT: usize = 32;
const MAX_PROVIDER_ARG_BYTES: usize = 4_096;
const MAX_PROVIDER_ARGV_BYTES: usize = 16_384;
const MIN_PROVIDER_STARTUP_TIMEOUT_MS: u64 = 100;
const MAX_PROVIDER_STARTUP_TIMEOUT_MS: u64 = 120_000;

#[derive(Debug)]
pub struct GatewayConfigError(String);

impl fmt::Display for GatewayConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for GatewayConfigError {}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    schema_version: u32,
    privacy_mode: GatewayPrivacyMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    provider_hosts: Vec<ProviderHost>,
    language: LanguageConfig,
    persona: PersonaConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory: Option<MemoryConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    voice: Option<VoiceConfig>,
}

pub struct GatewayAdapters {
    pub context: ConversationContext,
    pub language: Arc<dyn GenerationLanguageModel>,
    pub voice: Option<GatewayVoiceAdapters>,
    pub memory_store: Option<SqliteMemoryStore>,
    pub memory_extraction: Option<GatewayMemoryExtraction>,
    /// Provider processes the gateway supervises for this deployment, sorted by id.
    pub provider_hosts: Vec<ProviderHost>,
    pub status: RuntimeStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderHostOwnership {
    External,
    GatewayOwned,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderEnvironmentPolicy {
    Inherit,
    Clear,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderHost {
    id: String,
    ownership: ProviderHostOwnership,
    readiness_url: String,
    startup_timeout_ms: u64,
    environment: ProviderEnvironmentPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    executable: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    argv: Option<Vec<String>>,
}

impl ProviderHost {
    pub fn external(
        id: impl Into<String>,
        readiness_url: impl Into<String>,
        startup_timeout_ms: u64,
        environment: ProviderEnvironmentPolicy,
    ) -> Result<Self, GatewayConfigError> {
        let host = Self {
            id: id.into(),
            ownership: ProviderHostOwnership::External,
            readiness_url: readiness_url.into(),
            startup_timeout_ms,
            environment,
            executable: None,
            argv: None,
        };
        host.validate()?;
        Ok(host)
    }

    pub fn gateway_owned(
        id: impl Into<String>,
        readiness_url: impl Into<String>,
        startup_timeout_ms: u64,
        environment: ProviderEnvironmentPolicy,
        executable: impl Into<PathBuf>,
        argv: Vec<String>,
    ) -> Result<Self, GatewayConfigError> {
        let host = Self {
            id: id.into(),
            ownership: ProviderHostOwnership::GatewayOwned,
            readiness_url: readiness_url.into(),
            startup_timeout_ms,
            environment,
            executable: Some(executable.into()),
            argv: Some(argv),
        };
        host.validate()?;
        Ok(host)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn ownership(&self) -> ProviderHostOwnership {
        self.ownership
    }

    pub fn readiness_url(&self) -> &str {
        &self.readiness_url
    }

    pub const fn startup_timeout_ms(&self) -> u64 {
        self.startup_timeout_ms
    }

    pub const fn environment(&self) -> ProviderEnvironmentPolicy {
        self.environment
    }

    pub fn executable(&self) -> Option<&Path> {
        self.executable.as_deref()
    }

    pub fn argv(&self) -> Option<&[String]> {
        self.argv.as_deref()
    }

    fn validate(&self) -> Result<(), GatewayConfigError> {
        if self.id.is_empty()
            || self.id.trim() != self.id
            || self.id.len() > MAX_PROVIDER_HOST_ID_BYTES
        {
            return Err(config_error(
                "provider host id must be trimmed and within 1..=128 bytes",
            ));
        }
        if self.readiness_url.len() > MAX_PROVIDER_READINESS_URL_BYTES {
            return Err(config_error(
                "provider readiness URL exceeded 2048 UTF-8 bytes",
            ));
        }
        validate_loopback_http_endpoint(&self.readiness_url, "provider readiness")?;
        if !(MIN_PROVIDER_STARTUP_TIMEOUT_MS..=MAX_PROVIDER_STARTUP_TIMEOUT_MS)
            .contains(&self.startup_timeout_ms)
        {
            return Err(config_error(
                "provider startup_timeout_ms must be within 100..=120000",
            ));
        }
        match self.ownership {
            ProviderHostOwnership::External => {
                if self.executable.is_some() || self.argv.is_some() {
                    return Err(config_error(
                        "external provider hosts must not define executable or argv",
                    ));
                }
            }
            ProviderHostOwnership::GatewayOwned => {
                let executable = self.executable.as_deref().ok_or_else(|| {
                    config_error("gateway-owned provider host requires an executable")
                })?;
                if !executable.is_absolute() {
                    return Err(config_error(
                        "gateway-owned provider executable must be absolute",
                    ));
                }
                let argv = self
                    .argv
                    .as_deref()
                    .ok_or_else(|| config_error("gateway-owned provider host requires argv"))?;
                validate_provider_argv(argv)?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct LanguageDeployment {
    provider: String,
    endpoint: String,
    model: String,
    provider_host: String,
}

impl LanguageDeployment {
    pub fn ollama_compatible(
        provider: impl Into<String>,
        endpoint: impl Into<String>,
        model: impl Into<String>,
        provider_host: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            endpoint: endpoint.into(),
            model: model.into(),
            provider_host: provider_host.into(),
        }
    }
}

#[derive(Debug)]
pub struct GatewayDeploymentConfig {
    language: LanguageDeployment,
    provider_hosts: Vec<ProviderHost>,
}

impl GatewayDeploymentConfig {
    pub fn builder(language: LanguageDeployment) -> Self {
        Self {
            language,
            provider_hosts: Vec::new(),
        }
    }

    pub fn provider_host(mut self, provider_host: ProviderHost) -> Self {
        self.provider_hosts.push(provider_host);
        self
    }

    pub fn to_toml(self) -> Result<String, GatewayConfigError> {
        let mut config = GatewayConfig {
            schema_version: 2,
            privacy_mode: GatewayPrivacyMode::LocalOnly,
            provider_hosts: self.provider_hosts,
            language: LanguageConfig {
                backend: LanguageBackend::OllamaCompatible,
                execution: ExecutionConfig::Local,
                provider: self.language.provider,
                provider_host: Some(self.language.provider_host),
                endpoint: self.language.endpoint,
                model: self.language.model,
                system_prompt: None,
                thinking: false,
                temperature: 0.7,
                seed: 42,
                num_predict: 1024,
                num_ctx: 8192,
                max_assistant_content_bytes: 65_536,
            },
            persona: default_persona_config(),
            memory: None,
            voice: None,
        };
        config
            .provider_hosts
            .sort_by(|left, right| left.id.cmp(&right.id));
        let contents = toml::to_string_pretty(&config)
            .map_err(|_| config_error("gateway configuration could not be serialized"))?;
        if contents.len() as u64 > MAX_CONFIG_BYTES {
            return Err(config_error(
                "serialized gateway configuration exceeded 64 KiB",
            ));
        }
        let parsed: GatewayConfig = toml::from_str(&contents)
            .map_err(|_| config_error("gateway configuration could not be serialized"))?;
        parsed.build_adapters()?;
        Ok(contents)
    }
}

/// Extraction runs against its own model handle rather than the turn model's. The
/// adapter prepends the deployment's persona system prompt to every request it is
/// given, which would seat that prompt ahead of the extraction instruction and coax a
/// local model into answering in persona prose instead of the JSON array the parser
/// needs. This handle carries the same endpoint and model with no system prompt and a
/// temperature of zero.
pub struct GatewayMemoryExtraction {
    pub language: Arc<dyn GenerationLanguageModel>,
    pub settings: MemoryExtractionSettings,
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
    pub fn provider_hosts(&self) -> &[ProviderHost] {
        &self.provider_hosts
    }

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
        let provider_hosts = self.validate_provider_hosts()?;
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
        let memory_extraction = self
            .memory
            .as_ref()
            .and_then(|memory| memory.extraction.as_ref())
            .map(|extraction| self.extraction_adapters(extraction))
            .transpose()?;
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
        let mut capabilities = vec![
            "text".to_owned(),
            "conversation_context_seed".to_owned(),
            "persona_control".to_owned(),
        ];
        if memory_enabled {
            capabilities.push("memory_inspection".to_owned());
            capabilities.push("memory_mutation".to_owned());
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
            last_context_seed_operation_id: None,
        };

        Ok(GatewayAdapters {
            context,
            language,
            voice,
            memory_store,
            memory_extraction,
            provider_hosts,
            status,
        })
    }

    fn validate_provider_hosts(&self) -> Result<Vec<ProviderHost>, GatewayConfigError> {
        match self.schema_version {
            1 => {
                if !self.provider_hosts.is_empty()
                    || self.language.provider_host.is_some()
                    || self
                        .voice
                        .as_ref()
                        .and_then(|voice| voice.speech.provider_host.as_ref())
                        .is_some()
                {
                    return Err(config_error(
                        "schema v1 uses legacy external providers without provider hosts",
                    ));
                }
                Ok(Vec::new())
            }
            2 => {
                let mut ids = BTreeSet::new();
                for host in &self.provider_hosts {
                    host.validate()?;
                    if !ids.insert(host.id.as_str()) {
                        return Err(config_error("provider host ids must be unique"));
                    }
                }
                let language_host = self.language.provider_host.as_deref().ok_or_else(|| {
                    config_error("schema v2 language must reference a provider host")
                })?;
                if !ids.contains(language_host) {
                    return Err(config_error(
                        "schema v2 language provider host was not declared",
                    ));
                }
                if let Some(voice) = self.voice.as_ref() {
                    let speech_host = voice.speech.provider_host.as_deref().ok_or_else(|| {
                        config_error("schema v2 voice speech must reference a provider host")
                    })?;
                    if !ids.contains(speech_host) {
                        return Err(config_error(
                            "schema v2 voice speech provider host was not declared",
                        ));
                    }
                }
                let mut hosts = self.provider_hosts.clone();
                hosts.sort_by(|left, right| left.id.cmp(&right.id));
                Ok(hosts)
            }
            _ => Err(config_error(
                "gateway configuration schema_version must be 1 or 2",
            )),
        }
    }

    fn language_config(&self) -> Result<OllamaConfig, GatewayConfigError> {
        let mut language = self.base_language_config()?;
        if let Some(system_prompt) = self.language.system_prompt.as_deref() {
            if system_prompt.trim().is_empty() {
                return Err(config_error("language system_prompt cannot be empty"));
            }
            if system_prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
                return Err(config_error("language system_prompt exceeded 4 KiB"));
            }
            language = language.with_system_prompt(system_prompt);
        }
        Ok(language)
    }

    fn extraction_adapters(
        &self,
        extraction: &MemoryExtractionConfig,
    ) -> Result<GatewayMemoryExtraction, GatewayConfigError> {
        Ok(GatewayMemoryExtraction {
            language: Arc::new(OllamaLanguageModel::new_direct(
                self.base_language_config()?.with_temperature(0.0),
            )),
            settings: extraction.settings()?,
        })
    }

    /// Everything the deployment configured for the language model except its system
    /// prompt, which only the conversation model wants.
    fn base_language_config(&self) -> Result<OllamaConfig, GatewayConfigError> {
        if self.language.model.len() > MAX_MODEL_ID_BYTES {
            return Err(config_error("language model identifier exceeded 256 bytes"));
        }
        if !self.language.temperature.is_finite() || self.language.temperature < 0.0 {
            return Err(config_error(
                "language temperature must be finite and non-negative",
            ));
        }
        OllamaConfig::new(&self.language.model)
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
            .map_err(adapter_error)
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
            persona.maximum_spoken_seconds(),
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

fn validate_provider_argv(argv: &[String]) -> Result<(), GatewayConfigError> {
    if argv.len() > MAX_PROVIDER_ARG_COUNT {
        return Err(config_error(
            "gateway-owned provider argv exceeded 32 arguments",
        ));
    }
    let mut aggregate_bytes = 0_usize;
    for argument in argv {
        if argument.len() > MAX_PROVIDER_ARG_BYTES {
            return Err(config_error(
                "gateway-owned provider argument exceeded 4096 bytes",
            ));
        }
        if argument.contains('\0') {
            return Err(config_error(
                "gateway-owned provider arguments must be literal strings",
            ));
        }
        aggregate_bytes = aggregate_bytes
            .checked_add(argument.len())
            .ok_or_else(|| config_error("gateway-owned provider argv exceeded 16384 bytes"))?;
    }
    if aggregate_bytes > MAX_PROVIDER_ARGV_BYTES {
        return Err(config_error(
            "gateway-owned provider argv exceeded 16384 bytes",
        ));
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

fn default_persona_config() -> PersonaConfig {
    PersonaConfig {
        mode: GatewayConversationMode::DirectAnswer,
        warmth: 80,
        humor: 60,
        teasing: 40,
        initiative: 35,
        directness: 80,
        intimacy: 30,
        verbosity: 20,
        follow_up_frequency: 25,
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum GatewayPrivacyMode {
    LocalOnly,
    Hybrid,
    Cloud,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LanguageConfig {
    backend: LanguageBackend,
    execution: ExecutionConfig,
    provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_host: Option<String>,
    endpoint: String,
    model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    system_prompt: Option<String>,
    thinking: bool,
    temperature: f32,
    seed: u64,
    num_predict: usize,
    num_ctx: usize,
    max_assistant_content_bytes: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum LanguageBackend {
    OllamaCompatible,
}

#[derive(Debug, Deserialize, Serialize)]
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

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MemoryConfig {
    database: PathBuf,
    maximum_items: usize,
    maximum_bytes: usize,
    // Nesting extraction under `[memory]` is what makes `[memory.extraction]` alone
    // invalid: without the rest of `[memory]` there is no store to write into.
    extraction: Option<MemoryExtractionConfig>,
}

const MAXIMUM_MEMORIES_PER_TURN: usize = 5;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MemoryExtractionConfig {
    #[serde(default = "default_max_memories_per_turn")]
    max_memories_per_turn: usize,
    #[serde(default = "default_episodic_retention_days")]
    episodic_retention_days: u16,
}

impl MemoryExtractionConfig {
    fn settings(&self) -> Result<MemoryExtractionSettings, GatewayConfigError> {
        if !(1..=MAXIMUM_MEMORIES_PER_TURN).contains(&self.max_memories_per_turn) {
            return Err(config_error(
                "memory extraction max_memories_per_turn must be 1 through 5",
            ));
        }
        if self.episodic_retention_days == 0 {
            return Err(config_error(
                "memory extraction episodic_retention_days must be at least 1",
            ));
        }
        Ok(MemoryExtractionSettings::new(
            self.max_memories_per_turn,
            self.episodic_retention_days,
        ))
    }
}

const fn default_max_memories_per_turn() -> usize {
    3
}

const fn default_episodic_retention_days() -> u16 {
    90
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VoiceConfig {
    capture: VoiceCaptureConfig,
    turn: VoiceTurnConfig,
    asr: VoiceAsrConfig,
    speech: VoiceSpeechConfig,
    audio: VoiceAudioConfig,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VoiceCaptureConfig {
    device: VoiceCaptureDevice,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum VoiceCaptureDevice {
    SystemDefault,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VoiceTurnConfig {
    speech_start_ms: u64,
    final_silence_ms: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VoiceAsrConfig {
    backend: VoiceAsrBackend,
    execution: ExecutionConfig,
    provider: String,
    model_path: PathBuf,
    download: bool,
    language: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum VoiceAsrBackend {
    Whisperkit,
    Sensevoice,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VoiceSpeechConfig {
    backend: VoiceSpeechBackend,
    execution: ExecutionConfig,
    provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider_host: Option<String>,
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

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum VoiceSpeechBackend {
    OpenaiCompatible,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum VoiceSpeechMode {
    Buffered,
    Streaming,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VoiceAudioConfig {
    backend: VoiceAudioBackend,
    execution: ExecutionConfig,
    provider: String,
    sidecar_executable: PathBuf,
    max_error_bytes: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum VoiceAudioBackend {
    ManagedSidecar,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ExecutionConfig {
    Local,
    Remote,
}
