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

#[test]
fn lists_voices_without_starting_synthesis_or_playback() {
    let fixture = tempfile::tempdir().unwrap();
    let fake_say = fixture.path().join("fake-say");
    let synthesis_marker = fixture.path().join("synthesis-marker");
    std::fs::write(
        &fake_say,
        format!(
            "#!/bin/sh
if [ \"$1\" = '-v' ] && [ \"$2\" = '?' ]; then
  printf 'Tingting zh_CN # 你好\\n'
  exit 0
fi
printf 'synthesis' > '{}'
exit 1
",
            synthesis_marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake_say, std::fs::Permissions::from_mode(0o700)).unwrap();

    let unused_player = fixture.path().join("unused-player");
    let playback_marker = fixture.path().join("playback-marker");
    std::fs::write(
        &unused_player,
        format!(
            "#!/bin/sh
printf 'playback' > '{}'
",
            playback_marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&unused_player, std::fs::Permissions::from_mode(0o700)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_conversation-tts-probe"))
        .arg("--list-voices")
        .env("CONVERSATION_TTS_SAY_PATH", &fake_say)
        .env("CONVERSATION_TTS_PLAYER_PATH", &unused_player)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Tingting zh_CN # 你好\n"
    );
    assert!(!synthesis_marker.exists());
    assert!(!playback_marker.exists());
}
