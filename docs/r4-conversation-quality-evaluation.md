# R4 Conversation Quality Evaluation

## Status

`COMPLETE FOR BOUNDED IN-SESSION CONTROLS`

R4 is evaluated through deterministic contracts. It does not require a model
quality benchmark to prove that the runtime resolves, exposes, and preserves
typed controls correctly. Subjective model behavior remains a consuming
deployment concern.

## Evidence Classes

### Protocol Contracts

The protocol suite must prove that persona levels are bounded, every public mode
and signal is representable, and `QualityDecision` events contain no transcript,
prompt, response, model, or provider content.

### Controller Scenarios

The runtime suite must cover:

- short prompts receiving a short spoken-duration budget;
- explicit shorter and stop-explaining corrections;
- one-turn interruption state that expires after use;
- rejected questions not being repeated or rephrased;
- hesitation selecting measured pace without an automatic follow-up;
- rapid topic changes following the new subject;
- silence creating no turn or quality decision;
- all four explicit conversation modes;
- temporary state never mutating the saved persona;
- only completed exchanges entering bounded recent history; and
- relationship guidance containing no expression script, unlock, counter, or
  frequency quota.

### Adapter Translation

The Ollama-compatible reference test must observe this request order:

1. optional deployment-owned system guidance;
2. runtime-generated bounded quality guidance;
3. ordered bounded user and assistant history; and
4. the current transcript as the final user message.

The adapter test proves translation only. It does not promote one provider or
model into an SDK default.

### Lifecycle Regression

The complete runtime suite must retain exactly-one terminal publication,
cancellation cleanup, backpressure, stale-generation exclusion, and local-only
privacy behavior after quality decisions are inserted before generation.

## Privacy Boundary

Quality events and CLI metrics expose only content-free state such as selected
mode, response limits, signal kinds, history-message count, and context-source
kinds. Recent transcript content stays in process and is sent only to the
explicitly selected language adapter. It is never added to telemetry by this
milestone.

## Relationship Principle

Warm or affectionate language is not a runtime command. The runtime supplies
bounded shared context, visible persona, current mode, pacing, and correction
state. Any relationship expression must emerge from that evidence and user
reciprocity rather than a scripted event, hidden unlock, or repetition target.

## Completion Rule

R4 is complete when the focused protocol, controller, adapter, configuration,
and lifecycle scenarios pass; the full Rust and Swift regression suites pass;
and public documentation describes the inspectable saved and temporary state
without claiming persistent memory or a desktop UI. SQLite persistence remains
R5 and application controls remain R6.

## Current Verification

The 2026-08-02 implementation gate passed:

```text
cargo test --workspace --locked --no-fail-fast
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  xcrun swift test --package-path platform/macos/voice-sidecar \
  --parallel --num-workers 1
git diff --check
```

Focused evidence includes `4` protocol quality-contract tests, `11` controller
scenarios, `23` Ollama adapter tests including typed message ordering and output
cap translation, `10` streaming-turn tests including completed-history and
one-turn interruption state, and `21` schema-v1 continuous CLI tests including
configuration rejection and content-free metric output. The complete Rust
workspace passed with its one intentionally ignored immutable-fixture writer;
the complete Swift package passed `109` tests.

These results establish deterministic SDK behavior, not subjective output
quality for every model. They also do not add SQLite memory, a desktop editor,
or a durable relationship state.
