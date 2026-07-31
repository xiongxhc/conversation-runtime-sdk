use conversation_protocol::{
    ComponentDescriptor, ComponentKind, ExecutionLocation, PrivacyMode, RuntimeStage, SessionId,
    VoiceSessionPolicy,
};
use conversation_runtime::validate_voice_policy;

const REQUIRED_KINDS: [ComponentKind; 4] = [
    ComponentKind::SpeechRecognition,
    ComponentKind::LanguageModel,
    ComponentKind::SpeechSynthesis,
    ComponentKind::AudioIo,
];

fn descriptor(kind: ComponentKind, execution: ExecutionLocation) -> ComponentDescriptor {
    ComponentDescriptor::new(kind, format!("prompt-bearing-{kind:?}"), execution)
}

fn policy(
    privacy_mode: PrivacyMode,
    components: impl IntoIterator<Item = ComponentDescriptor>,
) -> VoiceSessionPolicy {
    VoiceSessionPolicy::new(SessionId::new(1), privacy_mode, 200, 600, components)
        .expect("test policy should satisfy protocol-level validation")
}

fn policy_with(kind: ComponentKind, execution: ExecutionLocation) -> VoiceSessionPolicy {
    let mut components: Vec<_> = REQUIRED_KINDS
        .into_iter()
        .map(|required_kind| {
            let location = if required_kind == kind {
                execution
            } else {
                ExecutionLocation::Local
            };
            descriptor(required_kind, location)
        })
        .collect();

    if !REQUIRED_KINDS.contains(&kind) {
        components.push(descriptor(kind, execution));
    }

    policy(PrivacyMode::LocalOnly, components)
}

fn hybrid_policy_with_local_and_remote() -> VoiceSessionPolicy {
    policy(
        PrivacyMode::Hybrid,
        [
            descriptor(ComponentKind::SpeechRecognition, ExecutionLocation::Local),
            descriptor(ComponentKind::LanguageModel, ExecutionLocation::Remote),
            descriptor(ComponentKind::SpeechSynthesis, ExecutionLocation::Local),
            descriptor(ComponentKind::AudioIo, ExecutionLocation::Local),
        ],
    )
}

fn hybrid_policy_with_only_remote() -> VoiceSessionPolicy {
    policy(
        PrivacyMode::Hybrid,
        [
            descriptor(ComponentKind::SpeechRecognition, ExecutionLocation::Remote),
            descriptor(ComponentKind::LanguageModel, ExecutionLocation::Remote),
            descriptor(ComponentKind::SpeechSynthesis, ExecutionLocation::Remote),
            descriptor(ComponentKind::AudioIo, ExecutionLocation::Local),
        ],
    )
}

fn hybrid_policy_with_only_local() -> VoiceSessionPolicy {
    policy(
        PrivacyMode::Hybrid,
        REQUIRED_KINDS
            .into_iter()
            .map(|kind| descriptor(kind, ExecutionLocation::Local)),
    )
}

fn cloud_policy_with_only_remote() -> VoiceSessionPolicy {
    policy(
        PrivacyMode::Cloud,
        [
            descriptor(ComponentKind::SpeechRecognition, ExecutionLocation::Remote),
            descriptor(ComponentKind::LanguageModel, ExecutionLocation::Remote),
            descriptor(ComponentKind::SpeechSynthesis, ExecutionLocation::Remote),
            descriptor(ComponentKind::AudioIo, ExecutionLocation::Local),
        ],
    )
}

fn cloud_policy_with_local_llm() -> VoiceSessionPolicy {
    policy(
        PrivacyMode::Cloud,
        [
            descriptor(ComponentKind::SpeechRecognition, ExecutionLocation::Remote),
            descriptor(ComponentKind::LanguageModel, ExecutionLocation::Local),
            descriptor(ComponentKind::SpeechSynthesis, ExecutionLocation::Remote),
            descriptor(ComponentKind::AudioIo, ExecutionLocation::Remote),
        ],
    )
}

#[test]
fn local_only_rejects_every_remote_component_kind() {
    for kind in [
        ComponentKind::SpeechRecognition,
        ComponentKind::LanguageModel,
        ComponentKind::SpeechSynthesis,
        ComponentKind::AudioIo,
        ComponentKind::Tool,
        ComponentKind::Memory,
        ComponentKind::Telemetry,
    ] {
        let policy = policy_with(kind, ExecutionLocation::Remote);
        let error = validate_voice_policy(&policy).unwrap_err();

        assert_eq!(error.stage(), RuntimeStage::PrivacyPolicy);
        assert!(!error.message().contains("prompt"));
    }
}

#[test]
fn hybrid_and_cloud_have_distinct_primary_component_rules() {
    assert!(validate_voice_policy(&hybrid_policy_with_local_and_remote()).is_ok());
    assert!(validate_voice_policy(&hybrid_policy_with_only_local()).is_err());
    assert!(validate_voice_policy(&hybrid_policy_with_only_remote()).is_err());
    assert!(validate_voice_policy(&cloud_policy_with_only_remote()).is_ok());
    assert!(validate_voice_policy(&cloud_policy_with_local_llm()).is_err());
}

#[test]
fn every_required_kind_must_appear_exactly_once() {
    for missing_kind in REQUIRED_KINDS {
        let missing = policy(
            PrivacyMode::LocalOnly,
            REQUIRED_KINDS
                .into_iter()
                .filter(|kind| *kind != missing_kind)
                .map(|kind| descriptor(kind, ExecutionLocation::Local)),
        );
        let error = validate_voice_policy(&missing).unwrap_err();
        assert_eq!(error.stage(), RuntimeStage::PrivacyPolicy);
        assert!(!error.message().contains("prompt"));

        let duplicate = policy(
            PrivacyMode::LocalOnly,
            REQUIRED_KINDS
                .into_iter()
                .map(|kind| descriptor(kind, ExecutionLocation::Local))
                .chain([descriptor(missing_kind, ExecutionLocation::Local)]),
        );
        let error = validate_voice_policy(&duplicate).unwrap_err();
        assert_eq!(error.stage(), RuntimeStage::PrivacyPolicy);
        assert!(!error.message().contains("prompt"));
    }
}

#[test]
fn empty_provider_names_are_rejected_without_echoing_content() {
    for provider in ["", " \t "] {
        let error = VoiceSessionPolicy::new(
            SessionId::new(1),
            PrivacyMode::LocalOnly,
            200,
            600,
            [ComponentDescriptor::new(
                ComponentKind::SpeechRecognition,
                provider,
                ExecutionLocation::Local,
            )],
        )
        .unwrap_err();

        assert_eq!(error.stage(), RuntimeStage::PrivacyPolicy);
        assert_eq!(error.message(), "component provider must not be empty");
    }
}

#[test]
fn privacy_summary_contains_every_descriptor_in_component_kind_order() {
    let policy = policy(
        PrivacyMode::Hybrid,
        [
            descriptor(ComponentKind::Telemetry, ExecutionLocation::Remote),
            descriptor(ComponentKind::AudioIo, ExecutionLocation::Local),
            descriptor(ComponentKind::Tool, ExecutionLocation::Remote),
            descriptor(ComponentKind::SpeechSynthesis, ExecutionLocation::Remote),
            descriptor(ComponentKind::Memory, ExecutionLocation::Local),
            descriptor(ComponentKind::SpeechRecognition, ExecutionLocation::Local),
            descriptor(ComponentKind::LanguageModel, ExecutionLocation::Local),
        ],
    );

    let summary = validate_voice_policy(&policy).unwrap();
    let kinds: Vec<_> = summary
        .components()
        .iter()
        .map(ComponentDescriptor::kind)
        .collect();

    assert_eq!(
        kinds,
        [
            ComponentKind::SpeechRecognition,
            ComponentKind::LanguageModel,
            ComponentKind::SpeechSynthesis,
            ComponentKind::AudioIo,
            ComponentKind::Tool,
            ComponentKind::Memory,
            ComponentKind::Telemetry,
        ]
    );
}
