# R4 Conversation Quality Controls Design

## Outcome

Add a backend-neutral, inspectable conversation-quality layer that turns saved
persona, bounded recent context, and current user signals into deterministic
per-turn controls. The layer must make short answers, corrections, silence, and
mode changes reliable without reducing relationship behavior to scripts.

## User Problem

The current voice loop sends only the latest transcript to the language model.
It cannot remember that the user asked for a shorter answer, distinguish a
rejected question from a new request, expose why a response style was chosen,
or let rapport emerge from shared context. Provider prompts currently own all
of that behavior invisibly.

## Approaches Considered

### 1. Add one larger system prompt

Rejected. It is opaque, provider-bound, difficult to test, and cannot preserve
temporary corrections or bounded recent history.

### 2. Add provider-specific chat history in the Ollama adapter

Rejected. It leaks product behavior into one model backend and prevents a
second provider from receiving equivalent context.

### 3. Typed runtime controller with provider translation

Selected. Public protocol types describe persona, modes, signals, response
controls, messages, and content-free decisions. The runtime owns bounded
session state and resolves each turn. Adapters only translate the typed turn
envelope into provider-native requests.

## Public Types

Add `crates/protocol/src/quality.rs` with:

- `PersonaLevel(u8)`, validated in `0..=100`;
- `PersonaProfile` containing warmth, humor, teasing, initiative, directness,
  intimacy, verbosity, and follow-up frequency;
- `ConversationMode::{DirectAnswer, Companionship, Brainstorming, Reflective}`;
- `SpeechPace::{Measured, Natural, Brisk}`;
- `FollowUpPolicy::{Never, Contextual, Allowed}`;
- `SilencePolicy::AllowWithoutFiller`;
- `ResponseControls` containing maximum spoken seconds, directness, pace,
  follow-up policy, and silence policy;
- `ConversationSignal::{Interrupted, ShorterRequested, StopExplaining,
  QuestionRejected, Hesitation, RapidTopicChange}`;
- `ConversationRole::{User, Assistant}` and `ConversationMessage`;
- `ContextSource::{SavedPersona, RecentHistory, CurrentTurn, BargeIn,
  TemporaryCorrection}`;
- `QualityDecision`, which contains no transcript or response content.

All types are backend-neutral and bounded at construction.

## Runtime Controller

Add `ConversationQualityController` in
`crates/runtime/src/conversation_quality.rs`.

It owns:

- immutable saved persona and default response controls;
- a bounded in-memory history of at most `8` completed exchanges and `16 KiB`;
- temporary one-turn corrections;
- the previous completed assistant response only as part of bounded history;
- content-free counters and the last resolved decision.

It does not persist data; R5 owns durable memory.

### Signal Resolution

Signal detection is conservative and explicit:

- shorter requests: exact correction phrases such as `shorter`, `briefly`,
  `keep it short`, `简短一点`, and `说短点`;
- stop explaining: explicit stop/explanation phrases;
- question rejection: explicit rejection phrases only when the previous
  completed assistant response ended with a question;
- hesitation: ellipsis or a small multilingual filler set;
- rapid topic change: explicit transition phrases such as `different topic`,
  `by the way`, `换个话题`, and `另外`;
- interruption: the existing typed barge-in event.

The controller does not claim semantic mind-reading from low lexical overlap.

### Response Resolution

- Inputs of at most `24` Unicode scalar values or `6` whitespace-delimited
  words default to at most `8` spoken seconds.
- `ShorterRequested`, `StopExplaining`, or the turn following an interruption
  caps the next response at the smaller of `8` seconds or half the saved
  maximum.
- A rejected question sets follow-up policy to `Never` for the current turn and
  adds an instruction not to repeat or rephrase the rejected question.
- Hesitation selects measured pace and disables automatic follow-up.
- Rapid topic change tells the model to follow the new topic without returning
  to the prior one.
- Temporary corrections expire after one successfully resolved turn and never
  mutate the saved persona.

## Typed Generation Envelope

Extend language requests to carry:

- current user transcript;
- bounded ordered recent messages;
- resolved mode and controls;
- runtime-generated system guidance;
- content-free context-source identifiers.

The Ollama adapter serializes the guidance as a system message, recent history
as ordered user/assistant messages, and the current transcript as the final user
message. Other providers can translate the same envelope independently.

Runtime-generated guidance is deterministic and bounded. It states observable
behavioral constraints rather than prescribing exact phrases.

## Relationship Principle

The controller never outputs `emit_affection`, affection frequency, unlock
levels, scripted special moments, or relationship quotas.

The model receives bounded shared context, visible persona, current mode, and
correction state. Warm or affectionate language may emerge only when supported
by that context and user reciprocity. `intimacy` changes style guidance but
cannot independently authorize an affectionate expression.

Every decision lists its `ContextSource` values, making relationship-relevant
behavior explainable without exposing transcript content in metrics.

## Events and Metrics

Add `RuntimeEvent::QualityResolved` and surface it through the existing voice
turn wrapper. `QualityDecision` includes mode, resolved controls, signal kinds,
history-message count, and context sources. It excludes transcripts, prompts,
responses, model identifiers, and provider payloads.

The event is nonterminal and precedes language generation.

## Configuration

Extend schema-v2 voice session configuration with:

```toml
[persona]
warmth = 0.8
humor = 0.6
teasing = 0.4
initiative = 0.35
directness = 0.8
intimacy = 0.3
verbosity = 0.2
follow_up_frequency = 0.25

[response]
mode = "direct-answer"
maximum_spoken_seconds = 20
pace = "natural"
allow_silence = true
ask_follow_up_by_default = false

[quality_metrics]
enabled = true
record_content = false
```

Float configuration values convert to `PersonaLevel`; NaN, infinity, and
values outside `0.0..=1.0` fail before microphone access.

The loaded persona and response configuration remain inspectable through the
runtime API. Temporary session state is separate and cannot overwrite them.

## Error and Privacy Boundaries

- Invalid persona, response, history, or content bounds fail as configuration
  errors before provider access.
- Quality metrics never contain content, even when telemetry is local.
- `LocalOnly` behavior is unchanged; recent context stays in-process and is
  sent only to the explicitly selected language adapter.
- Cancellation and barge-in preserve exactly-one terminal publication.
- Partial assistant output from cancelled or failed turns is not committed to
  history.

## Verification

Deterministic tests must prove:

- short prompts resolve to short controls;
- shorter and stop-explaining corrections constrain the current/next response;
- rejected questions are not immediately repeated in the generated envelope;
- silence creates no turn and no quality event;
- interruption changes the next decision and then expires;
- all four explicit modes are represented;
- saved persona remains unchanged across temporary corrections;
- only completed exchanges enter bounded history;
- relationship guidance contains no scripted expression or unlock;
- quality events and CLI metrics contain no transcript content;
- Ollama translation preserves message order and typed controls;
- existing cancellation, backpressure, privacy, and voice-session tests remain
  green.

## Scope Boundary

R4 provides bounded in-session state only. SQLite persistence, user editing,
retention, and durable relationship memory remain R5. Desktop controls remain
R6.
