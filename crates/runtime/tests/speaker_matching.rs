use conversation_memory::{
    NewPerson, PeopleStore, PersonName, SignatureKey, SignatureModel, SignatureRole,
    SqliteMemoryStore, VoiceSignature, VoiceSignatureDraft,
};
use conversation_protocol::{PersonId, SessionId, UnixTimestampMillis};
use conversation_runtime::{classify, cosine, IdentityPolicy, VoiceDirectory, VoiceEvidence};
use tempfile::{tempdir, TempDir};

const CHRIS: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
const JACK: [f32; 4] = [0.0, 1.0, 0.0, 0.0];

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

struct Fixture {
    _temporary: TempDir,
    store: SqliteMemoryStore,
    key: SignatureKey,
    model: SignatureModel,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempdir().unwrap();
        let store =
            SqliteMemoryStore::initialize(temporary.path().join("runtime.sqlite3")).unwrap();
        Self {
            _temporary: temporary,
            store,
            key: SignatureKey::from_hex(&"11".repeat(32)).unwrap(),
            model: SignatureModel::new("ecapa", "1").unwrap(),
        }
    }

    fn person(&self, name: &str) -> PersonId {
        self.store
            .create_person(NewPerson::new(
                PersonName::new(name, name.to_lowercase()).unwrap(),
                false,
                UnixTimestampMillis::new(1_000).unwrap(),
            ))
            .unwrap()
            .id()
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
                    UnixTimestampMillis::new(1_000).unwrap(),
                    SessionId::new(1),
                )
                .unwrap(),
            )
            .unwrap();
    }

    fn signatures(&self) -> Vec<VoiceSignature> {
        self.store.signatures(&self.key, &self.model).unwrap()
    }
}

fn chris_and_jack() -> (Fixture, PersonId, PersonId) {
    let fixture = Fixture::new();
    let chris = fixture.person("Chris");
    let jack = fixture.person("Jack");
    fixture.sample(chris, SignatureRole::Anchor, &CHRIS, 1_000);
    fixture.sample(jack, SignatureRole::Anchor, &JACK, 1_000);
    (fixture, chris, jack)
}

#[test]
fn best_match_is_confident_when_it_clears_score_and_margin() {
    let (fixture, chris, _jack) = chris_and_jack();
    let directory = VoiceDirectory::new(&fixture.signatures()).unwrap();
    let scores = directory.scores(&[0.95, 0.05, 0.0, 0.0]).unwrap();
    assert_eq!(scores[0].0, chris);
    assert!(matches!(
        classify(&scores, &policy()),
        VoiceEvidence::Confident { person, .. } if person == chris
    ));
}

#[test]
fn a_close_runner_up_makes_the_match_possible_not_confident() {
    let (fixture, _chris, _jack) = chris_and_jack();
    let directory = VoiceDirectory::new(&fixture.signatures()).unwrap();
    let scores = directory.scores(&[0.7, 0.68, 0.0, 0.2]).unwrap();
    assert!(scores[0].1 >= 0.6 && scores[0].1 - scores[1].1 < 0.1);
    assert!(matches!(
        classify(&scores, &policy()),
        VoiceEvidence::Possible { .. }
    ));
}

#[test]
fn a_low_score_is_unknown() {
    let (fixture, _chris, _jack) = chris_and_jack();
    let directory = VoiceDirectory::new(&fixture.signatures()).unwrap();
    let scores = directory.scores(&[0.0, 0.0, 1.0, 0.0]).unwrap();
    assert_eq!(classify(&scores, &policy()), VoiceEvidence::Unknown);
    assert_eq!(classify(&[], &policy()), VoiceEvidence::Unknown);
}

#[test]
fn a_single_known_person_is_measured_against_zero() {
    let fixture = Fixture::new();
    let chris = fixture.person("Chris");
    fixture.sample(chris, SignatureRole::Anchor, &CHRIS, 1_000);
    let directory = VoiceDirectory::new(&fixture.signatures()).unwrap();
    let scores = directory.scores(&[0.9, 0.1, 0.0, 0.0]).unwrap();
    assert!(matches!(
        classify(&scores, &policy()),
        VoiceEvidence::Confident { person, .. } if person == chris
    ));
}

#[test]
fn centroid_uses_every_sample_of_a_person() {
    let fixture = Fixture::new();
    let jack = fixture.person("Jack");
    fixture.sample(jack, SignatureRole::Anchor, &[0.0, 1.0, 0.0, 0.0], 1_000);
    fixture.sample(jack, SignatureRole::Enrolment, &[0.0, 0.8, 0.6, 0.0], 2_000);
    let directory = VoiceDirectory::new(&fixture.signatures()).unwrap();
    let query = [0.0, 0.9, 0.3, 0.0];
    let score = directory.scores(&query).unwrap()[0].1;
    assert!(score > 0.99, "{score}");
    assert!(score > cosine(&query, &[0.0, 1.0, 0.0, 0.0]));
    assert!(score > cosine(&query, &[0.0, 0.8, 0.6, 0.0]));
}

#[test]
fn mixed_dimensions_are_refused_and_mismatched_queries_score_nothing() {
    let fixture = Fixture::new();
    let chris = fixture.person("Chris");
    fixture.sample(chris, SignatureRole::Anchor, &CHRIS, 1_000);
    let directory = VoiceDirectory::new(&fixture.signatures()).unwrap();
    assert_eq!(directory.dimension(), Some(4));
    assert!(directory.scores(&[1.0, 0.0, 0.0]).is_none());

    fixture.sample(chris, SignatureRole::Enrolment, &[1.0, 0.0, 0.0], 2_000);
    assert!(VoiceDirectory::new(&fixture.signatures()).is_err());
}

#[test]
fn an_empty_directory_scores_nothing() {
    let directory = VoiceDirectory::new(&[]).unwrap();
    assert_eq!(directory.dimension(), None);
    assert!(directory.scores(&CHRIS).is_none());
}

#[test]
fn directory_reports_refinements_speech_and_anchor() {
    let (fixture, chris, jack) = chris_and_jack();
    fixture.sample(chris, SignatureRole::Enrolment, &CHRIS, 2_000);
    fixture.sample(chris, SignatureRole::Refinement, &CHRIS, 1_600);
    fixture.sample(chris, SignatureRole::Refinement, &CHRIS, 1_700);
    let directory = VoiceDirectory::new(&fixture.signatures()).unwrap();
    assert_eq!(directory.refinements(chris), 2);
    assert_eq!(directory.refinements(jack), 0);
    assert_eq!(directory.speech_ms(chris), 6_300);
    assert_eq!(directory.anchor(jack), Some(&JACK[..]));
    assert_eq!(directory.anchor(PersonId::OWNER), None);
}

#[test]
fn cosine_of_a_zero_vector_is_zero() {
    assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
}

#[test]
fn policy_validation_rejects_inverted_or_short_values() {
    assert!(policy().validate().is_ok());
    let cases: [fn(&mut IdentityPolicy); 7] = [
        |p| p.possible_score = 0.9,
        |p| p.anchor_score = 0.7,
        |p| p.possible_score = 0.0,
        |p| p.margin = 1.5,
        |p| p.min_sample_ms = 1_499,
        |p| p.maturity_speech_ms = 1_000,
        |p| p.refinement_cap = 65,
    ];
    for change in cases {
        let mut candidate = policy();
        change(&mut candidate);
        assert!(candidate.validate().is_err(), "{candidate:?}");
    }
}
