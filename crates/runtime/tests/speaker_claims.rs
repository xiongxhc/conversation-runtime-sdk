use conversation_protocol::PersonId;
use conversation_runtime::{decide_claim, ClaimDecision, ClaimHolder, VoiceEvidence};

fn id(value: u64) -> PersonId {
    PersonId::new(value).unwrap()
}

const OWNER: PersonId = PersonId::OWNER;

fn confident(person: PersonId) -> VoiceEvidence {
    VoiceEvidence::Confident { person, score: 0.9 }
}

fn possible(person: PersonId) -> VoiceEvidence {
    VoiceEvidence::Possible { person, score: 0.7 }
}

fn mature(person: PersonId) -> ClaimHolder {
    ClaimHolder {
        person,
        mature: true,
        owner: person == OWNER,
    }
}

fn provisional(person: PersonId) -> ClaimHolder {
    ClaimHolder {
        person,
        mature: false,
        owner: person == OWNER,
    }
}

#[test]
fn unknown_voice_with_a_free_name_enrols() {
    assert_eq!(
        decide_claim(&VoiceEvidence::Unknown, false, &[]),
        ClaimDecision::Enrol
    );
}

#[test]
fn voice_matching_the_claimed_person_confirms() {
    let jack = id(2);
    assert_eq!(
        decide_claim(&confident(jack), true, &[mature(jack)]),
        ClaimDecision::Confirmed(jack)
    );
    assert_eq!(
        decide_claim(&confident(jack), false, &[provisional(jack)]),
        ClaimDecision::Confirmed(jack)
    );
    assert_eq!(
        decide_claim(&confident(jack), true, &[mature(id(5)), mature(jack)]),
        ClaimDecision::Confirmed(jack)
    );
}

#[test]
fn a_mature_voice_claiming_someone_elses_name_is_denied() {
    let (chris, jack) = (id(2), id(3));
    assert_eq!(
        decide_claim(&confident(jack), true, &[mature(chris)]),
        ClaimDecision::Denied { speaker: jack }
    );
    assert_eq!(
        decide_claim(&confident(jack), true, &[mature(OWNER)]),
        ClaimDecision::Denied { speaker: jack }
    );
}

#[test]
fn a_mature_voice_claiming_a_free_name_is_a_possible_alias() {
    let chris = id(2);
    assert_eq!(
        decide_claim(&confident(chris), true, &[]),
        ClaimDecision::PossibleAlias { person: chris }
    );
}

#[test]
fn a_provisional_voice_never_denies_and_is_asked_about_its_first_name() {
    let (chris, jack) = (id(2), id(3));
    assert_eq!(
        decide_claim(&confident(jack), false, &[mature(chris)]),
        ClaimDecision::AskIsThisPerson { person: jack }
    );
    assert_eq!(
        decide_claim(&confident(jack), false, &[]),
        ClaimDecision::AskIsThisPerson { person: jack }
    );
}

#[test]
fn a_weak_match_to_the_claimed_person_never_denies() {
    let chris = id(2);
    assert_eq!(
        decide_claim(&possible(chris), true, &[mature(chris)]),
        ClaimDecision::AskIsNamed
    );
}

#[test]
fn a_stranger_claiming_the_owners_name_is_asked_never_confirmed() {
    assert_eq!(
        decide_claim(&VoiceEvidence::Unknown, false, &[mature(OWNER)]),
        ClaimDecision::AskIsNamed
    );
    assert_eq!(
        decide_claim(&VoiceEvidence::Unknown, false, &[provisional(OWNER)]),
        ClaimDecision::AskIsNamed
    );
    assert_eq!(
        decide_claim(&possible(OWNER), true, &[mature(OWNER)]),
        ClaimDecision::AskIsNamed
    );
}

#[test]
fn an_unknown_voice_claiming_a_provisional_name_is_asked_about_that_person() {
    let (jack, other_jack) = (id(3), id(4));
    assert_eq!(
        decide_claim(
            &VoiceEvidence::Unknown,
            false,
            &[provisional(other_jack), provisional(jack)]
        ),
        ClaimDecision::AskIsThisPerson { person: jack }
    );
    assert_eq!(
        decide_claim(
            &possible(other_jack),
            false,
            &[provisional(jack), provisional(other_jack)]
        ),
        ClaimDecision::AskIsThisPerson { person: other_jack }
    );
}

#[test]
fn a_mature_holder_among_provisional_ones_makes_it_a_new_person_question() {
    let (jack, other_jack) = (id(3), id(4));
    assert_eq!(
        decide_claim(
            &VoiceEvidence::Unknown,
            false,
            &[provisional(jack), mature(other_jack)]
        ),
        ClaimDecision::AskIsNamed
    );
}

#[test]
fn a_possible_match_with_a_free_name_is_asked_not_enrolled() {
    assert_eq!(
        decide_claim(&possible(id(3)), true, &[]),
        ClaimDecision::AskIsNamed
    );
    assert_eq!(
        decide_claim(&possible(id(3)), false, &[]),
        ClaimDecision::AskIsNamed
    );
}

#[test]
fn no_decision_names_the_owner_from_an_unconfirmed_voice() {
    let others = [id(2), id(3)];
    let evidences = [
        VoiceEvidence::Unknown,
        possible(OWNER),
        possible(others[0]),
        confident(others[0]),
    ];
    let holder_sets: [Vec<ClaimHolder>; 4] = [
        vec![mature(OWNER)],
        vec![provisional(OWNER)],
        vec![mature(OWNER), provisional(others[1])],
        vec![provisional(OWNER), mature(others[1])],
    ];
    for evidence in &evidences {
        for matched_is_mature in [false, true] {
            for holders in &holder_sets {
                let decision = decide_claim(evidence, matched_is_mature, holders);
                assert!(
                    !matches!(
                        decision,
                        ClaimDecision::Confirmed(person)
                            | ClaimDecision::Denied { speaker: person }
                            | ClaimDecision::PossibleAlias { person }
                            | ClaimDecision::AskIsThisPerson { person }
                            if person == OWNER
                    ),
                    "{evidence:?} {matched_is_mature} {holders:?} -> {decision:?}"
                );
            }
        }
    }
}
