# Speaker Identity, Plan 2: Identity Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Decide, for every voice turn, who is speaking, what a name claim means,
what to ask, and what to store, from voice signatures and a claimed name, with
no model, audio path or gateway involved.

**Architecture:** Four modules in `crates/runtime`. `speaker_names` turns names
into comparison keys and reads claim cues and yes/no answers.
`speaker_matching` scores an embedding against everyone's signatures.
`speaker_claims` is a pure decision table for name claims.
`speaker_identity` is the per-voice-session engine: it holds the pending
question and enrolment state and applies its decisions to the plan 1 store
(`PeopleStore`) synchronously, so the store assigns person ids. The engine
never logs. Plan 4 feeds it real embeddings, claimed names and turn flags, and
passes its output to retrieval, extraction and the language model.

**Tech Stack:** Rust; `conversation-memory` (plan 1); `pinyin` 0.11 (MIT,
mozillazg/rust-pinyin, no dependencies of its own) with only the `plain`
feature.

**Spec:** `docs/superpowers/specs/2026-11-03-voice-speaker-identity-design.md`
**Builds on:** `docs/superpowers/plans/2026-11-05-speaker-identity-1-people-store.md` (merged, `ac79701`)

## Global Constraints

- Wrongly naming is worse than saying "someone new" or asking; every threshold
  leans towards unsure.
- A weak score against the claimed person alone never denies.
- A provisional person can be greeted by name but cannot be the basis for
  denying someone else's claim.
- A spoken claim can never create the owner, take over the owner's name, or
  link a voice to the owner. A yes can never link a voice to the owner.
- A yes never adds samples to an already mature person, and a yes alone never
  matures anyone; maturity comes only from collected speech.
- Only single-segment turns of at least a minimum duration (at least 1.5 s)
  are added as samples; turns with companion playback or barge-in are never
  used to enrol or refine.
- On a turn whose speaker is "someone new" or unsure, no person's memories are
  retrieved and nothing is stored.
- Nothing said in a conversation can rename, merge or delete a person: the
  engine only creates people, adds signatures, adds aliases and marks maturity.
- Thresholds, minimum enrolment speech and the refinement cap come from
  measurement (plan 3): `IdentityPolicy` has no defaults.
- No logging or tracing in these modules; names and embeddings never leave
  through errors (`&'static str` messages only).
- Gates: `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets --locked -- -D warnings`,
  `cargo test --workspace --locked`.

## Decisions this plan makes (the spec leaves them to the builder)

- **Matching method:** cosine similarity against each person's centroid (mean
  of L2-normalised samples, renormalised). Evidence is `Confident` when the best
  score is at least `confident_score` and beats the runner-up by `margin`,
  `Possible` when it is at least `possible_score`, otherwise `Unknown`.
- **A no-claim turn that weakly matches someone** is labelled unsure with that
  candidate and asks "is that you?" at most once per candidate per session.
- **What a yes does**, by question:
  - "Is that X?" about a provisional, non-owner X: X is confirmed for the session
    and sample collection continues.
  - "Is that X?" about a mature person or the owner: creates a second person
    with X's display name, flagged for review, anchored on the asked turn.
  - "Is that also your name?" (alias): adds the alias.
  - "Are you N?" (a claim that could not be confirmed): creates N, flagged for
    review when the name is already taken.
- **Questions are raised only from turns that can enrol** (no playback, no
  barge-in), so every yes that creates a person has an anchor.
- **A claim on a playback-overlapped or barge-in turn is ignored**, so the
  companion's own voice saying a name can never enrol it.
- **Maturity counts every stored sample's speech**, the anchor included, across
  sessions.

## Review Focus

1. **The companion's own voice says "I'm Serena" during playback.** Nothing is
   enrolled and nothing is asked. Test: `playback_overlapped_claim_enrols_nobody`
   in Task 4.
2. **A stranger answers "yes" to a question about the owner.** A flagged
   duplicate is created; the owner's signatures and memories are untouched, and
   the stranger never gets `memory_subject == OWNER`. Test:
   `yes_about_the_owner_creates_a_flagged_duplicate` in Task 4.
3. **The answer turn is a different known person talking.** The question is
   cleared without effect and the turn is identified normally. Test:
   `answer_from_a_different_known_voice_is_not_an_answer` in Task 4.
4. **"Hi Amy" contains the letters "i am".** No claim cue. Test:
   `english_cues_need_word_boundaries` in Task 1.
5. **A turn embedding with a different dimension from the stored signatures.**
   The turn is unsure with no storage, not a panic or a match. Test:
   `embedding_of_another_dimension_is_unsure` in Task 4.

---

### Task 1: Name keys, claim cues and yes/no answers

**Files:**
- Modify: `Cargo.toml` (workspace dependency `pinyin = { version = "0.11", default-features = false, features = ["plain"] }`, with a comment saying why), `crates/runtime/Cargo.toml`, `Cargo.lock`
- Create: `crates/runtime/src/speaker_names.rs`
- Modify: `crates/runtime/src/lib.rs` (module and exports)
- Test: `crates/runtime/tests/speaker_names.rs`

**Interfaces:**
- Produces:
  - `pub fn name_key(name: &str) -> Option<String>`: each Chinese character
    becomes its plain pinyin, each other alphanumeric character is lowercased,
    everything else (spaces, punctuation, apostrophes) is dropped; `None` when
    nothing is left.
  - `pub fn has_name_claim_cue(transcript: &str) -> bool`: true when the text
    contains 我是, 我叫 or 我的名字是, or one of the English word sequences
    "i'm", "i am", "my name is", "call me" (whole words, case-insensitive, ’
    read as '). "im" without an apostrophe is not a cue.
  - `pub enum IdentityAnswer { Yes, No }` and
    `pub fn identity_answer(transcript: &str) -> Option<IdentityAnswer>`: the
    whole utterance, trimmed of whitespace and `。，！？.,!?~` and lowercased (’
    as '), must equal one entry. Yes: 是, 是的, 是我, 对, 对的, 对啊, 嗯, 嗯嗯,
    没错, yes, yeah, yep, yup, correct, right, that's me, it's me. No: 不是,
    不对, 不是我, 没有, no, nope, not me.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn chinese_names_with_different_characters_share_a_key() {
    assert_eq!(name_key("昊辰"), Some("haochen".to_owned()));
    assert_eq!(name_key("浩晨"), Some("haochen".to_owned()));
}

#[test]
fn latin_names_are_case_folded_and_stripped() {
    assert_eq!(name_key("Jack"), Some("jack".to_owned()));
    assert_eq!(name_key(" JACK "), Some("jack".to_owned()));
    assert_eq!(name_key("O'Brien"), Some("obrien".to_owned()));
    assert_eq!(name_key("陈 Chris"), Some("chenchris".to_owned()));
    assert_eq!(name_key(" ,.! "), None);
}

#[test]
fn claim_cues_in_both_languages() {
    for text in ["我是Jack", "你好，我叫昊辰", "我的名字是小明", "I'm Jack", "i am jack",
                 "Hi, my name is Jack", "Call me Jack", "I’m Jack"] {
        assert!(has_name_claim_cue(text), "{text}");
    }
    assert!(!has_name_claim_cue("what's the weather"));
}

#[test]
fn english_cues_need_word_boundaries() {
    for text in ["hi amy", "Miami is warm", "recall meetings", "xi am"] {
        assert!(!has_name_claim_cue(text), "{text}");
    }
}

#[test]
fn yes_and_no_answers_in_both_languages() {
    for text in ["是", "是的。", "对啊！", "嗯", "Yes.", "yeah", "That’s me"] {
        assert_eq!(identity_answer(text), Some(IdentityAnswer::Yes), "{text}");
    }
    for text in ["不是", "不对", "No!", "nope", "not me"] {
        assert_eq!(identity_answer(text), Some(IdentityAnswer::No), "{text}");
    }
}

#[test]
fn anything_else_is_not_an_answer() {
    for text in ["是不是", "yes and no", "I think so", "", "我是Jack"] {
        assert_eq!(identity_answer(text), None, "{text}");
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-runtime --test speaker_names --locked`
Expected: FAIL to compile (`name_key` not found). Run `cargo check -p conversation-runtime` once without `--locked` first: it adds `pinyin` to `Cargo.lock` (network); every later command keeps `--locked`.

- [ ] **Step 3: Implement `crates/runtime/src/speaker_names.rs`**

Use `pinyin::ToPinyin` per character (`'昊'.to_pinyin()` returns
`Option<Pinyin>`; `.plain()` is toneless). For English cues, split the
lowercased text into words on anything that is not a letter or apostrophe and
look for the token sequences `["i'm"]`, `["i", "am"]`, `["my", "name", "is"]`,
`["call", "me"]`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p conversation-runtime --test speaker_names --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/runtime
git commit -m "feat(runtime): name keys, claim cues and yes/no answers for speaker identity"
```

---

### Task 2: Voice matching

**Files:**
- Create: `crates/runtime/src/speaker_matching.rs`
- Modify: `crates/runtime/src/lib.rs`
- Test: `crates/runtime/tests/speaker_matching.rs`

**Interfaces:**
- Consumes: `conversation_memory::{VoiceSignature, SignatureRole}`, `conversation_protocol::PersonId`.
- Produces:
  - `pub struct IdentityPolicy { pub confident_score: f32, pub possible_score: f32, pub margin: f32, pub anchor_score: f32, pub min_sample_ms: u32, pub maturity_speech_ms: u32, pub refinement_cap: usize }`
    and `IdentityPolicy::validate(&self) -> Result<(), RuntimeError>`:
    `0 < possible_score <= confident_score <= anchor_score <= 1`,
    `0 <= margin <= 1`, `min_sample_ms >= 1_500`,
    `maturity_speech_ms >= min_sample_ms`,
    `refinement_cap <= MAX_SIGNATURES_PER_PERSON`; a configuration error otherwise.
  - `pub struct VoiceDirectory` with
    `VoiceDirectory::new(signatures: &[VoiceSignature]) -> Result<Self, RuntimeError>`
    (all signatures must share one dimension, else a configuration error),
    `dimension(&self) -> Option<usize>`,
    `scores(&self, embedding: &[f32]) -> Option<Vec<(PersonId, f32)>>` (`None`
    on a dimension mismatch or an empty directory; sorted best first),
    `refinements(&self, person: PersonId) -> usize`,
    `speech_ms(&self, person: PersonId) -> u64` (sum over stored samples),
    `anchor(&self, person: PersonId) -> Option<&[f32]>`.
  - `pub fn cosine(left: &[f32], right: &[f32]) -> f32` (0 when either is all zero).
  - `pub enum VoiceEvidence { Confident { person: PersonId, score: f32 }, Possible { person: PersonId, score: f32 }, Unknown }`
    and `pub fn classify(scores: &[(PersonId, f32)], policy: &IdentityPolicy) -> VoiceEvidence`.

- [ ] **Step 1: Write the failing tests**

Build signatures through a real `SqliteMemoryStore` in a `tempdir()` (plan 1
API), with 4-dimensional embeddings: Chris `[1,0,0,0]`, Jack `[0,1,0,0]`.
Test policy: `confident 0.80, possible 0.60, margin 0.10, anchor 0.85,
min_sample_ms 1_500, maturity_speech_ms 5_000, refinement_cap 3`.

```rust
#[test]
fn best_match_is_confident_when_it_clears_score_and_margin() {
    let scores = directory.scores(&[0.95, 0.05, 0.0, 0.0]).unwrap();
    assert_eq!(scores[0].0, chris);
    assert!(matches!(classify(&scores, &policy()), VoiceEvidence::Confident { person, .. } if person == chris));
}

#[test]
fn a_close_runner_up_makes_the_match_possible_not_confident() {
    let scores = directory.scores(&[0.7, 0.68, 0.0, 0.2]).unwrap();
    assert!(matches!(classify(&scores, &policy()), VoiceEvidence::Possible { .. }));
}

#[test]
fn a_low_score_is_unknown() {
    let scores = directory.scores(&[0.0, 0.0, 1.0, 0.0]).unwrap();
    assert_eq!(classify(&scores, &policy()), VoiceEvidence::Unknown);
}

#[test]
fn centroid_uses_every_sample_of_a_person() {
    // Jack samples [0,1,0,0] and [0,0.8,0.6,0]; query [0,0.9,0.3,0] scores higher against
    // Jack's centroid than against either sample alone would suggest: assert score > 0.95
}

#[test]
fn mixed_dimensions_are_refused_and_mismatched_queries_score_nothing() {
    // a 4-dim and a 3-dim signature under one model → VoiceDirectory::new is Err
    // a 3-dim query against a 4-dim directory → scores() is None
}

#[test]
fn directory_reports_refinements_speech_and_anchor() {}

#[test]
fn policy_validation_rejects_inverted_or_short_values() {
    // possible > confident, anchor < confident, min_sample_ms 1_499, maturity < min_sample, cap 65 → Err
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-runtime --test speaker_matching --locked`
Expected: FAIL to compile.

- [ ] **Step 3: Implement `crates/runtime/src/speaker_matching.rs`**

Centroids are computed once in `VoiceDirectory::new` (normalise each sample,
average, normalise). Ties in score sort by person id so results are
deterministic. A single known person has no runner-up: the margin is measured
against 0.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p conversation-runtime --test speaker_matching --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/runtime
git commit -m "feat(runtime): match voice embeddings against stored signatures"
```

---

### Task 3: Claim decisions

**Files:**
- Create: `crates/runtime/src/speaker_claims.rs`
- Modify: `crates/runtime/src/lib.rs`
- Test: `crates/runtime/tests/speaker_claims.rs`

**Interfaces:**
- Consumes: `VoiceEvidence` (Task 2), `conversation_memory::PersonMaturity`.
- Produces:
  - `pub struct ClaimHolder { pub person: PersonId, pub mature: bool, pub owner: bool }`: someone who already has the claimed name key.
  - `pub enum ClaimDecision { Enrol, Confirmed(PersonId), Denied { speaker: PersonId }, PossibleAlias { person: PersonId }, AskIsThisPerson { person: PersonId }, AskIsNamed }`
  - `pub fn decide_claim(evidence: &VoiceEvidence, matched_is_mature: bool, holders: &[ClaimHolder]) -> ClaimDecision`, where `matched_is_mature` describes the person in a `Confident`/`Possible` evidence and is ignored for `Unknown`.

The table, first matching row wins:

| Evidence | Holders | Decision |
|---|---|---|
| Confident(p), p holds the name | any | `Confirmed(p)` |
| Confident(p), p mature | non-empty | `Denied { speaker: p }` |
| Confident(p), p mature | empty | `PossibleAlias { person: p }` |
| Confident(p), p provisional | any | `AskIsThisPerson { person: p }` (asks only about the first name) |
| Unknown | empty | `Enrol` |
| Possible or Unknown | every holder provisional and not owner | `AskIsThisPerson { person }`: the matched holder if the evidence names one, else the lowest id |
| anything else | | `AskIsNamed` |

- [ ] **Step 1: Write the failing tests**

One test per row, plus the rules that guard people:

```rust
#[test]
fn unknown_voice_with_a_free_name_enrols() {
    assert_eq!(decide_claim(&VoiceEvidence::Unknown, false, &[]), ClaimDecision::Enrol);
}

#[test]
fn voice_matching_the_claimed_person_confirms() {}

#[test]
fn a_mature_voice_claiming_someone_elses_name_is_denied() {
    // Jack (mature) says "I'm Chris": Denied { speaker: jack }
}

#[test]
fn a_mature_voice_claiming_a_free_name_is_a_possible_alias() {}

#[test]
fn a_provisional_voice_never_denies_and_is_asked_about_its_first_name() {
    // Confident(provisional p) with mature holder Chris → AskIsThisPerson { person: p }
}

#[test]
fn a_weak_match_to_the_claimed_person_never_denies() {
    // Possible(chris) claiming "Chris" (Chris mature) → AskIsNamed (Chris with a cold)
}

#[test]
fn a_stranger_claiming_the_owners_name_is_asked_never_confirmed() {
    // Unknown, holder = owner → AskIsNamed; Possible(owner), holder = owner → AskIsNamed
}

#[test]
fn an_unknown_voice_claiming_a_provisional_name_is_asked_about_that_person() {}

#[test]
fn a_possible_match_with_a_free_name_is_asked_not_enrolled() {
    // Possible(jack), holders empty → AskIsNamed
}

#[test]
fn no_decision_names_the_owner_from_an_unconfirmed_voice() {
    // for every evidence that is not Confident(owner) and every holder set containing the
    // owner: decision is never Confirmed(owner), Denied{speaker: owner}, PossibleAlias{owner}
    // or AskIsThisPerson{owner}
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-runtime --test speaker_claims --locked`
Expected: FAIL to compile.

- [ ] **Step 3: Implement `decide_claim` in `crates/runtime/src/speaker_claims.rs`** as the table above.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p conversation-runtime --test speaker_claims --locked`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/runtime
git commit -m "feat(runtime): decide what a spoken name claim means"
```

---

### Task 4: The per-session identity engine

**Files:**
- Create: `crates/runtime/src/speaker_identity.rs`
- Modify: `crates/runtime/src/lib.rs`
- Test: `crates/runtime/tests/speaker_identity.rs`

**Interfaces:**
- Consumes: Tasks 1–3; `conversation_memory::{PeopleStore, SignatureKey, SignatureModel, SignatureRole, VoiceSignatureDraft, NewPerson, PersonName, PersonMaturity}`.
- Produces:
  - `pub struct VoiceTurn { pub transcript: String, pub claimed_name: Option<String>, pub embedding: Option<Vec<f32>>, pub speech_ms: u32, pub single_segment: bool, pub playback_overlap: bool, pub barge_in: bool, pub at: UnixTimestampMillis }`
    (`claimed_name` is the self-claimed name plan 4 extracts; `embedding` is
    `None` when identification was late or had no audio).
  - `pub enum Speaker { Person { person: PersonId, name: Option<String> }, SomeoneNew }`
  - `pub struct SpeakerLabel { pub speaker: Speaker, pub confirmed: bool }`
  - `pub enum ClaimOutcome { Enrolled, Confirmed, Denied { claimed_name: String }, Unsure }`: the guidance for the model on a claim turn.
  - `pub enum IdentityQuestion { IsThisPerson { person: PersonId, name: String }, IsNewAlias { person: PersonId, name: String }, IsNamed { name: String } }`
  - `pub struct TurnIdentity { pub label: SpeakerLabel, pub claim: Option<ClaimOutcome>, pub question: Option<IdentityQuestion>, pub memory_subject: Option<PersonId> }`
    (`memory_subject` is `Some` only when `label.confirmed` and the speaker is a person).
  - `pub struct SpeakerIdentitySession` with
    `start(store: Arc<dyn PeopleStore>, key: Arc<SignatureKey>, model: SignatureModel, policy: IdentityPolicy, session: SessionId) -> MemoryStoreResult<Self>`
    (validates the policy, loads people and the model's signatures) and
    `observe(&mut self, turn: VoiceTurn) -> MemoryStoreResult<TurnIdentity>`.

`observe` runs these steps in order:

1. **Pending question.** If one is pending, take it. If the turn's evidence is
   `Confident` for a person other than the question's person, discard the
   question and go on to step 2. Otherwise read `identity_answer`: on `Yes`
   apply the yes (see "Decisions this plan makes"), label the person concerned
   confirmed and return; on `No` return someone new, unsure; on anything else
   discard the question and go on.
2. **No embedding, or a dimension mismatch:** someone new, unsure; return.
3. **Claim** (when `claimed_name` has a `name_key`): ignore it entirely when the
   turn has playback or barge-in. Otherwise build `ClaimHolder`s from
   `people_named(key)` and apply `decide_claim`:
   `Enrol` → `create_person` and an `Anchor` signature, start collecting;
   `Confirmed(p)` → label p confirmed and confirm p for the session;
   `Denied` → label the matched speaker confirmed;
   `PossibleAlias` → label p confirmed, ask `IsNewAlias`;
   `AskIsThisPerson`/`AskIsNamed` → label unsure, ask.
4. **No claim:** `Confident(p)` with p mature or confirmed this session → p
   confirmed; `Confident(p)` provisional, or `Possible(p)` → p unsure and ask
   `IsThisPerson` once per person per session (never when p has no display
   name); `Unknown` → someone new, unsure.
5. **Samples**, only for an eligible turn (single segment, at least
   `min_sample_ms`, no playback, no barge-in) whose speaker is confirmed:
   - while p is collecting: add an `Enrolment` sample when the turn scores at
     least `anchor_score` against p's anchor and p is the best match; mark p
     mature once the stored speech reaches `maturity_speech_ms`;
   - for a mature p: add a `Refinement` sample while p holds fewer than
     `refinement_cap`.
6. Reload the directory after any store change. Questions are raised only
   from eligible turns (no playback, no barge-in); a new question replaces none
   while one is pending, because step 1 always clears it first.

- [ ] **Step 1: Write the failing tests**

Real `SqliteMemoryStore` in a `tempdir()`, the Task 2 test policy, 4-dim
embeddings, a helper `turn(text, claim, embedding, speech_ms)` with an eligible
single-segment default.

```rust
#[test]
fn saying_a_name_enrols_a_provisional_person_with_an_anchor() {
    let identity = session.observe(turn("I'm Jack", Some("Jack"), JACK, 1_000)).unwrap();
    assert_eq!(identity.claim, Some(ClaimOutcome::Enrolled));
    let jack = store.people_named("jack").unwrap()[0];
    assert_eq!(store.person(jack).unwrap().maturity(), PersonMaturity::Provisional);
    assert_eq!(identity.memory_subject, Some(jack));
    // one Anchor signature with speech_ms 1_000
}

#[test]
fn following_turns_mature_the_person_from_collected_speech() {
    // anchor 1_000 ms, then two eligible 2_000 ms turns near JACK → mature after the second
}

#[test]
fn short_multi_segment_or_off_anchor_turns_are_not_collected() {
    // 1_400 ms; single_segment false; embedding scoring below anchor_score → no Enrolment rows
}

#[test]
fn a_yes_alone_never_matures() {
    // later session: provisional Jack matched → IsThisPerson; "是" → confirmed, still Provisional
}

#[test]
fn returning_mature_person_is_greeted_without_asking() {}

#[test]
fn a_false_claim_is_denied_and_the_real_speaker_gets_the_memories() {
    // Jack mature, Chris mature; JACK says "I'm Chris" → Denied { claimed_name: "Chris" },
    // label Jack confirmed, memory_subject Some(jack)
}

#[test]
fn a_changed_voice_is_asked_not_denied() {
    // Possible(chris) says "I'm Chris" → claim Unsure, question IsNamed { name: "Chris" }
}

#[test]
fn an_alias_is_asked_and_a_yes_adds_it() {}

#[test]
fn yes_about_the_owner_creates_a_flagged_duplicate() {
    // owner renamed "Chris" with signatures; unknown voice "I'm Chris" → IsNamed; "yes" →
    // a second "Chris" with needs_review, owner signatures unchanged,
    // memory_subject never Some(PersonId::OWNER)
}

#[test]
fn yes_never_adds_samples_to_a_mature_person() {}

#[test]
fn a_no_clears_the_question_without_change() {}

#[test]
fn answer_from_a_different_known_voice_is_not_an_answer() {
    // question about Jack pending; CHRIS (confident) says "yes" → no change, label Chris
}

#[test]
fn an_unrelated_reply_clears_the_question() {}

#[test]
fn the_same_person_is_asked_about_once_per_session() {}

#[test]
fn someone_new_and_unsure_turns_store_nothing_and_retrieve_nothing() {
    // unknown voice, no claim; possible match; no embedding → memory_subject None,
    // people and signature counts unchanged
}

#[test]
fn playback_overlapped_claim_enrols_nobody() {}

#[test]
fn barge_in_turns_are_identified_but_never_sampled() {}

#[test]
fn refinement_stops_at_the_cap() {
    // mature Jack, four eligible confident turns, cap 3 → exactly 3 Refinement rows
}

#[test]
fn embedding_of_another_dimension_is_unsure() {}

#[test]
fn nothing_said_renames_merges_or_deletes_anyone() {
    // script of claims, answers and aliases; afterwards every pre-existing person still exists
    // with its original primary name, and no memory was deleted
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p conversation-runtime --test speaker_identity --locked`
Expected: FAIL to compile.

- [ ] **Step 3: Implement `SpeakerIdentitySession` in `crates/runtime/src/speaker_identity.rs`**

Session state: the directory, the policy, the pending question (with the asked
turn's embedding and speech), the people confirmed this session, the people
already asked about, and the people collecting enrolment speech. A
`LimitExceeded` from the store (people, names or signatures full) leaves the
turn's label as decided and skips the storage step; every other store error is
returned.

- [ ] **Step 4: Run all gates**

Run: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/runtime
git commit -m "feat(runtime): per-session speaker identity engine"
```
