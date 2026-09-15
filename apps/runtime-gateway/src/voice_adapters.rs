use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{stream::FuturesUnordered, StreamExt};

use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioCapture, CaptureEvent, OpenAiSpeechRecognitionConfig,
    OpenAiSpeechRecognizer, RecognitionEvent, SpeechRecognizer, StreamingSpeechSynthesizer,
    VoiceInput, VoiceInputEvent, VoiceIoFactory, VoiceIoSession,
};
use conversation_protocol::{
    ComponentDescriptor, PrivacyMode, RuntimeError, RuntimeStage, SessionId, VoiceSessionPolicy,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

type OwnedWorker = JoinHandle<Result<(), AdapterError>>;
type RecognitionWorkers = [OwnedWorker; 2];

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
                config: config.clone(),
                capture,
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
    config: OpenAiSpeechRecognitionConfig,
    capture: Arc<dyn AudioCapture>,
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
            let (relay_sender, relay_receiver) = oneshot::channel();
            let (drain_sender, drain_receiver) = oneshot::channel();
            let recognizer = Arc::new(OpenAiSpeechRecognizer::new(
                self.config.clone(),
                Arc::new(MediaLifetimeCapture {
                    source: self.capture.clone(),
                    cancellation: cancellation.child_token(),
                    completion: Mutex::new(Some(drain_sender)),
                }),
            ));
            let completion = tokio::spawn(complete_online_io(
                session.completion,
                relay_receiver,
                drain_receiver,
                cancellation,
            ));
            Ok(VoiceIoSession {
                input: Arc::new(OnlineRecognitionVoiceInput {
                    media_input: session.input,
                    recognizer,
                    relay_completion: Mutex::new(Some(relay_sender)),
                }),
                capture: session.capture,
                output: session.output,
                completion,
            })
        })
    }
}

/// The atomic network fan-out retains its recognition consumer for the MEDIA
/// lifetime, not the provider socket lifetime. After ASR closes, only this copy
/// is discarded; the independent recorder/voice copy keeps flowing unchanged.
/// One queued frame plus one pending send, each validated <=64 KiB. No replay.
struct MediaLifetimeCapture {
    source: Arc<dyn AudioCapture>,
    cancellation: CancellationToken,
    completion: Mutex<Option<oneshot::Sender<OwnedWorker>>>,
}

impl AudioCapture for MediaLifetimeCapture {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        _recognition_cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<CaptureEvent, AdapterError>>> {
        Box::pin(async move {
            let completion = self
                .completion
                .lock()
                .map_err(|_| AdapterError::new("online capture ownership lock failed"))?
                .take()
                .ok_or_else(|| AdapterError::new("online capture already started"))?;
            let mut source = self
                .source
                .start(session_id, self.cancellation.clone())
                .await?;
            let (sender, receiver) = mpsc::channel(1);
            let cancellation = self.cancellation.clone();
            let worker = tokio::spawn(async move {
                loop {
                    let event = tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => return Ok(()),
                        event = source.recv() => match event {
                            Some(event) => event,
                            None => return Ok(()),
                        },
                    };
                    if !sender.is_closed() {
                        tokio::select! {
                            biased;
                            _ = cancellation.cancelled() => return Ok(()),
                            _ = sender.closed() => {},
                            _ = sender.send(event) => {},
                        }
                    }
                }
            });
            if let Err(worker) = completion.send(worker) {
                self.cancellation.cancel();
                await_owned_worker(worker, self.cancellation.clone()).await?;
                return Err(AdapterError::new("online capture session already ended"));
            }
            Ok(receiver)
        })
    }
}

struct OnlineRecognitionVoiceInput {
    media_input: Arc<dyn VoiceInput>,
    recognizer: Arc<OpenAiSpeechRecognizer>,
    relay_completion: Mutex<Option<oneshot::Sender<RecognitionWorkers>>>,
}

impl VoiceInput for OnlineRecognitionVoiceInput {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>> {
        Box::pin(async move {
            let completion_sender = self
                .relay_completion
                .lock()
                .map_err(|_| AdapterError::new("online recognition ownership lock failed"))?
                .take()
                .ok_or_else(|| AdapterError::new("online recognition input already started"))?;
            let recognition_cancellation = cancellation.child_token();
            let media = self
                .media_input
                .start(session_id, cancellation.clone())
                .await?;
            let recognition = self
                .recognizer
                .start(session_id, recognition_cancellation.clone())
                .await
                .map_err(|error| error.with_stage(RuntimeStage::SpeechRecognizer))?;
            let (sender, receiver) = mpsc::channel(16);
            let actor = recognition.completion;
            let recognition = recognition.events;
            let late_start_cancellation = recognition_cancellation.clone();
            let completion = tokio::spawn(relay_online_input(
                media,
                recognition,
                sender,
                cancellation,
                recognition_cancellation,
            ));
            if let Err([relay, actor]) = completion_sender.send([completion, actor]) {
                // Factory cleanup may win while input.start is awaiting capture.
                // The rejected transfer still owns and reaps this exact relay.
                late_start_cancellation.cancel();
                let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
                let (relay, actor) = tokio::join!(
                    reap_worker_until(relay, deadline),
                    reap_worker_until(actor, deadline)
                );
                relay.and(actor)?;
                return Err(AdapterError::new(
                    "online recognition session already ended",
                ));
            }
            Ok(receiver)
        })
    }
}

async fn relay_online_input(
    mut media: mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>,
    mut recognition: mpsc::Receiver<Result<RecognitionEvent, AdapterError>>,
    sender: mpsc::Sender<Result<VoiceInputEvent, AdapterError>>,
    cancellation: CancellationToken,
    recognition_cancellation: CancellationToken,
) -> Result<(), AdapterError> {
    let relay = async {
        let mut recognition_open = true;
        let mut media_due = false;
        loop {
            // Alternate when BOTH ports stay ready: wording must not hide behind
            // continuous PCM, and a final flood must not starve recording either.
            if media_due {
                match media.try_recv() {
                    Ok(input) => {
                        if sender.send(input).await.is_err() {
                            return;
                        }
                        media_due = false;
                        continue;
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => return,
                    Err(mpsc::error::TryRecvError::Empty) => {}
                }
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return,
                event = recognition.recv(), if recognition_open => {
                    media_due = true;
                    match event {
                    Some(Ok(event @ RecognitionEvent::FinalizedUtterance(_))) => {
                        if sender.send(Ok(VoiceInputEvent::Recognition(event))).await.is_err() { return; }
                    }
                    Some(Ok(RecognitionEvent::Hypothesis(hypothesis))) => {
                        if sender.send(Ok(VoiceInputEvent::Recognition(RecognitionEvent::Hypothesis(hypothesis)))).await.is_err() { return; }
                    }
                    Some(Err(error)) => if sender.send(Err(error)).await.is_err() { return; },
                    Some(Ok(_)) => {
                        if sender.send(Err(AdapterError::new("online recognition event is unsupported"))).await.is_err() { return; }
                    }
                    None => recognition_open = false,
                    }
                },
                input = media.recv() => match input {
                    Some(input) => {
                        media_due = false;
                        if sender.send(input).await.is_err() { return; }
                    },
                    None => return,
                },
            }
        }
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => {},
        _ = recognition_cancellation.cancelled() => {},
        _ = sender.closed() => {},
        _ = relay => {},
    }
    recognition_cancellation.cancel();
    drop(recognition);
    Ok(())
}

async fn await_owned_worker(
    mut worker: OwnedWorker,
    cancellation: CancellationToken,
) -> Result<(), AdapterError> {
    tokio::select! {
        biased;
        result = &mut worker => result.map_err(|_| AdapterError::new("online recognition worker failed"))?,
        _ = cancellation.cancelled() => reap_worker_until(worker, tokio::time::Instant::now() + Duration::from_secs(2)).await,
    }
}

async fn reap_worker_until(
    mut worker: OwnedWorker,
    deadline: tokio::time::Instant,
) -> Result<(), AdapterError> {
    match tokio::time::timeout_at(deadline, &mut worker).await {
        Ok(result) => result.map_err(|_| AdapterError::new("online recognition worker failed"))?,
        Err(_) => {
            worker.abort();
            let _ = worker.await;
            Err(AdapterError::new(
                "online recognition cleanup deadline expired",
            ))
        }
    }
}

/// Direct ownership: no task in this set owns another task. A single absolute
/// cancellation deadline covers every worker and late transfer; all remaining
/// handles are aborted then joined, preserving the first observed failure.
async fn complete_online_io(
    media: OwnedWorker,
    mut recognition: oneshot::Receiver<RecognitionWorkers>,
    mut drain: oneshot::Receiver<OwnedWorker>,
    cancellation: CancellationToken,
) -> Result<(), AdapterError> {
    let mut workers = FuturesUnordered::new();
    workers.push(media);
    let (mut recognition_open, mut drain_open) = (true, true);
    let mut deadline = None;
    let mut first_error = None;
    loop {
        if workers.is_empty() && !recognition_open && !drain_open {
            return first_error.map_or(Ok(()), Err);
        }
        tokio::select! {
            biased;
            result = workers.next(), if !workers.is_empty() => {
                let result = result.expect("owned worker present").unwrap_or_else(|_| Err(AdapterError::new("online recognition worker failed")));
                if let Err(error) = result { first_error.get_or_insert(error); }
            },
            result = &mut recognition, if recognition_open => {
                recognition_open = false;
                if let Ok(handles) = result { workers.extend(handles); }
            },
            result = &mut drain, if drain_open => {
                drain_open = false;
                if let Ok(handle) = result { workers.push(handle); }
            },
            _ = cancellation.cancelled(), if deadline.is_none() => {
                deadline = Some(tokio::time::Instant::now() + Duration::from_secs(2));
                recognition.close(); drain.close();
                if recognition_open {
                    if let Ok(handles) = recognition.try_recv() { workers.extend(handles); }
                    recognition_open = false;
                }
                if drain_open {
                    if let Ok(handle) = drain.try_recv() { workers.push(handle); }
                    drain_open = false;
                }
            },
            _ = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                first_error.get_or_insert_with(|| AdapterError::new("online recognition cleanup deadline expired"));
                for worker in workers.iter() { worker.abort(); }
                while workers.next().await.is_some() {}
                return Err(first_error.expect("cleanup failure retained"));
            },
        }
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

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use conversation_model_adapters::{
        MockAudioCapture, MockContinuousAudioOutput, MockVoiceCaptureControl,
    };
    use conversation_protocol::VoiceActivity;
    use std::sync::Mutex;
    use std::time::Duration;

    struct ControlledInput(Mutex<Option<mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>>>);
    impl VoiceInput for ControlledInput {
        fn start<'a>(
            &'a self,
            _: SessionId,
            _: CancellationToken,
        ) -> AdapterFuture<'a, mpsc::Receiver<Result<VoiceInputEvent, AdapterError>>> {
            Box::pin(async move { Ok(self.0.lock().unwrap().take().unwrap()) })
        }
    }
    struct ControlledFactory(Arc<ControlledInput>);
    impl VoiceIoFactory for ControlledFactory {
        fn start<'a>(
            &'a self,
            _: SessionId,
            cancellation: CancellationToken,
        ) -> AdapterFuture<'a, VoiceIoSession> {
            Box::pin(async move {
                Ok(VoiceIoSession {
                    input: self.0.clone(),
                    capture: Arc::new(MockVoiceCaptureControl::new()),
                    output: Arc::new(MockContinuousAudioOutput::new()),
                    completion: tokio::spawn(async move {
                        cancellation.cancelled().await;
                        Ok(())
                    }),
                })
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_ready_final_is_not_starved_by_ready_media_capture() {
        let (media_sender, media) = mpsc::channel(4);
        for at_ms in 1..=4 {
            media_sender
                .send(Ok(VoiceInputEvent::Activity(
                    VoiceActivity::SpeechContinued { at_ms },
                )))
                .await
                .unwrap();
        }
        let (recognition_sender, recognition) = mpsc::channel(2);
        recognition_sender
            .send(Ok(RecognitionEvent::FinalizedUtterance(
                conversation_model_adapters::FinalizedUtterance::new(1, "ready wording").unwrap(),
            )))
            .await
            .unwrap();
        recognition_sender
            .send(Ok(RecognitionEvent::FinalizedUtterance(
                conversation_model_adapters::FinalizedUtterance::new(2, "also ready").unwrap(),
            )))
            .await
            .unwrap();
        let (sender, mut events) = mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let worker = tokio::spawn(relay_online_input(
            media,
            recognition,
            sender,
            cancellation.clone(),
            cancellation.child_token(),
        ));
        let first = events.recv().await.unwrap().unwrap();
        let second = events.recv().await.unwrap().unwrap();
        cancellation.cancel();
        await_owned_worker(worker, cancellation).await.unwrap();
        assert!(
            matches!(
                first,
                VoiceInputEvent::Recognition(RecognitionEvent::FinalizedUtterance(_))
            ),
            "an already-ready final must not wait behind a continuous ready media stream"
        );
        assert!(
            matches!(second, VoiceInputEvent::Activity(_)),
            "ready recording/media must get the next slot"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shared_cleanup_deadline_reaps_every_held_worker_and_preserves_first_failure() {
        struct Dropped(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (ready, mut started) = mpsc::channel(3);
        let spawn_held = || {
            let guard = Dropped(dropped.clone());
            let ready = ready.clone();
            tokio::spawn(async move {
                let _guard = guard;
                ready.send(()).await.unwrap();
                std::future::pending::<Result<(), AdapterError>>().await
            })
        };
        let (recognition_sender, recognition) = oneshot::channel();
        let (drain_sender, drain) = oneshot::channel();
        recognition_sender
            .send([spawn_held(), spawn_held()])
            .unwrap();
        drain_sender.send(spawn_held()).unwrap();
        let cancellation = CancellationToken::new();
        let worker = tokio::spawn(complete_online_io(
            tokio::spawn(async { Err(AdapterError::new("first media failure")) }),
            recognition,
            drain,
            cancellation.clone(),
        ));
        for _ in 0..3 {
            started.recv().await.unwrap();
        }
        tokio::task::yield_now().await;
        let cancelled_at = tokio::time::Instant::now();
        cancellation.cancel();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(1_999)).await;
        assert!(
            !worker.is_finished(),
            "held workers must be joined or aborted, not dropped"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        let result = worker.await.unwrap().unwrap_err();
        assert_eq!(result.message(), "first media failure");
        assert_eq!(
            tokio::time::Instant::now() - cancelled_at,
            Duration::from_secs(2)
        );
        assert_eq!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "every held worker must be reaped by the shared deadline"
        );
    }

    struct ControlledCapture(Mutex<Option<mpsc::Receiver<Result<CaptureEvent, AdapterError>>>>);
    impl AudioCapture for ControlledCapture {
        fn start<'a>(
            &'a self,
            _: SessionId,
            _: CancellationToken,
        ) -> AdapterFuture<'a, mpsc::Receiver<Result<CaptureEvent, AdapterError>>> {
            Box::pin(async move { Ok(self.0.lock().unwrap().take().unwrap()) })
        }
    }

    #[tokio::test]
    async fn media_lifetime_capture_bounds_forwarding_then_drains_after_provider_drop() {
        let (source, receiver) = mpsc::channel(1);
        let (transfer, completion) = oneshot::channel();
        let cancellation = CancellationToken::new();
        let capture = MediaLifetimeCapture {
            source: Arc::new(ControlledCapture(Mutex::new(Some(receiver)))),
            cancellation: cancellation.clone(),
            completion: Mutex::new(Some(transfer)),
        };
        let provider = capture
            .start(SessionId::new(1), CancellationToken::new())
            .await
            .unwrap();
        let worker = completion.await.unwrap();
        let frame = conversation_model_adapters::AudioFrame::new(
            conversation_protocol::TurnId::new(1),
            conversation_protocol::GenerationId::new(1),
            conversation_protocol::UtteranceId::new(1),
            1,
            conversation_model_adapters::PcmFormat::new(
                24_000,
                1,
                conversation_model_adapters::PcmSampleFormat::Signed16LittleEndian,
            )
            .unwrap(),
            vec![0; 64 * 1024],
        )
        .unwrap();
        // One forwarded + one pending + the capacity-one test source. The
        // fourth cannot enter while the provider holds its full receiver.
        tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..3 {
                source
                    .send(Ok(CaptureEvent::Frame(frame.clone())))
                    .await
                    .unwrap();
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            source.try_send(Ok(CaptureEvent::Frame(frame.clone()))),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        drop(provider);
        tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..32 {
                source
                    .send(Ok(CaptureEvent::Frame(frame.clone())))
                    .await
                    .unwrap();
            }
        })
        .await
        .unwrap();
        assert!(
            !worker.is_finished(),
            "provider EOF must not end media draining"
        );
        cancellation.cancel();
        await_owned_worker(worker, cancellation).await.unwrap();
        assert!(
            source.is_closed(),
            "completion must include the exact source receiver"
        );
    }

    #[tokio::test]
    async fn media_lifetime_capture_reaps_late_completion_transfer_without_cancelling_parent() {
        let (source, receiver) = mpsc::channel(1);
        let (transfer, completion) = oneshot::channel();
        drop(completion);
        let parent = CancellationToken::new();
        let capture = MediaLifetimeCapture {
            source: Arc::new(ControlledCapture(Mutex::new(Some(receiver)))),
            cancellation: parent.child_token(),
            completion: Mutex::new(Some(transfer)),
        };
        assert!(capture
            .start(SessionId::new(1), CancellationToken::new())
            .await
            .is_err());
        assert!(
            source.is_closed(),
            "rejected transfer must reap, not detach, the drain"
        );
        assert!(!parent.is_cancelled());
    }

    #[tokio::test]
    async fn online_factory_completion_joins_a_relay_blocked_on_full_downstream() {
        const ENV: &str = "CONVERSATION_TASK11_GATEWAY_OWNERSHIP_TEST";
        std::env::set_var(ENV, "fake-task11-credential");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = OpenAiSpeechRecognitionConfig::new(
            format!(
                "ws://{}/v1/realtime?intent=transcription",
                listener.local_addr().unwrap()
            ),
            "configured-model",
            ENV,
        )
        .unwrap()
        .with_server_vad(0.5, 300, 500)
        .unwrap();
        let (sender, receiver) = mpsc::channel(1);
        let factory = OnlineRecognitionVoiceIoFactory {
            media_io: Arc::new(ControlledFactory(Arc::new(ControlledInput(Mutex::new(
                Some(receiver),
            ))))),
            config,
            capture: Arc::new(MockAudioCapture::new([])),
        };
        let cancellation = CancellationToken::new();
        let session = factory
            .start(SessionId::new(7), cancellation.clone())
            .await
            .unwrap();
        let input = session
            .input
            .start(SessionId::new(7), cancellation.clone())
            .await
            .unwrap();
        // Capacity16 is full and the seventeenth relay send is pending when the
        // source's eighteenth send enters its capacity1 queue. No sleep race.
        tokio::time::timeout(Duration::from_secs(2), async {
            for at_ms in 0..18 {
                sender
                    .send(Ok(VoiceInputEvent::Activity(
                        VoiceActivity::SpeechContinued { at_ms },
                    )))
                    .await
                    .unwrap();
            }
        })
        .await
        .unwrap();
        cancellation.cancel();
        let completed = tokio::time::timeout(Duration::from_secs(2), session.completion)
            .await
            .unwrap()
            .unwrap();
        let relay_closed = input.is_closed();
        drop(input);
        drop(sender);
        drop(listener);
        assert_eq!(completed, Ok(()));
        assert!(
            relay_closed,
            "factory completion returned while its blocked relay still owned the output sender"
        );
    }
}
