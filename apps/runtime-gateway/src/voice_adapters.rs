use std::sync::Arc;

use conversation_model_adapters::{StreamingSpeechSynthesizer, VoiceIoFactory};
use conversation_protocol::{
    ComponentDescriptor, PrivacyMode, RuntimeError, SessionId, VoiceSessionPolicy,
};

pub struct GatewayVoiceAdapters {
    pub io: Arc<dyn VoiceIoFactory>,
    pub speech: Arc<dyn StreamingSpeechSynthesizer>,
    pub policy: VoicePolicyTemplate,
}

#[derive(Clone, Debug)]
pub struct VoicePolicyTemplate {
    privacy_mode: PrivacyMode,
    speech_start_ms: u64,
    final_silence_ms: u64,
    components: Vec<ComponentDescriptor>,
}

impl VoicePolicyTemplate {
    pub(crate) fn new(
        privacy_mode: PrivacyMode,
        speech_start_ms: u64,
        final_silence_ms: u64,
        components: Vec<ComponentDescriptor>,
    ) -> Result<Self, RuntimeError> {
        VoiceSessionPolicy::new(
            SessionId::new(1),
            privacy_mode,
            speech_start_ms,
            final_silence_ms,
            components.clone(),
        )?;
        Ok(Self {
            privacy_mode,
            speech_start_ms,
            final_silence_ms,
            components,
        })
    }

    pub fn for_session(&self, session_id: SessionId) -> Result<VoiceSessionPolicy, RuntimeError> {
        VoiceSessionPolicy::new(
            session_id,
            self.privacy_mode,
            self.speech_start_ms,
            self.final_silence_ms,
            self.components.clone(),
        )
    }

    pub(crate) fn components(&self) -> &[ComponentDescriptor] {
        &self.components
    }
}
