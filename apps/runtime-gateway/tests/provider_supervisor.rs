#![cfg(unix)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use conversation_runtime_gateway::{
    FrameReader, GatewayDeploymentConfig, LanguageDeployment, ProviderEnvironmentPolicy,
    ProviderHost, ProviderSupervisor,
};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const PRIVATE_OUTPUT: &str = "private-provider-output";

const PROVIDER_FIXTURE: &str = r#"
import os, signal, socket, sys, time
mode, port_text, marker_dir, literal, delay_text, flood_text = sys.argv[1:]
port, delay_ms, flood_bytes = int(port_text), int(delay_text), int(flood_text)
active = None
def mark(name, value):
    with open(os.path.join(marker_dir, name), "w", encoding="utf-8") as file:
        file.write(value)
mark("pid", str(os.getpid()))
mark("observation", "path=" + str("PATH" in os.environ) + "\nstdin=" + str(os.read(0, 1) == b"") + "\nliteral=" + literal)
remaining = flood_bytes
chunk = b"x" * 65536
while remaining:
    part = chunk[:min(remaining, len(chunk))]
    os.write(1, part)
    os.write(2, part)
    remaining -= len(part)
if mode == "exit-before":
    os.write(1, b"private-provider-output\n")
    os.write(2, b"private-provider-output\n")
    sys.exit(17)
def stopped():
    if active is None:
        return True
    try:
        active.setblocking(False)
        return active.recv(1, socket.MSG_PEEK) == b""
    except BlockingIOError:
        return False
    except OSError:
        return True
def terminate(signum, frame):
    mark("term", "received")
    if mode == "gateway-runtime":
        mark("order", "runtime-before-provider" if stopped() else "provider-before-runtime")
    sys.exit(0)
signal.signal(signal.SIGTERM, signal.SIG_IGN if mode == "ignore-term" else terminate)
time.sleep(delay_ms / 1000)
server = socket.socket()
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(("127.0.0.1", port))
server.listen()
mark("listening", "ready")
while True:
    connection, _ = server.accept()
    request = b""
    while b"\r\n\r\n" not in request:
        data = connection.recv(4096)
        if not data:
            break
        request += data
    header, _, body = request.partition(b"\r\n\r\n")
    length = 0
    for line in header.split(b"\r\n"):
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":", 1)[1].strip())
    while len(body) < length:
        body += connection.recv(4096)
    is_post = header.startswith(b"POST ")
    if mode == "gateway-runtime" and is_post:
        active = connection
        mark("runtime-active", "active")
        connection.sendall(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n")
        connection.sendall(b'{"message":{"role":"assistant","content":"fixture-partial"},"done":false}\n')
        while connection.recv(4096):
            pass
        mark("runtime-stopped", "stopped")
        active = None
        connection.close()
        continue
    if mode == "slow-ready":
        time.sleep(0.3)
    connection.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    connection.close()
    if mode == "exit-after":
        time.sleep(0.15)
        sys.exit(23)
"#;

#[tokio::test]
async fn direct_spawn_preserves_literal_argv_environment_policy_null_stdin_and_readiness() {
    let temporary = tempfile::tempdir().unwrap();
    let injection = temporary.path().join("shell-injection");
    let literal = format!("$(touch {})", injection.display());

    let clear_dir = temporary.path().join("clear");
    std::fs::create_dir(&clear_dir).unwrap();
    let clear = fixture_host(
        "clear",
        "persistent",
        unused_port(),
        &clear_dir,
        &literal,
        120,
        0,
        ProviderEnvironmentPolicy::Clear,
    );
    let started = Instant::now();
    let clear_supervisor = ProviderSupervisor::start(vec![clear], CancellationToken::new())
        .await
        .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(100));
    let clear_observation = tokio::fs::read_to_string(clear_dir.join("observation"))
        .await
        .unwrap();
    assert!(clear_observation.contains("path=False"));
    assert!(clear_observation.contains("stdin=True"));
    assert!(clear_observation.contains(&format!("literal={literal}")));
    assert!(!injection.exists());
    clear_supervisor.shutdown().await.unwrap();

    let inherit_dir = temporary.path().join("inherit");
    std::fs::create_dir(&inherit_dir).unwrap();
    let inherit = fixture_host(
        "inherit",
        "persistent",
        unused_port(),
        &inherit_dir,
        "literal",
        0,
        0,
        ProviderEnvironmentPolicy::Inherit,
    );
    let inherit_supervisor = ProviderSupervisor::start(vec![inherit], CancellationToken::new())
        .await
        .unwrap();
    let inherit_observation = tokio::fs::read_to_string(inherit_dir.join("observation"))
        .await
        .unwrap();
    assert!(inherit_observation.contains("path=True"));
    inherit_supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn child_exit_before_readiness_is_reaped_and_content_free() {
    let temporary = tempfile::tempdir().unwrap();
    let host = fixture_host(
        "pre-ready",
        "exit-before",
        unused_port(),
        temporary.path(),
        "literal",
        0,
        0,
        ProviderEnvironmentPolicy::Clear,
    );
    let error = ProviderSupervisor::start(vec![host], CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.diagnostic_code(), "provider_exited_before_ready");
    assert!(!error.to_string().contains(PRIVATE_OUTPUT));
    support::assert_process_reaped(read_pid(temporary.path()).await).await;
}

#[tokio::test]
async fn child_exit_after_readiness_is_monitored_without_restart() {
    let temporary = tempfile::tempdir().unwrap();
    let host = fixture_host(
        "post-ready",
        "exit-after",
        unused_port(),
        temporary.path(),
        "literal",
        0,
        0,
        ProviderEnvironmentPolicy::Clear,
    );
    let mut supervisor = ProviderSupervisor::start(vec![host], CancellationToken::new())
        .await
        .unwrap();
    let pid = read_pid(temporary.path()).await;
    let error = timeout(support::PROCESS_TIMEOUT, supervisor.wait_for_exit())
        .await
        .unwrap();
    assert_eq!(error.diagnostic_code(), "provider_exited_after_ready");
    support::assert_process_reaped(pid).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(read_pid(temporary.path()).await, pid);
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_timeout_reaps_the_owned_child() {
    let temporary = tempfile::tempdir().unwrap();
    let port = unused_port();
    let host = fixture_host_with_timeout(
        "timeout",
        "persistent",
        port,
        temporary.path(),
        "literal",
        1_000,
        0,
        ProviderEnvironmentPolicy::Clear,
        150,
    );
    let error = ProviderSupervisor::start(vec![host], CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.diagnostic_code(), "provider_startup_timeout");
    support::assert_process_reaped(read_pid(temporary.path()).await).await;
}

#[tokio::test]
async fn startup_cancellation_reaps_the_owned_child() {
    let temporary = tempfile::tempdir().unwrap();
    let host = fixture_host(
        "cancelled",
        "persistent",
        unused_port(),
        temporary.path(),
        "literal",
        1_000,
        0,
        ProviderEnvironmentPolicy::Clear,
    );
    let cancellation = CancellationToken::new();
    let start = tokio::spawn(ProviderSupervisor::start(vec![host], cancellation.clone()));
    support::wait_for_path(&temporary.path().join("pid")).await;
    let pid = read_pid(temporary.path()).await;
    cancellation.cancel();
    let error = start.await.unwrap().unwrap_err();
    assert_eq!(error.diagnostic_code(), "provider_startup_cancelled");
    support::assert_process_reaped(pid).await;
}

#[tokio::test]
async fn stdout_and_stderr_are_continuously_drained_without_blocking_readiness() {
    let temporary = tempfile::tempdir().unwrap();
    let host = fixture_host(
        "output",
        "persistent",
        unused_port(),
        temporary.path(),
        "literal",
        0,
        2 * 1024 * 1024,
        ProviderEnvironmentPolicy::Clear,
    );
    let supervisor = timeout(
        support::PROCESS_TIMEOUT,
        ProviderSupervisor::start(vec![host], CancellationToken::new()),
    )
    .await
    .expect("provider output filled an undrained pipe")
    .unwrap();
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_terminates_gracefully_before_waiting() {
    let temporary = tempfile::tempdir().unwrap();
    let host = fixture_host(
        "graceful",
        "persistent",
        unused_port(),
        temporary.path(),
        "literal",
        0,
        0,
        ProviderEnvironmentPolicy::Clear,
    );
    let supervisor = ProviderSupervisor::start(vec![host], CancellationToken::new())
        .await
        .unwrap();
    let pid = read_pid(temporary.path()).await;
    supervisor.shutdown().await.unwrap();
    assert_eq!(
        tokio::fs::read_to_string(temporary.path().join("term"))
            .await
            .unwrap(),
        "received"
    );
    support::assert_process_reaped(pid).await;
}

#[tokio::test]
async fn slow_readiness_responses_still_count_as_ready() {
    let temporary = tempfile::tempdir().unwrap();
    let host = fixture_host_with_timeout(
        "slow",
        "slow-ready",
        unused_port(),
        temporary.path(),
        "literal",
        0,
        0,
        ProviderEnvironmentPolicy::Clear,
        3_000,
    );
    // A readiness endpoint that answers in 300 ms is healthy; only the
    // configured startup timeout bounds how long readiness may take.
    let supervisor = ProviderSupervisor::start(vec![host], CancellationToken::new())
        .await
        .expect("a slow but healthy readiness endpoint counts as ready");
    supervisor.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_terminates_providers_launched_through_a_wrapper() {
    let temporary = tempfile::tempdir().unwrap();
    let port = unused_port();
    // The server is a grandchild: the wrapper shell stays the direct child and
    // does not forward signals.
    let mut argv = vec![
        "-c".to_owned(),
        "\"$0\" \"$@\" & wait".to_owned(),
        python_executable().display().to_string(),
    ];
    argv.extend(fixture_argv(
        "persistent",
        port,
        temporary.path(),
        "literal",
        0,
        0,
    ));
    let host = ProviderHost::gateway_owned(
        "wrapped",
        readiness_url(port),
        2_000,
        ProviderEnvironmentPolicy::Clear,
        PathBuf::from("/bin/sh"),
        argv,
    )
    .unwrap();
    let supervisor = ProviderSupervisor::start(vec![host], CancellationToken::new())
        .await
        .unwrap();
    let server_pid = read_pid(temporary.path()).await;
    supervisor.shutdown().await.unwrap();
    support::assert_process_reaped(server_pid).await;
    assert_eq!(
        tokio::fs::read_to_string(temporary.path().join("term"))
            .await
            .unwrap(),
        "received"
    );
}

#[tokio::test]
async fn shutdown_escalates_to_bounded_kill_and_wait() {
    let temporary = tempfile::tempdir().unwrap();
    let host = fixture_host(
        "kill",
        "ignore-term",
        unused_port(),
        temporary.path(),
        "literal",
        0,
        0,
        ProviderEnvironmentPolicy::Clear,
    );
    let supervisor = ProviderSupervisor::start(vec![host], CancellationToken::new())
        .await
        .unwrap();
    let pid = read_pid(temporary.path()).await;
    let started = Instant::now();
    supervisor.shutdown().await.unwrap();
    assert!(started.elapsed() < support::PROCESS_TIMEOUT);
    assert!(!temporary.path().join("term").exists());
    support::assert_process_reaped(pid).await;
}

#[tokio::test]
async fn external_hosts_are_observed_but_never_terminated() {
    let temporary = tempfile::tempdir().unwrap();
    let port = unused_port();
    let mut external = spawn_fixture("persistent", port, temporary.path(), "literal", 0, 0, true);
    let host = ProviderHost::external(
        "external",
        readiness_url(port),
        1_000,
        ProviderEnvironmentPolicy::Clear,
    )
    .unwrap();
    let supervisor = ProviderSupervisor::start(vec![host], CancellationToken::new())
        .await
        .unwrap();
    supervisor.shutdown().await.unwrap();
    assert!(external.try_wait().unwrap().is_none());
    external.kill().await.unwrap();
    external.wait().await.unwrap();
}

#[tokio::test]
async fn partial_multi_host_startup_failure_cleans_up_ready_owned_children() {
    let temporary = tempfile::tempdir().unwrap();
    let first_dir = temporary.path().join("first");
    let second_dir = temporary.path().join("second");
    std::fs::create_dir(&first_dir).unwrap();
    std::fs::create_dir(&second_dir).unwrap();
    let first = fixture_host(
        "first",
        "persistent",
        unused_port(),
        &first_dir,
        "literal",
        0,
        0,
        ProviderEnvironmentPolicy::Clear,
    );
    let second = fixture_host(
        "second",
        "exit-before",
        unused_port(),
        &second_dir,
        "literal",
        0,
        0,
        ProviderEnvironmentPolicy::Clear,
    );
    let error = ProviderSupervisor::start(vec![first, second], CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.diagnostic_code(), "provider_exited_before_ready");
    support::assert_process_reaped(read_pid(&first_dir).await).await;
    support::assert_process_reaped(read_pid(&second_dir).await).await;
}

#[tokio::test]
async fn compiled_gateway_fails_when_an_owned_provider_exits_after_readiness() {
    let temporary = tempfile::tempdir().unwrap();
    let provider_dir = temporary.path().join("provider");
    std::fs::create_dir(&provider_dir).unwrap();
    let port = unused_port();
    let host = fixture_host(
        "gateway-provider",
        "exit-after",
        port,
        &provider_dir,
        "literal",
        0,
        0,
        ProviderEnvironmentPolicy::Clear,
    );
    let config = GatewayDeploymentConfig::builder(LanguageDeployment::ollama_compatible(
        "fixture-provider",
        format!("http://127.0.0.1:{port}"),
        "private-fixture-model",
        "gateway-provider",
    ))
    .provider_host(host)
    .to_toml()
    .unwrap();
    let config_path = temporary.path().join("gateway.toml");
    tokio::fs::write(&config_path, config).await.unwrap();

    let mut gateway = support::gateway_command(&config_path).spawn().unwrap();
    let _stdin = gateway.stdin.take().unwrap();
    let stdout = gateway.stdout.take().unwrap();
    let stderr = gateway.stderr.take().unwrap();
    let stderr_task = tokio::spawn(async move {
        let mut stderr = BufReader::new(stderr);
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut bytes)
            .await
            .unwrap();
        bytes
    });
    let mut frames = FrameReader::new(BufReader::new(stdout));
    let ready = timeout(support::PROCESS_TIMEOUT, frames.read_frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8(ready)
        .unwrap()
        .contains(r#""type":"ready""#));
    // The client learns why the session ended from a fatal frame, not only from
    // the process exit code and stderr.
    let fatal = timeout(support::PROCESS_TIMEOUT, frames.read_frame())
        .await
        .unwrap()
        .unwrap()
        .expect("a fatal frame precedes the output closing");
    let fatal = String::from_utf8(fatal).unwrap();
    assert!(fatal.contains(r#""type":"fatal""#), "{fatal}");
    assert!(fatal.contains(r#""code":"adapter_failure""#), "{fatal}");
    assert!(!fatal.contains("private-fixture-model"), "{fatal}");
    assert!(timeout(support::PROCESS_TIMEOUT, frames.read_frame())
        .await
        .unwrap()
        .unwrap()
        .is_none());

    let status = match timeout(support::PROCESS_TIMEOUT, gateway.wait()).await {
        Ok(status) => status.unwrap(),
        Err(_) => {
            gateway.kill().await.unwrap();
            gateway.wait().await.unwrap();
            let stderr = String::from_utf8(stderr_task.await.unwrap()).unwrap();
            panic!("gateway did not exit; stderr: {stderr}");
        }
    };
    assert!(!status.success());
    assert_eq!(
        String::from_utf8(stderr_task.await.unwrap())
            .unwrap()
            .trim(),
        "gateway provider supervision failed: provider_exited_after_ready"
    );
    support::assert_process_reaped(read_pid(&provider_dir).await).await;
}

#[tokio::test]
async fn compiled_gateway_exits_cleanly_when_the_client_leaves_during_provider_startup() {
    let temporary = tempfile::tempdir().unwrap();
    let provider_dir = temporary.path().join("provider");
    std::fs::create_dir(&provider_dir).unwrap();
    let port = unused_port();
    let host = fixture_host_with_timeout(
        "gateway-provider",
        "serve",
        port,
        &provider_dir,
        "literal",
        1_500,
        0,
        ProviderEnvironmentPolicy::Clear,
        10_000,
    );
    let config = GatewayDeploymentConfig::builder(LanguageDeployment::ollama_compatible(
        "fixture-provider",
        format!("http://127.0.0.1:{port}"),
        "private-fixture-model",
        "gateway-provider",
    ))
    .provider_host(host)
    .to_toml()
    .unwrap();
    let config_path = temporary.path().join("gateway.toml");
    tokio::fs::write(&config_path, config).await.unwrap();

    let started = Instant::now();
    let mut gateway = support::gateway_command(&config_path).spawn().unwrap();
    let stderr = gateway.stderr.take().unwrap();
    let stderr_task = tokio::spawn(async move {
        let mut stderr = BufReader::new(stderr);
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut bytes)
            .await
            .unwrap();
        bytes
    });
    let provider_pid = read_pid(&provider_dir).await;
    // The client goes away while the provider is still becoming ready.
    drop(gateway.stdin.take());

    let status = match timeout(support::PROCESS_TIMEOUT, gateway.wait()).await {
        Ok(status) => status.unwrap(),
        Err(_) => {
            gateway.kill().await.unwrap();
            gateway.wait().await.unwrap();
            let stderr = String::from_utf8(stderr_task.await.unwrap()).unwrap();
            panic!("gateway did not exit; stderr: {stderr}");
        }
    };
    // Nothing failed: the provider was abandoned because nobody was left to use it.
    assert!(status.success(), "exit status: {status:?}");
    assert!(
        started.elapsed() < Duration::from_millis(1_400),
        "the gateway waited for readiness nobody needed: {:?}",
        started.elapsed()
    );
    assert_eq!(String::from_utf8(stderr_task.await.unwrap()).unwrap(), "");
    support::assert_process_reaped(provider_pid).await;
}

#[tokio::test]
async fn compiled_gateway_stops_runtime_work_before_owned_providers() {
    let temporary = tempfile::tempdir().unwrap();
    let provider_dir = temporary.path().join("provider");
    std::fs::create_dir(&provider_dir).unwrap();
    let port = unused_port();
    let host = fixture_host(
        "gateway-provider",
        "gateway-runtime",
        port,
        &provider_dir,
        "literal",
        0,
        0,
        ProviderEnvironmentPolicy::Clear,
    );
    let config = GatewayDeploymentConfig::builder(LanguageDeployment::ollama_compatible(
        "fixture-provider",
        format!("http://127.0.0.1:{port}"),
        "private-fixture-model",
        "gateway-provider",
    ))
    .provider_host(host)
    .to_toml()
    .unwrap();
    let config_path = temporary.path().join("gateway.toml");
    tokio::fs::write(&config_path, config).await.unwrap();

    let mut gateway = support::gateway_command(&config_path).spawn().unwrap();
    let mut stdin = gateway.stdin.take().unwrap();
    let stdout = gateway.stdout.take().unwrap();
    let mut frames = FrameReader::new(BufReader::new(stdout));
    let stderr = gateway.stderr.take().unwrap();
    let stderr_task = tokio::spawn(async move {
        let mut stderr = BufReader::new(stderr);
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut bytes)
            .await
            .unwrap();
        bytes
    });

    let ready = timeout(support::PROCESS_TIMEOUT, frames.read_frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(String::from_utf8(ready)
        .unwrap()
        .contains(r#""type":"ready""#));
    write_frame(
        &mut stdin,
        r#"{"protocol_version":1,"type":"start_turn","request_id":"start","transcript":"private-runtime-content"}"#,
    )
    .await;
    support::wait_for_path(&provider_dir.join("runtime-active")).await;
    drop(stdin);

    let status = timeout(support::PROCESS_TIMEOUT, gateway.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
    let provider_pid = read_pid(&provider_dir).await;
    support::assert_process_reaped(provider_pid).await;
    assert_eq!(
        tokio::fs::read_to_string(provider_dir.join("order"))
            .await
            .unwrap(),
        "runtime-before-provider"
    );
    let stderr = String::from_utf8(stderr_task.await.unwrap()).unwrap();
    assert!(!stderr.contains("private-runtime-content"));
    assert!(!stderr.contains("private-fixture-model"));
    assert!(!stderr.contains("fixture-partial"));
    assert!(!stderr.contains(&provider_dir.display().to_string()));
}

#[allow(clippy::too_many_arguments)]
fn fixture_host(
    id: &str,
    mode: &str,
    port: u16,
    marker_dir: &Path,
    literal: &str,
    delay_ms: u64,
    flood_bytes: usize,
    environment: ProviderEnvironmentPolicy,
) -> ProviderHost {
    fixture_host_with_timeout(
        id,
        mode,
        port,
        marker_dir,
        literal,
        delay_ms,
        flood_bytes,
        environment,
        2_000,
    )
}

#[allow(clippy::too_many_arguments)]
fn fixture_host_with_timeout(
    id: &str,
    mode: &str,
    port: u16,
    marker_dir: &Path,
    literal: &str,
    delay_ms: u64,
    flood_bytes: usize,
    environment: ProviderEnvironmentPolicy,
    startup_timeout_ms: u64,
) -> ProviderHost {
    ProviderHost::gateway_owned(
        id,
        readiness_url(port),
        startup_timeout_ms,
        environment,
        python_executable(),
        fixture_argv(mode, port, marker_dir, literal, delay_ms, flood_bytes),
    )
    .unwrap()
}

fn spawn_fixture(
    mode: &str,
    port: u16,
    marker_dir: &Path,
    literal: &str,
    delay_ms: u64,
    flood_bytes: usize,
    clear_environment: bool,
) -> Child {
    let mut command = Command::new(python_executable());
    command
        .args(fixture_argv(
            mode,
            port,
            marker_dir,
            literal,
            delay_ms,
            flood_bytes,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if clear_environment {
        command.env_clear();
    }
    command.spawn().unwrap()
}

fn fixture_argv(
    mode: &str,
    port: u16,
    marker_dir: &Path,
    literal: &str,
    delay_ms: u64,
    flood_bytes: usize,
) -> Vec<String> {
    vec![
        "-c".to_owned(),
        PROVIDER_FIXTURE.to_owned(),
        mode.to_owned(),
        port.to_string(),
        marker_dir.display().to_string(),
        literal.to_owned(),
        delay_ms.to_string(),
        flood_bytes.to_string(),
    ]
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
    .expect("a real Python executable is required for provider process fixtures")
}

fn unused_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn readiness_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/ready")
}

async fn write_frame(stdin: &mut tokio::process::ChildStdin, payload: &str) {
    stdin
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .unwrap();
    stdin.write_all(payload.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
}

async fn read_pid(marker_dir: &Path) -> u32 {
    support::wait_for_path(&marker_dir.join("pid")).await;
    tokio::fs::read_to_string(marker_dir.join("pid"))
        .await
        .unwrap()
        .parse()
        .unwrap()
}
