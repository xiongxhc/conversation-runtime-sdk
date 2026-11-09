mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use conversation_memory::{
    MemoryContextProvider, MemoryProviderFuture, SqliteMemoryContextProvider, SystemMemoryClock,
};
use conversation_model_adapters::{
    AdapterError, AudioFrame, MockContinuousAudioOutput, MockGenerationLanguageModel, PcmFormat,
    PcmSampleFormat, StreamingSpeechRequest, StreamingSpeechSynthesizer,
};
use conversation_protocol::{
    ConversationMode, ExecutionLocation, PersonId, PersonaProfile, ResponseControls, RuntimeEvent,
    SessionId, SpeakerTag, TurnId,
};
use conversation_runtime::{
    ConversationContext, ConversationQualityController, ConversationTurnSource,
    SessionSpeakerIdentity, StreamingTurnRuntime, TurnVoiceRequest,
};
use support::speaker_fakes::{at, StageFixture, CHRIS};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

struct SubjectRecorder {
    inner: SqliteMemoryContextProvider,
    subjects: Mutex<Vec<Option<PersonId>>>,
}

impl MemoryContextProvider for SubjectRecorder {
    fn execution_location(&self) -> ExecutionLocation {
        ExecutionLocation::Local
    }

    fn retrieve(
        &self,
        turn_id: TurnId,
        query: String,
        subject: Option<PersonId>,
        cancellation: CancellationToken,
    ) -> MemoryProviderFuture<'_> {
        self.subjects.lock().unwrap().push(subject);
        self.inner.retrieve(turn_id, query, subject, cancellation)
    }
}

/// Answers every speech request with one frame for that request's own turn.
struct OneFrameSpeech;

impl StreamingSpeechSynthesizer for OneFrameSpeech {
    fn stream(
        &self,
        request: StreamingSpeechRequest,
        _cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<AudioFrame, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        let frame = AudioFrame::new(
            request.turn_id(),
            request.generation_id(),
            request.utterance_id(),
            0,
            PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
            vec![0; 960],
        )
        .unwrap();
        sender.try_send(Ok(frame)).unwrap();
        receiver
    }
}

struct Harness {
    fixture: StageFixture,
    identity: SessionSpeakerIdentity,
    memory: Arc<SubjectRecorder>,
    language: Arc<MockGenerationLanguageModel>,
    runtime: StreamingTurnRuntime,
}

fn harness(wait: Duration) -> Harness {
    harness_with(wait, |_| {})
}

/// `setup` runs before the session starts, because a session reads people once at start.
fn harness_with(wait: Duration, setup: impl FnOnce(&StageFixture)) -> Harness {
    let fixture = StageFixture::new();
    setup(&fixture);
    let identity = fixture
        .stage(wait)
        .start_session(SessionId::new(1))
        .unwrap();
    let memory = Arc::new(SubjectRecorder {
        inner: SqliteMemoryContextProvider::new(
            (*fixture.store).clone(),
            Arc::new(SystemMemoryClock),
        ),
        subjects: Mutex::new(Vec::new()),
    });
    let context = ConversationContext::new(ConversationQualityController::new(
        PersonaProfile::default(),
        ResponseControls::default(),
        ConversationMode::DirectAnswer,
    ))
    .with_memory_provider(memory.clone(), ExecutionLocation::Local)
    .unwrap()
    .with_speaker_identity();
    let language = Arc::new(MockGenerationLanguageModel::new(["answer"]));
    let runtime = StreamingTurnRuntime::new(
        context,
        language.clone(),
        Arc::new(OneFrameSpeech),
        Arc::new(MockContinuousAudioOutput::new()),
    );
    Harness {
        fixture,
        identity,
        memory,
        language,
        runtime,
    }
}

fn voice(
    harness: &Harness,
    segment_id: u64,
    transcript: &str,
) -> Option<(SessionSpeakerIdentity, TurnVoiceRequest)> {
    Some((
        harness.identity.clone(),
        TurnVoiceRequest {
            segment_id,
            transcript: transcript.to_owned(),
            barge_in: false,
            at: at(),
        },
    ))
}

async fn run(
    harness: &Harness,
    segment_id: u64,
    transcript: &str,
    with_voice: bool,
) -> Vec<RuntimeEvent> {
    let started = harness
        .runtime
        .start_turn_with_voice(
            ConversationTurnSource::Voice {
                session_id: SessionId::new(1),
            },
            transcript,
            if with_voice {
                voice(harness, segment_id, transcript)
            } else {
                None
            },
        )
        .await
        .unwrap();
    let mut events = started.into_events();
    let mut observed = Vec::new();
    timeout(Duration::from_secs(5), async {
        while let Some(event) = events.recv().await {
            observed.push(event);
        }
    })
    .await
    .expect("the turn never ended");
    observed
}

fn position(events: &[RuntimeEvent], predicate: impl Fn(&RuntimeEvent) -> bool) -> Option<usize> {
    events.iter().position(predicate)
}

#[tokio::test]
async fn an_identified_turn_is_tagged_guided_and_scoped() {
    let mut chris = None;
    let harness = harness_with(Duration::from_millis(500), |fixture| {
        chris = Some(fixture.person("Chris", &CHRIS));
    });
    let chris = chris.unwrap();
    harness.fixture.audio.speak(1, 1.0, 2_000);

    let events = run(&harness, 1, "hello", true).await;

    let identified = position(&events, |event| {
        matches!(event, RuntimeEvent::SpeakerIdentified { person, confirmed: true, .. } if *person == Some(chris))
    })
    .expect("no speaker event");
    let retrieved = position(&events, |event| {
        matches!(event, RuntimeEvent::MemoryRetrieved { .. })
    })
    .expect("no retrieval");
    assert!(identified < retrieved);
    let request = harness.language.requests().remove(0);
    assert_eq!(
        request.input().speaker(),
        Some(&SpeakerTag::person(Some("Chris"), true))
    );
    assert!(request
        .input()
        .runtime_guidance()
        .unwrap()
        .contains("Never read tags aloud."));
    assert_eq!(*harness.memory.subjects.lock().unwrap(), vec![Some(chris)]);
}

#[tokio::test]
async fn an_unsure_or_late_turn_retrieves_nothing() {
    let harness = harness_with(Duration::from_millis(100), |fixture| {
        fixture.person("Chris", &CHRIS);
    });
    harness.fixture.audio.speak(1, 1.0, 2_000);
    *harness.fixture.embedder.delay.lock().unwrap() = Some(Duration::from_secs(5));

    let events = run(&harness, 1, "hello", true).await;

    assert!(events
        .iter()
        .any(|event| matches!(event, RuntimeEvent::IdentityLate { .. })));
    assert!(!events
        .iter()
        .any(|event| matches!(event, RuntimeEvent::MemoryRetrieved { .. })));
    assert!(harness.memory.subjects.lock().unwrap().is_empty());
    assert_eq!(
        harness.language.requests()[0].input().speaker(),
        Some(&SpeakerTag::someone_new())
    );
    assert!(events
        .iter()
        .any(|event| matches!(event, RuntimeEvent::TurnCompleted { .. })));
}

#[tokio::test]
async fn the_next_turn_sees_the_previous_speakers_tag_in_history() {
    let harness = harness_with(Duration::from_millis(500), |fixture| {
        fixture.person("Chris", &CHRIS);
    });
    harness.fixture.audio.speak(1, 1.0, 2_000);
    harness.fixture.audio.speak(2, 1.0, 2_000);

    run(&harness, 1, "hello", true).await;
    run(&harness, 2, "how are you", true).await;

    let second = harness.language.requests().remove(1);
    let history = second.input().recent_messages();
    assert_eq!(history[0].text(), "hello");
    assert_eq!(
        history[0].speaker(),
        Some(&SpeakerTag::person(Some("Chris"), true))
    );
}

#[tokio::test]
async fn cancellation_during_identification_reaches_the_embedder() {
    let harness = harness(Duration::from_secs(10));
    harness.fixture.audio.speak(1, 1.0, 2_000);
    *harness.fixture.embedder.delay.lock().unwrap() = Some(Duration::from_secs(10));
    let started = harness
        .runtime
        .start_turn_with_voice(
            ConversationTurnSource::Voice {
                session_id: SessionId::new(1),
            },
            "hello",
            voice(&harness, 1, "hello"),
        )
        .await
        .unwrap();
    let identity = started.identity();
    let mut events = started.into_events();
    tokio::time::sleep(Duration::from_millis(50)).await;

    harness
        .runtime
        .interrupt(identity.turn_id(), identity.generation_id())
        .await
        .unwrap();
    let mut observed = Vec::new();
    timeout(Duration::from_secs(2), async {
        while let Some(event) = events.recv().await {
            observed.push(event);
        }
    })
    .await
    .expect("cancellation did not end the turn");

    let terminals: Vec<_> = observed
        .iter()
        .filter(|event| event.is_terminal())
        .collect();
    assert_eq!(terminals.len(), 1);
    assert!(matches!(terminals[0], RuntimeEvent::TurnCancelled { .. }));
    assert!(harness.fixture.embedder.tokens.lock().unwrap()[0].is_cancelled());
    assert!(harness.language.requests().is_empty());
}

#[tokio::test]
async fn turns_without_a_voice_request_are_unchanged() {
    let harness = harness(Duration::from_millis(500));

    let events = run(&harness, 1, "hello", false).await;

    assert!(!events.iter().any(|event| matches!(
        event,
        RuntimeEvent::SpeakerIdentified { .. } | RuntimeEvent::IdentityLate { .. }
    )));
    assert_eq!(*harness.memory.subjects.lock().unwrap(), vec![None]);
    assert_eq!(harness.language.requests()[0].input().speaker(), None);
}
