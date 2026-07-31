use conversation_protocol::{
    ComponentDescriptor, ComponentKind, ExecutionLocation, PrivacyMode, PrivacySummary,
    RuntimeError, RuntimeErrorKind, RuntimeStage, VoiceSessionPolicy,
};

const PRIMARY_KINDS: [ComponentKind; 3] = [
    ComponentKind::SpeechRecognition,
    ComponentKind::LanguageModel,
    ComponentKind::SpeechSynthesis,
];

pub fn validate_voice_policy(policy: &VoiceSessionPolicy) -> Result<PrivacySummary, RuntimeError> {
    let components = policy.components();

    if components
        .iter()
        .any(|component| component.provider().trim().is_empty())
    {
        return Err(privacy_error(
            "voice policy contains an empty provider identifier",
        ));
    }

    if PRIMARY_KINDS
        .iter()
        .chain([&ComponentKind::AudioIo])
        .any(|kind| component_count(components, *kind) != 1)
    {
        return Err(privacy_error(
            "voice policy requires exactly one descriptor for each required component kind",
        ));
    }

    let primary_components: Vec<_> = components
        .iter()
        .filter(|component| PRIMARY_KINDS.contains(&component.kind()))
        .collect();
    let local_primary_count = primary_components
        .iter()
        .filter(|component| component.execution() == ExecutionLocation::Local)
        .count();
    let remote_primary_count = primary_components
        .iter()
        .filter(|component| component.execution() == ExecutionLocation::Remote)
        .count();

    match policy.privacy_mode() {
        PrivacyMode::LocalOnly => {
            if components
                .iter()
                .any(|component| component.execution() != ExecutionLocation::Local)
            {
                return Err(privacy_error(
                    "local-only voice policy requires local execution",
                ));
            }
        }
        PrivacyMode::Hybrid => {
            if local_primary_count == 0 || remote_primary_count == 0 {
                return Err(privacy_error(
                    "hybrid voice policy requires local and remote primary components",
                ));
            }
        }
        PrivacyMode::Cloud => {
            if remote_primary_count != PRIMARY_KINDS.len() {
                return Err(privacy_error(
                    "cloud voice policy requires remote primary components",
                ));
            }
        }
        _ => {
            return Err(privacy_error(
                "voice policy uses an unsupported privacy mode",
            ));
        }
    }

    let mut summary_components = components.to_vec();
    summary_components.sort_by_key(|component| component_kind_order(component.kind()));

    Ok(PrivacySummary::new(
        policy.privacy_mode(),
        summary_components,
    ))
}

fn component_count(components: &[ComponentDescriptor], kind: ComponentKind) -> usize {
    components
        .iter()
        .filter(|component| component.kind() == kind)
        .count()
}

const fn component_kind_order(kind: ComponentKind) -> u8 {
    match kind {
        ComponentKind::SpeechRecognition => 0,
        ComponentKind::LanguageModel => 1,
        ComponentKind::SpeechSynthesis => 2,
        ComponentKind::AudioIo => 3,
        ComponentKind::Tool => 4,
        ComponentKind::Memory => 5,
        ComponentKind::Telemetry => 6,
        _ => u8::MAX,
    }
}

fn privacy_error(message: &'static str) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::Configuration,
        RuntimeStage::PrivacyPolicy,
        message,
    )
}
