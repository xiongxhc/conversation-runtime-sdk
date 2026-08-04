use conversation_protocol::{
    decode_client_command, encode_gateway_message, ClientCommand, ClientMemoryTrace,
    ClientQualityDecision, ClientResponseControls, ClientRuntimeError, ClientRuntimeEvent,
    ClientWireError, ContextSource, ConversationMode, ConversationSignal, FollowUpPolicy,
    GatewayMessage, MemoryId, MemoryKind, MemoryRetrievalReason, MemoryRetrievalTrace,
    MemoryTraceExclusions, MemoryTraceItem, PersonaLevel, QualityDecision, ResponseControls,
    RetrievalTraceId, RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStage, RuntimeStatus,
    SilencePolicy, SpeechPace, TurnId, UnixTimestampMillis, CLIENT_PROTOCOL_VERSION,
    MAX_CLIENT_FRAME_BYTES,
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
        kind: "invalid_state".to_owned(),
        stage: "runtime".to_owned(),
        message: "an active turn already exists".to_owned(),
    }
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
        parse_event_fixture(line)
            .unwrap_or_else(|error| panic!("event fixture line {}: {error}", line_number + 1));
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

#[test]
fn outgoing_response_messages_reject_invalid_request_ids() {
    for request_id in [String::new(), "r".repeat(65)] {
        for message in [
            GatewayMessage::CommandAccepted {
                request_id: request_id.clone(),
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
            | Self::RuntimeEvent {
                protocol_version, ..
            }
            | Self::Fatal {
                protocol_version, ..
            } => *protocol_version,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FixtureRuntimeError {
    kind: String,
    stage: String,
    message: String,
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
