use conversation_protocol::decode_client_command_with_version;
use conversation_protocol::{ClientCommand, FinalizedRecordingCapability, SessionId};

#[test]
fn capability_and_enclosing_command_debug_and_decoder_errors_are_redacted() {
    let capability = FinalizedRecordingCapability {
        source_session_id: SessionId::new(41),
        recording_handle: "a".repeat(32),
        sha256: "b".repeat(64),
        key_id: "c".repeat(16),
        authorization: "d".repeat(64),
    };
    let command = ClientCommand::ReconcileFinalizedRecording {
        request_id: "recover".into(),
        capability: capability.clone(),
    };
    assert!(!format!("{command:?}").contains(&capability.authorization));
    assert!(!format!("{capability:?}").contains(&capability.recording_handle));
    for malformed in [
        serde_json::json!({"source_session_id":"SECRET-MARKER"}),
        serde_json::json!({"SECRET-MARKER":"private"}),
        serde_json::json!({"source_session_id":"41","recording_handle":"a".repeat(32),"sha256":"b".repeat(64),"key_id":"c".repeat(16),"authorization":"SECRET-MARKER"}),
    ] {
        let wire = serde_json::json!({"protocol_version":4,"type":"reconcile_finalized_recording","request_id":"recover","capability":malformed});
        for payload in [wire.to_string(), format!("{wire} trailing-invalid-json")] {
            let error = decode_client_command_with_version(payload.as_bytes()).unwrap_err();
            assert!(!format!("{error:?} {error}").contains("SECRET-MARKER"));
        }
    }
}

// Rejecting v4 here makes restart recovery unavailable before any authorization can run.
#[test]
fn finalized_recovery_has_a_v4_only_exact_command_shape() {
    let wire = br#"{"protocol_version":4,"type":"reconcile_finalized_recording","request_id":"recover-1","capability":{"source_session_id":"41","recording_handle":"0123456789abcdef0123456789abcdef","sha256":"1111111111111111111111111111111111111111111111111111111111111111","key_id":"0123456789abcdef","authorization":"2222222222222222222222222222222222222222222222222222222222222222"}}"#;
    let (version, _) = decode_client_command_with_version(wire)
        .expect("the explicit v4 recovery command must reach capability authorization");
    assert_eq!(version, 4);
    for legacy in [1, 2, 3] {
        let legacy = String::from_utf8(wire.to_vec()).unwrap().replacen(
            "\"protocol_version\":4",
            &format!("\"protocol_version\":{legacy}"),
            1,
        );
        assert!(decode_client_command_with_version(legacy.as_bytes()).is_err());
    }
}

#[test]
fn closed_recording_adoption_is_a_v4_only_command_naming_one_exact_recording() {
    let handle = "0123456789abcdef0123456789abcdef";
    let wire = |version: u64, session: &str, handle: &str, extra: &str| {
        format!(
            r#"{{"protocol_version":{version},"type":"adopt_closed_recording","request_id":"adopt-1","source_session_id":"{session}","recording_handle":"{handle}"{extra}}}"#
        )
    };
    let (version, command) =
        decode_client_command_with_version(wire(4, "41", handle, "").as_bytes()).unwrap();
    assert_eq!(version, 4);
    assert_eq!(
        command,
        ClientCommand::AdoptClosedRecording {
            request_id: "adopt-1".into(),
            source_session_id: SessionId::new(41),
            recording_handle: handle.into(),
        }
    );
    for refused in [
        wire(3, "41", handle, ""),
        wire(2, "41", handle, ""),
        wire(4, "0", handle, ""),
        wire(4, "041", handle, ""),
        wire(4, "41", "0123456789ABCDEF0123456789abcdef", ""),
        wire(4, "41", "0123456789abcdef", ""),
        wire(4, "41", "../../0123456789abcdef0123456789", ""),
        wire(4, "41", handle, r#","sha256":"00""#),
    ] {
        assert!(
            decode_client_command_with_version(refused.as_bytes()).is_err(),
            "{refused}"
        );
    }
}

#[test]
fn adoption_refusals_have_their_own_codes_and_the_capability_needs_version_four() {
    use conversation_protocol::{
        encode_gateway_message_for_version, ClientComponentDescriptor, ClientRuntimeError,
        GatewayMessage, RuntimeStatus,
    };
    for code in [
        "recording_not_found",
        "recording_unreadable",
        "recording_not_closed",
        "recording_incomplete",
        "recording_identity_mismatch",
        "recording_in_use",
    ] {
        let rejected = GatewayMessage::CommandRejected {
            request_id: "adopt-1".into(),
            error: ClientRuntimeError {
                code: code.into(),
                kind: "invalid_state".into(),
                stage: "runtime".into(),
                message: "refused".into(),
            },
        };
        assert!(
            encode_gateway_message_for_version(&rejected, 4).is_ok(),
            "{code}"
        );
    }
    let status = RuntimeStatus {
        profile: "offline".to_owned(),
        transport: "stdio".to_owned(),
        privacy_mode: "local_only".to_owned(),
        language_location: "local".to_owned(),
        model_id: "local-model".to_owned(),
        memory_enabled: false,
        memory_location: None,
        telemetry_enabled: false,
        capabilities: vec!["text".to_owned(), "closed_recording_adoption".to_owned()],
        components: vec![ClientComponentDescriptor {
            kind: "language_model".to_owned(),
            execution_location: "local".to_owned(),
            provider_label: "Local language".to_owned(),
        }],
        last_context_seed_operation_id: None,
    };
    let ready = GatewayMessage::ReadyV4 {
        status: status.clone(),
        current_session_id: SessionId::new(7),
    };
    assert!(encode_gateway_message_for_version(&ready, 4).is_ok());
    let legacy = GatewayMessage::Ready { status };
    assert!(encode_gateway_message_for_version(&legacy, 3).is_err());
}
