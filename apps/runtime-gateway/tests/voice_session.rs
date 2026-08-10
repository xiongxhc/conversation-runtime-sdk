//! Proves the gateway's voice lane against the *compiled* gateway binary talking real framed
//! stdio to the *real* fake sidecar binary (`conversation-fake-voice-sidecar`) over the actual
//! macOS sidecar control/media protocol — the deterministic merge gate for the voice lane slice.
//! `tests/support` supplies the harness shared with `gateway_cli.rs`.
#![cfg(unix)]

mod support;

use support::{
    assert_accepted, assert_process_reaped, assert_rejected, assert_voice_status, is_accepted,
    is_voice_terminal, start_turn, FakeOllamaServer, FakeTtsServer, GatewayProcess, WireMessage,
};

#[tokio::test]
async fn voice_session_runs_accept_to_terminal_through_compiled_gateway() {
    let language = FakeOllamaServer::completing(1).await;
    let speech = FakeTtsServer::completing(1).await;
    let mut gateway = GatewayProcess::start_with_voice_lane(
        language.endpoint(),
        speech.endpoint(),
        "partial-final",
    )
    .await;

    let ready = gateway.read_message().await;
    assert_eq!(ready.message_type(), "ready");
    assert_voice_status(&ready);

    gateway
        .write_message(r#"{"protocol_version":1,"type":"status","request_id":"voice-status"}"#)
        .await;
    assert_accepted(&gateway.read_message().await, "voice-status");
    let status = gateway.read_message().await;
    assert_eq!(status.message_type(), "status");
    assert_eq!(status.request_id(), Some("voice-status"));
    assert_voice_status(&status);

    gateway
        .write_message(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"v-1"}"#)
        .await;
    assert_accepted(&gateway.read_message().await, "v-1");

    // The voice turn's `turn_completed` (via `StreamingTurnRuntime`'s speech-completion path)
    // is only forwarded once TTS playback has rendered, so it arrives after the playback
    // events, not before — read through it rather than stopping at the first playback event.
    let events = gateway
        .read_until(|message| message.raw.contains(r#""type":"turn_completed""#))
        .await;
    let event_types = events
        .iter()
        .filter_map(WireMessage::event_type)
        .collect::<Vec<_>>();
    assert_eq!(event_types.first(), Some(&"voice_session_started"));
    let activity_index = index_of(&event_types, "voice_activity");
    let final_index = index_of(&event_types, "voice_transcript_final");
    let turn_index = index_of(&event_types, "voice_turn_event");
    let playback_index = index_of(&event_types, "voice_playback");
    assert!(
        activity_index < final_index && final_index < turn_index && turn_index < playback_index,
        "voice events were not ordered session_started < activity < final transcript < turn events < playback: {event_types:?}"
    );
    assert!(events
        .iter()
        .any(|message| message.raw.contains(r#""text":"hello""#)));
    assert!(events
        .iter()
        .any(|message| message.raw.contains(r#""state":"accepted""#)));
    assert!(events
        .iter()
        .any(|message| message.raw.contains(r#""state":"rendered""#)));

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"stop_voice_session","request_id":"v-stop"}"#,
        )
        .await;
    // `stop_voice_session` only accepts once the pump has finished forwarding the session's
    // terminal (session.rs's `shutdown_voice_pump` doc comment), so the terminal is on the wire
    // *before* the accept here — read through to the accept rather than stopping at the
    // terminal, which `is_voice_terminal` alone would do too early.
    let after_stop = gateway
        .read_until(|message| is_accepted(message, "v-stop"))
        .await;
    let accepted_index = after_stop
        .iter()
        .position(|message| is_accepted(message, "v-stop"))
        .expect("stop_voice_session was not accepted");
    let terminal_index = after_stop
        .iter()
        .position(is_voice_terminal)
        .expect("voice session terminal was not observed");
    assert!(terminal_index < accepted_index);
    assert_eq!(
        after_stop
            .iter()
            .filter(|message| is_voice_terminal(message))
            .count(),
        1
    );
    assert_eq!(
        after_stop[terminal_index].event_type(),
        Some("voice_session_ended")
    );
    assert!(is_accepted(after_stop.last().unwrap(), "v-stop"));

    let exit = gateway.close().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&["voice-status", "v-1", "v-stop", "hello", "fixture-answer"]);
}

#[tokio::test]
async fn voice_rejections_are_request_scoped_through_compiled_gateway() {
    let language = FakeOllamaServer::completing(1).await;
    let speech = FakeTtsServer::completing(0).await;
    let mut gateway =
        GatewayProcess::start_with_voice_lane(language.endpoint(), speech.endpoint(), "quiet")
            .await;

    let ready = gateway.read_message().await;
    assert_eq!(ready.message_type(), "ready");
    assert_voice_status(&ready);

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"pause_voice_capture","request_id":"pause-no-session"}"#,
        )
        .await;
    assert_rejected(
        &gateway.read_message().await,
        "pause-no-session",
        "invalid_state",
    );

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"resume_voice_capture","request_id":"resume-no-session"}"#,
        )
        .await;
    assert_rejected(
        &gateway.read_message().await,
        "resume-no-session",
        "invalid_state",
    );

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"stop_voice_session","request_id":"stop-no-session"}"#,
        )
        .await;
    assert_rejected(
        &gateway.read_message().await,
        "stop-no-session",
        "invalid_state",
    );

    gateway
        .write_message(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"v-2a"}"#)
        .await;
    assert_accepted(&gateway.read_message().await, "v-2a");
    let started = gateway.read_message().await;
    assert_eq!(started.event_type(), Some("voice_session_started"));

    gateway
        .write_message(r#"{"protocol_version":1,"type":"start_voice_session","request_id":"v-2b"}"#)
        .await;
    assert_rejected(&gateway.read_message().await, "v-2b", "invalid_state");

    gateway
        .write_message(&start_turn(
            "start-during-voice",
            "fixture transcript during voice",
        ))
        .await;
    assert_rejected(
        &gateway.read_message().await,
        "start-during-voice",
        "invalid_state",
    );

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"stop_voice_session","request_id":"v-2-stop"}"#,
        )
        .await;
    // As in the happy-path test: `stop_voice_session` accepts only after the terminal has
    // already been forwarded, so read through to the accept, not just the terminal.
    let after_stop = gateway
        .read_until(|message| is_accepted(message, "v-2-stop"))
        .await;
    assert!(after_stop.iter().any(is_voice_terminal));
    assert!(is_accepted(after_stop.last().unwrap(), "v-2-stop"));

    gateway
        .write_message(&start_turn(
            "start-after-voice",
            "fixture transcript after voice",
        ))
        .await;
    let start_accepted = gateway.read_message().await;
    assert_accepted(&start_accepted, "start-after-voice");
    let turn = gateway.read_until(|message| message.is_terminal()).await;
    assert_eq!(turn.last().unwrap().event_type(), Some("turn_completed"));

    let exit = gateway.close().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&[
        "pause-no-session",
        "resume-no-session",
        "stop-no-session",
        "v-2a",
        "v-2b",
        "start-during-voice",
        "v-2-stop",
        "start-after-voice",
        "fixture transcript during voice",
        "fixture transcript after voice",
        "fixture-answer",
    ]);
}

#[tokio::test]
async fn client_eof_mid_voice_session_reaps_the_sidecar() {
    let language = FakeOllamaServer::completing(0).await;
    let speech = FakeTtsServer::completing(0).await;
    let mut gateway =
        GatewayProcess::start_with_voice_lane(language.endpoint(), speech.endpoint(), "quiet")
            .await;

    let ready = gateway.read_message().await;
    assert_eq!(ready.message_type(), "ready");
    assert_voice_status(&ready);

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"start_voice_session","request_id":"v-eof"}"#,
        )
        .await;
    assert_accepted(&gateway.read_message().await, "v-eof");
    let started = gateway.read_message().await;
    assert_eq!(started.event_type(), Some("voice_session_started"));

    let sidecar_pid = gateway.sidecar_pid().await;

    gateway.stdin.take();
    let exit = gateway.finish().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&["v-eof"]);

    assert_process_reaped(sidecar_pid).await;
}

#[tokio::test]
async fn unconfigured_voice_still_rejects() {
    let server = FakeOllamaServer::completing(0).await;
    let mut gateway = GatewayProcess::start(server.endpoint()).await;
    assert_eq!(gateway.read_message().await.message_type(), "ready");

    gateway
        .write_message(
            r#"{"protocol_version":1,"type":"start_voice_session","request_id":"no-voice"}"#,
        )
        .await;
    let rejection = gateway.read_message().await;
    assert_rejected(&rejection, "no-voice", "invalid_state");
    assert!(rejection.raw.contains("voice is unavailable"));

    let exit = gateway.close().await;
    assert!(exit.status.success());
    exit.assert_content_free_stderr(&["no-voice"]);
}

fn index_of(event_types: &[&str], event_type: &str) -> usize {
    event_types
        .iter()
        .position(|candidate| *candidate == event_type)
        .unwrap_or_else(|| panic!("{event_type} was not observed among {event_types:?}"))
}
