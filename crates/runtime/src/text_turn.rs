use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::Instant;

use conversation_memory::{MemoryContextProvider, MemoryStoreError, MemoryStoreErrorKind};
use conversation_model_adapters::{
    AdapterError, GenerationLanguageModel, GenerationLanguageRequest, GenerationTextDelta,
    LanguageModelInput,
};
use conversation_protocol::{
    ConversationMode, ExecutionLocation, GenerationId, PersonaProfile, ResponseControls,
    RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStage, RuntimeTimingMilestone, SessionId,
    TurnId, MAX_CONVERSATION_MESSAGE_BYTES,
};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::ConversationQualityController;

const EVENT_BUFFER_SIZE: usize = 32;

pub struct TextTurnEventStream {
    events: mpsc::Receiver<RuntimeEvent>,
    terminal: Option<oneshot::Receiver<RuntimeEvent>>,
    events_closed: bool,
    task: Option<JoinHandle<()>>,
}

impl TextTurnEventStream {
    pub async fn recv(&mut self) -> Option<RuntimeEvent> {
        if !self.events_closed {
            if let Some(event) = self.events.recv().await {
                return Some(event);
            }
            self.events_closed = true;
        }

        let terminal_event = self.terminal.as_mut()?.await.ok();
        self.terminal = None;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        terminal_event
    }
}

#[derive(Clone)]
pub struct TextTurnRuntime {
    language_model: Arc<dyn GenerationLanguageModel>,
    state: Arc<Mutex<TextTurnState>>,
    quality: Arc<Mutex<ConversationQualityController>>,
    _session_id: Option<SessionId>,
    memory_provider: Option<Arc<dyn MemoryContextProvider>>,
}

#[derive(Default)]
struct TextTurnState {
    active: Option<ActiveTextTurn>,
    last_turn_id: Option<TurnId>,
    last_generation_id: Option<GenerationId>,
}

#[derive(Clone)]
struct ActiveTextTurn {
    turn_id: TurnId,
    generation_id: GenerationId,
    external_interruption: CancellationToken,
    work_cancellation: CancellationToken,
}

impl TextTurnRuntime {
    pub fn new(language_model: Arc<dyn GenerationLanguageModel>) -> Self {
        Self {
            language_model,
            state: Arc::new(Mutex::new(TextTurnState::default())),
            quality: Arc::new(Mutex::new(ConversationQualityController::new(
                PersonaProfile::default(),
                ResponseControls::default(),
                ConversationMode::DirectAnswer,
            ))),
            _session_id: None,
            memory_provider: None,
        }
    }

    pub fn with_quality_controller(mut self, controller: ConversationQualityController) -> Self {
        self.quality = Arc::new(Mutex::new(controller));
        self
    }

    pub fn with_session_id(mut self, session_id: SessionId) -> Self {
        self._session_id = Some(session_id);
        self
    }

    pub fn with_memory_provider(
        mut self,
        provider: Arc<dyn MemoryContextProvider>,
        language_execution: ExecutionLocation,
    ) -> Result<Self, RuntimeError> {
        if provider.execution_location() != ExecutionLocation::Local
            || language_execution != ExecutionLocation::Local
        {
            return Err(RuntimeError::new(
                RuntimeErrorKind::Configuration,
                RuntimeStage::Memory,
                "memory context requires local memory and language execution",
            ));
        }
        self.memory_provider = Some(provider);
        Ok(self)
    }

    pub async fn start_turn(
        &self,
        turn_id: TurnId,
        generation_id: GenerationId,
        transcript: impl Into<String>,
    ) -> Result<TextTurnEventStream, RuntimeError> {
        let transcript = transcript.into();
        let external_interruption = CancellationToken::new();
        let work_cancellation = CancellationToken::new();
        {
            let mut state = self.state.lock().await;
            if let Some(active) = state.active.as_ref() {
                return Err(runtime_error(format!(
                    "turn {} generation {} is still active",
                    active.turn_id, active.generation_id
                )));
            }
            if state
                .last_turn_id
                .is_some_and(|last_turn_id| turn_id <= last_turn_id)
            {
                return Err(runtime_error(format!(
                    "turn {turn_id} must be greater than the last started turn"
                )));
            }
            if state
                .last_generation_id
                .is_some_and(|last_generation_id| generation_id <= last_generation_id)
            {
                return Err(runtime_error(format!(
                    "generation {generation_id} must be greater than the last started generation"
                )));
            }
            state.last_turn_id = Some(turn_id);
            state.last_generation_id = Some(generation_id);
            state.active = Some(ActiveTextTurn {
                turn_id,
                generation_id,
                external_interruption: external_interruption.clone(),
                work_cancellation: work_cancellation.clone(),
            });
        }

        let resolved = {
            let mut quality = self.quality.lock().await;
            quality.resolve_turn(turn_id, transcript.clone(), None)
        };
        let resolved = match resolved {
            Ok(Some(resolved)) => resolved,
            Ok(None) => {
                self.release_start(turn_id, generation_id).await;
                return Err(runtime_error("a text turn transcript cannot be empty"));
            }
            Err(error) => {
                self.release_start(turn_id, generation_id).await;
                return Err(error);
            }
        };
        let language_input = match LanguageModelInput::with_quality(
            transcript,
            resolved.history_messages().iter().cloned(),
            resolved.decision().clone(),
            resolved.system_guidance(),
        ) {
            Ok(input) => input,
            Err(error) => {
                let mut quality = self.quality.lock().await;
                let _ = quality.discard_turn(turn_id);
                drop(quality);
                self.release_start(turn_id, generation_id).await;
                return Err(adapter_runtime_error(RuntimeStage::LanguageModel, error));
            }
        };

        let (event_sender, event_receiver) = mpsc::channel(EVENT_BUFFER_SIZE);
        let (terminal_sender, terminal_receiver) = oneshot::channel();
        event_sender
            .send(RuntimeEvent::TurnStarted { turn_id })
            .await
            .map_err(|_| runtime_error("text turn event stream closed before start"))?;

        let task = TextTurnTask {
            turn_id,
            generation_id,
            maximum_response_bytes: MAX_CONVERSATION_MESSAGE_BYTES
                .saturating_sub(language_input.transcript().len()),
            language_input,
            language_model: Arc::clone(&self.language_model),
            memory_provider: self.memory_provider.as_ref().map(Arc::clone),
            started_at: Instant::now(),
            external_interruption: external_interruption.clone(),
            work_cancellation,
        };
        let state = Arc::clone(&self.state);
        let quality = Arc::clone(&self.quality);
        let task = tokio::spawn(async move {
            let outcome = run_text_turn(task, &event_sender).await;
            drop(event_sender);

            let mut state = state.lock().await;
            let mut terminal = if external_interruption.is_cancelled() {
                RuntimeEvent::TurnCancelled { turn_id }
            } else {
                outcome.terminal
            };
            let quality_result = {
                let mut quality = quality.lock().await;
                match &terminal {
                    RuntimeEvent::TurnCompleted { .. } => {
                        quality.complete_turn(turn_id, outcome.generated_text)
                    }
                    RuntimeEvent::TurnCancelled { .. } => quality.interrupt_turn(turn_id),
                    RuntimeEvent::TurnFailed { .. } => quality.discard_turn(turn_id),
                    _ => Err(runtime_error("text turn produced a nonterminal result")),
                }
            };
            if let Err(error) = quality_result {
                terminal = RuntimeEvent::TurnFailed { turn_id, error };
            }
            if state.active.as_ref().is_some_and(|active| {
                active.turn_id == turn_id && active.generation_id == generation_id
            }) {
                state.active = None;
            }
            drop(state);

            let _ = terminal_sender.send(terminal);
        });

        Ok(TextTurnEventStream {
            events: event_receiver,
            terminal: Some(terminal_receiver),
            events_closed: false,
            task: Some(task),
        })
    }

    pub async fn interrupt(
        &self,
        turn_id: TurnId,
        generation_id: GenerationId,
    ) -> Result<(), RuntimeError> {
        let state = self.state.lock().await;
        match state.active.as_ref() {
            Some(active) if active.turn_id == turn_id && active.generation_id == generation_id => {
                active.external_interruption.cancel();
                active.work_cancellation.cancel();
                Ok(())
            }
            Some(active) => Err(runtime_error(format!(
                "turn {} generation {} is active, not turn {} generation {}",
                active.turn_id, active.generation_id, turn_id, generation_id
            ))),
            None => Err(runtime_error("there is no active text generation")),
        }
    }

    async fn release_start(&self, turn_id: TurnId, generation_id: GenerationId) {
        let mut state = self.state.lock().await;
        if state.active.as_ref().is_some_and(|active| {
            active.turn_id == turn_id && active.generation_id == generation_id
        }) {
            state.active = None;
        }
    }
}

struct TextTurnTask {
    turn_id: TurnId,
    generation_id: GenerationId,
    maximum_response_bytes: usize,
    language_input: LanguageModelInput,
    language_model: Arc<dyn GenerationLanguageModel>,
    memory_provider: Option<Arc<dyn MemoryContextProvider>>,
    started_at: Instant,
    external_interruption: CancellationToken,
    work_cancellation: CancellationToken,
}

struct TextTurnOutcome {
    terminal: RuntimeEvent,
    generated_text: String,
}

async fn run_text_turn(task: TextTurnTask, events: &mpsc::Sender<RuntimeEvent>) -> TextTurnOutcome {
    let TextTurnTask {
        turn_id,
        generation_id,
        maximum_response_bytes,
        mut language_input,
        language_model,
        memory_provider,
        started_at,
        external_interruption,
        work_cancellation,
    } = task;
    let mut generated_text = String::new();

    let decision = language_input
        .quality_decision()
        .expect("text turns always carry a quality decision")
        .clone();
    match send_event(
        events,
        RuntimeEvent::QualityResolved { decision },
        &external_interruption,
        &work_cancellation,
    )
    .await
    {
        EventSend::Sent => {}
        EventSend::Interrupted => return cancelled_outcome(turn_id, generated_text),
        EventSend::Closed => {
            return failed_outcome(
                turn_id,
                "text event consumer closed during quality resolution",
            );
        }
    }

    if let Some(memory_provider) = memory_provider {
        let memory_cancellation = work_cancellation.child_token();
        let retrieval_query = language_input.transcript().to_owned();
        let retrieval_cancellation = memory_cancellation.clone();
        let mut retrieval = tokio::spawn(async move {
            memory_provider
                .retrieve(turn_id, retrieval_query, retrieval_cancellation)
                .await
        });
        let retrieval = tokio::select! {
            biased;
            _ = external_interruption.cancelled() => {
                memory_cancellation.cancel();
                let _ = (&mut retrieval).await;
                return cancelled_outcome(turn_id, generated_text);
            }
            _ = work_cancellation.cancelled() => {
                memory_cancellation.cancel();
                let _ = (&mut retrieval).await;
                return cancelled_outcome(turn_id, generated_text);
            }
            _ = events.closed() => {
                memory_cancellation.cancel();
                let _ = (&mut retrieval).await;
                return failed_outcome(
                    turn_id,
                    "text event consumer closed during memory retrieval",
                );
            }
            result = &mut retrieval => result,
        };
        let retrieval = match retrieval {
            Ok(Ok(retrieval)) => retrieval,
            Ok(Err(_))
                if external_interruption.is_cancelled() || work_cancellation.is_cancelled() =>
            {
                return cancelled_outcome(turn_id, generated_text);
            }
            Ok(Err(error)) => {
                return TextTurnOutcome {
                    terminal: memory_failure(turn_id, error),
                    generated_text,
                };
            }
            Err(error) => {
                return TextTurnOutcome {
                    terminal: memory_task_failure(turn_id, error),
                    generated_text,
                };
            }
        };
        language_input = match language_input.with_memory_items(retrieval.items().iter().cloned()) {
            Ok(input) => input,
            Err(error) => {
                return TextTurnOutcome {
                    terminal: adapter_failure(turn_id, RuntimeStage::Memory, error),
                    generated_text,
                };
            }
        };
        match send_event(
            events,
            RuntimeEvent::MemoryRetrieved {
                trace: retrieval.trace().clone(),
            },
            &external_interruption,
            &work_cancellation,
        )
        .await
        {
            EventSend::Sent => {}
            EventSend::Interrupted => return cancelled_outcome(turn_id, generated_text),
            EventSend::Closed => {
                return failed_outcome(
                    turn_id,
                    "text event consumer closed during memory publication",
                );
            }
        }
    }

    let generation_request =
        match GenerationLanguageRequest::from_input(turn_id, generation_id, language_input) {
            Ok(request) => request,
            Err(error) => {
                return TextTurnOutcome {
                    terminal: adapter_failure(turn_id, RuntimeStage::LanguageModel, error),
                    generated_text,
                };
            }
        };
    if external_interruption.is_cancelled() || work_cancellation.is_cancelled() {
        return cancelled_outcome(turn_id, generated_text);
    }
    let language_cancellation = work_cancellation.child_token();
    let deltas = catch_unwind(AssertUnwindSafe(|| {
        language_model.stream(generation_request, language_cancellation.clone())
    }));
    let mut deltas = match deltas {
        Ok(deltas) => deltas,
        Err(_) => {
            return TextTurnOutcome {
                terminal: adapter_failure(
                    turn_id,
                    RuntimeStage::LanguageModel,
                    AdapterError::new("generation language adapter panicked"),
                ),
                generated_text,
            };
        }
    };
    let mut emitted_first_text = false;

    loop {
        let item = tokio::select! {
            biased;
            _ = external_interruption.cancelled() => {
                cleanup_language_stream(&language_cancellation, &mut deltas).await;
                return cancelled_outcome(turn_id, generated_text);
            }
            _ = work_cancellation.cancelled() => {
                cleanup_language_stream(&language_cancellation, &mut deltas).await;
                return cancelled_outcome(turn_id, generated_text);
            }
            _ = events.closed() => {
                cleanup_language_stream(&language_cancellation, &mut deltas).await;
                return failed_outcome(
                    turn_id,
                    "text event consumer closed during generation",
                );
            }
            item = deltas.recv() => item,
        };
        match item {
            Some(Ok(delta)) => {
                if delta.turn_id() != turn_id || delta.generation_id() != generation_id {
                    cleanup_language_stream(&language_cancellation, &mut deltas).await;
                    return TextTurnOutcome {
                        terminal: adapter_failure(
                            turn_id,
                            RuntimeStage::LanguageModel,
                            AdapterError::new("generation language delta identity mismatch"),
                        ),
                        generated_text,
                    };
                }
                if delta
                    .delta()
                    .len()
                    .gt(&maximum_response_bytes.saturating_sub(generated_text.len()))
                {
                    cleanup_language_stream(&language_cancellation, &mut deltas).await;
                    return TextTurnOutcome {
                        terminal: adapter_failure(
                            turn_id,
                            RuntimeStage::LanguageModel,
                            AdapterError::new(
                                "generation language response exceeds the completed history limit",
                            ),
                        ),
                        generated_text,
                    };
                }
                match publish_text_delta(
                    events,
                    &delta,
                    emitted_first_text,
                    started_at,
                    &external_interruption,
                    &work_cancellation,
                )
                .await
                {
                    EventSend::Sent => {
                        generated_text.push_str(delta.delta());
                    }
                    EventSend::Interrupted => {
                        cleanup_language_stream(&language_cancellation, &mut deltas).await;
                        return cancelled_outcome(turn_id, generated_text);
                    }
                    EventSend::Closed => {
                        cleanup_language_stream(&language_cancellation, &mut deltas).await;
                        return failed_outcome(
                            turn_id,
                            "text event consumer closed during text publication",
                        );
                    }
                }
                if !emitted_first_text {
                    emitted_first_text = true;
                }
            }
            Some(Err(error)) => {
                cleanup_language_stream(&language_cancellation, &mut deltas).await;
                return TextTurnOutcome {
                    terminal: adapter_failure(turn_id, RuntimeStage::LanguageModel, error),
                    generated_text,
                };
            }
            None => {
                if generated_text.trim().is_empty() {
                    return TextTurnOutcome {
                        terminal: adapter_failure(
                            turn_id,
                            RuntimeStage::LanguageModel,
                            AdapterError::new("generation language response is empty"),
                        ),
                        generated_text,
                    };
                }
                return TextTurnOutcome {
                    terminal: RuntimeEvent::TurnCompleted { turn_id },
                    generated_text,
                };
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventSend {
    Sent,
    Interrupted,
    Closed,
}

async fn send_event(
    events: &mpsc::Sender<RuntimeEvent>,
    event: RuntimeEvent,
    external_interruption: &CancellationToken,
    work_cancellation: &CancellationToken,
) -> EventSend {
    let permit = tokio::select! {
        biased;
        _ = external_interruption.cancelled() => return EventSend::Interrupted,
        _ = work_cancellation.cancelled() => return EventSend::Interrupted,
        permit = events.reserve() => match permit {
            Ok(permit) => permit,
            Err(_) => return EventSend::Closed,
        },
    };
    if external_interruption.is_cancelled() || work_cancellation.is_cancelled() {
        return EventSend::Interrupted;
    }
    permit.send(event);
    EventSend::Sent
}

async fn publish_text_delta(
    events: &mpsc::Sender<RuntimeEvent>,
    delta: &GenerationTextDelta,
    emitted_first_text: bool,
    started_at: Instant,
    external_interruption: &CancellationToken,
    work_cancellation: &CancellationToken,
) -> EventSend {
    let permit_count = if emitted_first_text { 1 } else { 2 };
    let mut permits = tokio::select! {
        biased;
        _ = external_interruption.cancelled() => return EventSend::Interrupted,
        _ = work_cancellation.cancelled() => return EventSend::Interrupted,
        permits = events.reserve_many(permit_count) => match permits {
            Ok(permits) => permits,
            Err(_) => return EventSend::Closed,
        },
    };
    if external_interruption.is_cancelled() || work_cancellation.is_cancelled() {
        return EventSend::Interrupted;
    }
    permits
        .next()
        .expect("text publication reserved its event permit")
        .send(RuntimeEvent::TextDelta {
            turn_id: delta.turn_id(),
            delta: delta.delta().to_owned(),
        });
    if !emitted_first_text {
        permits
            .next()
            .expect("first text publication reserved its timing permit")
            .send(RuntimeEvent::Timing {
                turn_id: delta.turn_id(),
                milestone: RuntimeTimingMilestone::FirstTextDelta,
                elapsed_ms: u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            });
    }
    EventSend::Sent
}

async fn cleanup_language_stream(
    cancellation: &CancellationToken,
    deltas: &mut mpsc::Receiver<Result<GenerationTextDelta, AdapterError>>,
) {
    cancellation.cancel();
    while deltas.recv().await.is_some() {}
}

fn cancelled_outcome(turn_id: TurnId, generated_text: String) -> TextTurnOutcome {
    TextTurnOutcome {
        terminal: RuntimeEvent::TurnCancelled { turn_id },
        generated_text,
    }
}

fn failed_outcome(turn_id: TurnId, message: impl Into<String>) -> TextTurnOutcome {
    TextTurnOutcome {
        terminal: RuntimeEvent::TurnFailed {
            turn_id,
            error: runtime_error(message),
        },
        generated_text: String::new(),
    }
}

fn adapter_failure(turn_id: TurnId, stage: RuntimeStage, error: AdapterError) -> RuntimeEvent {
    RuntimeEvent::TurnFailed {
        turn_id,
        error: adapter_runtime_error(stage, error),
    }
}

fn memory_failure(turn_id: TurnId, error: MemoryStoreError) -> RuntimeEvent {
    let message = match error.kind() {
        MemoryStoreErrorKind::InvalidPath => "memory provider path is invalid",
        MemoryStoreErrorKind::NotInitialized => "memory provider is not initialized",
        MemoryStoreErrorKind::UnsupportedSchema => "memory provider schema is unsupported",
        MemoryStoreErrorKind::InvalidDatabase => "memory provider database is invalid",
        MemoryStoreErrorKind::NotFound => "memory provider record was not found",
        MemoryStoreErrorKind::Conflict => "memory provider request conflicted",
        MemoryStoreErrorKind::Busy => "memory provider is busy",
        MemoryStoreErrorKind::Cancelled => "memory retrieval was cancelled",
        MemoryStoreErrorKind::LimitExceeded => "memory retrieval scan limit was exceeded",
        MemoryStoreErrorKind::Storage => "memory provider operation failed",
        _ => "memory provider operation failed",
    };
    RuntimeEvent::TurnFailed {
        turn_id,
        error: RuntimeError::new(RuntimeErrorKind::Adapter, RuntimeStage::Memory, message),
    }
}

fn memory_task_failure(turn_id: TurnId, error: JoinError) -> RuntimeEvent {
    let message = if error.is_panic() {
        "memory context provider panicked"
    } else {
        "memory context provider task failed"
    };
    adapter_failure(turn_id, RuntimeStage::Memory, AdapterError::new(message))
}

fn adapter_runtime_error(stage: RuntimeStage, error: AdapterError) -> RuntimeError {
    RuntimeError::new(RuntimeErrorKind::Adapter, stage, error.message())
}

fn runtime_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::InvalidState,
        RuntimeStage::Runtime,
        message,
    )
}
