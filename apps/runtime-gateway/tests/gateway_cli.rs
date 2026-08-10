mod support;

use conversation_protocol::{
    MemoryConfidence, MemoryDraft, MemoryKind, MemoryPatch, MemoryProvenance, MemoryProvenanceKind,
    MemoryRetention, UnixTimestampMillis, MAX_CLIENT_FRAME_BYTES,
};
use tokio::io::AsyncWriteExt;

use support::{
    assert_accepted, assert_local_status, assert_rejected, history_count, is_accepted, joined_text,
    start_turn, terminal_count, terminal_type, FakeOllamaServer, GatewayProcess, WireMessage,
};
#[cfg(unix)]
use support::{assert_voice_status, is_voice_terminal};

#[tokio::test]
async fn persistent_session_reports_local_status_and_preserves_completed_history() {
    let server = FakeOllamaServer::completing(2).await;
    let mut gateway = GatewayProcess::start(server.endpoint()).await;

    let ready = gateway.read_message().await;
    assert_eq!(ready.message_type(), "ready");
    assert_local_status(&ready, false);

    gateway
        .write_message(r#"{"protocol_version":1,"type":"status","request_id":"status-1"}"#)
        .await;
    assert_accepted(&gateway.read_message().await, "status-1");
    let status = gateway.read_message().await;
    assert_eq!(status.message_type(), "status");
    assert_eq!(status.request_id(), Some("status-1"));
    assert_local_status(&status, false);

    gateway
        .write_message(&start_turn("start-1", "fixture-first-transcript"))
        .await;
    let start_accepted = gateway.read_message().await;
    assert_accepted(&start_accepted, "start-1");
    assert!(start_accepted.raw.contains(r#""turn_id":"1""#));
    let first = gateway.read_turn("1").await;
    assert_eq!(first.first().unwrap().event_type(), Some("turn_started"));
    assert_eq!(first.first().unwrap().request_id(), Some("start-1"));
    assert_eq!(terminal_count(&first), 1);
    assert_eq!(terminal_type(&first), "turn_completed");
    assert_eq!(joined_text(&first), "fixture-answer");
    assert_eq!(history_count(&first), 0);

    gateway
        .write_message(&start_turn("start-2", "fixture-second-transcript"))
        .await;
    assert_accepted(&gateway.read_message().await, "start-2");
    let second = gateway.read_turn("2").await;
    assert_eq!(second.first().unwrap().event_type(), Some("turn_started"));
    assert_eq!(second.first().unwrap().request_id(), Some("start-2"));
    assert_eq!(terminal_count(&second), 1);
    assert_eq!(terminal_type(&second), "turn_completed");
    assert_eq!(joined_text(&second), "fixture-answer");
    assert_eq!(history_count(&second), 2);
    server.wait_for_requests(2).await;

    let exit = gateway.close().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&[
        "fixture-first-transcript",
        "fixture-second-transcript",
        "fixture-answer",
    ]);
}

#[tokio::test]
async fn status_reports_exact_model_and_enabled_local_memory() {
    let server = FakeOllamaServer::completing(0).await;
    let mut gateway = GatewayProcess::start_with_memory(server.endpoint()).await;

    let ready = gateway.read_message().await;
    assert_eq!(ready.message_type(), "ready");
    assert_local_status(&ready, true);

    gateway
        .write_message(r#"{"protocol_version":1,"type":"status","request_id":"status-memory"}"#)
        .await;
    assert_accepted(&gateway.read_message().await, "status-memory");
    assert_local_status(&gateway.read_message().await, true);

    let exit = gateway.close().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&["status-memory"]);
}

#[cfg(unix)]
#[tokio::test]
async fn configured_voice_session_spawns_the_sidecar_and_reports_its_failure() {
    // The fixture sidecar executable only marks that it was spawned; it never speaks the
    // sidecar handshake protocol. That proves `start_voice_session` now does real work
    // (spawns the configured sidecar) instead of the earlier universal rejection, while
    // staying within this task's mocked-adapter scope: a real handshake against a fake
    // sidecar binary is Task 7's compiled-gateway integration surface.
    let server = FakeOllamaServer::completing(0).await;
    let mut gateway = GatewayProcess::start_with_voice(server.endpoint()).await;

    let ready = gateway.read_message().await;
    assert_eq!(ready.message_type(), "ready");
    assert_voice_status(&ready);
    assert!(!gateway.sidecar_spawned());

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"start_voice_session","request_id":"voice-start"}"#,
        )
        .await;
    assert_accepted(&gateway.read_message().await, "voice-start");

    let messages = gateway.read_until(is_voice_terminal).await;
    let terminals = messages
        .iter()
        .filter(|message| is_voice_terminal(message))
        .count();
    assert_eq!(terminals, 1);
    assert_eq!(
        messages.last().unwrap().event_type(),
        Some("voice_session_failed")
    );
    assert!(gateway.sidecar_spawned());

    let exit = gateway.close().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&["voice-start"]);
}

#[tokio::test]
async fn compiled_gateway_lists_and_inspects_memory_with_exact_correlation() {
    let server = FakeOllamaServer::completing(0).await;
    let mut gateway = GatewayProcess::start_with_memory(server.endpoint()).await;
    let ready = gateway.read_message().await;
    assert_local_status(&ready, true);
    let store = gateway.memory_store();
    let record = create_memory_with_oversized_history(&store, "compiled gateway private memory");

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"memory_list","request_id":"compiled-list","cursor":null}"#,
        )
        .await;
    assert_accepted(&gateway.read_message().await, "compiled-list");
    let list = gateway.read_message().await;
    assert_eq!(list.message_type(), "memory_list");
    assert_eq!(list.request_id(), Some("compiled-list"));
    assert!(list
        .raw
        .contains(&format!(r#""id":"{}""#, record.id().get())));
    assert!(list
        .raw
        .contains(r#""content_preview":"compiled gateway private memory""#));

    gateway
        .write_message(&format!(
            r#"{{"protocol_version":1,"type":"memory_inspect","request_id":"compiled-inspect","memory_id":"{}"}}"#,
            record.id().get()
        ))
        .await;
    assert_accepted(&gateway.read_message().await, "compiled-inspect");
    let inspection = gateway.read_message().await;
    assert_eq!(inspection.message_type(), "memory_inspection");
    assert_eq!(inspection.request_id(), Some("compiled-inspect"));
    assert!(inspection
        .raw
        .contains(r#""content":"compiled gateway private memory""#));
    assert_eq!(inspection.raw.matches(r#""source_id""#).count(), 32);
    assert!(inspection.raw.contains(r#""sources_truncated":true"#));
    assert!(inspection.raw.len() < MAX_CLIENT_FRAME_BYTES);

    gateway
        .write_message(r#"{"protocol_version":1,"type":"status","request_id":"compiled-status"}"#)
        .await;
    assert_accepted(&gateway.read_message().await, "compiled-status");
    let status = gateway.read_message().await;
    assert_eq!(status.message_type(), "status");
    assert_eq!(status.request_id(), Some("compiled-status"));

    let exit = gateway.close().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&[
        "compiled gateway private memory",
        "compiled-gateway-test",
        "compiled-list",
        "compiled-inspect",
    ]);
}

#[tokio::test]
async fn compiled_gateway_rejects_active_memory_before_interrupt_and_reaps() {
    let server = FakeOllamaServer::holding_open().await;
    let mut gateway = GatewayProcess::start_with_memory(server.endpoint()).await;
    assert_eq!(gateway.read_message().await.message_type(), "ready");

    gateway
        .write_message(&start_turn(
            "start-active-memory",
            "compiled active memory transcript",
        ))
        .await;
    assert_accepted(&gateway.read_message().await, "start-active-memory");
    gateway.read_until_text_delta("1").await;

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"memory_list","request_id":"compiled-list-active","cursor":null}"#,
        )
        .await;
    let rejection = gateway
        .read_until(|message| {
            message.message_type() == "command_rejected"
                && message.request_id() == Some("compiled-list-active")
        })
        .await;
    assert_rejected(
        rejection.last().unwrap(),
        "compiled-list-active",
        "memory_turn_active",
    );
    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"status","request_id":"compiled-status-after-active"}"#,
        )
        .await;
    assert_accepted(
        &gateway.read_message().await,
        "compiled-status-after-active",
    );
    assert_eq!(gateway.read_message().await.message_type(), "status");

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"interrupt_turn","request_id":"compiled-interrupt-after-memory","turn_id":"1"}"#,
        )
        .await;
    let before_terminal = gateway.read_until(|message| message.is_terminal()).await;
    let accepted = before_terminal
        .iter()
        .position(|message| is_accepted(message, "compiled-interrupt-after-memory"))
        .unwrap();
    let terminal = before_terminal
        .iter()
        .position(WireMessage::is_terminal)
        .unwrap();
    assert!(accepted < terminal);
    assert_eq!(terminal_type(&before_terminal), "turn_cancelled");
    server.wait_for_connection_reaped().await;

    let (exit, trailing) = gateway.close_with_messages().await;
    let all_messages = before_terminal
        .into_iter()
        .chain(trailing)
        .collect::<Vec<_>>();
    assert_eq!(terminal_count(&all_messages), 1);
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&[
        "compiled active memory transcript",
        "fixture-partial",
        "compiled-list-active",
        "compiled-interrupt-after-memory",
    ]);
}

#[tokio::test]
async fn interrupt_is_accepted_before_one_cancelled_terminal() {
    let server = FakeOllamaServer::holding_open().await;
    let mut gateway = GatewayProcess::start(server.endpoint()).await;
    assert_eq!(gateway.read_message().await.message_type(), "ready");

    gateway
        .write_message(&start_turn(
            "start-interrupt",
            "fixture-interrupt-transcript",
        ))
        .await;
    assert_accepted(&gateway.read_message().await, "start-interrupt");
    gateway.read_until_text_delta("1").await;

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"interrupt_turn","request_id":"interrupt-1","turn_id":"1"}"#,
        )
        .await;
    let before_ack = gateway
        .read_until(|message| is_accepted(message, "interrupt-1") || message.is_terminal())
        .await;
    assert!(is_accepted(before_ack.last().unwrap(), "interrupt-1"));
    let events = gateway.read_turn("1").await;
    assert_eq!(terminal_count(&events), 1);
    assert_eq!(terminal_type(&events), "turn_cancelled");
    server.wait_for_connection_reaped().await;

    let exit = gateway.close().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&[
        "fixture-interrupt-transcript",
        "fixture-partial",
        "interrupt-1",
    ]);
}

#[tokio::test]
async fn malformed_command_is_rejected_and_the_session_survives() {
    let server = FakeOllamaServer::completing(0).await;
    let mut gateway = GatewayProcess::start(server.endpoint()).await;
    assert_eq!(gateway.read_message().await.message_type(), "ready");

    gateway.write_payload(br#"{"protocol_version":1"#).await;
    let rejection = gateway.read_message().await;
    assert_eq!(rejection.message_type(), "command_rejected");
    assert_eq!(rejection.request_id(), Some("invalid-command"));

    gateway
        .write_message(
            r#"{"protocol_version":2,"type":"start_turn","request_id":"version-two-start","transcript":"old peer"}"#,
        )
        .await;
    let rejection = gateway.read_message().await;
    assert_eq!(rejection.message_type(), "command_rejected");
    assert_eq!(rejection.request_id(), Some("invalid-command"));

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"status","request_id":"status-after-rejection"}"#,
        )
        .await;
    assert_accepted(&gateway.read_message().await, "status-after-rejection");
    assert_eq!(gateway.read_message().await.message_type(), "status");

    let exit = gateway.close().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&["status-after-rejection"]);
}

#[tokio::test]
async fn oversized_and_truncated_frames_emit_one_fatal_and_exit_nonzero() {
    for input in [FatalInput::Oversized, FatalInput::Truncated] {
        let server = FakeOllamaServer::completing(0).await;
        let mut gateway = GatewayProcess::start(server.endpoint()).await;
        assert_eq!(gateway.read_message().await.message_type(), "ready");

        match input {
            FatalInput::Oversized => {
                gateway
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(&((MAX_CLIENT_FRAME_BYTES as u32) + 1).to_be_bytes())
                    .await
                    .unwrap();
            }
            FatalInput::Truncated => {
                let stdin = gateway.stdin.as_mut().unwrap();
                stdin.write_all(&10_u32.to_be_bytes()).await.unwrap();
                stdin.write_all(b"short").await.unwrap();
                stdin.flush().await.unwrap();
                gateway.stdin.take();
            }
        }

        let fatal = gateway.read_message().await;
        assert_eq!(fatal.message_type(), "fatal");
        assert!(fatal.raw.contains(r#""stage":"runtime""#));
        let exit = gateway.finish().await;
        assert!(!exit.status.success());
        exit.assert_content_free_stderr(&["gateway input framing failed"]);
    }
}

#[tokio::test]
async fn stdin_eof_cancels_and_reaps_active_generation() {
    let server = FakeOllamaServer::holding_open().await;
    let mut gateway = GatewayProcess::start(server.endpoint()).await;
    assert_eq!(gateway.read_message().await.message_type(), "ready");

    gateway
        .write_message(&start_turn("start-eof", "fixture-eof-transcript"))
        .await;
    assert_accepted(&gateway.read_message().await, "start-eof");
    gateway.read_until_text_delta("1").await;

    gateway.stdin.take();
    let exit = gateway.finish().await;
    assert!(exit.status.success());
    server.wait_for_connection_reaped().await;
    exit.assert_content_free_stderr(&["fixture-eof-transcript", "fixture-partial"]);
}

#[derive(Clone, Copy)]
enum FatalInput {
    Oversized,
    Truncated,
}

fn create_memory_with_oversized_history(
    store: &conversation_memory::SqliteMemoryStore,
    content: &str,
) -> conversation_protocol::MemoryRecord {
    use conversation_memory::MemoryStore;

    let mut record = store
        .create(
            MemoryDraft::new(
                MemoryKind::Semantic,
                content,
                MemoryProvenance::new(
                    MemoryProvenanceKind::UserProvided,
                    "compiled-gateway-test",
                    UnixTimestampMillis::new(1_000).unwrap(),
                    "local-user",
                    None,
                )
                .unwrap(),
                MemoryConfidence::new(900).unwrap(),
                UnixTimestampMillis::new(1_000).unwrap(),
                MemoryRetention::UntilDeleted,
            )
            .unwrap(),
        )
        .unwrap();
    for revision in 1..=40 {
        let changed_at = 1_000 + revision;
        let revised_content = if revision % 2 == 0 {
            content.to_owned()
        } else {
            format!("{content} revision")
        };
        record = store
            .edit(
                record.id(),
                MemoryPatch::new(
                    record.revision(),
                    Some(revised_content),
                    None,
                    None,
                    UnixTimestampMillis::new(changed_at).unwrap(),
                    MemoryProvenance::new(
                        MemoryProvenanceKind::UserEdited,
                        format!("{revision:02}-{}", "s".repeat(500)),
                        UnixTimestampMillis::new(changed_at).unwrap(),
                        "local-user",
                        None,
                    )
                    .unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
    }
    record
}
