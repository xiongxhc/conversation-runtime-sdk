//! The speaker identity engine for one voice session. For each final voice turn it says
//! who is speaking and how sure it is, what a name claim means, what to ask, and whose
//! memories the turn may use, and it records new people and voice samples in the people
//! store. Nothing said in a conversation renames, merges or deletes anyone: those are the
//! owner's operations through the SDK.
//!
//! This module never logs. Names and embeddings leave only through `TurnIdentity`.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use conversation_memory::{
    MemoryStoreError, MemoryStoreErrorKind, NewPerson, PeopleStore, Person, PersonMaturity,
    PersonName, SignatureKey, SignatureModel, SignatureRole, VoiceSignatureDraft,
};
use conversation_protocol::{PersonId, RuntimeError, SessionId, UnixTimestampMillis};

use crate::{
    classify, cosine, decide_claim, identity_answer, name_key, ClaimDecision, ClaimHolder,
    IdentityAnswer, IdentityPolicy, VoiceDirectory, VoiceEvidence,
};

/// One final voice turn as the identity engine sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceTurn {
    pub transcript: String,
    /// The name the speaker gave for themselves, when the turn is a self-introduction.
    pub claimed_name: Option<String>,
    /// `None` when identification was late or the turn had no audio.
    pub embedding: Option<Vec<f32>>,
    pub speech_ms: u32,
    /// False when the turn joins several recognised segments, which may hold two voices.
    pub single_segment: bool,
    /// The companion's own playback was active at some point during the turn.
    pub playback_overlap: bool,
    pub barge_in: bool,
    pub at: UnixTimestampMillis,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Speaker {
    Person {
        person: PersonId,
        /// `None` only for an owner who has not set a name.
        name: Option<String>,
    },
    SomeoneNew,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpeakerLabel {
    pub speaker: Speaker,
    pub confirmed: bool,
}

/// Guidance for the language model on a turn that carried a name claim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimOutcome {
    Enrolled,
    Confirmed,
    Denied { claimed_name: String },
    Unsure,
}

/// What the companion should check, in its own words.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdentityQuestion {
    IsThisPerson { person: PersonId, name: String },
    IsNewAlias { person: PersonId, name: String },
    IsNamed { name: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnIdentity {
    pub label: SpeakerLabel,
    pub claim: Option<ClaimOutcome>,
    pub question: Option<IdentityQuestion>,
    /// The only person whose memories this turn may retrieve or add to. `None` on a turn
    /// whose speaker is someone new or unsure.
    pub memory_subject: Option<PersonId>,
}

impl TurnIdentity {
    fn someone_new() -> Self {
        Self {
            label: SpeakerLabel {
                speaker: Speaker::SomeoneNew,
                confirmed: false,
            },
            claim: None,
            question: None,
            memory_subject: None,
        }
    }
}

#[derive(Debug)]
pub enum SpeakerIdentityError {
    /// The policy is invalid or the stored signatures cannot be compared.
    Configuration(RuntimeError),
    Store(MemoryStoreError),
}

impl fmt::Display for SpeakerIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl Error for SpeakerIdentityError {}

impl From<MemoryStoreError> for SpeakerIdentityError {
    fn from(error: MemoryStoreError) -> Self {
        Self::Store(error)
    }
}

pub type SpeakerIdentityResult<T> = Result<T, SpeakerIdentityError>;

/// The question waiting for the next user turn, with the voice it was asked of.
struct Pending {
    question: IdentityQuestion,
    embedding: Vec<f32>,
    speech_ms: u32,
}

impl Pending {
    fn person(&self) -> Option<PersonId> {
        match &self.question {
            IdentityQuestion::IsThisPerson { person, .. }
            | IdentityQuestion::IsNewAlias { person, .. } => Some(*person),
            IdentityQuestion::IsNamed { .. } => None,
        }
    }
}

pub struct SpeakerIdentitySession {
    store: Arc<dyn PeopleStore>,
    key: Arc<SignatureKey>,
    model: SignatureModel,
    policy: IdentityPolicy,
    session: SessionId,
    directory: VoiceDirectory,
    people: BTreeMap<PersonId, Person>,
    pending: Option<Pending>,
    /// People confirmed for this session: enrolled, confirmed by a claim, or by a yes.
    confirmed: BTreeSet<PersonId>,
    /// People already asked about without a claim, so nobody is asked twice.
    asked: BTreeSet<PersonId>,
    /// Provisional people whose matching turns are collected as enrolment speech.
    collecting: BTreeSet<PersonId>,
}

impl SpeakerIdentitySession {
    pub fn start(
        store: Arc<dyn PeopleStore>,
        key: Arc<SignatureKey>,
        model: SignatureModel,
        policy: IdentityPolicy,
        session: SessionId,
    ) -> SpeakerIdentityResult<Self> {
        policy
            .validate()
            .map_err(SpeakerIdentityError::Configuration)?;
        let mut identity = Self {
            store,
            key,
            model,
            policy,
            session,
            directory: VoiceDirectory::new(&[]).map_err(SpeakerIdentityError::Configuration)?,
            people: BTreeMap::new(),
            pending: None,
            confirmed: BTreeSet::new(),
            asked: BTreeSet::new(),
            collecting: BTreeSet::new(),
        };
        identity.reload()?;
        Ok(identity)
    }

    pub fn observe(&mut self, turn: VoiceTurn) -> SpeakerIdentityResult<TurnIdentity> {
        let scores = match &turn.embedding {
            None => None,
            // Before anyone has a signature, every voice is unknown.
            Some(_) if self.directory.dimension().is_none() => Some(Vec::new()),
            Some(embedding) => self.directory.scores(embedding),
        };
        let evidence = scores
            .as_deref()
            .map(|scores| classify(scores, &self.policy));

        if let Some(pending) = self.pending.take() {
            let another_voice = matches!(
                evidence,
                Some(VoiceEvidence::Confident { person, .. }) if Some(person) != pending.person()
            );
            if !another_voice {
                match identity_answer(&turn.transcript) {
                    Some(IdentityAnswer::Yes) => return self.answer_yes(pending, turn.at),
                    Some(IdentityAnswer::No) => return Ok(TurnIdentity::someone_new()),
                    None => {}
                }
            }
        }

        let (Some(evidence), Some(scores), Some(embedding)) = (evidence, scores, &turn.embedding)
        else {
            return Ok(TurnIdentity::someone_new());
        };
        // The companion's own voice and interruptions never enrol anyone or raise a question.
        let can_enrol = !turn.playback_overlap && !turn.barge_in;
        let claim = turn
            .claimed_name
            .as_deref()
            .filter(|_| can_enrol)
            .and_then(|display| {
                let key = name_key(display)?;
                PersonName::new(display, key).ok()
            });
        let identity = match claim {
            Some(name) => self.claim(&evidence, name, embedding, &turn)?,
            None => self.identify(&evidence, embedding, &turn, can_enrol),
        };
        if identity.label.confirmed {
            if let Speaker::Person { person, .. } = identity.label.speaker {
                self.collect(person, embedding, &scores, &turn)?;
            }
        }
        Ok(identity)
    }

    fn claim(
        &mut self,
        evidence: &VoiceEvidence,
        name: PersonName,
        embedding: &[f32],
        turn: &VoiceTurn,
    ) -> SpeakerIdentityResult<TurnIdentity> {
        let holders: Vec<ClaimHolder> = self
            .store
            .people_named(name.key())?
            .into_iter()
            .filter_map(|person| self.people.get(&person))
            .map(|person| ClaimHolder {
                person: person.id(),
                mature: person.maturity() == PersonMaturity::Mature,
                owner: person.is_owner(),
            })
            .collect();
        let matched = match *evidence {
            VoiceEvidence::Confident { person, .. } | VoiceEvidence::Possible { person, .. } => {
                Some(person)
            }
            VoiceEvidence::Unknown => None,
        };
        let matched_is_mature = matched.is_some_and(|person| self.is_mature(person));
        let mut identity = match decide_claim(evidence, matched_is_mature, &holders) {
            ClaimDecision::Enrol => {
                let Some(person) =
                    self.create_person(name, false, embedding, turn.speech_ms, turn.at)?
                else {
                    return Ok(TurnIdentity::someone_new());
                };
                let mut identity = self.confirmed_as(person);
                identity.claim = Some(ClaimOutcome::Enrolled);
                identity
            }
            ClaimDecision::Confirmed(person) => {
                self.confirm(person);
                let mut identity = self.confirmed_as(person);
                identity.claim = Some(ClaimOutcome::Confirmed);
                identity
            }
            ClaimDecision::Denied { speaker } => {
                let mut identity = self.confirmed_as(speaker);
                identity.claim = Some(ClaimOutcome::Denied {
                    claimed_name: name.display().to_owned(),
                });
                identity
            }
            ClaimDecision::PossibleAlias { person } => {
                let mut identity = self.confirmed_as(person);
                identity.question = Some(IdentityQuestion::IsNewAlias {
                    person,
                    name: name.display().to_owned(),
                });
                identity
            }
            ClaimDecision::AskIsThisPerson { person } => {
                let mut identity = self.unsure_as(Some(person));
                identity.question = self
                    .display_name(person)
                    .map(|name| IdentityQuestion::IsThisPerson { person, name });
                identity
            }
            ClaimDecision::AskIsNamed => {
                let mut identity = self.unsure_as(matched);
                identity.question = Some(IdentityQuestion::IsNamed {
                    name: name.display().to_owned(),
                });
                identity
            }
        };
        if identity.claim.is_none() {
            identity.claim = Some(ClaimOutcome::Unsure);
        }
        self.remember_question(&identity, embedding, turn.speech_ms);
        Ok(identity)
    }

    fn identify(
        &mut self,
        evidence: &VoiceEvidence,
        embedding: &[f32],
        turn: &VoiceTurn,
        can_ask: bool,
    ) -> TurnIdentity {
        let person = match *evidence {
            VoiceEvidence::Confident { person, .. }
                if self.is_mature(person) || self.confirmed.contains(&person) =>
            {
                return self.confirmed_as(person);
            }
            VoiceEvidence::Confident { person, .. } | VoiceEvidence::Possible { person, .. } => {
                person
            }
            VoiceEvidence::Unknown => return TurnIdentity::someone_new(),
        };
        let mut identity = self.unsure_as(Some(person));
        if can_ask && self.asked.insert(person) {
            identity.question = self
                .display_name(person)
                .map(|name| IdentityQuestion::IsThisPerson { person, name });
            self.remember_question(&identity, embedding, turn.speech_ms);
        }
        identity
    }

    /// A yes confirms a provisional person for the session, adds an alias, or creates a new
    /// person. It never adds samples to a mature person and never links a voice to the
    /// owner: a yes about the owner or a mature person creates a flagged second person.
    fn answer_yes(
        &mut self,
        pending: Pending,
        at: UnixTimestampMillis,
    ) -> SpeakerIdentityResult<TurnIdentity> {
        let (display, needs_review) = match pending.question {
            IdentityQuestion::IsThisPerson { person, name } => {
                let provisional = self.people.get(&person).is_some_and(|known| {
                    !known.is_owner() && known.maturity() == PersonMaturity::Provisional
                });
                if provisional {
                    self.confirm(person);
                    return Ok(self.confirmed_as(person));
                }
                (name, true)
            }
            IdentityQuestion::IsNewAlias { person, name } => {
                if let Some(alias) = name_key(&name).and_then(|key| PersonName::new(name, key).ok())
                {
                    match self.store.add_name(person, alias) {
                        Ok(_) => self.reload()?,
                        Err(error) if is_full_or_duplicate(&error) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                return Ok(self.confirmed_as(person));
            }
            IdentityQuestion::IsNamed { name } => {
                let taken = match name_key(&name) {
                    Some(key) => !self.store.people_named(&key)?.is_empty(),
                    None => false,
                };
                (name, taken)
            }
        };
        let Some(name) = name_key(&display).and_then(|key| PersonName::new(display, key).ok())
        else {
            return Ok(TurnIdentity::someone_new());
        };
        match self.create_person(
            name,
            needs_review,
            &pending.embedding,
            pending.speech_ms,
            at,
        )? {
            Some(person) => Ok(self.confirmed_as(person)),
            None => Ok(TurnIdentity::someone_new()),
        }
    }

    /// Adds the turn as a sample for a confirmed speaker: enrolment speech while a
    /// provisional person is collecting, refinement for a mature one up to the cap.
    fn collect(
        &mut self,
        person: PersonId,
        embedding: &[f32],
        scores: &[(PersonId, f32)],
        turn: &VoiceTurn,
    ) -> SpeakerIdentityResult<()> {
        let eligible = turn.single_segment
            && turn.speech_ms >= self.policy.min_sample_ms
            && !turn.playback_overlap
            && !turn.barge_in;
        if !eligible {
            return Ok(());
        }
        if self.collecting.contains(&person) {
            let near_anchor = self
                .directory
                .anchor(person)
                .is_some_and(|anchor| cosine(embedding, anchor) >= self.policy.anchor_score);
            let best = scores.first().is_some_and(|(best, _)| *best == person);
            if !(near_anchor && best) {
                return Ok(());
            }
            if self.add_sample(person, SignatureRole::Enrolment, embedding, turn)?
                && self.directory.speech_ms(person) >= u64::from(self.policy.maturity_speech_ms)
            {
                self.store.mark_mature(person)?;
                self.collecting.remove(&person);
                self.reload()?;
            }
        } else if self.is_mature(person)
            && self.directory.refinements(person) < self.policy.refinement_cap
        {
            self.add_sample(person, SignatureRole::Refinement, embedding, turn)?;
        }
        Ok(())
    }

    /// Returns whether the sample was stored; a person already holding the maximum is not
    /// an error for the turn.
    fn add_sample(
        &mut self,
        person: PersonId,
        role: SignatureRole,
        embedding: &[f32],
        turn: &VoiceTurn,
    ) -> SpeakerIdentityResult<bool> {
        let draft = self.draft(role, embedding, turn.speech_ms, turn.at)?;
        match self.store.add_signature(&self.key, person, draft) {
            Ok(_) => {
                self.reload()?;
                Ok(true)
            }
            Err(error) if error.kind() == MemoryStoreErrorKind::LimitExceeded => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Creates a provisional person anchored on the given voice, confirmed for this session.
    /// `None` when the store holds the maximum number of people.
    fn create_person(
        &mut self,
        name: PersonName,
        needs_review: bool,
        embedding: &[f32],
        speech_ms: u32,
        at: UnixTimestampMillis,
    ) -> SpeakerIdentityResult<Option<PersonId>> {
        let person = match self
            .store
            .create_person(NewPerson::new(name, needs_review, at))
        {
            Ok(person) => person.id(),
            Err(error) if error.kind() == MemoryStoreErrorKind::LimitExceeded => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let draft = self.draft(SignatureRole::Anchor, embedding, speech_ms, at)?;
        self.store.add_signature(&self.key, person, draft)?;
        self.reload()?;
        self.confirm(person);
        Ok(Some(person))
    }

    fn draft(
        &self,
        role: SignatureRole,
        embedding: &[f32],
        speech_ms: u32,
        at: UnixTimestampMillis,
    ) -> SpeakerIdentityResult<VoiceSignatureDraft> {
        Ok(VoiceSignatureDraft::new(
            self.model.clone(),
            role,
            embedding.to_vec(),
            speech_ms,
            at,
            self.session,
        )?)
    }

    fn confirm(&mut self, person: PersonId) {
        self.confirmed.insert(person);
        if !self.is_mature(person) {
            self.collecting.insert(person);
        }
    }

    fn remember_question(&mut self, identity: &TurnIdentity, embedding: &[f32], speech_ms: u32) {
        if let Some(question) = &identity.question {
            self.pending = Some(Pending {
                question: question.clone(),
                embedding: embedding.to_vec(),
                speech_ms,
            });
        }
    }

    fn confirmed_as(&self, person: PersonId) -> TurnIdentity {
        TurnIdentity {
            label: SpeakerLabel {
                speaker: self.speaker(person),
                confirmed: true,
            },
            claim: None,
            question: None,
            memory_subject: Some(person),
        }
    }

    fn unsure_as(&self, person: Option<PersonId>) -> TurnIdentity {
        TurnIdentity {
            label: SpeakerLabel {
                speaker: person.map_or(Speaker::SomeoneNew, |person| self.speaker(person)),
                confirmed: false,
            },
            claim: None,
            question: None,
            memory_subject: None,
        }
    }

    fn speaker(&self, person: PersonId) -> Speaker {
        Speaker::Person {
            person,
            name: self.display_name(person),
        }
    }

    fn display_name(&self, person: PersonId) -> Option<String> {
        self.people
            .get(&person)
            .and_then(Person::display_name)
            .map(str::to_owned)
    }

    fn is_mature(&self, person: PersonId) -> bool {
        self.people
            .get(&person)
            .is_some_and(|known| known.maturity() == PersonMaturity::Mature)
    }

    fn reload(&mut self) -> SpeakerIdentityResult<()> {
        self.people = self
            .store
            .people()?
            .into_iter()
            .map(|person| (person.id(), person))
            .collect();
        let signatures = self.store.signatures(&self.key, &self.model)?;
        self.directory =
            VoiceDirectory::new(&signatures).map_err(SpeakerIdentityError::Configuration)?;
        Ok(())
    }
}

fn is_full_or_duplicate(error: &MemoryStoreError) -> bool {
    matches!(
        error.kind(),
        MemoryStoreErrorKind::LimitExceeded | MemoryStoreErrorKind::Conflict
    )
}
