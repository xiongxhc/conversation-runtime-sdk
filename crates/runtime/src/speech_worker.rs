use std::future::poll_fn;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::task::Poll;
use std::time::Instant;

use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioOutput, AudioOutputRequest, SpeechRequest, SpeechSynthesizer,
    SynthesizedAudio,
};
use conversation_protocol::{RuntimeEvent, RuntimeStage, RuntimeTimingMilestone, TurnId};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const PREPARED_AUDIO_CAPACITY: usize = 1;
const SYNTHESIS_TASK_FAILED: &str = "speech synthesis task failed";

pub(crate) struct SpeechSegment {
    pub(crate) index: u64,
    pub(crate) text: String,
}

struct PreparedAudio {
    index: u64,
    audio: SynthesizedAudio,
}

pub(crate) enum SpeechWorkerOutcome {
    Completed,
    Interrupted,
    Stopped,
    Failed {
        stage: RuntimeStage,
        error: AdapterError,
    },
    EventStreamClosed,
}

enum SynthesisStageOutcome {
    Completed { synthesized_any: bool },
    Interrupted,
    Stopped,
    EventStreamClosed,
    TaskFailed,
    Failed(AdapterError),
}

pub(crate) struct SpeechWorkerContext {
    pub(crate) turn_id: TurnId,
    pub(crate) speech_synthesizer: Arc<dyn SpeechSynthesizer>,
    pub(crate) audio_output: Arc<dyn AudioOutput>,
    pub(crate) segments: mpsc::Receiver<SpeechSegment>,
    pub(crate) events: mpsc::Sender<RuntimeEvent>,
    pub(crate) event_gate: Arc<Mutex<()>>,
    pub(crate) started_at: Instant,
    pub(crate) external_interruption: CancellationToken,
    pub(crate) work_cancellation: CancellationToken,
}

pub(crate) struct SpeechWorker {
    turn_id: TurnId,
    speech_synthesizer: Arc<dyn SpeechSynthesizer>,
    audio_output: Arc<dyn AudioOutput>,
    segments: mpsc::Receiver<SpeechSegment>,
    events: mpsc::Sender<RuntimeEvent>,
    event_gate: Arc<Mutex<()>>,
    started_at: Instant,
    external_interruption: CancellationToken,
    work_cancellation: CancellationToken,
}

#[derive(Clone)]
struct SpeechEventPublisher {
    turn_id: TurnId,
    events: mpsc::Sender<RuntimeEvent>,
    event_gate: Arc<Mutex<()>>,
    started_at: Instant,
    external_interruption: CancellationToken,
    work_cancellation: CancellationToken,
}

struct SynthesisStage {
    speech_synthesizer: Arc<dyn SpeechSynthesizer>,
    segments: mpsc::Receiver<SpeechSegment>,
    prepared_audio: mpsc::Sender<PreparedAudio>,
    publisher: SpeechEventPublisher,
}

struct OutputStage {
    audio_output: Arc<dyn AudioOutput>,
    publisher: SpeechEventPublisher,
}

impl SpeechWorker {
    pub(crate) fn new(context: SpeechWorkerContext) -> Self {
        let SpeechWorkerContext {
            turn_id,
            speech_synthesizer,
            audio_output,
            segments,
            events,
            event_gate,
            started_at,
            external_interruption,
            work_cancellation,
        } = context;
        Self {
            turn_id,
            speech_synthesizer,
            audio_output,
            segments,
            events,
            event_gate,
            started_at,
            external_interruption,
            work_cancellation,
        }
    }

    pub(crate) async fn run(self) -> SpeechWorkerOutcome {
        let Self {
            turn_id,
            speech_synthesizer,
            audio_output,
            segments,
            events,
            event_gate,
            started_at,
            external_interruption,
            work_cancellation,
        } = self;
        let publisher = SpeechEventPublisher {
            turn_id,
            events,
            event_gate,
            started_at,
            external_interruption,
            work_cancellation,
        };
        let (prepared_sender, prepared_receiver) = mpsc::channel(PREPARED_AUDIO_CAPACITY);
        let synthesis_stage = tokio::spawn(
            SynthesisStage {
                speech_synthesizer,
                segments,
                prepared_audio: prepared_sender,
                publisher: publisher.clone(),
            }
            .run(),
        );

        OutputStage {
            audio_output,
            publisher,
        }
        .run(prepared_receiver, synthesis_stage)
        .await
    }
}

impl SynthesisStage {
    async fn run(mut self) -> SynthesisStageOutcome {
        let mut speech_started = false;
        let mut first_playable_audio = false;
        let mut synthesized_any = false;

        loop {
            let segment = tokio::select! {
                biased;
                _ = self.publisher.external_interruption.cancelled() => {
                    self.publisher.work_cancellation.cancel();
                    return SynthesisStageOutcome::Interrupted;
                }
                _ = self.publisher.events.closed() => {
                    self.publisher.work_cancellation.cancel();
                    return SynthesisStageOutcome::EventStreamClosed;
                }
                _ = self.publisher.work_cancellation.cancelled() => {
                    return SynthesisStageOutcome::Stopped;
                }
                segment = self.segments.recv() => segment,
            };
            let Some(segment) = segment else {
                return SynthesisStageOutcome::Completed { synthesized_any };
            };

            let permit = tokio::select! {
                biased;
                _ = self.publisher.external_interruption.cancelled() => {
                    self.publisher.work_cancellation.cancel();
                    return SynthesisStageOutcome::Interrupted;
                }
                _ = self.publisher.events.closed() => {
                    self.publisher.work_cancellation.cancel();
                    return SynthesisStageOutcome::EventStreamClosed;
                }
                _ = self.publisher.work_cancellation.cancelled() => {
                    return SynthesisStageOutcome::Stopped;
                }
                permit = self.prepared_audio.reserve() => {
                    match permit {
                        Ok(permit) => permit,
                        Err(_) => return SynthesisStageOutcome::Stopped,
                    }
                }
            };

            if !speech_started {
                match self
                    .publisher
                    .send_required_pair(|| {
                        [
                            RuntimeEvent::SpeechStarted {
                                turn_id: self.publisher.turn_id,
                            },
                            self.publisher
                                .timing_event(RuntimeTimingMilestone::FirstSynthesisRequest),
                        ]
                    })
                    .await
                {
                    WorkerSend::Sent => speech_started = true,
                    WorkerSend::Interrupted => return SynthesisStageOutcome::Interrupted,
                    WorkerSend::Stopped => return SynthesisStageOutcome::Stopped,
                    WorkerSend::Closed => return SynthesisStageOutcome::EventStreamClosed,
                }
            }

            let synthesis = catch_unwind(AssertUnwindSafe(|| {
                self.speech_synthesizer.synthesize(
                    SpeechRequest::new(self.publisher.turn_id, segment.text),
                    self.publisher.work_cancellation.child_token(),
                )
            }));
            let synthesis = match synthesis {
                Ok(synthesis) => synthesis,
                Err(_) => {
                    return SynthesisStageOutcome::Failed(AdapterError::new(
                        "speech synthesizer adapter panicked",
                    ));
                }
            };
            let audio = match self.publisher.wait_for_adapter(synthesis).await {
                AdapterWait::Completed(Ok(audio)) => audio,
                AdapterWait::Completed(Err(error)) => {
                    return SynthesisStageOutcome::Failed(error);
                }
                AdapterWait::Panicked => {
                    return SynthesisStageOutcome::Failed(AdapterError::new(
                        "speech synthesizer adapter panicked",
                    ));
                }
                AdapterWait::Interrupted => return SynthesisStageOutcome::Interrupted,
                AdapterWait::Stopped => return SynthesisStageOutcome::Stopped,
                AdapterWait::Closed => return SynthesisStageOutcome::EventStreamClosed,
            };
            if let Err(error) = audio.validate() {
                return SynthesisStageOutcome::Failed(error);
            }

            if !first_playable_audio {
                match self
                    .publisher
                    .send_event(
                        self.publisher
                            .timing_event(RuntimeTimingMilestone::FirstPlayableAudio),
                    )
                    .await
                {
                    WorkerSend::Sent => first_playable_audio = true,
                    WorkerSend::Interrupted => return SynthesisStageOutcome::Interrupted,
                    WorkerSend::Stopped => return SynthesisStageOutcome::Stopped,
                    WorkerSend::Closed => return SynthesisStageOutcome::EventStreamClosed,
                }
            }

            permit.send(PreparedAudio {
                index: segment.index,
                audio,
            });
            synthesized_any = true;
        }
    }
}

impl OutputStage {
    async fn run(
        self,
        mut prepared_audio: mpsc::Receiver<PreparedAudio>,
        mut synthesis_stage: JoinHandle<SynthesisStageOutcome>,
    ) -> SpeechWorkerOutcome {
        let mut synthesis_outcome = None;

        loop {
            let prepared = tokio::select! {
                biased;
                _ = self.publisher.external_interruption.cancelled() => {
                    self.publisher.work_cancellation.cancel();
                    prepared_audio.close();
                    let _ = resolve_synthesis_stage(
                        &mut synthesis_stage,
                        &mut synthesis_outcome,
                    ).await;
                    return SpeechWorkerOutcome::Interrupted;
                }
                _ = self.publisher.events.closed() => {
                    self.publisher.work_cancellation.cancel();
                    prepared_audio.close();
                    let _ = resolve_synthesis_stage(
                        &mut synthesis_stage,
                        &mut synthesis_outcome,
                    ).await;
                    return SpeechWorkerOutcome::EventStreamClosed;
                }
                outcome = &mut synthesis_stage, if synthesis_outcome.is_none() => {
                    let outcome = synthesis_join_outcome(outcome);
                    if matches!(outcome, SynthesisStageOutcome::Completed { .. }) {
                        synthesis_outcome = Some(outcome);
                        continue;
                    }
                    self.publisher.work_cancellation.cancel();
                    prepared_audio.close();
                    return synthesis_worker_outcome(outcome);
                }
                _ = self.publisher.work_cancellation.cancelled() => {
                    prepared_audio.close();
                    let outcome = resolve_synthesis_stage(
                        &mut synthesis_stage,
                        &mut synthesis_outcome,
                    ).await;
                    return stopped_worker_outcome(outcome);
                }
                prepared = prepared_audio.recv() => prepared,
            };
            let Some(prepared) = prepared else {
                break;
            };

            if let Err(outcome) = self
                .play(
                    prepared,
                    &mut prepared_audio,
                    &mut synthesis_stage,
                    &mut synthesis_outcome,
                )
                .await
            {
                return outcome;
            }
        }

        match resolve_synthesis_stage(&mut synthesis_stage, &mut synthesis_outcome).await {
            SynthesisStageOutcome::Completed { synthesized_any } => {
                if synthesized_any {
                    match self
                        .publisher
                        .send_event(RuntimeEvent::SpeechCompleted {
                            turn_id: self.publisher.turn_id,
                        })
                        .await
                    {
                        WorkerSend::Sent => {}
                        WorkerSend::Interrupted => return SpeechWorkerOutcome::Interrupted,
                        WorkerSend::Stopped => return SpeechWorkerOutcome::Stopped,
                        WorkerSend::Closed => return SpeechWorkerOutcome::EventStreamClosed,
                    }
                }
                SpeechWorkerOutcome::Completed
            }
            outcome => synthesis_worker_outcome(outcome),
        }
    }

    async fn play(
        &self,
        prepared: PreparedAudio,
        prepared_audio: &mut mpsc::Receiver<PreparedAudio>,
        synthesis_stage: &mut JoinHandle<SynthesisStageOutcome>,
        synthesis_outcome: &mut Option<SynthesisStageOutcome>,
    ) -> Result<(), SpeechWorkerOutcome> {
        let output = catch_unwind(AssertUnwindSafe(|| {
            self.audio_output.play(
                AudioOutputRequest::new(self.publisher.turn_id, prepared.index, prepared.audio),
                self.publisher.work_cancellation.child_token(),
            )
        }));
        let output = match output {
            Ok(output) => output,
            Err(_) => {
                return Err(self
                    .finish_output_failure(
                        prepared_audio,
                        synthesis_stage,
                        synthesis_outcome,
                        AdapterError::new("audio output adapter panicked"),
                    )
                    .await);
            }
        };
        let output = catch_adapter_panic(output);
        tokio::pin!(output);

        loop {
            tokio::select! {
                biased;
                _ = self.publisher.external_interruption.cancelled() => {
                    self.publisher.work_cancellation.cancel();
                    prepared_audio.close();
                    let (_, _) = tokio::join!(
                        &mut output,
                        resolve_synthesis_stage(synthesis_stage, synthesis_outcome),
                    );
                    return Err(SpeechWorkerOutcome::Interrupted);
                }
                _ = self.publisher.events.closed() => {
                    self.publisher.work_cancellation.cancel();
                    prepared_audio.close();
                    let (_, _) = tokio::join!(
                        &mut output,
                        resolve_synthesis_stage(synthesis_stage, synthesis_outcome),
                    );
                    return Err(SpeechWorkerOutcome::EventStreamClosed);
                }
                result = &mut output => {
                    return match result {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(error)) => Err(
                            self.finish_output_failure(
                                prepared_audio,
                                synthesis_stage,
                                synthesis_outcome,
                                error,
                            ).await
                        ),
                        Err(()) => Err(
                            self.finish_output_failure(
                                prepared_audio,
                                synthesis_stage,
                                synthesis_outcome,
                                AdapterError::new("audio output adapter panicked"),
                            ).await
                        ),
                    };
                }
                outcome = &mut *synthesis_stage, if synthesis_outcome.is_none() => {
                    let outcome = synthesis_join_outcome(outcome);
                    if matches!(outcome, SynthesisStageOutcome::Completed { .. }) {
                        *synthesis_outcome = Some(outcome);
                        continue;
                    }
                    self.publisher.work_cancellation.cancel();
                    prepared_audio.close();
                    let _ = output.await;
                    return Err(synthesis_worker_outcome(outcome));
                }
                _ = self.publisher.work_cancellation.cancelled() => {
                    prepared_audio.close();
                    let (_, outcome) = tokio::join!(
                        &mut output,
                        resolve_synthesis_stage(synthesis_stage, synthesis_outcome),
                    );
                    return Err(stopped_worker_outcome(outcome));
                }
            }
        }
    }

    async fn finish_output_failure(
        &self,
        prepared_audio: &mut mpsc::Receiver<PreparedAudio>,
        synthesis_stage: &mut JoinHandle<SynthesisStageOutcome>,
        synthesis_outcome: &mut Option<SynthesisStageOutcome>,
        error: AdapterError,
    ) -> SpeechWorkerOutcome {
        self.publisher.work_cancellation.cancel();
        prepared_audio.close();
        let _ = resolve_synthesis_stage(synthesis_stage, synthesis_outcome).await;
        SpeechWorkerOutcome::Failed {
            stage: RuntimeStage::AudioOutput,
            error,
        }
    }
}

impl SpeechEventPublisher {
    async fn wait_for_adapter<T>(&self, adapter: AdapterFuture<'_, T>) -> AdapterWait<T> {
        let adapter = catch_adapter_panic(adapter);
        tokio::pin!(adapter);

        tokio::select! {
            biased;
            _ = self.external_interruption.cancelled() => {
                self.work_cancellation.cancel();
                let _ = adapter.await;
                AdapterWait::Interrupted
            }
            _ = self.events.closed() => {
                self.work_cancellation.cancel();
                let _ = adapter.await;
                AdapterWait::Closed
            }
            _ = self.work_cancellation.cancelled() => {
                let _ = adapter.await;
                AdapterWait::Stopped
            }
            result = &mut adapter => {
                match result {
                    Ok(result) => AdapterWait::Completed(result),
                    Err(()) => AdapterWait::Panicked,
                }
            }
        }
    }

    async fn send_event(&self, event: RuntimeEvent) -> WorkerSend {
        let _event_guard = tokio::select! {
            biased;
            _ = self.external_interruption.cancelled() => {
                return WorkerSend::Interrupted;
            }
            _ = self.events.closed() => {
                return WorkerSend::Closed;
            }
            _ = self.work_cancellation.cancelled() => {
                return WorkerSend::Stopped;
            }
            event_guard = self.event_gate.lock() => event_guard,
        };

        let result = tokio::select! {
            biased;
            _ = self.external_interruption.cancelled() => {
                return WorkerSend::Interrupted;
            }
            _ = self.events.closed() => {
                return WorkerSend::Closed;
            }
            _ = self.work_cancellation.cancelled() => {
                return WorkerSend::Stopped;
            }
            result = self.events.send(event) => result,
        };
        if result.is_err() {
            WorkerSend::Closed
        } else {
            WorkerSend::Sent
        }
    }

    async fn send_required_pair(
        &self,
        build_pair: impl FnOnce() -> [RuntimeEvent; 2],
    ) -> WorkerSend {
        let _event_guard = tokio::select! {
            biased;
            _ = self.external_interruption.cancelled() => {
                return WorkerSend::Interrupted;
            }
            _ = self.events.closed() => {
                return WorkerSend::Closed;
            }
            _ = self.work_cancellation.cancelled() => {
                return WorkerSend::Stopped;
            }
            event_guard = self.event_gate.lock() => event_guard,
        };

        let mut permits = tokio::select! {
            biased;
            _ = self.external_interruption.cancelled() => {
                return WorkerSend::Interrupted;
            }
            _ = self.events.closed() => {
                return WorkerSend::Closed;
            }
            _ = self.work_cancellation.cancelled() => {
                return WorkerSend::Stopped;
            }
            permits = self.events.reserve_many(2) => {
                match permits {
                    Ok(permits) => permits,
                    Err(_) => return WorkerSend::Closed,
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
        WorkerSend::Sent
    }

    fn timing_event(&self, milestone: RuntimeTimingMilestone) -> RuntimeEvent {
        RuntimeEvent::Timing {
            turn_id: self.turn_id,
            milestone,
            elapsed_ms: elapsed_milliseconds(self.started_at),
        }
    }
}

async fn resolve_synthesis_stage(
    synthesis_stage: &mut JoinHandle<SynthesisStageOutcome>,
    synthesis_outcome: &mut Option<SynthesisStageOutcome>,
) -> SynthesisStageOutcome {
    match synthesis_outcome.take() {
        Some(outcome) => outcome,
        None => synthesis_join_outcome(synthesis_stage.await),
    }
}

fn synthesis_join_outcome(
    outcome: Result<SynthesisStageOutcome, tokio::task::JoinError>,
) -> SynthesisStageOutcome {
    outcome.unwrap_or(SynthesisStageOutcome::TaskFailed)
}

fn synthesis_worker_outcome(outcome: SynthesisStageOutcome) -> SpeechWorkerOutcome {
    match outcome {
        SynthesisStageOutcome::Completed { .. } => SpeechWorkerOutcome::Completed,
        SynthesisStageOutcome::Interrupted => SpeechWorkerOutcome::Interrupted,
        SynthesisStageOutcome::Stopped => SpeechWorkerOutcome::Stopped,
        SynthesisStageOutcome::EventStreamClosed => SpeechWorkerOutcome::EventStreamClosed,
        SynthesisStageOutcome::TaskFailed => SpeechWorkerOutcome::Failed {
            stage: RuntimeStage::SpeechSynthesizer,
            error: AdapterError::new(SYNTHESIS_TASK_FAILED),
        },
        SynthesisStageOutcome::Failed(error) => SpeechWorkerOutcome::Failed {
            stage: RuntimeStage::SpeechSynthesizer,
            error,
        },
    }
}

fn stopped_worker_outcome(outcome: SynthesisStageOutcome) -> SpeechWorkerOutcome {
    match outcome {
        SynthesisStageOutcome::Failed(_) | SynthesisStageOutcome::TaskFailed => {
            synthesis_worker_outcome(outcome)
        }
        SynthesisStageOutcome::Interrupted => SpeechWorkerOutcome::Interrupted,
        SynthesisStageOutcome::EventStreamClosed => SpeechWorkerOutcome::EventStreamClosed,
        SynthesisStageOutcome::Completed { .. } | SynthesisStageOutcome::Stopped => {
            SpeechWorkerOutcome::Stopped
        }
    }
}

async fn catch_adapter_panic<T>(
    mut adapter: AdapterFuture<'_, T>,
) -> Result<Result<T, AdapterError>, ()> {
    poll_fn(
        |context| match catch_unwind(AssertUnwindSafe(|| adapter.as_mut().poll(context))) {
            Ok(Poll::Ready(result)) => Poll::Ready(Ok(result)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(Err(())),
        },
    )
    .await
}

fn elapsed_milliseconds(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

enum WorkerSend {
    Sent,
    Interrupted,
    Stopped,
    Closed,
}

enum AdapterWait<T> {
    Completed(Result<T, AdapterError>),
    Panicked,
    Interrupted,
    Stopped,
    Closed,
}

#[cfg(test)]
mod tests {
    use std::future::ready;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    use conversation_protocol::{RuntimeEvent, RuntimeStage, RuntimeTimingMilestone, TurnId};
    use tokio::sync::{mpsc, Mutex};
    use tokio_util::sync::CancellationToken;

    use super::{
        synthesis_join_outcome, synthesis_worker_outcome, SpeechEventPublisher,
        SpeechWorkerOutcome, WorkerSend,
    };

    #[tokio::test(flavor = "current_thread")]
    async fn speech_start_pair_is_not_partially_published_with_one_slot_free() {
        assert_interrupted_pair_is_absent(1).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn speech_start_pair_is_not_partially_published_when_saturated() {
        assert_interrupted_pair_is_absent(2).await;
    }

    #[tokio::test]
    async fn synthesis_join_error_maps_to_static_synthesis_failure() {
        let join_error = tokio::spawn(async { panic!("private synthesis panic payload") })
            .await
            .expect_err("synthesis task unexpectedly completed");
        let outcome = synthesis_worker_outcome(synthesis_join_outcome(Err(join_error)));

        match outcome {
            SpeechWorkerOutcome::Failed { stage, error } => {
                assert_eq!(stage, RuntimeStage::SpeechSynthesizer);
                assert_eq!(error.message(), "speech synthesis task failed");
            }
            _ => panic!("synthesis join error did not produce a synthesis failure"),
        }
    }

    async fn assert_interrupted_pair_is_absent(prefilled_slots: usize) {
        let (event_sender, mut event_receiver) = mpsc::channel(2);
        let turn_id = TurnId::new(40);
        event_sender
            .send(RuntimeEvent::TurnStarted { turn_id })
            .await
            .unwrap();
        if prefilled_slots == 2 {
            event_sender
                .send(RuntimeEvent::TranscriptFinal {
                    turn_id,
                    text: "prefill".into(),
                })
                .await
                .unwrap();
        }
        let external_interruption = CancellationToken::new();
        let timing_sampled = Arc::new(AtomicBool::new(false));
        let timing_sampled_for_pair = Arc::clone(&timing_sampled);
        let publisher = SpeechEventPublisher {
            turn_id,
            events: event_sender,
            event_gate: Arc::new(Mutex::new(())),
            started_at: Instant::now(),
            external_interruption: external_interruption.clone(),
            work_cancellation: CancellationToken::new(),
        };
        let send_pair = publisher.send_required_pair(move || {
            timing_sampled_for_pair.store(true, Ordering::Release);
            [
                RuntimeEvent::SpeechStarted { turn_id },
                RuntimeEvent::Timing {
                    turn_id,
                    milestone: RuntimeTimingMilestone::FirstSynthesisRequest,
                    elapsed_ms: 1,
                },
            ]
        });
        tokio::pin!(send_pair);

        tokio::select! {
            biased;
            result = &mut send_pair => {
                panic!("speech pair resolved before interruption: {}", matches!(result, WorkerSend::Sent));
            }
            _ = ready(()) => {}
        }
        assert!(
            !timing_sampled.load(Ordering::Acquire),
            "first synthesis timing was sampled before two event slots were reserved"
        );
        external_interruption.cancel();
        assert!(matches!(send_pair.await, WorkerSend::Interrupted));
        assert_eq!(
            event_receiver.recv().await,
            Some(RuntimeEvent::TurnStarted { turn_id })
        );
        if prefilled_slots == 2 {
            assert_eq!(
                event_receiver.recv().await,
                Some(RuntimeEvent::TranscriptFinal {
                    turn_id,
                    text: "prefill".into(),
                })
            );
        }
        assert!(
            event_receiver.try_recv().is_err(),
            "interruption exposed half of the speech start pair"
        );
    }
}
