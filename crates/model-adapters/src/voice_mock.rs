use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use conversation_protocol::{GenerationId, PlaybackState, SessionId};
use tokio::sync::mpsc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::{
    AdapterError, AdapterFuture, AudioCapture, AudioFrame, CaptureEvent, ContinuousAudioOutput,
    GenerationLanguageModel, GenerationLanguageRequest, GenerationTextDelta, PlaybackReceipt,
    RecognitionEvent, SpeechRecognizer, StreamingSpeechRequest, StreamingSpeechSynthesizer,
    VoiceInput, VoiceInputEvent, VoiceIoFactory, VoiceIoSession,
};

#[derive(Clone, Debug)]
pub struct MockAudioCapture {
    events: Vec<CaptureEvent>,
    scripted_stream: ScriptedStream,
}

impl MockAudioCapture {
    pub fn new<I>(events: I) -> Self
    where
        I: IntoIterator<Item = CaptureEvent>,
    {
        Self {
            events: events.into_iter().collect(),
            scripted_stream: ScriptedStream::default(),
        }
    }

    pub async fn wait_for_blocked_send(&self) {
        self.scripted_stream.wait_for_blocked_send().await;
    }
}

impl AudioCapture for MockAudioCapture {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<CaptureEvent, AdapterError>>> {
        Box::pin(async move {
            Ok(scripted_receiver(
                self.events.clone(),
                cancellation,
                self.scripted_stream.clone(),
            ))
        })
    }
}

#[derive(Clone, Debug)]
pub struct MockSpeechRecognizer {
    events: Vec<RecognitionEvent>,
    scripted_stream: ScriptedStream,
}

impl MockSpeechRecognizer {
    pub fn new<I>(events: I) -> Self
    where
        I: IntoIterator<Item = RecognitionEvent>,
    {
        Self {
            events: events.into_iter().collect(),
            scripted_stream: ScriptedStream::default(),
        }
    }

    pub async fn wait_for_blocked_send(&self) {
        self.scripted_stream.wait_for_blocked_send().await;
    }
}

impl SpeechRecognizer for MockSpeechRecognizer {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<RecognitionEvent, AdapterError>>> {
        Box::pin(async move {
            Ok(scripted_receiver(
                self.events.clone(),
                cancellation,
                self.scripted_stream.clone(),
            ))
        })
    }
}

#[derive(Clone, Debug)]
pub struct MockVoiceInput {
    events: Vec<VoiceInputEvent>,
    scripted_stream: ScriptedStream,
}

impl MockVoiceInput {
    pub fn new<I>(events: I) -> Self
    where
        I: IntoIterator<Item = VoiceInputEvent>,
    {
        Self {
            events: events.into_iter().collect(),
            scripted_stream: ScriptedStream::default(),
        }
    }

    pub async fn wait_for_blocked_send(&self) {
        self.scripted_stream.wait_for_blocked_send().await;
    }
}

impl VoiceInput for MockVoiceInput {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>> {
        Box::pin(async move {
            Ok(scripted_receiver(
                self.events.clone(),
                cancellation,
                self.scripted_stream.clone(),
            ))
        })
    }
}

#[derive(Clone, Debug)]
pub struct MockGenerationLanguageModel {
    deltas: Vec<String>,
    requests: Arc<Mutex<Vec<GenerationLanguageRequest>>>,
    scripted_stream: ScriptedStream,
}

impl MockGenerationLanguageModel {
    pub fn new<I, S>(deltas: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            deltas: deltas.into_iter().map(Into::into).collect(),
            requests: Arc::new(Mutex::new(Vec::new())),
            scripted_stream: ScriptedStream::default(),
        }
    }

    pub fn requests(&self) -> Vec<GenerationLanguageRequest> {
        self.requests
            .lock()
            .expect("mock generation language requests lock poisoned")
            .clone()
    }

    pub async fn wait_for_blocked_send(&self) {
        self.scripted_stream.wait_for_blocked_send().await;
    }
}

impl GenerationLanguageModel for MockGenerationLanguageModel {
    fn stream(
        &self,
        request: GenerationLanguageRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
        self.requests
            .lock()
            .expect("mock generation language requests lock poisoned")
            .push(request.clone());
        let deltas = self
            .deltas
            .iter()
            .cloned()
            .map(|delta| {
                GenerationTextDelta::new(request.turn_id(), request.generation_id(), delta)
            })
            .collect();

        scripted_receiver(deltas, cancellation, self.scripted_stream.clone())
    }
}

#[derive(Clone, Debug)]
pub struct MockStreamingSpeechSynthesizer {
    frames: Vec<AudioFrame>,
    requests: Arc<Mutex<Vec<StreamingSpeechRequest>>>,
    scripted_stream: ScriptedStream,
}

impl MockStreamingSpeechSynthesizer {
    pub fn new<I>(frames: I) -> Self
    where
        I: IntoIterator<Item = AudioFrame>,
    {
        Self {
            frames: frames.into_iter().collect(),
            requests: Arc::new(Mutex::new(Vec::new())),
            scripted_stream: ScriptedStream::default(),
        }
    }

    pub fn requests(&self) -> Vec<StreamingSpeechRequest> {
        self.requests
            .lock()
            .expect("mock streaming speech requests lock poisoned")
            .clone()
    }

    pub async fn wait_for_blocked_send(&self) {
        self.scripted_stream.wait_for_blocked_send().await;
    }
}

impl StreamingSpeechSynthesizer for MockStreamingSpeechSynthesizer {
    fn stream(
        &self,
        request: StreamingSpeechRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<AudioFrame, AdapterError>> {
        self.requests
            .lock()
            .expect("mock streaming speech requests lock poisoned")
            .push(request);
        scripted_receiver(
            self.frames.clone(),
            cancellation,
            self.scripted_stream.clone(),
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct MockContinuousAudioOutput {
    frames: Arc<Mutex<Vec<AudioFrame>>>,
    flushed_generations: Arc<Mutex<Vec<(SessionId, GenerationId)>>>,
}

impl MockContinuousAudioOutput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn frames(&self) -> Vec<AudioFrame> {
        self.frames
            .lock()
            .expect("mock continuous audio frames lock poisoned")
            .clone()
    }

    pub fn flushed_generations(&self) -> Vec<(SessionId, GenerationId)> {
        self.flushed_generations
            .lock()
            .expect("mock continuous audio flushes lock poisoned")
            .clone()
    }
}

impl ContinuousAudioOutput for MockContinuousAudioOutput {
    fn enqueue<'a>(
        &'a self,
        frame: AudioFrame,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, PlaybackReceipt> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(AdapterError::new("continuous audio output cancelled"));
            }

            self.frames
                .lock()
                .expect("mock continuous audio frames lock poisoned")
                .push(frame.clone());
            Ok(PlaybackReceipt::new(
                frame.generation_id(),
                PlaybackState::Accepted,
            ))
        })
    }

    fn flush<'a>(
        &'a self,
        session_id: SessionId,
        generation_id: GenerationId,
    ) -> AdapterFuture<'a, PlaybackReceipt> {
        Box::pin(async move {
            self.flushed_generations
                .lock()
                .expect("mock continuous audio flushes lock poisoned")
                .push((session_id, generation_id));
            Ok(PlaybackReceipt::new(generation_id, PlaybackState::Flushed))
        })
    }
}

#[derive(Clone, Debug)]
pub struct MockVoiceIoFactory {
    events: Vec<VoiceInputEvent>,
    start_count: Arc<AtomicUsize>,
}

impl MockVoiceIoFactory {
    pub fn new<I>(events: I) -> Self
    where
        I: IntoIterator<Item = VoiceInputEvent>,
    {
        Self {
            events: events.into_iter().collect(),
            start_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn start_count(&self) -> usize {
        self.start_count.load(Ordering::SeqCst)
    }
}

impl VoiceIoFactory for MockVoiceIoFactory {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, VoiceIoSession> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(AdapterError::new("voice I/O session cancelled"));
            }

            self.start_count.fetch_add(1, Ordering::SeqCst);
            let completion_cancellation = cancellation.clone();
            Ok(VoiceIoSession {
                input: Arc::new(MockVoiceInput::new(self.events.clone())),
                output: Arc::new(MockContinuousAudioOutput::new()),
                completion: tokio::spawn(async move {
                    completion_cancellation.cancelled().await;
                    Ok(())
                }),
            })
        })
    }
}

fn scripted_receiver<T>(
    events: Vec<T>,
    cancellation: CancellationToken,
    scripted_stream: ScriptedStream,
) -> mpsc::Receiver<Result<T, AdapterError>>
where
    T: Send + 'static,
{
    let (sender, receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        for event in events {
            if sender.capacity() == 0 {
                scripted_stream.blocked_sends.fetch_add(1, Ordering::SeqCst);
                scripted_stream.blocked_send_notify.notify_one();
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return,
                result = sender.send(Ok(event)) => {
                    if result.is_err() {
                        return;
                    }
                }
            }
        }
    });
    receiver
}

#[derive(Clone, Debug, Default)]
struct ScriptedStream {
    blocked_sends: Arc<AtomicUsize>,
    blocked_send_notify: Arc<Notify>,
}

impl ScriptedStream {
    async fn wait_for_blocked_send(&self) {
        loop {
            let notified = self.blocked_send_notify.notified();
            if self.blocked_sends.load(Ordering::SeqCst) > 0 {
                return;
            }
            notified.await;
        }
    }
}
