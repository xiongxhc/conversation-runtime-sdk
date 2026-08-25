use std::fmt;
use std::future::pending;
use std::process::Stdio;
use std::time::Duration;

use reqwest::redirect::Policy;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

use crate::{ProviderEnvironmentPolicy, ProviderHost, ProviderHostOwnership};

const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Bounds one readiness probe so a hung endpoint cannot stall polling; the
/// host's startup timeout bounds readiness as a whole.
const READINESS_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
const KILL_WAIT_TIMEOUT: Duration = Duration::from_secs(1);
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
const OUTPUT_BUFFER_BYTES: usize = 8 * 1024;

pub struct ProviderSupervisor {
    shutdown_senders: Vec<oneshot::Sender<()>>,
    tasks: JoinSet<Result<(), ProviderSupervisorError>>,
    exit_receiver: mpsc::UnboundedReceiver<ProviderSupervisorError>,
}

impl fmt::Debug for ProviderSupervisor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderSupervisor")
            .field("owned_provider_count", &self.shutdown_senders.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderSupervisorErrorKind {
    Spawn,
    ExitedBeforeReady,
    StartupTimeout,
    StartupCancelled,
    ExitedAfterReady,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderSupervisorError {
    kind: ProviderSupervisorErrorKind,
}

impl ProviderSupervisorError {
    /// Startup stopped because its cancellation token fired, not because a
    /// provider misbehaved.
    pub const fn is_startup_cancelled(self) -> bool {
        matches!(self.kind, ProviderSupervisorErrorKind::StartupCancelled)
    }

    pub const fn diagnostic_code(self) -> &'static str {
        match self.kind {
            ProviderSupervisorErrorKind::Spawn => "provider_spawn_failed",
            ProviderSupervisorErrorKind::ExitedBeforeReady => "provider_exited_before_ready",
            ProviderSupervisorErrorKind::StartupTimeout => "provider_startup_timeout",
            ProviderSupervisorErrorKind::StartupCancelled => "provider_startup_cancelled",
            ProviderSupervisorErrorKind::ExitedAfterReady => "provider_exited_after_ready",
            ProviderSupervisorErrorKind::Shutdown => "provider_shutdown_failed",
        }
    }
}

impl fmt::Display for ProviderSupervisorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.diagnostic_code())
    }
}

impl std::error::Error for ProviderSupervisorError {}

struct RunningProvider {
    child: Child,
    stdout: JoinHandle<()>,
    stderr: JoinHandle<()>,
}

enum StartupOutcome {
    Ready,
    Exited,
    Failed(ProviderSupervisorError),
}

impl ProviderSupervisor {
    pub async fn start(
        provider_hosts: Vec<ProviderHost>,
        cancellation: CancellationToken,
    ) -> Result<Self, ProviderSupervisorError> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(READINESS_REQUEST_TIMEOUT)
            .build()
            .map_err(|_| supervisor_error(ProviderSupervisorErrorKind::Spawn))?;
        let (exit_sender, exit_receiver) = mpsc::unbounded_channel();
        let mut supervisor = Self {
            shutdown_senders: Vec::new(),
            tasks: JoinSet::new(),
            exit_receiver,
        };

        for host in provider_hosts {
            let result = match host.ownership() {
                ProviderHostOwnership::External => {
                    wait_for_external_readiness(&client, &host, &cancellation).await
                }
                ProviderHostOwnership::GatewayOwned => {
                    match start_owned_provider(&client, &host, &cancellation).await {
                        Ok(provider) => {
                            supervisor.track(provider, exit_sender.clone());
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                }
            };
            if let Err(error) = result {
                let _ = supervisor.shutdown().await;
                return Err(error);
            }
        }
        drop(exit_sender);
        Ok(supervisor)
    }

    pub async fn wait_for_exit(&mut self) -> ProviderSupervisorError {
        match self.exit_receiver.recv().await {
            Some(error) => error,
            None => pending().await,
        }
    }

    pub async fn shutdown(mut self) -> Result<(), ProviderSupervisorError> {
        for sender in self.shutdown_senders.drain(..) {
            let _ = sender.send(());
        }
        let mut result = Ok(());
        while let Some(joined) = self.tasks.join_next().await {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(error)) => result = Err(error),
                Err(_) => result = Err(supervisor_error(ProviderSupervisorErrorKind::Shutdown)),
            }
        }
        result
    }

    fn track(
        &mut self,
        provider: RunningProvider,
        exit_sender: mpsc::UnboundedSender<ProviderSupervisorError>,
    ) {
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        self.shutdown_senders.push(shutdown_sender);
        self.tasks.spawn(monitor_owned_provider(
            provider,
            shutdown_receiver,
            exit_sender,
        ));
    }
}

impl Drop for ProviderSupervisor {
    fn drop(&mut self) {
        for sender in self.shutdown_senders.drain(..) {
            let _ = sender.send(());
        }
        self.tasks.detach_all();
    }
}

async fn start_owned_provider(
    client: &reqwest::Client,
    host: &ProviderHost,
    cancellation: &CancellationToken,
) -> Result<RunningProvider, ProviderSupervisorError> {
    let executable = host
        .executable()
        .ok_or_else(|| supervisor_error(ProviderSupervisorErrorKind::Spawn))?;
    let argv = host
        .argv()
        .ok_or_else(|| supervisor_error(ProviderSupervisorErrorKind::Spawn))?;
    let mut command = Command::new(executable);
    command
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Its own process group, so shutdown reaches servers started through a
    // wrapper (a shell, `uv run`, `npx`) and not only the wrapper itself.
    #[cfg(unix)]
    command.process_group(0);
    if matches!(host.environment(), ProviderEnvironmentPolicy::Clear) {
        command.env_clear();
    }
    let mut child = command
        .spawn()
        .map_err(|_| supervisor_error(ProviderSupervisorErrorKind::Spawn))?;
    let stdout = spawn_output_drain(
        child
            .stdout
            .take()
            .ok_or_else(|| supervisor_error(ProviderSupervisorErrorKind::Spawn))?,
    );
    let stderr = spawn_output_drain(
        child
            .stderr
            .take()
            .ok_or_else(|| supervisor_error(ProviderSupervisorErrorKind::Spawn))?,
    );
    let mut provider = RunningProvider {
        child,
        stdout,
        stderr,
    };
    let readiness = poll_readiness(client, host.readiness_url());
    tokio::pin!(readiness);
    let deadline = sleep(Duration::from_millis(host.startup_timeout_ms()));
    tokio::pin!(deadline);
    let outcome = tokio::select! {
        biased;
        status = provider.child.wait() => {
            let _ = status;
            StartupOutcome::Exited
        }
        _ = cancellation.cancelled() => StartupOutcome::Failed(supervisor_error(ProviderSupervisorErrorKind::StartupCancelled)),
        _ = &mut deadline => StartupOutcome::Failed(supervisor_error(ProviderSupervisorErrorKind::StartupTimeout)),
        () = &mut readiness => {
            match provider.child.try_wait() {
                Ok(None) => StartupOutcome::Ready,
                Ok(Some(_)) | Err(_) => StartupOutcome::Exited,
            }
        }
    };

    match outcome {
        StartupOutcome::Ready => Ok(provider),
        StartupOutcome::Exited => {
            finish_output_drains(&mut provider).await;
            Err(supervisor_error(
                ProviderSupervisorErrorKind::ExitedBeforeReady,
            ))
        }
        StartupOutcome::Failed(error) => {
            let _ = stop_provider(&mut provider).await;
            Err(error)
        }
    }
}

async fn wait_for_external_readiness(
    client: &reqwest::Client,
    host: &ProviderHost,
    cancellation: &CancellationToken,
) -> Result<(), ProviderSupervisorError> {
    let readiness = poll_readiness(client, host.readiness_url());
    tokio::pin!(readiness);
    let deadline = sleep(Duration::from_millis(host.startup_timeout_ms()));
    tokio::pin!(deadline);
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(supervisor_error(ProviderSupervisorErrorKind::StartupCancelled)),
        _ = &mut deadline => Err(supervisor_error(ProviderSupervisorErrorKind::StartupTimeout)),
        () = &mut readiness => Ok(()),
    }
}

async fn poll_readiness(client: &reqwest::Client, readiness_url: &str) {
    loop {
        let ready = client
            .get(readiness_url)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        if ready {
            return;
        }
        sleep(READINESS_POLL_INTERVAL).await;
    }
}

async fn monitor_owned_provider(
    mut provider: RunningProvider,
    shutdown: oneshot::Receiver<()>,
    exit_sender: mpsc::UnboundedSender<ProviderSupervisorError>,
) -> Result<(), ProviderSupervisorError> {
    #[cfg(unix)]
    let pid = provider.child.id();
    tokio::select! {
        result = provider.child.wait() => {
            let _ = result;
            let error = supervisor_error(ProviderSupervisorErrorKind::ExitedAfterReady);
            // Whatever the exited process left behind in its group goes with it.
            #[cfg(unix)]
            if let Some(pid) = pid {
                let _ = signal_group(pid, "-TERM").await;
            }
            finish_output_drains(&mut provider).await;
            let _ = exit_sender.send(error);
            Ok(())
        }
        _ = shutdown => stop_provider(&mut provider).await,
    }
}

async fn stop_provider(provider: &mut RunningProvider) -> Result<(), ProviderSupervisorError> {
    let result = terminate_and_wait(&mut provider.child).await;
    finish_output_drains(provider).await;
    result
}

async fn terminate_and_wait(child: &mut Child) -> Result<(), ProviderSupervisorError> {
    match child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(_) => return Err(supervisor_error(ProviderSupervisorErrorKind::Shutdown)),
    }

    #[cfg(unix)]
    let pid = child.id();
    #[cfg(unix)]
    let graceful = match pid {
        Some(pid) => signal_group(pid, "-TERM").await,
        None => false,
    };
    #[cfg(not(unix))]
    let graceful = false;

    if graceful {
        match timeout(GRACEFUL_SHUTDOWN_TIMEOUT, child.wait()).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(_)) => {
                return Err(supervisor_error(ProviderSupervisorErrorKind::Shutdown));
            }
            Err(_) => {}
        }
    }

    #[cfg(unix)]
    if let Some(pid) = pid {
        let _ = signal_group(pid, "-KILL").await;
    }
    if child.start_kill().is_err() && child.try_wait().ok().flatten().is_none() {
        return Err(supervisor_error(ProviderSupervisorErrorKind::Shutdown));
    }
    match timeout(KILL_WAIT_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(supervisor_error(ProviderSupervisorErrorKind::Shutdown)),
    }
}

/// Signals the process group the gateway created for the provider with `pid`.
/// Goes through `kill(1)` because the workspace forbids the unsafe `kill(2)` call.
#[cfg(unix)]
async fn signal_group(pid: u32, signal: &str) -> bool {
    Command::new("/bin/kill")
        .arg(signal)
        .arg("--")
        .arg(format!("-{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .status()
        .await
        .is_ok_and(|status| status.success())
}

fn spawn_output_drain<R>(mut reader: R) -> JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = [0_u8; OUTPUT_BUFFER_BYTES];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
}

async fn finish_output_drains(provider: &mut RunningProvider) {
    finish_output_drain(&mut provider.stdout).await;
    finish_output_drain(&mut provider.stderr).await;
}

async fn finish_output_drain(drain: &mut JoinHandle<()>) {
    if timeout(OUTPUT_DRAIN_TIMEOUT, &mut *drain).await.is_err() {
        drain.abort();
        let _ = drain.await;
    }
}

const fn supervisor_error(kind: ProviderSupervisorErrorKind) -> ProviderSupervisorError {
    ProviderSupervisorError { kind }
}
