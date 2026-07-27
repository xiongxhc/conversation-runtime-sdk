use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use conversation_model_adapters::{
    AdapterError, AudioOutput, LanguageModel, LanguageModelRequest, SpeechSynthesizer,
};
use conversation_protocol::{
    RuntimeCommand, RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStage,
    RuntimeTimingMilestone, TurnId,
};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

mod phrase_chunker;
mod speech_worker;

pub use phrase_chunker::PhraseChunkingConfig;

use phrase_chunker::PhraseChunker;
use speech_worker::{SpeechSegment, SpeechWorker, SpeechWorkerContext, SpeechWorkerOutcome};

const EVENT_BUFFER_SIZE: usize = 32;
const PHRASE_QUEUE_CAPACITY: usize = 2;
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
    audio_output: Arc<dyn AudioOutput>,
    max_response_bytes: usize,
    phrase_chunking: PhraseChunkingConfig,
    active_turn: Arc<Mutex<Option<ActiveTurn>>>,
    last_started_turn_id: Arc<Mutex<Option<TurnId>>>,
}

#[derive(Clone)]
struct ActiveTurn {
    turn_id: TurnId,
    external_interruption: CancellationToken,
    work_cancellation: CancellationToken,
}

impl ConversationRuntime {
    pub fn new(
        language_model: Arc<dyn LanguageModel>,
        speech_synthesizer: Arc<dyn SpeechSynthesizer>,
        audio_output: Arc<dyn AudioOutput>,
    ) -> Self {
        Self {
            language_model,
            speech_synthesizer,
            audio_output,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            phrase_chunking: PhraseChunkingConfig::default(),
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

    pub fn with_phrase_chunking(mut self, config: PhraseChunkingConfig) -> Self {
        self.phrase_chunking = config;
        self
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
        let external_interruption = CancellationToken::new();
        let work_cancellation = CancellationToken::new();

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
                external_interruption: external_interruption.clone(),
                work_cancellation: work_cancellation.clone(),
            });
        }

        let (event_sender, event_receiver) = mpsc::channel(EVENT_BUFFER_SIZE);
        let (terminal_sender, terminal_receiver) = oneshot::channel();
        let started_at = Instant::now();
        event_sender
            .send(RuntimeEvent::TurnStarted { turn_id })
            .await
            .map_err(|_| runtime_error("turn event stream closed before start"))?;
        let language_model = Arc::clone(&self.language_model);
        let speech_synthesizer = Arc::clone(&self.speech_synthesizer);
        let audio_output = Arc::clone(&self.audio_output);
        let max_response_bytes = self.max_response_bytes;
        let phrase_chunking = self.phrase_chunking;
        let active_turn = Arc::clone(&self.active_turn);
        let terminal_external_interruption = external_interruption.clone();

        tokio::spawn(async move {
            let terminal_event = run_turn(
                TurnTask {
                    turn_id,
                    transcript,
                    language_model,
                    speech_synthesizer,
                    audio_output,
                    max_response_bytes,
                    phrase_chunking,
                    started_at,
                    external_interruption,
                    work_cancellation,
                },
                &event_sender,
            )
            .await;
            drop(event_sender);

            let mut active = active_turn.lock().await;
            let terminal_event = if terminal_external_interruption.is_cancelled() {
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
                active.external_interruption.cancel();
                active.work_cancellation.cancel();
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

struct TurnTask {
    turn_id: TurnId,
    transcript: String,
    language_model: Arc<dyn LanguageModel>,
    speech_synthesizer: Arc<dyn SpeechSynthesizer>,
    audio_output: Arc<dyn AudioOutput>,
    max_response_bytes: usize,
    phrase_chunking: PhraseChunkingConfig,
    started_at: Instant,
    external_interruption: CancellationToken,
    work_cancellation: CancellationToken,
}

async fn run_turn(task: TurnTask, events: &mpsc::Sender<RuntimeEvent>) -> RuntimeEvent {
    let TurnTask {
        turn_id,
        transcript,
        language_model,
        speech_synthesizer,
        audio_output,
        max_response_bytes,
        phrase_chunking,
        started_at,
        external_interruption,
        work_cancellation,
    } = task;
    let event_gate = Arc::new(Mutex::new(()));

    match send_event_before_worker(
        events,
        &event_gate,
        RuntimeEvent::TranscriptFinal {
            turn_id,
            text: transcript.clone(),
        },
        &external_interruption,
        &work_cancellation,
    )
    .await
    {
        InitialSend::Sent => {}
        InitialSend::Interrupted => return RuntimeEvent::TurnCancelled { turn_id },
        InitialSend::Closed => {
            return runtime_failure(turn_id, "turn event stream closed during transcript");
        }
    }

    let (phrase_sender, phrase_receiver) = mpsc::channel(PHRASE_QUEUE_CAPACITY);
    let mut phrase_sender = Some(phrase_sender);
    let speech_worker = SpeechWorker::new(SpeechWorkerContext {
        turn_id,
        speech_synthesizer,
        audio_output,
        segments: phrase_receiver,
        events: events.clone(),
        event_gate: Arc::clone(&event_gate),
        started_at,
        external_interruption: external_interruption.clone(),
        work_cancellation: work_cancellation.clone(),
    })
    .run();
    tokio::pin!(speech_worker);

    let language_model_cancellation = work_cancellation.child_token();
    let mut deltas = language_model.stream(
        LanguageModelRequest::new(turn_id, transcript),
        language_model_cancellation.clone(),
    );
    let mut chunker = PhraseChunker::new(phrase_chunking);
    let mut response_bytes = 0_usize;
    let mut emitted_first_text_timing = false;
    let mut segment_index = 0_u64;

    loop {
        let item = tokio::select! {
            biased;
            _ = external_interruption.cancelled() => {
                stop_pipeline(
                    &mut phrase_sender,
                    &language_model_cancellation,
                    &work_cancellation,
                    &mut deltas,
                    speech_worker.as_mut(),
                ).await;
                return RuntimeEvent::TurnCancelled { turn_id };
            }
            _ = work_cancellation.cancelled() => {
                let worker_outcome = speech_worker.as_mut().await;
                phrase_sender.take();
                cleanup_language_model(&language_model_cancellation, &mut deltas).await;
                return terminal_from_worker(turn_id, worker_outcome);
            }
            worker_outcome = speech_worker.as_mut() => {
                phrase_sender.take();
                cleanup_language_model(&language_model_cancellation, &mut deltas).await;
                return terminal_from_worker(turn_id, worker_outcome);
            }
            item = deltas.recv() => {
                item
            }
        };

        match item {
            Some(Ok(delta)) => {
                if delta.len() > max_response_bytes.saturating_sub(response_bytes) {
                    let terminal = adapter_failure(
                        turn_id,
                        RuntimeStage::LanguageModel,
                        AdapterError::new(format!(
                            "language model response exceeds the maximum size of {max_response_bytes} bytes"
                        )),
                    );
                    stop_pipeline(
                        &mut phrase_sender,
                        &language_model_cancellation,
                        &work_cancellation,
                        &mut deltas,
                        speech_worker.as_mut(),
                    )
                    .await;
                    return terminal;
                }
                response_bytes += delta.len();

                let mut text_events = Vec::with_capacity(2);
                if !emitted_first_text_timing {
                    text_events.push(timing_event(
                        turn_id,
                        RuntimeTimingMilestone::FirstTextDelta,
                        started_at,
                    ));
                    emitted_first_text_timing = true;
                }
                text_events.push(RuntimeEvent::TextDelta {
                    turn_id,
                    delta: delta.clone(),
                });
                match send_events_while_worker_active(
                    events,
                    &event_gate,
                    text_events,
                    &external_interruption,
                    &work_cancellation,
                    speech_worker.as_mut(),
                )
                .await
                {
                    PipelineSend::Sent => {}
                    PipelineSend::Interrupted => {
                        stop_pipeline(
                            &mut phrase_sender,
                            &language_model_cancellation,
                            &work_cancellation,
                            &mut deltas,
                            speech_worker.as_mut(),
                        )
                        .await;
                        return RuntimeEvent::TurnCancelled { turn_id };
                    }
                    PipelineSend::WorkerFinished(worker_outcome) => {
                        phrase_sender.take();
                        cleanup_language_model(&language_model_cancellation, &mut deltas).await;
                        return terminal_from_worker(turn_id, worker_outcome);
                    }
                    PipelineSend::Closed => {
                        stop_pipeline(
                            &mut phrase_sender,
                            &language_model_cancellation,
                            &work_cancellation,
                            &mut deltas,
                            speech_worker.as_mut(),
                        )
                        .await;
                        return runtime_failure(
                            turn_id,
                            "turn event stream closed during generation",
                        );
                    }
                }

                for text in chunker.push_delta(&delta) {
                    let segment = SpeechSegment {
                        index: segment_index,
                        text,
                    };
                    segment_index += 1;
                    match send_phrase_while_worker_active(
                        phrase_sender
                            .as_ref()
                            .expect("phrase sender exists while generation is active"),
                        segment,
                        &external_interruption,
                        &work_cancellation,
                        speech_worker.as_mut(),
                    )
                    .await
                    {
                        PipelineSend::Sent => {}
                        PipelineSend::Interrupted => {
                            stop_pipeline(
                                &mut phrase_sender,
                                &language_model_cancellation,
                                &work_cancellation,
                                &mut deltas,
                                speech_worker.as_mut(),
                            )
                            .await;
                            return RuntimeEvent::TurnCancelled { turn_id };
                        }
                        PipelineSend::WorkerFinished(worker_outcome) => {
                            phrase_sender.take();
                            cleanup_language_model(&language_model_cancellation, &mut deltas).await;
                            return terminal_from_worker(turn_id, worker_outcome);
                        }
                        PipelineSend::Closed => {
                            stop_pipeline(
                                &mut phrase_sender,
                                &language_model_cancellation,
                                &work_cancellation,
                                &mut deltas,
                                speech_worker.as_mut(),
                            )
                            .await;
                            return runtime_failure(turn_id, "speech phrase queue closed early");
                        }
                    }
                }
            }
            Some(Err(error)) => {
                let terminal = adapter_failure(turn_id, RuntimeStage::LanguageModel, error);
                stop_pipeline(
                    &mut phrase_sender,
                    &language_model_cancellation,
                    &work_cancellation,
                    &mut deltas,
                    speech_worker.as_mut(),
                )
                .await;
                return terminal;
            }
            None => break,
        }
    }

    if let Some(text) = chunker.finish() {
        let segment = SpeechSegment {
            index: segment_index,
            text,
        };
        match send_phrase_while_worker_active(
            phrase_sender
                .as_ref()
                .expect("phrase sender exists before final worker drain"),
            segment,
            &external_interruption,
            &work_cancellation,
            speech_worker.as_mut(),
        )
        .await
        {
            PipelineSend::Sent => {}
            PipelineSend::Interrupted => {
                stop_pipeline(
                    &mut phrase_sender,
                    &language_model_cancellation,
                    &work_cancellation,
                    &mut deltas,
                    speech_worker.as_mut(),
                )
                .await;
                return RuntimeEvent::TurnCancelled { turn_id };
            }
            PipelineSend::WorkerFinished(worker_outcome) => {
                phrase_sender.take();
                return terminal_from_worker(turn_id, worker_outcome);
            }
            PipelineSend::Closed => {
                stop_pipeline(
                    &mut phrase_sender,
                    &language_model_cancellation,
                    &work_cancellation,
                    &mut deltas,
                    speech_worker.as_mut(),
                )
                .await;
                return runtime_failure(turn_id, "speech phrase queue closed before final segment");
            }
        }
    }

    phrase_sender.take();
    terminal_from_worker(turn_id, speech_worker.await)
}

async fn send_event_before_worker(
    events: &mpsc::Sender<RuntimeEvent>,
    event_gate: &Mutex<()>,
    event: RuntimeEvent,
    external_interruption: &CancellationToken,
    work_cancellation: &CancellationToken,
) -> InitialSend {
    let _event_guard = tokio::select! {
        biased;
        _ = external_interruption.cancelled() => {
            return InitialSend::Interrupted;
        }
        _ = work_cancellation.cancelled() => {
            return InitialSend::Interrupted;
        }
        event_guard = event_gate.lock() => event_guard,
    };

    let result = tokio::select! {
        biased;
        _ = external_interruption.cancelled() => {
            return InitialSend::Interrupted;
        }
        _ = work_cancellation.cancelled() => {
            return InitialSend::Interrupted;
        }
        result = events.send(event) => result,
    };
    if result.is_ok() {
        InitialSend::Sent
    } else {
        InitialSend::Closed
    }
}

async fn send_events_while_worker_active<F>(
    events: &mpsc::Sender<RuntimeEvent>,
    event_gate: &Mutex<()>,
    pending_events: Vec<RuntimeEvent>,
    external_interruption: &CancellationToken,
    work_cancellation: &CancellationToken,
    mut speech_worker: Pin<&mut F>,
) -> PipelineSend
where
    F: Future<Output = SpeechWorkerOutcome>,
{
    let _event_guard = tokio::select! {
        biased;
        _ = external_interruption.cancelled() => {
            return PipelineSend::Interrupted;
        }
        _ = work_cancellation.cancelled() => {
            return PipelineSend::WorkerFinished(speech_worker.await);
        }
        worker_outcome = speech_worker.as_mut() => {
            return PipelineSend::WorkerFinished(worker_outcome);
        }
        event_guard = event_gate.lock() => event_guard,
    };

    for event in pending_events {
        let result = tokio::select! {
            biased;
            _ = external_interruption.cancelled() => {
                return PipelineSend::Interrupted;
            }
            _ = work_cancellation.cancelled() => {
                return PipelineSend::WorkerFinished(speech_worker.await);
            }
            worker_outcome = speech_worker.as_mut() => {
                return PipelineSend::WorkerFinished(worker_outcome);
            }
            result = events.send(event) => result,
        };
        if result.is_err() {
            return PipelineSend::Closed;
        }
    }

    PipelineSend::Sent
}

async fn send_phrase_while_worker_active<F>(
    phrases: &mpsc::Sender<SpeechSegment>,
    segment: SpeechSegment,
    external_interruption: &CancellationToken,
    work_cancellation: &CancellationToken,
    mut speech_worker: Pin<&mut F>,
) -> PipelineSend
where
    F: Future<Output = SpeechWorkerOutcome>,
{
    tokio::select! {
        biased;
        _ = external_interruption.cancelled() => PipelineSend::Interrupted,
        _ = work_cancellation.cancelled() => {
            PipelineSend::WorkerFinished(speech_worker.await)
        }
        worker_outcome = speech_worker.as_mut() => {
            PipelineSend::WorkerFinished(worker_outcome)
        }
        result = phrases.send(segment) => {
            if result.is_ok() {
                PipelineSend::Sent
            } else {
                PipelineSend::Closed
            }
        }
    }
}

async fn stop_pipeline<F>(
    phrase_sender: &mut Option<mpsc::Sender<SpeechSegment>>,
    language_model_cancellation: &CancellationToken,
    work_cancellation: &CancellationToken,
    deltas: &mut mpsc::Receiver<Result<String, AdapterError>>,
    mut speech_worker: Pin<&mut F>,
) -> SpeechWorkerOutcome
where
    F: Future<Output = SpeechWorkerOutcome>,
{
    work_cancellation.cancel();
    language_model_cancellation.cancel();
    phrase_sender.take();
    let (_, worker_outcome) = tokio::join!(drain_language_model(deltas), speech_worker.as_mut());
    worker_outcome
}

async fn cleanup_language_model(
    cancellation: &CancellationToken,
    deltas: &mut mpsc::Receiver<Result<String, AdapterError>>,
) {
    cancellation.cancel();
    drain_language_model(deltas).await;
}

async fn drain_language_model(deltas: &mut mpsc::Receiver<Result<String, AdapterError>>) {
    while deltas.recv().await.is_some() {}
}

fn terminal_from_worker(turn_id: TurnId, outcome: SpeechWorkerOutcome) -> RuntimeEvent {
    match outcome {
        SpeechWorkerOutcome::Completed => RuntimeEvent::TurnCompleted { turn_id },
        SpeechWorkerOutcome::Interrupted => RuntimeEvent::TurnCancelled { turn_id },
        SpeechWorkerOutcome::Stopped => {
            runtime_failure(turn_id, "speech worker stopped before pipeline completion")
        }
        SpeechWorkerOutcome::Failed { stage, error } => adapter_failure(turn_id, stage, error),
        SpeechWorkerOutcome::EventStreamClosed => {
            runtime_failure(turn_id, "speech worker event stream closed")
        }
    }
}

fn timing_event(
    turn_id: TurnId,
    milestone: RuntimeTimingMilestone,
    started_at: Instant,
) -> RuntimeEvent {
    RuntimeEvent::Timing {
        turn_id,
        milestone,
        elapsed_ms: elapsed_milliseconds(started_at),
    }
}

fn elapsed_milliseconds(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn adapter_failure(turn_id: TurnId, stage: RuntimeStage, error: AdapterError) -> RuntimeEvent {
    RuntimeEvent::TurnFailed {
        turn_id,
        error: RuntimeError::new(RuntimeErrorKind::Adapter, stage, error.message()),
    }
}

fn runtime_failure(turn_id: TurnId, message: impl Into<String>) -> RuntimeEvent {
    RuntimeEvent::TurnFailed {
        turn_id,
        error: RuntimeError::new(
            RuntimeErrorKind::InvalidState,
            RuntimeStage::Runtime,
            message,
        ),
    }
}

enum InitialSend {
    Sent,
    Interrupted,
    Closed,
}

enum PipelineSend {
    Sent,
    Interrupted,
    WorkerFinished(SpeechWorkerOutcome),
    Closed,
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
