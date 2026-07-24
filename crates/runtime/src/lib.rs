use std::sync::Arc;

use conversation_model_adapters::{
    AdapterError, LanguageModel, LanguageModelRequest, SpeechRequest, SpeechSynthesizer,
};
use conversation_protocol::{
    RuntimeCommand, RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStage, TurnId,
};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

const EVENT_BUFFER_SIZE: usize = 32;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[non_exhaustive]
pub enum RuntimeCommandResult {
    TurnStarted { events: TurnEventStream },
    InterruptAccepted,
}

pub struct TurnEventStream {
    events: mpsc::Receiver<RuntimeEvent>,
    terminal: Option<oneshot::Receiver<RuntimeEvent>>,
    events_closed: bool,
}

impl TurnEventStream {
    pub async fn recv(&mut self) -> Option<RuntimeEvent> {
        if !self.events_closed {
            if let Some(event) = self.events.recv().await {
                return Some(event);
            }
            self.events_closed = true;
        }

        let terminal_event = self.terminal.as_mut()?.await.ok();
        self.terminal = None;
        terminal_event
    }
}

#[derive(Clone)]
pub struct ConversationRuntime {
    language_model: Arc<dyn LanguageModel>,
    speech_synthesizer: Arc<dyn SpeechSynthesizer>,
    max_response_bytes: usize,
    active_turn: Arc<Mutex<Option<ActiveTurn>>>,
    last_started_turn_id: Arc<Mutex<Option<TurnId>>>,
}

#[derive(Clone)]
struct ActiveTurn {
    turn_id: TurnId,
    cancellation: CancellationToken,
}

impl ConversationRuntime {
    pub fn new(
        language_model: Arc<dyn LanguageModel>,
        speech_synthesizer: Arc<dyn SpeechSynthesizer>,
    ) -> Self {
        Self {
            language_model,
            speech_synthesizer,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            active_turn: Arc::new(Mutex::new(None)),
            last_started_turn_id: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_max_response_bytes(
        mut self,
        max_response_bytes: usize,
    ) -> Result<Self, RuntimeError> {
        if max_response_bytes == 0 {
            return Err(RuntimeError::new(
                RuntimeErrorKind::Configuration,
                RuntimeStage::Runtime,
                "runtime response byte limit must be non-zero",
            ));
        }

        self.max_response_bytes = max_response_bytes;
        Ok(self)
    }

    pub async fn execute(
        &self,
        command: RuntimeCommand,
    ) -> Result<RuntimeCommandResult, RuntimeError> {
        match command {
            RuntimeCommand::StartTurn {
                turn_id,
                transcript,
            } => {
                let events = self.start_turn(turn_id, transcript).await?;
                Ok(RuntimeCommandResult::TurnStarted { events })
            }
            RuntimeCommand::Interrupt { turn_id } => {
                self.interrupt(turn_id).await?;
                Ok(RuntimeCommandResult::InterruptAccepted)
            }
            _ => Err(runtime_error("unsupported runtime command")),
        }
    }

    async fn start_turn(
        &self,
        turn_id: TurnId,
        transcript: impl Into<String>,
    ) -> Result<TurnEventStream, RuntimeError> {
        let transcript = transcript.into();
        let cancellation = CancellationToken::new();

        {
            let mut active_turn = self.active_turn.lock().await;
            if let Some(active) = active_turn.as_ref() {
                return Err(runtime_error(format!(
                    "turn {} is still active",
                    active.turn_id
                )));
            }
            let mut last_started_turn_id = self.last_started_turn_id.lock().await;
            if last_started_turn_id.is_some_and(|last_turn_id| turn_id <= last_turn_id) {
                return Err(runtime_error(format!(
                    "turn {} must be greater than the last started turn",
                    turn_id
                )));
            }
            *last_started_turn_id = Some(turn_id);
            *active_turn = Some(ActiveTurn {
                turn_id,
                cancellation: cancellation.clone(),
            });
        }

        let (event_sender, event_receiver) = mpsc::channel(EVENT_BUFFER_SIZE);
        let (terminal_sender, terminal_receiver) = oneshot::channel();
        event_sender
            .send(RuntimeEvent::TurnStarted { turn_id })
            .await
            .map_err(|_| runtime_error("turn event stream closed before start"))?;
        let language_model = Arc::clone(&self.language_model);
        let speech_synthesizer = Arc::clone(&self.speech_synthesizer);
        let max_response_bytes = self.max_response_bytes;
        let active_turn = Arc::clone(&self.active_turn);

        tokio::spawn(async move {
            let worker_cancellation = cancellation.clone();
            let terminal_event = run_turn(
                turn_id,
                transcript,
                language_model,
                speech_synthesizer,
                max_response_bytes,
                worker_cancellation,
                &event_sender,
            )
            .await;
            drop(event_sender);

            let mut active = active_turn.lock().await;
            let terminal_event = if cancellation.is_cancelled() {
                RuntimeEvent::TurnCancelled { turn_id }
            } else {
                terminal_event
            };
            let _ = terminal_sender.send(terminal_event);

            if active.as_ref().map(|current| current.turn_id) == Some(turn_id) {
                *active = None;
            }
        });

        Ok(TurnEventStream {
            events: event_receiver,
            terminal: Some(terminal_receiver),
            events_closed: false,
        })
    }

    async fn interrupt(&self, turn_id: TurnId) -> Result<(), RuntimeError> {
        let active_turn = self.active_turn.lock().await;
        match active_turn.as_ref() {
            Some(active) if active.turn_id == turn_id => {
                active.cancellation.cancel();
                Ok(())
            }
            Some(active) => Err(runtime_error(format!(
                "turn {} is active, not turn {}",
                active.turn_id, turn_id
            ))),
            None => Err(runtime_error("there is no active turn")),
        }
    }
}

async fn run_turn(
    turn_id: TurnId,
    transcript: String,
    language_model: Arc<dyn LanguageModel>,
    speech_synthesizer: Arc<dyn SpeechSynthesizer>,
    max_response_bytes: usize,
    cancellation: CancellationToken,
    events: &mpsc::Sender<RuntimeEvent>,
) -> RuntimeEvent {
    if !send_event(
        events,
        RuntimeEvent::TranscriptFinal {
            turn_id,
            text: transcript.clone(),
        },
        &cancellation,
    )
    .await
    {
        cancellation.cancel();
        return RuntimeEvent::TurnCancelled { turn_id };
    }

    let mut response = String::new();
    let language_model_cancellation = cancellation.child_token();
    let mut deltas = language_model.stream(
        LanguageModelRequest::new(turn_id, transcript),
        language_model_cancellation.clone(),
    );

    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return RuntimeEvent::TurnCancelled { turn_id };
            }
            item = deltas.recv() => {
                match item {
                    Some(Ok(delta)) => {
                        if delta.len() > max_response_bytes.saturating_sub(response.len()) {
                            language_model_cancellation.cancel();
                            return adapter_failure(
                                turn_id,
                                RuntimeStage::LanguageModel,
                                AdapterError::new(format!(
                                    "language model response exceeds the maximum size of {max_response_bytes} bytes"
                                )),
                            );
                        }
                        response.push_str(&delta);
                        if !send_event(
                            events,
                            RuntimeEvent::TextDelta { turn_id, delta },
                            &cancellation,
                        ).await {
                            cancellation.cancel();
                            return RuntimeEvent::TurnCancelled { turn_id };
                        }
                    }
                    Some(Err(error)) => {
                        return adapter_failure(turn_id, RuntimeStage::LanguageModel, error);
                    }
                    None => break,
                }
            }
        }
    }

    if !send_event(
        events,
        RuntimeEvent::SpeechStarted { turn_id },
        &cancellation,
    )
    .await
    {
        cancellation.cancel();
        return RuntimeEvent::TurnCancelled { turn_id };
    }

    let speech_result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            return RuntimeEvent::TurnCancelled { turn_id };
        }
        result = speech_synthesizer.synthesize(
            SpeechRequest::new(turn_id, response),
            cancellation.child_token(),
        ) => result,
    };

    if let Err(error) = speech_result {
        return adapter_failure(turn_id, RuntimeStage::SpeechSynthesizer, error);
    }

    if !send_event(
        events,
        RuntimeEvent::SpeechCompleted { turn_id },
        &cancellation,
    )
    .await
    {
        cancellation.cancel();
        return RuntimeEvent::TurnCancelled { turn_id };
    }

    RuntimeEvent::TurnCompleted { turn_id }
}

async fn send_event(
    events: &mpsc::Sender<RuntimeEvent>,
    event: RuntimeEvent,
    cancellation: &CancellationToken,
) -> bool {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => false,
        result = events.send(event) => result.is_ok(),
    }
}

fn adapter_failure(turn_id: TurnId, stage: RuntimeStage, error: AdapterError) -> RuntimeEvent {
    RuntimeEvent::TurnFailed {
        turn_id,
        error: RuntimeError::new(RuntimeErrorKind::Adapter, stage, error.message()),
    }
}

fn runtime_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::InvalidState,
        RuntimeStage::Runtime,
        message,
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use conversation_protocol::{RuntimeEvent, TurnId};
    use tokio::sync::{mpsc, oneshot};
    use tokio::time::timeout;

    use super::TurnEventStream;

    #[tokio::test]
    async fn cancelled_terminal_receive_can_be_retried() {
        let (event_sender, event_receiver) = mpsc::channel(1);
        let (terminal_sender, terminal_receiver) = oneshot::channel();
        drop(event_sender);
        let mut events = TurnEventStream {
            events: event_receiver,
            terminal: Some(terminal_receiver),
            events_closed: false,
        };
        let turn_id = TurnId::new(1);

        assert!(timeout(Duration::from_millis(1), events.recv())
            .await
            .is_err());
        terminal_sender
            .send(RuntimeEvent::TurnCompleted { turn_id })
            .unwrap();

        assert_eq!(
            events.recv().await,
            Some(RuntimeEvent::TurnCompleted { turn_id })
        );
        assert_eq!(events.recv().await, None);
    }
}
