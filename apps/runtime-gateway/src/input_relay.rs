use std::io::{self, Read};

use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const RELAY_BYTES: usize = 64 * 1024;
const CHUNK_BYTES: usize = 8 * 1024;
const CHUNK_COUNT: usize = RELAY_BYTES / CHUNK_BYTES;

/// Client input relayed from a blocking reader onto an async stream.
pub struct InputRelay {
    /// The session's input; it reaches end of stream once the client input ends.
    pub input: DuplexStream,
    /// Cancelled once the relay stops: the client input ended, failed, or the
    /// relay was stopped.
    pub ended: CancellationToken,
    /// Completes when the relay stops; `Err` carries a read failure other than EOF.
    pub task: JoinHandle<io::Result<()>>,
}

/// Relays `reader` onto an async stream until it ends, fails, or `stop` is
/// cancelled. Interrupted reads are retried; other read failures end the input
/// and are reported through the task result.
pub fn input_relay<R>(mut reader: R, stop: CancellationToken) -> InputRelay
where
    R: Read + Send + 'static,
{
    let (sender, mut receiver) = mpsc::channel::<io::Result<Vec<u8>>>(CHUNK_COUNT);
    std::thread::spawn(move || {
        let mut buffer = [0_u8; CHUNK_BYTES];
        loop {
            let outcome = match reader.read(&mut buffer) {
                Ok(0) => return,
                Ok(count) => Ok(buffer[..count].to_vec()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => Err(error),
            };
            let failed = outcome.is_err();
            if sender.blocking_send(outcome).is_err() || failed {
                return;
            }
        }
    });

    let (input, mut sink) = tokio::io::duplex(RELAY_BYTES);
    let ended = CancellationToken::new();
    let ended_signal = ended.clone();
    let task = tokio::spawn(async move {
        let result = loop {
            let chunk = tokio::select! {
                biased;
                _ = stop.cancelled() => break Ok(()),
                chunk = receiver.recv() => chunk,
            };
            match chunk {
                None => break Ok(()),
                Some(Err(error)) => break Err(error),
                // The consumer went away; that is the session's outcome, not an
                // input failure.
                Some(Ok(chunk)) => {
                    if sink.write_all(&chunk).await.is_err() {
                        break Ok(());
                    }
                }
            }
        };
        drop(sink);
        ended_signal.cancel();
        result
    });
    InputRelay { input, ended, task }
}
