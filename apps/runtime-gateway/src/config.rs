use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use conversation_memory::{SqliteMemoryContextProvider, SqliteMemoryStore, SystemMemoryClock};
use conversation_model_adapters::{
    BufferedStreamingSpeechSynthesizer, GenerationLanguageModel, MacOsVoiceSidecar,
    MacOsVoiceSidecarConfig, OllamaConfig, OllamaLanguageModel, OpenAiCompatibleSpeechConfig,
    OpenAiCompatibleSpeechSynthesizer, OpenAiCompatibleStreamingSpeechConfig,
    OpenAiCompatibleStreamingSpeechSynthesizer, OpenAiDiarizationConfig, OpenAiLanguageConfig,
    OpenAiLanguageModel, OpenAiSpeechRecognitionConfig, OpenAiTranscriptionDiarizer,
    SidecarAsrBackend, SpeechSynthesizer, SpeechbrainDiarizationConfig,
    SpeechbrainTranscriptionDiarizer, StreamingSpeechSynthesizer, SystemDevice,
    TranscriptionDiarizer, VoiceIoFactory,
};
use conversation_protocol::{
    ClientComponentDescriptor, ComponentDescriptor, ComponentKind, ConversationMode,
    ExecutionLocation, FollowUpPolicy, PersonaLevel, PersonaProfile,
    PrivacyMode as ProtocolPrivacyMode, ResponseControls, RuntimeProfile, RuntimeStatus,
    SilencePolicy, SpeechPace, VoiceResponseMode, MAX_CLIENT_PROVIDER_LABEL_BYTES,
};
use conversation_runtime::{ConversationContext, ConversationQualityController, RecordingSettings};
use serde::{Deserialize, Serialize};

use crate::media_server::{
    MediaServerConfig, MediaTlsConfig, DEFAULT_MEDIA_PACKET_READ_TIMEOUT_MS,
};
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
const MAX_VOICE_ID_BYTES: usize = 128;
/// Maximum UTF-8 byte length accepted for the *name* of an environment
/// variable holding a credential. The value of that variable is never read
/// into this crate's data structures.
const MAX_CREDENTIAL_ENV_NAME_BYTES: usize = 128;
const MAX_REMOTE_ENDPOINT_BYTES: usize = 2_048;
/// The longest legal DNS name, so a remote endpoint host stays bounded.
const MAX_REMOTE_ENDPOINT_HOST_BYTES: usize = 253;
const MAX_RECORDING_DIRECTORY_BYTES: usize = 1_024;
/// The diarization port refuses a result carrying more clusters than this, so a larger
/// configured maximum would configure a failure.
const MAX_DIARIZATION_SPEAKERS: u16 = 64;
const MIN_DIARIZATION_SPEAKERS: u16 = 2;

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
    /// Absent selects `offline`. Only schema version 3 may name a profile at
    /// all, so a version 1 or 2 file keeps exactly the meaning it had before
    /// profiles existed.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_runtime_profile"
    )]
    profile: Option<RuntimeProfile>,
    privacy_mode: GatewayPrivacyMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    provider_hosts: Vec<ProviderHost>,
    language: LanguageConfig,
    persona: PersonaConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory: Option<MemoryConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    voice: Option<VoiceConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recording: Option<RecordingConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    media: Option<MediaConfig>,
}

pub struct GatewayAdapters {
    pub context: ConversationContext,
    pub language: Arc<dyn GenerationLanguageModel>,
    pub voice: Option<GatewayVoiceAdapters>,
    pub memory_store: Option<SqliteMemoryStore>,
    pub memory_extraction: Option<GatewayMemoryExtraction>,
    /// Provider processes the gateway supervises for this deployment, sorted by id.
    pub provider_hosts: Vec<ProviderHost>,
    /// Where voice sessions write their encrypted recording, when `[recording]` is configured.
    /// Absent in the default configuration, which records nothing.
    pub recording: Option<RecordingSettings>,
    /// What reconciles a finished recording, when `[voice.diarization]` is configured. Absent
    /// in the default configuration, which reconciles nothing.
    pub diarizer: Option<Arc<dyn TranscriptionDiarizer>>,
    /// Optional transport listener configuration. `main` owns the resulting server;
    /// configuration loading never binds a socket.
    pub media: Option<MediaServerConfig>,
    pub status: RuntimeStatus,
}

/// The three remote transports that are replaced only while a crate unit test assembles real
/// adapters against deterministic loopback providers. Deployment TOML is still parsed and
/// validated with its configured endpoints before this is used.
#[derive(Clone, Copy)]
struct ProviderEndpointOverrides<'a> {
    language: &'a str,
    recognition: &'a str,
    speech: &'a str,
}

/// Loopback transport targets for a loader-built adapter set. This exists only in the lib-test
/// build, so normal gateway binaries and integration-test library artifacts cannot select it.
#[cfg(test)]
pub(crate) struct GatewayTestProviderEndpoints {
    language: String,
    recognition: String,
    speech: String,
}

#[cfg(test)]
impl GatewayTestProviderEndpoints {
    pub(crate) fn new(language: String, recognition: String, speech: String) -> Self {
        Self {
            language,
            recognition,
            speech,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MediaConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capture_format: Option<String>,
    bind: SocketAddr,
    advertised_endpoint: String,
    credential_lifetime_ms: u64,
    reconnect_window_ms: u64,
    max_frames_per_second: u32,
    max_payload_bytes: usize,
    max_outbound_queue_bytes: usize,
    max_outbound_queue_ms: u64,
    #[serde(default = "default_packet_read_timeout_ms")]
    packet_read_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tls_certificate_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tls_private_key_path: Option<PathBuf>,
}

impl MediaConfig {
    fn server_config(&self) -> Result<MediaServerConfig, GatewayConfigError> {
        if self.reconnect_window_ms > self.credential_lifetime_ms {
            return Err(config_error(
                "media reconnect window must not exceed credential lifetime",
            ));
        }
        let tls = match (&self.tls_certificate_path, &self.tls_private_key_path) {
            (Some(certificate), Some(private_key)) => Some(
                MediaTlsConfig::new(certificate.clone(), private_key.clone())
                    .map_err(adapter_error)?,
            ),
            (None, None) => None,
            _ => {
                return Err(config_error(
                    "media TLS certificate and private key must be configured together",
                ))
            }
        };
        MediaServerConfig::new(
            self.bind,
            self.advertised_endpoint.clone(),
            std::time::Duration::from_millis(self.credential_lifetime_ms),
            std::time::Duration::from_millis(self.reconnect_window_ms),
            self.max_frames_per_second,
            self.max_payload_bytes,
            self.max_outbound_queue_bytes,
            std::time::Duration::from_millis(self.max_outbound_queue_ms),
            std::time::Duration::from_millis(self.packet_read_timeout_ms),
            tls,
        )
        .and_then(|config| config.with_capture_format(self.capture_format.clone()))
        .map_err(adapter_error)
    }
}

const fn default_packet_read_timeout_ms() -> u64 {
    DEFAULT_MEDIA_PACKET_READ_TIMEOUT_MS
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
            profile: None,
            privacy_mode: GatewayPrivacyMode::LocalOnly,
            provider_hosts: self.provider_hosts,
            language: LanguageConfig {
                backend: LanguageBackend::OllamaCompatible,
                execution: ExecutionConfig::Local,
                provider: self.language.provider,
                provider_host: Some(self.language.provider_host),
                endpoint: self.language.endpoint,
                model: self.language.model,
                credential_env: None,
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
            recording: None,
            media: None,
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

    #[cfg(test)]
    pub(crate) fn load_with_test_provider_endpoints(
        path: &Path,
        endpoints: GatewayTestProviderEndpoints,
    ) -> Result<GatewayAdapters, GatewayConfigError> {
        let parsed = load_toml(path)?;
        parsed.build_adapters_with_provider_endpoints(Some(ProviderEndpointOverrides {
            language: &endpoints.language,
            recognition: &endpoints.recognition,
            speech: &endpoints.speech,
        }))
    }

    /// Parse and fully validate a configuration file, returning the runtime
    /// status the deployment will report. Adapters are deliberately not
    /// constructed: an online deployment names remote providers whose adapters
    /// are wired by the runtime, not by this loader.
    pub fn validate(path: &Path) -> Result<RuntimeStatus, GatewayConfigError> {
        Ok(load_toml(path)?.deployment_plan()?.status)
    }

    const fn profile(&self) -> RuntimeProfile {
        match self.profile {
            Some(profile) => profile,
            None => RuntimeProfile::Offline,
        }
    }

    /// Everything a deployment needs that can be decided from the file alone:
    /// which provider processes are supervised, which components run where,
    /// and the status the gateway will state. Every profile-dependent rule
    /// lives here so `validate` and `build_adapters` cannot drift apart.
    fn deployment_plan(&self) -> Result<DeploymentPlan, GatewayConfigError> {
        let provider_hosts = self.validate_provider_hosts()?;
        let profile = self.profile();
        match (profile, self.privacy_mode) {
            (RuntimeProfile::Offline, GatewayPrivacyMode::LocalOnly)
            | (RuntimeProfile::Online, GatewayPrivacyMode::Cloud) => {}
            (RuntimeProfile::Offline, _) => {
                return Err(config_error(
                    "offline gateway privacy_mode must be local-only",
                ));
            }
            (RuntimeProfile::Online, _) => {
                return Err(config_error("online gateway privacy_mode must be cloud"));
            }
            _ => return Err(config_error("gateway profile is not supported")),
        }
        let profile_execution = profile_execution(profile);

        require_declared_execution(
            self.language.execution,
            language_execution(self.language.backend),
            profile_execution,
            "language",
        )?;
        validate_provider_label(&self.language.provider, "language")?;
        validate_endpoint(&self.language.endpoint, self.language.execution, "language")?;
        validate_credential_reference(
            self.language.credential_env.as_deref(),
            self.language.execution,
            "language",
        )?;
        self.validate_language_tuning()?;

        let quality = self.quality_controller()?;
        let memory = self.memory.as_ref().map(memory_adapters).transpose()?;
        let language_component = component_descriptor(
            ComponentKind::LanguageModel,
            &self.language.provider,
            self.language.execution,
            "language",
        )?;
        let memory_component = memory.as_ref().map(|_| {
            ComponentDescriptor::new(ComponentKind::Memory, "sqlite", ExecutionLocation::Local)
        });
        if self
            .voice
            .as_ref()
            .is_some_and(|voice| voice.diarization.is_some())
            && self.recording.is_none()
        {
            return Err(config_error(
                "voice diarization requires a recording section",
            ));
        }
        let voice_configured = self.voice.is_some();
        let components = match self.voice.as_ref() {
            Some(voice) => voice.validate(
                profile_execution,
                language_component,
                memory_component.clone(),
            )?,
            None => {
                let mut components = vec![language_component];
                components.extend(memory_component.clone());
                components
            }
        };
        if let Some(recording) = self.recording.as_ref() {
            recording.validate()?;
        }
        if profile == RuntimeProfile::Online && voice_configured && self.media.is_none() {
            return Err(config_error(
                "online voice requires a media bridge configuration",
            ));
        }
        let media = self
            .media
            .as_ref()
            .map(MediaConfig::server_config)
            .transpose()?;

        let memory_enabled = memory.is_some();
        let mut capabilities = vec![
            "text".to_owned(),
            "conversation_context_seed".to_owned(),
            "persona_control".to_owned(),
        ];
        if memory_enabled {
            capabilities.push("memory_inspection".to_owned());
            capabilities.push("memory_mutation".to_owned());
        }
        if voice_configured {
            capabilities.push("voice_session".to_owned());
        }
        let status = RuntimeStatus {
            profile: profile.as_str().to_owned(),
            transport: "stdio".to_owned(),
            privacy_mode: self.privacy_mode.protocol_name().to_owned(),
            language_location: execution_name(self.language.execution).to_owned(),
            model_id: self.language.model.clone(),
            memory_enabled,
            memory_location: memory_enabled.then(|| "local".to_owned()),
            telemetry_enabled: false,
            capabilities,
            components: components
                .iter()
                .map(ClientComponentDescriptor::from)
                .collect(),
            last_context_seed_operation_id: None,
        };

        Ok(DeploymentPlan {
            provider_hosts,
            components,
            memory,
            quality,
            media,
            status,
        })
    }

    fn build_adapters(&self) -> Result<GatewayAdapters, GatewayConfigError> {
        self.build_adapters_with_provider_endpoints(None)
    }

    fn build_adapters_with_provider_endpoints(
        &self,
        provider_endpoints: Option<ProviderEndpointOverrides<'_>>,
    ) -> Result<GatewayAdapters, GatewayConfigError> {
        let DeploymentPlan {
            provider_hosts,
            components,
            memory,
            quality,
            media,
            status,
        } = self.deployment_plan()?;
        let language: Arc<dyn GenerationLanguageModel> = match self.language.backend {
            LanguageBackend::OllamaCompatible => {
                Arc::new(OllamaLanguageModel::new_direct(self.language_config()?))
            }
            LanguageBackend::OpenaiCompatible => {
                let credential = self.language.credential_env.as_deref().ok_or_else(|| {
                    config_error("remote language requires a credential environment variable")
                })?;
                let mut config = OpenAiLanguageConfig::new(
                    provider_endpoints.map_or(self.language.endpoint.as_str(), |endpoints| {
                        endpoints.language
                    }),
                    &self.language.model,
                    credential,
                )
                .map_err(adapter_error)?
                .with_temperature(self.language.temperature)
                .map_err(adapter_error)?
                .with_max_output_tokens(self.language.num_predict)
                .map_err(adapter_error)?
                .with_max_output_bytes(self.language.max_assistant_content_bytes)
                .map_err(adapter_error)?;
                if let Some(system_prompt) = self.language.system_prompt.as_deref() {
                    config = config
                        .with_system_prompt(system_prompt)
                        .map_err(adapter_error)?;
                }
                Arc::new(OpenAiLanguageModel::new(config))
            }
        };
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

        let voice = self
            .voice
            .as_ref()
            .map(|voice| voice.build(self.privacy_mode.into(), components, provider_endpoints))
            .transpose()?;

        let recording = self
            .recording
            .as_ref()
            .map(RecordingConfig::settings)
            .transpose()?;
        let diarizer = self
            .voice
            .as_ref()
            .and_then(|voice| voice.diarization.as_ref())
            .map(VoiceDiarizationConfig::diarizer)
            .transpose()?;

        Ok(GatewayAdapters {
            context,
            language,
            voice,
            memory_store,
            memory_extraction,
            provider_hosts,
            recording,
            diarizer,
            media,
            status,
        })
    }

    fn validate_language_tuning(&self) -> Result<(), GatewayConfigError> {
        validate_identifier(&self.language.model, MAX_MODEL_ID_BYTES, "language model")?;
        if !self.language.temperature.is_finite() || self.language.temperature < 0.0 {
            return Err(config_error(
                "language temperature must be finite and non-negative",
            ));
        }
        if let Some(system_prompt) = self.language.system_prompt.as_deref() {
            if system_prompt.trim().is_empty() {
                return Err(config_error("language system_prompt cannot be empty"));
            }
            if system_prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
                return Err(config_error("language system_prompt exceeded 4 KiB"));
            }
        }
        Ok(())
    }

    /// Schema versions 1 and 2 predate profiles, credential references, the
    /// response mode, and recording, so naming any of them is an error there
    /// rather than a silently ignored key.
    fn require_legacy_schema_keys_absent(&self) -> Result<(), GatewayConfigError> {
        let voice_keys = self.voice.as_ref().is_some_and(|voice| {
            voice.response_mode.is_some()
                || voice.diarization.is_some()
                || voice.asr.model.is_some()
                || voice.asr.endpoint.is_some()
                || voice.asr.credential_env.is_some()
                || voice.asr.server_vad.is_some()
                || voice.speech.credential_env.is_some()
        });
        if self.profile.is_some()
            || self.language.credential_env.is_some()
            || self.recording.is_some()
            || self.media.is_some()
            || voice_keys
        {
            return Err(config_error(
                "profile, credential_env, response_mode, recording, and media require schema_version 3",
            ));
        }
        Ok(())
    }

    fn validate_provider_hosts(&self) -> Result<Vec<ProviderHost>, GatewayConfigError> {
        match self.schema_version {
            1 => {
                self.require_legacy_schema_keys_absent()?;
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
                self.require_legacy_schema_keys_absent()?;
                let ids = self.declared_host_ids()?;
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
                Ok(self.sorted_provider_hosts())
            }
            3 => {
                let ids = self.declared_host_ids()?;
                require_host_reference(
                    self.language.provider_host.as_deref(),
                    self.language.execution,
                    &ids,
                    "language",
                )?;
                if let Some(voice) = self.voice.as_ref() {
                    require_host_reference(
                        voice.speech.provider_host.as_deref(),
                        voice.speech.execution,
                        &ids,
                        "voice speech",
                    )?;
                }
                Ok(self.sorted_provider_hosts())
            }
            _ => Err(config_error(
                "gateway configuration schema_version must be 1, 2, or 3",
            )),
        }
    }

    fn declared_host_ids(&self) -> Result<BTreeSet<&str>, GatewayConfigError> {
        let mut ids = BTreeSet::new();
        for host in &self.provider_hosts {
            host.validate()?;
            if !ids.insert(host.id.as_str()) {
                return Err(config_error("provider host ids must be unique"));
            }
        }
        Ok(ids)
    }

    fn sorted_provider_hosts(&self) -> Vec<ProviderHost> {
        let mut hosts = self.provider_hosts.clone();
        hosts.sort_by(|left, right| left.id.cmp(&right.id));
        hosts
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
    /// Validates the voice lane and returns its component descriptors in the
    /// rank order `validate_runtime_status` requires.
    fn validate(
        &self,
        profile_execution: ExecutionConfig,
        language: ComponentDescriptor,
        memory: Option<ComponentDescriptor>,
    ) -> Result<Vec<ComponentDescriptor>, GatewayConfigError> {
        // Exhaustive over the capture and audio backends: adding one is a
        // compile error here until this file decides what it means.
        match (self.capture.device, self.audio.backend) {
            (VoiceCaptureDevice::SystemDefault, VoiceAudioBackend::ManagedSidecar) => {}
        }
        require_declared_execution(
            self.asr.execution,
            asr_execution(self.asr.backend),
            profile_execution,
            "voice ASR",
        )?;
        require_declared_execution(
            self.speech.execution,
            speech_execution(self.speech.backend, profile_execution),
            profile_execution,
            "voice speech",
        )?;
        // Audio capture and playback stay on the local sidecar in every
        // profile; nothing in this repository moves them off the machine.
        require_execution(self.audio.execution, ExecutionConfig::Local, "voice audio")?;

        match self.asr.execution {
            ExecutionConfig::Local => {
                if self.asr.server_vad.is_some() {
                    return Err(config_error(
                        "local voice ASR must not configure remote server VAD",
                    ));
                }
                if self.asr.endpoint.is_some() {
                    return Err(config_error("local voice ASR must not name an endpoint"));
                }
                if self.asr.model.is_some() {
                    return Err(config_error(
                        "local voice ASR must not name a remote transcription model",
                    ));
                }
                let model_path = self
                    .asr
                    .model_path
                    .as_deref()
                    .ok_or_else(|| config_error("local voice ASR requires a model path"))?;
                let download = self
                    .asr
                    .download
                    .ok_or_else(|| config_error("local voice ASR requires a download flag"))?;
                if download {
                    return Err(config_error("voice ASR model download must be disabled"));
                }
                if !model_path.is_absolute() {
                    return Err(config_error("voice ASR model path must be absolute"));
                }
                if !model_path.is_dir() {
                    return Err(config_error(
                        "voice ASR model path must be an existing directory",
                    ));
                }
            }
            ExecutionConfig::Remote => {
                if self.asr.model_path.is_some() || self.asr.download.is_some() {
                    return Err(config_error(
                        "streaming online voice ASR must not configure local model material",
                    ));
                }
                let model =
                    self.asr.model.as_deref().ok_or_else(|| {
                        config_error("streaming online voice ASR requires a model")
                    })?;
                validate_identifier(model, MAX_MODEL_ID_BYTES, "voice ASR model")?;
                let endpoint = self.asr.endpoint.as_deref().ok_or_else(|| {
                    config_error("streaming online voice ASR requires an endpoint")
                })?;
                validate_transcription_endpoint(endpoint)?;
                self.asr.online_recognition(endpoint)?;
            }
        }
        validate_credential_reference(
            self.asr.credential_env.as_deref(),
            self.asr.execution,
            "voice ASR",
        )?;

        validate_endpoint(&self.speech.endpoint, self.speech.execution, "voice speech")?;
        validate_credential_reference(
            self.speech.credential_env.as_deref(),
            self.speech.execution,
            "voice speech",
        )?;
        if self.speech.execution == ExecutionConfig::Remote
            && self.speech.mode != VoiceSpeechMode::Streaming
        {
            return Err(config_error("remote voice speech requires streaming mode"));
        }
        validate_identifier(&self.speech.model, MAX_MODEL_ID_BYTES, "voice speech model")?;
        validate_identifier(&self.speech.voice, MAX_VOICE_ID_BYTES, "voice speech voice")?;
        if !self.audio.sidecar_executable.is_absolute() {
            return Err(config_error(
                "voice audio sidecar executable must be absolute",
            ));
        }

        let recognition = component_descriptor(
            ComponentKind::SpeechRecognition,
            &self.asr.provider,
            self.asr.execution,
            "voice ASR",
        )?;
        let speech = component_descriptor(
            ComponentKind::SpeechSynthesis,
            &self.speech.provider,
            self.speech.execution,
            "voice speech",
        )?;
        let audio = component_descriptor(
            ComponentKind::AudioIo,
            &self.audio.provider,
            self.audio.execution,
            "voice audio",
        )?;
        let mut components = vec![recognition, language, speech, audio];
        components.extend(memory);
        // Diarization ranks after memory, so it is appended last and every existing component
        // keeps the position the wire already expects.
        if let Some(diarization) = self.diarization.as_ref() {
            components.push(diarization.validate(profile_execution)?);
        }
        Ok(components)
    }

    fn build(
        &self,
        privacy_mode: ProtocolPrivacyMode,
        components: Vec<ComponentDescriptor>,
        provider_endpoints: Option<ProviderEndpointOverrides<'_>>,
    ) -> Result<GatewayVoiceAdapters, GatewayConfigError> {
        let policy = VoicePolicyTemplate::new(
            privacy_mode,
            self.turn.speech_start_ms,
            self.turn.final_silence_ms,
            components,
        )
        .map_err(runtime_error)?;

        let (io, online_recognition) = match self.asr.backend {
            VoiceAsrBackend::Whisperkit | VoiceAsrBackend::Sensevoice => {
                let model_path = self
                    .asr
                    .model_path
                    .as_deref()
                    .ok_or_else(|| config_error("local voice ASR requires a model path"))?;
                let download = self
                    .asr
                    .download
                    .ok_or_else(|| config_error("local voice ASR requires a download flag"))?;
                let mut sidecar = MacOsVoiceSidecarConfig::new(
                    &self.audio.sidecar_executable,
                    model_path,
                    SystemDevice::SystemDefault,
                    download,
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
                (
                    Arc::new(MacOsVoiceSidecar::new(sidecar)) as Arc<dyn VoiceIoFactory>,
                    None,
                )
            }
            VoiceAsrBackend::OpenaiStreaming => {
                let endpoint = provider_endpoints
                    .map(|endpoints| endpoints.recognition)
                    .or(self.asr.endpoint.as_deref())
                    .ok_or_else(|| {
                        config_error("streaming online voice ASR requires an endpoint")
                    })?;
                let recognition = self.asr.online_recognition(endpoint)?;
                (
                    GatewayVoiceAdapters::unavailable_online_io(),
                    Some(recognition),
                )
            }
        };

        Ok(GatewayVoiceAdapters {
            io,
            speech: self.speech.synthesizer(
                provider_endpoints
                    .map_or(self.speech.endpoint.as_str(), |endpoints| endpoints.speech),
            )?,
            policy,
            online_recognition,
        })
    }
}

impl VoiceSpeechConfig {
    fn synthesizer(
        &self,
        endpoint: &str,
    ) -> Result<Arc<dyn StreamingSpeechSynthesizer>, GatewayConfigError> {
        let speech = OpenAiCompatibleSpeechConfig::new(&self.model)
            .map_err(adapter_error)?
            .with_endpoint(endpoint)
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
                let mut streaming = OpenAiCompatibleStreamingSpeechConfig::new(speech, interval)
                    .map_err(adapter_error)?;
                if let Some(credential) = self.credential_env.as_deref() {
                    streaming = streaming
                        .with_credential_env(credential)
                        .map_err(adapter_error)?;
                }
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

/// A remote provider endpoint. Unlike a supervised loopback process this is a
/// third-party API, so the rule is transport security and the absence of any
/// credential material in the URL itself rather than a loopback address.
fn validate_remote_https_endpoint(
    endpoint: &str,
    component: &str,
) -> Result<(), GatewayConfigError> {
    if endpoint.len() > MAX_REMOTE_ENDPOINT_BYTES {
        return Err(config_error(format!(
            "{component} endpoint exceeded 2048 UTF-8 bytes"
        )));
    }
    let endpoint = reqwest::Url::parse(endpoint)
        .map_err(|_| config_error(format!("{component} endpoint must be a valid URL")))?;
    if endpoint.scheme() != "https" {
        return Err(config_error(format!("{component} endpoint must use HTTPS")));
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
    let host = endpoint
        .host_str()
        .ok_or_else(|| config_error(format!("{component} endpoint must name a host")))?;
    if host.is_empty() || host.len() > MAX_REMOTE_ENDPOINT_HOST_BYTES {
        return Err(config_error(format!(
            "{component} endpoint host must be within 1..=253 bytes"
        )));
    }
    if host
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
    {
        return Err(config_error(format!(
            "{component} endpoint must not be a loopback address"
        )));
    }
    Ok(())
}

fn validate_endpoint(
    endpoint: &str,
    execution: ExecutionConfig,
    component: &str,
) -> Result<(), GatewayConfigError> {
    match execution {
        ExecutionConfig::Local => validate_loopback_http_endpoint(endpoint, component),
        ExecutionConfig::Remote => validate_remote_https_endpoint(endpoint, component),
    }
}

fn validate_transcription_endpoint(endpoint: &str) -> Result<(), GatewayConfigError> {
    if endpoint.len() > MAX_REMOTE_ENDPOINT_BYTES {
        return Err(config_error("voice ASR endpoint exceeded 2048 UTF-8 bytes"));
    }
    let endpoint = reqwest::Url::parse(endpoint)
        .map_err(|_| config_error("voice ASR endpoint must be a valid URL"))?;
    if endpoint.scheme() != "wss"
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(config_error(
            "voice ASR endpoint must use WSS without credentials or fragments",
        ));
    }
    let host = endpoint
        .host_str()
        .ok_or_else(|| config_error("voice ASR endpoint must name a host"))?;
    if host.is_empty()
        || host.len() > MAX_REMOTE_ENDPOINT_HOST_BYTES
        || host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    {
        return Err(config_error(
            "voice ASR endpoint must name a remote host within 253 bytes",
        ));
    }
    // Preserve the complete configured query, including intent=transcription.
    Ok(())
}

fn require_execution(
    execution: ExecutionConfig,
    expected: ExecutionConfig,
    component: &str,
) -> Result<(), GatewayConfigError> {
    if execution == expected {
        Ok(())
    } else {
        Err(config_error(format!(
            "{component} execution must be {}",
            execution_name(expected)
        )))
    }
}

/// The declared `execution`, the backend's own locality, and the profile's
/// locality must all agree. An offline profile therefore still refuses every
/// remote component, while an online profile refuses a component the operator
/// left pointing at a local provider.
fn require_declared_execution(
    declared: ExecutionConfig,
    backend: ExecutionConfig,
    profile: ExecutionConfig,
    component: &str,
) -> Result<(), GatewayConfigError> {
    require_execution(declared, profile, component)?;
    require_execution(backend, profile, &format!("{component} backend"))
}

const fn profile_execution(profile: RuntimeProfile) -> ExecutionConfig {
    match profile {
        RuntimeProfile::Online => ExecutionConfig::Remote,
        _ => ExecutionConfig::Local,
    }
}

const fn language_execution(backend: LanguageBackend) -> ExecutionConfig {
    match backend {
        LanguageBackend::OllamaCompatible => ExecutionConfig::Local,
        LanguageBackend::OpenaiCompatible => ExecutionConfig::Remote,
    }
}

const fn asr_execution(backend: VoiceAsrBackend) -> ExecutionConfig {
    match backend {
        VoiceAsrBackend::Whisperkit | VoiceAsrBackend::Sensevoice => ExecutionConfig::Local,
        VoiceAsrBackend::OpenaiStreaming => ExecutionConfig::Remote,
    }
}

/// The OpenAI-compatible synthesizer serves a supervised loopback server and a
/// remote API alike, so the profile alone decides where it runs.
const fn speech_execution(
    backend: VoiceSpeechBackend,
    profile: ExecutionConfig,
) -> ExecutionConfig {
    match backend {
        VoiceSpeechBackend::OpenaiCompatible => profile,
    }
}

const fn diarization_execution(backend: VoiceDiarizationBackend) -> ExecutionConfig {
    match backend {
        VoiceDiarizationBackend::SpeechbrainSidecar => ExecutionConfig::Local,
        VoiceDiarizationBackend::Openai => ExecutionConfig::Remote,
    }
}

const fn execution_name(execution: ExecutionConfig) -> &'static str {
    match execution {
        ExecutionConfig::Local => "local",
        ExecutionConfig::Remote => "remote",
    }
}

/// Provider hosts are supervised local processes. A remote API is not one, and
/// referencing a host from a remote component would ask the supervisor to poll
/// and reap something it does not own.
fn require_host_reference(
    reference: Option<&str>,
    execution: ExecutionConfig,
    ids: &BTreeSet<&str>,
    component: &str,
) -> Result<(), GatewayConfigError> {
    match (execution, reference) {
        (ExecutionConfig::Local, Some(id)) if ids.contains(id) => Ok(()),
        (ExecutionConfig::Local, _) => Err(config_error(format!(
            "local {component} must reference a declared provider host"
        ))),
        (ExecutionConfig::Remote, None) => Ok(()),
        (ExecutionConfig::Remote, Some(_)) => Err(config_error(format!(
            "remote {component} must not reference a supervised provider host"
        ))),
    }
}

/// Credentials live in the process environment, never in the file. This checks
/// the *name* of the variable and that the variable is set; the value is read
/// only to prove it is not empty and is never bound, stored, or reported.
fn validate_credential_reference(
    credential_env: Option<&str>,
    execution: ExecutionConfig,
    component: &str,
) -> Result<(), GatewayConfigError> {
    match (execution, credential_env) {
        (ExecutionConfig::Local, None) => Ok(()),
        (ExecutionConfig::Local, Some(_)) => Err(config_error(format!(
            "local {component} must not name a credential environment variable"
        ))),
        (ExecutionConfig::Remote, None) => Err(config_error(format!(
            "remote {component} requires a credential environment variable"
        ))),
        (ExecutionConfig::Remote, Some(name)) => validate_environment_reference(name, component),
    }
}

fn validate_environment_reference(name: &str, component: &str) -> Result<(), GatewayConfigError> {
    if name.is_empty() || name.len() > MAX_CREDENTIAL_ENV_NAME_BYTES {
        return Err(config_error(format!(
            "{component} environment variable name must be within 1..=128 bytes"
        )));
    }
    if name.starts_with(|character: char| character.is_ascii_digit())
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(config_error(format!(
            "{component} environment variable name must be uppercase ASCII letters, digits, and underscores"
        )));
    }
    if !std::env::var(name).is_ok_and(|value| !value.trim().is_empty()) {
        return Err(config_error(format!(
            "{component} environment variable {name} is not set"
        )));
    }
    Ok(())
}

/// An absolute path this machine owns: bounded, UTF-8, and free of parent traversal. Applied to
/// the sidecar executable and the model cache alike; neither is required to exist at load time,
/// exactly as the voice sidecar's own path is not.
fn require_local_directory(path: &Path, component: &str) -> Result<(), GatewayConfigError> {
    if !path.is_absolute() {
        return Err(config_error(format!("{component} must be absolute")));
    }
    if path.as_os_str().len() > MAX_RECORDING_DIRECTORY_BYTES || path.to_str().is_none() {
        return Err(config_error(format!(
            "{component} must be bounded UTF-8 within 1024 bytes"
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(config_error(format!(
            "{component} must not traverse parent directories"
        )));
    }
    Ok(())
}

fn validate_identifier(
    value: &str,
    maximum_bytes: usize,
    component: &str,
) -> Result<(), GatewayConfigError> {
    if value.trim().is_empty() || value.len() > maximum_bytes || value.chars().any(char::is_control)
    {
        return Err(config_error(format!(
            "{component} must be non-empty, free of control characters, and within {maximum_bytes} bytes"
        )));
    }
    Ok(())
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

fn component_descriptor(
    kind: ComponentKind,
    provider: &str,
    execution: ExecutionConfig,
    name: &str,
) -> Result<ComponentDescriptor, GatewayConfigError> {
    validate_provider_label(provider, name)?;
    Ok(ComponentDescriptor::new(
        kind,
        provider,
        match execution {
            ExecutionConfig::Local => ExecutionLocation::Local,
            ExecutionConfig::Remote => ExecutionLocation::Remote,
        },
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

impl GatewayPrivacyMode {
    const fn protocol_name(self) -> &'static str {
        match self {
            Self::LocalOnly => "local_only",
            Self::Hybrid => "hybrid",
            Self::Cloud => "cloud",
        }
    }
}

impl From<GatewayPrivacyMode> for ProtocolPrivacyMode {
    fn from(value: GatewayPrivacyMode) -> Self {
        match value {
            GatewayPrivacyMode::LocalOnly => Self::LocalOnly,
            GatewayPrivacyMode::Hybrid => Self::Hybrid,
            GatewayPrivacyMode::Cloud => Self::Cloud,
        }
    }
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
    /// The NAME of an environment variable holding this provider's credential.
    /// The value never enters this structure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_env: Option<String>,
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
    OpenaiCompatible,
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
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_voice_response_mode"
    )]
    response_mode: Option<VoiceResponseMode>,
    capture: VoiceCaptureConfig,
    turn: VoiceTurnConfig,
    asr: VoiceAsrConfig,
    speech: VoiceSpeechConfig,
    audio: VoiceAudioConfig,
    /// Post-meeting speaker diarization. Absent means the deployment records meetings but
    /// does not reconcile them, which is what every configuration says today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diarization: Option<VoiceDiarizationConfig>,
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
    /// Remote transcription model identifier; local backends load a model
    /// directory instead and must leave this unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// The NAME of an environment variable holding this provider's credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_env: Option<String>,
    /// The remote streaming recognizer endpoint. Local sidecars never receive one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    download: Option<bool>,
    language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    server_vad: Option<VoiceServerVadConfig>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VoiceServerVadConfig {
    threshold: f64,
    prefix_padding_ms: u32,
    silence_duration_ms: u32,
}

impl VoiceAsrConfig {
    fn online_recognition(
        &self,
        endpoint: &str,
    ) -> Result<OpenAiSpeechRecognitionConfig, GatewayConfigError> {
        let model = self
            .model
            .as_deref()
            .ok_or_else(|| config_error("streaming online voice ASR requires a model"))?;
        let credential = self.credential_env.as_deref().ok_or_else(|| {
            config_error("remote voice ASR requires a credential environment variable")
        })?;
        let vad = self.server_vad.as_ref().ok_or_else(|| {
            config_error("streaming online voice ASR requires explicit server VAD configuration")
        })?;
        let mut recognition = OpenAiSpeechRecognitionConfig::new(endpoint, model, credential)
            .and_then(|config| {
                config.with_server_vad(
                    vad.threshold,
                    vad.prefix_padding_ms,
                    vad.silence_duration_ms,
                )
            })
            .map_err(adapter_error)?;
        if let Some(language) = &self.language {
            recognition = recognition.with_language(language).map_err(adapter_error)?;
        }
        Ok(recognition)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum VoiceAsrBackend {
    Whisperkit,
    Sensevoice,
    OpenaiStreaming,
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
    /// The NAME of an environment variable holding this provider's credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_env: Option<String>,
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ExecutionConfig {
    Local,
    Remote,
}

/// Post-meeting speaker diarization.
///
/// This is the one component an online deployment may deliberately keep local: a meeting whose
/// text and speech go to a provider can still have its speakers clustered on the machine that
/// recorded it. The reverse is refused — an offline deployment must not name a provider here.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VoiceDiarizationConfig {
    backend: VoiceDiarizationBackend,
    execution: ExecutionConfig,
    provider: String,
    model: String,
    /// The local sidecar's absolute path. Required by the sidecar backend, forbidden otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sidecar_executable: Option<PathBuf>,
    /// Where the operator put the model. The sidecar loads from here and never downloads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_cache_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_speakers: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    /// The NAME of an environment variable holding this provider's credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_env: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum VoiceDiarizationBackend {
    SpeechbrainSidecar,
    Openai,
}

impl VoiceDiarizationConfig {
    /// Validates the section and returns its component descriptor.
    ///
    /// `profile_execution` is where the *deployment* runs, not where this component must run:
    /// diarization is exempt from `require_declared_execution` on purpose, because a local
    /// diarizer inside an online meeting is the point of this backend. The one direction that
    /// is refused is the other one.
    fn validate(
        &self,
        profile_execution: ExecutionConfig,
    ) -> Result<ComponentDescriptor, GatewayConfigError> {
        require_execution(
            self.execution,
            diarization_execution(self.backend),
            "voice diarization backend",
        )?;
        if profile_execution == ExecutionConfig::Local && self.execution == ExecutionConfig::Remote
        {
            return Err(config_error(
                "offline gateway diarization backend must run locally",
            ));
        }
        validate_identifier(&self.model, MAX_MODEL_ID_BYTES, "voice diarization model")?;

        match self.backend {
            VoiceDiarizationBackend::SpeechbrainSidecar => {
                let executable = self.sidecar_executable.as_deref().ok_or_else(|| {
                    config_error("local voice diarization requires a sidecar executable")
                })?;
                require_local_directory(executable, "voice diarization sidecar executable")?;
                let cache = self.model_cache_dir.as_deref().ok_or_else(|| {
                    config_error("local voice diarization requires a model cache directory")
                })?;
                require_local_directory(cache, "voice diarization model cache directory")?;
                let max_speakers = self.max_speakers.ok_or_else(|| {
                    config_error("local voice diarization requires a maximum speaker count")
                })?;
                if !(MIN_DIARIZATION_SPEAKERS..=MAX_DIARIZATION_SPEAKERS).contains(&max_speakers) {
                    return Err(config_error(
                        "voice diarization maximum speaker count is outside 2..=64",
                    ));
                }
                if self.endpoint.is_some() {
                    return Err(config_error(
                        "local voice diarization must not name an endpoint",
                    ));
                }
            }
            VoiceDiarizationBackend::Openai => {
                let endpoint = self
                    .endpoint
                    .as_deref()
                    .ok_or_else(|| config_error("remote voice diarization requires an endpoint"))?;
                validate_endpoint(endpoint, self.execution, "voice diarization")?;
                if self.sidecar_executable.is_some()
                    || self.model_cache_dir.is_some()
                    || self.max_speakers.is_some()
                {
                    return Err(config_error(
                        "remote voice diarization must not configure local sidecar material",
                    ));
                }
            }
        }
        validate_credential_reference(
            self.credential_env.as_deref(),
            self.execution,
            "voice diarization",
        )?;

        component_descriptor(
            ComponentKind::Diarization,
            &self.provider,
            self.execution,
            "voice diarization",
        )
    }

    /// The adapter this section names. Only the local backend is constructed here, for the
    /// reason the online profile is refused as a whole: an online adapter is wired by the
    /// runtime, and building one from the loader would place a meeting somewhere the operator
    /// did not ask for.
    fn diarizer(&self) -> Result<Arc<dyn TranscriptionDiarizer>, GatewayConfigError> {
        match self.backend {
            VoiceDiarizationBackend::SpeechbrainSidecar => {
                let (Some(executable), Some(cache), Some(max_speakers)) = (
                    self.sidecar_executable.as_deref(),
                    self.model_cache_dir.as_deref(),
                    self.max_speakers,
                ) else {
                    return Err(config_error(
                        "local voice diarization requires a sidecar executable",
                    ));
                };
                Ok(Arc::new(SpeechbrainTranscriptionDiarizer::new(
                    SpeechbrainDiarizationConfig::new(executable, &self.model, cache, max_speakers)
                        .map_err(adapter_error)?,
                )))
            }
            VoiceDiarizationBackend::Openai => {
                let endpoint = self
                    .endpoint
                    .as_deref()
                    .ok_or_else(|| config_error("remote voice diarization requires an endpoint"))?;
                let credential = self.credential_env.as_deref().ok_or_else(|| {
                    config_error(
                        "remote voice diarization requires a credential environment variable",
                    )
                })?;
                Ok(Arc::new(OpenAiTranscriptionDiarizer::new(
                    OpenAiDiarizationConfig::new(endpoint, &self.model, credential)
                        .map_err(adapter_error)?,
                )))
            }
        }
    }
}

/// Encrypted session recording. `key_env` names the environment variable that
/// holds the encryption key; the key itself never enters this structure, the
/// serialized configuration, a status, or an error.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecordingConfig {
    directory: PathBuf,
    key_env: String,
}

impl RecordingConfig {
    fn validate(&self) -> Result<(), GatewayConfigError> {
        if !self.directory.is_absolute() {
            return Err(config_error("recording directory must be absolute"));
        }
        if self.directory.as_os_str().len() > MAX_RECORDING_DIRECTORY_BYTES {
            return Err(config_error("recording directory exceeded 1024 bytes"));
        }
        if self
            .directory
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(config_error(
                "recording directory must not traverse parent directories",
            ));
        }
        validate_environment_reference(&self.key_env, "recording key")
    }

    /// The runtime's view of this section. The key is not read here — the sink resolves it when
    /// a session starts, so a variable removed after load still fails the session, not the
    /// process.
    fn settings(&self) -> Result<RecordingSettings, GatewayConfigError> {
        RecordingSettings::new(self.directory.clone(), self.key_env.clone())
            .map_err(|error| config_error(error.message().to_owned()))
    }
}

/// Everything decided from the configuration file before any adapter exists.
struct DeploymentPlan {
    provider_hosts: Vec<ProviderHost>,
    components: Vec<ComponentDescriptor>,
    memory: Option<(SqliteMemoryContextProvider, SqliteMemoryStore)>,
    quality: ConversationQualityController,
    media: Option<MediaServerConfig>,
    status: RuntimeStatus,
}

mod optional_runtime_profile {
    use conversation_protocol::RuntimeProfile;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &Option<RuntimeProfile>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.map(RuntimeProfile::as_str).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<RuntimeProfile>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|value| {
                RuntimeProfile::parse(&value)
                    .ok_or_else(|| D::Error::custom("unsupported runtime profile"))
            })
            .transpose()
    }
}

mod optional_voice_response_mode {
    use conversation_protocol::VoiceResponseMode;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &Option<VoiceResponseMode>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.map(VoiceResponseMode::as_str).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<VoiceResponseMode>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|value| {
                VoiceResponseMode::parse(&value)
                    .ok_or_else(|| D::Error::custom("unsupported voice response mode"))
            })
            .transpose()
    }
}
