use std::fmt;

use conversation_protocol::{
    decode_client_command, encode_gateway_message, ClientCommand, ClientRuntimeError,
    ClientRuntimeEvent, GatewayMessage, GenerationId, RuntimeStatus, TurnId,
};
use conversation_runtime::{TextTurnEventStream, TextTurnRuntime};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinError, JoinHandle};

use crate::{FrameError, FrameReader, FrameWriter};

const WRITER_BUFFER_SIZE: usize = 1;
const INVALID_COMMAND_REQUEST_ID: &str = "invalid-command";

pub struct GatewaySession {
    runtime: TextTurnRuntime,
    status: RuntimeStatus,
}

impl GatewaySession {
    pub fn new(runtime: TextTurnRuntime, status: RuntimeStatus) -> Self {
        Self { runtime, status }
    }

    pub async fn run<R, W>(self, reader: R, writer: W) -> Result<(), GatewaySessionError>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut reader = FrameReader::new(reader);
        let (writer_sender, writer_receiver) = mpsc::channel(WRITER_BUFFER_SIZE);
        let mut writer_task = tokio::spawn(writer_loop(writer, writer_receiver));
        let mut active: Option<ActiveForwarder> = None;

        send_message(
            &writer_sender,
            GatewayMessage::Ready {
                status: self.status.clone(),
            },
        )
        .await?;

        loop {
            let next = if let Some(active_turn) = active.as_mut() {
                tokio::select! {
                    writer = &mut writer_task => SessionInput::Writer(writer),
                    forwarding = &mut active_turn.task => SessionInput::Forwarder(forwarding),
                    frame = reader.read_frame() => SessionInput::Frame(frame),
                }
            } else {
                tokio::select! {
                    writer = &mut writer_task => SessionInput::Writer(writer),
                    frame = reader.read_frame() => SessionInput::Frame(frame),
                }
            };

            match next {
                SessionInput::Writer(result) => {
                    shutdown_active(&self.runtime, &mut active).await;
                    drop(writer_sender);
                    return match writer_result(result) {
                        Ok(()) => Err(GatewaySessionError::WriterUnavailable),
                        Err(error) => Err(error),
                    };
                }
                SessionInput::Forwarder(result) => {
                    active = None;
                    match forwarder_result(result) {
                        Ok(()) => {}
                        Err(error) => {
                            send_fatal(&writer_sender, "gateway event forwarding failed").await;
                            drop(writer_sender);
                            let _ = writer_result(writer_task.await);
                            return Err(error);
                        }
                    }
                }
                SessionInput::Frame(Ok(Some(payload))) => {
                    let command = match decode_client_command(&payload) {
                        Ok(command) => command,
                        Err(_) => {
                            send_rejection(
                                &writer_sender,
                                INVALID_COMMAND_REQUEST_ID,
                                command_error("client command could not be decoded"),
                            )
                            .await?;
                            continue;
                        }
                    };
                    self.handle_command(command, &writer_sender, &mut active)
                        .await?;
                }
                SessionInput::Frame(Ok(None)) => {
                    shutdown_active(&self.runtime, &mut active).await;
                    drop(writer_sender);
                    return writer_result(writer_task.await);
                }
                SessionInput::Frame(Err(error)) => {
                    shutdown_active(&self.runtime, &mut active).await;
                    send_fatal(&writer_sender, "gateway input framing failed").await;
                    drop(writer_sender);
                    let _ = writer_result(writer_task.await);
                    return Err(GatewaySessionError::Framing(error));
                }
            }
        }
    }

    async fn handle_command(
        &self,
        command: ClientCommand,
        writer: &mpsc::Sender<GatewayMessage>,
        active: &mut Option<ActiveForwarder>,
    ) -> Result<(), GatewaySessionError> {
        match command {
            ClientCommand::Status { request_id } => {
                send_accepted(writer, &request_id).await?;
                send_message(
                    writer,
                    GatewayMessage::Status {
                        request_id,
                        status: self.status.clone(),
                    },
                )
                .await
            }
            ClientCommand::StartTurn {
                request_id,
                turn_id,
                transcript,
            } => {
                if active.is_some() {
                    return send_rejection(
                        writer,
                        &request_id,
                        command_error("an active turn already exists"),
                    )
                    .await;
                }

                let generation_id = GenerationId::new(turn_id.get());
                let events = match self
                    .runtime
                    .start_turn(turn_id, generation_id, transcript)
                    .await
                {
                    Ok(events) => events,
                    Err(error) => {
                        return send_rejection(
                            writer,
                            &request_id,
                            ClientRuntimeError::from(error),
                        )
                        .await;
                    }
                };

                if let Err(error) = send_accepted(writer, &request_id).await {
                    reap_unforwarded_turn(&self.runtime, turn_id, generation_id, events).await;
                    return Err(error);
                }

                let (shutdown, shutdown_receiver) = oneshot::channel();
                let writer = writer.clone();
                let task = tokio::spawn(forward_events(events, writer, shutdown_receiver));
                *active = Some(ActiveForwarder {
                    turn_id,
                    generation_id,
                    shutdown: Some(shutdown),
                    task,
                });
                Ok(())
            }
            ClientCommand::InterruptTurn {
                request_id,
                turn_id,
            } => {
                let Some(active_turn) = active.as_ref() else {
                    return send_rejection(
                        writer,
                        &request_id,
                        command_error("there is no active text generation"),
                    )
                    .await;
                };
                if active_turn.turn_id != turn_id {
                    return send_rejection(
                        writer,
                        &request_id,
                        command_error("a different turn is active"),
                    )
                    .await;
                }

                match self
                    .runtime
                    .interrupt(turn_id, active_turn.generation_id)
                    .await
                {
                    Ok(()) => send_accepted(writer, &request_id).await,
                    Err(error) => {
                        send_rejection(writer, &request_id, ClientRuntimeError::from(error)).await
                    }
                }
            }
        }
    }
}

enum SessionInput {
    Frame(Result<Option<Vec<u8>>, FrameError>),
    Forwarder(Result<Result<(), GatewaySessionError>, JoinError>),
    Writer(Result<Result<(), GatewaySessionError>, JoinError>),
}

struct ActiveForwarder {
    turn_id: TurnId,
    generation_id: GenerationId,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), GatewaySessionError>>,
}

async fn forward_events(
    mut events: TextTurnEventStream,
    writer: mpsc::Sender<GatewayMessage>,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<(), GatewaySessionError> {
    let mut forwarding = true;
    while let Some(event) = if forwarding {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                forwarding = false;
                events.recv().await
            }
            event = events.recv() => event,
        }
    } else {
        events.recv().await
    } {
        if !forwarding {
            continue;
        }
        let event = match ClientRuntimeEvent::try_from(event) {
            Ok(event) => event,
            Err(_) => {
                while events.recv().await.is_some() {}
                return Err(GatewaySessionError::Projection);
            }
        };
        let message = GatewayMessage::RuntimeEvent { event };
        tokio::select! {
            biased;
            _ = &mut shutdown => forwarding = false,
            result = writer.send(message) => {
                if result.is_err() {
                    forwarding = false;
                }
            }
        }
    }
    Ok(())
}

async fn reap_unforwarded_turn(
    runtime: &TextTurnRuntime,
    turn_id: TurnId,
    generation_id: GenerationId,
    mut events: TextTurnEventStream,
) {
    let _ = runtime.interrupt(turn_id, generation_id).await;
    while events.recv().await.is_some() {}
}

async fn shutdown_active(runtime: &TextTurnRuntime, active: &mut Option<ActiveForwarder>) {
    let Some(mut active_turn) = active.take() else {
        return;
    };
    if let Some(shutdown) = active_turn.shutdown.take() {
        let _ = shutdown.send(());
    }
    let _ = runtime
        .interrupt(active_turn.turn_id, active_turn.generation_id)
        .await;
    let _ = active_turn.task.await;
}

async fn writer_loop<W>(
    writer: W,
    mut messages: mpsc::Receiver<GatewayMessage>,
) -> Result<(), GatewaySessionError>
where
    W: AsyncWrite + Unpin,
{
    let mut writer = FrameWriter::new(writer);
    while let Some(message) = messages.recv().await {
        let payload =
            encode_gateway_message(&message).map_err(|_| GatewaySessionError::Encoding)?;
        writer
            .write_frame(&payload)
            .await
            .map_err(GatewaySessionError::Writing)?;
    }
    Ok(())
}

async fn send_accepted(
    writer: &mpsc::Sender<GatewayMessage>,
    request_id: &str,
) -> Result<(), GatewaySessionError> {
    send_message(
        writer,
        GatewayMessage::CommandAccepted {
            request_id: request_id.to_owned(),
        },
    )
    .await
}

async fn send_rejection(
    writer: &mpsc::Sender<GatewayMessage>,
    request_id: &str,
    error: ClientRuntimeError,
) -> Result<(), GatewaySessionError> {
    send_message(
        writer,
        GatewayMessage::CommandRejected {
            request_id: request_id.to_owned(),
            error,
        },
    )
    .await
}

async fn send_message(
    writer: &mpsc::Sender<GatewayMessage>,
    message: GatewayMessage,
) -> Result<(), GatewaySessionError> {
    writer
        .send(message)
        .await
        .map_err(|_| GatewaySessionError::WriterUnavailable)
}

async fn send_fatal(writer: &mpsc::Sender<GatewayMessage>, message: &'static str) {
    let _ = send_message(
        writer,
        GatewayMessage::Fatal {
            error: ClientRuntimeError {
                kind: "configuration".to_owned(),
                stage: "runtime".to_owned(),
                message: message.to_owned(),
            },
        },
    )
    .await;
}

fn command_error(message: &'static str) -> ClientRuntimeError {
    ClientRuntimeError {
        kind: "invalid_state".to_owned(),
        stage: "runtime".to_owned(),
        message: message.to_owned(),
    }
}

fn writer_result(
    result: Result<Result<(), GatewaySessionError>, JoinError>,
) -> Result<(), GatewaySessionError> {
    match result {
        Ok(result) => result,
        Err(_) => Err(GatewaySessionError::WriterTask),
    }
}

fn forwarder_result(
    result: Result<Result<(), GatewaySessionError>, JoinError>,
) -> Result<(), GatewaySessionError> {
    match result {
        Ok(result) => result,
        Err(_) => Err(GatewaySessionError::ForwarderTask),
    }
}

#[derive(Debug)]
pub enum GatewaySessionError {
    Encoding,
    ForwarderTask,
    Framing(FrameError),
    Projection,
    WriterTask,
    WriterUnavailable,
    Writing(FrameError),
}

impl fmt::Display for GatewaySessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Encoding => "gateway message encoding failed",
            Self::ForwarderTask => "gateway event forwarding task failed",
            Self::Framing(_) => "gateway input framing failed",
            Self::Projection => "gateway event projection failed",
            Self::WriterTask => "gateway writer task failed",
            Self::WriterUnavailable => "gateway writer became unavailable",
            Self::Writing(_) => "gateway output framing failed",
        })
    }
}

impl std::error::Error for GatewaySessionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Framing(error) | Self::Writing(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use conversation_model_adapters::{OllamaConfig, OllamaLanguageModel};
    use conversation_protocol::RuntimeStatus;
    use tokio::io::{duplex, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Notify;
    use tokio::time::timeout;

    use super::GatewaySession;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn interrupt_cancels_and_reaps_while_stdout_writer_is_blocked() {
        let language = HoldOpenLanguageServer::start().await;
        let model = OllamaLanguageModel::new_direct(
            OllamaConfig::new("test-model")
                .unwrap()
                .with_endpoint(language.endpoint())
                .unwrap(),
        );
        let runtime = conversation_runtime::TextTurnRuntime::new(Arc::new(model));
        let session = GatewaySession::new(runtime, status());
        let (mut input, reader) = duplex(4096);
        let (writer, writer_state) = BlockingWriter::new(4);
        let session_task = tokio::spawn(session.run(reader, writer));

        write_command(
            &mut input,
            r#"{"protocol_version":1,"type":"start_turn","request_id":"start-1","turn_id":"1","transcript":"fixture question"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, writer_state.blocked.wait())
            .await
            .expect("gateway writer never reached bounded backpressure");
        timeout(TEST_TIMEOUT, language.request_started.wait())
            .await
            .expect("fake language request never started");

        write_command(
            &mut input,
            r#"{"protocol_version":1,"type":"interrupt_turn","request_id":"interrupt-1","turn_id":"1"}"#,
        )
        .await;
        timeout(TEST_TIMEOUT, language.connection_reaped.wait())
            .await
            .expect("language cancellation did not reap the request while stdout was blocked");
        assert!(writer_state.blocked.is_set());
        assert!(!writer_state.released.load(Ordering::SeqCst));
        assert!(!session_task.is_finished());

        writer_state.release();
        timeout(
            TEST_TIMEOUT,
            writer_state.wait_for_bytes(r#""type":"turn_cancelled""#),
        )
        .await
        .expect("cancelled terminal was not written after stdout resumed");
        drop(input);
        timeout(TEST_TIMEOUT, session_task)
            .await
            .expect("gateway session did not clean up after writer release")
            .unwrap()
            .unwrap();

        let messages = decode_frames(&writer_state.bytes());
        assert_eq!(message_type(&messages[0]), "ready");
        let start_accepted = message_index(&messages, |message| {
            message_type(message) == "command_accepted"
                && message.contains(r#""request_id":"start-1""#)
        });
        let first_event = message_index(&messages, |message| {
            message_type(message) == "runtime_event"
        });
        assert!(start_accepted < first_event);
        assert!(messages.iter().any(|message| {
            message_type(message) == "command_accepted"
                && message.contains(r#""request_id":"interrupt-1""#)
        }));
        let terminals = messages
            .iter()
            .filter(|message| is_terminal(message))
            .collect::<Vec<_>>();
        assert_eq!(terminals.len(), 1);
        assert!(terminals[0].contains(r#""type":"turn_cancelled""#));
    }

    fn status() -> RuntimeStatus {
        RuntimeStatus {
            transport: "stdio".to_owned(),
            privacy_mode: "local_only".to_owned(),
            language_location: "local".to_owned(),
            model_id: "test-model".to_owned(),
            memory_enabled: false,
            memory_location: None,
            telemetry_enabled: false,
            capabilities: vec!["text".to_owned()],
        }
    }

    async fn write_command(writer: &mut (impl AsyncWrite + Unpin), payload: &str) {
        writer
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await
            .unwrap();
        writer.write_all(payload.as_bytes()).await.unwrap();
        writer.flush().await.unwrap();
    }

    struct BlockingWriter {
        state: Arc<BlockingWriterState>,
        writes_before_block: usize,
        completed_writes: usize,
    }

    struct BlockingWriterState {
        bytes: StdMutex<Vec<u8>>,
        blocked: Condition,
        released: AtomicBool,
        waker: StdMutex<Option<Waker>>,
        write_notify: Notify,
    }

    impl BlockingWriter {
        fn new(writes_before_block: usize) -> (Self, Arc<BlockingWriterState>) {
            let state = Arc::new(BlockingWriterState {
                bytes: StdMutex::new(Vec::new()),
                blocked: Condition::default(),
                released: AtomicBool::new(false),
                waker: StdMutex::new(None),
                write_notify: Notify::new(),
            });
            (
                Self {
                    state: Arc::clone(&state),
                    writes_before_block,
                    completed_writes: 0,
                },
                state,
            )
        }
    }

    impl AsyncWrite for BlockingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.completed_writes >= self.writes_before_block
                && !self.state.released.load(Ordering::SeqCst)
            {
                self.state.blocked.set();
                *self.state.waker.lock().unwrap() = Some(context.waker().clone());
                if !self.state.released.load(Ordering::SeqCst) {
                    return Poll::Pending;
                }
            }
            self.state.bytes.lock().unwrap().extend_from_slice(bytes);
            self.state.write_notify.notify_waiters();
            self.completed_writes += 1;
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl BlockingWriterState {
        fn release(&self) {
            self.released.store(true, Ordering::SeqCst);
            if let Some(waker) = self.waker.lock().unwrap().take() {
                waker.wake();
            }
        }

        fn bytes(&self) -> Vec<u8> {
            self.bytes.lock().unwrap().clone()
        }

        async fn wait_for_bytes(&self, expected: &str) {
            loop {
                let notified = self.write_notify.notified();
                if String::from_utf8_lossy(&self.bytes()).contains(expected) {
                    return;
                }
                notified.await;
            }
        }
    }

    #[derive(Default)]
    struct Condition {
        set: AtomicBool,
        notify: Notify,
    }

    impl Condition {
        fn set(&self) {
            self.set.store(true, Ordering::SeqCst);
            self.notify.notify_waiters();
        }

        fn is_set(&self) -> bool {
            self.set.load(Ordering::SeqCst)
        }

        async fn wait(&self) {
            loop {
                let notified = self.notify.notified();
                if self.is_set() {
                    return;
                }
                notified.await;
            }
        }
    }

    struct HoldOpenLanguageServer {
        endpoint: String,
        request_started: Arc<Condition>,
        connection_reaped: Arc<Condition>,
        task: tokio::task::JoinHandle<()>,
    }

    impl HoldOpenLanguageServer {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let request_started = Arc::new(Condition::default());
            let connection_reaped = Arc::new(Condition::default());
            let task_request_started = Arc::clone(&request_started);
            let task_connection_reaped = Arc::clone(&connection_reaped);
            let task = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request(&mut stream).await;
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n\
{\"message\":{\"role\":\"assistant\",\"content\":\"partial\"},\"done\":false}\n",
                    )
                    .await
                    .unwrap();
                stream.flush().await.unwrap();
                task_request_started.set();
                let mut byte = [0_u8; 1];
                loop {
                    match stream.read(&mut byte).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                task_connection_reaped.set();
            });
            Self {
                endpoint,
                request_started,
                connection_reaped,
                task,
            }
        }

        fn endpoint(&self) -> &str {
            &self.endpoint
        }
    }

    impl Drop for HoldOpenLanguageServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn read_request(stream: &mut TcpStream) {
        let mut request = Vec::new();
        let header_end = loop {
            if let Some(index) = find_bytes(&request, b"\r\n\r\n") {
                break index + 4;
            }
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&chunk[..count]);
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let mut chunk = [0_u8; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&chunk[..count]);
        }
    }

    fn find_bytes(bytes: &[u8], pattern: &[u8]) -> Option<usize> {
        bytes
            .windows(pattern.len())
            .position(|window| window == pattern)
    }

    fn decode_frames(bytes: &[u8]) -> Vec<String> {
        let mut messages = Vec::new();
        let mut offset = 0;
        while offset < bytes.len() {
            assert!(bytes.len() - offset >= 4);
            let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;
            assert!(bytes.len() - offset >= length);
            messages.push(String::from_utf8(bytes[offset..offset + length].to_vec()).unwrap());
            offset += length;
        }
        messages
    }

    fn message_type(message: &str) -> &str {
        message
            .split_once(r#""type":""#)
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap()
    }

    fn message_index(messages: &[String], predicate: impl Fn(&str) -> bool) -> usize {
        messages
            .iter()
            .position(|message| predicate(message))
            .unwrap()
    }

    fn is_terminal(message: &str) -> bool {
        message.contains(r#""type":"turn_completed""#)
            || message.contains(r#""type":"turn_cancelled""#)
            || message.contains(r#""type":"turn_failed""#)
    }
}
