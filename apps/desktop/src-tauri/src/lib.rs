#![forbid(unsafe_code)]

pub mod gateway_bridge;
pub mod history_store;

use std::sync::{Arc, Mutex};

use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, RunEvent, State};

use crate::gateway_bridge::{close_for_app_exit, GatewayBridge, ValidatedPaths};
use crate::history_store::{ConversationHistory, ConversationHistoryStore, ConversationSummary};

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryStorageInfo {
    database_path: String,
}

#[derive(Default)]
struct HistoryState(Mutex<Option<Arc<ConversationHistoryStore>>>);

fn app_history_store(app: &AppHandle) -> Result<Arc<ConversationHistoryStore>, String> {
    let state = app.state::<HistoryState>();
    let mut slot = state
        .0
        .lock()
        .map_err(|_| "conversation history store is unavailable".to_owned())?;
    if let Some(store) = slot.as_ref() {
        return Ok(Arc::clone(store));
    }
    let database = app
        .path()
        .app_data_dir()
        .map_err(|_| "conversation history path could not be resolved".to_owned())?
        .join("conversations.sqlite3");
    let store =
        Arc::new(ConversationHistoryStore::open(&database).map_err(|error| error.to_string())?);
    *slot = Some(Arc::clone(&store));
    Ok(store)
}

async fn with_history_store<T, F>(app: AppHandle, operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&ConversationHistoryStore) -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(move || {
        let store = app_history_store(&app)?;
        operation(&store)
    })
    .await
    .map_err(|_| "conversation history task could not complete".to_owned())?
}

#[tauri::command]
async fn history_storage_info(app: AppHandle) -> Result<HistoryStorageInfo, String> {
    with_history_store(app, |store| {
        Ok(HistoryStorageInfo {
            database_path: store.database_path().to_string_lossy().into_owned(),
        })
    })
    .await
}

#[tauri::command]
async fn list_conversation_history(app: AppHandle) -> Result<Vec<ConversationSummary>, String> {
    with_history_store(app, |store| store.list().map_err(|error| error.to_string())).await
}

#[tauri::command]
async fn get_conversation_history(
    app: AppHandle,
    id: String,
) -> Result<Option<ConversationHistory>, String> {
    with_history_store(app, move |store| {
        store.get(&id).map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn save_conversation_history(
    app: AppHandle,
    conversation: ConversationHistory,
) -> Result<(), String> {
    with_history_store(app, move |store| {
        store.save(&conversation).map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn delete_conversation_history(app: AppHandle, id: String) -> Result<(), String> {
    with_history_store(app, move |store| {
        store.delete(&id).map_err(|error| error.to_string())
    })
    .await
}

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
        .manage(HistoryState::default())
        .invoke_handler(tauri::generate_handler![
            open_runtime,
            send_runtime,
            close_runtime,
            history_storage_info,
            list_conversation_history,
            get_conversation_history,
            save_conversation_history,
            delete_conversation_history
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
