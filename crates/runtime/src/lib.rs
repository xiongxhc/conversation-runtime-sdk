use std::panic::{catch_unwind, AssertUnwindSafe};
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
use tokio::task::{JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

mod conversation_quality;
mod generation;
mod phrase_chunker;
mod session_clock;
mod speech_text;
mod speech_worker;
mod streaming_turn;
mod turn_finalizer;
mod utterance_assembler;
mod voice_privacy;
mod voice_session;

pub use conversation_quality::{ConversationQualityController, ResolvedConversationQuality};
pub use phrase_chunker::PhraseChunkingConfig;
pub use session_clock::{SessionClock, TurnFinalizationDeadline};
pub use streaming_turn::{StreamingTurnEventStream, StreamingTurnRuntime};
pub use turn_finalizer::{FinalizedTranscript, TurnFinalizer};
pub use utterance_assembler::UtteranceAssembler;
pub use voice_privacy::validate_voice_policy;
pub use voice_session::{VoiceSessionAdapters, VoiceSessionEventStream, VoiceSessionRuntime};

use phrase_chunker::PhraseChunker;
use speech_text::normalize_speech_text;
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
    let mut speech_worker = tokio::spawn(
        SpeechWorker::new(SpeechWorkerContext {
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
        .run(),
    );

    let language_model_cancellation = work_cancellation.child_token();
    let deltas = catch_unwind(AssertUnwindSafe(|| {
        language_model.stream(
            LanguageModelRequest::new(turn_id, transcript),
            language_model_cancellation.clone(),
        )
    }));
    let mut deltas = match deltas {
        Ok(deltas) => deltas,
        Err(_) => {
            work_cancellation.cancel();
            phrase_sender.take();
            let _ = (&mut speech_worker).await;
            return adapter_failure(
                turn_id,
                RuntimeStage::LanguageModel,
                AdapterError::new("language model adapter panicked"),
            );
        }
    };
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
                    &mut speech_worker,
                ).await;
                return RuntimeEvent::TurnCancelled { turn_id };
            }
            _ = events.closed() => {
                stop_pipeline(
                    &mut phrase_sender,
                    &language_model_cancellation,
                    &work_cancellation,
                    &mut deltas,
                    &mut speech_worker,
                ).await;
                return runtime_failure(turn_id, "turn event stream closed during generation");
            }
            _ = work_cancellation.cancelled() => {
                let worker_outcome = (&mut speech_worker).await;
                phrase_sender.take();
                cleanup_language_model(&language_model_cancellation, &mut deltas).await;
                return terminal_from_worker_result(turn_id, worker_outcome);
            }
            worker_outcome = &mut speech_worker => {
                work_cancellation.cancel();
                phrase_sender.take();
                cleanup_language_model(&language_model_cancellation, &mut deltas).await;
                return terminal_from_worker_result(turn_id, worker_outcome);
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
                        &mut speech_worker,
                    )
                    .await;
                    return terminal;
                }
                response_bytes += delta.len();

                let send_result = if emitted_first_text_timing {
                    send_event_while_worker_active(
                        events,
                        &event_gate,
                        RuntimeEvent::TextDelta {
                            turn_id,
                            delta: delta.clone(),
                        },
                        &external_interruption,
                        &work_cancellation,
                        &mut speech_worker,
                    )
                    .await
                } else {
                    let delta_for_event = delta.clone();
                    send_required_pair_while_worker_active(
                        events,
                        &event_gate,
                        || {
                            [
                                timing_event(
                                    turn_id,
                                    RuntimeTimingMilestone::FirstTextDelta,
                                    started_at,
                                ),
                                RuntimeEvent::TextDelta {
                                    turn_id,
                                    delta: delta_for_event,
                                },
                            ]
                        },
                        &external_interruption,
                        &work_cancellation,
                        &mut speech_worker,
                    )
                    .await
                };
                match send_result {
                    PipelineSend::Sent => {}
                    PipelineSend::Interrupted => {
                        stop_pipeline(
                            &mut phrase_sender,
                            &language_model_cancellation,
                            &work_cancellation,
                            &mut deltas,
                            &mut speech_worker,
                        )
                        .await;
                        return RuntimeEvent::TurnCancelled { turn_id };
                    }
                    PipelineSend::WorkerFinished(worker_outcome) => {
                        work_cancellation.cancel();
                        phrase_sender.take();
                        cleanup_language_model(&language_model_cancellation, &mut deltas).await;
                        return terminal_from_worker_result(turn_id, worker_outcome);
                    }
                    PipelineSend::Closed => {
                        stop_pipeline(
                            &mut phrase_sender,
                            &language_model_cancellation,
                            &work_cancellation,
                            &mut deltas,
                            &mut speech_worker,
                        )
                        .await;
                        return runtime_failure(
                            turn_id,
                            "turn event stream closed during generation",
                        );
                    }
                }
                emitted_first_text_timing = true;

                for phrase in chunker.push_delta(&delta) {
                    let Some(text) = normalize_speech_text(&phrase) else {
                        continue;
                    };
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
                        events,
                        &external_interruption,
                        &work_cancellation,
                        &mut speech_worker,
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
                                &mut speech_worker,
                            )
                            .await;
                            return RuntimeEvent::TurnCancelled { turn_id };
                        }
                        PipelineSend::WorkerFinished(worker_outcome) => {
                            work_cancellation.cancel();
                            phrase_sender.take();
                            cleanup_language_model(&language_model_cancellation, &mut deltas).await;
                            return terminal_from_worker_result(turn_id, worker_outcome);
                        }
                        PipelineSend::Closed => {
                            stop_pipeline(
                                &mut phrase_sender,
                                &language_model_cancellation,
                                &work_cancellation,
                                &mut deltas,
                                &mut speech_worker,
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
                    &mut speech_worker,
                )
                .await;
                return terminal;
            }
            None => break,
        }
    }

    if let Some(text) = chunker
        .finish()
        .and_then(|phrase| normalize_speech_text(&phrase))
    {
        let segment = SpeechSegment {
            index: segment_index,
            text,
        };
        match send_phrase_while_worker_active(
            phrase_sender
                .as_ref()
                .expect("phrase sender exists before final worker drain"),
            segment,
            events,
            &external_interruption,
            &work_cancellation,
            &mut speech_worker,
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
                    &mut speech_worker,
                )
                .await;
                return RuntimeEvent::TurnCancelled { turn_id };
            }
            PipelineSend::WorkerFinished(worker_outcome) => {
                work_cancellation.cancel();
                phrase_sender.take();
                cleanup_language_model(&language_model_cancellation, &mut deltas).await;
                return terminal_from_worker_result(turn_id, worker_outcome);
            }
            PipelineSend::Closed => {
                stop_pipeline(
                    &mut phrase_sender,
                    &language_model_cancellation,
                    &work_cancellation,
                    &mut deltas,
                    &mut speech_worker,
                )
                .await;
                return runtime_failure(turn_id, "speech phrase queue closed before final segment");
            }
        }
    }

    phrase_sender.take();
    let worker_outcome = tokio::select! {
        biased;
        _ = external_interruption.cancelled() => {
            stop_pipeline(
                &mut phrase_sender,
                &language_model_cancellation,
                &work_cancellation,
                &mut deltas,
                &mut speech_worker,
            ).await;
            return RuntimeEvent::TurnCancelled { turn_id };
        }
        _ = events.closed() => {
            stop_pipeline(
                &mut phrase_sender,
                &language_model_cancellation,
                &work_cancellation,
                &mut deltas,
                &mut speech_worker,
            ).await;
            return runtime_failure(turn_id, "turn event stream closed during speech drain");
        }
        worker_outcome = &mut speech_worker => worker_outcome,
    };
    terminal_from_worker_result(turn_id, worker_outcome)
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
        _ = events.closed() => {
            return InitialSend::Closed;
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
        _ = events.closed() => {
            return InitialSend::Closed;
        }
        result = events.send(event) => result,
    };
    if result.is_ok() {
        InitialSend::Sent
    } else {
        InitialSend::Closed
    }
}

async fn send_event_while_worker_active(
    events: &mpsc::Sender<RuntimeEvent>,
    event_gate: &Mutex<()>,
    event: RuntimeEvent,
    external_interruption: &CancellationToken,
    work_cancellation: &CancellationToken,
    speech_worker: &mut JoinHandle<SpeechWorkerOutcome>,
) -> PipelineSend {
    let _event_guard = tokio::select! {
        biased;
        _ = external_interruption.cancelled() => {
            return PipelineSend::Interrupted;
        }
        _ = work_cancellation.cancelled() => {
            return PipelineSend::WorkerFinished((&mut *speech_worker).await);
        }
        _ = events.closed() => {
            return PipelineSend::Closed;
        }
        worker_outcome = &mut *speech_worker => {
            return PipelineSend::WorkerFinished(worker_outcome);
        }
        event_guard = event_gate.lock() => event_guard,
    };

    let result = tokio::select! {
        biased;
        _ = external_interruption.cancelled() => {
            return PipelineSend::Interrupted;
        }
        _ = work_cancellation.cancelled() => {
            return PipelineSend::WorkerFinished((&mut *speech_worker).await);
        }
        _ = events.closed() => {
            return PipelineSend::Closed;
        }
        worker_outcome = &mut *speech_worker => {
            return PipelineSend::WorkerFinished(worker_outcome);
        }
        result = events.send(event) => result,
    };
    if result.is_err() {
        PipelineSend::Closed
    } else {
        PipelineSend::Sent
    }
}

async fn send_required_pair_while_worker_active(
    events: &mpsc::Sender<RuntimeEvent>,
    event_gate: &Mutex<()>,
    build_pair: impl FnOnce() -> [RuntimeEvent; 2],
    external_interruption: &CancellationToken,
    work_cancellation: &CancellationToken,
    speech_worker: &mut JoinHandle<SpeechWorkerOutcome>,
) -> PipelineSend {
    let _event_guard = tokio::select! {
        biased;
        _ = external_interruption.cancelled() => {
            return PipelineSend::Interrupted;
        }
        _ = work_cancellation.cancelled() => {
            return PipelineSend::WorkerFinished((&mut *speech_worker).await);
        }
        _ = events.closed() => {
            return PipelineSend::Closed;
        }
        worker_outcome = &mut *speech_worker => {
            return PipelineSend::WorkerFinished(worker_outcome);
        }
        event_guard = event_gate.lock() => event_guard,
    };

    let mut permits = tokio::select! {
        biased;
        _ = external_interruption.cancelled() => {
            return PipelineSend::Interrupted;
        }
        _ = work_cancellation.cancelled() => {
            return PipelineSend::WorkerFinished((&mut *speech_worker).await);
        }
        _ = events.closed() => {
            return PipelineSend::Closed;
        }
        worker_outcome = &mut *speech_worker => {
            return PipelineSend::WorkerFinished(worker_outcome);
        }
        permits = events.reserve_many(2) => {
            match permits {
                Ok(permits) => permits,
                Err(_) => return PipelineSend::Closed,
            }
        }
    };

    let [first, second] = build_pair();
    permits
        .next()
        .expect("two event permits were reserved")
        .send(first);
    permits
        .next()
        .expect("two event permits were reserved")
        .send(second);
    PipelineSend::Sent
}

async fn send_phrase_while_worker_active(
    phrases: &mpsc::Sender<SpeechSegment>,
    segment: SpeechSegment,
    events: &mpsc::Sender<RuntimeEvent>,
    external_interruption: &CancellationToken,
    work_cancellation: &CancellationToken,
    speech_worker: &mut JoinHandle<SpeechWorkerOutcome>,
) -> PipelineSend {
    tokio::select! {
        biased;
        _ = external_interruption.cancelled() => PipelineSend::Interrupted,
        _ = work_cancellation.cancelled() => {
            PipelineSend::WorkerFinished((&mut *speech_worker).await)
        }
        _ = events.closed() => {
            PipelineSend::Closed
        }
        worker_outcome = &mut *speech_worker => {
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

async fn stop_pipeline(
    phrase_sender: &mut Option<mpsc::Sender<SpeechSegment>>,
    language_model_cancellation: &CancellationToken,
    work_cancellation: &CancellationToken,
    deltas: &mut mpsc::Receiver<Result<String, AdapterError>>,
    speech_worker: &mut JoinHandle<SpeechWorkerOutcome>,
) {
    work_cancellation.cancel();
    language_model_cancellation.cancel();
    phrase_sender.take();
    let (_, _) = tokio::join!(drain_language_model(deltas), &mut *speech_worker);
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

fn terminal_from_worker_result(
    turn_id: TurnId,
    outcome: Result<SpeechWorkerOutcome, JoinError>,
) -> RuntimeEvent {
    let Ok(outcome) = outcome else {
        return runtime_failure(turn_id, "speech worker task failed");
    };

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
    WorkerFinished(Result<SpeechWorkerOutcome, JoinError>),
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
    use std::future::{pending, ready};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use conversation_protocol::{
        RuntimeErrorKind, RuntimeEvent, RuntimeStage, RuntimeTimingMilestone, TurnId,
    };
    use tokio::sync::{mpsc, oneshot, Mutex};
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::{
        send_required_pair_while_worker_active, terminal_from_worker_result, PipelineSend,
        SpeechWorkerOutcome, TurnEventStream,
    };

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

    #[tokio::test(flavor = "current_thread")]
    async fn first_text_pair_waits_for_two_slots_before_sampling_or_publication() {
        let (event_sender, mut event_receiver) = mpsc::channel(2);
        let turn_id = TurnId::new(2);
        event_sender
            .send(RuntimeEvent::TurnStarted { turn_id })
            .await
            .unwrap();
        let event_gate = Mutex::new(());
        let external_interruption = CancellationToken::new();
        let work_cancellation = CancellationToken::new();
        let timing_sampled = Arc::new(AtomicBool::new(false));
        let timing_sampled_for_pair = Arc::clone(&timing_sampled);
        let build_pair = move || {
            timing_sampled_for_pair.store(true, Ordering::Release);
            [
                RuntimeEvent::Timing {
                    turn_id,
                    milestone: RuntimeTimingMilestone::FirstTextDelta,
                    elapsed_ms: 1,
                },
                RuntimeEvent::TextDelta {
                    turn_id,
                    delta: "hello".into(),
                },
            ]
        };
        let mut speech_worker = tokio::spawn(pending::<SpeechWorkerOutcome>());
        let send_pair = send_required_pair_while_worker_active(
            &event_sender,
            &event_gate,
            build_pair,
            &external_interruption,
            &work_cancellation,
            &mut speech_worker,
        );
        tokio::pin!(send_pair);

        tokio::select! {
            biased;
            result = &mut send_pair => {
                panic!("pair send resolved before two slots were available: {}", matches!(result, PipelineSend::Sent));
            }
            _ = ready(()) => {}
        }

        assert!(
            !timing_sampled.load(Ordering::Acquire),
            "timing was sampled before two event slots were reserved"
        );
        external_interruption.cancel();
        assert!(matches!(
            send_pair.as_mut().await,
            PipelineSend::Interrupted
        ));
        assert_eq!(
            event_receiver.recv().await,
            Some(RuntimeEvent::TurnStarted { turn_id })
        );
        assert!(
            event_receiver.try_recv().is_err(),
            "interruption exposed half of the required event pair"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn first_text_pair_is_not_sampled_or_published_when_saturated() {
        let (event_sender, mut event_receiver) = mpsc::channel(2);
        let turn_id = TurnId::new(4);
        event_sender
            .send(RuntimeEvent::TurnStarted { turn_id })
            .await
            .unwrap();
        event_sender
            .send(RuntimeEvent::TranscriptFinal {
                turn_id,
                text: "prefill".into(),
            })
            .await
            .unwrap();
        let event_gate = Mutex::new(());
        let external_interruption = CancellationToken::new();
        let work_cancellation = CancellationToken::new();
        let timing_sampled = Arc::new(AtomicBool::new(false));
        let timing_sampled_for_pair = Arc::clone(&timing_sampled);
        let mut speech_worker = tokio::spawn(pending::<SpeechWorkerOutcome>());
        let send_pair = send_required_pair_while_worker_active(
            &event_sender,
            &event_gate,
            move || {
                timing_sampled_for_pair.store(true, Ordering::Release);
                [
                    RuntimeEvent::Timing {
                        turn_id,
                        milestone: RuntimeTimingMilestone::FirstTextDelta,
                        elapsed_ms: 1,
                    },
                    RuntimeEvent::TextDelta {
                        turn_id,
                        delta: "hello".into(),
                    },
                ]
            },
            &external_interruption,
            &work_cancellation,
            &mut speech_worker,
        );
        tokio::pin!(send_pair);

        tokio::select! {
            biased;
            result = &mut send_pair => {
                panic!("saturated pair send resolved before interruption: {}", matches!(result, PipelineSend::Sent));
            }
            _ = ready(()) => {}
        }
        assert!(!timing_sampled.load(Ordering::Acquire));

        external_interruption.cancel();
        assert!(matches!(
            send_pair.as_mut().await,
            PipelineSend::Interrupted
        ));
        assert_eq!(
            event_receiver.recv().await,
            Some(RuntimeEvent::TurnStarted { turn_id })
        );
        assert_eq!(
            event_receiver.recv().await,
            Some(RuntimeEvent::TranscriptFinal {
                turn_id,
                text: "prefill".into(),
            })
        );
        assert!(event_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn unexpected_speech_worker_join_error_is_a_runtime_failure() {
        let turn_id = TurnId::new(3);
        let worker_error = tokio::spawn(async {
            panic!("unexpected speech worker panic");
        })
        .await
        .unwrap_err();

        let terminal = terminal_from_worker_result(turn_id, Err(worker_error));

        let RuntimeEvent::TurnFailed {
            turn_id: failed_turn_id,
            error,
        } = terminal
        else {
            panic!("unexpected terminal: {terminal:?}");
        };
        assert_eq!(failed_turn_id, turn_id);
        assert_eq!(error.kind(), RuntimeErrorKind::InvalidState);
        assert_eq!(error.stage(), RuntimeStage::Runtime);
        assert_eq!(error.message(), "speech worker task failed");
    }
}
