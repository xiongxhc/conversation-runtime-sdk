use std::sync::Arc;
use std::time::{Duration, Instant};

use conversation_model_adapters::{DiscardAudioOutput, MockLanguageModel, MockSpeechSynthesizer};
use conversation_protocol::{
    RuntimeCommand, RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStage,
    RuntimeTimingMilestone, TurnId,
};
use conversation_runtime::{ConversationRuntime, RuntimeCommandResult};

fn minimal_aiff() -> Vec<u8> {
    let mut bytes = Vec::from(&b"FORM"[..]);
    bytes.extend_from_slice(&48_u32.to_be_bytes());
    bytes.extend_from_slice(b"AIFFCOMM");
    bytes.extend_from_slice(&18_u32.to_be_bytes());
    bytes.extend_from_slice(&[0; 18]);
    bytes.extend_from_slice(b"SSND");
    bytes.extend_from_slice(&9_u32.to_be_bytes());
    bytes.extend_from_slice(&[0; 8]);
    bytes.extend_from_slice(&[0x80, 0]);
    bytes
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimingSample {
    label: &'static str,
    elapsed: Duration,
}

impl TimingSample {
    pub const fn label(&self) -> &'static str {
        self.label
    }

    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }
}

pub async fn measure_mock_turn(transcript: &str) -> Result<Vec<TimingSample>, RuntimeError> {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::delayed(
            ["A brief mock response."],
            Duration::from_millis(5),
        )),
        Arc::new(MockSpeechSynthesizer::delayed(
            minimal_aiff(),
            Duration::from_millis(5),
        )),
        Arc::new(DiscardAudioOutput),
    );
    let started = Instant::now();
    let result = runtime
        .execute(RuntimeCommand::StartTurn {
            turn_id: TurnId::new(1),
            transcript: transcript.into(),
        })
        .await?;
    let mut events = match result {
        RuntimeCommandResult::TurnStarted { events } => events,
        RuntimeCommandResult::InterruptAccepted => {
            return Err(RuntimeError::new(
                RuntimeErrorKind::InvalidState,
                RuntimeStage::Runtime,
                "start command returned an interrupt result",
            ));
        }
        _ => {
            return Err(RuntimeError::new(
                RuntimeErrorKind::InvalidState,
                RuntimeStage::Runtime,
                "start command returned an unknown result",
            ));
        }
    };

    let mut samples = Vec::new();
    while let Some(event) = events.recv().await {
        let checkpoint = match event {
            RuntimeEvent::TurnStarted { .. } => Some(("turn_started", None)),
            RuntimeEvent::TranscriptFinal { .. } => Some(("transcript_final", None)),
            RuntimeEvent::Timing {
                milestone,
                elapsed_ms,
                ..
            } => {
                let label = match milestone {
                    RuntimeTimingMilestone::FirstTextDelta => "first_text_delta",
                    RuntimeTimingMilestone::FirstSynthesisRequest => "first_synthesis_request",
                    RuntimeTimingMilestone::FirstPlayableAudio => "first_playable_audio",
                    _ => continue,
                };
                Some((label, Some(Duration::from_millis(elapsed_ms))))
            }
            RuntimeEvent::SpeechStarted { .. } => Some(("speech_started", None)),
            RuntimeEvent::SpeechCompleted { .. } => Some(("speech_completed", None)),
            RuntimeEvent::TurnCompleted { .. } => Some(("turn_completed", None)),
            RuntimeEvent::TurnCancelled { .. } => Some(("turn_cancelled", None)),
            RuntimeEvent::TurnFailed { .. } => Some(("turn_failed", None)),
            _ => None,
        };

        if let Some((label, runtime_elapsed)) = checkpoint {
            let measured_elapsed = runtime_elapsed.unwrap_or_else(|| started.elapsed());
            let elapsed = samples
                .last()
                .map_or(measured_elapsed, |sample: &TimingSample| {
                    sample.elapsed.max(measured_elapsed)
                });
            samples.push(TimingSample { label, elapsed });
        }
    }

    Ok(samples)
}
