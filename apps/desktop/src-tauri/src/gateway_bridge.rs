use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use conversation_protocol::MAX_CLIENT_FRAME_BYTES;
use conversation_runtime_gateway::{FrameReader, FrameWriter};
use serde_json::{json, Value};
use tauri::ipc::Channel;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub struct GatewayBridgeError(&'static str);

impl fmt::Display for GatewayBridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for GatewayBridgeError {}

#[derive(Debug)]
pub struct ValidatedPaths {
    gateway: PathBuf,
    config: PathBuf,
}

impl ValidatedPaths {
    pub fn new(
        gateway: impl AsRef<Path>,
        config: impl AsRef<Path>,
    ) -> Result<Self, GatewayBridgeError> {
        let gateway = gateway.as_ref();
        if !gateway.is_absolute() {
            return Err(GatewayBridgeError("gateway path must be absolute"));
        }

        let config = config.as_ref();
        if !config.is_absolute() {
            return Err(GatewayBridgeError("config path must be absolute"));
        }

        Ok(Self {
            gateway: gateway.to_owned(),
            config: config.to_owned(),
        })
    }
}

#[derive(Default)]
pub struct GatewayBridge {
    lifecycle: Mutex<GatewayLifecycle>,
}

struct ActiveGateway {
    child: Child,
    stdin: FrameWriter<ChildStdin>,
    forwarder: JoinHandle<()>,
}

#[derive(Default)]
enum GatewayLifecycle {
    #[default]
    Idle,
    Running(ActiveGateway),
    Closing,
}

impl GatewayBridge {
    pub async fn open(
        &self,
        paths: ValidatedPaths,
        messages: Channel<Value>,
    ) -> Result<(), GatewayBridgeError> {
        let mut lifecycle = self.lifecycle.lock().await;
        if !matches!(*lifecycle, GatewayLifecycle::Idle) {
            return Err(GatewayBridgeError("runtime is already open"));
        }

        let stderr = paths
            .config
            .parent()
            .and_then(open_gateway_log)
            .map_or_else(Stdio::null, Stdio::from);
        let mut child = Command::new(&paths.gateway)
            .arg("--config")
            .arg(&paths.config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .map_err(|_| GatewayBridgeError("runtime could not start"))?;

        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill().await;
            return Err(GatewayBridgeError("runtime could not start"));
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill().await;
            return Err(GatewayBridgeError("runtime could not start"));
        };

        *lifecycle = GatewayLifecycle::Running(ActiveGateway {
            child,
            stdin: FrameWriter::new(stdin),
            forwarder: tokio::spawn(forward_gateway_output(stdout, messages)),
        });
        Ok(())
    }

    pub async fn send(&self, payload: &str) -> Result<(), GatewayBridgeError> {
        if payload.len() > MAX_CLIENT_FRAME_BYTES {
            return Err(GatewayBridgeError(
                "runtime payload exceeds maximum frame size",
            ));
        }
        serde_json::from_str::<Value>(payload)
            .map_err(|_| GatewayBridgeError("runtime payload must be valid JSON"))?;

        let mut lifecycle = self.lifecycle.lock().await;
        let GatewayLifecycle::Running(active) = &mut *lifecycle else {
            return Err(GatewayBridgeError("runtime is not open"));
        };
        active
            .stdin
            .write_frame(payload.as_bytes())
            .await
            .map_err(|_| GatewayBridgeError("runtime I/O failed"))
    }

    pub async fn close(&self) -> Result<(), GatewayBridgeError> {
        let mut lifecycle = self.lifecycle.lock().await;
        let active = std::mem::replace(&mut *lifecycle, GatewayLifecycle::Closing);
        let GatewayLifecycle::Running(ActiveGateway {
            mut child,
            stdin,
            forwarder,
        }) = active
        else {
            *lifecycle = GatewayLifecycle::Idle;
            return Ok(());
        };

        drop(stdin);
        let child_result = match timeout(SHUTDOWN_TIMEOUT, child.wait()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(_)) | Err(_) => child
                .kill()
                .await
                .map_err(|_| GatewayBridgeError("runtime close failed")),
        };
        let forwarder_result = forwarder
            .await
            .map_err(|_| GatewayBridgeError("runtime forwarding failed"));
        *lifecycle = GatewayLifecycle::Idle;
        child_result.and(forwarder_result)
    }
}

#[cfg(unix)]
fn open_gateway_log(directory: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(directory.join("gateway.log"))
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    file.set_len(0).ok()?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .ok()?;
    Some(file)
}

#[cfg(not(unix))]
fn open_gateway_log(_directory: &Path) -> Option<std::fs::File> {
    None
}

pub async fn close_for_app_exit(bridge: &GatewayBridge) -> Result<(), GatewayBridgeError> {
    bridge.close().await
}

async fn forward_gateway_output(stdout: ChildStdout, messages: Channel<Value>) {
    let mut reader = FrameReader::new(stdout);
    loop {
        let Ok(Some(frame)) = reader.read_frame().await else {
            break;
        };
        let Ok(message) = serde_json::from_slice::<Value>(&frame) else {
            break;
        };
        if messages
            .send(json!({
                "bridge_version": 1,
                "type": "gateway_message",
                "message": message
            }))
            .is_err()
        {
            break;
        }
    }
    let _ = messages.send(json!({
        "bridge_version": 1,
        "type": "runtime_ended"
    }));
}
