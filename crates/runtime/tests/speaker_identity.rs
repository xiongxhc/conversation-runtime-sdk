use std::sync::Arc;

use conversation_memory::{
    NewPerson, PeopleStore, PersonMaturity, PersonName, SignatureKey, SignatureModel,
    SignatureRole, SqliteMemoryStore, VoiceSignatureDraft,
};
use conversation_protocol::{PersonId, SessionId, UnixTimestampMillis};
use conversation_runtime::{
    ClaimOutcome, IdentityPolicy, IdentityQuestion, Speaker, SpeakerIdentitySession, TurnIdentity,
    VoiceTurn,
};
use tempfile::{tempdir, TempDir};

const CHRIS: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
const JACK: [f32; 4] = [0.0, 1.0, 0.0, 0.0];
const NEW: [f32; 4] = [0.0, 0.0, 1.0, 0.0];
/// Scores about 0.7 against Chris: possible, not confident.
const WEAK_CHRIS: [f32; 4] = [0.7, 0.0, 0.0, 0.714];

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

fn at() -> UnixTimestampMillis {
    UnixTimestampMillis::new(10_000).unwrap()
}

fn turn(text: &str, claim: Option<&str>, embedding: Option<&[f32]>, speech_ms: u32) -> VoiceTurn {
    VoiceTurn {
        transcript: text.to_owned(),
        claimed_name: claim.map(str::to_owned),
        embedding: embedding.map(<[f32]>::to_vec),
        speech_ms,
        single_segment: true,
        playback_overlap: false,
        barge_in: false,
        at: at(),
    }
}

/// A short reply such as "是" is too short to embed.
fn answer(text: &str) -> VoiceTurn {
    turn(text, None, None, 400)
}

struct Fixture {
    _temporary: TempDir,
    store: Arc<SqliteMemoryStore>,
    key: Arc<SignatureKey>,
    model: SignatureModel,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let store = Arc::new(
            SqliteMemoryStore::initialize(temporary.path().join("runtime.sqlite3")).unwrap(),
        );
        Self {
            _temporary: temporary,
            store,
            key: Arc::new(SignatureKey::from_hex(&"11".repeat(32)).unwrap()),
            model: SignatureModel::new("ecapa", "1").unwrap(),
        }
    }

    fn session(&self, id: u64) -> SpeakerIdentitySession {
        SpeakerIdentitySession::start(
            self.store.clone(),
            self.key.clone(),
            self.model.clone(),
            policy(),
            SessionId::new(id),
        )
        .unwrap()
    }

    fn sample(&self, person: PersonId, role: SignatureRole, embedding: &[f32], speech_ms: u32) {
        self.store
            .add_signature(
                &self.key,
                person,
                VoiceSignatureDraft::new(
                    self.model.clone(),
                    role,
                    embedding.to_vec(),
                    speech_ms,
                    at(),
                    SessionId::new(1),
                )
                .unwrap(),
            )
            .unwrap();
    }

    fn person(&self, name: &str, embedding: &[f32], mature: bool) -> PersonId {
        let id = self
            .store
            .create_person(NewPerson::new(
                PersonName::new(name, name.to_lowercase()).unwrap(),
                false,
                at(),
            ))
            .unwrap()
            .id();
        self.sample(id, SignatureRole::Anchor, embedding, 1_000);
        if mature {
            self.sample(id, SignatureRole::Enrolment, embedding, 5_000);
            self.store.mark_mature(id).unwrap();
        }
        id
    }

    fn named(&self, key: &str) -> Vec<PersonId> {
        self.store.people_named(key).unwrap()
    }

    fn roles(&self, person: PersonId) -> Vec<SignatureRole> {
        self.store
            .signatures(&self.key, &self.model)
            .unwrap()
            .iter()
            .filter(|signature| signature.person() == person)
            .map(|signature| signature.role())
            .collect()
    }

    fn signature_count(&self) -> usize {
        self.store.signatures(&self.key, &self.model).unwrap().len()
    }
}

fn person_of(identity: &TurnIdentity) -> Option<PersonId> {
    match identity.label.speaker {
        Speaker::Person { person, .. } => Some(person),
        Speaker::SomeoneNew => None,
    }
}

fn assert_someone_new(identity: &TurnIdentity) {
    assert_eq!(identity.label.speaker, Speaker::SomeoneNew);
    assert!(!identity.label.confirmed);
    assert_eq!(identity.memory_subject, None);
}

#[test]
fn saying_a_name_enrols_a_provisional_person_with_an_anchor() {
    let fixture = Fixture::new();
    let mut session = fixture.session(1);

    let identity = session
        .observe(turn("I'm Jack", Some("Jack"), Some(&JACK), 1_000))
        .unwrap();

    assert_eq!(identity.claim, Some(ClaimOutcome::Enrolled));
    let jack = fixture.named("jack")[0];
    assert_eq!(
        fixture.store.person(jack).unwrap().maturity(),
        PersonMaturity::Provisional
    );
    assert_eq!(person_of(&identity), Some(jack));
    assert!(identity.label.confirmed);
    assert_eq!(identity.memory_subject, Some(jack));
    assert_eq!(identity.question, None);
    assert_eq!(fixture.roles(jack), vec![SignatureRole::Anchor]);
}

#[test]
fn following_turns_mature_the_person_from_collected_speech() {
    let fixture = Fixture::new();
    let mut session = fixture.session(1);
    session
        .observe(turn("I'm Jack", Some("Jack"), Some(&JACK), 1_000))
        .unwrap();
    let jack = fixture.named("jack")[0];

    let second = session
        .observe(turn("nice to meet you", None, Some(&JACK), 2_000))
        .unwrap();
    assert!(second.label.confirmed);
    assert_eq!(person_of(&second), Some(jack));
    assert_eq!(
        fixture.store.person(jack).unwrap().maturity(),
        PersonMaturity::Provisional
    );

    session
        .observe(turn("how are you", None, Some(&JACK), 2_000))
        .unwrap();
    assert_eq!(
        fixture.store.person(jack).unwrap().maturity(),
        PersonMaturity::Mature
    );
    assert_eq!(
        fixture.roles(jack),
        vec![
            SignatureRole::Anchor,
            SignatureRole::Enrolment,
            SignatureRole::Enrolment
        ]
    );
}

#[test]
fn short_multi_segment_or_off_anchor_turns_are_not_collected() {
    let fixture = Fixture::new();
    let mut session = fixture.session(1);
    session
        .observe(turn("I'm Jack", Some("Jack"), Some(&JACK), 1_000))
        .unwrap();
    let jack = fixture.named("jack")[0];

    session
        .observe(turn("short", None, Some(&JACK), 1_400))
        .unwrap();
    let mut joined = turn("two segments", None, Some(&JACK), 3_000);
    joined.single_segment = false;
    session.observe(joined).unwrap();
    // Confidently Jack (about 0.83) but under the stricter anchor score of 0.85.
    session
        .observe(turn(
            "off anchor",
            None,
            Some(&[0.0, 0.83, 0.558, 0.0]),
            3_000,
        ))
        .unwrap();

    assert_eq!(fixture.roles(jack), vec![SignatureRole::Anchor]);
}

#[test]
fn a_yes_alone_never_matures() {
    let fixture = Fixture::new();
    let jack = fixture.person("Jack", &JACK, false);
    let mut session = fixture.session(2);

    let asked = session
        .observe(turn("hello", None, Some(&JACK), 1_000))
        .unwrap();
    assert!(!asked.label.confirmed);
    assert_eq!(asked.memory_subject, None);
    assert_eq!(
        asked.question,
        Some(IdentityQuestion::IsThisPerson {
            person: jack,
            name: "Jack".to_owned()
        })
    );

    let confirmed = session.observe(answer("是")).unwrap();

    assert_eq!(person_of(&confirmed), Some(jack));
    assert!(confirmed.label.confirmed);
    assert_eq!(confirmed.memory_subject, Some(jack));
    assert_eq!(
        fixture.store.person(jack).unwrap().maturity(),
        PersonMaturity::Provisional
    );
}

#[test]
fn a_confirmed_provisional_person_keeps_collecting_until_mature() {
    let fixture = Fixture::new();
    let jack = fixture.person("Jack", &JACK, false);
    let mut session = fixture.session(2);
    session
        .observe(turn("hello", None, Some(&JACK), 1_000))
        .unwrap();
    session.observe(answer("yes")).unwrap();

    for _ in 0..2 {
        let identity = session
            .observe(turn("more speech", None, Some(&JACK), 2_000))
            .unwrap();
        assert!(identity.label.confirmed);
    }

    assert_eq!(
        fixture.store.person(jack).unwrap().maturity(),
        PersonMaturity::Mature
    );
}

#[test]
fn returning_mature_person_is_greeted_without_asking() {
    let fixture = Fixture::new();
    let jack = fixture.person("Jack", &JACK, true);
    let mut session = fixture.session(2);

    let identity = session
        .observe(turn("hi again", None, Some(&JACK), 1_000))
        .unwrap();

    assert_eq!(
        identity.label.speaker,
        Speaker::Person {
            person: jack,
            name: Some("Jack".to_owned())
        }
    );
    assert!(identity.label.confirmed);
    assert_eq!(identity.question, None);
    assert_eq!(identity.memory_subject, Some(jack));
}

#[test]
fn a_false_claim_is_denied_and_the_real_speaker_gets_the_memories() {
    let fixture = Fixture::new();
    fixture.person("Chris", &CHRIS, true);
    let jack = fixture.person("Jack", &JACK, true);
    let mut session = fixture.session(2);

    let identity = session
        .observe(turn("I'm Chris", Some("Chris"), Some(&JACK), 1_000))
        .unwrap();

    assert_eq!(
        identity.claim,
        Some(ClaimOutcome::Denied {
            claimed_name: "Chris".to_owned()
        })
    );
    assert_eq!(person_of(&identity), Some(jack));
    assert!(identity.label.confirmed);
    assert_eq!(identity.memory_subject, Some(jack));
    assert_eq!(identity.question, None);
}

#[test]
fn a_changed_voice_is_asked_not_denied() {
    let fixture = Fixture::new();
    let chris = fixture.person("Chris", &CHRIS, true);
    let mut session = fixture.session(2);

    let identity = session
        .observe(turn("I'm Chris", Some("Chris"), Some(&WEAK_CHRIS), 1_000))
        .unwrap();

    assert_eq!(identity.claim, Some(ClaimOutcome::Unsure));
    assert_eq!(
        identity.question,
        Some(IdentityQuestion::IsNamed {
            name: "Chris".to_owned()
        })
    );
    assert_eq!(person_of(&identity), Some(chris));
    assert!(!identity.label.confirmed);
    assert_eq!(identity.memory_subject, None);
}

#[test]
fn an_alias_is_asked_and_a_yes_adds_it() {
    let fixture = Fixture::new();
    let chris = fixture.person("Chris", &CHRIS, true);
    let mut session = fixture.session(2);

    let asked = session
        .observe(turn("我是昊辰", Some("昊辰"), Some(&CHRIS), 1_000))
        .unwrap();
    assert_eq!(asked.claim, Some(ClaimOutcome::Unsure));
    assert_eq!(
        asked.question,
        Some(IdentityQuestion::IsNewAlias {
            person: chris,
            name: "昊辰".to_owned()
        })
    );
    assert_eq!(asked.memory_subject, Some(chris));

    let confirmed = session.observe(answer("对")).unwrap();

    assert_eq!(person_of(&confirmed), Some(chris));
    assert_eq!(fixture.named("haochen"), vec![chris]);
    assert_eq!(fixture.named("chris"), vec![chris]);
}

#[test]
fn yes_about_the_owner_creates_a_flagged_duplicate() {
    let fixture = Fixture::new();
    fixture
        .store
        .rename(PersonId::OWNER, PersonName::new("Chris", "chris").unwrap())
        .unwrap();
    fixture.sample(PersonId::OWNER, SignatureRole::Enrolment, &CHRIS, 6_000);
    let owner_roles = fixture.roles(PersonId::OWNER);
    let mut session = fixture.session(2);

    let asked = session
        .observe(turn("I'm Chris", Some("Chris"), Some(&NEW), 2_000))
        .unwrap();
    assert_eq!(
        asked.question,
        Some(IdentityQuestion::IsNamed {
            name: "Chris".to_owned()
        })
    );
    let confirmed = session.observe(answer("yes")).unwrap();

    let chrises = fixture.named("chris");
    assert_eq!(chrises.len(), 2);
    let duplicate = chrises[1];
    assert!(fixture.store.person(duplicate).unwrap().needs_review());
    assert_eq!(fixture.roles(duplicate), vec![SignatureRole::Anchor]);
    assert_eq!(fixture.roles(PersonId::OWNER), owner_roles);
    assert_eq!(confirmed.memory_subject, Some(duplicate));
    assert_ne!(asked.memory_subject, Some(PersonId::OWNER));

    let later = session
        .observe(turn("still me", None, Some(&NEW), 2_000))
        .unwrap();
    assert_eq!(person_of(&later), Some(duplicate));
    assert_ne!(later.memory_subject, Some(PersonId::OWNER));
}

#[test]
fn a_possible_owner_voice_is_asked_and_a_yes_never_links_it_to_the_owner() {
    let fixture = Fixture::new();
    fixture
        .store
        .rename(PersonId::OWNER, PersonName::new("Chris", "chris").unwrap())
        .unwrap();
    fixture.sample(PersonId::OWNER, SignatureRole::Enrolment, &CHRIS, 6_000);
    let mut session = fixture.session(2);

    let asked = session
        .observe(turn("hello", None, Some(&WEAK_CHRIS), 2_000))
        .unwrap();
    assert!(matches!(
        asked.question,
        Some(IdentityQuestion::IsThisPerson { person, .. }) if person == PersonId::OWNER
    ));
    let confirmed = session.observe(answer("yes")).unwrap();

    assert_ne!(confirmed.memory_subject, Some(PersonId::OWNER));
    assert_eq!(fixture.named("chris").len(), 2);
    assert_eq!(
        fixture.roles(PersonId::OWNER),
        vec![SignatureRole::Enrolment]
    );
}

#[test]
fn yes_never_adds_samples_to_a_mature_person() {
    let fixture = Fixture::new();
    let chris = fixture.person("Chris", &CHRIS, true);
    let before = fixture.roles(chris);
    let mut session = fixture.session(2);

    session
        .observe(turn("hello", None, Some(&WEAK_CHRIS), 2_000))
        .unwrap();
    session.observe(answer("yes")).unwrap();

    assert_eq!(fixture.roles(chris), before);
}

#[test]
fn a_no_clears_the_question_without_change() {
    let fixture = Fixture::new();
    let jack = fixture.person("Jack", &JACK, false);
    let mut session = fixture.session(2);
    session
        .observe(turn("hello", None, Some(&JACK), 1_000))
        .unwrap();
    let people_before = fixture.store.people().unwrap();

    let refused = session.observe(answer("不是")).unwrap();

    assert_someone_new(&refused);
    assert_someone_new(&session.observe(answer("是")).unwrap());
    assert_eq!(fixture.store.people().unwrap(), people_before);
    assert_eq!(fixture.roles(jack), vec![SignatureRole::Anchor]);
}

#[test]
fn answer_from_a_different_known_voice_is_not_an_answer() {
    let fixture = Fixture::new();
    let chris = fixture.person("Chris", &CHRIS, true);
    fixture.person("Jack", &JACK, false);
    let mut session = fixture.session(2);
    session
        .observe(turn("hello", None, Some(&JACK), 1_000))
        .unwrap();

    let identity = session
        .observe(turn("yes", None, Some(&CHRIS), 1_000))
        .unwrap();

    assert_eq!(person_of(&identity), Some(chris));
    assert!(identity.label.confirmed);
    let later = session
        .observe(turn("hello", None, Some(&JACK), 1_000))
        .unwrap();
    assert!(!later.label.confirmed);
    assert_eq!(later.question, None);
}

#[test]
fn an_unrelated_reply_clears_the_question() {
    let fixture = Fixture::new();
    let jack = fixture.person("Jack", &JACK, false);
    let mut session = fixture.session(2);
    session
        .observe(turn("hello", None, Some(&JACK), 1_000))
        .unwrap();

    session.observe(answer("what's the weather")).unwrap();
    let late_yes = session.observe(answer("是")).unwrap();

    assert_someone_new(&late_yes);
    assert_eq!(
        fixture.store.person(jack).unwrap().maturity(),
        PersonMaturity::Provisional
    );
}

#[test]
fn the_same_person_is_asked_about_once_per_session() {
    let fixture = Fixture::new();
    fixture.person("Jack", &JACK, false);
    let mut session = fixture.session(2);

    let first = session
        .observe(turn("hello", None, Some(&JACK), 1_000))
        .unwrap();
    session.observe(answer("hmm")).unwrap();
    let second = session
        .observe(turn("hello again", None, Some(&JACK), 1_000))
        .unwrap();

    assert!(first.question.is_some());
    assert_eq!(second.question, None);
    assert!(!second.label.confirmed);
}

#[test]
fn someone_new_and_unsure_turns_store_nothing_and_retrieve_nothing() {
    let fixture = Fixture::new();
    fixture.person("Chris", &CHRIS, true);
    let people_before = fixture.store.people().unwrap();
    let signatures_before = fixture.signature_count();
    let mut session = fixture.session(2);

    assert_someone_new(
        &session
            .observe(turn("hi", None, Some(&NEW), 2_000))
            .unwrap(),
    );
    let possible = session
        .observe(turn("hi", None, Some(&WEAK_CHRIS), 2_000))
        .unwrap();
    assert!(!possible.label.confirmed);
    assert_eq!(possible.memory_subject, None);
    session.observe(answer("hmm")).unwrap();
    assert_someone_new(&session.observe(turn("hi", None, None, 2_000)).unwrap());

    assert_eq!(fixture.store.people().unwrap(), people_before);
    assert_eq!(fixture.signature_count(), signatures_before);
}

#[test]
fn playback_overlapped_claim_enrols_nobody() {
    let fixture = Fixture::new();
    let mut session = fixture.session(1);
    let mut companion = turn("I'm Serena", Some("Serena"), Some(&NEW), 2_000);
    companion.playback_overlap = true;

    let identity = session.observe(companion).unwrap();

    assert_someone_new(&identity);
    assert_eq!(identity.claim, None);
    assert_eq!(identity.question, None);
    assert!(fixture.named("serena").is_empty());
    assert_eq!(fixture.signature_count(), 0);
}

#[test]
fn barge_in_turns_are_identified_but_never_sampled() {
    let fixture = Fixture::new();
    let jack = fixture.person("Jack", &JACK, true);
    let before = fixture.roles(jack);
    let mut session = fixture.session(2);
    let mut interrupting = turn("wait", None, Some(&JACK), 3_000);
    interrupting.barge_in = true;

    let identity = session.observe(interrupting).unwrap();

    assert_eq!(person_of(&identity), Some(jack));
    assert!(identity.label.confirmed);
    assert_eq!(fixture.roles(jack), before);
}

#[test]
fn refinement_stops_at_the_cap() {
    let fixture = Fixture::new();
    let jack = fixture.person("Jack", &JACK, true);
    let mut session = fixture.session(2);

    for _ in 0..4 {
        session
            .observe(turn("talking", None, Some(&JACK), 2_000))
            .unwrap();
    }

    let refinements = fixture
        .roles(jack)
        .into_iter()
        .filter(|role| *role == SignatureRole::Refinement)
        .count();
    assert_eq!(refinements, 3);
}

#[test]
fn embedding_of_another_dimension_is_unsure() {
    let fixture = Fixture::new();
    fixture.person("Chris", &CHRIS, true);
    let mut session = fixture.session(2);

    let identity = session
        .observe(turn("I'm Tom", Some("Tom"), Some(&[1.0, 0.0, 0.0]), 2_000))
        .unwrap();

    assert_someone_new(&identity);
    assert!(fixture.named("tom").is_empty());
}

#[test]
fn nothing_said_renames_merges_or_deletes_anyone() {
    let fixture = Fixture::new();
    let chris = fixture.person("Chris", &CHRIS, true);
    let jack = fixture.person("Jack", &JACK, true);
    let mut session = fixture.session(2);

    for spoken in [
        turn("I'm Chris", Some("Chris"), Some(&JACK), 2_000),
        turn("I'm Jack", Some("Jack"), Some(&NEW), 2_000),
        answer("yes"),
        turn("我是昊辰", Some("昊辰"), Some(&CHRIS), 2_000),
        answer("对"),
        turn("call me Jack", Some("Jack"), Some(&CHRIS), 2_000),
        answer("yes"),
    ] {
        session.observe(spoken).unwrap();
    }

    assert_eq!(
        fixture.store.person(chris).unwrap().display_name(),
        Some("Chris")
    );
    assert_eq!(
        fixture.store.person(jack).unwrap().display_name(),
        Some("Jack")
    );
    assert!(fixture.store.person(PersonId::OWNER).unwrap().is_owner());
}

#[test]
fn an_invalid_policy_refuses_to_start() {
    let fixture = Fixture::new();
    let mut invalid = policy();
    invalid.min_sample_ms = 1_000;
    assert!(SpeakerIdentitySession::start(
        fixture.store.clone(),
        fixture.key.clone(),
        fixture.model.clone(),
        invalid,
        SessionId::new(1),
    )
    .is_err());
}

fn owner_with_voice(fixture: &Fixture, name: Option<&str>) {
    if let Some(name) = name {
        fixture
            .store
            .rename(
                PersonId::OWNER,
                PersonName::new(name, name.to_lowercase()).unwrap(),
            )
            .unwrap();
    }
    fixture.sample(PersonId::OWNER, SignatureRole::Anchor, &CHRIS, 1_500);
}

#[test]
fn the_owners_own_voice_is_recognised_without_asking() {
    let fixture = Fixture::new();
    owner_with_voice(&fixture, Some("Chris"));
    let mut session = fixture.session(2);

    let identity = session
        .observe(turn("hello", None, Some(&CHRIS), 2_000))
        .unwrap();

    assert_eq!(person_of(&identity), Some(PersonId::OWNER));
    assert!(identity.label.confirmed);
    assert_eq!(identity.question, None);
    assert_eq!(identity.memory_subject, Some(PersonId::OWNER));
    session.observe(answer("yes")).unwrap();
    assert_eq!(fixture.named("chris"), vec![PersonId::OWNER]);
}

#[test]
fn the_owner_never_gains_a_spoken_alias() {
    let fixture = Fixture::new();
    owner_with_voice(&fixture, None);
    let mut session = fixture.session(2);

    let unnamed = session
        .observe(turn("I'm Bob", Some("Bob"), Some(&CHRIS), 2_000))
        .unwrap();
    assert_eq!(unnamed.question, None);
    session.observe(answer("yes")).unwrap();
    assert_eq!(
        fixture
            .store
            .person(PersonId::OWNER)
            .unwrap()
            .display_name(),
        None
    );

    fixture
        .store
        .rename(PersonId::OWNER, PersonName::new("Chris", "chris").unwrap())
        .unwrap();
    let mut session = fixture.session(3);
    let named = session
        .observe(turn("我是昊辰", Some("昊辰"), Some(&CHRIS), 2_000))
        .unwrap();
    assert_eq!(named.question, None);
    assert!(fixture.named("haochen").is_empty());
}

#[test]
fn invalid_embeddings_are_treated_as_missing() {
    let fixture = Fixture::new();
    let mut session = fixture.session(1);

    for embedding in [
        vec![f32::NAN, 0.0, 0.0, 0.0],
        vec![0.0, 0.0, 0.0, 0.0],
        vec![f32::INFINITY, 0.0, 0.0, 0.0],
        Vec::new(),
    ] {
        let identity = session
            .observe(turn("I'm Jack", Some("Jack"), Some(&embedding), 2_000))
            .unwrap();
        assert_someone_new(&identity);
    }

    assert!(fixture.named("jack").is_empty());
    assert_eq!(fixture.store.people().unwrap().len(), 1);
}

#[test]
fn a_person_deleted_mid_session_does_not_fail_later_turns() {
    let fixture = Fixture::new();
    let jack = fixture.person("Jack", &JACK, true);
    let mut session = fixture.session(2);
    fixture.store.delete_person(jack).unwrap();

    session
        .observe(turn("talking", None, Some(&JACK), 2_000))
        .unwrap();
    let later = session
        .observe(turn("still talking", None, Some(&JACK), 2_000))
        .unwrap();

    assert_someone_new(&later);
}

#[test]
fn someone_added_mid_session_still_holds_their_name() {
    let fixture = Fixture::new();
    let mut session = fixture.session(2);
    fixture.person("Tom", &JACK, true);

    let identity = session
        .observe(turn("I'm Tom", Some("Tom"), Some(&NEW), 2_000))
        .unwrap();

    assert_eq!(
        identity.question,
        Some(IdentityQuestion::IsNamed {
            name: "Tom".to_owned()
        })
    );
    assert_eq!(fixture.named("tom").len(), 1);
}

#[test]
fn refresh_picks_up_people_changed_outside_the_session() {
    let fixture = Fixture::new();
    let mut session = fixture.session(2);
    let tom = fixture.person("Tom", &JACK, true);

    session.refresh().unwrap();
    let identity = session
        .observe(turn("hello", None, Some(&JACK), 1_000))
        .unwrap();

    assert_eq!(person_of(&identity), Some(tom));
    assert!(identity.label.confirmed);
}

#[test]
fn playback_overlapped_turn_is_not_an_answer() {
    let fixture = Fixture::new();
    let jack = fixture.person("Jack", &JACK, false);
    let mut session = fixture.session(2);
    session
        .observe(turn("hello", None, Some(&JACK), 1_000))
        .unwrap();
    let mut echo = answer("嗯");
    echo.playback_overlap = true;

    let echoed = session.observe(echo).unwrap();
    assert_ne!(echoed.memory_subject, Some(jack));
    let real = session.observe(answer("yes")).unwrap();

    assert_eq!(person_of(&real), Some(jack));
    assert!(real.label.confirmed);
}

#[test]
fn someone_confirmed_this_session_is_not_asked_about_again() {
    let fixture = Fixture::new();
    fixture.person("Chris", &CHRIS, true);
    let mut session = fixture.session(2);
    session
        .observe(turn("hello", None, Some(&CHRIS), 1_000))
        .unwrap();

    let coughing = session
        .observe(turn("sorry", None, Some(&WEAK_CHRIS), 2_000))
        .unwrap();

    assert_eq!(coughing.question, None);
    assert_eq!(coughing.memory_subject, None);
}
