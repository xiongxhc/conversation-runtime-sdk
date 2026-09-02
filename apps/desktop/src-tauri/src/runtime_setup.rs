use std::fmt;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use conversation_model_adapters::{LanguageModelRequest, OllamaConfig, OllamaLanguageModel};
use conversation_protocol::TurnId;
use conversation_runtime_gateway::{
    GatewayDeploymentConfig, LanguageDeployment, ProviderEnvironmentPolicy, ProviderHost,
    ProviderSupervisor,
};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};
use tokio::sync::{Mutex, MutexGuard, Notify};
use tokio::time::{timeout, timeout_at};
use tokio_util::sync::CancellationToken;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:11434";
const PROVIDER_HOST_ID: &str = "local-language";
const PROVIDER_STARTUP_TIMEOUT_MS: u64 = 10_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const BENCHMARK_TOTAL_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_DISCOVERY_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_DISCOVERED_MODELS: usize = 32;
const MAX_MODEL_ID_BYTES: usize = 256;
const PRIVATE_CONFIG_NAME: &str = "runtime.toml";
const BENCHMARK_PROMPT: &str = "Reply with the single word ready.";

static TEMPORARY_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedProviderLaunch {
    pub executable: String,
    #[serde(default)]
    pub argv: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSetupDefaults {
    pub endpoint: String,
    pub gateway_path: String,
    pub config_path: String,
}

#[derive(Debug, Eq, PartialEq)]
pub struct RuntimeSetupPaths {
    pub gateway_path: PathBuf,
    pub config_path: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedRuntimeConfig {
    pub config_path: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalModelLatency {
    pub first_delta_ms: u64,
    pub total_ms: u64,
    pub ollama_total_duration_ns: Option<u64>,
    pub ollama_load_duration_ns: Option<u64>,
    pub ollama_prompt_eval_count: Option<u64>,
    pub ollama_prompt_eval_duration_ns: Option<u64>,
    pub ollama_eval_count: Option<u64>,
    pub ollama_eval_duration_ns: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum LocalModelLatencyRequest {
    Start {
        request_id: String,
        endpoint: String,
        model: String,
    },
    Cancel {
        request_id: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum LocalModelLatencyResponse {
    Completed { report: LocalModelLatency },
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeSetupError(&'static str);

impl fmt::Display for RuntimeSetupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for RuntimeSetupError {}

struct TemporaryProvider {
    endpoint: String,
    host: ProviderHost,
    supervisor: ProviderSupervisor,
}

struct ActiveBenchmark {
    endpoint: String,
    request_id: String,
    cancellation: CancellationToken,
}

#[derive(Default)]
struct RuntimeSetupLifecycle {
    temporary_provider: Option<TemporaryProvider>,
    active_benchmark: Option<ActiveBenchmark>,
}

pub struct RuntimeSetupState {
    lifecycle: Mutex<RuntimeSetupLifecycle>,
    benchmark_finished: Notify,
}

impl Default for RuntimeSetupState {
    fn default() -> Self {
        Self {
            lifecycle: Mutex::new(RuntimeSetupLifecycle::default()),
            benchmark_finished: Notify::new(),
        }
    }
}

impl RuntimeSetupState {
    pub async fn shutdown(&self) -> Result<(), RuntimeSetupError> {
        shutdown_temporary_provider(self).await
    }
}

#[tauri::command]
pub async fn runtime_setup_defaults(app: AppHandle) -> Result<RuntimeSetupDefaults, String> {
    let resource_root = app
        .path()
        .resource_dir()
        .map_err(|_| RuntimeSetupError("bundled_runtime_missing").to_string())?;
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|_| RuntimeSetupError("configuration_failed").to_string())?;
    let paths =
        resolve_runtime_paths(&resource_root, &app_data).map_err(|error| error.to_string())?;
    Ok(RuntimeSetupDefaults {
        endpoint: DEFAULT_ENDPOINT.to_owned(),
        gateway_path: paths.gateway_path.to_string_lossy().into_owned(),
        config_path: paths.config_path.to_string_lossy().into_owned(),
    })
}

#[tauri::command]
pub async fn discover_local_models(
    endpoint: String,
    managed_provider: Option<ManagedProviderLaunch>,
    state: State<'_, RuntimeSetupState>,
) -> Result<Vec<String>, String> {
    discover_local_models_at(&endpoint, managed_provider, &state)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn check_local_model_latency(
    request: LocalModelLatencyRequest,
    state: State<'_, RuntimeSetupState>,
) -> Result<LocalModelLatencyResponse, String> {
    check_local_model_latency_request(request, &state)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn prepare_runtime_config(
    app: AppHandle,
    endpoint: String,
    model: String,
    state: State<'_, RuntimeSetupState>,
) -> Result<PreparedRuntimeConfig, String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|_| RuntimeSetupError("configuration_failed").to_string())?;
    prepare_runtime_config_at(&app_data, &endpoint, &model, &state)
        .await
        .map_err(|error| error.to_string())
}

pub async fn discover_local_models_at(
    endpoint: &str,
    managed_provider: Option<ManagedProviderLaunch>,
    state: &RuntimeSetupState,
) -> Result<Vec<String>, RuntimeSetupError> {
    let endpoint = normalize_endpoint(validated_endpoint(endpoint)?);
    let endpoint_key = endpoint.as_str().to_owned();
    let tags_url = tags_url(&endpoint);
    let mut lifecycle = acquire_transition(state).await;
    shutdown_stored_provider(&mut lifecycle.temporary_provider).await?;

    let Some(launch) = managed_provider else {
        return discover_models(&tags_url).await;
    };
    let host = ProviderHost::gateway_owned(
        PROVIDER_HOST_ID,
        tags_url.as_str(),
        PROVIDER_STARTUP_TIMEOUT_MS,
        ProviderEnvironmentPolicy::Clear,
        launch.executable,
        launch.argv,
    )
    .map_err(|_| RuntimeSetupError("provider_unavailable"))?;
    let supervisor = ProviderSupervisor::start(vec![host.clone()], CancellationToken::new())
        .await
        .map_err(|error| RuntimeSetupError(error.diagnostic_code()))?;
    match discover_models(&tags_url).await {
        Ok(models) => {
            lifecycle.temporary_provider = Some(TemporaryProvider {
                endpoint: endpoint_key,
                host,
                supervisor,
            });
            Ok(models)
        }
        Err(error) => {
            supervisor
                .shutdown()
                .await
                .map_err(|_| RuntimeSetupError("configuration_failed"))?;
            Err(error)
        }
    }
}

pub async fn check_local_model_latency_request(
    request: LocalModelLatencyRequest,
    state: &RuntimeSetupState,
) -> Result<LocalModelLatencyResponse, RuntimeSetupError> {
    match request {
        LocalModelLatencyRequest::Start {
            request_id,
            endpoint,
            model,
        } => {
            validate_request_id(&request_id)?;
            let endpoint = normalize_endpoint(validated_endpoint(&endpoint)?);
            validate_model_identifier(&model).map_err(|_| RuntimeSetupError("benchmark_failed"))?;
            let endpoint_key = endpoint.as_str().to_owned();
            let cancellation = begin_benchmark(state, &endpoint_key, request_id.clone()).await?;
            let result =
                check_local_model_latency_at(endpoint.as_str(), &model, cancellation).await;
            finish_benchmark(state, &request_id, &endpoint_key).await;
            result.map(|report| LocalModelLatencyResponse::Completed { report })
        }
        LocalModelLatencyRequest::Cancel { request_id } => {
            validate_request_id(&request_id)?;
            cancel_benchmark(state, &request_id).await?;
            Ok(LocalModelLatencyResponse::Cancelled)
        }
    }
}

pub async fn check_local_model_latency_at(
    endpoint: &str,
    model: &str,
    cancellation: CancellationToken,
) -> Result<LocalModelLatency, RuntimeSetupError> {
    let endpoint = validated_endpoint(endpoint)?;
    if cancellation.is_cancelled() {
        return Err(RuntimeSetupError("benchmark_cancelled"));
    }
    validate_model_identifier(model).map_err(|_| RuntimeSetupError("benchmark_failed"))?;
    let model = OllamaConfig::new(model)
        .and_then(|config| config.with_endpoint(endpoint.as_str()))
        .map_err(|_| RuntimeSetupError("benchmark_failed"))?
        .with_thinking(false)
        .with_temperature(0.0)
        .with_seed(42)
        .with_num_predict(16)
        .map_err(|_| RuntimeSetupError("benchmark_failed"))?
        .with_num_ctx(8192)
        .map_err(|_| RuntimeSetupError("benchmark_failed"))?
        .with_response_start_timeout(REQUEST_TIMEOUT)
        .map_err(|_| RuntimeSetupError("benchmark_failed"))?
        .with_response_chunk_timeout(REQUEST_TIMEOUT)
        .map_err(|_| RuntimeSetupError("benchmark_failed"))?;
    let model = OllamaLanguageModel::new_direct(model);
    let started = Instant::now();
    let deadline = tokio::time::Instant::from_std(started + BENCHMARK_TOTAL_TIMEOUT);
    let mut stream = model.stream_chat(
        LanguageModelRequest::new(TurnId::new(1), BENCHMARK_PROMPT),
        cancellation.clone(),
    );
    let mut first_delta_at = None;

    loop {
        let delta = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(RuntimeSetupError("benchmark_cancelled")),
            result = timeout_at(deadline, stream.recv_delta()) => result.map_err(|_| RuntimeSetupError("benchmark_failed"))?,
        };
        match delta {
            Some(Ok(_)) => {
                first_delta_at.get_or_insert_with(Instant::now);
            }
            Some(Err(_)) | None => break,
        }
    }
    let Some(first_delta_at) = first_delta_at else {
        return Err(if cancellation.is_cancelled() {
            RuntimeSetupError("benchmark_cancelled")
        } else {
            RuntimeSetupError("benchmark_failed")
        });
    };
    let metrics = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(RuntimeSetupError("benchmark_cancelled")),
        result = timeout_at(deadline, stream.final_metrics()) => result
            .map_err(|_| RuntimeSetupError("benchmark_failed"))?
            .map_err(|_| RuntimeSetupError("benchmark_failed"))?,
    };
    let completed = Instant::now();
    Ok(LocalModelLatency {
        first_delta_ms: duration_millis(first_delta_at.duration_since(started)),
        total_ms: duration_millis(completed.duration_since(started)),
        ollama_total_duration_ns: metrics.total_duration_ns(),
        ollama_load_duration_ns: metrics.load_duration_ns(),
        ollama_prompt_eval_count: metrics.prompt_eval_count(),
        ollama_prompt_eval_duration_ns: metrics.prompt_eval_duration_ns(),
        ollama_eval_count: metrics.eval_count(),
        ollama_eval_duration_ns: metrics.eval_duration_ns(),
    })
}

pub async fn prepare_runtime_config_at(
    app_data: &Path,
    endpoint: &str,
    model: &str,
    state: &RuntimeSetupState,
) -> Result<PreparedRuntimeConfig, RuntimeSetupError> {
    let endpoint = normalize_endpoint(validated_endpoint(endpoint)?);
    validate_model_identifier(model).map_err(|_| RuntimeSetupError("configuration_failed"))?;
    let endpoint_key = endpoint.as_str();
    let tags_url = tags_url(&endpoint);
    let mut lifecycle = acquire_transition(state).await;
    if lifecycle
        .temporary_provider
        .as_ref()
        .is_some_and(|provider| provider.endpoint != endpoint_key)
    {
        return Err(RuntimeSetupError("configuration_failed"));
    }
    let temporary = lifecycle.temporary_provider.take();
    let host = if let Some(TemporaryProvider {
        host, supervisor, ..
    }) = temporary
    {
        supervisor
            .shutdown()
            .await
            .map_err(|_| RuntimeSetupError("configuration_failed"))?;
        host
    } else {
        ProviderHost::external(
            PROVIDER_HOST_ID,
            tags_url.as_str(),
            PROVIDER_STARTUP_TIMEOUT_MS,
            ProviderEnvironmentPolicy::Inherit,
        )
        .map_err(|_| RuntimeSetupError("invalid_endpoint"))?
    };
    let contents = GatewayDeploymentConfig::builder(LanguageDeployment::ollama_compatible(
        "local-ollama-compatible",
        endpoint.as_str(),
        model,
        PROVIDER_HOST_ID,
    ))
    .provider_host(host)
    .to_toml()
    .map_err(|_| RuntimeSetupError("configuration_failed"))?;
    let config_path = app_data.join(PRIVATE_CONFIG_NAME);
    write_private_config(&config_path, &contents)?;
    drop(lifecycle);
    Ok(PreparedRuntimeConfig { config_path })
}

pub fn resolve_runtime_paths(
    resource_root: &Path,
    app_data: &Path,
) -> Result<RuntimeSetupPaths, RuntimeSetupError> {
    let gateway_path = resource_root.join("conversation-runtime-gateway");
    if !gateway_path.is_file() {
        return Err(RuntimeSetupError("bundled_runtime_missing"));
    }
    Ok(RuntimeSetupPaths {
        gateway_path,
        config_path: app_data.join(PRIVATE_CONFIG_NAME),
    })
}

async fn shutdown_temporary_provider(state: &RuntimeSetupState) -> Result<(), RuntimeSetupError> {
    let mut lifecycle = acquire_transition(state).await;
    shutdown_stored_provider(&mut lifecycle.temporary_provider).await
}

async fn shutdown_stored_provider(
    temporary: &mut Option<TemporaryProvider>,
) -> Result<(), RuntimeSetupError> {
    if let Some(TemporaryProvider { supervisor, .. }) = temporary.take() {
        supervisor
            .shutdown()
            .await
            .map_err(|_| RuntimeSetupError("configuration_failed"))?;
    }
    Ok(())
}

async fn acquire_transition(state: &RuntimeSetupState) -> MutexGuard<'_, RuntimeSetupLifecycle> {
    loop {
        let benchmark_finished = state.benchmark_finished.notified();
        let lifecycle = state.lifecycle.lock().await;
        let Some(active) = lifecycle.active_benchmark.as_ref() else {
            return lifecycle;
        };
        active.cancellation.cancel();
        drop(lifecycle);
        benchmark_finished.await;
    }
}

async fn begin_benchmark(
    state: &RuntimeSetupState,
    endpoint: &str,
    request_id: String,
) -> Result<CancellationToken, RuntimeSetupError> {
    let mut lifecycle = state.lifecycle.lock().await;
    if lifecycle.active_benchmark.is_some()
        || lifecycle
            .temporary_provider
            .as_ref()
            .is_some_and(|provider| provider.endpoint != endpoint)
    {
        return Err(RuntimeSetupError("benchmark_failed"));
    }
    let cancellation = CancellationToken::new();
    lifecycle.active_benchmark = Some(ActiveBenchmark {
        endpoint: endpoint.to_owned(),
        request_id,
        cancellation: cancellation.clone(),
    });
    Ok(cancellation)
}

async fn cancel_benchmark(
    state: &RuntimeSetupState,
    request_id: &str,
) -> Result<(), RuntimeSetupError> {
    let benchmark_finished = state.benchmark_finished.notified();
    let cancellation = {
        let lifecycle = state.lifecycle.lock().await;
        let active = lifecycle
            .active_benchmark
            .as_ref()
            .filter(|active| active.request_id == request_id)
            .ok_or(RuntimeSetupError("benchmark_cancelled"))?;
        active.cancellation.clone()
    };
    cancellation.cancel();
    benchmark_finished.await;
    Ok(())
}

async fn finish_benchmark(state: &RuntimeSetupState, request_id: &str, endpoint: &str) {
    let mut lifecycle = state.lifecycle.lock().await;
    if lifecycle
        .active_benchmark
        .as_ref()
        .is_some_and(|active| active.request_id == request_id && active.endpoint == endpoint)
    {
        lifecycle.active_benchmark = None;
        state.benchmark_finished.notify_waiters();
    }
}

async fn discover_models(tags_url: &reqwest::Url) -> Result<Vec<String>, RuntimeSetupError> {
    let client = direct_client()?;
    let response = timeout(REQUEST_TIMEOUT, client.get(tags_url.clone()).send())
        .await
        .map_err(|_| RuntimeSetupError("model_discovery_failed"))?
        .map_err(|_| RuntimeSetupError("model_discovery_failed"))?;
    if !response.status().is_success() {
        return Err(RuntimeSetupError("model_discovery_failed"));
    }
    let body = read_bounded_body(response).await?;
    let response: TagsResponse =
        serde_json::from_slice(&body).map_err(|_| RuntimeSetupError("model_discovery_failed"))?;
    if response.models.len() > MAX_DISCOVERED_MODELS {
        return Err(RuntimeSetupError("model_discovery_failed"));
    }
    let mut models = Vec::with_capacity(response.models.len());
    for model in response.models {
        if validate_model_identifier(&model.name).is_err() {
            return Err(RuntimeSetupError("model_discovery_failed"));
        }
        models.push(model.name);
    }
    models.sort();
    models.dedup();
    Ok(models)
}

fn validate_model_identifier(model: &str) -> Result<(), ()> {
    if model.is_empty()
        || model.trim() != model
        || model.len() > MAX_MODEL_ID_BYTES
        || model.chars().any(char::is_control)
    {
        return Err(());
    }
    Ok(())
}

fn validate_request_id(request_id: &str) -> Result<(), RuntimeSetupError> {
    if request_id.is_empty()
        || request_id.trim() != request_id
        || request_id.len() > 128
        || request_id.chars().any(char::is_control)
    {
        return Err(RuntimeSetupError("benchmark_failed"));
    }
    Ok(())
}

fn validated_endpoint(endpoint: &str) -> Result<reqwest::Url, RuntimeSetupError> {
    ProviderHost::external(
        "runtime-setup-validation",
        endpoint,
        PROVIDER_STARTUP_TIMEOUT_MS,
        ProviderEnvironmentPolicy::Inherit,
    )
    .map_err(|_| RuntimeSetupError("invalid_endpoint"))?;
    reqwest::Url::parse(endpoint).map_err(|_| RuntimeSetupError("invalid_endpoint"))
}

fn normalize_endpoint(mut endpoint: reqwest::Url) -> reqwest::Url {
    let path = endpoint.path().trim_end_matches('/').to_owned();
    endpoint.set_path(&path);
    endpoint
}

fn tags_url(endpoint: &reqwest::Url) -> reqwest::Url {
    let mut tags_url = endpoint.clone();
    let base_path = endpoint.path().trim_end_matches('/');
    let path = if base_path.is_empty() {
        "/api/tags".to_owned()
    } else {
        format!("{base_path}/api/tags")
    };
    tags_url.set_path(&path);
    tags_url.set_query(None);
    tags_url.set_fragment(None);
    tags_url
}

fn direct_client() -> Result<reqwest::Client, RuntimeSetupError> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|_| RuntimeSetupError("model_discovery_failed"))
}

async fn read_bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, RuntimeSetupError> {
    let mut body = Vec::new();
    loop {
        let chunk = timeout(REQUEST_TIMEOUT, response.chunk())
            .await
            .map_err(|_| RuntimeSetupError("model_discovery_failed"))?
            .map_err(|_| RuntimeSetupError("model_discovery_failed"))?;
        let Some(chunk) = chunk else {
            return Ok(body);
        };
        if chunk.len() > MAX_DISCOVERY_RESPONSE_BYTES.saturating_sub(body.len()) {
            return Err(RuntimeSetupError("model_discovery_failed"));
        }
        body.extend_from_slice(&chunk);
    }
}

fn write_private_config(path: &Path, contents: &str) -> Result<(), RuntimeSetupError> {
    let parent = path
        .parent()
        .ok_or(RuntimeSetupError("configuration_failed"))?;
    fs::create_dir_all(parent).map_err(|_| RuntimeSetupError("configuration_failed"))?;
    if fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(RuntimeSetupError("configuration_failed"));
    }
    let temporary = create_private_temporary_file(parent)?;
    let result = (|| {
        use std::io::Write;

        let mut file = temporary.0;
        file.write_all(contents.as_bytes())
            .map_err(|_| RuntimeSetupError("configuration_failed"))?;
        file.sync_all()
            .map_err(|_| RuntimeSetupError("configuration_failed"))?;
        drop(file);
        fs::rename(&temporary.1, path).map_err(|_| RuntimeSetupError("configuration_failed"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary.1);
    }
    result
}

fn create_private_temporary_file(parent: &Path) -> Result<(fs::File, PathBuf), RuntimeSetupError> {
    for _ in 0..32 {
        let sequence = TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".runtime-setup-{}-{sequence}.toml",
            std::process::id()
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(RuntimeSetupError("configuration_failed")),
        }
    }
    Err(RuntimeSetupError("configuration_failed"))
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[derive(Deserialize)]
struct TagsResponse {
    models: Vec<TaggedModel>,
}

#[derive(Deserialize)]
struct TaggedModel {
    name: String,
}
