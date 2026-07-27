use std::sync::Arc;
use std::time::{Duration, Instant};

use conversation_model_adapters::{DiscardAudioOutput, MockLanguageModel, MockSpeechSynthesizer};
use conversation_protocol::{
    RuntimeCommand, RuntimeError, RuntimeErrorKind, RuntimeEvent, RuntimeStage, TurnId,
};
use conversation_runtime::{ConversationRuntime, RuntimeCommandResult};

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
            [1, 2, 3],
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
    let mut recorded_first_text_delta = false;
    while let Some(event) = events.recv().await {
        let label = match event {
            RuntimeEvent::TurnStarted { .. } => Some("turn_started"),
            RuntimeEvent::TranscriptFinal { .. } => Some("transcript_final"),
            RuntimeEvent::TextDelta { .. } if !recorded_first_text_delta => {
                recorded_first_text_delta = true;
                Some("first_text_delta")
            }
            RuntimeEvent::SpeechStarted { .. } => Some("speech_started"),
            RuntimeEvent::SpeechCompleted { .. } => Some("speech_completed"),
            RuntimeEvent::TurnCompleted { .. } => Some("turn_completed"),
            RuntimeEvent::TurnCancelled { .. } => Some("turn_cancelled"),
            RuntimeEvent::TurnFailed { .. } => Some("turn_failed"),
            _ => None,
        };

        if let Some(label) = label {
            samples.push(TimingSample {
                label,
                elapsed: started.elapsed(),
            });
        }
    }

    Ok(samples)
}
