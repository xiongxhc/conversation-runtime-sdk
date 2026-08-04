use std::fmt;
use std::fs;
use std::io::Read;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use conversation_memory::{SqliteMemoryContextProvider, SqliteMemoryStore, SystemMemoryClock};
use conversation_model_adapters::{OllamaConfig, OllamaLanguageModel};
use conversation_protocol::{
    ConversationMode, FollowUpPolicy, PersonaLevel, PersonaProfile, ResponseControls,
    SilencePolicy, SpeechPace,
};
use conversation_runtime::ConversationQualityController;
use serde::Deserialize;

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

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
    privacy_mode: PrivacyMode,
    language: LanguageConfig,
    persona: PersonaConfig,
    memory: Option<MemoryConfig>,
}

pub struct GatewayAdapters {
    pub language: OllamaLanguageModel,
    pub quality: ConversationQualityController,
    pub memory: Option<SqliteMemoryContextProvider>,
}

impl GatewayConfig {
    pub fn load(path: &Path) -> Result<Self, GatewayConfigError> {
        let config = load_toml(path)?;
        config.validate()?;
        Ok(config)
    }

    pub fn into_adapters(self) -> Result<GatewayAdapters, GatewayConfigError> {
        self.build_adapters()
    }

    fn validate(&self) -> Result<(), GatewayConfigError> {
        if self.schema_version != 1 {
            return Err(config_error(
                "gateway configuration schema_version must be 1",
            ));
        }
        if !matches!(self.privacy_mode, PrivacyMode::LocalOnly) {
            return Err(config_error("gateway privacy_mode must be local-only"));
        }
        if !matches!(self.language.backend, LanguageBackend::OllamaCompatible) {
            return Err(config_error("language backend must be ollama-compatible"));
        }
        validate_loopback_http_endpoint(&self.language.endpoint)?;
        self.build_adapters().map(|_| ())
    }

    fn build_adapters(&self) -> Result<GatewayAdapters, GatewayConfigError> {
        let language = OllamaConfig::new(&self.language.model)
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
        if !self.language.temperature.is_finite() || self.language.temperature < 0.0 {
            return Err(config_error(
                "language temperature must be finite and non-negative",
            ));
        }

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
        let memory = self.memory.as_ref().map(memory_provider).transpose()?;

        Ok(GatewayAdapters {
            language: OllamaLanguageModel::new_direct(language),
            quality: ConversationQualityController::new(
                persona,
                controls,
                self.persona.mode.into(),
            ),
            memory,
        })
    }
}

fn load_toml(path: &Path) -> Result<GatewayConfig, GatewayConfigError> {
    if !path.is_absolute() {
        return Err(config_error("gateway configuration path must be absolute"));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(config_error(
                "gateway configuration path must be a regular file and not a symbolic link",
            ));
        }
        Err(_) => {
            return Err(config_error(
                "gateway configuration file could not be opened",
            ));
        }
    }

    let file = fs::File::open(path)
        .map_err(|_| config_error("gateway configuration file could not be opened"))?;
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

fn validate_loopback_http_endpoint(endpoint: &str) -> Result<(), GatewayConfigError> {
    let endpoint = reqwest::Url::parse(endpoint)
        .map_err(|_| config_error("language endpoint must be a valid URL"))?;
    if endpoint.scheme() != "http" {
        return Err(config_error("language endpoint must use plain HTTP"));
    }
    if !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(config_error(
            "language endpoint must not contain credentials, a query, or a fragment",
        ));
    }
    let address = endpoint
        .host_str()
        .and_then(|host| host.parse::<IpAddr>().ok())
        .ok_or_else(|| config_error("language endpoint must use a loopback IP address"))?;
    if !address.is_loopback() {
        return Err(config_error(
            "language endpoint must use a loopback IP address",
        ));
    }
    Ok(())
}

fn memory_provider(
    config: &MemoryConfig,
) -> Result<SqliteMemoryContextProvider, GatewayConfigError> {
    if !config.database.is_absolute() {
        return Err(config_error("memory database path must be absolute"));
    }
    let store = SqliteMemoryStore::open(&config.database).map_err(memory_error)?;
    SqliteMemoryContextProvider::new(store, Arc::new(SystemMemoryClock))
        .with_limits(config.maximum_items, config.maximum_bytes)
        .map_err(memory_error)
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

fn config_error(message: impl Into<String>) -> GatewayConfigError {
    GatewayConfigError(message.into())
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum PrivacyMode {
    LocalOnly,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LanguageConfig {
    backend: LanguageBackend,
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
