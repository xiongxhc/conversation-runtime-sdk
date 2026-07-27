use std::future::poll_fn;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::task::Poll;
use std::time::Instant;

use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioOutput, AudioOutputRequest, SpeechRequest, SpeechSynthesizer,
};
use conversation_protocol::{RuntimeEvent, RuntimeStage, RuntimeTimingMilestone, TurnId};
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

pub(crate) struct SpeechSegment {
    pub(crate) index: u64,
    pub(crate) text: String,
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

    pub(crate) async fn run(mut self) -> SpeechWorkerOutcome {
        let mut speech_started = false;
        let mut first_playable_audio = false;

        loop {
            let segment = tokio::select! {
                biased;
                _ = self.external_interruption.cancelled() => {
                    return SpeechWorkerOutcome::Interrupted;
                }
                _ = self.work_cancellation.cancelled() => {
                    return SpeechWorkerOutcome::Stopped;
                }
                _ = self.events.closed() => {
                    return self.event_stream_closed();
                }
                segment = self.segments.recv() => segment,
            };
            let Some(segment) = segment else {
                break;
            };

            if !speech_started {
                match self
                    .send_required_pair(|| {
                        [
                            RuntimeEvent::SpeechStarted {
                                turn_id: self.turn_id,
                            },
                            self.timing_event(RuntimeTimingMilestone::FirstSynthesisRequest),
                        ]
                    })
                    .await
                {
                    WorkerSend::Sent => speech_started = true,
                    WorkerSend::Interrupted => return SpeechWorkerOutcome::Interrupted,
                    WorkerSend::Stopped => return SpeechWorkerOutcome::Stopped,
                    WorkerSend::Closed => return self.event_stream_closed(),
                }
            }

            let synthesis = catch_unwind(AssertUnwindSafe(|| {
                self.speech_synthesizer.synthesize(
                    SpeechRequest::new(self.turn_id, segment.text),
                    self.work_cancellation.child_token(),
                )
            }));
            let synthesis = match synthesis {
                Ok(synthesis) => synthesis,
                Err(_) => {
                    return self.failure(
                        RuntimeStage::SpeechSynthesizer,
                        AdapterError::new("speech synthesizer adapter panicked"),
                    );
                }
            };
            let audio = match self.wait_for_adapter(synthesis).await {
                AdapterWait::Completed(Ok(audio)) => audio,
                AdapterWait::Completed(Err(error)) => {
                    return self.failure(RuntimeStage::SpeechSynthesizer, error);
                }
                AdapterWait::Panicked => {
                    return self.failure(
                        RuntimeStage::SpeechSynthesizer,
                        AdapterError::new("speech synthesizer adapter panicked"),
                    );
                }
                AdapterWait::Interrupted => return SpeechWorkerOutcome::Interrupted,
                AdapterWait::Stopped => return SpeechWorkerOutcome::Stopped,
                AdapterWait::Closed => return self.event_stream_closed(),
            };
            if let Err(error) = audio.validate() {
                return self.failure(RuntimeStage::SpeechSynthesizer, error);
            }

            if !first_playable_audio {
                match self
                    .send_event(self.timing_event(RuntimeTimingMilestone::FirstPlayableAudio))
                    .await
                {
                    WorkerSend::Sent => first_playable_audio = true,
                    WorkerSend::Interrupted => return SpeechWorkerOutcome::Interrupted,
                    WorkerSend::Stopped => return SpeechWorkerOutcome::Stopped,
                    WorkerSend::Closed => return self.event_stream_closed(),
                }
            }

            let output = catch_unwind(AssertUnwindSafe(|| {
                self.audio_output.play(
                    AudioOutputRequest::new(self.turn_id, segment.index, audio),
                    self.work_cancellation.child_token(),
                )
            }));
            let output = match output {
                Ok(output) => output,
                Err(_) => {
                    return self.failure(
                        RuntimeStage::AudioOutput,
                        AdapterError::new("audio output adapter panicked"),
                    );
                }
            };
            match self.wait_for_adapter(output).await {
                AdapterWait::Completed(Ok(())) => {}
                AdapterWait::Completed(Err(error)) => {
                    return self.failure(RuntimeStage::AudioOutput, error);
                }
                AdapterWait::Panicked => {
                    return self.failure(
                        RuntimeStage::AudioOutput,
                        AdapterError::new("audio output adapter panicked"),
                    );
                }
                AdapterWait::Interrupted => return SpeechWorkerOutcome::Interrupted,
                AdapterWait::Stopped => return SpeechWorkerOutcome::Stopped,
                AdapterWait::Closed => return self.event_stream_closed(),
            }
        }

        if speech_started {
            match self
                .send_event(RuntimeEvent::SpeechCompleted {
                    turn_id: self.turn_id,
                })
                .await
            {
                WorkerSend::Sent => {}
                WorkerSend::Interrupted => return SpeechWorkerOutcome::Interrupted,
                WorkerSend::Stopped => return SpeechWorkerOutcome::Stopped,
                WorkerSend::Closed => return self.event_stream_closed(),
            }
        }

        SpeechWorkerOutcome::Completed
    }

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
            _ = self.work_cancellation.cancelled() => {
                return WorkerSend::Stopped;
            }
            _ = self.events.closed() => {
                return WorkerSend::Closed;
            }
            event_guard = self.event_gate.lock() => event_guard,
        };

        let result = tokio::select! {
            biased;
            _ = self.external_interruption.cancelled() => {
                return WorkerSend::Interrupted;
            }
            _ = self.work_cancellation.cancelled() => {
                return WorkerSend::Stopped;
            }
            _ = self.events.closed() => {
                return WorkerSend::Closed;
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
            _ = self.work_cancellation.cancelled() => {
                return WorkerSend::Stopped;
            }
            _ = self.events.closed() => {
                return WorkerSend::Closed;
            }
            event_guard = self.event_gate.lock() => event_guard,
        };

        let mut permits = tokio::select! {
            biased;
            _ = self.external_interruption.cancelled() => {
                return WorkerSend::Interrupted;
            }
            _ = self.work_cancellation.cancelled() => {
                return WorkerSend::Stopped;
            }
            _ = self.events.closed() => {
                return WorkerSend::Closed;
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

    fn failure(&self, stage: RuntimeStage, error: AdapterError) -> SpeechWorkerOutcome {
        self.work_cancellation.cancel();
        SpeechWorkerOutcome::Failed { stage, error }
    }

    fn event_stream_closed(&self) -> SpeechWorkerOutcome {
        self.work_cancellation.cancel();
        SpeechWorkerOutcome::EventStreamClosed
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

    use conversation_model_adapters::{DiscardAudioOutput, MockSpeechSynthesizer};
    use conversation_protocol::{RuntimeEvent, RuntimeTimingMilestone, TurnId};
    use tokio::sync::{mpsc, Mutex};
    use tokio_util::sync::CancellationToken;

    use super::{SpeechWorker, SpeechWorkerContext, WorkerSend};

    #[tokio::test(flavor = "current_thread")]
    async fn speech_start_pair_is_not_partially_published_with_one_slot_free() {
        assert_interrupted_pair_is_absent(1).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn speech_start_pair_is_not_partially_published_when_saturated() {
        assert_interrupted_pair_is_absent(2).await;
    }

    async fn assert_interrupted_pair_is_absent(prefilled_slots: usize) {
        let (segment_sender, segment_receiver) = mpsc::channel(1);
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
        let worker = SpeechWorker::new(SpeechWorkerContext {
            turn_id,
            speech_synthesizer: Arc::new(MockSpeechSynthesizer::new([])),
            audio_output: Arc::new(DiscardAudioOutput),
            segments: segment_receiver,
            events: event_sender,
            event_gate: Arc::new(Mutex::new(())),
            started_at: Instant::now(),
            external_interruption: external_interruption.clone(),
            work_cancellation: CancellationToken::new(),
        });
        let send_pair = worker.send_required_pair(move || {
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
        drop(segment_sender);
    }
}
