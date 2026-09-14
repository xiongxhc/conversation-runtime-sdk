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
