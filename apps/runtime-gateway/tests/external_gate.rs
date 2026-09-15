//! Protocol-level regression coverage for the external response gate. The runtime's ten-segment
//! no-provider-work invariant is exercised in `conversation-runtime` where the voice loop owns
//! the pending registry; this gateway test fixes the public command/version boundary.

use conversation_protocol::{
    decode_client_command, encode_gateway_message_for_version, ClientCommand,
    ClientVoiceSessionEvent, GatewayMessage, SessionId, TurnId, VoiceResponseMode,
};

#[test]
fn external_gate_commands_are_v3_only_and_preserve_session_segment_identity() {
    let start = decode_client_command(
        br#"{"protocol_version":3,"type":"start_voice_session","request_id":"start","profile":"offline","response_mode":"external_gate"}"#,
    )
    .unwrap();
    assert!(matches!(
        start,
        ClientCommand::StartVoiceSession {
            response_mode: Some(VoiceResponseMode::ExternalGate),
            ..
        }
    ));

    let authorization = decode_client_command(
        br#"{"protocol_version":3,"type":"authorize_voice_response","request_id":"allow","session_id":"7","segment_id":"9","context_exchanges":[{"user":"earlier","assistant":"context"}]}"#,
    )
    .unwrap();
    assert!(matches!(
        authorization,
        ClientCommand::AuthorizeVoiceResponse { session_id, segment_id: 9, .. }
            if session_id.get() == 7
    ));

    assert!(decode_client_command(
        br#"{"protocol_version":1,"type":"start_voice_session","request_id":"legacy","response_mode":"external_gate"}"#,
    )
    .is_err());
}

#[test]
fn legacy_transcript_final_payload_stays_exactly_version_one_compatible() {
    let encoded = encode_gateway_message_for_version(
        &GatewayMessage::VoiceEvent {
            event: ClientVoiceSessionEvent::VoiceTranscriptFinal {
                session_id: SessionId::new(7),
                segment_id: 9,
                turn_id: Some(TurnId::new(11)),
                text: "agenda item".to_owned(),
                capture_evidence: None,
            },
        },
        1,
    )
    .unwrap();

    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&encoded).unwrap(),
        serde_json::json!({
            "protocol_version": 1,
            "type": "voice_event",
            "event": {
                "type": "voice_transcript_final",
                "session_id": "7",
                "turn_id": "11",
                "text": "agenda item"
            }
        })
    );
}
