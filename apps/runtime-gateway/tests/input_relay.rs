use std::collections::VecDeque;
use std::io::{self, Read};
use std::time::Duration;

use conversation_runtime_gateway::input_relay;
use tokio::io::AsyncReadExt;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A blocking reader that replays scripted `read` outcomes, then reports EOF.
struct ScriptedReader {
    outcomes: VecDeque<io::Result<Vec<u8>>>,
}

impl ScriptedReader {
    fn new(outcomes: Vec<io::Result<Vec<u8>>>) -> Self {
        Self {
            outcomes: outcomes.into(),
        }
    }
}

impl Read for ScriptedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self.outcomes.pop_front() {
            Some(Ok(bytes)) => {
                buffer[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            Some(Err(error)) => Err(error),
            None => Ok(0),
        }
    }
}

#[tokio::test]
async fn forwards_input_then_closes_the_stream_and_marks_the_input_ended() {
    let reader = ScriptedReader::new(vec![Ok(b"abc".to_vec()), Ok(b"def".to_vec())]);
    let relay = input_relay(reader, CancellationToken::new());
    let mut input = relay.input;

    let mut received = Vec::new();
    timeout(TEST_TIMEOUT, input.read_to_end(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received, b"abcdef");
    timeout(TEST_TIMEOUT, relay.ended.cancelled())
        .await
        .unwrap();
    assert!(timeout(TEST_TIMEOUT, relay.task)
        .await
        .unwrap()
        .unwrap()
        .is_ok());
}

#[tokio::test]
async fn retries_interrupted_reads_instead_of_treating_them_as_eof() {
    let reader = ScriptedReader::new(vec![
        Ok(b"abc".to_vec()),
        Err(io::Error::from(io::ErrorKind::Interrupted)),
        Ok(b"def".to_vec()),
    ]);
    let relay = input_relay(reader, CancellationToken::new());
    let mut input = relay.input;

    let mut received = Vec::new();
    timeout(TEST_TIMEOUT, input.read_to_end(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received, b"abcdef");
    assert!(timeout(TEST_TIMEOUT, relay.task)
        .await
        .unwrap()
        .unwrap()
        .is_ok());
}

#[tokio::test]
async fn reports_read_failures_instead_of_folding_them_into_eof() {
    let reader = ScriptedReader::new(vec![
        Ok(b"abc".to_vec()),
        Err(io::Error::from(io::ErrorKind::BrokenPipe)),
        Ok(b"never".to_vec()),
    ]);
    let relay = input_relay(reader, CancellationToken::new());
    let mut input = relay.input;

    let mut received = Vec::new();
    timeout(TEST_TIMEOUT, input.read_to_end(&mut received))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(received, b"abc");
    let error = timeout(TEST_TIMEOUT, relay.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
}

#[tokio::test]
async fn stops_when_the_session_is_cancelled() {
    // A reader that never produces data and never reaches EOF.
    struct BlockedReader(std::sync::mpsc::Receiver<()>);
    impl Read for BlockedReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }
    let (_keep_blocked, blocked) = std::sync::mpsc::channel();
    let reader = BlockedReader(blocked);
    let stop = CancellationToken::new();
    let relay = input_relay(reader, stop.clone());
    stop.cancel();
    assert!(timeout(TEST_TIMEOUT, relay.task)
        .await
        .unwrap()
        .unwrap()
        .is_ok());
}
