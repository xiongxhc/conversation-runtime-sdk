#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use conversation_desktop::runtime_setup::{
    check_local_model_latency_at, check_local_model_latency_request, discover_local_models_at,
    prepare_runtime_config_at, resolve_runtime_paths, LocalModelLatencyRequest,
    LocalModelLatencyResponse, ManagedProviderLaunch, RuntimeSetupState,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn discovery_rejects_non_loopback_endpoints_without_contacting_them() {
    let state = RuntimeSetupState::default();

    let error = discover_local_models_at("http://192.0.2.10:11434", None, &state)
        .await
        .expect_err("non-loopback model discovery must fail");

    assert_eq!(error.to_string(), "invalid_endpoint");
}

#[tokio::test]
async fn discovery_returns_a_bounded_model_list_and_redacts_provider_errors() {
    let server = fake_server(vec![
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"models\":[{\"name\":\"first-local\"},{\"name\":\"second-local\"}]}",
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 27\r\nConnection: close\r\n\r\nprivate provider failure body",
    ])
    .await;
    let state = RuntimeSetupState::default();

    let models = discover_local_models_at(&server.endpoint, None, &state)
        .await
        .expect("model list");
    assert_eq!(models, vec!["first-local", "second-local"]);

    let error = discover_local_models_at(&server.endpoint, None, &state)
        .await
        .expect_err("provider failure must be redacted");
    assert_eq!(error.to_string(), "model_discovery_failed");
}

#[tokio::test]
async fn discovery_rejects_a_response_body_over_64_kib() {
    let body = format!("{{\"models\":[]}}{}", " ".repeat(64 * 1024));
    let server = fake_server(vec![http_ok(&body)]).await;
    let state = RuntimeSetupState::default();

    let error = discover_local_models_at(&server.endpoint, None, &state)
        .await
        .expect_err("oversized discovery body must fail");

    assert_eq!(error.to_string(), "model_discovery_failed");
}

#[tokio::test]
async fn discovery_rejects_more_than_32_models() {
    let models = (0..33)
        .map(|index| format!("{{\"name\":\"model-{index}\"}}"))
        .collect::<Vec<_>>()
        .join(",");
    let server = fake_server(vec![http_ok(&format!("{{\"models\":[{models}]}}"))]).await;
    let state = RuntimeSetupState::default();

    let error = discover_local_models_at(&server.endpoint, None, &state)
        .await
        .expect_err("more than 32 models must fail");

    assert_eq!(error.to_string(), "model_discovery_failed");
}

#[tokio::test]
async fn discovery_rejects_a_model_identifier_over_256_bytes() {
    let identifier = "m".repeat(257);
    let server = fake_server(vec![http_ok(&format!(
        "{{\"models\":[{{\"name\":\"{identifier}\"}}]}}"
    ))])
    .await;
    let state = RuntimeSetupState::default();

    let error = discover_local_models_at(&server.endpoint, None, &state)
        .await
        .expect_err("oversized model identifier must fail");

    assert_eq!(error.to_string(), "model_discovery_failed");
}

#[tokio::test]
async fn benchmark_reports_only_bounded_numeric_metrics_and_discards_response_text() {
    let server = fake_server(vec![
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n{\"message\":{\"role\":\"assistant\",\"content\":\"private generated benchmark text\"},\"done\":false}\n{\"done\":true,\"total_duration\":101,\"load_duration\":102,\"prompt_eval_count\":103,\"prompt_eval_duration\":104,\"eval_count\":105,\"eval_duration\":106}\n",
    ])
    .await;

    let report = check_local_model_latency_at(
        &server.endpoint,
        "fixture-local-model",
        CancellationToken::new(),
    )
    .await
    .expect("benchmark result");

    assert!(report.first_delta_ms <= report.total_ms);
    assert_eq!(report.ollama_total_duration_ns, Some(101));
    assert_eq!(report.ollama_load_duration_ns, Some(102));
    let serialized = serde_json::to_string(&report).expect("serialize report");
    assert!(!serialized.contains("private generated benchmark text"));
    assert!(!serialized.contains("fixture-local-model"));
}

#[tokio::test]
async fn benchmark_cancellation_returns_a_redacted_retryable_error() {
    let server = fake_server(vec![
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n",
    ])
    .await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = check_local_model_latency_at(&server.endpoint, "fixture-local-model", cancellation)
        .await
        .expect_err("cancelled benchmark must not produce a report");

    assert_eq!(error.to_string(), "benchmark_cancelled");
}

#[tokio::test]
async fn benchmark_rejects_a_model_identifier_over_256_bytes_before_contacting_the_provider() {
    let server = counting_server().await;

    let error =
        check_local_model_latency_at(&server.endpoint, &"m".repeat(257), CancellationToken::new())
            .await
            .expect_err("oversized benchmark model identifier must fail");

    assert_eq!(error.to_string(), "benchmark_failed");
    assert_eq!(server.request_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn public_benchmark_cancellation_drops_the_active_provider_request() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let provider_stopped = temporary.path().join("provider-stopped");
    let benchmark_started = temporary.path().join("benchmark-started");
    let benchmark_dropped = temporary.path().join("benchmark-dropped");
    let port = unused_port();
    let endpoint = format!("http://127.0.0.1:{port}");
    let state = Arc::new(RuntimeSetupState::default());
    let launch = ManagedProviderLaunch {
        executable: python_executable().display().to_string(),
        argv: vec![
            "-c".to_owned(),
            managed_benchmark_provider_fixture().to_owned(),
            port.to_string(),
            provider_stopped.display().to_string(),
            benchmark_started.display().to_string(),
            benchmark_dropped.display().to_string(),
        ],
    };
    discover_local_models_at(&endpoint, Some(launch), &state)
        .await
        .expect("managed provider discovery");

    let start_state = Arc::clone(&state);
    let start_endpoint = endpoint.clone();
    let start = tokio::spawn(async move {
        check_local_model_latency_request(
            LocalModelLatencyRequest::Start {
                request_id: "benchmark-cancel".to_owned(),
                endpoint: start_endpoint,
                model: "fixture-local-model".to_owned(),
            },
            &start_state,
        )
        .await
    });
    wait_for_path(&benchmark_started).await;

    let cancellation = check_local_model_latency_request(
        LocalModelLatencyRequest::Cancel {
            request_id: "benchmark-cancel".to_owned(),
        },
        &state,
    )
    .await
    .expect("public cancellation action");
    assert!(matches!(cancellation, LocalModelLatencyResponse::Cancelled));
    let error = start
        .await
        .expect("benchmark task")
        .expect_err("cancelled benchmark must be retryable");
    assert_eq!(error.to_string(), "benchmark_cancelled");
    wait_for_path(&benchmark_dropped).await;

    state.shutdown().await.expect("managed provider cleanup");
    wait_for_path(&provider_stopped).await;
}

#[tokio::test]
async fn config_write_is_atomic_private_and_never_follows_the_destination_symlink() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let config = temporary.path().join("runtime.toml");
    let state = RuntimeSetupState::default();

    let prepared = prepare_runtime_config_at(
        temporary.path(),
        "http://127.0.0.1:11434",
        "fixture-local-model",
        &state,
    )
    .await
    .expect("private config");
    assert_eq!(prepared.config_path, config);
    assert_eq!(
        fs::metadata(&config).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let contents = fs::read_to_string(&config).expect("serialized config");
    assert!(contents.contains("schema_version = 2"));
    assert!(contents.contains("ownership = \"external\""));

    let target = temporary.path().join("private-target.toml");
    fs::write(&target, "preserve me").unwrap();
    fs::remove_file(&config).unwrap();
    symlink(&target, &config).unwrap();
    let error = prepare_runtime_config_at(
        temporary.path(),
        "http://127.0.0.1:11434",
        "fixture-local-model",
        &state,
    )
    .await
    .expect_err("unsafe config destination must fail");
    assert_eq!(error.to_string(), "configuration_failed");
    assert_eq!(fs::read_to_string(target).unwrap(), "preserve me");
}

#[tokio::test]
async fn preparing_config_reaps_a_temporary_managed_provider_before_gateway_owns_it() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let marker = temporary.path().join("provider-stopped");
    let port = unused_port();
    let endpoint = format!("http://127.0.0.1:{port}");
    let state = RuntimeSetupState::default();
    let launch = ManagedProviderLaunch {
        executable: python_executable().display().to_string(),
        argv: vec![
            "-c".to_owned(),
            managed_provider_fixture().to_owned(),
            port.to_string(),
            marker.display().to_string(),
        ],
    };

    let models = discover_local_models_at(&endpoint, Some(launch), &state)
        .await
        .expect("managed provider discovery");
    assert_eq!(models, vec!["fixture-local-model"]);
    let prepared = prepare_runtime_config_at(temporary.path(), &endpoint, &models[0], &state)
        .await
        .expect("config after managed discovery");

    timeout(Duration::from_secs(5), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("temporary managed provider must be reaped");
    let contents = fs::read_to_string(prepared.config_path).unwrap();
    assert!(contents.contains("ownership = \"gateway-owned\""));
}

#[tokio::test]
async fn unmanaged_discovery_reaps_a_stale_managed_provider_before_configuring_another_endpoint() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let marker = temporary.path().join("provider-stopped");
    let managed_port = unused_port();
    let managed_endpoint = format!("http://127.0.0.1:{managed_port}");
    let state = RuntimeSetupState::default();
    let launch = ManagedProviderLaunch {
        executable: python_executable().display().to_string(),
        argv: vec![
            "-c".to_owned(),
            managed_provider_fixture().to_owned(),
            managed_port.to_string(),
            marker.display().to_string(),
        ],
    };
    discover_local_models_at(&managed_endpoint, Some(launch), &state)
        .await
        .expect("managed provider discovery");

    let external = fake_server(vec![http_ok(
        "{\"models\":[{\"name\":\"external-model\"}]}",
    )])
    .await;
    discover_local_models_at(&external.endpoint, None, &state)
        .await
        .expect("unmanaged discovery");
    wait_for_path(&marker).await;

    let prepared = prepare_runtime_config_at(
        temporary.path(),
        &external.endpoint,
        "external-model",
        &state,
    )
    .await
    .expect("external configuration");
    let contents = fs::read_to_string(prepared.config_path).unwrap();
    assert!(contents.contains("ownership = \"external\""));
    assert!(contents.contains(&external.endpoint));
    assert!(!contents.contains(&managed_endpoint));
}

#[tokio::test]
async fn preparation_rejects_a_managed_provider_for_a_different_endpoint() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let marker = temporary.path().join("provider-stopped");
    let managed_port = unused_port();
    let managed_endpoint = format!("http://127.0.0.1:{managed_port}");
    let requested_endpoint = format!("http://127.0.0.1:{}", unused_port());
    let state = RuntimeSetupState::default();
    let launch = ManagedProviderLaunch {
        executable: python_executable().display().to_string(),
        argv: vec![
            "-c".to_owned(),
            managed_provider_fixture().to_owned(),
            managed_port.to_string(),
            marker.display().to_string(),
        ],
    };
    discover_local_models_at(&managed_endpoint, Some(launch), &state)
        .await
        .expect("managed provider discovery");

    let error =
        prepare_runtime_config_at(temporary.path(), &requested_endpoint, "other-model", &state)
            .await
            .expect_err("mismatched managed provider must not be serialized");
    assert_eq!(error.to_string(), "configuration_failed");
    assert!(!temporary.path().join("runtime.toml").exists());

    state.shutdown().await.expect("temporary cleanup");
    wait_for_path(&marker).await;
}

#[tokio::test]
async fn preparation_waits_for_an_in_flight_managed_discovery_before_selecting_ownership() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let marker = temporary.path().join("provider-stopped");
    let discovery_started = temporary.path().join("discovery-started");
    let release = temporary.path().join("release-discovery");
    let port = unused_port();
    let endpoint = format!("http://127.0.0.1:{port}");
    let state = Arc::new(RuntimeSetupState::default());
    let launch = ManagedProviderLaunch {
        executable: python_executable().display().to_string(),
        argv: vec![
            "-c".to_owned(),
            managed_provider_fixture_with_discovery_gate().to_owned(),
            port.to_string(),
            marker.display().to_string(),
            discovery_started.display().to_string(),
            release.display().to_string(),
        ],
    };

    let discovery_state = Arc::clone(&state);
    let discovery_endpoint = endpoint.clone();
    let discovery = tokio::spawn(async move {
        discover_local_models_at(&discovery_endpoint, Some(launch), &discovery_state).await
    });
    wait_for_path(&discovery_started).await;

    let prepare_state = Arc::clone(&state);
    let prepare_endpoint = endpoint.clone();
    let app_data = temporary.path().to_path_buf();
    let mut preparation = tokio::spawn(async move {
        prepare_runtime_config_at(
            &app_data,
            &prepare_endpoint,
            "fixture-local-model",
            &prepare_state,
        )
        .await
    });
    assert!(
        timeout(Duration::from_millis(100), &mut preparation)
            .await
            .is_err(),
        "configuration must not race ahead of an in-flight managed discovery"
    );

    fs::write(&release, "release").unwrap();
    discovery
        .await
        .expect("discovery task")
        .expect("managed discovery");
    let prepared = preparation
        .await
        .expect("preparation task")
        .expect("prepared configuration");
    let contents = fs::read_to_string(prepared.config_path).unwrap();
    assert!(contents.contains("ownership = \"gateway-owned\""));
    wait_for_path(&marker).await;
}

#[tokio::test]
async fn configuration_cancels_and_awaits_an_active_managed_benchmark_before_reaping_it() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let provider_stopped = temporary.path().join("provider-stopped");
    let benchmark_started = temporary.path().join("benchmark-started");
    let benchmark_dropped = temporary.path().join("benchmark-dropped");
    let port = unused_port();
    let endpoint = format!("http://127.0.0.1:{port}");
    let state = Arc::new(RuntimeSetupState::default());
    let launch = managed_benchmark_provider_launch(
        port,
        &provider_stopped,
        &benchmark_started,
        &benchmark_dropped,
    );
    discover_local_models_at(&endpoint, Some(launch), &state)
        .await
        .expect("managed provider discovery");

    let start = start_benchmark(&state, &endpoint, "benchmark-config");
    wait_for_path(&benchmark_started).await;
    let prepared =
        prepare_runtime_config_at(temporary.path(), &endpoint, "fixture-local-model", &state)
            .await
            .expect("configuration after benchmark cancellation");

    let error = start
        .await
        .expect("benchmark task")
        .expect_err("configuration must cancel the active benchmark");
    assert_eq!(error.to_string(), "benchmark_cancelled");
    wait_for_path(&benchmark_dropped).await;
    wait_for_path(&provider_stopped).await;
    assert!(fs::read_to_string(prepared.config_path)
        .unwrap()
        .contains("ownership = \"gateway-owned\""));
}

#[tokio::test]
async fn rediscovery_cancels_and_awaits_an_active_managed_benchmark_before_reaping_it() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let provider_stopped = temporary.path().join("provider-stopped");
    let benchmark_started = temporary.path().join("benchmark-started");
    let benchmark_dropped = temporary.path().join("benchmark-dropped");
    let port = unused_port();
    let endpoint = format!("http://127.0.0.1:{port}");
    let state = Arc::new(RuntimeSetupState::default());
    let launch = managed_benchmark_provider_launch(
        port,
        &provider_stopped,
        &benchmark_started,
        &benchmark_dropped,
    );
    discover_local_models_at(&endpoint, Some(launch), &state)
        .await
        .expect("managed provider discovery");

    let start = start_benchmark(&state, &endpoint, "benchmark-rediscovery");
    wait_for_path(&benchmark_started).await;
    let external = fake_server(vec![http_ok(
        "{\"models\":[{\"name\":\"external-model\"}]}",
    )])
    .await;
    let models = discover_local_models_at(&external.endpoint, None, &state)
        .await
        .expect("rediscovery after benchmark cancellation");

    let error = start
        .await
        .expect("benchmark task")
        .expect_err("rediscovery must cancel the active benchmark");
    assert_eq!(error.to_string(), "benchmark_cancelled");
    assert_eq!(models, vec!["external-model"]);
    wait_for_path(&benchmark_dropped).await;
    wait_for_path(&provider_stopped).await;
}

#[tokio::test]
async fn app_exit_cleanup_reaps_a_temporary_managed_provider() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let marker = temporary.path().join("provider-stopped");
    let port = unused_port();
    let endpoint = format!("http://127.0.0.1:{port}");
    let state = RuntimeSetupState::default();
    let launch = ManagedProviderLaunch {
        executable: python_executable().display().to_string(),
        argv: vec![
            "-c".to_owned(),
            managed_provider_fixture().to_owned(),
            port.to_string(),
            marker.display().to_string(),
        ],
    };

    discover_local_models_at(&endpoint, Some(launch), &state)
        .await
        .expect("managed provider discovery");
    state.shutdown().await.expect("explicit setup cleanup");

    timeout(Duration::from_secs(5), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("app exit must reap the temporary provider");
}

#[test]
fn bundled_paths_are_resolved_from_an_injected_root_and_missing_gateway_fails_closed() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let root = temporary.path().join("resources");
    let app_data = temporary.path().join("app-data");
    fs::create_dir_all(&root).unwrap();

    let error = resolve_runtime_paths(&root, &app_data).expect_err("missing bundle must fail");
    assert_eq!(error.to_string(), "bundled_runtime_missing");

    let gateway = root.join("conversation-runtime-gateway");
    fs::write(&gateway, "fixture gateway").unwrap();
    let paths = resolve_runtime_paths(&root, &app_data).expect("resolved paths");
    assert_eq!(paths.gateway_path, gateway);
    assert_eq!(paths.config_path, app_data.join("runtime.toml"));
}

struct FakeServer {
    endpoint: String,
    _task: tokio::task::JoinHandle<()>,
}

struct CountingServer {
    endpoint: String,
    request_count: Arc<AtomicUsize>,
    _task: tokio::task::JoinHandle<()>,
}

async fn counting_server() -> CountingServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let request_count = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&request_count);
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            counted.fetch_add(1, Ordering::SeqCst);
            let mut request = vec![0; 8192];
            let _ = stream.read(&mut request).await;
            let _ = stream
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\n\r\n")
                .await;
        }
    });
    CountingServer {
        endpoint,
        request_count,
        _task: task,
    }
}

async fn fake_server<T>(responses: Vec<T>) -> FakeServer
where
    T: Into<String> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        for response in responses {
            let response = response.into();
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    FakeServer {
        endpoint,
        _task: task,
    }
}

fn http_ok(body: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}")
}

async fn wait_for_path(path: &std::path::Path) {
    timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fixture signal");
}

fn python_executable() -> PathBuf {
    [
        "/usr/bin/python3",
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
    .expect("Python is required for the provider lifecycle fixture")
}

fn unused_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn managed_provider_fixture() -> &'static str {
    r#"
import http.server
import signal
import sys

port, marker = int(sys.argv[1]), sys.argv[2]
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'{"models":[{"name":"fixture-local-model"}]}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, format, *args):
        pass
def stop(signum, frame):
    open(marker, "w", encoding="utf-8").write("stopped")
    raise SystemExit(0)
signal.signal(signal.SIGTERM, stop)
http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#
}

fn managed_provider_fixture_with_discovery_gate() -> &'static str {
    r#"
import http.server
import os
import signal
import sys
import time

port, marker, discovery_started, release = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
request_count = 0
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        global request_count
        request_count += 1
        if request_count == 2:
            open(discovery_started, "w", encoding="utf-8").write("started")
            while not os.path.exists(release):
                time.sleep(0.01)
        body = b'{"models":[{"name":"fixture-local-model"}]}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, format, *args):
        pass
def stop(signum, frame):
    open(marker, "w", encoding="utf-8").write("stopped")
    raise SystemExit(0)
signal.signal(signal.SIGTERM, stop)
http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#
}

fn managed_benchmark_provider_launch(
    port: u16,
    provider_stopped: &std::path::Path,
    benchmark_started: &std::path::Path,
    benchmark_dropped: &std::path::Path,
) -> ManagedProviderLaunch {
    ManagedProviderLaunch {
        executable: python_executable().display().to_string(),
        argv: vec![
            "-c".to_owned(),
            managed_benchmark_provider_fixture().to_owned(),
            port.to_string(),
            provider_stopped.display().to_string(),
            benchmark_started.display().to_string(),
            benchmark_dropped.display().to_string(),
        ],
    }
}

fn start_benchmark(
    state: &Arc<RuntimeSetupState>,
    endpoint: &str,
    request_id: &str,
) -> tokio::task::JoinHandle<
    Result<LocalModelLatencyResponse, conversation_desktop::runtime_setup::RuntimeSetupError>,
> {
    let state = Arc::clone(state);
    let endpoint = endpoint.to_owned();
    let request_id = request_id.to_owned();
    tokio::spawn(async move {
        check_local_model_latency_request(
            LocalModelLatencyRequest::Start {
                request_id,
                endpoint,
                model: "fixture-local-model".to_owned(),
            },
            &state,
        )
        .await
    })
}

fn managed_benchmark_provider_fixture() -> &'static str {
    r#"
import http.server
import signal
import sys

port, provider_stopped, benchmark_started, benchmark_dropped = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'{"models":[{"name":"fixture-local-model"}]}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        open(benchmark_started, "w", encoding="utf-8").write("started")
        self.send_response(200)
        self.send_header("Content-Type", "application/x-ndjson")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.flush()
        while self.connection.recv(1024):
            pass
        open(benchmark_dropped, "w", encoding="utf-8").write("dropped")
    def log_message(self, format, *args):
        pass
def stop(signum, frame):
    open(provider_stopped, "w", encoding="utf-8").write("stopped")
    raise SystemExit(0)
signal.signal(signal.SIGTERM, stop)
http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#
}
