# Late Recognition Endpointing Design

## Problem

The runtime correctly waits for configured final silence before starting a voice
turn. When that silence deadline expires before WhisperKit emits an engine-final
hypothesis, the current session waits for the hypothesis and then arms the full
final-silence duration again. With the default configuration, a delayed ASR final
therefore adds a redundant `600 ms` after the user has already been silent long
enough.

The user-visible requirement is shorter speech-end-to-response latency without
cutting off adjacent ASR segments, adding language-specific punctuation rules, or
changing the public runtime contract.

## Decision

Keep the configured final-silence gate authoritative. Add an internal `120 ms`
late-recognition debounce that applies only when an engine-final hypothesis
arrives after the silence gate has already elapsed.

- If final silence has not elapsed, retain only its remaining duration.
- If final silence has elapsed, arm `120 ms` instead of the full silence duration.
- Each additional engine-final hypothesis during that debounce restarts `120 ms`
  so adjacent segments can join the same turn.
- New speech disarms the deadline exactly as it does today.
- A partial replacement segment still prevents finalization until it becomes
  engine-final.

The public protocol, configuration schema, provider interfaces, privacy policy,
and default `final_silence_ms` remain unchanged.

## Considered Approaches

### Immediate finalization after a late engine final

This minimizes latency but can split one utterance when the recognizer emits
adjacent final segments in separate callbacks. Rejected.

### Reapply the complete configured silence duration

This is the current behavior. It is safe but creates avoidable dead air after
the silence contract has already been satisfied. Rejected.

### Short late-final debounce

This retains a small collection window without charging the user for a second
full silence interval. Selected because it is backend-neutral, language-neutral,
and isolated to runtime endpointing.

## Runtime Changes

`TurnFinalizer` exposes the remaining configured silence for the current
utterance at a runtime-clock timestamp. It returns no value before speech ends,
the positive remainder before the gate, and zero once the gate has elapsed.

`VoiceSession` uses that value when an engine-final hypothesis arrives while
listening:

```text
speech not ended          -> no hypothesis-owned deadline
silence still remaining  -> arm only the remaining duration
silence already elapsed  -> arm 120 ms late-final debounce
additional late final    -> restart the 120 ms debounce
speech resumes           -> disarm
```

The finalizer remains authoritative for whether a transcript is actually ready.
The deadline only schedules the next readiness check.

## Failure and Privacy Rules

- No transcript content is added to diagnostics or telemetry.
- Deadline arithmetic uses saturating operations.
- Empty, whitespace-only, partial, cancelled, and stale hypotheses retain their
  existing behavior.
- No remote service, semantic endpointing model, or silent provider fallback is
  introduced.

## Verification

Deterministic tests must prove:

- remaining silence is absent before speech end, decreases against the runtime
  clock, reaches zero, and clears when speech resumes;
- a late engine final starts a turn after `120 ms`, not another `600 ms`;
- an adjacent late engine-final segment restarts the short debounce and joins the
  same turn;
- a late partial still prevents premature finalization;
- ordinary engine-final-before-speech-end and normal silence behavior remain
  unchanged;
- Rust formatting, Clippy, workspace tests, Swift tests, desktop tests/build, and
  independent review pass.

Real speech-end-to-first-audible improvement remains a hardware measurement, not
an automated-test claim.

## Non-Goals

- Semantic VAD or an endpointing model.
- Punctuation-based or language-specific completion rules.
- Changing `final_silence_ms` defaults or accepted ranges.
- Starting generation from partial transcripts.
- Claiming subjective or GPT-class voice quality without acoustic evidence.
