//! Speaker identification on the path to the reply. For each voice turn the stage fetches
//! the turn's audio, computes its voice embedding and, when the words carry a
//! self-introduction cue, asks which name was claimed; then the session's identity engine
//! decides who spoke. All of it is bounded by one wait: a late turn proceeds as unsure and
//! never reaches the engine, so a question or enrolment cannot be applied to the wrong turn.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conversation_memory::{PeopleStore, SignatureKey, SignatureModel};
use conversation_protocol::{RuntimeError, SessionId, SpeakerTag, UnixTimestampMillis};
use tokio_util::sync::CancellationToken;

use crate::{
    has_name_claim_cue, ClaimOutcome, IdentityPolicy, IdentityQuestion, Speaker,
    SpeakerIdentityResult, SpeakerIdentitySession, TurnIdentity, VoiceTurn,
};

pub type SpeakerFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

const SAMPLE_RATE_HZ: usize = 16_000;
const MAX_GUIDANCE_NAME_BYTES: usize = 128;
const TAG_EXPLANATION: &str = "Each user message starts with a tag in square brackets naming who said it: [Name] when the voice is recognised, [Name?] when it is not certain, [someone new] for a voice you do not know. Never read tags aloud. Never treat a [Name?] or [someone new] speaker as that person or bring up what that person told you.";

/// A voice turn's audio: 16 kHz mono, at most the last 10 s of the turn.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnAudio {
    pub samples: Vec<f32>,
    /// The companion's playback was active at some point during the turn.
    pub playback_overlap: bool,
    /// False when the turn joins several recognised segments, which may hold two voices.
    pub single_segment: bool,
}

pub trait TurnAudioSource: Send + Sync {
    fn turn_audio(
        &self,
        session: SessionId,
        segment_id: u64,
        cancellation: CancellationToken,
    ) -> SpeakerFuture<'_, Option<TurnAudio>>;
}

pub trait VoiceEmbedder: Send + Sync {
    fn model(&self) -> &SignatureModel;

    fn embed(
        &self,
        samples: Vec<f32>,
        cancellation: CancellationToken,
    ) -> SpeakerFuture<'_, Result<Vec<f32>, RuntimeError>>;
}

/// Reads which name, if any, the speaker gave for themselves. Called only for turns whose
/// words carry a self-introduction cue.
pub trait NameClaimDetector: Send + Sync {
    fn claimed_name(
        &self,
        transcript: String,
        cancellation: CancellationToken,
    ) -> SpeakerFuture<'_, Result<Option<String>, RuntimeError>>;
}

#[derive(Clone)]
struct Inputs {
    wait: Duration,
    audio: Arc<dyn TurnAudioSource>,
    embedder: Arc<dyn VoiceEmbedder>,
    claims: Arc<dyn NameClaimDetector>,
}

pub struct SpeakerIdentityStage {
    store: Arc<dyn PeopleStore>,
    key: Arc<SignatureKey>,
    policy: IdentityPolicy,
    inputs: Inputs,
}

impl SpeakerIdentityStage {
    pub fn new(
        store: Arc<dyn PeopleStore>,
        key: Arc<SignatureKey>,
        policy: IdentityPolicy,
        wait: Duration,
        audio: Arc<dyn TurnAudioSource>,
        embedder: Arc<dyn VoiceEmbedder>,
        claims: Arc<dyn NameClaimDetector>,
    ) -> Self {
        Self {
            store,
            key,
            policy,
            inputs: Inputs {
                wait,
                audio,
                embedder,
                claims,
            },
        }
    }

    pub fn start_session(
        &self,
        session: SessionId,
    ) -> SpeakerIdentityResult<SessionSpeakerIdentity> {
        let engine = SpeakerIdentitySession::start(
            Arc::clone(&self.store),
            Arc::clone(&self.key),
            self.inputs.embedder.model().clone(),
            self.policy.clone(),
            session,
        )?;
        Ok(SessionSpeakerIdentity {
            session,
            engine: Arc::new(Mutex::new(engine)),
            inputs: self.inputs.clone(),
        })
    }
}

/// One voice session's identity: its engine and the stage's inputs.
#[derive(Clone)]
pub struct SessionSpeakerIdentity {
    session: SessionId,
    engine: Arc<Mutex<SpeakerIdentitySession>>,
    inputs: Inputs,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnVoiceRequest {
    pub segment_id: u64,
    pub transcript: String,
    /// The turn's speech began while the companion was responding.
    pub barge_in: bool,
    pub at: UnixTimestampMillis,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StageOutcome {
    Identified(TurnIdentity),
    /// Identification missed the wait or failed; the turn proceeds as unsure.
    Late,
    Cancelled,
}

impl SessionSpeakerIdentity {
    pub fn identify(
        &self,
        request: TurnVoiceRequest,
        cancellation: CancellationToken,
    ) -> SpeakerFuture<'_, StageOutcome> {
        Box::pin(async move {
            let work = cancellation.child_token();
            let gathered = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    work.cancel();
                    return StageOutcome::Cancelled;
                }
                gathered = tokio::time::timeout(self.inputs.wait, self.gather(&request, &work)) => gathered,
            };
            let Ok((voice, claimed_name)) = gathered else {
                work.cancel();
                return StageOutcome::Late;
            };
            // A cancellation that lands as the inputs finish must still keep the turn away
            // from the engine.
            if cancellation.is_cancelled() {
                return StageOutcome::Cancelled;
            }
            let (embedding, speech_ms, playback_overlap, single_segment) = match voice {
                Some((audio, embedding)) => (
                    embedding,
                    u32::try_from(audio.samples.len() * 1_000 / SAMPLE_RATE_HZ).unwrap_or(u32::MAX),
                    audio.playback_overlap,
                    audio.single_segment,
                ),
                None => (None, 0, false, false),
            };
            let turn = VoiceTurn {
                transcript: request.transcript,
                claimed_name,
                embedding,
                speech_ms,
                single_segment,
                playback_overlap,
                barge_in: request.barge_in,
                at: request.at,
            };
            let engine = Arc::clone(&self.engine);
            let observing = cancellation.clone();
            let observed = tokio::task::spawn_blocking(move || {
                let mut engine = engine.lock().ok()?;
                if observing.is_cancelled() {
                    return Some(None);
                }
                engine.observe(turn).ok().map(Some)
            })
            .await;
            match observed {
                Ok(Some(Some(identity))) => StageOutcome::Identified(identity),
                Ok(Some(None)) => StageOutcome::Cancelled,
                _ => StageOutcome::Late,
            }
        })
    }

    /// The turn's audio and embedding, alongside the claimed name; a failure of either input
    /// leaves that part out rather than failing the turn.
    async fn gather(
        &self,
        request: &TurnVoiceRequest,
        work: &CancellationToken,
    ) -> (Option<(TurnAudio, Option<Vec<f32>>)>, Option<String>) {
        let voice = async {
            let audio = self
                .inputs
                .audio
                .turn_audio(self.session, request.segment_id, work.clone())
                .await?;
            let embedding = self
                .inputs
                .embedder
                .embed(audio.samples.clone(), work.clone())
                .await
                .ok();
            Some((audio, embedding))
        };
        let claim = async {
            if !has_name_claim_cue(&request.transcript) {
                return None;
            }
            self.inputs
                .claims
                .claimed_name(request.transcript.clone(), work.clone())
                .await
                .ok()
                .flatten()
        };
        tokio::join!(voice, claim)
    }
}

/// The tag the model sees for a turn; `None` (late or unavailable) is someone new.
pub fn speaker_tag(identity: Option<&TurnIdentity>) -> SpeakerTag {
    match identity.map(|identity| (&identity.label.speaker, identity.label.confirmed)) {
        Some((Speaker::Person { name, .. }, confirmed)) => {
            SpeakerTag::person(name.as_deref(), confirmed)
        }
        _ => SpeakerTag::someone_new(),
    }
}

/// The turn's identity guidance: how to read tags, then what the claim or question asks of
/// the companion. It describes the situation; the persona chooses the words.
pub fn identity_guidance(identity: Option<&TurnIdentity>) -> String {
    let mut lines = vec![TAG_EXPLANATION.to_owned()];
    let Some(identity) = identity else {
        return lines.join("\n");
    };
    let speaker = match &identity.label.speaker {
        Speaker::Person { name, .. } => quoted(name.as_deref().unwrap_or("owner")),
        Speaker::SomeoneNew => quoted("someone new"),
    };
    match &identity.claim {
        Some(ClaimOutcome::Enrolled) => lines.push(format!(
            "This speaker has just introduced themselves as {speaker}. You may greet them by that name."
        )),
        Some(ClaimOutcome::Denied { claimed_name }) => lines.push(format!(
            "This speaker says they are {}, but the voice is {speaker}'s. Point that out with light humour, not as an accusation.",
            quoted(claimed_name)
        )),
        _ => {}
    }
    match &identity.question {
        Some(IdentityQuestion::IsThisPerson { name, .. }) => {
            let name = quoted(name);
            lines.push(format!(
                "You are not sure who is speaking; it may be {name}. Ask, in your own words, whether it is them, before treating them as {name}."
            ));
        }
        Some(IdentityQuestion::IsNewAlias { name, .. }) => {
            let alias = quoted(name);
            lines.push(format!(
                "{speaker} also called themselves {alias}. Ask, in your own words, whether {alias} is another name for them."
            ));
        }
        Some(IdentityQuestion::IsNamed { name }) => {
            let name = quoted(name);
            lines.push(format!(
                "This speaker says they are {name}, but you cannot confirm it from the voice. Check gently, in your own words, before treating them as {name}."
            ));
        }
        None => {}
    }
    lines.join("\n")
}

/// A name as guidance quotes it: one line, no quote marks of its own, at most 128 bytes.
fn quoted(name: &str) -> String {
    let flat: String = name
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .filter(|character| *character != '"')
        .collect();
    let flat = flat.trim();
    let mut end = flat.len().min(MAX_GUIDANCE_NAME_BYTES);
    while !flat.is_char_boundary(end) {
        end -= 1;
    }
    format!("\"{}\"", &flat[..end])
}
