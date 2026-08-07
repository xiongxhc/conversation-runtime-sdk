# R6 Gateway Voice Lane Design

**Status:** Approved for implementation planning
**Date:** 2026-08-07
**Milestone:** R6 — Desktop Reference App and SDK Boundary
**Parent:** [R6 Desktop Voice Session Design](2026-08-06-r6-desktop-voice-session-design.md)

## Scope

This slice implements the remaining gateway and TypeScript SDK portion of the
approved parent design. It ends at the SDK boundary: the desktop application,
Voice Focus activation, and the native human-verification boundary are the
next slice. The parent design's contracts, shared-context semantics,
lifecycle invariants, and privacy rules apply unchanged; this document only
narrows scope, records slice-level decisions, and reconciles the version
rebaseline that happened after the parent was approved.

Already landed from the parent design and reused as-is:

- Wire protocol voice commands, events, status capabilities, and component
  descriptors (`crates/protocol`, `packages/typescript/src/protocol.ts`).
- Gateway `[voice]` configuration, adapter construction, and capability
  computation (`apps/runtime-gateway/src/config.rs`, `voice_adapters.rs`).
- Shared `ConversationContext`, `VoiceSessionRuntime`, typed startup-failure
  stages, and the hardened abort/reap lifecycle in `conversation-runtime`.
- The fake managed voice sidecar (`conversation-fake-voice-sidecar`).

Not yet built, and in scope here:

1. The gateway session loop's voice lane (`apps/runtime-gateway/src/session.rs`
   currently rejects every voice command with "voice is unavailable" and
   serves `text_only_status()` unconditionally).
2. The TypeScript SDK voice surface (`RuntimeClient` currently fails the
   client if any `voice_event` arrives).
3. Compiled-gateway mixed typed/voice acceptance through the public SDK.

## Decisions Recorded in This Slice

- **Slice boundary:** gateway + SDK only, proven against the real compiled
  gateway with the fake sidecar. Desktop activation is a separate follow-up
  slice consuming this one's public surface.
- **Done gate:** deterministic tests are the merge gate; one env-gated opt-in
  live smoke with the real sidecar exists but is not part of CI or the merge
  gate, matching the existing opt-in full-duplex capture smoke idiom.
- **Version reconciliation:** the parent design predates the version
  rebaseline. Where it says config schema "version 2" and protocol "v3", the
  current contracts are config schema version 1 and client wire protocol
  version 1. No schema or protocol version changes in this slice.

## Gateway Voice Lane

The voice session lives inside the existing session actor, mirroring the
proven `conversation-voice-loop` composition in-process:

- When `[voice]` is configured, the gateway serves the full status —
  `voice_session` capability plus speech/audio component descriptors —
  instead of `text_only_status()`. Without `[voice]`, behavior is unchanged
  and voice commands keep the existing typed rejection.
- `start_voice_session` (accepted) builds the voice session from the
  already-constructed `GatewayVoiceAdapters` and the shared
  `ConversationContext`, holds the runtime handle and its event stream in the
  session state, and forwards every runtime event as a `voice_event` message
  on the session lane as it arrives.
- `stop_voice_session`, `pause_voice_capture`, and `resume_voice_capture`
  map onto the corresponding runtime controls. Acknowledgement ordering
  follows the parent design's invariants (pause acknowledgement follows
  actual capture shutdown; stop completion follows flush, cancellation,
  sidecar shutdown, and reaping).
- Request-scoped typed rejections, never session failure: voice not
  configured, voice already active, `start_turn` while a voice session is
  active, and voice control commands with no active voice session.
- One voice session per connection at a time. Client EOF or disconnect aborts
  the voice session through the abort-and-reap path, bounded and idempotent.
- A voice session failure emits its typed events (including startup-failure
  stages) and ends that session only; the gateway process and text lane
  remain healthy. Nothing conflates voice-session death with gateway death.
- The bounded writer path reserves capacity for reliable terminals and
  control acknowledgements per the parent's backpressure rules.

## TypeScript SDK Voice Surface

`RuntimeClient` gains the client half of the protocol it already validates:

- `startVoiceSession()` resolves with a `VoiceSession` handle on
  `command_accepted` and rejects request-scoped on `command_rejected`.
  A rejection never fails the client or an unrelated in-flight turn.
- `VoiceSession` exposes a typed async event stream (activity, partial and
  final transcripts, barge-in, turn events, timing, playback, capture
  pause/resume state, session errors) plus `stop()`, `pauseCapture()`, and
  `resumeCapture()`. Terminal session events settle the stream; exactly one
  terminal per session.
- `voice_event` messages route to the active session. The current
  fail-on-any-voice-event guard remains only for events that arrive with no
  active session (a protocol violation).
- The browser entry exports the voice types and methods without Node or
  desktop dependencies.

## Validation

Deterministic merge gate:

- **Rust gateway integration tests** drive the framed protocol against the
  compiled gateway with the fake sidecar and loopback STT/LLM/TTS fixtures:
  accept → activity → transcripts → assistant turn → playback → stop;
  barge-in; pause/resume ordering; every rejection path above; disconnect,
  repeated stop, and blocked-output cleanup; child-process reaping.
- **SDK tests**: unit coverage over scripted transports for correlation,
  request-scoped rejection, terminal settlement, and close during an active
  session; integration coverage spawning the real compiled gateway with a
  fake-sidecar configuration, including one mixed typed → spoken → typed
  flow proving shared context through the public SDK.
- All existing workspace gates stay green (cargo test, clippy `-D warnings`,
  fmt, npm workspaces, Swift sidecar tests).

Opt-in live smoke (not a merge gate): one env-gated test starting a real
voice session through gateway + SDK against the real sidecar using a private
local configuration, recording only content-free stage milestones — the
gateway-level analogue of the existing opt-in capture/playback smoke. The
parent design's native human boundary remains with the desktop slice, and no
R3 latency or acoustic acceptance claim is made here.

## Non-Goals

Everything in the parent design's non-goals, plus: desktop UI changes,
persona or memory mutation surfaces, configuration schema changes, protocol
changes, and R3 human or acoustic acceptance.
