use std::sync::Arc;
use std::time::Instant;

use conversation_model_adapters::{
    AdapterError, AudioOutput, AudioOutputRequest, SpeechRequest, SpeechSynthesizer,
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
                segment = self.segments.recv() => segment,
            };
            let Some(segment) = segment else {
                break;
            };

            if !speech_started {
                match self
                    .send_events([
                        RuntimeEvent::SpeechStarted {
                            turn_id: self.turn_id,
                        },
                        self.timing_event(RuntimeTimingMilestone::FirstSynthesisRequest),
                    ])
                    .await
                {
                    WorkerSend::Sent => speech_started = true,
                    WorkerSend::Interrupted => return SpeechWorkerOutcome::Interrupted,
                    WorkerSend::Stopped => return SpeechWorkerOutcome::Stopped,
                    WorkerSend::Closed => return self.event_stream_closed(),
                }
            }

            let audio = self
                .speech_synthesizer
                .synthesize(
                    SpeechRequest::new(self.turn_id, segment.text),
                    self.work_cancellation.child_token(),
                )
                .await;
            if let Some(outcome) = self.cancellation_outcome() {
                return outcome;
            }
            let audio = match audio {
                Ok(audio) => audio,
                Err(error) => {
                    return self.failure(RuntimeStage::SpeechSynthesizer, error);
                }
            };
            if let Err(error) = audio.validate() {
                return self.failure(RuntimeStage::SpeechSynthesizer, error);
            }

            if !first_playable_audio {
                match self
                    .send_events([self.timing_event(RuntimeTimingMilestone::FirstPlayableAudio)])
                    .await
                {
                    WorkerSend::Sent => first_playable_audio = true,
                    WorkerSend::Interrupted => return SpeechWorkerOutcome::Interrupted,
                    WorkerSend::Stopped => return SpeechWorkerOutcome::Stopped,
                    WorkerSend::Closed => return self.event_stream_closed(),
                }
            }

            let output = self
                .audio_output
                .play(
                    AudioOutputRequest::new(self.turn_id, segment.index, audio),
                    self.work_cancellation.child_token(),
                )
                .await;
            if let Some(outcome) = self.cancellation_outcome() {
                return outcome;
            }
            if let Err(error) = output {
                return self.failure(RuntimeStage::AudioOutput, error);
            }
        }

        if speech_started {
            match self
                .send_events([RuntimeEvent::SpeechCompleted {
                    turn_id: self.turn_id,
                }])
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

    async fn send_events<const N: usize>(&self, events: [RuntimeEvent; N]) -> WorkerSend {
        let _event_guard = tokio::select! {
            biased;
            _ = self.external_interruption.cancelled() => {
                return WorkerSend::Interrupted;
            }
            _ = self.work_cancellation.cancelled() => {
                return WorkerSend::Stopped;
            }
            event_guard = self.event_gate.lock() => event_guard,
        };

        for event in events {
            let result = tokio::select! {
                biased;
                _ = self.external_interruption.cancelled() => {
                    return WorkerSend::Interrupted;
                }
                _ = self.work_cancellation.cancelled() => {
                    return WorkerSend::Stopped;
                }
                result = self.events.send(event) => result,
            };
            if result.is_err() {
                return WorkerSend::Closed;
            }
        }

        WorkerSend::Sent
    }

    fn timing_event(&self, milestone: RuntimeTimingMilestone) -> RuntimeEvent {
        RuntimeEvent::Timing {
            turn_id: self.turn_id,
            milestone,
            elapsed_ms: elapsed_milliseconds(self.started_at),
        }
    }

    fn cancellation_outcome(&self) -> Option<SpeechWorkerOutcome> {
        if self.external_interruption.is_cancelled() {
            Some(SpeechWorkerOutcome::Interrupted)
        } else if self.work_cancellation.is_cancelled() {
            Some(SpeechWorkerOutcome::Stopped)
        } else {
            None
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

fn elapsed_milliseconds(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

enum WorkerSend {
    Sent,
    Interrupted,
    Stopped,
    Closed,
}
