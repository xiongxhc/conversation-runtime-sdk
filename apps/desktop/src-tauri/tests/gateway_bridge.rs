#![cfg(unix)]

use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use conversation_desktop::gateway_bridge::{close_for_app_exit, GatewayBridge, ValidatedPaths};
use serde_json::Value;
use tauri::ipc::Channel;
use tokio::time::timeout;

#[test]
fn rejects_relative_gateway_path() {
    let error = ValidatedPaths::new("target/debug/gateway", "/tmp/runtime.toml")
        .expect_err("relative gateway path must fail");
    assert_eq!(error.to_string(), "gateway path must be absolute");
}

#[tokio::test]
async fn close_is_idempotent() {
    let bridge = GatewayBridge::default();
    bridge.close().await.expect("first close");
    bridge.close().await.expect("second close");
}

#[tokio::test]
async fn reopen_waits_until_the_closing_gateway_is_reaped() {
    let fixture = gateway_fixture();
    let bridge = Arc::new(GatewayBridge::default());
    bridge
        .open(paths_for(&fixture), channel())
        .await
        .expect("open first gateway");
    wait_for_path(&fixture.started).await;

    let closing_bridge = Arc::clone(&bridge);
    let closing = tokio::spawn(async move { closing_bridge.close().await });
    wait_for_path(&fixture.closing).await;

    let reopening_bridge = Arc::clone(&bridge);
    let reopening_fixture = fixture.clone();
    let mut reopening = tokio::spawn(async move {
        reopening_bridge
            .open(paths_for(&reopening_fixture), channel())
            .await
    });
    assert!(
        timeout(Duration::from_millis(50), &mut reopening)
            .await
            .is_err(),
        "reopen must wait until the closing gateway is reaped"
    );

    closing.await.expect("close task").expect("close gateway");
    reopening
        .await
        .expect("reopen task")
        .expect("open second gateway");
    bridge.close().await.expect("close second gateway");
}

#[tokio::test]
async fn app_exit_cleanup_waits_for_gateway_reaping() {
    let fixture = gateway_fixture();
    let bridge = GatewayBridge::default();
    bridge
        .open(paths_for(&fixture), channel())
        .await
        .expect("open gateway");
    wait_for_path(&fixture.started).await;

    close_for_app_exit(&bridge).await.expect("app exit cleanup");

    assert!(
        fixture.finished.exists(),
        "gateway must finish before exit cleanup returns"
    );
    let error = bridge.send("{}").await.expect_err("gateway must be closed");
    assert_eq!(error.to_string(), "runtime is not open");
}

#[tokio::test]
async fn gateway_diagnostics_replace_old_content_with_private_permissions() {
    let fixture = gateway_fixture();
    let log = fixture.config.parent().unwrap().join("gateway.log");
    fs::write(&log, "stale diagnostic\n").expect("stale diagnostic");
    let mut permissions = fs::metadata(&log).unwrap().permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(&log, permissions).unwrap();
    let bridge = GatewayBridge::default();

    bridge
        .open(paths_for(&fixture), channel())
        .await
        .expect("open gateway");
    wait_for_path(&fixture.started).await;
    bridge.close().await.expect("close gateway");

    assert_eq!(fs::read_to_string(&log).unwrap(), "fixture diagnostic\n");
    assert_eq!(
        fs::metadata(&log).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
async fn gateway_diagnostics_never_follow_a_leaf_symlink() {
    let fixture = gateway_fixture();
    let log = fixture.config.parent().unwrap().join("gateway.log");
    let target = fixture.config.parent().unwrap().join("diagnostic-target");
    fs::write(&target, "preserve me\n").unwrap();
    std::os::unix::fs::symlink(&target, &log).unwrap();
    let bridge = GatewayBridge::default();

    bridge
        .open(paths_for(&fixture), channel())
        .await
        .expect("open gateway without unsafe diagnostic sink");
    wait_for_path(&fixture.started).await;
    bridge.close().await.expect("close gateway");

    assert_eq!(fs::read_to_string(&target).unwrap(), "preserve me\n");
}

#[tokio::test]
async fn gateway_diagnostics_reject_a_named_pipe() {
    let fixture = gateway_fixture();
    let log = fixture.config.parent().unwrap().join("gateway.log");
    assert!(std::process::Command::new("mkfifo")
        .arg(&log)
        .status()
        .unwrap()
        .success());
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let worker_fixture = fixture.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let bridge = GatewayBridge::default();
            let result = bridge.open(paths_for(&worker_fixture), channel()).await;
            if result.is_ok() {
                wait_for_path(&worker_fixture.started).await;
                bridge.close().await.unwrap();
            }
            sender.send(result).unwrap();
        });
    });

    receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("gateway startup must not block on a named pipe")
        .expect("open gateway without unsafe diagnostic sink");
    assert!(fs::metadata(&log).unwrap().file_type().is_fifo());
}

#[tokio::test]
async fn unexpected_gateway_exit_is_forwarded_to_the_desktop() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let gateway = temporary.path().join("gateway.sh");
    let config = temporary.path().join("runtime.toml");
    fs::write(&gateway, "#!/bin/sh\nexit 0\n").expect("gateway fixture");
    let mut permissions = fs::metadata(&gateway)
        .expect("gateway fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&gateway, permissions).expect("gateway fixture permissions");
    fs::write(&config, "").expect("gateway fixture configuration");

    let events = Arc::new(StdMutex::new(Vec::<Value>::new()));
    let captured_events = Arc::clone(&events);
    let bridge = GatewayBridge::default();
    bridge
        .open(
            ValidatedPaths::new(&gateway, &config).expect("fixture paths"),
            Channel::new(move |value| {
                captured_events
                    .lock()
                    .expect("event capture")
                    .push(value.deserialize().expect("serialized bridge event"));
                Ok(())
            }),
        )
        .await
        .expect("open gateway");

    timeout(Duration::from_secs(5), async {
        while events.lock().expect("event capture").is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("runtime termination event");

    assert_eq!(
        events.lock().expect("event capture").as_slice(),
        [serde_json::json!({
            "bridge_version": 1,
            "type": "runtime_ended"
        })]
    );
    bridge.close().await.expect("close gateway");
}

#[derive(Clone)]
struct GatewayFixture {
    gateway: PathBuf,
    config: PathBuf,
    started: PathBuf,
    closing: PathBuf,
    finished: PathBuf,
    _temporary: Arc<tempfile::TempDir>,
}

fn gateway_fixture() -> GatewayFixture {
    let temporary = Arc::new(tempfile::tempdir().expect("temporary directory"));
    let gateway = temporary.path().join("gateway.sh");
    let config = temporary.path().join("runtime.toml");
    let marker = temporary.path().join("gateway");
    fs::write(
        &gateway,
        "#!/bin/sh\nset -eu\nmarker=$(/bin/cat \"$2\")\nprintf 'fixture diagnostic\\n' >&2\n: > \"${marker}.started\"\nread -r _ || true\n: > \"${marker}.closing\"\n/bin/sleep 0.2\n: > \"${marker}.finished\"\n",
    )
    .expect("gateway fixture");
    let mut permissions = fs::metadata(&gateway)
        .expect("gateway fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&gateway, permissions).expect("gateway fixture permissions");
    fs::write(&config, marker.display().to_string()).expect("gateway fixture configuration");

    GatewayFixture {
        gateway,
        config,
        started: marker.with_extension("started"),
        closing: marker.with_extension("closing"),
        finished: marker.with_extension("finished"),
        _temporary: temporary,
    }
}

fn paths_for(fixture: &GatewayFixture) -> ValidatedPaths {
    ValidatedPaths::new(&fixture.gateway, &fixture.config).expect("fixture paths")
}

fn channel() -> Channel<Value> {
    Channel::new(|_| Ok(()))
}

async fn wait_for_path(path: &Path) {
    timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("gateway fixture signal");
}
