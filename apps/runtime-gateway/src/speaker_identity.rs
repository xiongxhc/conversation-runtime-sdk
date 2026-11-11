//! Assembles speaker identification for a gateway session from its configured parts.

use std::sync::Arc;

use conversation_memory::SqliteMemoryStore;
use conversation_model_adapters::{GenerationLanguageModel, SidecarTurnAudio};
use conversation_runtime::{LanguageModelNameClaimDetector, SpeakerIdentityStage, VoiceEmbedder};

use crate::speaker_audio::GatewayTurnAudio;
use crate::{GatewaySession, SpeakerIdentitySettings};

/// A configured `[speaker_identity]` section and the voice embedding model it runs on.
pub struct GatewaySpeakerIdentity {
    pub settings: SpeakerIdentitySettings,
    pub embedder: Arc<dyn VoiceEmbedder>,
}

/// Turns speaker identification on for `session`'s voice sessions. Turn audio comes from the
/// media bridge or `sidecar_turn_audio`, and name claims are read by the session's own
/// language model.
pub fn attach_speaker_identity(
    session: GatewaySession,
    identity: GatewaySpeakerIdentity,
    store: SqliteMemoryStore,
    language: Arc<dyn GenerationLanguageModel>,
    sidecar_turn_audio: Option<SidecarTurnAudio>,
) -> GatewaySession {
    let audio = Arc::new(GatewayTurnAudio::new(sidecar_turn_audio));
    let stage = SpeakerIdentityStage::new(
        Arc::new(store),
        identity.settings.key,
        identity.settings.policy,
        identity.settings.wait,
        Arc::clone(&audio) as _,
        identity.embedder,
        Arc::new(LanguageModelNameClaimDetector::new(language)),
    );
    session.with_speaker_identity(Arc::new(stage), audio)
}
