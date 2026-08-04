use conversation_protocol::{
    decode_client_command, encode_gateway_message, ClientCommand, ClientRuntimeEvent,
    ContextSource, ConversationMode, ConversationSignal, FollowUpPolicy, GatewayMessage, MemoryId,
    MemoryKind, MemoryRetrievalReason, MemoryRetrievalTrace, MemoryTraceExclusions,
    MemoryTraceItem, PersonaLevel, QualityDecision, ResponseControls, RetrievalTraceId,
    RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStage, SilencePolicy, SpeechPace, TurnId,
    UnixTimestampMillis, CLIENT_PROTOCOL_VERSION, MAX_CLIENT_FRAME_BYTES,
};

fn gateway_value(message: &GatewayMessage) -> serde_json::Value {
    serde_json::from_slice(&encode_gateway_message(message).unwrap()).unwrap()
}

#[test]
fn identifiers_round_trip_as_decimal_strings() {
    let command = decode_client_command(
        br#"{"protocol_version":1,"type":"start_turn","request_id":"req-1","turn_id":"18446744073709551615","transcript":"hello"}"#,
    )
    .unwrap();

    assert!(
        matches!(command, ClientCommand::StartTurn { turn_id, .. } if turn_id.get() == u64::MAX)
    );
}

#[test]
fn unknown_fields_and_versions_are_rejected() {
    assert!(decode_client_command(
        br#"{"protocol_version":2,"type":"status","request_id":"req-1"}"#
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
        br#"{"protocol_version":1,"type":"start_turn","request_id":"req-1","turn_id":"1","transcript":""}"#
    )
    .is_err());
    assert!(decode_client_command(
        format!(
            r#"{{"protocol_version":1,"type":"start_turn","request_id":"req-1","turn_id":"1","transcript":"{}"}}"#,
            "x".repeat(16 * 1024 + 1)
        )
        .as_bytes()
    )
    .is_err());
}

#[test]
fn fixture_commands_and_events_parse_as_version_one_contracts() {
    let commands = include_str!("../../../tests/fixtures/client-wire-v1/commands.jsonl");
    let events = include_str!("../../../tests/fixtures/client-wire-v1/events.jsonl");

    for (line_number, line) in commands.lines().enumerate() {
        assert!(
            decode_client_command(line.as_bytes()).is_ok(),
            "command fixture line {} must decode",
            line_number + 1
        );
    }

    for (line_number, line) in events.lines().enumerate() {
        let event: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("event fixture line {}: {error}", line_number + 1));
        assert_eq!(event["protocol_version"], CLIENT_PROTOCOL_VERSION);
        assert!(event["type"].is_string());
    }
}

#[test]
fn invalid_fixture_commands_are_rejected() {
    for (line_number, line) in include_str!("../../../tests/fixtures/client-wire-v1/invalid.jsonl")
        .lines()
        .enumerate()
    {
        assert!(
            decode_client_command(line.as_bytes()).is_err(),
            "invalid fixture line {} must be rejected",
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

    let error = RuntimeError::new(
        RuntimeErrorKind::Adapter,
        RuntimeStage::LanguageModel,
        "x".repeat(MAX_CLIENT_FRAME_BYTES),
    );
    let oversized = GatewayMessage::RuntimeEvent {
        event: ClientRuntimeEvent::try_from(RuntimeEvent::TurnFailed {
            turn_id: TurnId::new(1),
            error,
        })
        .unwrap(),
    };
    assert!(encode_gateway_message(&oversized).is_err());
}
