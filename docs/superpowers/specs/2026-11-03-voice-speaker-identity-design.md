# Voice Speaker Identity Design

**Status:** Draft for product-owner review, 2026-11-03; revised 2026-11-05 after design review.

## Problem

The companion treats everyone in the room as one "user". It cannot tell Chris
from Jack, so it cannot greet a returning person, keep what each person said
about themselves apart, or notice when someone claims to be somebody else. A
person should be recognised by voice the way a friend would recognise them,
without having to re-introduce themselves every time.

## The behaviour (product-owner decisions)

These come from the product owner and are not open to the builder:

1. **Saying a name is enough.** When an unknown voice says "I'm Jack" (in
   English or Chinese), the runtime links that voice to Jack and keeps it. It
   does not ask anyone first and does not announce that it is keeping the voice.
2. **Returning people are recognised.** When Jack speaks again, in the same
   conversation or weeks later, the companion knows it is Jack. On his arrival
   it may simply say hi.
3. **False claims are caught.** If Jack later says "I'm Chris", the companion
   knows from his voice that he is not Chris and says so, with humour rather
   than as an accusation.
4. **It asks only when it needs to.** When the voice evidence is not strong
   enough to be sure, the companion checks instead of asserting.
5. **People are managed by whoever runs the runtime.** Listing, renaming,
   merging and deleting people happens through the deployment's configuration
   or the app, through the SDK. Someone in the room cannot rename or delete a
   person by asking the companion.

## Goals

- Live, per-utterance identification of known voices during a conversation.
- Enrolment of a new person from a spoken name claim, with no extra step.
- A claim-versus-voice check that produces one of: confirmed, denied, or
  unsure (ask).
- Every line the language model sees carries who said it.
- Facts people share about themselves are stored against that person.
- Owner-facing SDK operations to list, rename, merge and delete people, and a
  People screen in the desktop app that uses them. People are not declared in
  gateway configuration; the store is the single source of truth.
- Measured accuracy, with the rate of wrongly naming someone as the main metric.

## Non-goals

- Meeting-agent integration. The component must allow a later caller to
  restrict matching to a candidate set (for example meeting attendees), but
  wiring it into meeting-agent is a separate project.
- A replay detector. Audio from a TV or loudspeaker that says "I'm Jack" can
  enrol a person, and a recording of Chris is accepted as Chris; this is a
  documented limit of the first version.
- Separating overlapping speech. Identification is per utterance; an utterance
  with two people talking over each other may come out unsure.
- Changing the after-the-fact diarization sidecar
  (`platform/diarization-sidecar`). It keeps its anonymous-only guarantee.
- Recognising people who never speak, or by anything other than voice.
- Security-grade authentication. This recognises people socially; it must not
  be used to authorise anything (payments, unlocking, memory deletion).

## Anti-goals (failure even if it technically works)

- Calling someone by the wrong name with confidence. Wrongly naming is worse
  than saying "someone new" or asking. Chris with a cold must be asked, not told
  he is an impostor.
- Inventing a name for a voice that never gave one.
- Enrolling the companion's own synthesized voice as a person.
- Storing raw audio, or writing voice signatures, names or transcript content
  into logs or traces.
- A voice in the room being able to change the people records.

## Decisions on the how

**Product owner's:**

- Voice signatures persist across conversations and are kept until the owner
  deletes them.
- No consent prompt or announcement on enrolment (decision 1 above).
- **What a person tells the AI stays private to them.** Retrieval for a turn
  returns only memories whose subject is the identified speaker. When Jack is speaking, the companion never brings up what
  only Chris told it. On a turn whose speaker is "someone new" or unsure, no
  person's memories are retrieved, and memory extraction is skipped for that
  turn: nothing said on it is stored, with or without a subject.

**Designer's calls (the owner can override):**

- **The owner person.** Exactly one person is the owner. The owner marks their
  own voice once through the People screen or the SDK ("this is me"). The command
  names one specific recent voice turn, shown in the People screen with its time
  and text, and links that turn's signature to the owner person; it never means
  "whoever spoke last". A spoken claim can never
  create the owner, take over the owner's name, or link a voice to the owner; a
  stranger saying the owner's name goes through the normal claim check. The
  owner's display name is set through the People screen or the SDK, never from
  speech. Until the owner has marked their voice, owner memories are retrieved
  only for typed turns, and the owner's spoken turns are "someone new", so
  nothing the owner says aloud is stored until then.
- **Typed turns are the owner.** A typed message has no voice, so it counts as
  the owner speaking: it retrieves the owner's memories and its facts are stored
  against the owner.
- **Feature off means today's behaviour.** With speaker identity disabled,
  retrieval behaves as it does now, and every new memory is stored with the owner
  as its subject, the same rule the upgrade applies to existing memories. No path
  creates a memory without a subject, so turning the feature on later cannot
  expose anything to another person.

- **New opt-in component.** Build speaker identity as its own runtime
  component, enabled by explicit gateway configuration and off by default. It is
  separate from the diarization sidecar so that sidecar's "the sidecar cannot be
  asked 'is this Chris'" guarantee stays true.
- **Reversal of the current rule.** `docs/HUMAN-CHECKLIST.md` (section 5, "speaker
  enrollment and embeddings are not implemented or authorized") and
  `docs/architecture.md` ("Mapping a cluster onto a real participant is the
  Meeting Agent's work") are amended to say: the diarization path stays
  anonymous; the opt-in speaker identity component may name a speaker from an
  enrolled voice signature. The wire rule that diarization clusters are
  anonymous labels stays unchanged.
- **Local only.** Voice signatures are computed and matched on the Mac in every
  privacy mode, including the online profile. No voice audio or signature is
  sent to a provider for identification.
- **Storage.** Store one or more voice signatures per person (embedding, model
  identifier and version, creation time, source conversation id), never audio,
  encrypted at rest with its own key, supplied the same way as the recording key
  (an environment variable named in configuration) but independent of the
  `[recording]` section, so the feature works without recording and outside a
  voice session. A missing or wrong key disables identification at startup
  rather than starting with an empty store. Signatures from different models are
  never compared.
- **Enrolment matures over the conversation.** One "I'm Jack" is about a second
  of speech, too little for a reliable signature. Enrol provisionally from the
  claim utterance, then add Jack's following utterances in the same conversation
  when they match the provisional signature well. During the provisional phase
  the claim utterance is a fixed anchor: a turn is added only if it matches the
  anchor above a stricter threshold than normal matching and matches no other
  known person better. Only single-segment turns of at
  least a minimum duration (measured; at least 1.5 s) are added, because a turn
  that joins adjacent segments can contain two speakers. A "conversation" here is
  one voice session. Until a minimum amount of
  speech has been collected (the builder measures the amount; the reference
  project silverstein/minutes requires at least 5 s of speech for enrolment), a
  provisional person can be greeted by name but cannot be the basis for denying
  someone else's claim.
- **Refinement is bounded.** Later confident matches may refine a signature, with
  a cap on added samples per person. The enrolment samples that made a person
  mature are kept unchanged and are always part of the signature, so added
  samples cannot drift it into another person's voice. The same single-segment
  and minimum-duration rule applies.
- **Outcomes for a claim.** For an utterance where someone claims a name:
  - unknown voice, name not taken: enrol under that name;
  - voice matches the claimed person: confirmed;
  - voice confidently matches a different known, mature person, and the claimed
    name belongs to another existing person: denied (Jack claiming to be Chris);
  - voice confidently matches a known, mature person, and the claimed name is not
    taken: possible alias (Chris saying "我是昊辰"). The companion asks; it is
    never a denial. A confirmed alias is added as another name for that person;
  - anything else, including a voice that matches nobody claiming a known name:
    unsure, and the companion asks. A weak score against the claimed person alone
    never denies, because that is exactly what Chris with a cold looks like.
  On a denied turn the speaker is the person the voice matched (Jack), and only
  that person's memories are retrieved.
  The thresholds are set from measurement and lean towards unsure. A real
  second person with the same name is asked rather than denied unless their
  voice matches someone else; the owner resolves duplicates in the app.
- **Provisional people across conversations.** A person whose signature never
  reached the minimum speech stays stored, but in a later conversation a match to
  them counts as unsure: the companion asks ("Jack, is that you?") and a yes
  matures the signature. Only mature people are greeted outright.
- **Identity questions are runtime state.** Whenever the companion is told to
  ask (unsure, alias, provisional match), the runtime records one pending identity
  question for that voice session: the voice signature of the asked turn and the
  name in question. The next user turn in the session is checked for a yes or no
  in Chinese and English, unless that turn confidently matches a different known
  person; answer turns ("是", "yes") are usually too short to match by voice.
  Anything other than a yes or no on that turn clears the question with no
  change. A yes:
  - confirms a provisional person for the rest of the session and lets sample
    collection continue; it does not by itself make the person mature. Maturity
    is reached only by the measured minimum of collected speech;
  - confirms an alias for a mature person;
  - never adds samples to an already mature person. If the name belongs to a
    mature person whose voice did not match, a yes creates a second person with
    the same display name, flagged in the People screen for the owner to merge or
    rename;
  - if the name is the owner's, always creates such a flagged duplicate, whatever
    the state of the owner's voice. A yes can never link a voice to the owner.
- **Aliases need a mature person.** A provisional person who claims a second name
  is treated as unsure, and the companion asks only about the first name.
- **Introductions by someone else** ("this is my friend Jack") do not enrol
  anyone in the first version; Jack is enrolled when he names himself.
- **Unclaimed unknown voices** are "someone new" for that conversation. They are
  never given a guessed name, and they are not stored after the conversation
  unless they claim a name.
- **The companion's own voice and barge-in.** The echo canceller gives no
  per-utterance signal today. The voice sidecar adds a per-final field saying
  whether the companion's playback was active at any point during the turn; on
  the media-bridge path the gateway computes the same flag from its own playback
  state.
  Such turns, and barge-in turns, are still identified but are never used to
  enrol or refine a signature.
- **What the model sees.** Each user message in the conversation context carries
  a speaker label: a person's display name, or "someone new", plus whether the
  identification was confirmed or unsure. A claim outcome (confirmed, denied,
  unsure) is given to the model as runtime guidance for that turn, so the
  companion's persona produces the greeting, the playful denial or the check in
  its own words. The runtime never scripts the words.
- **Memory.** Memory records gain an optional subject person. Extraction
  attributes a fact to the person who said it about themselves; the speaker label
  is passed with the exchange to the extractor so a late label cannot attach to
  the wrong turn. Extraction stays single-flight: an exchange dropped because an
  earlier extraction is still running is simply not stored. Every memory stored
  before this feature belongs to the owner person. The existing approval rules
  for identity and relationship memories stay as they are; only the voice
  signature store saves without asking.
- **Memory store upgrade.** The store today refuses any schema version other than
  its own and has a single migration. This work adds a real migration path: schema
  version 2 with the subject column, an in-place upgrade of existing version 1
  databases that sets every existing record's subject to the owner person, and
  tests for the upgrade and for refusing an unknown future version.
- **Deletion.** Deleting a person deletes their voice signatures and every memory
  whose subject is that person. Merging two people moves the signatures and
  memories of one onto the other.
- **Model choice by measurement.** Compare at least the SpeechBrain ECAPA model
  already used by the diarization sidecar and the 3D-Speaker ERes2Net model the
  product owner tested on 2026-10-23, and pick on measured accuracy and latency
  on Apple Silicon.
- **Where the voice signature is computed.** The voice sidecar forwards each
  final turn's audio (16 kHz mono, at most the last 10 s of the turn) to the
  gateway over a new version of the private sidecar protocol, and the gateway
  computes signatures in Rust. The media-bridge path already gives the gateway
  per-utterance audio, so both paths share one implementation and one model
  runtime. When a media-bridge utterance has no capture range, the turn is
  "unsure".
- **Latency budget.** Retrieval needs the speaker before the response starts, so
  identification sits directly on the path to the reply. The wait is a bounded
  configuration value (default from measurement, target well under 2 s from the
  end of the utterance). If identification is late, the turn proceeds as unsure
  and the runtime emits a content-free "identity late" signal; the late-label
  rate is part of acceptance.
- **Name-claim detection.** A cheap pattern check first (我是, 我叫, 我的名字是,
  I'm, I am, my name is, call me); only a hit is passed to a structured-output
  language-model call that returns the claimed name or nothing. Names are
  compared in a normalised form (pinyin for Chinese, case-folded for Latin), so a
  name recognised with different characters still matches the same person.

## Constraints from the existing system

- Traces and logs are content-free: no names, transcript text, signatures or
  audio. Follow the existing content-free trace patterns.
- Protocol changes follow the existing version rules (default protocol 3,
  opt-in 4; earlier frames never change). New events and the people-management
  commands are added only where those rules allow, with matching TypeScript SDK
  types and tests.
- **Gateway and SDK ship together.** The SDK's voice-event decoder rejects any
  event type it does not know (`packages/typescript/src/protocol.ts`), so a
  client pinned to an older SDK fails on the first new voice event. Every
  consumer, including Meeting Agent, repins the gateway and the SDK tarball
  together from one Runtime revision.
- Gateway configuration stays strict (unknown keys rejected, bounded values,
  schema versioning as in earlier bumps).
- The voice loop's existing guarantees hold: one terminal event per turn,
  cancellation reaching generation and playback, barge-in behaviour unchanged.
- The language model is told who is speaking through the existing context path
  (`ConversationTurnSource` / `begin_turn` in
  `crates/runtime/src/conversation_context.rs`, into `ConversationMessage` and
  `LanguageModelInput`). Today `ConversationRole`
  (`crates/protocol/src/quality.rs`) is only User or Assistant.
- **Both audio paths must work.** The companion's everyday setup runs local
  recognition inside the managed voice sidecar, and in that setup the gateway
  never receives microphone audio (`apps/runtime-gateway/src/config.rs`, "local
  ASR listens on the voice sidecar's microphone"; `RecognitionHypothesis` carries
  no audio or timing). The media-bridge path (online recognition, network clients) does give
  the gateway PCM with per-utterance sample ranges. Identification must work on
  both; for the sidecar path that means either computing signatures in the
  sidecar or forwarding each utterance's audio, with a versioned sidecar
  protocol change either way.

## Evidence the product owner already has

From `Estidama/Product/Meeting Agent/Speaker Identification by Voice.md`
(2026-10-23): on a 37.5-minute, three-speaker, mixed Chinese and English phone
recording, ERes2Net signatures from 3 to 7 clips per person separated one
speaker cleanly (0.84 own, about 0.2 others), while the other two were close
(0.7 to 0.86 own, 0.4 to 0.6 other). Of 582 lines, 210 matched with a margin
under 0.1 and 74 were too short to match. Expect the unsure outcome to be
common with similar voices, and design the asking to stay pleasant when it is.

A second check on 2026-11-04 used a 40.5-minute, two-speaker Chinese 1-on-1
recorded on the Mac's microphone. Voice signatures were built from the first 10
minutes (about 60 s of speech per person) and matched against the remaining 734
segments. ERes2Net: signature-to-signature similarity 0.11, median 0.58 own
against 0.08 other, median margin 0.49, 27 segments (3.7%) under a 0.1 margin.
CAM++: 0.15, 0.61 against 0.11, median margin 0.50, 23 segments under 0.1. The
two models agreed on 97.7% of segment labels; 189 of 1,156 segments were under
1 s and not matched. Two distinct voices separate easily; similar voices, as on
2026-10-23, are the hard case.

## Acceptance

- A deterministic test suite with synthetic or fixture embeddings covers every
  claim outcome including alias, enrolment maturation (a yes alone never
  matures), the owner duplicate rule, refinement caps and
  immutable enrolment samples, the pending identity question (a yes never touches
  a mature person), the owner rules (no voice claim reaches the owner; typed turns
  are the owner), no storage on unsure or someone-new turns, no enrolment from
  playback-overlapped or barge-in turns, the memory store upgrade, deletion and
  merge, content-free traces, and the "voice in the room cannot change people"
  rule.
- A measured evaluation on hand-labelled recordings (at minimum the product
  owner's 2026-10-23 recording if made available, plus a companion-style
  multi-person session recorded through the live voice-sidecar path) reports,
  per model: wrong-name rate, unsure rate, correct-name rate, identification
  latency and late-label rate on the live path, false-enrolment rate (voices saved
  from utterances that were not a name claim), name-claim detection precision
  and recall in Chinese and English, and the share of identity questions that
  resolve to a yes or no. The product owner sets the
  pass thresholds from that report; the builder proposes values.
- A native human check: Chris enrols by saying his name, Jack enrols, Jack is
  greeted on return in a new conversation, Jack's claim to be Chris is denied,
  and Chris speaking with a changed voice is asked rather than denied.

## Open risks

- **Biometric data.** Voice signatures are biometric data. The product owner's
  2026-10-23 draft called for a legal check on consent and retention before
  storing them; the product owner has decided to save without asking. That
  check remains open and is the deployer's responsibility.
- **Spoofing.** A recording of Chris played to the companion will be accepted as
  Chris. This is acceptable only because the feature authorises nothing.

## Left to the builder

- The embedding model, after measurement, and the Rust model runtime for it.
- The exact claim patterns and the yes/no wording handled, within the approach
  above.
- Thresholds, minimum enrolment speech, sample caps and the matching method,
  from measurement.
- Exact protocol shapes, version placement and SDK method names, within the
  constraints above.
- The voice-signature storage schema and its place next to the memory store.
- Whether the desktop app already speaks protocol 4; if not, moving it to 4 is
  part of the People screen work.
