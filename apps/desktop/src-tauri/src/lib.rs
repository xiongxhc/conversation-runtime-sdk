#![forbid(unsafe_code)]

pub mod gateway_bridge;
pub mod history_store;
pub mod runtime_setup;

use std::sync::{Arc, Mutex};

use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, RunEvent, State};

use crate::gateway_bridge::{close_for_app_exit, GatewayBridge, ValidatedPaths};
use crate::history_store::{
    ContinuationState, ConversationHistory, ConversationHistoryStore, ConversationSummary,
    HistoryRevision, PreparedContinuation,
};
use crate::runtime_setup::RuntimeSetupState;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryStorageInfo {
    database_path: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct HistorySaveResult {
    revision: HistoryRevision,
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
    expected_revision: Option<HistoryRevision>,
) -> Result<HistorySaveResult, String> {
    with_history_store(app, move |store| {
        store
            .save_revisioned(&conversation, expected_revision)
            .map(|revision| HistorySaveResult { revision })
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn delete_conversation_history(
    app: AppHandle,
    id: String,
    expected_revision: HistoryRevision,
) -> Result<(), String> {
    with_history_store(app, move |store| {
        store
            .delete_revisioned(&id, expected_revision)
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn prepare_conversation_continuation(
    app: AppHandle,
    source_id: String,
    expected_revision: HistoryRevision,
    branch_id: String,
    operation_id: String,
    now_ms: i64,
) -> Result<PreparedContinuation, String> {
    with_history_store(app, move |store| {
        store
            .prepare_continuation(
                &source_id,
                expected_revision,
                now_ms,
                &branch_id,
                &operation_id,
            )
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn set_conversation_continuation_state(
    app: AppHandle,
    branch_id: String,
    expected_revision: HistoryRevision,
    state: ContinuationState,
) -> Result<HistorySaveResult, String> {
    with_history_store(app, move |store| {
        store
            .set_continuation_state(&branch_id, expected_revision, state)
            .map(|revision| HistorySaveResult { revision })
            .map_err(|error| error.to_string())
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
        .manage(RuntimeSetupState::default())
        .invoke_handler(tauri::generate_handler![
            open_runtime,
            send_runtime,
            close_runtime,
            history_storage_info,
            list_conversation_history,
            get_conversation_history,
            save_conversation_history,
            delete_conversation_history,
            prepare_conversation_continuation,
            set_conversation_continuation_state,
            runtime_setup::runtime_setup_defaults,
            runtime_setup::discover_local_models,
            runtime_setup::check_local_model_latency,
            runtime_setup::prepare_runtime_config
        ])
        .build(tauri::generate_context!())
        .expect("failed to build Conversation Runtime desktop app")
        .run(|app, event| {
            if matches!(event, RunEvent::ExitRequested { .. } | RunEvent::Exit) {
                let bridge = app.state::<GatewayBridge>();
                let setup = app.state::<RuntimeSetupState>();
                tauri::async_runtime::block_on(async {
                    let _ = close_for_app_exit(&bridge).await;
                    let _ = setup.shutdown().await;
                });
            }
        });
}
