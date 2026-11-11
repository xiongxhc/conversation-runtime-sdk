mod config;
mod framing;
mod input_relay;
#[cfg(feature = "media-admission-evidence")]
mod media_admission_evidence;
mod media_server;
mod memory_extraction;
mod provider_supervisor;
mod session;
mod speaker_audio;
mod speaker_identity;
mod voice_adapters;

#[cfg(feature = "scripted-providers")]
pub use config::ScriptedGatewayConfig;
pub use config::{
    GatewayAdapters, GatewayConfig, GatewayConfigError, GatewayDeploymentConfig,
    GatewayMemoryExtraction, LanguageDeployment, ProviderEnvironmentPolicy, ProviderHost,
    ProviderHostOwnership, SpeakerIdentitySettings,
};
pub use framing::{FrameError, FrameReader, FrameWriter};
pub use input_relay::{input_relay, InputRelay};
pub use media_server::{MediaLease, MediaServer, MediaServerConfig};
pub use memory_extraction::MemoryExtractionSettings;
pub use provider_supervisor::{ProviderSupervisor, ProviderSupervisorError};
pub use session::{GatewaySession, GatewaySessionError};
pub use speaker_audio::{GatewayTurnAudio, MediaTurnAudio, MediaTurnAudioRegistration};
pub use speaker_identity::{attach_speaker_identity, GatewaySpeakerIdentity};
pub use voice_adapters::{GatewayVoiceAdapters, VoicePolicyTemplate};
