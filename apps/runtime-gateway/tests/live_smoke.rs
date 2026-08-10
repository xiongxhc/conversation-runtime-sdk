//! Opt-in live smoke: drives one real voice session through the *compiled* gateway against a
//! real sidecar and a private local `[voice]` configuration. This is the gateway-level analogue
//! of `tests/voice`'s manual, real-hardware acceptance harness
//! (`tests/voice/acceptance-macos.sh`) — it is not part of `cargo test --workspace` or the merge
//! gate. `apps/runtime-gateway/tests/voice_session.rs` already proves the lane deterministically
//! against the fake sidecar and fixture STT/LLM/TTS; this file only exercises the same
//! start → activity → stop flow against real hardware, for a human operator to run by hand.
//!
//! With `GATEWAY_VOICE_LIVE_SMOKE` unset (the default everywhere, including CI), this test
//! early-returns and passes. To run it on a dev Mac with real voice hardware and a private
//! local model/config already set up:
//!
//! ```sh
//! GATEWAY_VOICE_LIVE_SMOKE=1 \
//! GATEWAY_VOICE_LIVE_CONFIG=/absolute/path/to/private/gateway.toml \
//!   cargo test -p conversation-runtime-gateway --test live_smoke -- --nocapture
//! ```
//!
//! Output is content-free by construction: only stage names and elapsed milliseconds are ever
//! printed — never transcripts, audio, file paths, or configuration values. No latency or
//! acoustic-quality claim is made or asserted here; that acceptance stays open under the R3
//! human-spoken/acoustic acceptance gate.
#![cfg(unix)]

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use conversation_runtime_gateway::{FrameReader, FrameWriter};
use support::{assert_accepted, gateway_command, is_accepted, is_voice_terminal, WireMessage};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::time::timeout;

/// The fixture-driven deterministic tests reuse `support::PROCESS_TIMEOUT` (5s), tuned for a
/// fake sidecar responding instantly. The live path waits on real hardware and a real
/// human/ambient signal to trigger voice activity detection, so it needs far more room.
const LIVE_FRAME_TIMEOUT: Duration = Duration::from_secs(120);

#[tokio::test]
async fn live_voice_session_smoke() {
    if std::env::var("GATEWAY_VOICE_LIVE_SMOKE").as_deref() != Ok("1") {
        eprintln!(
            "live_smoke: skipped (set GATEWAY_VOICE_LIVE_SMOKE=1 and \
             GATEWAY_VOICE_LIVE_CONFIG=<abs path to a private local voice config> to run)"
        );
        return;
    }
    let config_path = std::env::var("GATEWAY_VOICE_LIVE_CONFIG").expect(
        "GATEWAY_VOICE_LIVE_SMOKE=1 requires GATEWAY_VOICE_LIVE_CONFIG=<abs path to a private \
         local voice config>",
    );
    let config_path = PathBuf::from(config_path);
    assert!(
        config_path.is_absolute(),
        "GATEWAY_VOICE_LIVE_CONFIG must be an absolute path"
    );

    let started = Instant::now();
    let mut child = gateway_command(&config_path)
        .spawn()
        .expect("failed to spawn the compiled gateway");
    let mut writer = FrameWriter::new(child.stdin.take().expect("gateway stdin"));
    let mut reader = FrameReader::new(child.stdout.take().expect("gateway stdout"));
    let mut stderr = child.stderr.take().expect("gateway stderr");
    let stderr_drain = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
    });

    let ready = read_message(&mut reader).await;
    assert_eq!(ready.message_type(), "ready");
    milestone("ready", started);

    write_message(
        &mut writer,
        r#"{"protocol_version":1,"type":"start_voice_session","request_id":"live-start"}"#,
    )
    .await;
    assert_accepted(&read_message(&mut reader).await, "live-start");
    milestone("start_accepted", started);

    loop {
        let message = read_message(&mut reader).await;
        if message.event_type() == Some("voice_activity") {
            milestone("voice_activity", started);
            break;
        }
    }

    write_message(
        &mut writer,
        r#"{"protocol_version":1,"type":"stop_voice_session","request_id":"live-stop"}"#,
    )
    .await;
    // As in `voice_session.rs`: `stop_voice_session` only accepts once the terminal has already
    // been forwarded, so the terminal milestone always precedes the stop-accepted milestone.
    let mut seen_terminal = false;
    loop {
        let message = read_message(&mut reader).await;
        if !seen_terminal && is_voice_terminal(&message) {
            seen_terminal = true;
            milestone("terminal", started);
        }
        if is_accepted(&message, "live-stop") {
            assert!(
                seen_terminal,
                "stop_voice_session accepted before the session terminal arrived"
            );
            milestone("stop_accepted", started);
            break;
        }
    }

    // Close our end of stdin (matches `GatewayProcess::close`'s idiom) so the gateway sees EOF
    // and shuts itself down, then drain stdout to EOF before reaping the process.
    drop(writer);
    while timeout(LIVE_FRAME_TIMEOUT, reader.read_frame())
        .await
        .expect("gateway did not close stdout within the live smoke timeout")
        .expect("frame read failed while draining stdout")
        .is_some()
    {}

    let status = timeout(LIVE_FRAME_TIMEOUT, child.wait())
        .await
        .expect("gateway did not exit within the live smoke timeout")
        .expect("failed to wait on the gateway process");
    let _ = stderr_drain.await;
    assert!(status.success(), "gateway exited with {status}");
    milestone("exited", started);
}

async fn read_message(reader: &mut FrameReader<ChildStdout>) -> WireMessage {
    let payload = timeout(LIVE_FRAME_TIMEOUT, reader.read_frame())
        .await
        .expect("gateway did not produce a frame within the live smoke timeout")
        .expect("frame read failed")
        .expect("gateway stdout closed before the expected frame");
    WireMessage::from_payload(payload)
}

async fn write_message(writer: &mut FrameWriter<ChildStdin>, message: &str) {
    writer
        .write_frame(message.as_bytes())
        .await
        .expect("frame write failed");
}

fn milestone(stage: &str, started: Instant) {
    println!(
        "live_smoke: stage={stage} elapsed_ms={}",
        started.elapsed().as_millis()
    );
}
