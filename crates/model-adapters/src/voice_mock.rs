use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use conversation_protocol::{GenerationId, PlaybackState, SessionId};
use tokio::sync::mpsc;
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
}

impl MockAudioCapture {
    pub fn new<I>(events: I) -> Self
    where
        I: IntoIterator<Item = CaptureEvent>,
    {
        Self {
            events: events.into_iter().collect(),
        }
    }
}

impl AudioCapture for MockAudioCapture {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<CaptureEvent, AdapterError>>> {
        Box::pin(async move { Ok(scripted_receiver(self.events.clone(), cancellation)) })
    }
}

#[derive(Clone, Debug)]
pub struct MockSpeechRecognizer {
    events: Vec<RecognitionEvent>,
}

impl MockSpeechRecognizer {
    pub fn new<I>(events: I) -> Self
    where
        I: IntoIterator<Item = RecognitionEvent>,
    {
        Self {
            events: events.into_iter().collect(),
        }
    }
}

impl SpeechRecognizer for MockSpeechRecognizer {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<RecognitionEvent, AdapterError>>> {
        Box::pin(async move { Ok(scripted_receiver(self.events.clone(), cancellation)) })
    }
}

#[derive(Clone, Debug)]
pub struct MockVoiceInput {
    events: Vec<VoiceInputEvent>,
}

impl MockVoiceInput {
    pub fn new<I>(events: I) -> Self
    where
        I: IntoIterator<Item = VoiceInputEvent>,
    {
        Self {
            events: events.into_iter().collect(),
        }
    }
}

impl VoiceInput for MockVoiceInput {
    fn start<'a>(
        &'a self,
        _session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>> {
        Box::pin(async move { Ok(scripted_receiver(self.events.clone(), cancellation)) })
    }
}

#[derive(Clone, Debug)]
pub struct MockGenerationLanguageModel {
    deltas: Vec<String>,
    requests: Arc<Mutex<Vec<GenerationLanguageRequest>>>,
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
        }
    }

    pub fn requests(&self) -> Vec<GenerationLanguageRequest> {
        self.requests
            .lock()
            .expect("mock generation language requests lock poisoned")
            .clone()
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

        scripted_receiver(deltas, cancellation)
    }
}

#[derive(Clone, Debug)]
pub struct MockStreamingSpeechSynthesizer {
    frames: Vec<AudioFrame>,
    requests: Arc<Mutex<Vec<StreamingSpeechRequest>>>,
}

impl MockStreamingSpeechSynthesizer {
    pub fn new<I>(frames: I) -> Self
    where
        I: IntoIterator<Item = AudioFrame>,
    {
        Self {
            frames: frames.into_iter().collect(),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn requests(&self) -> Vec<StreamingSpeechRequest> {
        self.requests
            .lock()
            .expect("mock streaming speech requests lock poisoned")
            .clone()
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
        scripted_receiver(self.frames.clone(), cancellation)
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
) -> mpsc::Receiver<Result<T, AdapterError>>
where
    T: Send + 'static,
{
    let (sender, receiver) = mpsc::channel(events.len().max(1));
    tokio::spawn(async move {
        for event in events {
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
