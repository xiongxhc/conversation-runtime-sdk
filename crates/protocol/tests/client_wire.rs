use conversation_protocol::{
    decode_client_command, encode_gateway_message, encode_gateway_message_for_version,
    memory_preview, ClientCommand, ClientComponentDescriptor, ClientDiarizedSegment,
    ClientMediaLeaseDescriptor, ClientMemoryApproval, ClientMemoryCursor, ClientMemoryInspection,
    ClientMemoryProvenance, ClientMemoryRecord, ClientMemoryRetention, ClientMemorySummary,
    ClientMemoryTrace, ClientPersonaState, ClientPrivacySummary, ClientProcessedText,
    ClientQualityDecision, ClientRecordingGap, ClientRecordingReconciliation,
    ClientRecordingSummary, ClientResponseControls, ClientRuntimeError, ClientRuntimeEvent,
    ClientVoiceSessionEvent, ClientWireError, ComponentDescriptor, ComponentKind, ContextSource,
    ConversationContextExchange, ConversationMode, ConversationSignal, ExecutionLocation,
    FollowUpPolicy, GatewayMessage, GenerationId, MemoryApproval, MemoryConfidence, MemoryDraft,
    MemoryId, MemoryInspection, MemoryKind, MemoryProvenance, MemoryProvenanceKind, MemoryRecord,
    MemoryRetention, MemoryRetrievalReason, MemoryRetrievalTrace, MemoryTraceExclusions,
    MemoryTraceItem, PersonaLevel, PersonaProfile, PlaybackState, PrivacyMode, PrivacySummary,
    QualityDecision, RecoveryDisposition, ResponseControls, RetrievalTraceId, RuntimeError,
    RuntimeErrorKind, RuntimeEvent, RuntimeStage, RuntimeStatus, SessionId, SilencePolicy,
    SpeechPace, TurnId, UnixTimestampMillis, VoiceActivity, VoiceSessionEvent,
    VoiceTimingMilestone, CONTEXT_SEED_PROTOCOL_VERSION,
    DEFAULT_CLIENT_PROTOCOL_VERSION as CLIENT_PROTOCOL_VERSION, LEGACY_CLIENT_PROTOCOL_VERSION,
    MAX_CLIENT_COMPONENT_DESCRIPTORS, MAX_CLIENT_DEVICE_LABEL_BYTES, MAX_CLIENT_DIARIZED_SEGMENTS,
    MAX_CLIENT_DIARIZED_SEGMENT_TEXT_BYTES, MAX_CLIENT_FRAME_BYTES,
    MAX_CLIENT_PROCESSED_OUTPUT_BYTES, MAX_CLIENT_PROVIDER_LABEL_BYTES, MAX_CLIENT_RECORDING_GAPS,
    MAX_CONVERSATION_MESSAGE_BYTES, MAX_HISTORY_BYTES, MAX_MEMORY_PREVIEW_BYTES,
    RUNTIME_PROFILE_PROTOCOL_VERSION,
};
use serde::{Deserialize, Deserializer};

fn gateway_value(message: &GatewayMessage) -> serde_json::Value {
    serde_json::from_slice(&encode_gateway_message(message).unwrap()).unwrap()
}

fn gateway_value_for_version(message: &GatewayMessage, version: u64) -> serde_json::Value {
    serde_json::from_slice(&encode_gateway_message_for_version(message, version).unwrap()).unwrap()
}

fn unix_now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap()
}

fn ephemeral_test_media_credential() -> String {
    format!("lease{}{}", std::process::id(), unix_now_millis())
}

fn status() -> RuntimeStatus {
    RuntimeStatus {
        profile: "offline".to_owned(),
        transport: "stdio".to_owned(),
        privacy_mode: "local_only".to_owned(),
        language_location: "local".to_owned(),
        model_id: "local-model".to_owned(),
        memory_enabled: false,
        memory_location: None,
        telemetry_enabled: false,
        capabilities: vec!["text".to_owned()],
        components: vec![client_component("language_model", "Local language")],
        last_context_seed_operation_id: None,
    }
}

fn client_component(kind: &str, provider_label: &str) -> ClientComponentDescriptor {
    ClientComponentDescriptor {
        kind: kind.to_owned(),
        execution_location: "local".to_owned(),
        provider_label: provider_label.to_owned(),
    }
}

fn core_component(kind: ComponentKind, provider: &str) -> ComponentDescriptor {
    ComponentDescriptor::new(kind, provider, ExecutionLocation::Local)
}

fn voice_components() -> Vec<ClientComponentDescriptor> {
    vec![
        client_component("speech_recognition", "Local speech recognition"),
        client_component("language_model", "Local language"),
        client_component("speech_synthesis", "Local speech synthesis"),
        client_component("audio_io", "Local audio"),
    ]
}

fn runtime_error() -> ClientRuntimeError {
    ClientRuntimeError {
        code: "invalid_state".to_owned(),
        kind: "invalid_state".to_owned(),
        stage: "runtime".to_owned(),
        message: "an active turn already exists".to_owned(),
    }
}

fn memory_summary(id: u64) -> ClientMemorySummary {
    ClientMemorySummary {
        id: id.to_string(),
        content_preview: "memory preview".to_owned(),
        kind: "semantic".to_owned(),
        state: "active".to_owned(),
        pinned: false,
        updated_at_ms: "1".to_owned(),
    }
}

fn client_memory_inspection() -> ClientMemoryInspection {
    ClientMemoryInspection {
        record: ClientMemoryRecord {
            id: "7".to_owned(),
            kind: "semantic".to_owned(),
            content: "A generic local preference.".to_owned(),
            state: "active".to_owned(),
            confidence: "900".to_owned(),
            created_at_ms: "0".to_owned(),
            updated_at_ms: i64::MAX.to_string(),
            pinned: false,
            revision: "3".to_owned(),
            retention: ClientMemoryRetention::Session {
                session_id: u64::MAX.to_string(),
            },
            last_used_at_ms: Some("0".to_owned()),
            last_retrieval_reason: None,
        },
        sources: vec![ClientMemoryProvenance {
            kind: "user_provided".to_owned(),
            source_id: "fixture-source".to_owned(),
            source_timestamp_ms: "0".to_owned(),
            actor: "fixture-user".to_owned(),
        }],
        approvals: vec![ClientMemoryApproval {
            confirmation_id: "fixture-confirmation".to_owned(),
            actor: "fixture-user".to_owned(),
            confirmed_at_ms: "0".to_owned(),
            approved_revision: "2".to_owned(),
        }],
        sources_truncated: false,
        approvals_truncated: false,
    }
}

fn timestamp(value: i64) -> UnixTimestampMillis {
    UnixTimestampMillis::new(value).unwrap()
}

fn provenance(
    kind: MemoryProvenanceKind,
    source_id: impl Into<String>,
    source_timestamp: i64,
) -> MemoryProvenance {
    MemoryProvenance::new(
        kind,
        source_id,
        timestamp(source_timestamp),
        "local-user",
        None,
    )
    .unwrap()
}

fn approval(
    confirmation_id: impl Into<String>,
    actor: impl Into<String>,
    confirmed_at: i64,
    approved_revision: u64,
) -> MemoryApproval {
    MemoryApproval::new(
        confirmation_id,
        actor,
        timestamp(confirmed_at),
        approved_revision,
    )
    .unwrap()
}

#[test]
fn both_protocol_versions_accept_start_turns_without_client_selected_identifiers() {
    for version in [LEGACY_CLIENT_PROTOCOL_VERSION, CLIENT_PROTOCOL_VERSION] {
        let payload = format!(
            r#"{{"protocol_version":{version},"type":"start_turn","request_id":"req-1","transcript":"hello"}}"#
        );
        let command = decode_client_command(payload.as_bytes()).unwrap();
        assert!(matches!(command, ClientCommand::StartTurn { .. }));
    }

    assert!(decode_client_command(
        br#"{"protocol_version":1,"type":"start_turn","request_id":"req-1","turn_id":"1","transcript":"hello"}"#,
    )
    .is_err());
}

#[test]
fn version_two_seed_command_decodes_the_exact_wire_contract() {
    let command = decode_client_command(
        br#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"request-1","operation_id":"continue-1","exchanges":[{"user":"hello","assistant":"hi"}]}"#,
    )
    .unwrap();

    assert_eq!(
        command,
        ClientCommand::SeedConversationContext {
            request_id: "request-1".to_owned(),
            operation_id: "continue-1".to_owned(),
            exchanges: vec![ConversationContextExchange::new("hello", "hi").unwrap()],
        }
    );
}

#[test]
fn seed_command_rejects_v1_empty_malformed_and_unknown_fields() {
    let invalid = [
        r#"{"protocol_version":1,"type":"seed_conversation_context","request_id":"request-1","operation_id":"continue-1","exchanges":[{"user":"hello","assistant":"hi"}]}"#,
        r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"request-1","operation_id":"continue-1","exchanges":[]}"#,
        r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"request-1","operation_id":"continue-1","exchanges":[{"user":" ","assistant":"hi"}]}"#,
        r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"request-1","operation_id":"continue-1","exchanges":[{"user":"hello","assistant":"\n\t"}]}"#,
        r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"request-1","operation_id":"continue-1","exchanges":[{"user":"hello"}]}"#,
        r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"request-1","operation_id":"continue-1","exchanges":[{"user":"hello","assistant":"hi","extra":true}]}"#,
        r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"request-1","operation_id":"continue-1","exchanges":[{"user":"hello","assistant":"hi"}],"extra":true}"#,
        r#"{"protocol_version":2,"type":"seed_conversation_context","request_id":"request-1","operation_id":"continue-1","operation_id":"duplicate","exchanges":[{"user":"hello","assistant":"hi"}]}"#,
    ];

    for payload in invalid {
        assert!(
            decode_client_command(payload.as_bytes()).is_err(),
            "accepted invalid seed payload: {payload}"
        );
    }
}

#[test]
fn seed_command_enforces_operation_pair_message_and_aggregate_byte_bounds() {
    let valid_operation_id = "🙂".repeat(16);
    let exact_limit_exchanges = vec![serde_json::json!({
        "user": "🙂".repeat(MAX_CONVERSATION_MESSAGE_BYTES / 4),
        "assistant": "🙂".repeat(MAX_CONVERSATION_MESSAGE_BYTES / 4),
    })];
    let valid = serde_json::json!({
        "protocol_version": 2,
        "type": "seed_conversation_context",
        "request_id": "request-1",
        "operation_id": valid_operation_id,
        "exchanges": exact_limit_exchanges,
    });
    assert!(decode_client_command(&serde_json::to_vec(&valid).unwrap()).is_ok());

    let sixteen_pairs = serde_json::json!({
        "protocol_version": 2,
        "type": "seed_conversation_context",
        "request_id": "request-1",
        "operation_id": "continue-1",
        "exchanges": (0..16).map(|_| serde_json::json!({"user":"u","assistant":"a"})).collect::<Vec<_>>(),
    });
    assert!(decode_client_command(&serde_json::to_vec(&sixteen_pairs).unwrap()).is_ok());

    let invalid = [
        serde_json::json!({
            "protocol_version": 2,
            "type": "seed_conversation_context",
            "request_id": "request-1",
            "operation_id": "",
            "exchanges": [{"user":"hello","assistant":"hi"}],
        }),
        serde_json::json!({
            "protocol_version": 2,
            "type": "seed_conversation_context",
            "request_id": "request-1",
            "operation_id": "🙂".repeat(17),
            "exchanges": [{"user":"hello","assistant":"hi"}],
        }),
        serde_json::json!({
            "protocol_version": 2,
            "type": "seed_conversation_context",
            "request_id": "request-1",
            "operation_id": "continue-1",
            "exchanges": (0..17).map(|_| serde_json::json!({"user":"u","assistant":"a"})).collect::<Vec<_>>(),
        }),
        serde_json::json!({
            "protocol_version": 2,
            "type": "seed_conversation_context",
            "request_id": "request-1",
            "operation_id": "continue-1",
            "exchanges": [{"user":"u".repeat(MAX_CONVERSATION_MESSAGE_BYTES + 1),"assistant":"a"}],
        }),
        serde_json::json!({
            "protocol_version": 2,
            "type": "seed_conversation_context",
            "request_id": "request-1",
            "operation_id": "continue-1",
            "exchanges": [
                {"user":"u".repeat(MAX_CONVERSATION_MESSAGE_BYTES),"assistant":"a".repeat(MAX_CONVERSATION_MESSAGE_BYTES - 1)},
                {"user":"u","assistant":"a"},
            ],
        }),
    ];

    for payload in invalid {
        assert!(
            decode_client_command(&serde_json::to_vec(&payload).unwrap()).is_err(),
            "accepted invalid seed payload with {} bytes of JSON",
            serde_json::to_vec(&payload).unwrap().len()
        );
    }

    assert_eq!(MAX_HISTORY_BYTES, 32_768);
}

#[test]
fn gateway_status_encoding_is_strictly_version_specific() {
    let legacy_status = RuntimeStatus {
        last_context_seed_operation_id: Some("continue-1".to_owned()),
        ..status()
    };
    let legacy_ready = gateway_value_for_version(
        &GatewayMessage::Ready {
            status: legacy_status,
        },
        LEGACY_CLIENT_PROTOCOL_VERSION,
    );
    assert_eq!(legacy_ready["protocol_version"], 1);
    assert!(!legacy_ready["status"]
        .as_object()
        .unwrap()
        .contains_key("last_context_seed_operation_id"));

    let v2_status = RuntimeStatus {
        capabilities: vec!["text".to_owned(), "conversation_context_seed".to_owned()],
        last_context_seed_operation_id: None,
        ..status()
    };
    let v2_ready = gateway_value_for_version(
        &GatewayMessage::Ready {
            status: v2_status.clone(),
        },
        CONTEXT_SEED_PROTOCOL_VERSION,
    );
    assert_eq!(
        v2_ready,
        serde_json::json!({
            "protocol_version": 2,
            "type": "ready",
            "status": {
                "transport": "stdio",
                "privacy_mode": "local_only",
                "language_location": "local",
                "model_id": "local-model",
                "memory_enabled": false,
                "memory_location": null,
                "telemetry_enabled": false,
                "capabilities": ["text", "conversation_context_seed"],
                "components": [{
                    "kind": "language_model",
                    "execution_location": "local",
                    "provider_label": "Local language"
                }],
                "last_context_seed_operation_id": null
            }
        })
    );

    let v2_status_with_operation = RuntimeStatus {
        last_context_seed_operation_id: Some("continue-1".to_owned()),
        ..v2_status
    };
    let correlated = gateway_value_for_version(
        &GatewayMessage::Status {
            request_id: "request-1".to_owned(),
            status: v2_status_with_operation,
        },
        CONTEXT_SEED_PROTOCOL_VERSION,
    );
    assert_eq!(
        correlated["status"]["last_context_seed_operation_id"],
        "continue-1"
    );
}

#[test]
fn status_capability_and_operation_identity_are_validated_per_version() {
    let v2 = RuntimeStatus {
        capabilities: vec![
            "text".to_owned(),
            "conversation_context_seed".to_owned(),
            "persona_control".to_owned(),
        ],
        ..status()
    };
    assert!(encode_gateway_message_for_version(
        &GatewayMessage::Ready { status: v2 },
        CLIENT_PROTOCOL_VERSION,
    )
    .is_ok());

    let invalid = [
        (
            LEGACY_CLIENT_PROTOCOL_VERSION,
            RuntimeStatus {
                capabilities: vec!["text".to_owned(), "conversation_context_seed".to_owned()],
                ..status()
            },
        ),
        (
            CLIENT_PROTOCOL_VERSION,
            RuntimeStatus {
                capabilities: vec![
                    "text".to_owned(),
                    "persona_control".to_owned(),
                    "conversation_context_seed".to_owned(),
                ],
                ..status()
            },
        ),
        (
            CLIENT_PROTOCOL_VERSION,
            RuntimeStatus {
                capabilities: vec![
                    "text".to_owned(),
                    "conversation_context_seed".to_owned(),
                    "conversation_context_seed".to_owned(),
                ],
                ..status()
            },
        ),
        (
            CLIENT_PROTOCOL_VERSION,
            RuntimeStatus {
                capabilities: vec!["text".to_owned(), "conversation_context_seed".to_owned()],
                last_context_seed_operation_id: Some(String::new()),
                ..status()
            },
        ),
        (
            CLIENT_PROTOCOL_VERSION,
            RuntimeStatus {
                capabilities: vec!["text".to_owned(), "conversation_context_seed".to_owned()],
                last_context_seed_operation_id: Some("x".repeat(65)),
                ..status()
            },
        ),
    ];

    for (version, status) in invalid {
        assert!(matches!(
            encode_gateway_message_for_version(&GatewayMessage::Ready { status }, version,),
            Err(ClientWireError::InvalidRuntimeStatus)
        ));
    }
}

#[test]
fn compatibility_gateway_encoder_preserves_the_exact_v1_schema() {
    let message = GatewayMessage::Ready { status: status() };
    assert_eq!(
        gateway_value(&message),
        gateway_value_for_version(&message, LEGACY_CLIENT_PROTOCOL_VERSION)
    );
    assert_eq!(gateway_value(&message)["protocol_version"], 1);
}

#[test]
fn accepted_start_turns_carry_the_gateway_allocated_identifier() {
    let value = gateway_value(&GatewayMessage::CommandAccepted {
        request_id: "req-1".to_owned(),
        turn_id: Some(TurnId::new(9)),
        media_lease: None,
    });

    assert_eq!(value["type"], "command_accepted");
    assert_eq!(value["protocol_version"], 1);
    assert_eq!(value["request_id"], "req-1");
    assert_eq!(value["turn_id"], "9");
}

// Production break caught: a version-three start acceptance could disclose a lease on a legacy
// wire shape, or serialize an unbounded/unvalidated credential-bearing descriptor.
#[test]
fn version_three_start_acceptance_carries_a_redacted_bounded_media_lease() {
    let credential = ephemeral_test_media_credential();
    let lease = ClientMediaLeaseDescriptor::new(
        1,
        SessionId::new(17),
        "tcp://127.0.0.1:41000".to_owned(),
        credential.clone(),
        unix_now_millis() + 60_000,
        10_000,
    )
    .unwrap();
    let message = GatewayMessage::CommandAccepted {
        request_id: "start-lease".to_owned(),
        turn_id: None,
        media_lease: Some(lease.clone()),
    };

    let v3 = gateway_value_for_version(&message, RUNTIME_PROFILE_PROTOCOL_VERSION);
    assert_eq!(v3["type"], "command_accepted");
    assert_eq!(v3["protocol_version"], RUNTIME_PROFILE_PROTOCOL_VERSION);
    assert_eq!(v3["request_id"], "start-lease");
    assert_eq!(v3["media_lease"]["media_protocol_version"], 1);
    assert_eq!(v3["media_lease"]["session_id"], "17");
    assert_eq!(
        v3["media_lease"]["connect_endpoint"],
        "tcp://127.0.0.1:41000"
    );
    assert_eq!(v3["media_lease"]["credential"], credential);
    assert_eq!(v3["media_lease"]["reconnect_window_ms"], "10000");

    for version in [
        LEGACY_CLIENT_PROTOCOL_VERSION,
        CONTEXT_SEED_PROTOCOL_VERSION,
    ] {
        assert_eq!(
            encode_gateway_message_for_version(&message, version).unwrap(),
            format!(
                r#"{{"type":"command_accepted","protocol_version":{version},"request_id":"start-lease"}}"#
            )
            .into_bytes(),
        );
    }
    assert!(!format!("{lease:?}").contains(&credential));

    let endpoint = "tcp://127.0.0.1:41000".to_owned();
    let future = unix_now_millis() + 60_000;
    for invalid in [
        ClientMediaLeaseDescriptor::new(
            2,
            SessionId::new(17),
            endpoint.clone(),
            ephemeral_test_media_credential(),
            future,
            10_000,
        ),
        ClientMediaLeaseDescriptor::new(
            1,
            SessionId::new(0),
            endpoint.clone(),
            ephemeral_test_media_credential(),
            future,
            10_000,
        ),
        ClientMediaLeaseDescriptor::new(
            1,
            SessionId::new(17),
            "https://127.0.0.1:41000".to_owned(),
            ephemeral_test_media_credential(),
            future,
            10_000,
        ),
        ClientMediaLeaseDescriptor::new(
            1,
            SessionId::new(17),
            endpoint.clone(),
            String::new(),
            future,
            10_000,
        ),
        ClientMediaLeaseDescriptor::new(
            1,
            SessionId::new(17),
            endpoint,
            ephemeral_test_media_credential(),
            unix_now_millis().saturating_sub(1),
            10_000,
        ),
    ] {
        assert!(matches!(invalid, Err(ClientWireError::InvalidMediaLease)));
    }
}

#[test]
fn version_one_voice_controls_decode_without_client_selected_identifiers() {
    let cases = [
        ("start_voice_session", "start"),
        ("stop_voice_session", "stop"),
        ("pause_voice_capture", "pause"),
        ("resume_voice_capture", "resume"),
    ];

    for (command_type, expected) in cases {
        let payload = format!(
            r#"{{"protocol_version":1,"type":"{command_type}","request_id":"req-{expected}"}}"#
        );
        let command = decode_client_command(payload.as_bytes()).unwrap();
        assert!(
            matches!(
                (expected, command),
                ("start", ClientCommand::StartVoiceSession { .. })
                    | ("stop", ClientCommand::StopVoiceSession { .. })
                    | ("pause", ClientCommand::PauseVoiceCapture { .. })
                    | ("resume", ClientCommand::ResumeVoiceCapture { .. })
            ),
            "{command_type}"
        );
    }
}

#[test]
fn typed_start_projection_carries_request_correlation_and_exact_final_text() {
    let started = ClientRuntimeEvent::TurnStarted {
        request_id: Some("req-1".to_owned()),
        turn_id: TurnId::new(1),
    };
    let completed = ClientRuntimeEvent::try_from(RuntimeEvent::TextCompleted {
        turn_id: TurnId::new(1),
        text: "exact answer".to_owned(),
    })
    .unwrap();

    assert_eq!(
        gateway_value(&GatewayMessage::RuntimeEvent { event: started }),
        serde_json::json!({
            "protocol_version": 1,
            "type": "runtime_event",
            "event": {"type": "turn_started", "request_id": "req-1", "turn_id": "1"}
        })
    );
    assert_eq!(
        gateway_value(&GatewayMessage::RuntimeEvent { event: completed }),
        serde_json::json!({
            "protocol_version": 1,
            "type": "runtime_event",
            "event": {"type": "text_completed", "turn_id": "1", "text": "exact answer"}
        })
    );
}

#[test]
fn voice_events_project_every_approved_lifecycle_variant() {
    let session_id = SessionId::new(1);
    let turn_id = TurnId::new(1);
    let generation_id = GenerationId::new(1);
    let privacy = PrivacySummary::new(
        PrivacyMode::LocalOnly,
        [
            core_component(ComponentKind::SpeechRecognition, "Local speech recognition"),
            core_component(ComponentKind::LanguageModel, "Local language"),
            core_component(ComponentKind::SpeechSynthesis, "Local speech synthesis"),
            core_component(ComponentKind::AudioIo, "Local audio"),
        ],
    );
    let failure = RuntimeError::new(
        RuntimeErrorKind::Adapter,
        RuntimeStage::SpeechRecognizer,
        "recognition unavailable",
    );
    let events = vec![
        VoiceSessionEvent::SessionStarted {
            session_id,
            privacy,
        },
        VoiceSessionEvent::CapturePaused { session_id },
        VoiceSessionEvent::CaptureResumed { session_id },
        VoiceSessionEvent::VoiceActivity {
            session_id,
            activity: VoiceActivity::SpeechStarted { at_ms: 10 },
        },
        VoiceSessionEvent::TranscriptPartial {
            session_id,
            segment_id: 1,
            text: "hel".to_owned(),
        },
        VoiceSessionEvent::TranscriptFinal {
            session_id,
            segment_id: 1,
            turn_id: Some(turn_id),
            text: "hello".to_owned(),
            capture_evidence: None,
        },
        VoiceSessionEvent::BargeIn {
            session_id,
            turn_id,
            generation_id,
        },
        VoiceSessionEvent::Turn {
            session_id,
            generation_id,
            event: RuntimeEvent::TurnStarted { turn_id },
        },
        VoiceSessionEvent::Turn {
            session_id,
            generation_id,
            event: RuntimeEvent::TextCompleted {
                turn_id,
                text: "exact answer".to_owned(),
            },
        },
        VoiceSessionEvent::Timing {
            session_id,
            turn_id: Some(turn_id),
            milestone: VoiceTimingMilestone::FirstPlayableAudio,
            elapsed_ms: 42,
        },
        VoiceSessionEvent::Playback {
            session_id,
            generation_id,
            state: PlaybackState::Rendered,
        },
        VoiceSessionEvent::SessionFailed {
            session_id,
            error: failure,
            recovery: RecoveryDisposition::ContinueSession,
        },
        VoiceSessionEvent::SessionEnded { session_id },
    ];

    let encoded = events
        .into_iter()
        .map(|event| {
            let event = event.try_into().unwrap();
            gateway_value(&GatewayMessage::VoiceEvent { event })
        })
        .collect::<Vec<_>>();
    let fixture_events = include_str!("../../../tests/fixtures/client-wire-v1/events.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|value| value["type"] == "voice_event")
        .collect::<Vec<_>>();

    assert_eq!(encoded, fixture_events);
}

#[test]
fn public_runtime_errors_do_not_expose_adapter_details() {
    let private_detail = "private provider response body";
    let error = ClientRuntimeError::from(RuntimeError::new(
        RuntimeErrorKind::Adapter,
        RuntimeStage::LanguageModel,
        private_detail,
    ));

    assert_eq!(error.message, "language model operation failed");
    assert!(!error.message.contains(private_detail));
}

#[test]
fn runtime_status_accepts_only_canonical_capability_and_component_combinations() {
    let memory_component = client_component("memory", "Local memory");
    let valid_statuses = [
        RuntimeStatus {
            capabilities: vec!["text".to_owned(), "persona_control".to_owned()],
            ..status()
        },
        RuntimeStatus {
            memory_enabled: true,
            memory_location: Some("local".to_owned()),
            capabilities: vec![
                "text".to_owned(),
                "persona_control".to_owned(),
                "memory_inspection".to_owned(),
                "memory_mutation".to_owned(),
            ],
            components: vec![
                client_component("language_model", "Local language"),
                memory_component.clone(),
            ],
            ..status()
        },
        RuntimeStatus {
            capabilities: vec![
                "text".to_owned(),
                "persona_control".to_owned(),
                "voice_session".to_owned(),
            ],
            components: voice_components(),
            ..status()
        },
        RuntimeStatus {
            memory_enabled: true,
            memory_location: Some("local".to_owned()),
            capabilities: vec![
                "text".to_owned(),
                "persona_control".to_owned(),
                "memory_inspection".to_owned(),
                "memory_mutation".to_owned(),
                "voice_session".to_owned(),
            ],
            components: voice_components()
                .into_iter()
                .chain([memory_component])
                .collect(),
            ..status()
        },
    ];

    let encoded_statuses = valid_statuses
        .iter()
        .map(|status| {
            gateway_value(&GatewayMessage::Ready {
                status: status.clone(),
            })["status"]
                .clone()
        })
        .collect::<Vec<_>>();
    let fixture_statuses = include_str!("../../../tests/fixtures/client-wire-v1/events.jsonl")
        .lines()
        .take(4)
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["status"].clone())
        .collect::<Vec<_>>();

    assert_eq!(encoded_statuses, fixture_statuses);

    for status in &valid_statuses {
        let ready = gateway_value(&GatewayMessage::Ready {
            status: status.clone(),
        });
        let correlated = gateway_value(&GatewayMessage::Status {
            request_id: "canonical-status".to_owned(),
            status: status.clone(),
        });
        assert_eq!(
            ready["status"]["capabilities"],
            correlated["status"]["capabilities"]
        );
    }

    let invalid_statuses = [
        RuntimeStatus {
            capabilities: vec!["voice_session".to_owned(), "text".to_owned()],
            components: voice_components(),
            ..status()
        },
        RuntimeStatus {
            capabilities: vec!["text".to_owned(), "voice_session".to_owned()],
            components: voice_components().into_iter().take(3).collect(),
            ..status()
        },
        RuntimeStatus {
            capabilities: vec!["text".to_owned(), "voice_session".to_owned()],
            components: voice_components()
                .into_iter()
                .map(|mut component| {
                    if component.kind == "audio_io" {
                        component.execution_location = "remote".to_owned();
                    }
                    component
                })
                .collect(),
            ..status()
        },
        RuntimeStatus {
            capabilities: vec!["text".to_owned(), "voice_session".to_owned()],
            components: voice_components()
                .into_iter()
                .map(|mut component| {
                    if component.kind == "audio_io" {
                        component.provider_label = "x".repeat(MAX_CLIENT_PROVIDER_LABEL_BYTES + 1);
                    }
                    component
                })
                .collect(),
            ..status()
        },
        RuntimeStatus {
            components: [client_component("language_model", "Local language")]
                .into_iter()
                .chain(
                    (0..MAX_CLIENT_COMPONENT_DESCRIPTORS)
                        .map(|_| client_component("tool", "Local tool")),
                )
                .collect(),
            ..status()
        },
    ];

    for invalid in invalid_statuses {
        assert!(matches!(
            encode_gateway_message(&GatewayMessage::Ready { status: invalid }),
            Err(ClientWireError::InvalidRuntimeStatus)
        ));
    }
}

#[test]
fn public_voice_string_enums_reject_unknown_values_at_encode_time() {
    let invalid_statuses = [
        RuntimeStatus {
            components: vec![client_component("unknown", "Local language")],
            ..status()
        },
        RuntimeStatus {
            components: vec![ClientComponentDescriptor {
                execution_location: "unknown".to_owned(),
                ..client_component("language_model", "Local language")
            }],
            ..status()
        },
    ];
    for status in invalid_statuses {
        assert!(matches!(
            encode_gateway_message(&GatewayMessage::Ready { status }),
            Err(ClientWireError::InvalidRuntimeStatus)
        ));
    }

    let invalid_events = [
        ClientVoiceSessionEvent::VoiceSessionStarted {
            session_id: SessionId::new(1),
            privacy: ClientPrivacySummary {
                privacy_mode: "unknown".to_owned(),
                components: voice_components(),
            },
        },
        ClientVoiceSessionEvent::VoiceTiming {
            session_id: SessionId::new(1),
            turn_id: None,
            milestone: "unknown_milestone".to_owned(),
            elapsed_ms: 1,
        },
        ClientVoiceSessionEvent::VoicePlayback {
            session_id: SessionId::new(1),
            generation_id: GenerationId::new(1),
            state: "unknown_state".to_owned(),
        },
        ClientVoiceSessionEvent::VoiceSessionFailed {
            session_id: SessionId::new(1),
            error: runtime_error(),
            recovery: "unknown_recovery".to_owned(),
        },
    ];
    for event in invalid_events {
        assert!(matches!(
            encode_gateway_message(&GatewayMessage::VoiceEvent { event }),
            Err(ClientWireError::InvalidVoiceEvent)
        ));
    }
}

#[test]
fn voice_device_status_projects_exact_bounded_labels() {
    let event = ClientVoiceSessionEvent::try_from(VoiceSessionEvent::DeviceStatus {
        session_id: SessionId::new(1),
        input_label: "MacBook Pro Microphone".to_owned(),
        output_label: "Chris 的 AirPods".to_owned(),
    })
    .unwrap();

    assert_eq!(
        gateway_value(&GatewayMessage::VoiceEvent { event }),
        serde_json::json!({
            "protocol_version": 1,
            "type": "voice_event",
            "event": {
                "type": "voice_device_status",
                "session_id": "1",
                "input_label": "MacBook Pro Microphone",
                "output_label": "Chris 的 AirPods"
            }
        })
    );

    assert!(
        ClientVoiceSessionEvent::try_from(VoiceSessionEvent::DeviceStatus {
            session_id: SessionId::new(1),
            input_label: "x".repeat(MAX_CLIENT_DEVICE_LABEL_BYTES + 1),
            output_label: "Speakers".to_owned(),
        },)
        .is_err()
    );
}

#[test]
fn public_voice_timing_and_playback_accept_every_approved_value() {
    for milestone in [
        "speech_end",
        "transcript_final",
        "first_text_delta",
        "first_synthesis_request",
        "first_playable_audio",
        "first_sidecar_accept",
        "playback_render_acknowledged",
        "barge_in_onset",
        "barge_in_threshold",
        "playback_flush_acknowledged",
        "cleanup",
    ] {
        let event = ClientVoiceSessionEvent::VoiceTiming {
            session_id: SessionId::new(1),
            turn_id: None,
            milestone: milestone.to_owned(),
            elapsed_ms: 1,
        };
        encode_gateway_message(&GatewayMessage::VoiceEvent { event }).unwrap();
    }

    for state in ["accepted", "rendered", "flushed"] {
        let event = ClientVoiceSessionEvent::VoicePlayback {
            session_id: SessionId::new(1),
            generation_id: GenerationId::new(1),
            state: state.to_owned(),
        };
        encode_gateway_message(&GatewayMessage::VoiceEvent { event }).unwrap();
    }
}

#[test]
fn production_and_fixture_provider_labels_share_exact_boundary_rules() {
    let cases = [
        ("x".repeat(MAX_CLIENT_PROVIDER_LABEL_BYTES), true),
        ("x".repeat(MAX_CLIENT_PROVIDER_LABEL_BYTES + 1), false),
        (String::new(), false),
        (" Local language".to_owned(), false),
        ("Local language ".to_owned(), false),
        ("\u{2003}Local language".to_owned(), false),
        ("Local language\u{2003}".to_owned(), false),
    ];

    for (provider_label, expected_valid) in cases {
        let production = RuntimeStatus {
            components: vec![client_component("language_model", &provider_label)],
            ..status()
        };
        let production_valid =
            encode_gateway_message(&GatewayMessage::Ready { status: production }).is_ok();

        let mut fixture: serde_json::Value = serde_json::from_str(
            include_str!("../../../tests/fixtures/client-wire-v1/events.jsonl")
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        fixture["status"]["components"][0]["provider_label"] =
            serde_json::Value::String(provider_label.clone());
        let fixture_valid = parse_event_fixture(&fixture.to_string()).is_ok();

        assert_eq!(
            production_valid, expected_valid,
            "production: {provider_label:?}"
        );
        assert_eq!(fixture_valid, expected_valid, "fixture: {provider_label:?}");
        assert_eq!(production_valid, fixture_valid, "{provider_label:?}");
    }
}

#[test]
fn maximum_valid_voice_payload_stays_below_the_frame_limit() {
    let event = VoiceSessionEvent::Turn {
        session_id: SessionId::new(u64::MAX),
        generation_id: GenerationId::new(u64::MAX),
        event: RuntimeEvent::TextCompleted {
            turn_id: TurnId::new(u64::MAX),
            text: "x".repeat(MAX_CONVERSATION_MESSAGE_BYTES),
        },
    }
    .try_into()
    .unwrap();

    let encoded = encode_gateway_message(&GatewayMessage::VoiceEvent { event }).unwrap();

    assert!(encoded.len() < MAX_CLIENT_FRAME_BYTES);
}

#[test]
fn version_one_memory_commands_decode_and_unsupported_versions_are_rejected() {
    let list = decode_client_command(
        br#"{"protocol_version":1,"type":"memory_list","request_id":"req-1","cursor":null}"#,
    )
    .unwrap();
    assert!(matches!(
        list,
        ClientCommand::MemoryList {
            before_id: None,
            ..
        }
    ));

    let inspect = decode_client_command(
        br#"{"protocol_version":1,"type":"memory_inspect","request_id":"req-2","memory_id":"7"}"#,
    )
    .unwrap();
    assert!(matches!(
        inspect,
        ClientCommand::MemoryInspect { memory_id, .. } if memory_id.get() == 7
    ));

    for line in
        include_str!("../../../tests/fixtures/client-wire-unsupported/commands.jsonl").lines()
    {
        assert!(decode_client_command(line.as_bytes()).is_err());
    }
    for line in
        include_str!("../../../tests/fixtures/client-wire-unsupported/invalid.jsonl").lines()
    {
        if line == r#"{"protocol_version":2,"type":"status","request_id":"req-1"}"# {
            assert!(matches!(
                decode_client_command(line.as_bytes()).unwrap(),
                ClientCommand::Status { .. }
            ));
        } else {
            assert!(decode_client_command(line.as_bytes()).is_err());
        }
    }
}

#[test]
fn version_one_persona_and_memory_mutation_commands_decode_and_round_trip() {
    let persona_get = decode_client_command(
        br#"{"protocol_version":1,"type":"persona_get","request_id":"req-1"}"#,
    )
    .unwrap();
    assert!(matches!(persona_get, ClientCommand::PersonaGet { .. }));

    let persona_update = decode_client_command(
        br#"{"protocol_version":1,"type":"persona_update","request_id":"req-2","persona":{"mode":"companionship","warmth":95,"humor":60,"teasing":40,"initiative":35,"directness":80,"intimacy":30,"verbosity":20,"follow_up_frequency":25}}"#,
    )
    .unwrap();
    assert!(matches!(
        persona_update,
        ClientCommand::PersonaUpdate { ref persona, .. }
            if persona.mode == "companionship" && persona.warmth == 95
    ));

    let approve = decode_client_command(
        br#"{"protocol_version":1,"type":"memory_approve","request_id":"req-3","memory_id":"7","expected_revision":"2"}"#,
    )
    .unwrap();
    assert!(matches!(
        approve,
        ClientCommand::MemoryApprove { memory_id, expected_revision, .. }
            if memory_id.get() == 7 && expected_revision == 2
    ));

    let delete = decode_client_command(
        br#"{"protocol_version":1,"type":"memory_delete","request_id":"req-4","memory_id":"7","expected_revision":"2"}"#,
    )
    .unwrap();
    assert!(matches!(
        delete,
        ClientCommand::MemoryDelete { memory_id, expected_revision, .. }
            if memory_id.get() == 7 && expected_revision == 2
    ));

    assert!(matches!(
        decode_client_command(
            br#"{"protocol_version":2,"type":"persona_get","request_id":"req-1"}"#,
        )
        .unwrap(),
        ClientCommand::PersonaGet { .. }
    ));
}

#[test]
fn persona_and_memory_mutation_gateway_messages_encode() {
    let persona = ClientPersonaState::new("companionship", 95, 60, 40, 35, 80, 30, 20, 25).unwrap();

    let value = gateway_value(&GatewayMessage::PersonaState {
        request_id: "req-2".to_owned(),
        persona: persona.clone(),
    });
    assert_eq!(value["type"], "persona_state");
    assert_eq!(value["protocol_version"], 1);
    assert_eq!(value["persona"]["mode"], "companionship");
    assert_eq!(value["persona"]["warmth"], 95);

    let value = gateway_value(&GatewayMessage::MemoryDeleted {
        request_id: "req-4".to_owned(),
        memory_id: MemoryId::new(7).unwrap(),
    });
    assert_eq!(value["type"], "memory_deleted");
    assert_eq!(value["memory_id"], "7");

    let value = gateway_value(&GatewayMessage::MemoryExtracted {
        created: 2,
        activated: 1,
        pending_approval: 1,
    });
    assert_eq!(value["type"], "memory_extracted");
    assert_eq!(value["created"], 2);
    assert_eq!(value["activated"], 1);
    assert_eq!(value["pending_approval"], 1);
}

#[test]
fn gateway_status_capabilities_are_encoded_exactly_as_declared() {
    // The wire advertises what the runtime declared: no capability is spliced in
    // at serialization time, so a runtime that does not declare persona control
    // never advertises it.
    let value = gateway_value(&GatewayMessage::Ready { status: status() });
    assert_eq!(value["status"]["capabilities"], serde_json::json!(["text"]));

    let declared = RuntimeStatus {
        capabilities: vec!["text".to_owned(), "persona_control".to_owned()],
        ..status()
    };
    let value = gateway_value(&GatewayMessage::Status {
        request_id: "req-status".to_owned(),
        status: declared,
    });
    assert_eq!(
        value["status"]["capabilities"],
        serde_json::json!(["text", "persona_control"])
    );
}

#[test]
fn command_rejected_accepts_the_memory_conflict_and_persona_invalid_codes() {
    for code in ["memory_conflict", "persona_invalid"] {
        let value = gateway_value(&GatewayMessage::CommandRejected {
            request_id: "req-1".to_owned(),
            error: ClientRuntimeError {
                code: code.to_owned(),
                kind: "invalid_state".to_owned(),
                stage: "runtime".to_owned(),
                message: "rejected".to_owned(),
            },
        });
        assert_eq!(value["type"], "command_rejected");
        assert_eq!(value["error"]["code"], code);
    }

    // Pins the accepted-code set as closed: an error code outside it must still
    // be rejected at encode time, the same way an unmirrored code would silently
    // desync gateway and client if it slipped onto the wire.
    assert!(matches!(
        encode_gateway_message(&GatewayMessage::CommandRejected {
            request_id: "req-1".to_owned(),
            error: ClientRuntimeError {
                code: "not_a_real_code".to_owned(),
                kind: "invalid_state".to_owned(),
                stage: "runtime".to_owned(),
                message: "rejected".to_owned(),
            },
        }),
        Err(ClientWireError::InvalidRuntimeErrorCode)
    ));
}

#[test]
fn client_persona_state_round_trips_through_persona_profile_and_mode() {
    let profile = PersonaProfile::default();
    let mode = ConversationMode::Brainstorming;
    let state = ClientPersonaState::from_profile(&profile, mode);
    assert_eq!(state.mode, "brainstorming");
    assert_eq!(state.warmth, profile.warmth().get());

    let (round_tripped_profile, round_tripped_mode) = state.to_profile().unwrap();
    assert_eq!(round_tripped_profile, profile);
    assert_eq!(round_tripped_mode, mode);
}

#[test]
fn persona_state_rejects_levels_above_one_hundred_and_unknown_modes() {
    assert!(ClientPersonaState::new("companionship", 101, 0, 0, 0, 0, 0, 0, 0).is_err());
    assert!(ClientPersonaState::new("unknown_mode", 0, 0, 0, 0, 0, 0, 0, 0).is_err());

    assert!(decode_client_command(
        br#"{"protocol_version":1,"type":"persona_update","request_id":"req-1","persona":{"mode":"companionship","warmth":101,"humor":0,"teasing":0,"initiative":0,"directness":0,"intimacy":0,"verbosity":0,"follow_up_frequency":0}}"#
    )
    .is_err());
    assert!(decode_client_command(
        br#"{"protocol_version":1,"type":"persona_update","request_id":"req-1","persona":{"mode":"unknown","warmth":0,"humor":0,"teasing":0,"initiative":0,"directness":0,"intimacy":0,"verbosity":0,"follow_up_frequency":0}}"#
    )
    .is_err());

    let mut invalid_persona =
        ClientPersonaState::new("companionship", 0, 0, 0, 0, 0, 0, 0, 0).unwrap();
    invalid_persona.warmth = 101;
    assert!(matches!(
        encode_gateway_message(&GatewayMessage::PersonaState {
            request_id: "req-1".to_owned(),
            persona: invalid_persona,
        }),
        Err(ClientWireError::InvalidPersonaState)
    ));
}

#[test]
fn memory_mutation_commands_reject_non_numeric_or_zero_expected_revision() {
    assert!(decode_client_command(
        br#"{"protocol_version":1,"type":"memory_approve","request_id":"req-1","memory_id":"7","expected_revision":"abc"}"#
    )
    .is_err());
    assert!(decode_client_command(
        br#"{"protocol_version":1,"type":"memory_delete","request_id":"req-1","memory_id":"7","expected_revision":"0"}"#
    )
    .is_err());
}

#[test]
fn unknown_fields_and_versions_are_rejected() {
    assert!(decode_client_command(
        br#"{"protocol_version":99,"type":"status","request_id":"req-1"}"#
    )
    .is_err());
    assert!(decode_client_command(
        br#"{"protocol_version":1,"type":"status","request_id":"req-1","extra":true}"#
    )
    .is_err());
}

#[test]
fn identifiers_must_be_canonical_non_zero_decimal_strings() {
    for identifier in ["0", "01", "+1", " 1", "1 ", "-1"] {
        let payload = format!(
            r#"{{"protocol_version":1,"type":"interrupt_turn","request_id":"req-1","turn_id":"{identifier}"}}"#
        );
        assert!(
            decode_client_command(payload.as_bytes()).is_err(),
            "{identifier}"
        );
    }

    assert!(decode_client_command(
        br#"{"protocol_version":1,"type":"interrupt_turn","request_id":"req-1","turn_id":1}"#
    )
    .is_err());
}

#[test]
fn commands_reject_invalid_request_ids_and_transcripts() {
    assert!(
        decode_client_command(br#"{"protocol_version":1,"type":"status","request_id":""}"#)
            .is_err()
    );
    assert!(decode_client_command(
        format!(
            r#"{{"protocol_version":1,"type":"status","request_id":"{}"}}"#,
            "r".repeat(65)
        )
        .as_bytes()
    )
    .is_err());
    assert!(decode_client_command(
        br#"{"protocol_version":1,"type":"start_turn","request_id":"req-1","transcript":""}"#
    )
    .is_err());
    assert!(decode_client_command(
        format!(
            r#"{{"protocol_version":1,"type":"start_turn","request_id":"req-1","transcript":"{}"}}"#,
            "x".repeat(16 * 1024 + 1)
        )
        .as_bytes()
    )
    .is_err());
}

#[test]
fn unsupported_version_command_fixtures_are_rejected() {
    let commands = include_str!("../../../tests/fixtures/client-wire-unsupported/commands.jsonl");

    for (line_number, line) in commands.lines().enumerate() {
        assert!(
            decode_client_command(line.as_bytes()).is_err(),
            "unsupported command fixture line {} must be rejected",
            line_number + 1
        );
    }
}

#[test]
fn event_fixtures_reject_numeric_or_malformed_nested_identifiers() {
    for payload in [
        r#"{"protocol_version":1,"type":"runtime_event","event":{"type":"turn_started","turn_id":1}}"#,
        r#"{"protocol_version":1,"type":"runtime_event","event":{"type":"quality_resolved","decision":{"turn_id":"0","mode":"direct_answer","controls":{"maximum_spoken_seconds":20,"directness":80,"pace":"natural","follow_up_policy":"contextual","silence_policy":"allow_without_filler"},"signals":[],"history_message_count":0,"context_sources":["saved_persona","current_turn"]}}}"#,
        r#"{"protocol_version":1,"type":"runtime_event","event":{"type":"memory_retrieved","trace":{"trace_id":"01","turn_id":"1","selected_items":1,"used_bytes":12}}}"#,
    ] {
        assert!(parse_event_fixture(payload).is_err(), "{payload}");
    }
}

#[test]
fn memory_inspection_projects_bounded_identity_history() {
    let created_at = timestamp(9_007_199_254_740_993);
    let first_source = provenance(MemoryProvenanceKind::UserProvided, "voice-turn:7", 900);
    let latest_source = provenance(MemoryProvenanceKind::UserEdited, "memory-control", 901);
    let draft = MemoryDraft::new(
        MemoryKind::Identity,
        "The user prefers concise technical answers.",
        latest_source.clone(),
        MemoryConfidence::new(900).unwrap(),
        created_at,
        MemoryRetention::UntilDeleted,
    )
    .unwrap();
    let first_approval =
        approval("confirmation:1", "local-user", 902, 1).evidence_for(draft.content());
    let latest_approval =
        approval("confirmation:2", "local-user", 903, 2).evidence_for(draft.content());
    let record = MemoryRecord::new(
        MemoryId::new(7).unwrap(),
        draft,
        conversation_protocol::MemoryState::Active,
        timestamp(9_007_199_254_740_994),
        false,
        3,
        Some(latest_approval.clone()),
        None,
        None,
    )
    .unwrap();
    let inspection = MemoryInspection::new(
        record,
        [first_source, latest_source],
        [first_approval, latest_approval],
    )
    .unwrap();

    let wire = ClientMemoryInspection::from(&inspection);

    assert_eq!(wire.record.id, "7");
    assert_eq!(wire.record.revision, "3");
    assert_eq!(wire.record.created_at_ms, "9007199254740993");
    assert_eq!(wire.sources[0].kind, "user_provided");
    assert_eq!(wire.sources[1].kind, "user_edited");
    assert_eq!(wire.approvals.last().unwrap().approved_revision, "2");
    assert!(!wire.sources_truncated);
    assert!(!wire.approvals_truncated);
}

#[test]
fn memory_previews_are_normalized_utf8_safe_and_bounded() {
    assert_eq!(memory_preview("short preview"), "short preview");
    assert_eq!(memory_preview("\t你好\u{3000}世界\n"), "你好 世界");
    assert_eq!(memory_preview(" alpha\n beta\t gamma "), "alpha beta gamma");

    let exact = "a".repeat(MAX_MEMORY_PREVIEW_BYTES);
    assert_eq!(memory_preview(&exact), exact);

    let truncated = format!("{}x", "你".repeat(64));
    let preview = memory_preview(&truncated);
    assert_eq!(preview, format!("{}…", "你".repeat(63)));
    assert_eq!(preview.len(), MAX_MEMORY_PREVIEW_BYTES);
}

#[test]
fn maximum_memory_inspection_response_stays_within_frame_limit() {
    let content = "c".repeat(4 * 1024);
    let source_id = "s".repeat(512);
    let actor = "a".repeat(256);
    let confirmation_id = "c".repeat(512);
    let sources = (1..=32)
        .map(|index| {
            MemoryProvenance::new(
                MemoryProvenanceKind::UserEdited,
                source_id.clone(),
                timestamp(1_000 + index),
                actor.clone(),
                None,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    let draft = MemoryDraft::new(
        MemoryKind::Semantic,
        content,
        sources.last().unwrap().clone(),
        MemoryConfidence::new(1_000).unwrap(),
        timestamp(1_000),
        MemoryRetention::UntilDeleted,
    )
    .unwrap();
    let approvals = (1..=32)
        .map(|index| {
            approval(
                confirmation_id.clone(),
                actor.clone(),
                1_100 + index,
                index as u64,
            )
            .evidence_for(draft.content())
        })
        .collect::<Vec<_>>();
    let record = MemoryRecord::new(
        MemoryId::new(u64::MAX).unwrap(),
        draft,
        conversation_protocol::MemoryState::Active,
        timestamp(1_200),
        true,
        u64::MAX,
        approvals.last().cloned(),
        Some(timestamp(1_201)),
        Some(MemoryRetrievalReason::ExactPhrase),
    )
    .unwrap();
    let inspection = MemoryInspection::new(record, sources, approvals).unwrap();
    let encoded = encode_gateway_message(&GatewayMessage::MemoryInspection {
        request_id: "r".repeat(64),
        inspection: ClientMemoryInspection::from(&inspection),
    })
    .unwrap();

    assert!(encoded.len() < MAX_CLIENT_FRAME_BYTES);
}

#[test]
fn version_one_fixtures_parse_and_invalid_cases_are_rejected() {
    for (line_number, line) in include_str!("../../../tests/fixtures/client-wire-v1/commands.jsonl")
        .lines()
        .enumerate()
    {
        decode_client_command(line.as_bytes())
            .unwrap_or_else(|error| panic!("command fixture line {}: {error}", line_number + 1));
    }

    for (line_number, line) in include_str!("../../../tests/fixtures/client-wire-v1/events.jsonl")
        .lines()
        .enumerate()
    {
        parse_event_fixture(line)
            .unwrap_or_else(|error| panic!("event fixture line {}: {error}", line_number + 1));
    }

    for (line_number, line) in include_str!("../../../tests/fixtures/client-wire-v1/invalid.jsonl")
        .lines()
        .enumerate()
    {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        if value["protocol_version"] == CONTEXT_SEED_PROTOCOL_VERSION
            && matches!(
                value["type"].as_str(),
                Some("status" | "start_voice_session")
            )
        {
            assert!(decode_client_command(line.as_bytes()).is_ok());
            assert!(parse_event_fixture(line).is_err());
            continue;
        }
        assert!(
            decode_client_command(line.as_bytes()).is_err() && parse_event_fixture(line).is_err(),
            "invalid fixture line {} must be rejected by every v1 envelope",
            line_number + 1
        );
    }
}

#[test]
fn runtime_events_project_content_free_fields_and_decimal_identifiers() {
    let turn_id = TurnId::new(7);
    let decision = QualityDecision::new(
        turn_id,
        ConversationMode::Reflective,
        ResponseControls::new(
            8,
            PersonaLevel::new(90).unwrap(),
            SpeechPace::Measured,
            FollowUpPolicy::Never,
            SilencePolicy::AllowWithoutFiller,
        )
        .unwrap(),
        [ConversationSignal::ShorterRequested],
        4,
        [ContextSource::SavedPersona, ContextSource::CurrentTurn],
    )
    .unwrap();
    let trace = MemoryRetrievalTrace::new(
        RetrievalTraceId::new(11).unwrap(),
        turn_id,
        UnixTimestampMillis::new(1_000).unwrap(),
        [MemoryTraceItem::new(
            0,
            MemoryId::new(1).unwrap(),
            MemoryKind::Semantic,
            MemoryRetrievalReason::ExactPhrase,
            12,
        )
        .unwrap()],
        MemoryTraceExclusions::default(),
    )
    .unwrap();

    let quality = ClientRuntimeEvent::try_from(RuntimeEvent::QualityResolved { decision }).unwrap();
    let memory = ClientRuntimeEvent::try_from(RuntimeEvent::MemoryRetrieved { trace }).unwrap();
    let quality_value = gateway_value(&GatewayMessage::RuntimeEvent { event: quality });
    let memory_value = gateway_value(&GatewayMessage::RuntimeEvent { event: memory });

    assert_eq!(quality_value["event"]["decision"]["turn_id"], "7");
    assert_eq!(quality_value["event"]["decision"]["mode"], "reflective");
    assert_eq!(
        quality_value["event"]["decision"]["controls"]["maximum_spoken_seconds"],
        8
    );
    assert_eq!(
        quality_value["event"]["decision"]["signals"],
        serde_json::json!(["shorter_requested"])
    );
    assert_eq!(memory_value["event"]["trace"]["trace_id"], "11");
    assert_eq!(memory_value["event"]["trace"]["turn_id"], "7");
    assert_eq!(memory_value["event"]["trace"]["selected_items"], 1);
    assert_eq!(memory_value["event"]["trace"]["used_bytes"], 12);
    assert!(ClientRuntimeEvent::try_from(RuntimeEvent::SpeechStarted { turn_id }).is_err());
}

#[test]
fn root_runtime_messages_reject_voice_only_event_variants() {
    for event in [
        ClientRuntimeEvent::TranscriptFinal {
            turn_id: TurnId::new(1),
            text: "hello".to_owned(),
        },
        ClientRuntimeEvent::SpeechStarted {
            turn_id: TurnId::new(1),
        },
        ClientRuntimeEvent::SpeechCompleted {
            turn_id: TurnId::new(1),
        },
    ] {
        assert!(matches!(
            encode_gateway_message(&GatewayMessage::RuntimeEvent { event }),
            Err(ClientWireError::UnsupportedRuntimeEvent { .. })
        ));
    }
}

#[test]
fn shared_transcription_v4_vectors_encode_without_losing_source_or_unknown_evidence() {
    let vectors = include_str!("../../../tests/fixtures/transcription-v4/events.jsonl");
    let source = conversation_protocol::SourceSampleRange {
        clock_domain_id: "source-process".into(),
        capture_epoch: 1,
        first_frame_sequence: 7,
        last_frame_sequence: 8,
        source_start_sample: 9007199254740993,
        source_end_sample: 9007199254741017,
        sample_rate_hz: 24000,
    };
    let evidence = [
        conversation_protocol::UtteranceCaptureEvidence {
            capture: Some(source.clone()),
            live_coverage: Some(conversation_protocol::WholeFrameNominalEnvelope {
                host_start_ns: 1000000000,
                host_end_ns: 2000000000,
                first_frame_host_end_ns: 1500000000,
                capture_uncertainty_ns: None,
                known_rounding_allowance_ns: 1,
            }),
            capture_unavailable_reason: None,
        },
        conversation_protocol::UtteranceCaptureEvidence {
            capture: Some(conversation_protocol::SourceSampleRange {
                first_frame_sequence: 9,
                last_frame_sequence: 9,
                source_start_sample: 9007199254741017,
                source_end_sample: 9007199254741041,
                ..source
            }),
            live_coverage: None,
            capture_unavailable_reason: Some(
                conversation_protocol::CaptureUnavailableReason::CaptureTimingMissing,
            ),
        },
        conversation_protocol::UtteranceCaptureEvidence::unavailable(
            conversation_protocol::CaptureUnavailableReason::CaptureBoundaryEvicted,
        ),
    ];
    for ((line, evidence), (segment_id, text)) in vectors.lines().zip(evidence).zip([
        (9, "wording"),
        (10, "timing unavailable"),
        (11, "boundary unavailable"),
    ]) {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        let event = ClientVoiceSessionEvent::try_from(VoiceSessionEvent::TranscriptFinal {
            session_id: SessionId::new(7),
            segment_id,
            turn_id: None,
            text: text.into(),
            capture_evidence: Some(Box::new(evidence)),
        })
        .unwrap();
        let encoded =
            encode_gateway_message_for_version(&GatewayMessage::VoiceEvent { event }, 4).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&encoded).unwrap(),
            value
        );
    }
}

#[test]
fn finalized_capture_evidence_is_v4_only_and_legacy_transcript_bytes_are_unchanged() {
    let event = ClientVoiceSessionEvent::try_from(VoiceSessionEvent::TranscriptFinal {
        session_id: SessionId::new(7),
        segment_id: 9,
        turn_id: Some(TurnId::new(11)),
        text: "wording".into(),
        capture_evidence: Some(Box::new(conversation_protocol::UtteranceCaptureEvidence {
            capture: Some(conversation_protocol::SourceSampleRange {
                clock_domain_id: "source-process".into(),
                capture_epoch: 1,
                first_frame_sequence: 7,
                last_frame_sequence: 8,
                source_start_sample: 9007199254740993,
                source_end_sample: 9007199254741017,
                sample_rate_hz: 24000,
            }),
            live_coverage: None,
            capture_unavailable_reason: Some(
                conversation_protocol::CaptureUnavailableReason::CaptureTimingMissing,
            ),
        })),
    })
    .unwrap();
    let message = GatewayMessage::VoiceEvent { event };
    for version in [1, 2, 3, 4] {
        let encoded = encode_gateway_message_for_version(&message, version).unwrap();
        let decoded: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        if version == 4 {
            assert_eq!(
                decoded["event"]["capture"]["source_start_sample"],
                "9007199254740993"
            );
            assert_eq!(
                decoded["event"]["capture_unavailable_reason"],
                "capture_timing_missing"
            );
        } else {
            let expected = if version == 3 {
                serde_json::json!({"type":"voice_transcript_final","session_id":"7","segment_id":"9","turn_id":"11","text":"wording"})
            } else {
                serde_json::json!({"type":"voice_transcript_final","session_id":"7","turn_id":"11","text":"wording"})
            };
            assert_eq!(decoded["event"], expected);
        }
    }
    for version in [1, 2] {
        let link = GatewayMessage::VoiceEvent {
            event: ClientVoiceSessionEvent::ResponseAuthorized {
                session_id: SessionId::new(7),
                segment_id: 9,
                turn_id: TurnId::new(11),
            },
        };
        assert!(encode_gateway_message_for_version(&link, version).is_err());
    }
}

#[test]
fn voice_transcript_and_response_fields_enforce_conversation_bounds() {
    let oversized = "x".repeat(MAX_CONVERSATION_MESSAGE_BYTES + 1);
    for event in [
        VoiceSessionEvent::TranscriptPartial {
            session_id: SessionId::new(1),
            segment_id: 1,
            text: oversized.clone(),
        },
        VoiceSessionEvent::TranscriptFinal {
            session_id: SessionId::new(1),
            segment_id: 1,
            turn_id: Some(TurnId::new(1)),
            text: oversized.clone(),
            capture_evidence: None,
        },
        VoiceSessionEvent::Turn {
            session_id: SessionId::new(1),
            generation_id: GenerationId::new(1),
            event: RuntimeEvent::TextCompleted {
                turn_id: TurnId::new(1),
                text: oversized,
            },
        },
    ] {
        assert!(ClientVoiceSessionEvent::try_from(event).is_err());
    }
}

#[test]
fn encoded_messages_never_use_numeric_u64_ids_and_are_frame_bounded() {
    let encoded = encode_gateway_message(&GatewayMessage::RuntimeEvent {
        event: ClientRuntimeEvent::TurnCompleted {
            turn_id: TurnId::new(u64::MAX),
        },
    })
    .unwrap();

    assert!(std::str::from_utf8(&encoded)
        .unwrap()
        .contains("\"18446744073709551615\""));
    assert_eq!(MAX_CLIENT_FRAME_BYTES, 512 * 1024);

    let oversized = GatewayMessage::RuntimeEvent {
        event: ClientRuntimeEvent::TurnFailed {
            turn_id: TurnId::new(1),
            error: ClientRuntimeError {
                code: "adapter_failure".to_owned(),
                kind: "adapter".to_owned(),
                stage: "language_model".to_owned(),
                message: "x".repeat(MAX_CLIENT_FRAME_BYTES),
            },
        },
    };
    assert!(encode_gateway_message(&oversized).is_err());
}

#[test]
fn outgoing_response_messages_reject_invalid_request_ids() {
    for request_id in [String::new(), "r".repeat(65)] {
        for message in [
            GatewayMessage::CommandAccepted {
                request_id: request_id.clone(),
                turn_id: None,
                media_lease: None,
            },
            GatewayMessage::CommandRejected {
                request_id: request_id.clone(),
                error: runtime_error(),
            },
            GatewayMessage::Status {
                request_id: request_id.clone(),
                status: status(),
            },
        ] {
            assert!(matches!(
                encode_gateway_message(&message),
                Err(ClientWireError::InvalidRequestId)
            ));
        }
    }
}

#[test]
fn outgoing_status_rejects_incoherent_memory_capabilities() {
    let valid_memory_status = RuntimeStatus {
        memory_enabled: true,
        memory_location: Some("local".to_owned()),
        capabilities: vec!["text".to_owned(), "memory_inspection".to_owned()],
        components: vec![
            client_component("language_model", "Local language"),
            client_component("memory", "Local memory"),
        ],
        ..status()
    };
    assert!(encode_gateway_message(&GatewayMessage::Ready {
        status: valid_memory_status,
    })
    .is_ok());

    let invalid_statuses = [
        RuntimeStatus {
            memory_enabled: false,
            memory_location: Some("local".to_owned()),
            ..status()
        },
        RuntimeStatus {
            capabilities: vec!["text".to_owned(), "memory_inspection".to_owned()],
            ..status()
        },
        RuntimeStatus {
            memory_enabled: true,
            capabilities: vec!["text".to_owned(), "memory_inspection".to_owned()],
            ..status()
        },
        RuntimeStatus {
            memory_enabled: true,
            ..status()
        },
        RuntimeStatus {
            memory_enabled: true,
            memory_location: Some("local".to_owned()),
            ..status()
        },
        RuntimeStatus {
            memory_enabled: true,
            memory_location: Some("remote".to_owned()),
            capabilities: vec!["text".to_owned(), "memory_inspection".to_owned()],
            ..status()
        },
    ];

    for invalid_status in invalid_statuses {
        assert!(encode_gateway_message(&GatewayMessage::Ready {
            status: invalid_status.clone(),
        })
        .is_err());
        assert!(encode_gateway_message(&GatewayMessage::Status {
            request_id: "req-status".to_owned(),
            status: invalid_status,
        })
        .is_err());
    }
}

#[test]
fn outgoing_memory_numbers_enforce_u64_i64_and_confidence_bounds() {
    for (id, updated_at_ms) in [
        ("0", "1"),
        ("18446744073709551616", "1"),
        ("7", "9223372036854775808"),
    ] {
        let mut summary = memory_summary(7);
        summary.id = id.to_owned();
        summary.updated_at_ms = updated_at_ms.to_owned();
        assert!(matches!(
            encode_gateway_message(&GatewayMessage::MemoryList {
                request_id: "req-list".to_owned(),
                records: vec![summary],
                next_cursor: None,
            }),
            Err(ClientWireError::InvalidMemoryResponse)
        ));
    }

    let valid = client_memory_inspection();
    assert!(encode_gateway_message(&GatewayMessage::MemoryInspection {
        request_id: "req-inspect".to_owned(),
        inspection: valid.clone(),
    })
    .is_ok());

    for confidence in ["0", "1000"] {
        let mut inspection = valid.clone();
        inspection.record.confidence = confidence.to_owned();
        assert!(encode_gateway_message(&GatewayMessage::MemoryInspection {
            request_id: "req-inspect".to_owned(),
            inspection,
        })
        .is_ok());
    }

    let mut invalid_values = Vec::new();
    let mut inspection = valid.clone();
    inspection.record.id = "0".to_owned();
    invalid_values.push(inspection);
    let mut inspection = valid.clone();
    inspection.record.id = "18446744073709551616".to_owned();
    invalid_values.push(inspection);
    let mut inspection = valid.clone();
    inspection.record.revision = "0".to_owned();
    invalid_values.push(inspection);
    let mut inspection = valid.clone();
    inspection.record.revision = "18446744073709551616".to_owned();
    invalid_values.push(inspection);
    let mut inspection = valid.clone();
    inspection.record.updated_at_ms = "9223372036854775808".to_owned();
    invalid_values.push(inspection);
    let mut inspection = valid.clone();
    inspection.record.retention = ClientMemoryRetention::Session {
        session_id: "0".to_owned(),
    };
    invalid_values.push(inspection);
    let mut inspection = valid.clone();
    inspection.record.retention = ClientMemoryRetention::Session {
        session_id: "18446744073709551616".to_owned(),
    };
    invalid_values.push(inspection);
    let mut inspection = valid;
    inspection.record.confidence = "1001".to_owned();
    invalid_values.push(inspection);

    for inspection in invalid_values {
        assert!(matches!(
            encode_gateway_message(&GatewayMessage::MemoryInspection {
                request_id: "req-inspect".to_owned(),
                inspection,
            }),
            Err(ClientWireError::InvalidMemoryResponse)
        ));
    }
}

#[test]
fn outgoing_memory_lists_require_descending_keyset_pages() {
    let invalid_pages = [
        (vec![memory_summary(7), memory_summary(9)], None),
        (vec![memory_summary(7), memory_summary(7)], None),
        (
            vec![memory_summary(9), memory_summary(7)],
            Some(ClientMemoryCursor {
                before_id: "9".to_owned(),
            }),
        ),
        (
            Vec::new(),
            Some(ClientMemoryCursor {
                before_id: "7".to_owned(),
            }),
        ),
    ];

    let rejected = invalid_pages.map(|(records, next_cursor)| {
        matches!(
            encode_gateway_message(&GatewayMessage::MemoryList {
                request_id: "req-list".to_owned(),
                records,
                next_cursor,
            }),
            Err(ClientWireError::InvalidMemoryResponse)
        )
    });

    assert_eq!(rejected, [true; 4]);
}

#[test]
fn outgoing_events_reject_zero_turn_identifiers_in_every_wire_position() {
    let zero_turn_id = TurnId::new(0);
    let controls = ClientResponseControls {
        maximum_spoken_seconds: 20,
        directness: 80,
        pace: "natural".to_owned(),
        follow_up_policy: "contextual".to_owned(),
        silence_policy: "allow_without_filler".to_owned(),
    };
    let messages = [
        GatewayMessage::RuntimeEvent {
            event: ClientRuntimeEvent::TurnStarted {
                request_id: None,
                turn_id: zero_turn_id,
            },
        },
        GatewayMessage::RuntimeEvent {
            event: ClientRuntimeEvent::TextDelta {
                turn_id: zero_turn_id,
                delta: "hello".to_owned(),
            },
        },
        GatewayMessage::RuntimeEvent {
            event: ClientRuntimeEvent::Timing {
                turn_id: zero_turn_id,
                milestone: "first_text_delta".to_owned(),
                elapsed_ms: 42,
            },
        },
        GatewayMessage::RuntimeEvent {
            event: ClientRuntimeEvent::TurnCompleted {
                turn_id: zero_turn_id,
            },
        },
        GatewayMessage::RuntimeEvent {
            event: ClientRuntimeEvent::TurnCancelled {
                turn_id: zero_turn_id,
            },
        },
        GatewayMessage::RuntimeEvent {
            event: ClientRuntimeEvent::TurnFailed {
                turn_id: zero_turn_id,
                error: runtime_error(),
            },
        },
        GatewayMessage::RuntimeEvent {
            event: ClientRuntimeEvent::QualityResolved {
                decision: ClientQualityDecision {
                    turn_id: zero_turn_id,
                    mode: "direct_answer".to_owned(),
                    controls,
                    signals: Vec::new(),
                    history_message_count: 0,
                    context_sources: vec!["current_turn".to_owned()],
                },
            },
        },
        GatewayMessage::RuntimeEvent {
            event: ClientRuntimeEvent::MemoryRetrieved {
                trace: ClientMemoryTrace {
                    trace_id: RetrievalTraceId::new(1).unwrap(),
                    turn_id: zero_turn_id,
                    selected_items: 0,
                    used_bytes: 0,
                },
            },
        },
    ];

    for message in messages {
        assert!(matches!(
            encode_gateway_message(&message),
            Err(ClientWireError::InvalidIdentifier)
        ));
    }
}

fn parse_event_fixture(payload: &str) -> Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_str(payload).map_err(|error| error.to_string())?;
    validate_required_nullable_keys(&value)?;
    let message: FixtureGatewayMessage =
        serde_json::from_str(payload).map_err(|error| error.to_string())?;
    if message.protocol_version() != LEGACY_CLIENT_PROTOCOL_VERSION {
        return Err("unsupported protocol version".to_owned());
    }
    message.validate()?;
    Ok(())
}

fn validate_required_nullable_keys(value: &serde_json::Value) -> Result<(), String> {
    let Some(message) = value.as_object() else {
        return Err("gateway message is not an object".to_owned());
    };
    if message.get("type").and_then(serde_json::Value::as_str) != Some("voice_event") {
        return Ok(());
    }
    let event = message
        .get("event")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "voice event is not an object".to_owned())?;
    match event.get("type").and_then(serde_json::Value::as_str) {
        Some("voice_turn_event") => {
            let nested = event
                .get("event")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| "voice turn event is not an object".to_owned())?;
            if nested.get("type").and_then(serde_json::Value::as_str) == Some("turn_started")
                && !nested.contains_key("request_id")
            {
                return Err("voice turn start is missing request_id".to_owned());
            }
        }
        Some("voice_timing") if !event.contains_key("turn_id") => {
            return Err("voice timing is missing turn_id".to_owned());
        }
        _ => {}
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[allow(dead_code)]
enum FixtureGatewayMessage {
    Ready {
        protocol_version: u64,
        status: FixtureRuntimeStatus,
    },
    CommandAccepted {
        protocol_version: u64,
        request_id: FixtureRequestId,
        #[serde(default)]
        turn_id: FixtureOptionalIdentifier,
    },
    CommandRejected {
        protocol_version: u64,
        request_id: FixtureRequestId,
        error: FixtureRuntimeError,
    },
    Status {
        protocol_version: u64,
        request_id: FixtureRequestId,
        status: FixtureRuntimeStatus,
    },
    MemoryList {
        protocol_version: u64,
        request_id: FixtureRequestId,
        records: Vec<FixtureMemorySummary>,
        next_cursor: Option<FixtureMemoryCursor>,
    },
    MemoryInspection {
        protocol_version: u64,
        request_id: FixtureRequestId,
        inspection: FixtureMemoryInspection,
    },
    RuntimeEvent {
        protocol_version: u64,
        event: FixtureRuntimeEvent,
    },
    VoiceEvent {
        protocol_version: u64,
        event: FixtureVoiceSessionEvent,
    },
    Fatal {
        protocol_version: u64,
        error: FixtureRuntimeError,
    },
    PersonaState {
        protocol_version: u64,
        request_id: FixtureRequestId,
        persona: FixturePersonaState,
    },
    MemoryDeleted {
        protocol_version: u64,
        request_id: FixtureRequestId,
        memory_id: FixtureIdentifier,
    },
    MemoryExtracted {
        protocol_version: u64,
        created: u32,
        activated: u32,
        pending_approval: u32,
    },
}

impl FixtureGatewayMessage {
    fn protocol_version(&self) -> u64 {
        match self {
            Self::Ready {
                protocol_version, ..
            }
            | Self::CommandAccepted {
                protocol_version, ..
            }
            | Self::CommandRejected {
                protocol_version, ..
            }
            | Self::Status {
                protocol_version, ..
            }
            | Self::MemoryList {
                protocol_version, ..
            }
            | Self::MemoryInspection {
                protocol_version, ..
            }
            | Self::RuntimeEvent {
                protocol_version, ..
            }
            | Self::VoiceEvent {
                protocol_version, ..
            }
            | Self::Fatal {
                protocol_version, ..
            }
            | Self::PersonaState {
                protocol_version, ..
            }
            | Self::MemoryDeleted {
                protocol_version, ..
            }
            | Self::MemoryExtracted {
                protocol_version, ..
            } => *protocol_version,
        }
    }

    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Ready { status, .. } | Self::Status { status, .. } => status.validate(),
            Self::MemoryList { records, .. } if records.len() > 50 => {
                Err("memory list exceeds 50 records".to_owned())
            }
            Self::MemoryInspection { inspection, .. } => inspection.validate(),
            Self::RuntimeEvent { event, .. } => event.validate(true),
            Self::VoiceEvent { event, .. } => event.validate(),
            _ => Ok(()),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FixtureRuntimeStatus {
    transport: String,
    privacy_mode: String,
    language_location: String,
    model_id: String,
    memory_enabled: bool,
    memory_location: Option<String>,
    telemetry_enabled: bool,
    capabilities: Vec<String>,
    components: Vec<FixtureComponentDescriptor>,
}

impl FixtureRuntimeStatus {
    fn validate(&self) -> Result<(), String> {
        if self.transport != "stdio"
            || self.privacy_mode != "local_only"
            || self.language_location != "local"
            || self.model_id.trim().is_empty()
            || self.telemetry_enabled
        {
            return Err("runtime status enum is invalid".to_owned());
        }
        validate_fixture_components(&self.components)?;
        if self
            .components
            .iter()
            .any(|component| component.execution_location != FixtureExecutionLocation::Local)
            || self
                .components
                .iter()
                .filter(|component| component.kind == FixtureComponentKind::LanguageModel)
                .count()
                != 1
        {
            return Err("runtime components are invalid".to_owned());
        }

        let memory = self.capabilities
            == [
                "text",
                "persona_control",
                "memory_inspection",
                "memory_mutation",
            ]
            || self.capabilities
                == [
                    "text",
                    "persona_control",
                    "memory_inspection",
                    "memory_mutation",
                    "voice_session",
                ];
        let voice = self.capabilities == ["text", "persona_control", "voice_session"]
            || self.capabilities
                == [
                    "text",
                    "persona_control",
                    "memory_inspection",
                    "memory_mutation",
                    "voice_session",
                ];
        if self.capabilities != ["text", "persona_control"]
            && self.capabilities
                != [
                    "text",
                    "persona_control",
                    "memory_inspection",
                    "memory_mutation",
                ]
            && self.capabilities != ["text", "persona_control", "voice_session"]
            && self.capabilities
                != [
                    "text",
                    "persona_control",
                    "memory_inspection",
                    "memory_mutation",
                    "voice_session",
                ]
        {
            return Err("runtime capabilities are not canonical".to_owned());
        }
        let memory_descriptors = self
            .components
            .iter()
            .filter(|component| component.kind == FixtureComponentKind::Memory)
            .count();
        if memory
            != (self.memory_enabled
                && self.memory_location.as_deref() == Some("local")
                && memory_descriptors == 1)
            || (!memory
                && (self.memory_enabled
                    || self.memory_location.is_some()
                    || memory_descriptors != 0))
        {
            return Err("runtime memory status is incoherent".to_owned());
        }
        if voice {
            validate_fixture_voice_components(&self.privacy_mode, &self.components)?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureComponentDescriptor {
    kind: FixtureComponentKind,
    execution_location: FixtureExecutionLocation,
    provider_label: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
enum FixtureComponentKind {
    SpeechRecognition,
    LanguageModel,
    SpeechSynthesis,
    AudioIo,
    Tool,
    Memory,
    Telemetry,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum FixtureExecutionLocation {
    Local,
    Remote,
}

fn validate_fixture_components(components: &[FixtureComponentDescriptor]) -> Result<(), String> {
    if components.is_empty() || components.len() > 32 {
        return Err("component descriptor count is invalid".to_owned());
    }
    for pair in components.windows(2) {
        if pair[0].kind > pair[1].kind {
            return Err("component descriptors are not canonical".to_owned());
        }
    }
    if components.iter().any(|component| {
        let trimmed_provider_label = component.provider_label.trim();
        trimmed_provider_label.is_empty()
            || trimmed_provider_label != component.provider_label
            || component.provider_label.len() > MAX_CLIENT_PROVIDER_LABEL_BYTES
    }) {
        return Err("component provider label is invalid".to_owned());
    }
    Ok(())
}

fn validate_fixture_voice_components(
    privacy_mode: &str,
    components: &[FixtureComponentDescriptor],
) -> Result<(), String> {
    validate_fixture_components(components)?;
    if privacy_mode != "local_only"
        || components
            .iter()
            .any(|component| component.execution_location != FixtureExecutionLocation::Local)
    {
        return Err("voice privacy is not local only".to_owned());
    }
    for kind in [
        FixtureComponentKind::SpeechRecognition,
        FixtureComponentKind::LanguageModel,
        FixtureComponentKind::SpeechSynthesis,
        FixtureComponentKind::AudioIo,
    ] {
        if components
            .iter()
            .filter(|component| component.kind == kind)
            .count()
            != 1
        {
            return Err("voice component is missing or duplicated".to_owned());
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FixtureRuntimeError {
    code: FixtureRuntimeErrorCode,
    kind: String,
    stage: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum FixtureRuntimeErrorCode {
    AdapterFailure,
    ConfigurationInvalid,
    InvalidState,
    MemoryDisabled,
    MemoryTurnActive,
    MemoryNotFound,
    MemoryUnavailable,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[allow(dead_code)]
enum FixtureRuntimeEvent {
    TurnStarted {
        request_id: FixtureNullableRequestId,
        turn_id: FixtureIdentifier,
    },
    TranscriptFinal {
        turn_id: FixtureIdentifier,
        text: String,
    },
    QualityResolved {
        decision: FixtureQualityDecision,
    },
    MemoryRetrieved {
        trace: FixtureMemoryTrace,
    },
    TextDelta {
        turn_id: FixtureIdentifier,
        delta: String,
    },
    TextCompleted {
        turn_id: FixtureIdentifier,
        text: String,
    },
    SpeechStarted {
        turn_id: FixtureIdentifier,
    },
    SpeechCompleted {
        turn_id: FixtureIdentifier,
    },
    Timing {
        turn_id: FixtureIdentifier,
        milestone: String,
        elapsed_ms: u64,
    },
    TurnCompleted {
        turn_id: FixtureIdentifier,
    },
    TurnCancelled {
        turn_id: FixtureIdentifier,
    },
    TurnFailed {
        turn_id: FixtureIdentifier,
        error: FixtureRuntimeError,
    },
}

impl FixtureRuntimeEvent {
    fn turn_id(&self) -> u64 {
        match self {
            Self::TurnStarted { turn_id, .. }
            | Self::TranscriptFinal { turn_id, .. }
            | Self::TextDelta { turn_id, .. }
            | Self::TextCompleted { turn_id, .. }
            | Self::SpeechStarted { turn_id }
            | Self::SpeechCompleted { turn_id }
            | Self::Timing { turn_id, .. }
            | Self::TurnCompleted { turn_id }
            | Self::TurnCancelled { turn_id }
            | Self::TurnFailed { turn_id, .. } => turn_id.0,
            Self::QualityResolved { decision } => decision.turn_id.0,
            Self::MemoryRetrieved { trace } => trace.turn_id.0,
        }
    }

    fn validate(&self, typed: bool) -> Result<(), String> {
        match self {
            Self::TurnStarted { request_id, .. } if typed != request_id.is_some() => {
                Err("turn start correlation is invalid".to_owned())
            }
            Self::TranscriptFinal { text, .. } | Self::TextCompleted { text, .. }
                if text.is_empty() || text.len() > MAX_CONVERSATION_MESSAGE_BYTES =>
            {
                Err("final text is invalid".to_owned())
            }
            Self::TextDelta { delta, .. } if delta.len() > MAX_CONVERSATION_MESSAGE_BYTES => {
                Err("text delta is oversized".to_owned())
            }
            Self::Timing { milestone, .. }
                if !matches!(
                    milestone.as_str(),
                    "first_text_delta" | "first_synthesis_request" | "first_playable_audio"
                ) =>
            {
                Err("runtime timing milestone is invalid".to_owned())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[allow(dead_code)]
enum FixtureVoiceSessionEvent {
    #[serde(rename = "voice_session_started")]
    SessionStarted {
        session_id: FixtureIdentifier,
        privacy: FixturePrivacySummary,
    },
    #[serde(rename = "voice_capture_paused")]
    CapturePaused { session_id: FixtureIdentifier },
    #[serde(rename = "voice_capture_resumed")]
    CaptureResumed { session_id: FixtureIdentifier },
    #[serde(rename = "voice_activity")]
    Activity {
        session_id: FixtureIdentifier,
        activity: FixtureVoiceActivity,
    },
    #[serde(rename = "voice_transcript_partial")]
    TranscriptPartial {
        session_id: FixtureIdentifier,
        segment_id: FixtureIdentifier,
        text: String,
    },
    #[serde(rename = "voice_transcript_final")]
    TranscriptFinal {
        session_id: FixtureIdentifier,
        turn_id: FixtureIdentifier,
        text: String,
    },
    #[serde(rename = "voice_barge_in")]
    BargeIn {
        session_id: FixtureIdentifier,
        turn_id: FixtureIdentifier,
        generation_id: FixtureIdentifier,
    },
    #[serde(rename = "voice_turn_event")]
    TurnEvent {
        session_id: FixtureIdentifier,
        generation_id: FixtureIdentifier,
        event: FixtureRuntimeEvent,
    },
    #[serde(rename = "voice_timing")]
    Timing {
        session_id: FixtureIdentifier,
        turn_id: FixtureNullableIdentifier,
        milestone: FixtureVoiceTimingMilestone,
        elapsed_ms: u64,
    },
    #[serde(rename = "voice_playback")]
    Playback {
        session_id: FixtureIdentifier,
        generation_id: FixtureIdentifier,
        state: FixturePlaybackState,
    },
    #[serde(rename = "voice_session_failed")]
    SessionFailed {
        session_id: FixtureIdentifier,
        error: FixtureRuntimeError,
        recovery: FixtureRecoveryDisposition,
    },
    #[serde(rename = "voice_session_ended")]
    SessionEnded { session_id: FixtureIdentifier },
}

impl FixtureVoiceSessionEvent {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::SessionStarted { privacy, .. } => privacy.validate(),
            Self::TranscriptPartial { text, .. }
                if text.is_empty() || text.len() > MAX_CONVERSATION_MESSAGE_BYTES =>
            {
                Err("partial transcript is invalid".to_owned())
            }
            Self::TranscriptFinal { text, .. }
                if text.is_empty() || text.len() > MAX_CONVERSATION_MESSAGE_BYTES =>
            {
                Err("final transcript is invalid".to_owned())
            }
            Self::BargeIn {
                turn_id,
                generation_id,
                ..
            } if turn_id.0 != generation_id.0 => Err("barge-in identity mismatch".to_owned()),
            Self::TurnEvent {
                generation_id,
                event,
                ..
            } if generation_id.0 != event.turn_id() => {
                Err("voice turn identity mismatch".to_owned())
            }
            Self::TurnEvent { event, .. } => event.validate(false),
            _ => Ok(()),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixturePrivacySummary {
    privacy_mode: String,
    components: Vec<FixtureComponentDescriptor>,
}

impl FixturePrivacySummary {
    fn validate(&self) -> Result<(), String> {
        validate_fixture_voice_components(&self.privacy_mode, &self.components)
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[allow(dead_code)]
enum FixtureVoiceActivity {
    #[serde(rename = "speech_started")]
    Started { at_ms: u64 },
    #[serde(rename = "speech_continued")]
    Continued { at_ms: u64 },
    #[serde(rename = "speech_ended")]
    Ended { at_ms: u64 },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum FixtureVoiceTimingMilestone {
    SpeechEnd,
    TranscriptFinal,
    FirstTextDelta,
    FirstSynthesisRequest,
    FirstPlayableAudio,
    FirstSidecarAccept,
    PlaybackRenderAcknowledged,
    BargeInOnset,
    BargeInThreshold,
    PlaybackFlushAcknowledged,
    Cleanup,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum FixturePlaybackState {
    Accepted,
    Rendered,
    Flushed,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum FixtureRecoveryDisposition {
    ContinueSession,
    NewSession,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FixtureQualityDecision {
    turn_id: FixtureIdentifier,
    mode: String,
    controls: FixtureResponseControls,
    signals: Vec<String>,
    history_message_count: usize,
    context_sources: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FixtureResponseControls {
    maximum_spoken_seconds: u16,
    directness: u8,
    pace: String,
    follow_up_policy: String,
    silence_policy: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FixtureMemoryTrace {
    trace_id: FixtureIdentifier,
    turn_id: FixtureIdentifier,
    selected_items: usize,
    used_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FixtureMemorySummary {
    id: FixtureIdentifier,
    content_preview: FixtureMemoryPreview,
    kind: FixtureMemoryKind,
    state: FixtureMemoryState,
    pinned: bool,
    updated_at_ms: FixtureTimestamp,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FixtureMemoryCursor {
    before_id: FixtureIdentifier,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FixturePersonaState {
    mode: FixtureConversationMode,
    warmth: FixturePersonaLevel,
    humor: FixturePersonaLevel,
    teasing: FixturePersonaLevel,
    initiative: FixturePersonaLevel,
    directness: FixturePersonaLevel,
    intimacy: FixturePersonaLevel,
    verbosity: FixturePersonaLevel,
    follow_up_frequency: FixturePersonaLevel,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum FixtureConversationMode {
    DirectAnswer,
    Companionship,
    Brainstorming,
    Reflective,
}

struct FixturePersonaLevel;

impl<'de> Deserialize<'de> for FixturePersonaLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        if value > 100 {
            return Err(serde::de::Error::custom("persona level exceeds 100"));
        }
        Ok(Self)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FixtureMemoryInspection {
    record: FixtureMemoryRecord,
    sources: Vec<FixtureMemoryProvenance>,
    approvals: Vec<FixtureMemoryApproval>,
    sources_truncated: bool,
    approvals_truncated: bool,
}

impl FixtureMemoryInspection {
    fn validate(&self) -> Result<(), String> {
        if self.sources.len() > 32 || self.approvals.len() > 32 {
            return Err("memory inspection history exceeds 32 entries".to_owned());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FixtureMemoryRecord {
    id: FixtureIdentifier,
    kind: FixtureMemoryKind,
    content: String,
    state: FixtureMemoryState,
    confidence: FixtureConfidence,
    created_at_ms: FixtureTimestamp,
    updated_at_ms: FixtureTimestamp,
    pinned: bool,
    revision: FixtureIdentifier,
    retention: FixtureMemoryRetention,
    last_used_at_ms: Option<FixtureTimestamp>,
    last_retrieval_reason: Option<FixtureMemoryRetrievalReason>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FixtureMemoryProvenance {
    kind: FixtureMemoryProvenanceKind,
    source_id: String,
    source_timestamp_ms: FixtureTimestamp,
    actor: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FixtureMemoryApproval {
    confirmation_id: String,
    actor: String,
    confirmed_at_ms: FixtureTimestamp,
    approved_revision: FixtureIdentifier,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum FixtureMemoryKind {
    Working,
    Episodic,
    Semantic,
    Identity,
    Relationship,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum FixtureMemoryState {
    Candidate,
    Active,
    Expired,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum FixtureMemoryProvenanceKind {
    UserProvided,
    UserEdited,
    CompletedExchange,
    ApplicationImported,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum FixtureMemoryRetrievalReason {
    PinnedMatch,
    ExactPhrase,
    SharedTerm,
    RecentWorking,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[allow(dead_code)]
enum FixtureMemoryRetention {
    Working { expires_at_ms: FixtureTimestamp },
    Session { session_id: FixtureIdentifier },
    Until { expires_at_ms: FixtureTimestamp },
    UntilDeleted,
}

struct FixtureMemoryPreview;

impl<'de> Deserialize<'de> for FixtureMemoryPreview {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.len() > MAX_MEMORY_PREVIEW_BYTES {
            return Err(serde::de::Error::custom("memory preview exceeds 192 bytes"));
        }
        Ok(Self)
    }
}

struct FixtureConfidence;

impl<'de> Deserialize<'de> for FixtureConfidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if !is_canonical_decimal(&value)
            || value
                .parse::<u16>()
                .map_or(true, |confidence| confidence > 1_000)
        {
            return Err(serde::de::Error::custom("invalid memory confidence"));
        }
        Ok(Self)
    }
}

struct FixtureTimestamp;

impl<'de> Deserialize<'de> for FixtureTimestamp {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if !is_canonical_decimal(&value) || value.parse::<i64>().is_err() {
            return Err(serde::de::Error::custom("invalid decimal timestamp"));
        }
        Ok(Self)
    }
}

struct FixtureRequestId;

impl<'de> Deserialize<'de> for FixtureRequestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() || value.len() > 64 {
            return Err(serde::de::Error::custom("invalid request identifier"));
        }
        Ok(Self)
    }
}

struct FixtureNullableRequestId(Option<FixtureRequestId>);

impl FixtureNullableRequestId {
    fn is_some(&self) -> bool {
        self.0.is_some()
    }
}

impl<'de> Deserialize<'de> for FixtureNullableRequestId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<String>::deserialize(deserializer)?;
        match value {
            Some(value) if !value.is_empty() && value.len() <= 64 => {
                Ok(Self(Some(FixtureRequestId)))
            }
            Some(_) => Err(serde::de::Error::custom("invalid request identifier")),
            None => Ok(Self(None)),
        }
    }
}

struct FixtureIdentifier(u64);

#[derive(Default)]
#[allow(dead_code)]
struct FixtureOptionalIdentifier(Option<FixtureIdentifier>);

impl<'de> Deserialize<'de> for FixtureOptionalIdentifier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        FixtureIdentifier::deserialize(deserializer).map(|identifier| Self(Some(identifier)))
    }
}

#[allow(dead_code)]
struct FixtureNullableIdentifier(Option<FixtureIdentifier>);

impl<'de> Deserialize<'de> for FixtureNullableIdentifier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer)?
            .map(|value| parse_fixture_identifier::<D::Error>(&value))
            .transpose()
            .map(Self)
    }
}

fn is_canonical_decimal(value: &str) -> bool {
    !value.is_empty()
        && (value == "0" || !value.starts_with('0'))
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

impl<'de> Deserialize<'de> for FixtureIdentifier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_fixture_identifier::<D::Error>(&value)
    }
}

fn parse_fixture_identifier<E>(value: &str) -> Result<FixtureIdentifier, E>
where
    E: serde::de::Error,
{
    if value.is_empty()
        || value == "0"
        || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(E::custom("invalid decimal identifier"));
    }
    value
        .parse()
        .map(FixtureIdentifier)
        .map_err(|_| E::custom("identifier exceeds u64"))
}

fn remote_client_component(kind: &str, provider_label: &str) -> ClientComponentDescriptor {
    ClientComponentDescriptor {
        kind: kind.to_owned(),
        execution_location: "remote".to_owned(),
        provider_label: provider_label.to_owned(),
    }
}

fn online_status() -> RuntimeStatus {
    RuntimeStatus {
        profile: "online".to_owned(),
        privacy_mode: "cloud".to_owned(),
        language_location: "remote".to_owned(),
        components: vec![remote_client_component("language_model", "Cloud language")],
        ..status()
    }
}

#[test]
fn legacy_status_payloads_omit_profile_and_report_offline_on_version_three() {
    let message = GatewayMessage::Ready { status: status() };

    for version in [
        LEGACY_CLIENT_PROTOCOL_VERSION,
        CONTEXT_SEED_PROTOCOL_VERSION,
    ] {
        let value = gateway_value_for_version(&message, version);
        assert!(
            !value["status"].as_object().unwrap().contains_key("profile"),
            "protocol version {version} must not carry a profile field"
        );
    }

    assert_eq!(
        gateway_value_for_version(&message, LEGACY_CLIENT_PROTOCOL_VERSION),
        serde_json::json!({
            "protocol_version": 1,
            "type": "ready",
            "status": {
                "transport": "stdio",
                "privacy_mode": "local_only",
                "language_location": "local",
                "model_id": "local-model",
                "memory_enabled": false,
                "memory_location": serde_json::Value::Null,
                "telemetry_enabled": false,
                "capabilities": ["text"],
                "components": [{
                    "kind": "language_model",
                    "execution_location": "local",
                    "provider_label": "Local language",
                }],
            },
        })
    );

    let v3 = gateway_value_for_version(&message, RUNTIME_PROFILE_PROTOCOL_VERSION);
    assert_eq!(v3["protocol_version"], 3);
    assert_eq!(v3["status"]["profile"], "offline");
    assert_eq!(v3["status"]["privacy_mode"], "local_only");
    assert_eq!(
        v3["status"]["last_context_seed_operation_id"],
        serde_json::Value::Null
    );
}

#[test]
fn online_profile_reports_remote_execution_truthfully() {
    let value = gateway_value_for_version(
        &GatewayMessage::Ready {
            status: online_status(),
        },
        RUNTIME_PROFILE_PROTOCOL_VERSION,
    );

    assert_eq!(value["status"]["profile"], "online");
    assert_eq!(value["status"]["privacy_mode"], "cloud");
    assert_eq!(value["status"]["language_location"], "remote");
    assert_eq!(
        value["status"]["components"][0]["execution_location"],
        "remote"
    );
}

#[test]
fn online_status_is_refused_on_versions_that_cannot_state_a_profile() {
    for version in [
        LEGACY_CLIENT_PROTOCOL_VERSION,
        CONTEXT_SEED_PROTOCOL_VERSION,
    ] {
        assert!(
            matches!(
                encode_gateway_message_for_version(
                    &GatewayMessage::Ready {
                        status: online_status()
                    },
                    version,
                ),
                Err(ClientWireError::InvalidRuntimeStatus)
            ),
            "an online status must not be encodable at version {version}"
        );
    }
}

#[test]
fn online_profile_rejects_local_only_privacy_mode() {
    let status = RuntimeStatus {
        privacy_mode: "local_only".to_owned(),
        ..online_status()
    };

    assert!(matches!(
        encode_gateway_message_for_version(
            &GatewayMessage::Ready { status },
            RUNTIME_PROFILE_PROTOCOL_VERSION,
        ),
        Err(ClientWireError::InvalidRuntimeStatus)
    ));
}

#[test]
fn online_profile_requires_at_least_one_remote_component() {
    let status = RuntimeStatus {
        language_location: "local".to_owned(),
        components: vec![client_component("language_model", "Local language")],
        ..online_status()
    };

    assert!(matches!(
        encode_gateway_message_for_version(
            &GatewayMessage::Ready { status },
            RUNTIME_PROFILE_PROTOCOL_VERSION,
        ),
        Err(ClientWireError::InvalidRuntimeStatus)
    ));
}

#[test]
fn offline_profile_rejects_any_remote_component() {
    let remote_tool = RuntimeStatus {
        components: vec![
            client_component("language_model", "Local language"),
            remote_client_component("tool", "Cloud tool"),
        ],
        ..status()
    };
    assert!(matches!(
        encode_gateway_message_for_version(
            &GatewayMessage::Ready {
                status: remote_tool
            },
            RUNTIME_PROFILE_PROTOCOL_VERSION,
        ),
        Err(ClientWireError::InvalidRuntimeStatus)
    ));

    let status = RuntimeStatus {
        language_location: "remote".to_owned(),
        components: vec![remote_client_component("language_model", "Cloud language")],
        ..status()
    };

    for version in [
        LEGACY_CLIENT_PROTOCOL_VERSION,
        CONTEXT_SEED_PROTOCOL_VERSION,
        RUNTIME_PROFILE_PROTOCOL_VERSION,
    ] {
        assert!(matches!(
            encode_gateway_message_for_version(
                &GatewayMessage::Ready {
                    status: status.clone()
                },
                version,
            ),
            Err(ClientWireError::InvalidRuntimeStatus)
        ));
    }
}

#[test]
fn offline_profile_rejects_non_local_privacy_modes() {
    for privacy_mode in ["hybrid", "cloud"] {
        let status = RuntimeStatus {
            privacy_mode: privacy_mode.to_owned(),
            ..status()
        };
        assert!(matches!(
            encode_gateway_message_for_version(
                &GatewayMessage::Ready { status },
                RUNTIME_PROFILE_PROTOCOL_VERSION,
            ),
            Err(ClientWireError::InvalidRuntimeStatus)
        ));
    }
}

#[test]
fn unknown_profiles_and_privacy_modes_are_rejected() {
    for profile in ["", "meeting", "Offline", "online "] {
        let status = RuntimeStatus {
            profile: profile.to_owned(),
            ..status()
        };
        assert!(
            matches!(
                encode_gateway_message_for_version(
                    &GatewayMessage::Ready { status },
                    RUNTIME_PROFILE_PROTOCOL_VERSION,
                ),
                Err(ClientWireError::InvalidRuntimeStatus)
            ),
            "profile {profile:?} must be rejected"
        );
    }

    let status = RuntimeStatus {
        privacy_mode: "public".to_owned(),
        ..online_status()
    };
    assert!(matches!(
        encode_gateway_message_for_version(
            &GatewayMessage::Ready { status },
            RUNTIME_PROFILE_PROTOCOL_VERSION,
        ),
        Err(ClientWireError::InvalidRuntimeStatus)
    ));
}

#[test]
fn language_location_must_match_the_language_model_component() {
    let online_claiming_local = RuntimeStatus {
        language_location: "local".to_owned(),
        ..online_status()
    };
    assert!(matches!(
        encode_gateway_message_for_version(
            &GatewayMessage::Ready {
                status: online_claiming_local
            },
            RUNTIME_PROFILE_PROTOCOL_VERSION,
        ),
        Err(ClientWireError::InvalidRuntimeStatus)
    ));

    let offline_claiming_remote = RuntimeStatus {
        language_location: "remote".to_owned(),
        ..status()
    };
    assert!(matches!(
        encode_gateway_message_for_version(
            &GatewayMessage::Ready {
                status: offline_claiming_remote
            },
            RUNTIME_PROFILE_PROTOCOL_VERSION,
        ),
        Err(ClientWireError::InvalidRuntimeStatus)
    ));
}

#[test]
fn online_voice_status_may_report_remote_components() {
    let status = RuntimeStatus {
        capabilities: vec!["text".to_owned(), "voice_session".to_owned()],
        components: vec![
            remote_client_component("speech_recognition", "Cloud speech recognition"),
            remote_client_component("language_model", "Cloud language"),
            remote_client_component("speech_synthesis", "Cloud speech synthesis"),
            client_component("audio_io", "Local audio"),
        ],
        ..online_status()
    };

    assert!(encode_gateway_message_for_version(
        &GatewayMessage::Ready {
            status: status.clone()
        },
        RUNTIME_PROFILE_PROTOCOL_VERSION,
    )
    .is_ok());

    let local_only = RuntimeStatus {
        profile: "offline".to_owned(),
        privacy_mode: "local_only".to_owned(),
        language_location: "local".to_owned(),
        ..status
    };
    assert!(matches!(
        encode_gateway_message_for_version(
            &GatewayMessage::Ready { status: local_only },
            RUNTIME_PROFILE_PROTOCOL_VERSION,
        ),
        Err(ClientWireError::InvalidRuntimeStatus)
    ));
}

#[test]
fn protocol_version_three_is_supported_and_unknown_versions_are_rejected() {
    let payload = format!(
        r#"{{"protocol_version":{RUNTIME_PROFILE_PROTOCOL_VERSION},"type":"status","request_id":"req-1"}}"#
    );
    assert!(matches!(
        decode_client_command(payload.as_bytes()).unwrap(),
        ClientCommand::Status { .. }
    ));

    assert!(matches!(
        decode_client_command(br#"{"protocol_version":5,"type":"status","request_id":"req-1"}"#),
        Err(ClientWireError::UnsupportedProtocolVersion(5))
    ));
    assert!(matches!(
        decode_client_command(br#"{"protocol_version":0,"type":"status","request_id":"req-1"}"#),
        Err(ClientWireError::UnsupportedProtocolVersion(0))
    ));
}

#[test]
fn context_seeding_stays_available_from_version_two_onwards() {
    for version in [
        CONTEXT_SEED_PROTOCOL_VERSION,
        RUNTIME_PROFILE_PROTOCOL_VERSION,
    ] {
        let payload = format!(
            r#"{{"protocol_version":{version},"type":"seed_conversation_context","request_id":"request-1","operation_id":"continue-1","exchanges":[{{"user":"hello","assistant":"hi"}}]}}"#
        );
        assert!(matches!(
            decode_client_command(payload.as_bytes()).unwrap(),
            ClientCommand::SeedConversationContext { .. }
        ));

        let seeded = RuntimeStatus {
            capabilities: vec!["text".to_owned(), "conversation_context_seed".to_owned()],
            last_context_seed_operation_id: Some("continue-1".to_owned()),
            ..status()
        };
        let value = gateway_value_for_version(&GatewayMessage::Ready { status: seeded }, version);
        assert_eq!(
            value["status"]["last_context_seed_operation_id"],
            "continue-1"
        );
    }

    assert!(matches!(
        decode_client_command(
            br#"{"protocol_version":1,"type":"seed_conversation_context","request_id":"request-1","operation_id":"continue-1","exchanges":[{"user":"hello","assistant":"hi"}]}"#,
        ),
        Err(ClientWireError::UnsupportedProtocolVersion(1))
    ));

    let seeded = RuntimeStatus {
        capabilities: vec!["text".to_owned(), "conversation_context_seed".to_owned()],
        ..status()
    };
    assert!(matches!(
        encode_gateway_message_for_version(
            &GatewayMessage::Ready { status: seeded },
            LEGACY_CLIENT_PROTOCOL_VERSION,
        ),
        Err(ClientWireError::InvalidRuntimeStatus)
    ));
}

fn recording_summary() -> ClientRecordingSummary {
    ClientRecordingSummary {
        recording_handle: "0123456789abcdef0123456789abcdef".to_owned(),
        encrypted_path: "/var/recordings/0123456789abcdef0123456789abcdef.crsr".to_owned(),
        sha256: "a".repeat(64),
        duration_ms: 61_000,
        format: "pcm_s16le/16000/1".to_owned(),
        key_id: "0011223344556677".to_owned(),
        gaps: vec![ClientRecordingGap {
            started_at_ms: 1_200,
            ended_at_ms: Some(1_800),
            reason: "capture_discontinuity".to_owned(),
        }],
    }
}

#[test]
fn finalize_recording_is_a_version_three_command_only() {
    let payload = |version: u64| {
        format!(
            r#"{{"protocol_version":{version},"type":"finalize_recording","request_id":"r-1","session_id":"7"}}"#
        )
    };

    assert_eq!(
        decode_client_command(payload(RUNTIME_PROFILE_PROTOCOL_VERSION).as_bytes()).unwrap(),
        ClientCommand::FinalizeRecording {
            request_id: "r-1".to_owned(),
            session_id: SessionId::new(7),
        }
    );
    for version in [
        LEGACY_CLIENT_PROTOCOL_VERSION,
        CONTEXT_SEED_PROTOCOL_VERSION,
    ] {
        assert!(matches!(
            decode_client_command(payload(version).as_bytes()),
            Err(ClientWireError::UnsupportedProtocolVersion(_))
        ));
    }
    // Unsupported versions stay unsupported rather than becoming a recording command.
    assert!(matches!(
        decode_client_command(payload(5).as_bytes()),
        Err(ClientWireError::UnsupportedProtocolVersion(5))
    ));
    // A zero session identifier is not canonical and never reaches the gateway.
    assert!(decode_client_command(
        br#"{"protocol_version":3,"type":"finalize_recording","request_id":"r-1","session_id":"0"}"#
    )
    .is_err());
}

#[test]
fn a_finalized_recording_states_its_handle_only_from_version_three() {
    let message = GatewayMessage::RecordingFinalized {
        request_id: "r-1".to_owned(),
        recording: recording_summary(),
    };

    let encoded =
        encode_gateway_message_for_version(&message, RUNTIME_PROFILE_PROTOCOL_VERSION).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "protocol_version": 3,
            "type": "recording_finalized",
            "request_id": "r-1",
            "recording": {
                "recording_handle": "0123456789abcdef0123456789abcdef",
                "encrypted_path": "/var/recordings/0123456789abcdef0123456789abcdef.crsr",
                "sha256": "a".repeat(64),
                "duration_ms": "61000",
                "format": "pcm_s16le/16000/1",
                "key_id": "0011223344556677",
                "gaps": [{
                    "started_at_ms": "1200",
                    "ended_at_ms": "1800",
                    "reason": "capture_discontinuity",
                }],
            },
        }),
        "64-bit values travel as canonical decimal strings, never JSON numbers"
    );

    for version in [
        LEGACY_CLIENT_PROTOCOL_VERSION,
        CONTEXT_SEED_PROTOCOL_VERSION,
    ] {
        assert!(matches!(
            encode_gateway_message_for_version(&message, version),
            Err(ClientWireError::UnsupportedProtocolVersion(_))
        ));
    }
}

#[test]
fn an_open_gap_and_an_empty_recording_are_expressible() {
    let message = GatewayMessage::RecordingFinalized {
        request_id: "r-1".to_owned(),
        recording: ClientRecordingSummary {
            duration_ms: 0,
            format: "none".to_owned(),
            gaps: vec![ClientRecordingGap {
                started_at_ms: 0,
                ended_at_ms: None,
                reason: "capture_discontinuity".to_owned(),
            }],
            ..recording_summary()
        },
    };
    let encoded =
        encode_gateway_message_for_version(&message, RUNTIME_PROFILE_PROTOCOL_VERSION).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(value["recording"]["duration_ms"], "0");
    assert_eq!(value["recording"]["gaps"][0]["started_at_ms"], "0");
    assert_eq!(
        value["recording"]["gaps"][0]["ended_at_ms"],
        serde_json::Value::Null
    );
}

#[test]
fn recording_summaries_fail_closed_on_values_the_sink_cannot_produce() {
    let refuse = |recording: ClientRecordingSummary| {
        assert!(
            matches!(
                encode_gateway_message_for_version(
                    &GatewayMessage::RecordingFinalized {
                        request_id: "r-1".to_owned(),
                        recording,
                    },
                    RUNTIME_PROFILE_PROTOCOL_VERSION,
                ),
                Err(ClientWireError::InvalidRecordingSummary)
            ),
            "an invalid recording summary must fail closed"
        );
    };

    refuse(ClientRecordingSummary {
        recording_handle: String::new(),
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        recording_handle: "../escape".to_owned(),
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        recording_handle: "A".repeat(32),
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        encrypted_path: String::new(),
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        encrypted_path: "/tmp/\u{0}.crsr".to_owned(),
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        encrypted_path: format!("/{}", "p".repeat(1024)),
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        sha256: "a".repeat(63),
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        sha256: "A".repeat(64),
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        format: String::new(),
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        format: "PCM".to_owned(),
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        key_id: String::new(),
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        key_id: "not-hex".to_owned(),
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        gaps: vec![ClientRecordingGap {
            started_at_ms: 10,
            ended_at_ms: Some(9),
            reason: "capture_discontinuity".to_owned(),
        }],
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        gaps: vec![ClientRecordingGap {
            started_at_ms: 10,
            ended_at_ms: Some(20),
            reason: "muted".to_owned(),
        }],
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        gaps: vec![ClientRecordingGap {
            started_at_ms: 99_000,
            ended_at_ms: None,
            reason: "capture_discontinuity".to_owned(),
        }],
        ..recording_summary()
    });
    refuse(ClientRecordingSummary {
        gaps: (0..=MAX_CLIENT_RECORDING_GAPS)
            .map(|index| ClientRecordingGap {
                started_at_ms: index as u64,
                ended_at_ms: None,
                reason: "capture_discontinuity".to_owned(),
            })
            .collect(),
        ..recording_summary()
    });
}

#[test]
fn reconcile_recording_is_a_version_three_command_only() {
    let payload = |version: u64| {
        format!(
            r#"{{"protocol_version":{version},"type":"reconcile_recording","request_id":"r-1","session_id":"7","recording_handle":"0123456789abcdef0123456789abcdef"}}"#
        )
    };

    assert_eq!(
        decode_client_command(payload(RUNTIME_PROFILE_PROTOCOL_VERSION).as_bytes()).unwrap(),
        ClientCommand::ReconcileRecording {
            request_id: "r-1".to_owned(),
            session_id: SessionId::new(7),
            recording_handle: "0123456789abcdef0123456789abcdef".to_owned(),
        }
    );
    for version in [
        LEGACY_CLIENT_PROTOCOL_VERSION,
        CONTEXT_SEED_PROTOCOL_VERSION,
    ] {
        assert!(matches!(
            decode_client_command(payload(version).as_bytes()),
            Err(ClientWireError::UnsupportedProtocolVersion(_))
        ));
    }
    assert!(matches!(
        decode_client_command(payload(5).as_bytes()),
        Err(ClientWireError::UnsupportedProtocolVersion(5))
    ));
    // A handle that is not a handle never reaches the runtime's resolver.
    for handle in ["", "../escape", "A".repeat(32).as_str(), &"a".repeat(129)] {
        assert!(
            decode_client_command(
                format!(
                    r#"{{"protocol_version":3,"type":"reconcile_recording","request_id":"r-1","session_id":"7","recording_handle":"{handle}"}}"#
                )
                .as_bytes()
            )
            .is_err(),
            "handle {handle:?} must be refused at decode"
        );
    }
}

#[test]
fn a_reconciled_recording_states_anonymous_clusters_only_from_version_three() {
    let message = GatewayMessage::RecordingReconciled {
        request_id: "r-1".to_owned(),
        reconciliation: reconciliation(),
    };

    let encoded =
        encode_gateway_message_for_version(&message, RUNTIME_PROFILE_PROTOCOL_VERSION).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "protocol_version": 3,
            "type": "recording_reconciled",
            "request_id": "r-1",
            "reconciliation": {
                "segments": [
                    {
                        "segment_id": "seg-1",
                        "started_at_ms": "0",
                        "ended_at_ms": "1200",
                        "text": "good morning",
                        "speaker_cluster": "cluster-1",
                    },
                    {
                        "segment_id": "seg-2",
                        "started_at_ms": "1200",
                        "ended_at_ms": "61000",
                        "text": "and now the agenda",
                        "speaker_cluster": null,
                    },
                ],
                "warnings": ["recording_truncated"],
            },
        }),
        "64-bit values travel as canonical decimal strings, never JSON numbers"
    );

    for version in [
        LEGACY_CLIENT_PROTOCOL_VERSION,
        CONTEXT_SEED_PROTOCOL_VERSION,
    ] {
        assert!(matches!(
            encode_gateway_message_for_version(&message, version),
            Err(ClientWireError::UnsupportedProtocolVersion(_))
        ));
    }
}

#[test]
fn an_empty_reconciliation_is_expressible() {
    let encoded = encode_gateway_message_for_version(
        &GatewayMessage::RecordingReconciled {
            request_id: "r-1".to_owned(),
            reconciliation: ClientRecordingReconciliation {
                segments: Vec::new(),
                warnings: vec!["no_speech_detected".to_owned()],
            },
        },
        RUNTIME_PROFILE_PROTOCOL_VERSION,
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(
        value["reconciliation"]["segments"],
        serde_json::json!([]),
        "a meeting nobody spoke in still has a shape"
    );
}

#[test]
fn reconciliations_fail_closed_on_values_the_runtime_cannot_produce() {
    let refuse = |reconciliation: ClientRecordingReconciliation, why: &str| {
        assert!(
            matches!(
                encode_gateway_message_for_version(
                    &GatewayMessage::RecordingReconciled {
                        request_id: "r-1".to_owned(),
                        reconciliation,
                    },
                    RUNTIME_PROFILE_PROTOCOL_VERSION,
                ),
                Err(ClientWireError::InvalidRecordingReconciliation)
            ),
            "{why}"
        );
    };
    let with = |segment: ClientDiarizedSegment| ClientRecordingReconciliation {
        segments: vec![segment],
        warnings: Vec::new(),
    };

    // A person's name is exactly what a cluster label must never be.
    refuse(
        with(ClientDiarizedSegment {
            speaker_cluster: Some("Chris Xiong".to_owned()),
            ..segment()
        }),
        "a provider identity is not an anonymous cluster",
    );
    refuse(
        with(ClientDiarizedSegment {
            speaker_cluster: Some("cluster-0".to_owned()),
            ..segment()
        }),
        "cluster numbering starts at one",
    );
    refuse(
        with(ClientDiarizedSegment {
            speaker_cluster: Some(String::new()),
            ..segment()
        }),
        "an empty cluster label is not absence of a cluster",
    );
    refuse(
        with(ClientDiarizedSegment {
            started_at_ms: 900,
            ended_at_ms: 800,
            ..segment()
        }),
        "a segment cannot end before it starts",
    );
    refuse(
        with(ClientDiarizedSegment {
            segment_id: String::new(),
            ..segment()
        }),
        "a segment must be identifiable",
    );
    refuse(
        with(ClientDiarizedSegment {
            segment_id: "SEG-1".to_owned(),
            ..segment()
        }),
        "identifiers are lowercase",
    );
    refuse(
        with(ClientDiarizedSegment {
            text: "line\nbreak".to_owned(),
            ..segment()
        }),
        "a control character would multiply the encoded size",
    );
    refuse(
        with(ClientDiarizedSegment {
            text: "x".repeat(MAX_CLIENT_DIARIZED_SEGMENT_TEXT_BYTES + 1),
            ..segment()
        }),
        "one segment cannot exceed the text bound",
    );
    refuse(
        ClientRecordingReconciliation {
            segments: vec![segment(), segment()],
            warnings: Vec::new(),
        },
        "a segment identifier names one segment",
    );
    refuse(
        ClientRecordingReconciliation {
            segments: (0..=MAX_CLIENT_DIARIZED_SEGMENTS)
                .map(|index| ClientDiarizedSegment {
                    segment_id: format!("seg-{index}"),
                    ..segment()
                })
                .collect(),
            warnings: Vec::new(),
        },
        "the segment count is bounded",
    );
    refuse(
        ClientRecordingReconciliation {
            segments: (0..128)
                .map(|index| ClientDiarizedSegment {
                    segment_id: format!("seg-{index}"),
                    text: "x".repeat(MAX_CLIENT_DIARIZED_SEGMENT_TEXT_BYTES),
                    ..segment()
                })
                .collect(),
            warnings: Vec::new(),
        },
        "the whole answer's text is bounded",
    );
    refuse(
        ClientRecordingReconciliation {
            segments: Vec::new(),
            warnings: vec!["the provider was unhappy".to_owned()],
        },
        "warnings come from a closed list",
    );
    refuse(
        ClientRecordingReconciliation {
            segments: Vec::new(),
            warnings: vec!["no_speech_detected".to_owned(); 2],
        },
        "a warning is stated once",
    );
}

fn segment() -> ClientDiarizedSegment {
    ClientDiarizedSegment {
        segment_id: "seg-1".to_owned(),
        started_at_ms: 0,
        ended_at_ms: 1_200,
        text: "good morning".to_owned(),
        speaker_cluster: Some("cluster-1".to_owned()),
    }
}

fn reconciliation() -> ClientRecordingReconciliation {
    ClientRecordingReconciliation {
        segments: vec![
            segment(),
            ClientDiarizedSegment {
                segment_id: "seg-2".to_owned(),
                started_at_ms: 1_200,
                ended_at_ms: 61_000,
                text: "and now the agenda".to_owned(),
                speaker_cluster: None,
            },
        ],
        warnings: vec!["recording_truncated".to_owned()],
    }
}

#[test]
fn process_text_is_a_version_three_command_only() {
    let payload = |version: u64| {
        format!(
            r#"{{"protocol_version":{version},"type":"process_text","request_id":"p-1","session_id":"7","instructions":"summarise","input":"the meeting","output_schema":{{"type":"object"}},"max_output_bytes":4096}}"#
        )
    };

    assert_eq!(
        decode_client_command(payload(RUNTIME_PROFILE_PROTOCOL_VERSION).as_bytes()).unwrap(),
        ClientCommand::ProcessText {
            request_id: "p-1".to_owned(),
            session_id: SessionId::new(7),
            instructions: "summarise".to_owned(),
            input: "the meeting".to_owned(),
            output_schema: r#"{"type":"object"}"#.to_owned(),
            max_output_bytes: 4096,
        }
    );
    for version in [
        LEGACY_CLIENT_PROTOCOL_VERSION,
        CONTEXT_SEED_PROTOCOL_VERSION,
    ] {
        assert!(matches!(
            decode_client_command(payload(version).as_bytes()),
            Err(ClientWireError::UnsupportedProtocolVersion(_))
        ));
    }
    assert!(matches!(
        decode_client_command(payload(5).as_bytes()),
        Err(ClientWireError::UnsupportedProtocolVersion(5))
    ));
}

#[test]
fn a_text_processing_command_the_runtime_cannot_serve_is_refused_at_decode() {
    let command = |body: &str| {
        format!(
            r#"{{"protocol_version":3,"type":"process_text","request_id":"p-1","session_id":"7",{body}}}"#
        )
    };
    for body in [
        // An output schema must be an object; an array or a string is not a schema.
        r#""instructions":"s","input":"i","output_schema":["type"],"max_output_bytes":4096"#,
        r#""instructions":"s","input":"i","output_schema":"object","max_output_bytes":4096"#,
        // Empty instructions or input cannot produce a structured answer.
        r#""instructions":"","input":"i","output_schema":{"type":"object"},"max_output_bytes":4096"#,
        r#""instructions":"s","input":"","output_schema":{"type":"object"},"max_output_bytes":4096"#,
        // A zero or unbounded output limit is not a limit.
        r#""instructions":"s","input":"i","output_schema":{"type":"object"},"max_output_bytes":0"#,
        r#""instructions":"s","input":"i","output_schema":{"type":"object"},"max_output_bytes":4294967296"#,
        // Every field is required.
        r#""instructions":"s","input":"i","output_schema":{"type":"object"}"#,
    ] {
        assert!(
            decode_client_command(command(body).as_bytes()).is_err(),
            "a text-processing command with {body} must be refused at decode"
        );
    }
}

#[test]
fn processed_text_is_stated_only_from_version_three() {
    let message = GatewayMessage::TextProcessed {
        request_id: "p-1".to_owned(),
        processed: ClientProcessedText {
            output: r#"{"decisions":[]}"#.to_owned(),
            warnings: vec!["schema_retry".to_owned()],
        },
    };

    let encoded =
        encode_gateway_message_for_version(&message, RUNTIME_PROFILE_PROTOCOL_VERSION).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "protocol_version": 3,
            "type": "text_processed",
            "request_id": "p-1",
            "processed": {
                "output": {"decisions": []},
                "warnings": ["schema_retry"],
            },
        })
    );

    for version in [
        LEGACY_CLIENT_PROTOCOL_VERSION,
        CONTEXT_SEED_PROTOCOL_VERSION,
    ] {
        assert!(matches!(
            encode_gateway_message_for_version(&message, version),
            Err(ClientWireError::UnsupportedProtocolVersion(_))
        ));
    }
}

#[test]
fn processed_text_fails_closed_on_values_the_runtime_cannot_produce() {
    for processed in [
        // Output that is not JSON at all: the runtime only ever states validated JSON.
        ClientProcessedText {
            output: "not json".to_owned(),
            warnings: Vec::new(),
        },
        // A warning outside the closed list, and a repeated one.
        ClientProcessedText {
            output: "{}".to_owned(),
            warnings: vec!["the model was unhappy".to_owned()],
        },
        ClientProcessedText {
            output: "{}".to_owned(),
            warnings: vec!["schema_retry".to_owned(), "schema_retry".to_owned()],
        },
        // Output past the size a client frame can carry.
        ClientProcessedText {
            output: format!("\"{}\"", "x".repeat(MAX_CLIENT_PROCESSED_OUTPUT_BYTES)),
            warnings: Vec::new(),
        },
    ] {
        assert!(matches!(
            encode_gateway_message_for_version(
                &GatewayMessage::TextProcessed {
                    request_id: "p-1".to_owned(),
                    processed,
                },
                RUNTIME_PROFILE_PROTOCOL_VERSION,
            ),
            Err(ClientWireError::InvalidProcessedText)
        ));
    }
}
