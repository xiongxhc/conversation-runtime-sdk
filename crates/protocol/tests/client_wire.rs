use conversation_protocol::{
    decode_client_command, encode_gateway_message, memory_preview, ClientCommand,
    ClientMemoryApproval, ClientMemoryCursor, ClientMemoryInspection, ClientMemoryProvenance,
    ClientMemoryRecord, ClientMemoryRetention, ClientMemorySummary, ClientMemoryTrace,
    ClientQualityDecision, ClientResponseControls, ClientRuntimeError, ClientRuntimeEvent,
    ClientWireError, ContextSource, ConversationMode, ConversationSignal, FollowUpPolicy,
    GatewayMessage, MemoryApproval, MemoryConfidence, MemoryDraft, MemoryId, MemoryInspection,
    MemoryKind, MemoryProvenance, MemoryProvenanceKind, MemoryRecord, MemoryRetention,
    MemoryRetrievalReason, MemoryRetrievalTrace, MemoryTraceExclusions, MemoryTraceItem,
    PersonaLevel, QualityDecision, ResponseControls, RetrievalTraceId, RuntimeError,
    RuntimeErrorKind, RuntimeEvent, RuntimeStage, RuntimeStatus, SilencePolicy, SpeechPace, TurnId,
    UnixTimestampMillis, CLIENT_PROTOCOL_VERSION, MAX_CLIENT_FRAME_BYTES, MAX_MEMORY_PREVIEW_BYTES,
};
use serde::{Deserialize, Deserializer};

fn gateway_value(message: &GatewayMessage) -> serde_json::Value {
    serde_json::from_slice(&encode_gateway_message(message).unwrap()).unwrap()
}

fn status() -> RuntimeStatus {
    RuntimeStatus {
        transport: "stdio".to_owned(),
        privacy_mode: "local_only".to_owned(),
        language_location: "local".to_owned(),
        model_id: "local-model".to_owned(),
        memory_enabled: false,
        memory_location: None,
        telemetry_enabled: false,
        capabilities: vec!["text".to_owned()],
    }
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
fn start_turn_commands_do_not_accept_client_selected_identifiers() {
    let command = decode_client_command(
        br#"{"protocol_version":2,"type":"start_turn","request_id":"req-1","transcript":"hello"}"#,
    )
    .unwrap();

    assert!(matches!(command, ClientCommand::StartTurn { .. }));
    assert!(decode_client_command(
        br#"{"protocol_version":2,"type":"start_turn","request_id":"req-1","turn_id":"1","transcript":"hello"}"#,
    )
    .is_err());
}

#[test]
fn accepted_start_turns_carry_the_gateway_allocated_identifier() {
    let value = gateway_value(&GatewayMessage::CommandAccepted {
        request_id: "req-1".to_owned(),
        turn_id: Some(TurnId::new(9)),
    });

    assert_eq!(value["type"], "command_accepted");
    assert_eq!(value["request_id"], "req-1");
    assert_eq!(value["turn_id"], "9");
}

#[test]
fn version_two_memory_commands_decode_and_version_one_commands_are_rejected() {
    let list = decode_client_command(
        br#"{"protocol_version":2,"type":"memory_list","request_id":"req-1","cursor":null}"#,
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
        br#"{"protocol_version":2,"type":"memory_inspect","request_id":"req-2","memory_id":"7"}"#,
    )
    .unwrap();
    assert!(matches!(
        inspect,
        ClientCommand::MemoryInspect { memory_id, .. } if memory_id.get() == 7
    ));

    for line in include_str!("../../../tests/fixtures/client-wire-v1/commands.jsonl").lines() {
        assert!(decode_client_command(line.as_bytes()).is_err());
    }
}

#[test]
fn unknown_fields_and_versions_are_rejected() {
    assert!(decode_client_command(
        br#"{"protocol_version":1,"type":"status","request_id":"req-1"}"#
    )
    .is_err());
    assert!(decode_client_command(
        br#"{"protocol_version":2,"type":"status","request_id":"req-1","extra":true}"#
    )
    .is_err());
}

#[test]
fn identifiers_must_be_canonical_non_zero_decimal_strings() {
    for identifier in ["0", "01", "+1", " 1", "1 ", "-1"] {
        let payload = format!(
            r#"{{"protocol_version":2,"type":"interrupt_turn","request_id":"req-1","turn_id":"{identifier}"}}"#
        );
        assert!(
            decode_client_command(payload.as_bytes()).is_err(),
            "{identifier}"
        );
    }

    assert!(decode_client_command(
        br#"{"protocol_version":2,"type":"interrupt_turn","request_id":"req-1","turn_id":1}"#
    )
    .is_err());
}

#[test]
fn commands_reject_invalid_request_ids_and_transcripts() {
    assert!(
        decode_client_command(br#"{"protocol_version":2,"type":"status","request_id":""}"#)
            .is_err()
    );
    assert!(decode_client_command(
        format!(
            r#"{{"protocol_version":2,"type":"status","request_id":"{}"}}"#,
            "r".repeat(65)
        )
        .as_bytes()
    )
    .is_err());
    assert!(decode_client_command(
        br#"{"protocol_version":2,"type":"start_turn","request_id":"req-1","transcript":""}"#
    )
    .is_err());
    assert!(decode_client_command(
        format!(
            r#"{{"protocol_version":2,"type":"start_turn","request_id":"req-1","transcript":"{}"}}"#,
            "x".repeat(16 * 1024 + 1)
        )
        .as_bytes()
    )
    .is_err());
}

#[test]
fn version_one_command_fixtures_are_rejected() {
    let commands = include_str!("../../../tests/fixtures/client-wire-v1/commands.jsonl");

    for (line_number, line) in commands.lines().enumerate() {
        assert!(
            decode_client_command(line.as_bytes()).is_err(),
            "v1 command fixture line {} must be rejected",
            line_number + 1
        );
    }
}

#[test]
fn event_fixtures_reject_numeric_or_malformed_nested_identifiers() {
    for payload in [
        r#"{"protocol_version":2,"type":"runtime_event","event":{"type":"turn_started","turn_id":1}}"#,
        r#"{"protocol_version":2,"type":"runtime_event","event":{"type":"quality_resolved","decision":{"turn_id":"0","mode":"direct_answer","controls":{"maximum_spoken_seconds":20,"directness":80,"pace":"natural","follow_up_policy":"contextual","silence_policy":"allow_without_filler"},"signals":[],"history_message_count":0,"context_sources":["saved_persona","current_turn"]}}}"#,
        r#"{"protocol_version":2,"type":"runtime_event","event":{"type":"memory_retrieved","trace":{"trace_id":"01","turn_id":"1","selected_items":1,"used_bytes":12}}}"#,
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
fn version_two_fixtures_parse_and_invalid_cases_are_rejected() {
    for (line_number, line) in include_str!("../../../tests/fixtures/client-wire-v2/commands.jsonl")
        .lines()
        .enumerate()
    {
        decode_client_command(line.as_bytes())
            .unwrap_or_else(|error| panic!("command fixture line {}: {error}", line_number + 1));
    }

    for (line_number, line) in include_str!("../../../tests/fixtures/client-wire-v2/events.jsonl")
        .lines()
        .enumerate()
    {
        parse_event_fixture(line)
            .unwrap_or_else(|error| panic!("event fixture line {}: {error}", line_number + 1));
    }

    for (line_number, line) in include_str!("../../../tests/fixtures/client-wire-v2/invalid.jsonl")
        .lines()
        .enumerate()
    {
        assert!(
            decode_client_command(line.as_bytes()).is_err() && parse_event_fixture(line).is_err(),
            "invalid fixture line {} must be rejected by every v2 envelope",
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

#[test]
fn outgoing_response_messages_reject_invalid_request_ids() {
    for request_id in [String::new(), "r".repeat(65)] {
        for message in [
            GatewayMessage::CommandAccepted {
                request_id: request_id.clone(),
                turn_id: None,
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
    let message: FixtureGatewayMessage =
        serde_json::from_str(payload).map_err(|error| error.to_string())?;
    if message.protocol_version() != CLIENT_PROTOCOL_VERSION {
        return Err("unsupported protocol version".to_owned());
    }
    message.validate()?;
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
    Fatal {
        protocol_version: u64,
        error: FixtureRuntimeError,
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
            | Self::Fatal {
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
}

impl FixtureRuntimeStatus {
    fn validate(&self) -> Result<(), String> {
        let disabled =
            !self.memory_enabled && self.memory_location.is_none() && self.capabilities == ["text"];
        let inspectable_local = self.memory_enabled
            && self.memory_location.as_deref() == Some("local")
            && self.capabilities == ["text", "memory_inspection"];

        if disabled || inspectable_local {
            Ok(())
        } else {
            Err("runtime memory status is incoherent".to_owned())
        }
    }
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
        turn_id: FixtureIdentifier,
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

struct FixtureIdentifier;

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
        if value.is_empty()
            || value == "0"
            || value.starts_with('0')
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || value.parse::<u64>().is_err()
        {
            return Err(serde::de::Error::custom("invalid decimal identifier"));
        }
        Ok(Self)
    }
}
