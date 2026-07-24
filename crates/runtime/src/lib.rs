use std::sync::Arc;

use conversation_model_adapters::{
    AdapterError, LanguageModel, LanguageModelRequest, SpeechRequest, SpeechSynthesizer,
};
use conversation_protocol::{RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStage, TurnId};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

const EVENT_BUFFER_SIZE: usize = 32;

#[derive(Clone)]
pub struct ConversationRuntime {
    language_model: Arc<dyn LanguageModel>,
    speech_synthesizer: Arc<dyn SpeechSynthesizer>,
    active_turn: Arc<Mutex<Option<ActiveTurn>>>,
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
            active_turn: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn start_turn(
        &self,
        turn_id: TurnId,
        transcript: impl Into<String>,
    ) -> Result<mpsc::Receiver<RuntimeEvent>, RuntimeError> {
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
            *active_turn = Some(ActiveTurn {
                turn_id,
                cancellation: cancellation.clone(),
            });
        }

        let (event_sender, event_receiver) = mpsc::channel(EVENT_BUFFER_SIZE);
        let language_model = Arc::clone(&self.language_model);
        let speech_synthesizer = Arc::clone(&self.speech_synthesizer);
        let active_turn = Arc::clone(&self.active_turn);

        tokio::spawn(async move {
            let terminal_event = run_turn(
                turn_id,
                transcript,
                language_model,
                speech_synthesizer,
                cancellation,
                &event_sender,
            )
            .await;

            {
                let mut active = active_turn.lock().await;
                if active.as_ref().map(|current| current.turn_id) == Some(turn_id) {
                    *active = None;
                }
            }

            let _ = event_sender.send(terminal_event).await;
        });

        Ok(event_receiver)
    }

    pub async fn interrupt(&self, turn_id: TurnId) -> Result<(), RuntimeError> {
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
    cancellation: CancellationToken,
    events: &mpsc::Sender<RuntimeEvent>,
) -> RuntimeEvent {
    if !send_event(events, RuntimeEvent::TurnStarted { turn_id }).await {
        cancellation.cancel();
        return RuntimeEvent::TurnCancelled { turn_id };
    }

    if !send_event(
        events,
        RuntimeEvent::TranscriptFinal {
            turn_id,
            text: transcript.clone(),
        },
    )
    .await
    {
        cancellation.cancel();
        return RuntimeEvent::TurnCancelled { turn_id };
    }

    let mut response = String::new();
    let mut deltas = language_model.stream(
        LanguageModelRequest::new(turn_id, transcript),
        cancellation.child_token(),
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
                        response.push_str(&delta);
                        if !send_event(
                            events,
                            RuntimeEvent::TextDelta { turn_id, delta },
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

    if !send_event(events, RuntimeEvent::SpeechStarted { turn_id }).await {
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

    if !send_event(events, RuntimeEvent::SpeechCompleted { turn_id }).await {
        cancellation.cancel();
        return RuntimeEvent::TurnCancelled { turn_id };
    }

    RuntimeEvent::TurnCompleted { turn_id }
}

async fn send_event(events: &mpsc::Sender<RuntimeEvent>, event: RuntimeEvent) -> bool {
    events.send(event).await.is_ok()
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
