use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use conversation_memory::{MemoryContextProvider, MemoryStoreError, MemoryStoreErrorKind};
use conversation_model_adapters::{
    AdapterError, AudioFrame, ContinuousAudioOutput, GenerationLanguageModel,
    GenerationLanguageRequest, GenerationTextDelta, LanguageModelInput, PcmFormat,
    StreamingSpeechRequest, StreamingSpeechSynthesizer,
};
use conversation_protocol::{
    GenerationId, PlaybackState, RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStage,
    RuntimeTimingMilestone, TurnId, UtteranceId,
};
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use tokio::task::{AbortHandle, JoinError, JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::generation::GenerationGuard;
use crate::speech_text::normalize_speech_text;
use crate::{
    ConversationContext, ConversationTurnIdentity, ConversationTurnSource, UtteranceAssembler,
};

const EVENT_BUFFER_SIZE: usize = 32;
const UTTERANCE_QUEUE_CAPACITY: usize = 2;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024;

pub struct StreamingTurnEventStream {
    events: mpsc::Receiver<RuntimeEvent>,
    terminal: Option<oneshot::Receiver<RuntimeEvent>>,
    events_closed: bool,
    task: Option<JoinHandle<()>>,
    tasks: TurnTaskGroup,
}

pub struct StartedStreamingTurn {
    identity: ConversationTurnIdentity,
    events: StreamingTurnEventStream,
}

impl StartedStreamingTurn {
    pub const fn identity(&self) -> ConversationTurnIdentity {
        self.identity
    }

    pub fn into_events(self) -> StreamingTurnEventStream {
        self.events
    }
}

impl Deref for StartedStreamingTurn {
    type Target = StreamingTurnEventStream;

    fn deref(&self) -> &Self::Target {
        &self.events
    }
}

impl DerefMut for StartedStreamingTurn {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.events
    }
}

impl StreamingTurnEventStream {
    pub async fn recv(&mut self) -> Option<RuntimeEvent> {
        if !self.events_closed {
            if let Some(event) = self.events.recv().await {
                return Some(event);
            }
            self.events_closed = true;
        }

        let terminal_event = self.terminal.as_mut()?.await;
        self.terminal = None;
        self.join_task().await;
        terminal_event.ok()
    }

    async fn join_task(&mut self) {
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    async fn abort_and_reap(&mut self) -> Result<(), RuntimeError> {
        self.tasks.abort_and_wait().await;
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        match task.await {
            Ok(()) => Ok(()),
            Err(error) if error.is_cancelled() => Ok(()),
            Err(_) => Err(runtime_error("streaming turn task failed during abort")),
        }
    }
}

#[derive(Clone, Default)]
struct TurnTaskGroup {
    inner: Arc<TurnTaskGroupInner>,
}

#[derive(Default)]
struct TurnTaskGroupInner {
    aborts: StdMutex<Vec<AbortHandle>>,
    active: AtomicUsize,
    finished: Notify,
}

impl TurnTaskGroup {
    fn spawn<T>(&self, future: impl Future<Output = T> + Send + 'static) -> JoinHandle<T>
    where
        T: Send + 'static,
    {
        self.inner.active.fetch_add(1, Ordering::AcqRel);
        let completion = TurnTaskCompletion {
            group: self.clone(),
        };
        let task = tokio::spawn(async move {
            let _completion = completion;
            future.await
        });
        self.inner
            .aborts
            .lock()
            .expect("turn task abort lock poisoned")
            .push(task.abort_handle());
        task
    }

    async fn abort_and_wait(&self) {
        loop {
            let finished = self.inner.finished.notified();
            self.abort_all();
            if self.inner.active.load(Ordering::Acquire) == 0 {
                self.inner
                    .aborts
                    .lock()
                    .expect("turn task abort lock poisoned")
                    .clear();
                return;
            }
            finished.await;
        }
    }

    fn abort_all(&self) {
        for task in self
            .inner
            .aborts
            .lock()
            .expect("turn task abort lock poisoned")
            .iter()
        {
            task.abort();
        }
    }
}

struct TurnTaskCompletion {
    group: TurnTaskGroup,
}

impl Drop for TurnTaskCompletion {
    fn drop(&mut self) {
        self.group.inner.active.fetch_sub(1, Ordering::AcqRel);
        self.group.inner.finished.notify_one();
    }
}

#[derive(Clone)]
pub struct StreamingTurnRuntime {
    context: ConversationContext,
    language_model: Arc<dyn GenerationLanguageModel>,
    speech_synthesizer: Arc<dyn StreamingSpeechSynthesizer>,
    audio_output: Arc<dyn ContinuousAudioOutput>,
    active: Arc<Mutex<Option<ActiveGeneration>>>,
    generation_guard: GenerationGuard,
}

#[derive(Clone)]
struct ActiveGeneration {
    identity: ConversationTurnIdentity,
    turn_id: TurnId,
    generation_id: GenerationId,
    session_id: Option<conversation_protocol::SessionId>,
    external_interruption: CancellationToken,
    work_cancellation: CancellationToken,
}

impl StreamingTurnRuntime {
    pub fn new(
        context: ConversationContext,
        language_model: Arc<dyn GenerationLanguageModel>,
        speech_synthesizer: Arc<dyn StreamingSpeechSynthesizer>,
        audio_output: Arc<dyn ContinuousAudioOutput>,
    ) -> Self {
        Self {
            context,
            language_model,
            speech_synthesizer,
            audio_output,
            active: Arc::new(Mutex::new(None)),
            generation_guard: GenerationGuard::default(),
        }
    }

    pub async fn start_turn(
        &self,
        source: ConversationTurnSource,
        transcript: impl Into<String>,
    ) -> Result<StartedStreamingTurn, RuntimeError> {
        let prepared = self.context.begin_turn(source, transcript).await?;
        let identity = prepared.identity();
        let turn_id = identity.turn_id();
        let generation_id = identity.generation_id();
        if !self.generation_guard.activate(turn_id, generation_id).await {
            let _ = self.context.discard_turn(identity, false).await;
            return Err(runtime_error("a streaming generation is still active"));
        }

        let language_input = match LanguageModelInput::with_quality(
            prepared.transcript(),
            prepared.resolved().history_messages().iter().cloned(),
            prepared.resolved().decision().clone(),
            prepared.resolved().system_guidance(),
        ) {
            Ok(input) => input,
            Err(error) => {
                let _ = self.context.discard_turn(identity, false).await;
                self.generation_guard
                    .deactivate(turn_id, generation_id)
                    .await;
                return Err(adapter_runtime_error(RuntimeStage::LanguageModel, error));
            }
        };

        let external_interruption = CancellationToken::new();
        let work_cancellation = CancellationToken::new();
        {
            let mut active = self.active.lock().await;
            if active.is_some() {
                drop(active);
                let _ = self.context.discard_turn(identity, false).await;
                self.generation_guard
                    .deactivate(turn_id, generation_id)
                    .await;
                return Err(runtime_error("a streaming generation is still active"));
            }
            *active = Some(ActiveGeneration {
                identity,
                turn_id,
                generation_id,
                session_id: match source {
                    ConversationTurnSource::Text => None,
                    ConversationTurnSource::Voice { session_id } => Some(session_id),
                },
                external_interruption: external_interruption.clone(),
                work_cancellation: work_cancellation.clone(),
            });
        }

        let (event_sender, event_receiver) = mpsc::channel(EVENT_BUFFER_SIZE);
        let (terminal_sender, terminal_receiver) = oneshot::channel();
        if event_sender
            .send(RuntimeEvent::TurnStarted { turn_id })
            .await
            .is_err()
        {
            let _ = self.context.discard_turn(identity, false).await;
            self.generation_guard
                .deactivate(turn_id, generation_id)
                .await;
            *self.active.lock().await = None;
            return Err(runtime_error(
                "streaming turn event stream closed before start",
            ));
        }
        let tasks = TurnTaskGroup::default();
        let task = StreamingTurnTask {
            turn_id,
            generation_id,
            language_input,
            language_model: Arc::clone(&self.language_model),
            speech_synthesizer: Arc::clone(&self.speech_synthesizer),
            audio_output: Arc::clone(&self.audio_output),
            generation_guard: self.generation_guard.clone(),
            started_at: Instant::now(),
            external_interruption: external_interruption.clone(),
            work_cancellation,
            tasks: tasks.clone(),
            generated_text: Arc::new(StdMutex::new(String::new())),
            memory_provider: self.context.memory_provider(),
        };
        let active = Arc::clone(&self.active);
        let context = self.context.clone();
        let generation_guard = self.generation_guard.clone();
        let terminal_interruption = external_interruption;
        let generated_text = Arc::clone(&task.generated_text);

        let task = tasks.spawn(async move {
            let mut terminal_event = run_streaming_turn(task, &event_sender).await;

            if terminal_interruption.is_cancelled() {
                terminal_event = RuntimeEvent::TurnCancelled { turn_id };
            }

            let generated_text = generated_text
                .lock()
                .expect("generated text lock poisoned")
                .clone();
            let context_result = match &terminal_event {
                RuntimeEvent::TurnCompleted { .. } => {
                    if generated_text.trim().is_empty() {
                        context.discard_turn(identity, false).await
                    } else {
                        context
                            .complete_turn(identity, generated_text.clone())
                            .await
                    }
                }
                RuntimeEvent::TurnCancelled { .. } => context.discard_turn(identity, true).await,
                RuntimeEvent::TurnFailed { .. } => context.discard_turn(identity, false).await,
                _ => Err(runtime_error(
                    "streaming turn produced a nonterminal result",
                )),
            };
            if let Err(error) = context_result {
                terminal_event = RuntimeEvent::TurnFailed { turn_id, error };
            }

            if matches!(terminal_event, RuntimeEvent::TurnCompleted { .. }) {
                let _ = event_sender
                    .send(RuntimeEvent::TextCompleted {
                        turn_id,
                        text: generated_text,
                    })
                    .await;
            }
            drop(event_sender);
            generation_guard.deactivate(turn_id, generation_id).await;

            let mut active = active.lock().await;
            if active.as_ref().is_some_and(|current| {
                current.turn_id == turn_id && current.generation_id == generation_id
            }) {
                *active = None;
            }
            drop(active);

            let _ = terminal_sender.send(terminal_event);
        });

        Ok(StartedStreamingTurn {
            identity,
            events: StreamingTurnEventStream {
                events: event_receiver,
                terminal: Some(terminal_receiver),
                events_closed: false,
                task: Some(task),
                tasks,
            },
        })
    }

    pub async fn interrupt(
        &self,
        turn_id: TurnId,
        generation_id: GenerationId,
    ) -> Result<(), RuntimeError> {
        let active = {
            let active = self.active.lock().await;
            match active.as_ref() {
                Some(active)
                    if active.turn_id == turn_id && active.generation_id == generation_id =>
                {
                    active.clone()
                }
                Some(active) => {
                    return Err(runtime_error(format!(
                        "turn {} generation {} is active, not turn {} generation {}",
                        active.turn_id, active.generation_id, turn_id, generation_id
                    )));
                }
                None => return Err(runtime_error("there is no active streaming generation")),
            }
        };

        active.external_interruption.cancel();
        active.work_cancellation.cancel();
        if let Some(session_id) = active.session_id {
            self.audio_output
                .flush(session_id, generation_id)
                .await
                .map_err(|error| {
                    adapter_runtime_error(RuntimeStage::ContinuousAudioOutput, error)
                })?;
        }
        Ok(())
    }

    pub(crate) async fn abort_turn(
        &self,
        turn_id: TurnId,
        generation_id: GenerationId,
        events: &mut StreamingTurnEventStream,
    ) -> Result<(), RuntimeError> {
        events.abort_and_reap().await?;
        let identity = {
            let active = self.active.lock().await;
            active.as_ref().and_then(|current| {
                (current.turn_id == turn_id && current.generation_id == generation_id)
                    .then_some(current.identity)
            })
        };
        // The reaped task may have finalized the context before it could clear
        // the active slot; skip the discard for that benign race, and keep
        // deactivation and the slot clear ahead of any discard error.
        let discard_result = match identity {
            Some(identity) if self.context.active_turn().await == Some(identity) => {
                self.context.discard_turn(identity, true).await
            }
            _ => Ok(()),
        };
        self.generation_guard
            .deactivate(turn_id, generation_id)
            .await;

        let mut active = self.active.lock().await;
        if active.as_ref().is_some_and(|current| {
            current.turn_id == turn_id && current.generation_id == generation_id
        }) {
            *active = None;
        }
        discard_result
    }
}

struct StreamingTurnTask {
    turn_id: TurnId,
    generation_id: GenerationId,
    language_input: LanguageModelInput,
    language_model: Arc<dyn GenerationLanguageModel>,
    speech_synthesizer: Arc<dyn StreamingSpeechSynthesizer>,
    audio_output: Arc<dyn ContinuousAudioOutput>,
    generation_guard: GenerationGuard,
    started_at: Instant,
    external_interruption: CancellationToken,
    work_cancellation: CancellationToken,
    tasks: TurnTaskGroup,
    generated_text: Arc<StdMutex<String>>,
    memory_provider: Option<Arc<dyn MemoryContextProvider>>,
}

#[derive(Debug)]
struct QueuedUtterance {
    utterance_id: UtteranceId,
    text: String,
}

async fn run_streaming_turn(
    task: StreamingTurnTask,
    events: &mpsc::Sender<RuntimeEvent>,
) -> RuntimeEvent {
    let StreamingTurnTask {
        turn_id,
        generation_id,
        mut language_input,
        language_model,
        speech_synthesizer,
        audio_output,
        generation_guard,
        started_at,
        external_interruption,
        work_cancellation,
        tasks,
        generated_text,
        memory_provider,
    } = task;

    match send_event(
        events,
        RuntimeEvent::TranscriptFinal {
            turn_id,
            text: language_input.transcript().to_owned(),
        },
        &external_interruption,
        &work_cancellation,
    )
    .await
    {
        SendOutcome::Sent => {}
        SendOutcome::Interrupted => return RuntimeEvent::TurnCancelled { turn_id },
        SendOutcome::Closed => {
            return runtime_failure(turn_id, "streaming event consumer closed during transcript");
        }
        SendOutcome::Stale => {
            return runtime_failure(
                turn_id,
                "streaming generation became stale during transcript",
            );
        }
    }

    let decision = language_input
        .quality_decision()
        .expect("streaming turns always carry a quality decision")
        .clone();
    match send_event(
        events,
        RuntimeEvent::QualityResolved { decision },
        &external_interruption,
        &work_cancellation,
    )
    .await
    {
        SendOutcome::Sent => {}
        SendOutcome::Interrupted => return RuntimeEvent::TurnCancelled { turn_id },
        SendOutcome::Closed => {
            return runtime_failure(
                turn_id,
                "streaming event consumer closed during quality resolution",
            );
        }
        SendOutcome::Stale => {
            return runtime_failure(
                turn_id,
                "streaming generation became stale during quality resolution",
            );
        }
    }

    if let Some(memory_provider) = memory_provider {
        let memory_cancellation = work_cancellation.child_token();
        let mut retrieval = memory_provider.retrieve(
            turn_id,
            language_input.transcript().to_owned(),
            memory_cancellation.clone(),
        );
        let retrieval = tokio::select! {
            biased;
            _ = external_interruption.cancelled() => {
                memory_cancellation.cancel();
                let _ = retrieval.await;
                return RuntimeEvent::TurnCancelled { turn_id };
            }
            _ = work_cancellation.cancelled() => {
                memory_cancellation.cancel();
                let _ = retrieval.await;
                return RuntimeEvent::TurnCancelled { turn_id };
            }
            _ = events.closed() => {
                memory_cancellation.cancel();
                let _ = retrieval.await;
                return runtime_failure(
                    turn_id,
                    "streaming event consumer closed during memory retrieval",
                );
            }
            result = &mut retrieval => result,
        };
        let retrieval = match retrieval {
            Ok(retrieval) => retrieval,
            Err(_) if external_interruption.is_cancelled() || work_cancellation.is_cancelled() => {
                return RuntimeEvent::TurnCancelled { turn_id };
            }
            Err(error) => return memory_failure(turn_id, error),
        };
        language_input = match language_input.with_memory_items(retrieval.items().iter().cloned()) {
            Ok(input) => input,
            Err(error) => return adapter_failure(turn_id, RuntimeStage::Memory, error),
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
            SendOutcome::Sent => {}
            SendOutcome::Interrupted | SendOutcome::Stale => {
                return RuntimeEvent::TurnCancelled { turn_id };
            }
            SendOutcome::Closed => {
                return runtime_failure(
                    turn_id,
                    "streaming event consumer closed during memory publication",
                );
            }
        }
    }

    let (utterance_sender, utterance_receiver) = mpsc::channel(UTTERANCE_QUEUE_CAPACITY);
    let mut utterance_sender = Some(utterance_sender);
    let mut speech_worker = tasks.spawn(run_streaming_speech(StreamingSpeechTask {
        turn_id,
        generation_id,
        speech_synthesizer,
        audio_output,
        generation_guard: generation_guard.clone(),
        utterances: utterance_receiver,
        events: events.clone(),
        started_at,
        external_interruption: external_interruption.clone(),
        work_cancellation: work_cancellation.clone(),
    }));

    let language_cancellation = work_cancellation.child_token();
    let generation_request =
        match GenerationLanguageRequest::from_input(turn_id, generation_id, language_input) {
            Ok(request) => request,
            Err(error) => {
                work_cancellation.cancel();
                utterance_sender.take();
                let _ = (&mut speech_worker).await;
                return adapter_failure(turn_id, RuntimeStage::LanguageModel, error);
            }
        };
    let deltas = catch_unwind(AssertUnwindSafe(|| {
        language_model.stream(generation_request, language_cancellation.clone())
    }));
    let mut deltas = match deltas {
        Ok(deltas) => deltas,
        Err(_) => {
            work_cancellation.cancel();
            utterance_sender.take();
            let _ = (&mut speech_worker).await;
            return adapter_failure(
                turn_id,
                RuntimeStage::LanguageModel,
                AdapterError::new("generation language adapter panicked"),
            );
        }
    };
    let mut assembler = UtteranceAssembler::default();
    let mut response_bytes = 0_usize;
    let mut emitted_first_text = false;
    let mut next_utterance_id = 1_u64;

    loop {
        let item = tokio::select! {
            biased;
            _ = external_interruption.cancelled() => {
                stop_streaming_pipeline(
                    &mut utterance_sender,
                    &language_cancellation,
                    &work_cancellation,
                    &mut deltas,
                    &mut speech_worker,
                ).await;
                return RuntimeEvent::TurnCancelled { turn_id };
            }
            _ = events.closed() => {
                stop_streaming_pipeline(
                    &mut utterance_sender,
                    &language_cancellation,
                    &work_cancellation,
                    &mut deltas,
                    &mut speech_worker,
                ).await;
                return runtime_failure(
                    turn_id,
                    "streaming event consumer closed during generation",
                );
            }
            _ = work_cancellation.cancelled() => {
                let outcome = (&mut speech_worker).await;
                utterance_sender.take();
                cleanup_language_stream(&language_cancellation, &mut deltas).await;
                return terminal_from_speech(turn_id, outcome);
            }
            outcome = &mut speech_worker => {
                work_cancellation.cancel();
                utterance_sender.take();
                cleanup_language_stream(&language_cancellation, &mut deltas).await;
                return terminal_from_speech(turn_id, outcome);
            }
            item = deltas.recv() => item,
        };

        match item {
            Some(Ok(delta)) => {
                if delta.turn_id() != turn_id || delta.generation_id() != generation_id {
                    let terminal = adapter_failure(
                        turn_id,
                        RuntimeStage::LanguageModel,
                        AdapterError::new("generation language delta identity mismatch"),
                    );
                    stop_streaming_pipeline(
                        &mut utterance_sender,
                        &language_cancellation,
                        &work_cancellation,
                        &mut deltas,
                        &mut speech_worker,
                    )
                    .await;
                    return terminal;
                }
                if delta
                    .delta()
                    .len()
                    .gt(&DEFAULT_MAX_RESPONSE_BYTES.saturating_sub(response_bytes))
                {
                    let terminal = adapter_failure(
                        turn_id,
                        RuntimeStage::LanguageModel,
                        AdapterError::new(format!(
                            "generation language response exceeds the maximum size of {DEFAULT_MAX_RESPONSE_BYTES} bytes"
                        )),
                    );
                    stop_streaming_pipeline(
                        &mut utterance_sender,
                        &language_cancellation,
                        &work_cancellation,
                        &mut deltas,
                        &mut speech_worker,
                    )
                    .await;
                    return terminal;
                }
                response_bytes += delta.delta().len();

                let publication = publish_text_delta(
                    events,
                    &generation_guard,
                    &delta,
                    emitted_first_text,
                    started_at,
                    &external_interruption,
                    &work_cancellation,
                )
                .await;
                match publication {
                    SendOutcome::Sent => {
                        emitted_first_text = true;
                        generated_text
                            .lock()
                            .expect("generated text lock poisoned")
                            .push_str(delta.delta());
                    }
                    SendOutcome::Interrupted | SendOutcome::Stale => {
                        stop_streaming_pipeline(
                            &mut utterance_sender,
                            &language_cancellation,
                            &work_cancellation,
                            &mut deltas,
                            &mut speech_worker,
                        )
                        .await;
                        return RuntimeEvent::TurnCancelled { turn_id };
                    }
                    SendOutcome::Closed => {
                        stop_streaming_pipeline(
                            &mut utterance_sender,
                            &language_cancellation,
                            &work_cancellation,
                            &mut deltas,
                            &mut speech_worker,
                        )
                        .await;
                        return runtime_failure(
                            turn_id,
                            "streaming event consumer closed during text publication",
                        );
                    }
                }

                for utterance in assembler.push_delta(delta.delta()) {
                    if let Some(terminal) = queue_utterance(
                        &mut next_utterance_id,
                        utterance,
                        utterance_sender
                            .as_ref()
                            .expect("utterance sender exists while generation is active"),
                        events,
                        &external_interruption,
                        &work_cancellation,
                        &mut speech_worker,
                    )
                    .await
                    {
                        return finish_queue_outcome(
                            turn_id,
                            terminal,
                            &mut utterance_sender,
                            &language_cancellation,
                            &work_cancellation,
                            &mut deltas,
                            &mut speech_worker,
                        )
                        .await;
                    }
                }
            }
            Some(Err(error)) => {
                let terminal = adapter_failure(turn_id, RuntimeStage::LanguageModel, error);
                stop_streaming_pipeline(
                    &mut utterance_sender,
                    &language_cancellation,
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

    if let Some(utterance) = assembler.finish() {
        if let Some(outcome) = queue_utterance(
            &mut next_utterance_id,
            utterance,
            utterance_sender
                .as_ref()
                .expect("utterance sender exists before final speech drain"),
            events,
            &external_interruption,
            &work_cancellation,
            &mut speech_worker,
        )
        .await
        {
            return finish_queue_outcome(
                turn_id,
                outcome,
                &mut utterance_sender,
                &language_cancellation,
                &work_cancellation,
                &mut deltas,
                &mut speech_worker,
            )
            .await;
        }
    }

    utterance_sender.take();
    let outcome = tokio::select! {
        biased;
        _ = external_interruption.cancelled() => {
            work_cancellation.cancel();
            let outcome = (&mut speech_worker).await;
            return match outcome {
                Ok(SpeechOutcome::Failed { stage, error }) => adapter_failure(turn_id, stage, error),
                _ => RuntimeEvent::TurnCancelled { turn_id },
            };
        }
        _ = events.closed() => {
            work_cancellation.cancel();
            let _ = (&mut speech_worker).await;
            return runtime_failure(
                turn_id,
                "streaming event consumer closed during speech drain",
            );
        }
        outcome = &mut speech_worker => outcome,
    };
    terminal_from_speech(turn_id, outcome)
}

async fn publish_text_delta(
    events: &mpsc::Sender<RuntimeEvent>,
    generation_guard: &GenerationGuard,
    delta: &GenerationTextDelta,
    emitted_first_text: bool,
    started_at: Instant,
    external_interruption: &CancellationToken,
    work_cancellation: &CancellationToken,
) -> SendOutcome {
    let permit_count = if emitted_first_text { 1 } else { 2 };
    let mut permits = tokio::select! {
        biased;
        _ = external_interruption.cancelled() => return SendOutcome::Interrupted,
        _ = work_cancellation.cancelled() => return SendOutcome::Interrupted,
        _ = events.closed() => return SendOutcome::Closed,
        permits = events.reserve_many(permit_count) => match permits {
            Ok(permits) => permits,
            Err(_) => return SendOutcome::Closed,
        },
    };
    let generation_permit = tokio::select! {
        biased;
        _ = external_interruption.cancelled() => return SendOutcome::Interrupted,
        _ = work_cancellation.cancelled() => return SendOutcome::Interrupted,
        permit = generation_guard.permit(delta.turn_id(), delta.generation_id()) => permit,
    };
    let Some(_generation_permit) = generation_permit else {
        return SendOutcome::Stale;
    };

    if !emitted_first_text {
        permits
            .next()
            .expect("first text publication reserved two permits")
            .send(timing_event(
                delta.turn_id(),
                RuntimeTimingMilestone::FirstTextDelta,
                started_at,
            ));
    }
    permits
        .next()
        .expect("text publication reserved its event permit")
        .send(RuntimeEvent::TextDelta {
            turn_id: delta.turn_id(),
            delta: delta.delta().to_owned(),
        });
    SendOutcome::Sent
}

async fn queue_utterance(
    next_utterance_id: &mut u64,
    selected_text: String,
    utterances: &mpsc::Sender<QueuedUtterance>,
    events: &mpsc::Sender<RuntimeEvent>,
    external_interruption: &CancellationToken,
    work_cancellation: &CancellationToken,
    speech_worker: &mut JoinHandle<SpeechOutcome>,
) -> Option<QueueOutcome> {
    let text = normalize_speech_text(&selected_text)?;
    let utterance_id = UtteranceId::new(*next_utterance_id);
    let Some(incremented) = next_utterance_id.checked_add(1) else {
        return Some(QueueOutcome::Failed(AdapterError::new(
            "utterance identifier overflowed",
        )));
    };
    *next_utterance_id = incremented;
    let utterance = QueuedUtterance { utterance_id, text };

    tokio::select! {
        biased;
        _ = external_interruption.cancelled() => Some(QueueOutcome::Interrupted),
        _ = work_cancellation.cancelled() => {
            Some(QueueOutcome::WorkerFinished((&mut *speech_worker).await))
        }
        _ = events.closed() => Some(QueueOutcome::Closed),
        outcome = &mut *speech_worker => Some(QueueOutcome::WorkerFinished(outcome)),
        result = utterances.send(utterance) => {
            result.err().map(|_| QueueOutcome::Closed)
        }
    }
}

struct StreamingSpeechTask {
    turn_id: TurnId,
    generation_id: GenerationId,
    speech_synthesizer: Arc<dyn StreamingSpeechSynthesizer>,
    audio_output: Arc<dyn ContinuousAudioOutput>,
    generation_guard: GenerationGuard,
    utterances: mpsc::Receiver<QueuedUtterance>,
    events: mpsc::Sender<RuntimeEvent>,
    started_at: Instant,
    external_interruption: CancellationToken,
    work_cancellation: CancellationToken,
}

async fn run_streaming_speech(task: StreamingSpeechTask) -> SpeechOutcome {
    let StreamingSpeechTask {
        turn_id,
        generation_id,
        speech_synthesizer,
        audio_output,
        generation_guard,
        mut utterances,
        events,
        started_at,
        external_interruption,
        work_cancellation,
    } = task;
    let mut emitted_speech_started = false;
    let mut emitted_first_playable = false;
    let mut emitted_first_accepted = false;
    let mut negotiated_format = None;

    loop {
        let utterance = tokio::select! {
            biased;
            _ = external_interruption.cancelled() => return SpeechOutcome::Interrupted,
            _ = work_cancellation.cancelled() => return SpeechOutcome::Interrupted,
            _ = events.closed() => return SpeechOutcome::EventStreamClosed,
            utterance = utterances.recv() => utterance,
        };
        let Some(utterance) = utterance else {
            break;
        };

        if !emitted_speech_started {
            match send_event_pair(
                &events,
                [
                    RuntimeEvent::SpeechStarted { turn_id },
                    timing_event(
                        turn_id,
                        RuntimeTimingMilestone::FirstSynthesisRequest,
                        started_at,
                    ),
                ],
                &external_interruption,
                &work_cancellation,
            )
            .await
            {
                SendOutcome::Sent => emitted_speech_started = true,
                SendOutcome::Interrupted | SendOutcome::Stale => {
                    return SpeechOutcome::Interrupted;
                }
                SendOutcome::Closed => return SpeechOutcome::EventStreamClosed,
            }
        }

        let speech_cancellation = work_cancellation.child_token();
        let frames = catch_unwind(AssertUnwindSafe(|| {
            speech_synthesizer.stream(
                StreamingSpeechRequest::new(
                    turn_id,
                    generation_id,
                    utterance.utterance_id,
                    utterance.text,
                ),
                speech_cancellation.clone(),
            )
        }));
        let mut frames = match frames {
            Ok(frames) => frames,
            Err(_) => {
                return SpeechOutcome::Failed {
                    stage: RuntimeStage::SpeechSynthesizer,
                    error: AdapterError::new("streaming speech adapter panicked"),
                };
            }
        };
        let mut expected_sequence = 0_u64;
        let mut received_frame = false;

        loop {
            let item = tokio::select! {
                biased;
                _ = external_interruption.cancelled() => {
                    cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                    return SpeechOutcome::Interrupted;
                }
                _ = work_cancellation.cancelled() => {
                    cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                    return SpeechOutcome::Interrupted;
                }
                _ = events.closed() => {
                    cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                    return SpeechOutcome::EventStreamClosed;
                }
                item = frames.recv() => item,
            };

            let Some(item) = item else {
                break;
            };
            let frame = match item {
                Ok(frame) => frame,
                Err(error) => {
                    cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                    return SpeechOutcome::Failed {
                        stage: RuntimeStage::SpeechSynthesizer,
                        error,
                    };
                }
            };
            if let Err(error) = validate_frame(
                &frame,
                turn_id,
                generation_id,
                utterance.utterance_id,
                expected_sequence,
                negotiated_format,
            ) {
                cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                return SpeechOutcome::Failed {
                    stage: RuntimeStage::SpeechSynthesizer,
                    error,
                };
            }
            if negotiated_format.is_none() {
                negotiated_format = Some(frame.format());
            }
            expected_sequence = match frame.next_sequence() {
                Ok(sequence) => sequence,
                Err(error) => {
                    cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                    return SpeechOutcome::Failed {
                        stage: RuntimeStage::SpeechSynthesizer,
                        error,
                    };
                }
            };
            received_frame = true;

            if !emitted_first_playable {
                match send_event(
                    &events,
                    timing_event(
                        turn_id,
                        RuntimeTimingMilestone::FirstPlayableAudio,
                        started_at,
                    ),
                    &external_interruption,
                    &work_cancellation,
                )
                .await
                {
                    SendOutcome::Sent => emitted_first_playable = true,
                    SendOutcome::Interrupted | SendOutcome::Stale => {
                        cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                        return SpeechOutcome::Interrupted;
                    }
                    SendOutcome::Closed => {
                        cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                        return SpeechOutcome::EventStreamClosed;
                    }
                }
            }

            let generation_permit = tokio::select! {
                biased;
                _ = external_interruption.cancelled() => {
                    cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                    return SpeechOutcome::Interrupted;
                }
                _ = work_cancellation.cancelled() => {
                    cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                    return SpeechOutcome::Interrupted;
                }
                permit = generation_guard.permit(turn_id, generation_id) => permit,
            };
            let Some(_generation_permit) = generation_permit else {
                cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                return SpeechOutcome::Interrupted;
            };
            let enqueue_cancellation = work_cancellation.child_token();
            let enqueue = audio_output.enqueue(frame, enqueue_cancellation.clone());
            tokio::pin!(enqueue);
            let receipt = tokio::select! {
                biased;
                _ = external_interruption.cancelled() => {
                    enqueue_cancellation.cancel();
                    let _ = (&mut enqueue).await;
                    cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                    return SpeechOutcome::Interrupted;
                }
                _ = work_cancellation.cancelled() => {
                    enqueue_cancellation.cancel();
                    let _ = (&mut enqueue).await;
                    cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                    return SpeechOutcome::Interrupted;
                }
                _ = events.closed() => {
                    enqueue_cancellation.cancel();
                    let _ = (&mut enqueue).await;
                    cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                    return SpeechOutcome::EventStreamClosed;
                }
                receipt = &mut enqueue => receipt,
            };
            let receipt = match receipt {
                Ok(receipt) if receipt.generation_id() == generation_id => receipt,
                Ok(_) => {
                    cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                    return SpeechOutcome::Failed {
                        stage: RuntimeStage::ContinuousAudioOutput,
                        error: AdapterError::new("continuous audio receipt identity mismatch"),
                    };
                }
                Err(_)
                    if external_interruption.is_cancelled() || work_cancellation.is_cancelled() =>
                {
                    cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                    return SpeechOutcome::Interrupted;
                }
                Err(error) => {
                    cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                    return SpeechOutcome::Failed {
                        stage: RuntimeStage::ContinuousAudioOutput,
                        error,
                    };
                }
            };
            if receipt.state() != PlaybackState::Accepted {
                cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                return SpeechOutcome::Failed {
                    stage: RuntimeStage::ContinuousAudioOutput,
                    error: AdapterError::new(
                        "continuous audio enqueue did not return playback acceptance",
                    ),
                };
            }
            if !emitted_first_accepted {
                match send_event(
                    &events,
                    RuntimeEvent::Playback {
                        turn_id,
                        generation_id,
                        state: receipt.state(),
                    },
                    &external_interruption,
                    &work_cancellation,
                )
                .await
                {
                    SendOutcome::Sent => emitted_first_accepted = true,
                    SendOutcome::Interrupted | SendOutcome::Stale => {
                        cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                        return SpeechOutcome::Interrupted;
                    }
                    SendOutcome::Closed => {
                        cleanup_frame_stream(&speech_cancellation, &mut frames).await;
                        return SpeechOutcome::EventStreamClosed;
                    }
                }
            }
        }

        if !received_frame {
            return SpeechOutcome::Failed {
                stage: RuntimeStage::SpeechSynthesizer,
                error: AdapterError::new("streaming speech returned no audio frames"),
            };
        }
    }

    if emitted_speech_started {
        match send_event(
            &events,
            RuntimeEvent::SpeechCompleted { turn_id },
            &external_interruption,
            &work_cancellation,
        )
        .await
        {
            SendOutcome::Sent => {}
            SendOutcome::Interrupted | SendOutcome::Stale => {
                return SpeechOutcome::Interrupted;
            }
            SendOutcome::Closed => return SpeechOutcome::EventStreamClosed,
        }
    }
    SpeechOutcome::Completed
}

fn validate_frame(
    frame: &AudioFrame,
    turn_id: TurnId,
    generation_id: GenerationId,
    utterance_id: UtteranceId,
    expected_sequence: u64,
    negotiated_format: Option<PcmFormat>,
) -> Result<(), AdapterError> {
    if frame.turn_id() != turn_id {
        return Err(AdapterError::new("audio frame turn identity mismatch"));
    }
    if frame.generation_id() != generation_id {
        return Err(AdapterError::new(
            "audio frame generation identity mismatch",
        ));
    }
    if frame.utterance_id() != utterance_id {
        return Err(AdapterError::new("audio frame utterance identity mismatch"));
    }
    if frame.sequence() != expected_sequence {
        return Err(AdapterError::new(format!(
            "audio frame sequence gap: expected {expected_sequence}, received {}",
            frame.sequence()
        )));
    }
    if negotiated_format.is_some_and(|format| frame.format() != format) {
        return Err(AdapterError::new("audio frame format changed"));
    }
    Ok(())
}

async fn send_event(
    events: &mpsc::Sender<RuntimeEvent>,
    event: RuntimeEvent,
    external_interruption: &CancellationToken,
    work_cancellation: &CancellationToken,
) -> SendOutcome {
    tokio::select! {
        biased;
        _ = external_interruption.cancelled() => SendOutcome::Interrupted,
        _ = work_cancellation.cancelled() => SendOutcome::Interrupted,
        _ = events.closed() => SendOutcome::Closed,
        result = events.send(event) => {
            if result.is_ok() {
                SendOutcome::Sent
            } else {
                SendOutcome::Closed
            }
        }
    }
}

async fn send_event_pair(
    events: &mpsc::Sender<RuntimeEvent>,
    pair: [RuntimeEvent; 2],
    external_interruption: &CancellationToken,
    work_cancellation: &CancellationToken,
) -> SendOutcome {
    let mut permits = tokio::select! {
        biased;
        _ = external_interruption.cancelled() => return SendOutcome::Interrupted,
        _ = work_cancellation.cancelled() => return SendOutcome::Interrupted,
        _ = events.closed() => return SendOutcome::Closed,
        permits = events.reserve_many(2) => match permits {
            Ok(permits) => permits,
            Err(_) => return SendOutcome::Closed,
        },
    };
    let [first, second] = pair;
    permits
        .next()
        .expect("two event permits were reserved")
        .send(first);
    permits
        .next()
        .expect("two event permits were reserved")
        .send(second);
    SendOutcome::Sent
}

async fn stop_streaming_pipeline(
    utterance_sender: &mut Option<mpsc::Sender<QueuedUtterance>>,
    language_cancellation: &CancellationToken,
    work_cancellation: &CancellationToken,
    deltas: &mut mpsc::Receiver<Result<GenerationTextDelta, AdapterError>>,
    speech_worker: &mut JoinHandle<SpeechOutcome>,
) {
    work_cancellation.cancel();
    language_cancellation.cancel();
    utterance_sender.take();
    let (_, _) = tokio::join!(drain_language_stream(deltas), &mut *speech_worker);
}

async fn finish_queue_outcome(
    turn_id: TurnId,
    outcome: QueueOutcome,
    utterance_sender: &mut Option<mpsc::Sender<QueuedUtterance>>,
    language_cancellation: &CancellationToken,
    work_cancellation: &CancellationToken,
    deltas: &mut mpsc::Receiver<Result<GenerationTextDelta, AdapterError>>,
    speech_worker: &mut JoinHandle<SpeechOutcome>,
) -> RuntimeEvent {
    match outcome {
        QueueOutcome::WorkerFinished(outcome) => {
            work_cancellation.cancel();
            utterance_sender.take();
            cleanup_language_stream(language_cancellation, deltas).await;
            terminal_from_speech(turn_id, outcome)
        }
        QueueOutcome::Interrupted => {
            stop_streaming_pipeline(
                utterance_sender,
                language_cancellation,
                work_cancellation,
                deltas,
                speech_worker,
            )
            .await;
            RuntimeEvent::TurnCancelled { turn_id }
        }
        QueueOutcome::Closed => {
            stop_streaming_pipeline(
                utterance_sender,
                language_cancellation,
                work_cancellation,
                deltas,
                speech_worker,
            )
            .await;
            runtime_failure(turn_id, "streaming speech utterance queue closed early")
        }
        QueueOutcome::Failed(error) => {
            stop_streaming_pipeline(
                utterance_sender,
                language_cancellation,
                work_cancellation,
                deltas,
                speech_worker,
            )
            .await;
            adapter_failure(turn_id, RuntimeStage::Runtime, error)
        }
    }
}

async fn cleanup_language_stream(
    cancellation: &CancellationToken,
    deltas: &mut mpsc::Receiver<Result<GenerationTextDelta, AdapterError>>,
) {
    cancellation.cancel();
    drain_language_stream(deltas).await;
}

async fn drain_language_stream(
    deltas: &mut mpsc::Receiver<Result<GenerationTextDelta, AdapterError>>,
) {
    while deltas.recv().await.is_some() {}
}

async fn cleanup_frame_stream(
    cancellation: &CancellationToken,
    frames: &mut mpsc::Receiver<Result<AudioFrame, AdapterError>>,
) {
    cancellation.cancel();
    while frames.recv().await.is_some() {}
}

fn terminal_from_speech(
    turn_id: TurnId,
    outcome: Result<SpeechOutcome, JoinError>,
) -> RuntimeEvent {
    let Ok(outcome) = outcome else {
        return runtime_failure(turn_id, "streaming speech worker task failed");
    };

    match outcome {
        SpeechOutcome::Completed => RuntimeEvent::TurnCompleted { turn_id },
        SpeechOutcome::Interrupted => RuntimeEvent::TurnCancelled { turn_id },
        SpeechOutcome::Failed { stage, error } => adapter_failure(turn_id, stage, error),
        SpeechOutcome::EventStreamClosed => {
            runtime_failure(turn_id, "streaming speech event consumer closed")
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
        elapsed_ms: u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
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

fn adapter_runtime_error(stage: RuntimeStage, error: AdapterError) -> RuntimeError {
    RuntimeError::new(RuntimeErrorKind::Adapter, stage, error.message())
}

fn runtime_failure(turn_id: TurnId, message: impl Into<String>) -> RuntimeEvent {
    RuntimeEvent::TurnFailed {
        turn_id,
        error: runtime_error(message),
    }
}

fn runtime_error(message: impl Into<String>) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::InvalidState,
        RuntimeStage::Runtime,
        message,
    )
}

enum SendOutcome {
    Sent,
    Interrupted,
    Closed,
    Stale,
}

enum QueueOutcome {
    Interrupted,
    Closed,
    Failed(AdapterError),
    WorkerFinished(Result<SpeechOutcome, JoinError>),
}

enum SpeechOutcome {
    Completed,
    Interrupted,
    Failed {
        stage: RuntimeStage,
        error: AdapterError,
    },
    EventStreamClosed,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use conversation_model_adapters::{
        MockContinuousAudioOutput, MockGenerationLanguageModel, MockStreamingSpeechSynthesizer,
        PcmSampleFormat,
    };
    use conversation_protocol::{ConversationMode, PersonaProfile, ResponseControls};
    use tokio::time::timeout;

    use super::*;
    use crate::{ConversationContext, ConversationQualityController, ConversationTurnSource};

    fn context() -> ConversationContext {
        ConversationContext::new(ConversationQualityController::new(
            PersonaProfile::default(),
            ResponseControls::default(),
            ConversationMode::DirectAnswer,
        ))
    }

    #[derive(Default)]
    struct BlockingLanguage {
        started: Arc<Notify>,
    }

    impl GenerationLanguageModel for BlockingLanguage {
        fn stream(
            &self,
            _request: GenerationLanguageRequest,
            cancellation: CancellationToken,
        ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
            let (sender, receiver) = mpsc::channel(1);
            let started = Arc::clone(&self.started);
            tokio::spawn(async move {
                started.notify_one();
                cancellation.cancelled().await;
                drop(sender);
            });
            receiver
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn abort_racing_a_finalized_context_still_releases_the_runtime() {
        let context = context();
        let language = Arc::new(BlockingLanguage::default());
        let runtime = StreamingTurnRuntime::new(
            context.clone(),
            language.clone(),
            Arc::new(MockStreamingSpeechSynthesizer::new([])),
            Arc::new(MockContinuousAudioOutput::new()),
        );
        let started = runtime
            .start_turn(ConversationTurnSource::Text, "barged in at completion")
            .await
            .unwrap();
        let identity = started.identity();
        language.started.notified().await;

        // The terminal task finalizes the context before clearing the active
        // slot; this discard reproduces an abort landing inside that window.
        context.discard_turn(identity, true).await.unwrap();
        let mut events = started.into_events();
        runtime
            .abort_turn(identity.turn_id(), identity.generation_id(), &mut events)
            .await
            .expect("abort racing a finalized context must succeed");

        let mut second = runtime
            .start_turn(ConversationTurnSource::Text, "next")
            .await
            .expect("aborted runtime must immediately accept a new turn");
        runtime
            .interrupt(
                second.identity().turn_id(),
                second.identity().generation_id(),
            )
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while second.recv().await.is_some() {}
        })
        .await
        .expect("second turn did not clean up");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_waits_for_active_clear_and_immediately_allows_reuse() {
        let first_turn = TurnId::new(1);
        let first_generation = GenerationId::new(1);
        let format = PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
        let runtime = StreamingTurnRuntime::new(
            context(),
            Arc::new(MockGenerationLanguageModel::new(["answer"])),
            Arc::new(MockStreamingSpeechSynthesizer::new([AudioFrame::new(
                first_turn,
                first_generation,
                UtteranceId::new(1),
                0,
                format,
                vec![0; 960],
            )
            .unwrap()])),
            Arc::new(MockContinuousAudioOutput::new()),
        );
        let mut first = runtime
            .start_turn(ConversationTurnSource::Text, "first")
            .await
            .unwrap();
        let active = runtime.active.lock().await;

        let terminal_while_active_is_locked = timeout(Duration::from_millis(50), async {
            while let Some(event) = first.recv().await {
                if event.is_terminal() {
                    return event;
                }
            }
            panic!("stream ended without a terminal event");
        })
        .await;
        assert!(
            terminal_while_active_is_locked.is_err(),
            "terminal published before the active generation was cleared"
        );

        drop(active);
        let terminal = timeout(Duration::from_secs(1), async {
            loop {
                let event = first
                    .recv()
                    .await
                    .expect("stream ended without a terminal event");
                if event.is_terminal() {
                    break event;
                }
            }
        })
        .await
        .expect("terminal did not publish after active generation clear");
        assert_eq!(
            terminal,
            RuntimeEvent::TurnCompleted {
                turn_id: first_turn
            }
        );

        let interrupt_error = runtime
            .interrupt(first_turn, first_generation)
            .await
            .unwrap_err();
        assert_eq!(
            interrupt_error.message(),
            "there is no active streaming generation"
        );

        let second_turn = TurnId::new(2);
        let second_generation = GenerationId::new(2);
        let mut second = runtime
            .start_turn(ConversationTurnSource::Text, "second")
            .await
            .expect("terminal observation must imply immediate runtime reuse");
        runtime
            .interrupt(second_turn, second_generation)
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while second.recv().await.is_some() {}
        })
        .await
        .expect("second turn did not clean up");
    }
}
