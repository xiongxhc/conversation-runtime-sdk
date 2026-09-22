use conversation_protocol::{decode_client_command_with_version, ClientCommand};
use std::time::{SystemTime, UNIX_EPOCH};

// Removing the v4 timed command would prevent the real start transaction from
// negotiating a capture format and binding the independent bridge identity.
#[test]
fn timed_start_reaches_v4_admission_but_never_legacy_admission() {
    let wire = br#"{"protocol_version":4,"type":"start_timed_voice_session","request_id":"timed-start","bridge_session_id":"bridge-independent","profile":"offline","response_mode":"external_gate"}"#;
    let (version, _) = decode_client_command_with_version(wire)
        .expect("timed start must reach the configured gateway admission checks");
    assert_eq!(version, 4);
    for legacy in [1, 2, 3] {
        let old = String::from_utf8(wire.to_vec()).unwrap().replacen(
            "\"protocol_version\":4",
            &format!("\"protocol_version\":{legacy}"),
            1,
        );
        assert!(decode_client_command_with_version(old.as_bytes()).is_err());
    }
}

// Removing this discriminator would silently force existing media2 callers to
// understand the recorder-finish records. The new path is explicit and v4-only.
#[test]
fn drainable_timed_start_is_an_additive_v4_command() {
    let wire = br#"{"protocol_version":4,"type":"start_drainable_timed_voice_session","request_id":"drainable-start","bridge_session_id":"bridge-independent","profile":"offline","response_mode":"external_gate"}"#;
    let (version, command) = decode_client_command_with_version(wire)
        .expect("explicit drainable start must reach gateway admission");
    assert_eq!(version, 4);
    assert!(matches!(
        command,
        ClientCommand::StartDrainableTimedVoiceSession {
            request_id,
            bridge_session_id,
            ..
        } if request_id == "drainable-start" && bridge_session_id == "bridge-independent"
    ));

    for legacy in [1, 2, 3] {
        let old = String::from_utf8(wire.to_vec()).unwrap().replacen(
            "\"protocol_version\":4",
            &format!("\"protocol_version\":{legacy}"),
            1,
        );
        assert!(decode_client_command_with_version(old.as_bytes()).is_err());
    }

    let existing = br#"{"protocol_version":4,"type":"start_timed_voice_session","request_id":"timed-start","bridge_session_id":"bridge-independent","profile":"offline","response_mode":"external_gate"}"#;
    assert!(matches!(
        decode_client_command_with_version(existing).unwrap().1,
        ClientCommand::StartTimedVoiceSession { .. }
    ));
}

// Accidentally upgrading the existing response or accepting an untyped version would
// break media2 clients. Each explicit start path must retain its exact lease version.
#[test]
fn timed_start_responses_preserve_exact_media_versions() {
    use conversation_protocol::{
        encode_gateway_message_for_version, ClientMediaLeaseDescriptor, GatewayMessage, SessionId,
    };

    let expires_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 60_000;
    let base = || {
        ClientMediaLeaseDescriptor::new(
            1,
            SessionId::new(41),
            "tcp://127.0.0.1:9001".into(),
            "a".repeat(64),
            expires_at_ms,
            10_000,
        )
        .unwrap()
    };
    let old = base().into_timed("pcm_s16le/24000/1/20ms".into()).unwrap();
    let drainable = base()
        .into_drainable_timed("pcm_s16le/24000/1/20ms".into())
        .unwrap();

    for (lease, expected) in [(old, 2_u64), (drainable, 3_u64)] {
        let encoded = encode_gateway_message_for_version(
            &GatewayMessage::TimedVoiceSessionStarted {
                request_id: "timed-start".into(),
                media_lease: lease,
            },
            4,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(value["media_lease"]["media_protocol_version"], expected);
    }
}

// Treating alignment as reconciliation would require a provider and prevent
// authenticated timing recovery on a new, otherwise idle gateway connection.
#[test]
fn alignment_read_and_original_request_controls_reach_v4_admission() {
    let read = br#"{"protocol_version":4,"type":"read_recording_alignment","request_id":"alignment","capability":{"source_session_id":"41","recording_handle":"0123456789abcdef0123456789abcdef","sha256":"1111111111111111111111111111111111111111111111111111111111111111","key_id":"0123456789abcdef","authorization":"2222222222222222222222222222222222222222222222222222222222222222"}}"#;
    for wire in [read.as_slice(), br#"{"protocol_version":4,"type":"recording_alignment_credit","request_id":"alignment","consumed_page_index":"0"}"#, br#"{"protocol_version":4,"type":"cancel_recording_alignment","request_id":"alignment"}"#] {
        assert!(decode_client_command_with_version(wire).is_ok(), "alignment/control command is unavailable");
    }
}

#[test]
fn canonical_alignment_outputs_match_sdk_fixture_and_keep_legacy_bytes_opt_in() {
    use conversation_protocol::*;
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/timed-capture-v1/output.json"
    ))
    .unwrap();
    let source = ClientRecordingIdentity {
        source_session_id: "41".into(),
        recording_handle: "02".repeat(16),
        sha256: "11".repeat(32),
    };
    let epoch = ClientCaptureEpoch {
        bridge_session_id: "bridge-independent".into(),
        clock_domain_id: "source-process".into(),
        capture_epoch: "1".into(),
    };
    let frame = ClientAlignmentEntry::Frame {
        media_start_sample: "0".into(),
        media_end_sample: "2".into(),
        sample_rate_hz: 24_000,
        gap_offset_ms: "0".into(),
        epoch: Some(epoch.clone()),
        provenance: Some(Box::new(ClientFrameProvenance {
            capture_epoch: "1".into(),
            sequence: "7".into(),
            output_start_sample: "0".into(),
            sample_count: 2,
            source_start_numerator: "0".into(),
            source_end_numerator: "96000".into(),
            source_denominator: 24_000,
            source_rate_hz: 48_000,
            quality: "nominal_frame_endpoint_interpolation".into(),
            span: Some(ClientNominalCaptureSpan {
                host_start_ns: "1000000000".into(),
                host_end_ns: "1000083334".into(),
                rounding_envelope_ns: "4".into(),
                capture_uncertainty_ns: None,
            }),
        })),
        capture_status: "available".into(),
        reasons: vec![],
        wall: Some(Box::new(ClientWallEstimate {
            quality: "local_wall_estimate",
            policy_id: "local-wall-estimate/v1",
            local_start_ns: "2100000000".into(),
            local_end_ns: "2100083334".into(),
            wall_epoch: "1".into(),
            wall_run: "1".into(),
            maximum_bracket_ns: "0".into(),
            maximum_resolution_ns: "1".into(),
            model_allowance_ns: "10000000".into(),
            interpolation_rounding_ns: "2".into(),
            capture_rounding_ns: "4".into(),
            capture_uncertainty_ns: None,
            external_utc_accuracy_ns: None,
            overall_physical_accuracy_ns: None,
        })),
    };
    let page = ClientRecordingAlignmentPage {
        source: source.clone(),
        page_index: "0".into(),
        policy: ClientAlignmentPolicy::default(),
        entries: vec![ClientAlignmentEntry::Epoch { epoch }, frame.clone()],
    };
    let complete = ClientRecordingAlignmentComplete {
        source,
        page_count: "1".into(),
        coverage_status: "available".into(),
        warnings: vec!["local_wall_timestamp_estimate".into()],
        traversal_succeeded: true,
    };
    for (key, message) in [
        (
            "page",
            GatewayMessage::RecordingAlignmentPage {
                request_id: "alignment".into(),
                page: page.clone(),
            },
        ),
        (
            "complete",
            GatewayMessage::RecordingAlignmentComplete {
                request_id: "alignment".into(),
                completion: complete,
            },
        ),
    ] {
        let encoded = encode_gateway_message_for_version(&message, 4).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&encoded).unwrap(),
            fixture[key]
        );
        for legacy in [1, 2, 3] {
            assert!(encode_gateway_message_for_version(&message, legacy).is_err());
        }
    }
    let mut too_many = page.clone();
    let mut impossible_wall = frame.clone();
    if let ClientAlignmentEntry::Frame {
        wall: Some(wall), ..
    } = &mut impossible_wall
    {
        wall.maximum_bracket_ns = "5000001".into();
    }
    assert!(
        impossible_wall.validate().is_err(),
        "output claimed admitted wall mapping beyond the clock-bracket policy"
    );
    too_many.entries = vec![frame.clone(); 65];
    assert!(too_many.validate().is_err());
    let mut envelope_edge = None;
    'find: for width in 1..=64 {
        let mut entry = frame.clone();
        if let ClientAlignmentEntry::Frame {
            epoch: Some(epoch), ..
        } = &mut entry
        {
            epoch.bridge_session_id = "x".repeat(width);
            epoch.clock_domain_id = "y".repeat(width);
        }
        for count in 1..=64 {
            let mut candidate = page.clone();
            candidate.entries = vec![entry.clone(); count];
            let bytes = serde_json::to_vec(&candidate).unwrap().len();
            if (32 * 1024 - 256..=32 * 1024).contains(&bytes) {
                envelope_edge = Some(candidate);
                break 'find;
            }
        }
    }
    let bounded_body = envelope_edge.expect("fixture reaches the envelope edge");
    bounded_body.validate().unwrap();
    let envelope = GatewayMessage::RecordingAlignmentPage {
        request_id: "\0".repeat(64),
        page: bounded_body,
    };
    assert!(
        encode_gateway_message_for_version(&envelope, 4).is_err(),
        "JSON-escaped correlation bytes exceeded the full page envelope bound"
    );
    let mut too_large = page;
    too_large.entries = vec![frame; 64];
    assert!(serde_json::to_vec(&too_large).unwrap().len() > 32 * 1024);
    assert!(too_large.validate().is_err());
}
