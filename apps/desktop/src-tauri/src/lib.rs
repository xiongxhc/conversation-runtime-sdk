#![forbid(unsafe_code)]

pub mod gateway_bridge;

use tauri::ipc::Channel;
use tauri::{Manager, RunEvent, State};

use crate::gateway_bridge::{close_for_app_exit, GatewayBridge, ValidatedPaths};

#[tauri::command]
async fn open_runtime(
    bridge: State<'_, GatewayBridge>,
    gateway_path: String,
    config_path: String,
    messages: Channel<serde_json::Value>,
) -> Result<(), String> {
    let paths =
        ValidatedPaths::new(gateway_path, config_path).map_err(|error| error.to_string())?;
    bridge
        .open(paths, messages)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn send_runtime(bridge: State<'_, GatewayBridge>, payload: String) -> Result<(), String> {
    bridge
        .send(&payload)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn close_runtime(bridge: State<'_, GatewayBridge>) -> Result<(), String> {
    bridge.close().await.map_err(|error| error.to_string())
}

pub fn run() {
    tauri::Builder::default()
        .manage(GatewayBridge::default())
        .invoke_handler(tauri::generate_handler![
            open_runtime,
            send_runtime,
            close_runtime
        ])
        .build(tauri::generate_context!())
        .expect("failed to build Conversation Runtime desktop app")
        .run(|app, event| {
            if matches!(event, RunEvent::ExitRequested { .. } | RunEvent::Exit) {
                let bridge = app.state::<GatewayBridge>();
                let _ = tauri::async_runtime::block_on(close_for_app_exit(&bridge));
            }
        });
}
