mod support;

use conversation_runtime_gateway::FrameReader;
use support::{config_contents, gateway_command, PROCESS_TIMEOUT};
use tokio::time::timeout;

#[cfg(unix)]
#[path = "support/restart_recording.rs"]
mod restart_recording;

// A global version bump breaks unchanged v3 clients; an ignored argv opt-in prevents recovery.
#[tokio::test]
async fn compiled_gateway_keeps_default_v3_and_explicit_v4_exposes_current_identity() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("gateway.toml");
    std::fs::write(&config, config_contents("http://127.0.0.1:9", None)).unwrap();
    for explicit_v4 in [false, true] {
        let mut command = gateway_command(&config);
        if explicit_v4 {
            command.args(["--client-protocol-version", "4"]);
        }
        let mut child = command.spawn().unwrap();
        let frame = timeout(
            PROCESS_TIMEOUT,
            FrameReader::new(child.stdout.take().unwrap()).read_frame(),
        )
        .await;
        drop(child.stdin.take());
        let exited = timeout(PROCESS_TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(
            exited.success(),
            "the selected control version must start successfully"
        );
        let ready: serde_json::Value = serde_json::from_slice(
            &frame
                .unwrap()
                .unwrap()
                .expect("gateway must announce ready"),
        )
        .unwrap();
        assert_eq!(ready["protocol_version"], if explicit_v4 { 4 } else { 3 });
        if explicit_v4 {
            assert!(
                ready["current_session_id"]
                    .as_str()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
                    > 0
            );
        } else {
            assert_eq!(ready.as_object().unwrap().len(), 3);
            assert!(ready.get("current_session_id").is_none());
        }
    }
}
