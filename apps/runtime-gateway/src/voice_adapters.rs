use std::sync::Arc;

use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioCapture, OpenAiSpeechRecognitionConfig,
    OpenAiSpeechRecognizer, RecognitionEvent, SpeechRecognizer, StreamingSpeechSynthesizer,
    VoiceInput, VoiceInputEvent, VoiceIoFactory, VoiceIoSession,
};
use conversation_protocol::{
    ComponentDescriptor, PrivacyMode, RuntimeError, RuntimeStage, SessionId, VoiceActivity,
    VoiceSessionPolicy,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct GatewayVoiceAdapters {
    /// Local voice I/O for the offline profile. An explicit online meeting supplies the Task 6
    /// network I/O at session start instead; its placeholder fails closed if no media bridge is
    /// configured and therefore cannot become a sidecar fallback.
    pub io: Arc<dyn VoiceIoFactory>,
    pub speech: Arc<dyn StreamingSpeechSynthesizer>,
    pub policy: VoicePolicyTemplate,
    /// The remote recognizer selected by an online profile. Its capture is supplied only by the
    /// production media lease, after Task 6 attached that exact bounded receiver.
    pub online_recognition: Option<OpenAiSpeechRecognitionConfig>,
}

impl GatewayVoiceAdapters {
    pub fn unavailable_online_io() -> Arc<dyn VoiceIoFactory> {
        Arc::new(UnavailableOnlineVoiceIoFactory)
    }

    /// Binds the selected online recognizer to the one capture branch Task 6 attached before the
    /// media lease was disclosed. There is no alternate capture or offline fallback path.
    pub fn online_media_io(
        &self,
        media_io: Arc<dyn VoiceIoFactory>,
        capture: Arc<dyn AudioCapture>,
    ) -> Option<Arc<dyn VoiceIoFactory>> {
        self.online_recognition.as_ref().map(|config| {
            Arc::new(OnlineRecognitionVoiceIoFactory {
                media_io,
                recognizer: Arc::new(OpenAiSpeechRecognizer::new(config.clone(), capture)),
            }) as Arc<dyn VoiceIoFactory>
        })
    }
}

struct UnavailableOnlineVoiceIoFactory;

impl VoiceIoFactory for UnavailableOnlineVoiceIoFactory {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        _cancellation: CancellationToken,
    ) -> AdapterFuture<'a, VoiceIoSession> {
        Box::pin(async {
            Err(AdapterError::new(
                "online voice requires a configured media bridge",
            ))
        })
    }
}

struct OnlineRecognitionVoiceIoFactory {
    media_io: Arc<dyn VoiceIoFactory>,
    recognizer: Arc<OpenAiSpeechRecognizer>,
}

impl VoiceIoFactory for OnlineRecognitionVoiceIoFactory {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, VoiceIoSession> {
        Box::pin(async move {
            let session = self
                .media_io
                .start(session_id, cancellation.clone())
                .await?;
            Ok(VoiceIoSession {
                input: Arc::new(OnlineRecognitionVoiceInput {
                    media_input: session.input,
                    recognizer: Arc::clone(&self.recognizer),
                }),
                capture: session.capture,
                output: session.output,
                completion: session.completion,
            })
        })
    }
}

struct OnlineRecognitionVoiceInput {
    media_input: Arc<dyn VoiceInput>,
    recognizer: Arc<OpenAiSpeechRecognizer>,
}

impl VoiceInput for OnlineRecognitionVoiceInput {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>> {
        Box::pin(async move {
            let mut media = self
                .media_input
                .start(session_id, cancellation.clone())
                .await?;
            let mut recognition = self
                .recognizer
                .start(session_id, cancellation.clone())
                .await
                .map_err(|error| error.with_stage(RuntimeStage::SpeechRecognizer))?;
            let (sender, receiver) = mpsc::channel(16);
            tokio::spawn(async move {
                let mut recognition_open = true;
                loop {
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => return,
                        input = media.recv() => match input {
                            Some(input) => if sender.send(input).await.is_err() { return; },
                            None => return,
                        },
                        event = recognition.recv(), if recognition_open => match event {
                            Some(Ok(RecognitionEvent::Hypothesis(hypothesis))) => {
                                if hypothesis.is_engine_final() {
                                    if sender.send(Ok(VoiceInputEvent::Activity(VoiceActivity::SpeechStarted { at_ms: 0 }))).await.is_err() { return; }
                                    if sender.send(Ok(VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(hypothesis)))).await.is_err() { return; }
                                    if sender.send(Ok(VoiceInputEvent::Activity(VoiceActivity::SpeechEnded { at_ms: 0 }))).await.is_err() { return; }
                                } else if sender.send(Ok(VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(hypothesis)))).await.is_err() { return; }
                            }
                            Some(Err(error)) => if sender.send(Err(error)).await.is_err() { return; },
                            Some(Ok(_)) => {
                                if sender.send(Err(AdapterError::new("online recognition event is unsupported"))).await.is_err() { return; }
                            }
                            None => recognition_open = false,
                        },
                    }
                }
            });
            Ok(receiver)
        })
    }
}

#[derive(Clone, Debug)]
pub struct VoicePolicyTemplate {
    privacy_mode: PrivacyMode,
    speech_start_ms: u64,
    final_silence_ms: u64,
    components: Vec<ComponentDescriptor>,
}

impl VoicePolicyTemplate {
    pub(crate) fn new(
        privacy_mode: PrivacyMode,
        speech_start_ms: u64,
        final_silence_ms: u64,
        components: Vec<ComponentDescriptor>,
    ) -> Result<Self, RuntimeError> {
        VoiceSessionPolicy::new(
            SessionId::new(1),
            privacy_mode,
            speech_start_ms,
            final_silence_ms,
            components.clone(),
        )?;
        Ok(Self {
            privacy_mode,
            speech_start_ms,
            final_silence_ms,
            components,
        })
    }

    pub fn for_session(&self, session_id: SessionId) -> Result<VoiceSessionPolicy, RuntimeError> {
        VoiceSessionPolicy::new(
            session_id,
            self.privacy_mode,
            self.speech_start_ms,
            self.final_silence_ms,
            self.components.clone(),
        )
    }
}
