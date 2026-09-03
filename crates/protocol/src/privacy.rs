use crate::{RuntimeError, RuntimeErrorKind, RuntimeStage, SessionId};

const SPEECH_START_MS: std::ops::RangeInclusive<u64> = 100..=1_000;
const FINAL_SILENCE_MS: std::ops::RangeInclusive<u64> = 200..=3_000;

/// Which execution posture the operator selected for a runtime.
///
/// `Offline` is the default whenever no profile is stated: every component runs
/// locally and the runtime reports [`PrivacyMode::LocalOnly`]. `Online` is only
/// ever set explicitly and requires the runtime to name each remote component
/// truthfully through [`ExecutionLocation::Remote`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeProfile {
    #[default]
    Offline,
    Online,
}

impl RuntimeProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::Online => "online",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "offline" => Some(Self::Offline),
            "online" => Some(Self::Online),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PrivacyMode {
    LocalOnly,
    Hybrid,
    Cloud,
}

impl PrivacyMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "local_only" => Some(Self::LocalOnly),
            "hybrid" => Some(Self::Hybrid),
            "cloud" => Some(Self::Cloud),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExecutionLocation {
    Local,
    Remote,
}

impl ExecutionLocation {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "local" => Some(Self::Local),
            "remote" => Some(Self::Remote),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ComponentKind {
    SpeechRecognition,
    LanguageModel,
    SpeechSynthesis,
    AudioIo,
    Tool,
    Memory,
    Telemetry,
    /// Post-meeting speaker diarization. It runs after a recording has stopped rather than
    /// inside a session, and it is listed so a status states where a meeting's audio was
    /// clustered — which is the whole point of choosing the local backend.
    Diarization,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentDescriptor {
    kind: ComponentKind,
    provider: String,
    execution: ExecutionLocation,
}

impl ComponentDescriptor {
    pub fn new(
        kind: ComponentKind,
        provider: impl Into<String>,
        execution: ExecutionLocation,
    ) -> Self {
        Self {
            kind,
            provider: provider.into(),
            execution,
        }
    }

    pub const fn kind(&self) -> ComponentKind {
        self.kind
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub const fn execution(&self) -> ExecutionLocation {
        self.execution
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceSessionPolicy {
    session_id: SessionId,
    privacy_mode: PrivacyMode,
    speech_start_ms: u64,
    final_silence_ms: u64,
    components: Vec<ComponentDescriptor>,
}

impl VoiceSessionPolicy {
    pub fn new(
        session_id: SessionId,
        privacy_mode: PrivacyMode,
        speech_start_ms: u64,
        final_silence_ms: u64,
        components: impl IntoIterator<Item = ComponentDescriptor>,
    ) -> Result<Self, RuntimeError> {
        let components: Vec<_> = components.into_iter().collect();

        if session_id.get() == 0 {
            return Err(policy_error("voice session identity must not be zero"));
        }
        if components.is_empty() {
            return Err(policy_error("voice session policy requires a component"));
        }
        if !SPEECH_START_MS.contains(&speech_start_ms) {
            return Err(policy_error(
                "speech start threshold is outside the supported range",
            ));
        }
        if !FINAL_SILENCE_MS.contains(&final_silence_ms) {
            return Err(policy_error(
                "final silence threshold is outside the supported range",
            ));
        }
        if components
            .iter()
            .any(|component| component.provider().trim().is_empty())
        {
            return Err(policy_error("component provider must not be empty"));
        }

        Ok(Self {
            session_id,
            privacy_mode,
            speech_start_ms,
            final_silence_ms,
            components,
        })
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub const fn privacy_mode(&self) -> PrivacyMode {
        self.privacy_mode
    }

    pub const fn speech_start_ms(&self) -> u64 {
        self.speech_start_ms
    }

    pub const fn final_silence_ms(&self) -> u64 {
        self.final_silence_ms
    }

    pub fn components(&self) -> &[ComponentDescriptor] {
        &self.components
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrivacySummary {
    privacy_mode: PrivacyMode,
    components: Vec<ComponentDescriptor>,
}

impl PrivacySummary {
    pub fn new(
        privacy_mode: PrivacyMode,
        components: impl IntoIterator<Item = ComponentDescriptor>,
    ) -> Self {
        Self {
            privacy_mode,
            components: components.into_iter().collect(),
        }
    }

    pub const fn privacy_mode(&self) -> PrivacyMode {
        self.privacy_mode
    }

    pub fn components(&self) -> &[ComponentDescriptor] {
        &self.components
    }
}

fn policy_error(message: &'static str) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::Configuration,
        RuntimeStage::PrivacyPolicy,
        message,
    )
}
