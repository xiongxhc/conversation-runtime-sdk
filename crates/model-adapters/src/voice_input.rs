use conversation_protocol::{is_valid_device_label, SessionId, VoiceActivity};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{
    AdapterError, AdapterFuture, CaptureEvent, CaptureFinishBoundary, CaptureMetadata,
    PlaybackReceipt, RecognitionEvent,
};

type CaptureFinishCompletion = Arc<Mutex<Option<oneshot::Sender<Result<(), AdapterError>>>>>;

#[derive(Clone)]
pub struct CaptureFinish {
    boundary: CaptureFinishBoundary,
    completion: CaptureFinishCompletion,
}

impl CaptureFinish {
    pub fn new(
        boundary: CaptureFinishBoundary,
    ) -> (Self, oneshot::Receiver<Result<(), AdapterError>>) {
        let (sender, receiver) = oneshot::channel();
        (
            Self {
                boundary,
                completion: Arc::new(Mutex::new(Some(sender))),
            },
            receiver,
        )
    }

    pub const fn boundary(&self) -> &CaptureFinishBoundary {
        &self.boundary
    }

    pub fn complete(&self, result: Result<(), AdapterError>) {
        if let Some(completion) = self
            .completion
            .lock()
            .expect("capture finish completion lock poisoned")
            .take()
        {
            let _ = completion.send(result);
        }
    }
}

impl std::fmt::Debug for CaptureFinish {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptureFinish")
            .field("boundary", &self.boundary)
            .finish_non_exhaustive()
    }
}

impl PartialEq for CaptureFinish {
    fn eq(&self, other: &Self) -> bool {
        self.boundary == other.boundary && Arc::ptr_eq(&self.completion, &other.completion)
    }
}

impl Eq for CaptureFinish {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDeviceStatus {
    input_label: String,
    output_label: String,
}

impl AudioDeviceStatus {
    /// Trims device names an adapter reports, then holds them to the client
    /// wire rule so nothing accepted here can fail projection later.
    pub fn new(input_label: &str, output_label: &str) -> Result<Self, AdapterError> {
        let status = Self {
            input_label: input_label.trim().to_owned(),
            output_label: output_label.trim().to_owned(),
        };
        if is_valid_device_label(&status.input_label) && is_valid_device_label(&status.output_label)
        {
            Ok(status)
        } else {
            Err(AdapterError::new("invalid audio device label"))
        }
    }

    pub fn input_label(&self) -> &str {
        &self.input_label
    }

    pub fn output_label(&self) -> &str {
        &self.output_label
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum VoiceInputEvent {
    DeviceStatus(AudioDeviceStatus),
    Activity(VoiceActivity),
    Capture(CaptureEvent),
    /// Recorder-only evidence; never submitted to a speech recognizer or activity gate.
    CaptureMetadata(CaptureMetadata),
    /// Recorder-only FIFO barrier; never submitted to recognition or activity gating.
    CaptureFinish(CaptureFinish),
    Recognition(RecognitionEvent),
    Playback(PlaybackReceipt),
}

/// Streams fused capture and recognition events for one voice session.
///
/// Implementations must observe `cancellation`, stop session-owned work, and
/// close the returned receiver only after cleanup completes.
pub trait VoiceInput: Send + Sync {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>>;
}

#[cfg(test)]
mod tests {
    use conversation_protocol::{
        ClientVoiceSessionEvent, SessionId, VoiceSessionEvent, MAX_CLIENT_DEVICE_LABEL_BYTES,
    };

    use crate::CaptureFinishBoundary;

    use super::{AudioDeviceStatus, CaptureFinish};

    #[tokio::test]
    async fn capture_finish_completes_once_and_closes_when_its_owner_drops() {
        let boundary = CaptureFinishBoundary::new(SessionId::new(1), 2, false, 0, 0, 3).unwrap();
        let (finish, completion) = CaptureFinish::new(boundary.clone());
        finish
            .clone()
            .complete(Err(crate::AdapterError::new("first completion")));
        finish.complete(Ok(()));
        assert_eq!(
            completion.await.unwrap().unwrap_err().message(),
            "first completion"
        );

        let (abandoned, completion) = CaptureFinish::new(boundary);
        drop(abandoned);
        assert!(completion.await.is_err());
    }

    #[test]
    fn audio_device_status_accepts_unicode_labels() {
        let status = AudioDeviceStatus::new("MacBook Pro Microphone", "Chris 的 AirPods").unwrap();

        assert_eq!(status.input_label(), "MacBook Pro Microphone");
        assert_eq!(status.output_label(), "Chris 的 AirPods");
    }

    #[test]
    fn audio_device_status_trims_padded_sidecar_labels() {
        let status = AudioDeviceStatus::new("USB Audio Device \n", "\tSpeakers ").unwrap();

        assert_eq!(status.input_label(), "USB Audio Device");
        assert_eq!(status.output_label(), "Speakers");
    }

    #[test]
    fn audio_device_status_rejects_labels_that_stay_invalid_after_trimming() {
        assert!(AudioDeviceStatus::new("", "Speakers").is_err());
        assert!(AudioDeviceStatus::new("  \n", "Speakers").is_err());
        assert!(AudioDeviceStatus::new("Microphone", "").is_err());
        assert!(AudioDeviceStatus::new(
            &format!(" {} ", "x".repeat(MAX_CLIENT_DEVICE_LABEL_BYTES + 1)),
            "Speakers"
        )
        .is_err());
    }

    #[test]
    fn accepted_audio_device_labels_survive_client_wire_projection() {
        let status = AudioDeviceStatus::new(
            "USB Audio Device ",
            &format!(" {} ", "麦".repeat(MAX_CLIENT_DEVICE_LABEL_BYTES / 3)),
        )
        .unwrap();

        assert!(
            ClientVoiceSessionEvent::try_from(VoiceSessionEvent::DeviceStatus {
                session_id: SessionId::new(1),
                input_label: status.input_label().to_owned(),
                output_label: status.output_label().to_owned(),
            })
            .is_ok()
        );
    }
}
