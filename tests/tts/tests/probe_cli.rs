#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn reports_distinct_timeout_after_synthesis_cleanup() {
    let fixture = tempfile::tempdir().unwrap();
    let say = fixture.path().join("slow-say");
    std::fs::write(&say, "#!/bin/sh\nexec /bin/sleep 1\n").unwrap();
    std::fs::set_permissions(&say, std::fs::Permissions::from_mode(0o700)).unwrap();
    let player = fixture.path().join("unused-player");
    std::fs::write(&player, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&player, std::fs::Permissions::from_mode(0o700)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_conversation-tts-probe"))
        .args(["--no-play", "timeout locally"])
        .env("CONVERSATION_TTS_SAY_PATH", say)
        .env("CONVERSATION_TTS_PLAYER_PATH", player)
        .env("CONVERSATION_TTS_TIMEOUT_MS", "50")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("status=error"));
    assert!(error.contains("stage=timeout"));
    assert!(error.contains("error=probe deadline exceeded"));
    assert!(!error.contains("timeout locally"));
}
