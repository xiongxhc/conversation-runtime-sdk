//! Capture-process boundary for the opt-in latency sample runner; no device in local tests.
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStdout, Command};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

pub trait CaptureProcess {
    fn stdout(&mut self) -> &mut (dyn AsyncRead + Unpin + Send);
    fn completed(&mut self) -> io::Result<Option<bool>>;
    fn terminate(&mut self) -> io::Result<()>;
    async fn reap(&mut self) -> io::Result<bool>;
}

pub trait CaptureFactory {
    type Process: CaptureProcess;
    fn start(&mut self) -> Result<Self::Process, &'static str>;
}

pub struct OperatorCapture(pub PathBuf);
pub struct RealCapture {
    child: Child,
    stdout: ChildStdout,
}

pub fn capture_command(path: &Path) -> Result<Command, &'static str> {
    if !path.is_absolute() {
        return Err("capture executable must be absolute");
    }
    let mut command = Command::new(path);
    command
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    Ok(command)
}

impl CaptureFactory for OperatorCapture {
    type Process = RealCapture;
    fn start(&mut self) -> Result<RealCapture, &'static str> {
        let mut child = capture_command(&self.0)?
            .spawn()
            .map_err(|_| "capture did not start")?;
        let stdout = child
            .stdout
            .take()
            .expect("configured piped capture stdout");
        Ok(RealCapture { child, stdout })
    }
}

impl CaptureProcess for RealCapture {
    fn stdout(&mut self) -> &mut (dyn AsyncRead + Unpin + Send) {
        &mut self.stdout
    }
    fn completed(&mut self) -> io::Result<Option<bool>> {
        self.child
            .try_wait()
            .map(|status| status.map(|status| status.success()))
    }
    fn terminate(&mut self) -> io::Result<()> {
        self.child.start_kill()
    }
    async fn reap(&mut self) -> io::Result<bool> {
        self.child.wait().await.map(|status| status.success())
    }
}

pub async fn collect_pcm(
    process: &mut impl CaptureProcess,
    limit: Duration,
) -> Result<Vec<u8>, &'static str> {
    collect_pcm_until_cancelled(process, limit, &CancellationToken::new()).await
}

pub async fn collect_pcm_until_cancelled(
    process: &mut impl CaptureProcess,
    limit: Duration,
    cancelled: &CancellationToken,
) -> Result<Vec<u8>, &'static str> {
    let read = timeout(limit, async {
        let mut pcm = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            let count = process
                .stdout()
                .read(&mut buffer)
                .await
                .map_err(|_| "capture output read failed")?;
            if count == 0 {
                break;
            }
            if pcm.len() + count > super::MAX_CAPTURE_PCM_BYTES {
                return Err("capture exceeded byte bound");
            }
            pcm.extend_from_slice(&buffer[..count]);
        }
        Ok(pcm)
    });
    // Cancellation drops only the stdout read, never the cleanup which follows it.
    let capture = tokio::select! {
        biased;
        _ = cancelled.cancelled() => Err("capture cancelled"),
        result = read => result.unwrap_or(Err("capture timed out")),
    };
    // EOF and process exit are separate states. Never wait on a still-live helper after EOF.
    let completed = process.completed().map_err(|_| "capture status failed");
    if !matches!(completed, Ok(Some(_))) {
        let _ = process.terminate();
    }
    let success = timeout(CLEANUP_TIMEOUT, process.reap())
        .await
        .map_err(|_| "capture reap timed out")?
        .map_err(|_| "capture reap failed")?;
    let pcm = capture?;
    if completed?.is_none() {
        return Err("capture closed stdout before exiting");
    }
    if !success {
        return Err("capture exited unsuccessfully");
    }
    if pcm.is_empty() || !pcm.len().is_multiple_of(2) {
        return Err("capture must be non-empty signed-16 PCM");
    }
    Ok(pcm)
}

#[derive(Default)]
pub struct State {
    pub complete: Option<bool>,
    pub terminated: usize,
    pub reaped: usize,
    pub reading: Arc<tokio::sync::Notify>,
}
pub struct FakeCapture {
    pub output: Box<dyn AsyncRead + Unpin + Send>,
    pub state: Arc<Mutex<State>>,
}
impl FakeCapture {
    pub fn new(pcm: Vec<u8>, complete: Option<bool>) -> Self {
        Self {
            output: Box::new(std::io::Cursor::new(pcm)),
            state: Arc::new(Mutex::new(State {
                complete,
                ..State::default()
            })),
        }
    }
}
impl CaptureProcess for FakeCapture {
    fn stdout(&mut self) -> &mut (dyn AsyncRead + Unpin + Send) {
        self.state.lock().unwrap().reading.notify_one();
        &mut *self.output
    }
    fn completed(&mut self) -> io::Result<Option<bool>> {
        Ok(self.state.lock().unwrap().complete)
    }
    fn terminate(&mut self) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        state.terminated += 1;
        state.complete = Some(false);
        Ok(())
    }
    async fn reap(&mut self) -> io::Result<bool> {
        let complete = self.state.lock().unwrap().complete;
        if let Some(success) = complete {
            self.state.lock().unwrap().reaped += 1;
            Ok(success)
        } else {
            std::future::pending().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Omitting terminate at stdout EOF leaves a live process and never completes reap.
    #[tokio::test(start_paused = true)]
    async fn capture_eof_with_live_state_is_terminated_and_reaped_once() {
        let mut capture = FakeCapture::new(vec![0, 1], None);
        let state = capture.state.clone();
        let result = collect_pcm(&mut capture, Duration::from_secs(1)).await;
        assert!(result.is_err());
        let state = state.lock().unwrap();
        assert_eq!(
            state.terminated, 1,
            "EOF must terminate the still-live capture"
        );
        assert_eq!(
            state.reaped, 1,
            "terminated capture must be reaped exactly once"
        );
        assert!(state.complete.is_some());
    }

    // Returning PCM for any failed capture, or skipping its cleanup, must fail this table.
    #[tokio::test(start_paused = true)]
    async fn capture_failures_are_closed_and_reaped() {
        let (pending_output, _keep_stdout_open) = tokio::io::duplex(1);
        let mut pending = FakeCapture::new(vec![], None);
        pending.output = Box::new(pending_output);
        for (mut capture, kills) in [
            (pending, 1),
            (
                FakeCapture::new(vec![0; super::super::MAX_CAPTURE_PCM_BYTES + 2], None),
                1,
            ),
            (FakeCapture::new(vec![0], Some(true)), 0),
            (FakeCapture::new(vec![], Some(true)), 0),
            (FakeCapture::new(vec![0, 1], Some(false)), 0),
        ] {
            let result = collect_pcm(&mut capture, Duration::from_secs(1)).await;
            assert!(result.is_err(), "invalid capture must not return PCM");
            let state = capture.state.lock().unwrap();
            assert_eq!(state.terminated, kills);
            assert_eq!(state.reaped, 1);
            assert!(state.complete.is_some());
        }
    }

    #[tokio::test]
    async fn completed_capture_preserves_pcm_without_termination() {
        let mut capture = FakeCapture::new(vec![0, 1, 2, 3], Some(true));
        assert_eq!(
            collect_pcm(&mut capture, Duration::from_secs(1))
                .await
                .unwrap(),
            [0, 1, 2, 3]
        );
        let state = capture.state.lock().unwrap();
        assert_eq!((state.terminated, state.reaped), (0, 1));
    }

    // Removing env_clear or adding inherited credential-bearing env must fail without a spawn.
    #[test]
    fn real_capture_command_clears_ambient_environment() {
        let command = capture_command(Path::new("/private/capture-pcm")).unwrap();
        assert!(format!("{:?}", command.as_std()).starts_with("env -i "));
        let environment: Vec<_> = command.as_std().get_envs().collect();
        assert_eq!(
            environment,
            [(
                std::ffi::OsStr::new("LC_ALL"),
                Some(std::ffi::OsStr::new("C"))
            )]
        );
        assert_eq!(command.as_std().get_program(), "/private/capture-pcm");
        assert_eq!(command.as_std().get_args().count(), 0);
        assert!(capture_command(Path::new("capture-pcm")).is_err());
    }
}
