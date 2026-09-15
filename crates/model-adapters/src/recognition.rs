use conversation_protocol::{SessionId, UtteranceCaptureEvidence};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{AdapterError, AdapterFuture};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecognitionHypothesis {
    segment_id: u64,
    text: String,
    engine_final: bool,
}

impl RecognitionHypothesis {
    pub fn partial(segment_id: u64, text: impl Into<String>) -> Self {
        Self {
            segment_id,
            text: text.into(),
            engine_final: false,
        }
    }

    pub fn engine_final(segment_id: u64, text: impl Into<String>) -> Self {
        Self {
            segment_id,
            text: text.into(),
            engine_final: true,
        }
    }

    pub const fn segment_id(&self) -> u64 {
        self.segment_id
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn is_engine_final(&self) -> bool {
        self.engine_final
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RecognitionEvent {
    Hypothesis(RecognitionHypothesis),
    FinalizedUtterance(FinalizedUtterance),
}

/// Already-finalized wording, distinct from fragments that require local silence
/// aggregation. The provider identity and provider receipt clock never escape.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizedUtterance {
    segment_id: u64,
    text: String,
    capture_evidence: Option<UtteranceCaptureEvidence>,
}

impl FinalizedUtterance {
    pub fn new(segment_id: u64, text: impl Into<String>) -> Result<Self, AdapterError> {
        let text = text.into();
        if segment_id == 0 || text.trim().is_empty() || text.len() > 16 * 1024 {
            return Err(AdapterError::new("invalid finalized utterance"));
        }
        Ok(Self {
            segment_id,
            text,
            capture_evidence: None,
        })
    }
    pub fn with_capture_evidence(
        mut self,
        evidence: UtteranceCaptureEvidence,
    ) -> Result<Self, AdapterError> {
        evidence
            .validate()
            .map_err(|_| AdapterError::new("invalid finalized capture evidence"))?;
        self.capture_evidence = Some(evidence);
        Ok(self)
    }
    pub const fn segment_id(&self) -> u64 {
        self.segment_id
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn capture_evidence(&self) -> Option<&UtteranceCaptureEvidence> {
        self.capture_evidence.as_ref()
    }
}

/// The event channel and the actual session-owned worker completion. Owners must
/// cancel and await completion; channel EOF alone is not cleanup evidence.
#[derive(Debug)]
pub struct RecognitionStream<T = RecognitionEvent> {
    pub events: mpsc::Receiver<Result<T, AdapterError>>,
    pub completion: JoinHandle<Result<(), AdapterError>>,
}

impl<T> RecognitionStream<T> {
    pub async fn recv(&mut self) -> Option<Result<T, AdapterError>> {
        self.events.recv().await
    }
}

/// Streams recognition events for one voice session.
///
/// Implementations must observe `cancellation`, stop session-owned work, and
/// close the returned receiver only after cleanup completes.
pub trait SpeechRecognizer: Send + Sync {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, RecognitionStream>;
}
