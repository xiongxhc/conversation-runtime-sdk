//! Proves that a voice session identifies its speakers from real inputs: an in-process gateway
//! session at protocol 4 runs the real macOS sidecar adapter against the fake sidecar binary,
//! which sends each finished turn's audio at sidecar protocol version 2. The memory store holds
//! one mature person with one voice signature, and a stand-in embedder returns that signature's
//! vector for any audio, so a turn whose audio arrives is that person's and a turn without
//! audio is late.
//!
//! The fake sidecar takes its scenario from a process environment variable it inherits at
//! spawn, so every sidecar start in this file sets it under one lock, as
//! `tests/voice/tests/sidecar_process.rs` does.
#![cfg(unix)]

mod support;

use std::ffi::OsString;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use conversation_memory::{
    NewPerson, PeopleStore, PersonName, SignatureKey, SignatureModel, SignatureRole,
    SqliteMemoryStore, VoiceSignatureDraft,
};
use conversation_model_adapters::{
    AdapterError, AdapterFuture, AudioFrame, GenerationLanguageModel, GenerationLanguageRequest,
    GenerationTextDelta, MacOsVoiceSidecar, MacOsVoiceSidecarConfig, PcmFormat, PcmSampleFormat,
    SidecarTurnAudio, StreamingSpeechRequest, StreamingSpeechSynthesizer, SystemDevice,
    VoiceIoFactory, VoiceIoSession,
};
use conversation_protocol::{PersonId, SessionId, UnixTimestampMillis, MAX_CLIENT_FRAME_BYTES};
use conversation_runtime::{IdentityPolicy, SpeakerFuture, TextTurnRuntime, VoiceEmbedder};
use conversation_runtime_gateway::{
    attach_speaker_identity, FrameReader, GatewayConfig, GatewaySession, GatewaySpeakerIdentity,
    SpeakerIdentitySettings,
};
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{duplex, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// A deliberately fake signature key.
const FAKE_KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const JACK_VOICE: [f32; 4] = [0.0, 1.0, 0.0, 0.0];

#[tokio::test]
async fn a_sidecar_voice_turn_reports_its_speaker() {
    let mut gateway = Gateway::start("turn-audio", Duration::from_secs(5)).await;
    gateway.start_voice().await;

    let events = gateway.voice_events_through_turn_terminal(1).await;
    let speaker = position(&events, "voice_speaker").expect("the turn's speaker was reported");
    assert_eq!(events[speaker]["person_id"], gateway.jack.get().to_string());
    assert_eq!(events[speaker]["confirmed"], true);
    assert_eq!(events[speaker]["turn_id"], "1");
    assert!(position(&events, "voice_identity_late").is_none());
    let terminal = turn_terminal(&events).expect("the turn ended");
    assert!(speaker < terminal, "the speaker comes before the turn ends");

    gateway.stop().await;
}

#[tokio::test]
async fn a_turn_whose_audio_never_arrives_is_reported_late_and_still_answers() {
    let mut gateway = Gateway::start("partial-final", Duration::from_millis(300)).await;
    gateway.start_voice().await;

    let events = gateway.voice_events_through_turn_terminal(1).await;
    let late = position(&events, "voice_identity_late").expect("the turn was reported late");
    assert_eq!(events[late]["turn_id"], "1");
    assert!(position(&events, "voice_speaker").is_none());
    let terminal = turn_terminal(&events).expect("the turn ended");
    assert!(late < terminal);
    assert_eq!(
        events[terminal]["event"]["type"], "turn_completed",
        "the late turn is still answered: {:?}",
        events[terminal]
    );

    gateway.stop().await;
}

#[tokio::test]
async fn a_second_turn_never_reuses_the_first_turns_audio() {
    let mut gateway =
        Gateway::start("two-turns-one-audio-after-playback", Duration::from_secs(2)).await;
    gateway.start_voice().await;

    // The sidecar ends its first turn with that turn's audio (session segment 1). Its second
    // turn (segment 2) has no audio, and must not be identified from the first turn's. The
    // second turn starts while the first answer is playing, so it interrupts that answer.
    let events = gateway.voice_events_through_turn_terminal(2).await;
    let finals: Vec<(&str, &str, &str)> = events
        .iter()
        .filter(|event| event["type"] == "voice_transcript_final")
        .map(|event| {
            (
                event["turn_id"].as_str().unwrap_or_default(),
                event["segment_id"].as_str().unwrap_or_default(),
                event["text"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        finals,
        [("1", "1", "first"), ("2", "2", "second")],
        "{events:?}"
    );
    let identities: Vec<(&str, &str)> = events
        .iter()
        .filter_map(|event| {
            let kind = event["type"].as_str()?;
            matches!(kind, "voice_speaker" | "voice_identity_late")
                .then(|| (kind, event["turn_id"].as_str().unwrap_or_default()))
        })
        .collect();
    assert_eq!(
        identities,
        [("voice_speaker", "1"), ("voice_identity_late", "2")],
        "{events:?}"
    );
    let speaker = position(&events, "voice_speaker").unwrap();
    assert_eq!(events[speaker]["person_id"], gateway.jack.get().to_string());
    assert_eq!(events[speaker]["confirmed"], true);

    // The first turn is identified, then interrupted by the second, which is late; each
    // turn's identity comes before that turn ends.
    let terminals: Vec<(usize, &str, &str)> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| is_turn_terminal(event))
        .map(|(index, event)| {
            (
                index,
                event["event"]["turn_id"].as_str().unwrap_or_default(),
                event["event"]["type"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        terminals
            .iter()
            .map(|(_, turn, kind)| (*turn, *kind))
            .collect::<Vec<_>>(),
        [("1", "turn_cancelled"), ("2", "turn_completed")],
        "{events:?}"
    );
    let barge_in = position(&events, "voice_barge_in").expect("the second turn interrupted");
    let late = position(&events, "voice_identity_late").unwrap();
    assert!(speaker < barge_in && barge_in < terminals[0].0);
    assert!(terminals[0].0 < late && late < terminals[1].0);

    gateway.stop().await;
}

/// `(type, turn_id, person_id, confirmed)` of every identity event, in order.
fn identities(events: &[Value]) -> Vec<(String, String, Value, Value)> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event["type"].as_str(),
                Some("voice_speaker" | "voice_identity_late")
            )
        })
        .map(|event| {
            (
                event["type"].as_str().unwrap_or_default().to_owned(),
                event["turn_id"].as_str().unwrap_or_default().to_owned(),
                event["person_id"].clone(),
                event["confirmed"].clone(),
            )
        })
        .collect()
}

fn finals(events: &[Value]) -> Vec<(String, String, String)> {
    events
        .iter()
        .filter(|event| event["type"] == "voice_transcript_final")
        .map(|event| {
            (
                event["turn_id"].as_str().unwrap_or_default().to_owned(),
                event["segment_id"].as_str().unwrap_or_default().to_owned(),
                event["text"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_turn_joining_two_sidecar_turns_is_never_identified_from_one_of_them() {
    // Each sidecar turn ends with its own one-segment audio, but the runtime joins both into
    // one turn of two segments: the second sidecar turn's audio covers only its last segment.
    let mut gateway = Gateway::start("two-turns-two-audio", Duration::from_secs(2)).await;
    gateway.start_voice().await;

    let events = gateway.voice_events_through_turn_terminal(1).await;
    assert_eq!(
        finals(&events),
        [("1".to_owned(), "2".to_owned(), "firstsecond".to_owned())],
        "{events:?}"
    );
    // Decided at once, without the voice: someone new and unsure, not Jack.
    assert_eq!(
        identities(&events),
        [(
            "voice_speaker".to_owned(),
            "1".to_owned(),
            Value::Null,
            Value::Bool(false)
        )],
        "{events:?}"
    );

    gateway.stop().await;
}

#[tokio::test]
async fn a_joined_turn_whose_last_sidecar_turn_has_no_audio_is_late() {
    let mut gateway = Gateway::start("two-turns-one-audio", Duration::from_millis(300)).await;
    gateway.start_voice().await;

    let events = gateway.voice_events_through_turn_terminal(1).await;
    assert_eq!(
        finals(&events),
        [("1".to_owned(), "2".to_owned(), "firstsecond".to_owned())],
        "{events:?}"
    );
    assert_eq!(
        identities(&events),
        [(
            "voice_identity_late".to_owned(),
            "1".to_owned(),
            Value::Null,
            Value::Null
        )],
        "{events:?}"
    );

    gateway.stop().await;
}

#[tokio::test]
async fn a_turn_covering_part_of_a_sidecar_turn_is_never_identified_from_all_of_it() {
    // One sidecar turn holds two finals, but the runtime ended a turn after the first: the
    // sidecar's audio covers both segments while the second runtime turn has only one.
    let mut gateway = Gateway::start(
        "one-sidecar-turn-two-runtime-turns",
        Duration::from_millis(300),
    )
    .await;
    gateway.start_voice().await;

    let events = gateway.voice_events_through_turn_terminal(2).await;
    assert_eq!(
        finals(&events),
        [
            ("1".to_owned(), "1".to_owned(), "first".to_owned()),
            ("2".to_owned(), "2".to_owned(), "second".to_owned())
        ],
        "{events:?}"
    );
    // The first turn's segment has no audio of its own while its wait runs; the second turn
    // is decided without the voice that also holds the first.
    assert_eq!(
        identities(&events),
        [
            (
                "voice_identity_late".to_owned(),
                "1".to_owned(),
                Value::Null,
                Value::Null
            ),
            (
                "voice_speaker".to_owned(),
                "2".to_owned(),
                Value::Null,
                Value::Bool(false)
            )
        ],
        "{events:?}"
    );

    gateway.stop().await;
}

/// The index of the first voice event of `kind`.
fn position(events: &[Value], kind: &str) -> Option<usize> {
    events.iter().position(|event| event["type"] == kind)
}

fn is_turn_terminal(event: &Value) -> bool {
    event["type"] == "voice_turn_event"
        && matches!(
            event["event"]["type"].as_str(),
            Some("turn_completed" | "turn_failed" | "turn_cancelled")
        )
}

fn turn_terminal(events: &[Value]) -> Option<usize> {
    events.iter().position(is_turn_terminal)
}

struct Gateway {
    input: DuplexStream,
    output: FrameReader<DuplexStream>,
    session: JoinHandle<()>,
    jack: PersonId,
    _directory: TempDir,
}

impl Gateway {
    /// A protocol-4 gateway session whose voice lane runs the fake sidecar in `scenario`, with
    /// speaker identity bounded by `wait`.
    async fn start(scenario: &'static str, wait: Duration) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let model_path = directory.path().join("asr-model");
        std::fs::create_dir(&model_path).unwrap();
        let config = directory.path().join("gateway.toml");
        std::fs::write(
            &config,
            support::config_contents("http://127.0.0.1:9", None)
                + &support::voice_lane_config_contents(&model_path, "http://127.0.0.1:9"),
        )
        .unwrap();
        let adapters = GatewayConfig::load(&config).unwrap();
        let mut voice = adapters.voice.expect("the voice lane is configured");

        // The configuration's sidecar and speech provider are replaced by the fake sidecar,
        // speaking version 2 into one turn audio handle, and an in-process speech fake.
        let turn_audio = SidecarTurnAudio::new();
        voice.io = Arc::new(ScenarioSidecar {
            sidecar: MacOsVoiceSidecar::new(
                MacOsVoiceSidecarConfig::new(
                    support::fake_sidecar_binary(),
                    &model_path,
                    SystemDevice::SystemDefault,
                    false,
                    200,
                    600,
                )
                .unwrap()
                .with_turn_audio(turn_audio.clone()),
            ),
            scenario,
        });
        voice.speech = Arc::new(OneFrameSpeech);
        voice.sidecar_turn_audio = Some(turn_audio);
        let sidecar_turn_audio = voice.sidecar_turn_audio.clone();

        let database = directory.path().join("runtime.sqlite3");
        let store = SqliteMemoryStore::initialize(&database).unwrap();
        let key = Arc::new(SignatureKey::from_hex(FAKE_KEY).unwrap());
        let jack = mature_person(&store, &key, "Jack", &JACK_VOICE);

        let language: Arc<dyn GenerationLanguageModel> = Arc::new(AnsweringLanguage);
        let runtime = TextTurnRuntime::new(
            adapters.context.with_speaker_identity(),
            Arc::clone(&language),
        );
        let session = GatewaySession::new(runtime, adapters.status)
            .with_v4_control()
            .with_voice(voice, Arc::clone(&language));
        let session = attach_speaker_identity(
            session,
            GatewaySpeakerIdentity {
                settings: SpeakerIdentitySettings {
                    key,
                    policy: policy(),
                    wait,
                },
                embedder: Arc::new(StandInEmbedder {
                    model: model(),
                    voice: JACK_VOICE.to_vec(),
                }),
            },
            store,
            language,
            sidecar_turn_audio,
        );

        let (input, reader) = duplex(MAX_CLIENT_FRAME_BYTES * 2);
        let (writer, output) = duplex(MAX_CLIENT_FRAME_BYTES * 2);
        let session = tokio::spawn(async move {
            let _ = session.run(reader, writer).await;
        });
        let mut gateway = Self {
            input,
            output: FrameReader::new(output),
            session,
            jack,
            _directory: directory,
        };
        let ready = gateway.read_message().await;
        assert_eq!(ready["type"], "ready", "{ready}");
        gateway
    }

    async fn start_voice(&mut self) {
        self.write(r#"{"protocol_version":4,"type":"start_voice_session","request_id":"v-1"}"#)
            .await;
        self.read_until(|message| message["request_id"] == "v-1")
            .await
            .into_iter()
            .last()
            .filter(|message| message["type"] == "command_accepted")
            .expect("the voice session was accepted");
    }

    /// Voice events up to and including the `turns`-th turn's terminal event.
    async fn voice_events_through_turn_terminal(&mut self, turns: usize) -> Vec<Value> {
        let mut ended = 0;
        let mut events = Vec::new();
        while ended < turns {
            let message = self.read_message().await;
            if message["type"] != "voice_event" {
                continue;
            }
            let event = message["event"].clone();
            assert!(
                !matches!(
                    event["type"].as_str(),
                    Some("voice_session_ended" | "voice_session_failed")
                ),
                "the voice session ended early: {events:?} then {event}"
            );
            ended += usize::from(is_turn_terminal(&event));
            events.push(event);
        }
        events
    }

    async fn stop(mut self) {
        self.write(r#"{"protocol_version":4,"type":"stop_voice_session","request_id":"v-stop"}"#)
            .await;
        let stopped = self
            .read_until(|message| message["request_id"] == "v-stop")
            .await;
        assert_eq!(stopped.last().unwrap()["type"], "command_accepted");
        drop(self.input);
        timeout(READ_TIMEOUT, self.session)
            .await
            .expect("the gateway session ends at input EOF")
            .unwrap();
    }

    async fn write(&mut self, payload: &str) {
        self.input
            .write_all(&u32::try_from(payload.len()).unwrap().to_be_bytes())
            .await
            .unwrap();
        self.input.write_all(payload.as_bytes()).await.unwrap();
        self.input.flush().await.unwrap();
    }

    /// Messages up to and including the first that satisfies `predicate`.
    async fn read_until(&mut self, predicate: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut messages = Vec::new();
        loop {
            let message = self.read_message().await;
            let done = predicate(&message);
            messages.push(message);
            if done {
                return messages;
            }
        }
    }

    async fn read_message(&mut self) -> Value {
        let frame = timeout(READ_TIMEOUT, self.output.read_frame())
            .await
            .expect("the gateway produced no frame before the test timeout")
            .expect("the gateway frame reader failed")
            .expect("the gateway closed before answering");
        serde_json::from_slice(&frame).expect("gateway frames are JSON")
    }
}

fn model() -> SignatureModel {
    SignatureModel::new("ecapa", "1").unwrap()
}

fn policy() -> IdentityPolicy {
    IdentityPolicy {
        confident_score: 0.80,
        possible_score: 0.60,
        margin: 0.10,
        anchor_score: 0.85,
        min_sample_ms: 1_500,
        maturity_speech_ms: 5_000,
        refinement_cap: 3,
    }
}

/// A mature person with one stored voice signature.
fn mature_person(
    store: &SqliteMemoryStore,
    key: &SignatureKey,
    name: &str,
    voice: &[f32],
) -> PersonId {
    let at = UnixTimestampMillis::new(10_000).unwrap();
    let person = store
        .create_person(NewPerson::new(
            PersonName::new(name, name.to_lowercase()).unwrap(),
            false,
            at,
        ))
        .unwrap()
        .id();
    store
        .add_signature(
            key,
            person,
            VoiceSignatureDraft::new(
                model(),
                SignatureRole::Enrolment,
                voice.to_vec(),
                5_000,
                at,
                SessionId::new(1),
            )
            .unwrap(),
        )
        .unwrap();
    store.mark_mature(person).unwrap();
    person
}

/// The real sidecar adapter, started with the fake sidecar's scenario in its environment.
struct ScenarioSidecar {
    sidecar: MacOsVoiceSidecar,
    scenario: &'static str,
}

impl VoiceIoFactory for ScenarioSidecar {
    fn start<'a>(
        &'a self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, VoiceIoSession> {
        Box::pin(async move {
            let _lock = environment_lock().lock().await;
            let _scenario = ScopedScenario::set(self.scenario);
            self.sidecar.start(session_id, cancellation).await
        })
    }
}

fn environment_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

struct ScopedScenario {
    previous: Option<OsString>,
}

impl ScopedScenario {
    fn set(scenario: &str) -> Self {
        let previous = std::env::var_os(support::SCENARIO_ENV);
        std::env::set_var(support::SCENARIO_ENV, scenario);
        Self { previous }
    }
}

impl Drop for ScopedScenario {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(previous) => std::env::set_var(support::SCENARIO_ENV, previous),
            None => std::env::remove_var(support::SCENARIO_ENV),
        }
    }
}

/// Stands in for the voice embedding model this build does not include: every voice is the
/// one it was given.
struct StandInEmbedder {
    model: SignatureModel,
    voice: Vec<f32>,
}

impl VoiceEmbedder for StandInEmbedder {
    fn model(&self) -> &SignatureModel {
        &self.model
    }

    fn embed(
        &self,
        _samples: Vec<f32>,
        _cancellation: CancellationToken,
    ) -> SpeakerFuture<'_, Result<Vec<f32>, conversation_protocol::RuntimeError>> {
        let voice = self.voice.clone();
        Box::pin(async move { Ok(voice) })
    }
}

/// Answers every turn at once.
struct AnsweringLanguage;

impl GenerationLanguageModel for AnsweringLanguage {
    fn stream(
        &self,
        request: GenerationLanguageRequest,
        _cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<GenerationTextDelta, AdapterError>> {
        let (sender, receiver) = mpsc::channel(1);
        sender
            .try_send(Ok(GenerationTextDelta::new(
                request.turn_id(),
                request.generation_id(),
                "fixture answer",
            )))
            .expect("the answer fits the channel");
        receiver
    }
}

/// Speaks every answer as one short frame.
struct OneFrameSpeech;

impl StreamingSpeechSynthesizer for OneFrameSpeech {
    fn stream(
        &self,
        request: StreamingSpeechRequest,
        _cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<AudioFrame, AdapterError>> {
        let frame = AudioFrame::new(
            request.turn_id(),
            request.generation_id(),
            request.utterance_id(),
            0,
            PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
            vec![0; 960],
        )
        .unwrap();
        let (sender, receiver) = mpsc::channel(1);
        sender
            .try_send(Ok(frame))
            .expect("the frame fits the channel");
        receiver
    }
}
