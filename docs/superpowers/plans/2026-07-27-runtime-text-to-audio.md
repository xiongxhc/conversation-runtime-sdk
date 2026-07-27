# Runtime Text-to-Audio Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn one typed runtime turn into incrementally generated, phrase-segmented, local audible speech with unified cancellation and measured timing.

**Architecture:** `ConversationRuntime` owns a bounded phrase queue and one sequential speech worker. Generic language, speech, and audio-output adapters remain replaceable; lifecycle events carry timing and state but never encoded audio.

**Tech Stack:** Rust 2021, Tokio, tokio-util cancellation tokens, reqwest, serde/TOML, tempfile, Ollama-compatible streaming HTTP, OpenAI-compatible speech HTTP, macOS `afplay`.

## Global Constraints

- Public protocol and runtime types contain no Ollama, MLX-Audio, Qwen, voice, or deployment-policy types.
- Audio bytes move directly from `SpeechSynthesizer` to `AudioOutput`; they never enter `RuntimeEvent`.
- One accepted interruption token cancels generation, synthesis, queued phrases, and active playback.
- External interruption and internal pipeline-stop signals remain distinct so adapter failure cannot be misreported as user cancellation.
- Cleanup completes before terminal cancellation or failure is published.
- Every observed turn emits exactly one terminal event.
- Phrase defaults are soft limit 96 UTF-8 bytes, hard limit 192 UTF-8 bytes, and queue capacity 2.
- Sentence boundaries are `.`, `?`, `!`, `。`, `？`, `！`, and newline; soft boundaries additionally include whitespace, comma, colon, and semicolon.
- `FirstPlayableAudio` means validated encoded audio is ready for `AudioOutput`, not physical first-audible output.
- Typed WAV and AIFF containers are structurally validated before `FirstPlayableAudio`.
- Public example configuration is an explicitly backend-specific reference composition with generic identifiers, not an SDK default.
- Model files, private paths, credentials, and private deployment configuration stay outside the repository.
- New behavior follows strict RED-GREEN-REFACTOR: run each focused test before and after production changes.
- Do not add `Co-Authored-By` trailers.

---

## File Structure

- `crates/model-adapters/src/audio_output.rs`: generic output request, trait, and explicit discard output.
- `crates/model-adapters/src/speech.rs`: shared typed WAV and AIFF validation.
- `crates/model-adapters/src/macos_afplay.rs`: reusable macOS process-backed audio output.
- `crates/model-adapters/tests/macos_afplay.rs`: process, cleanup, bounds, and cancellation tests.
- `crates/runtime/src/phrase_chunker.rs`: pure UTF-8-safe phrase segmentation.
- `crates/runtime/src/speech_worker.rs`: sequential synthesis/output worker and stage-aware outcome.
- `crates/runtime/src/lib.rs`: public construction, language stream coordination, bounded queue, and terminal arbitration.
- `crates/protocol/src/event.rs`: neutral timing milestone and timing event.
- `crates/protocol/src/lib.rs`: public timing milestone re-export.
- `crates/protocol/src/error.rs`: audio-output runtime stage.
- `tests/voice/src/config.rs`: bounded backend-specific reference configuration.
- `tests/voice/src/main.rs`: integrated typed-turn CLI and signal handling.
- `tests/voice/tests/probe_cli.rs`: loopback language/speech fixtures plus fake player.
- `configs/voice.example.toml`: generic reference-composition template.
- `docs/runtime-text-to-audio-evaluation.md`: reproducible integration evidence and limitations.

---

### Task 1: Add the Generic Audio Output Contract

**Files:**
- Create: `crates/model-adapters/src/audio_output.rs`
- Modify: `crates/model-adapters/src/lib.rs`
- Modify: `crates/model-adapters/src/mock.rs`
- Modify: `crates/protocol/src/error.rs`
- Modify: `crates/runtime/src/lib.rs`
- Modify: `crates/runtime/tests/cancellation.rs`
- Modify: `crates/runtime/tests/commands.rs`
- Modify: `crates/runtime/tests/turn_flow.rs`
- Modify: `tests/latency/src/lib.rs`

**Interfaces:**
- Produces: `AudioOutputRequest::new(turn_id, segment_index, audio)`.
- Produces: `AudioOutput::play(&self, request, cancellation) -> AdapterFuture<'_, ()>`.
- Produces: `DiscardAudioOutput` and `MockAudioOutput`.
- Produces: `ConversationRuntime::new(language_model, speech_synthesizer, audio_output)`.
- Produces: `RuntimeStage::AudioOutput`.

- [ ] **Step 1: Write failing contract tests**

Add tests proving request accessors, explicit discard behavior, mock cancellation, and the new constructor dependency:

```rust
#[tokio::test]
async fn discard_output_accepts_typed_audio() {
    let output = DiscardAudioOutput;
    output
        .play(
            AudioOutputRequest::new(
                TurnId::new(1),
                7,
                SynthesizedAudio::new([1, 2, 3], AudioFormat::Aiff),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn pre_cancelled_mock_output_returns_cancellation() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = MockAudioOutput::new()
        .play(
            AudioOutputRequest::new(
                TurnId::new(1),
                0,
                SynthesizedAudio::new([1], AudioFormat::Aiff),
            ),
            cancellation,
        )
        .await
        .unwrap_err();
    assert_eq!(error.message(), "audio output cancelled");
}
```

- [ ] **Step 2: Run focused tests and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters mock --locked
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-runtime --locked
```

Expected: compilation fails because `AudioOutput`, request types, `RuntimeStage::AudioOutput`, and the third runtime constructor argument do not exist.

- [ ] **Step 3: Implement the minimal contract**

Use these exact public signatures:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioOutputRequest {
    turn_id: TurnId,
    segment_index: u64,
    audio: SynthesizedAudio,
}

pub trait AudioOutput: Send + Sync {
    fn play<'a>(
        &'a self,
        request: AudioOutputRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, ()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DiscardAudioOutput;
```

Add accessor methods for every request field. `DiscardAudioOutput` returns cancellation when already cancelled and otherwise succeeds. `MockAudioOutput` records cloned requests in `Arc<Mutex<Vec<AudioOutputRequest>>>`, supports an optional delay, and exposes a snapshot for deterministic runtime assertions.

Add `audio_output: Arc<dyn AudioOutput>` to `ConversationRuntime` and require it as the third constructor argument. Update all 19 executable construction sites with `Arc::new(DiscardAudioOutput)` until a test needs an observing output.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters --locked
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-runtime --locked
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-latency-harness --locked
```

Expected: all focused package tests pass with no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/model-adapters/src/audio_output.rs crates/model-adapters/src/lib.rs crates/model-adapters/src/mock.rs crates/protocol/src/error.rs crates/runtime/src/lib.rs crates/runtime/tests/cancellation.rs crates/runtime/tests/commands.rs crates/runtime/tests/turn_flow.rs tests/latency/src/lib.rs
git commit -m "feat: add runtime audio output contract"
```

---

### Task 2: Validate Typed Audio Containers

**Files:**
- Modify: `crates/model-adapters/src/speech.rs`
- Modify: `crates/model-adapters/src/macos_system_speech.rs`
- Modify: `crates/model-adapters/src/openai_compatible_speech.rs`
- Modify: `crates/model-adapters/tests/openai_compatible_speech.rs`
- Modify: `crates/model-adapters/tests/macos_system_speech.rs`
- Modify: `crates/model-adapters/src/mock.rs`
- Modify: `crates/runtime/tests/cancellation.rs`
- Modify: `crates/runtime/tests/commands.rs`
- Modify: `crates/runtime/tests/turn_flow.rs`
- Modify: `tests/latency/src/lib.rs`

**Interfaces:**
- Produces: `SynthesizedAudio::validate(&self) -> Result<(), AdapterError>`.
- Consumes: typed `AudioFormat::Wav` and `AudioFormat::Aiff`.

- [ ] **Step 1: Write failing WAV and AIFF validation tests**

Add table-driven tests with hand-built byte fixtures. Valid WAV requires exact `RIFF`/`WAVE`, matching RIFF size, a `fmt ` body of at least 16 bytes, and non-empty `data`. Valid AIFF requires exact `FORM`, matching big-endian FORM size, a `COMM` body of at least 18 bytes for `AIFF` or at least 22 bytes for `AIFC`, and an `SSND` body containing its 8-byte offset/block header plus sound data after the declared offset.

```rust
#[test]
fn typed_audio_rejects_malformed_containers() {
    for audio in [
        SynthesizedAudio::new(b"RIFF".to_vec(), AudioFormat::Wav),
        SynthesizedAudio::new(b"FORM".to_vec(), AudioFormat::Aiff),
    ] {
        assert_eq!(
            audio.validate().unwrap_err().message(),
            "synthesized audio was not a valid encoded container"
        );
    }
}
```

Cover overflow-safe declared sizes, truncated chunk headers/bodies/padding, missing format/data chunks, `SSND` offsets beyond the chunk body, and empty sound data after the offset for both endiannesses.

- [ ] **Step 2: Run focused tests and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters speech --locked
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters --test openai_compatible_speech --locked
```

Expected: compilation fails because `SynthesizedAudio::validate` does not exist.

- [ ] **Step 3: Implement shared structural validation**

Implement checked RIFF and FORM chunk walkers in `speech.rs`. Keep container error text content-free. Refactor both concrete speech synthesizers to validate their typed output before returning it. Preserve the OpenAI-compatible adapter's existing privacy-safe WAV error by mapping the shared validation failure at that boundary. Update every success fixture used through the runtime and latency harness to a hand-built minimal valid PCM WAV or AIFF; do not weaken validation for mocks.

- [ ] **Step 4: Run adapter tests and verify GREEN**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters --locked
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-runtime --locked
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-latency-harness --locked
```

Expected: all shared validation, OpenAI-compatible speech, macOS speech, and mock tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/model-adapters/src/speech.rs crates/model-adapters/src/macos_system_speech.rs crates/model-adapters/src/openai_compatible_speech.rs crates/model-adapters/tests/openai_compatible_speech.rs crates/model-adapters/tests/macos_system_speech.rs crates/model-adapters/src/mock.rs crates/runtime/tests/cancellation.rs crates/runtime/tests/commands.rs crates/runtime/tests/turn_flow.rs tests/latency/src/lib.rs
git commit -m "feat: validate typed audio containers"
```

---

### Task 3: Add the Reusable macOS Audio Output

**Files:**
- Create: `crates/model-adapters/src/macos_afplay.rs`
- Create: `crates/model-adapters/tests/macos_afplay.rs`
- Modify: `crates/model-adapters/src/lib.rs`
- Modify: `crates/model-adapters/README.md`

**Interfaces:**
- Consumes: `AudioOutput`, `AudioOutputRequest`, `AdapterFuture`, and typed `SynthesizedAudio`.
- Produces: `MacOsAfplayConfig` and `MacOsAfplayAudioOutput`.

- [ ] **Step 1: Write failing process-boundary tests**

Create tests for absolute-path validation, non-zero limits, platform default, direct invocation, typed `.aiff`/`.wav` suffixes, empty and oversized audio, bounded sanitized standard error, spawn failure, non-zero exit, pre-cancellation, active cancellation, descendant-held stderr, and temporary-file cleanup.

The success test must assert behavior rather than the fake executable itself:

```rust
let request = AudioOutputRequest::new(
    TurnId::new(3),
    2,
    SynthesizedAudio::new(minimal_pcm_wav(), AudioFormat::Wav),
);
output.play(request, CancellationToken::new()).await.unwrap();
let played_path = PathBuf::from(std::fs::read_to_string(capture_path).unwrap());
assert_eq!(played_path.extension().and_then(|value| value.to_str()), Some("wav"));
assert!(!played_path.exists());
assert!(std::fs::read_dir(playback_directory).unwrap().next().is_none());
```

- [ ] **Step 2: Run the adapter test and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters --test macos_afplay --locked
```

Expected: compilation fails because the macOS output types do not exist.

- [ ] **Step 3: Implement bounded process playback**

Use defaults:

```rust
const DEFAULT_MAX_AUDIO_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_ERROR_BYTES: usize = 4 * 1024;
const STDERR_CLEANUP_GRACE: Duration = Duration::from_millis(100);
```

`MacOsAfplayConfig::new` validates an absolute executable and uses `std::env::temp_dir()`. Builders validate an absolute temporary directory and non-zero audio/error limits. `system_default` returns `/usr/bin/afplay` only on macOS.

Before creating a file or process, reject cancellation, audio above the configured limit, and any `SynthesizedAudio::validate` failure. Write one typed temporary file, invoke the configured executable directly, pipe and concurrently drain bounded standard error, use `kill_on_drop(true)`, bias cancellation, kill and await the child, bound stderr cleanup, sanitize controls and whitespace, and rely on `NamedTempFile` cleanup on every path.

- [ ] **Step 4: Run focused and package tests**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters --test macos_afplay --locked
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-model-adapters --locked
```

Expected: every new process and cleanup test passes; existing speech adapter tests remain green.

- [ ] **Step 5: Commit**

```bash
git add crates/model-adapters/src/macos_afplay.rs crates/model-adapters/tests/macos_afplay.rs crates/model-adapters/src/lib.rs crates/model-adapters/README.md
git commit -m "feat: add macOS audio output adapter"
```

---

### Task 4: Add Configurable UTF-8-Safe Phrase Segmentation

**Files:**
- Create: `crates/runtime/src/phrase_chunker.rs`
- Modify: `crates/runtime/src/lib.rs`

**Interfaces:**
- Produces: public `PhraseChunkingConfig::new(soft_limit_bytes, hard_limit_bytes)`.
- Produces: `ConversationRuntime::with_phrase_chunking(config)`.
- Produces: `PhraseChunker::push_delta(&mut self, delta) -> Vec<String>`.
- Produces: `PhraseChunker::finish(self) -> Option<String>`.

- [ ] **Step 1: Write failing phrase behavior tests**

Colocate pure tests with the private module. Use hand-derived literals:

```rust
#[test]
fn fragmented_multilingual_deltas_flush_complete_phrases() {
    let mut chunker = PhraseChunker::default();
    assert!(chunker.push_delta("你好，").is_empty());
    assert_eq!(chunker.push_delta("世界。Next"), vec!["你好，世界。"]);
    assert_eq!(chunker.push_delta(" sentence!"), vec!["Next sentence!"]);
    assert_eq!(chunker.finish(), None);
}

#[test]
fn hard_limit_never_splits_utf8() {
    let mut chunker = PhraseChunker::new(PhraseChunkingConfig::new(6, 9).unwrap());
    assert_eq!(chunker.push_delta("你好世界"), vec!["你好世"]);
    assert_eq!(chunker.finish().as_deref(), Some("界"));
}
```

Also test newline removal as a boundary, soft whitespace/comma/colon/semicolon boundaries after the soft limit, multiple phrases in one delta, final remainder, whitespace-only input, and invalid zero/reversed limits.

- [ ] **Step 2: Run the focused unit tests and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-runtime phrase_chunker --locked
```

Expected: compilation fails because the module and types do not exist.

- [ ] **Step 3: Implement the minimal pure chunker**

Defaults:

```rust
const DEFAULT_SOFT_LIMIT_BYTES: usize = 96;
const DEFAULT_HARD_LIMIT_BYTES: usize = 192;
```

Scan only `char_indices`, retain sentence punctuation, treat newline as a consumed boundary, trim only segment-edge whitespace, and repeatedly drain complete segments so one delta may return multiple phrases. At the hard limit, split at the greatest valid UTF-8 boundary not exceeding the limit.

Store `PhraseChunkingConfig` in `ConversationRuntime`, default it to 96/192, and expose validated getters plus `with_phrase_chunking`. The concurrent-pipeline task adds the behavior-level test proving the configured values determine synthesis segmentation.

- [ ] **Step 4: Run the focused tests and verify GREEN**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-runtime phrase_chunker --locked
```

Expected: all phrase tests pass without Tokio timing.

- [ ] **Step 5: Commit**

```bash
git add crates/runtime/src/phrase_chunker.rs crates/runtime/src/lib.rs
git commit -m "feat: add runtime phrase segmentation"
```

---

### Task 5: Add Public Runtime Timing Events

**Files:**
- Modify: `crates/protocol/src/event.rs`
- Modify: `crates/protocol/src/lib.rs`

**Interfaces:**
- Produces: `RuntimeTimingMilestone`.
- Produces: `RuntimeEvent::Timing { turn_id, milestone, elapsed_ms }`.

- [ ] **Step 1: Write failing public protocol tests**

Add tests that construct all public milestone values through the crate root, prove timing events are nonterminal, and preserve the turn identifier:

```rust
let event = RuntimeEvent::Timing {
    turn_id: TurnId::new(9),
    milestone: RuntimeTimingMilestone::FirstPlayableAudio,
    elapsed_ms: 42,
};
assert_eq!(event.turn_id(), TurnId::new(9));
assert!(!event.is_terminal());
```

- [ ] **Step 2: Run focused tests and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-protocol --locked
```

Expected: compilation fails because the timing enum and event variant do not exist.

- [ ] **Step 3: Implement and export neutral timing types**

Add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RuntimeTimingMilestone {
    FirstTextDelta,
    FirstSynthesisRequest,
    FirstPlayableAudio,
}
```

`RuntimeEvent::Timing` contains `elapsed_ms: u64`. Update `turn_id` matching and terminal classification. Re-export both `RuntimeEvent` and `RuntimeTimingMilestone` from `conversation_protocol`.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-protocol --locked
```

Expected: all protocol tests pass. Task 6 verifies downstream runtime and harness compatibility.

- [ ] **Step 5: Commit**

```bash
git add crates/protocol/src/event.rs crates/protocol/src/lib.rs
git commit -m "feat: add runtime timing events"
```

---

### Task 6: Integrate the Concurrent Speech Pipeline

**Files:**
- Create: `crates/runtime/src/speech_worker.rs`
- Modify: `crates/runtime/src/lib.rs`
- Modify: `crates/runtime/tests/turn_flow.rs`
- Modify: `crates/runtime/tests/cancellation.rs`
- Modify: `crates/runtime/tests/commands.rs`
- Modify: `tests/latency/src/lib.rs`
- Modify: `tests/latency/tests/mock_probe.rs`

**Interfaces:**
- Consumes: `PhraseChunker`, public `PhraseChunkingConfig`, typed-audio validation, `RuntimeTimingMilestone`, `SpeechSynthesizer`, `AudioOutput`, and queue capacity 2.
- Produces: stage-aware `SpeechWorkerOutcome`.
- Produces: separate external-interruption and internal pipeline-stop arbitration.

- [ ] **Step 1: Write failing runtime behavior tests**

Use controlled adapters with barriers and channels, not sleeps. Add tests proving:

```rust
assert!(matches!(
    events[first_text_delta_index - 1],
    RuntimeEvent::Timing {
        milestone: RuntimeTimingMilestone::FirstTextDelta,
        ..
    }
));
assert!(matches!(events[first_speech_index], RuntimeEvent::SpeechStarted { .. }));
assert!(matches!(
    events[first_speech_index + 1],
    RuntimeEvent::Timing {
        milestone: RuntimeTimingMilestone::FirstSynthesisRequest,
        ..
    }
));
assert_eq!(
    synthesized_text,
    vec!["First sentence.".to_owned(), "Second sentence.".to_owned()]
);
assert!(first_synthesis_started_before_model_release);
assert_eq!(terminal_events.len(), 1);
```

Cover:

- first synthesis before model completion;
- segment ordering;
- a custom 6/9 `PhraseChunkingConfig` changing actual synthesis segmentation;
- a full two-item phrase queue backpressuring further language consumption;
- exactly one of each timing milestone in causal order;
- typed-audio validation before first playable and output;
- whitespace-only model output emitting no speech lifecycle or speech timing;
- interruption during queued synthesis and active playback;
- output failure cancelling generation, active synthesis, queue, and output cleanup;
- output failure while the lifecycle event channel is saturated;
- language failure while speech is active, with phrase-queue discard, speech/output cleanup, and `RuntimeStage::LanguageModel`;
- synthesis failure while generation is active, with generation cancellation, queue discard, and `RuntimeStage::SpeechSynthesizer`;
- worker lifecycle sends blocked by a saturated event channel still resolving on interruption and internal failure;
- cleanup before cancellation/failure terminal publication;
- runtime reuse after completion, external cancellation, language failure, synthesis failure, and output failure.

- [ ] **Step 2: Run runtime tests and verify behavioral RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-runtime --locked
```

Expected: tests compile with the timing protocol but fail because synthesis still waits for complete generation, output is not called, and phrase configuration does not yet control runtime behavior.

- [ ] **Step 3: Implement the bounded worker and failure arbitration**

Capture one `Instant` immediately before successfully sending `TurnStarted`; convert elapsed milliseconds with `u64::try_from(value).unwrap_or(u64::MAX)`.

Create `SpeechSegment { index: u64, text: String }`, `mpsc::channel(2)`, and one worker. The worker:

1. emits `SpeechStarted` immediately before first-synthesis timing;
2. emits first-synthesis timing immediately before invoking the speech adapter;
3. calls `SynthesizedAudio::validate` after synthesis;
4. emits first-playable timing once after validation and before output;
5. calls output sequentially;
6. emits `SpeechCompleted` after the closed queue drains;
7. returns exact speech/output stage failure only after owned cleanup.

Every worker-owned send of `SpeechStarted`, timing, or `SpeechCompleted` selects the external-interruption token, work-cancellation token, and event-channel send. A blocked worker event send therefore resolves when interruption or internal pipeline shutdown occurs.

Maintain two tokens:

```rust
struct ActiveTurn {
    turn_id: TurnId,
    external_interruption: CancellationToken,
    work_cancellation: CancellationToken,
}
```

`Interrupt` cancels both tokens. Internal language, speech, or output failure cancels only `work_cancellation`. The terminal override checks only `external_interruption`, so internal failures cannot become `TurnCancelled`.

While the worker is active, every blocking text-event send and phrase-queue send selects among external interruption, internal work cancellation, worker completion, and the send itself. When work cancellation wins, await the worker result before selecting the stage-aware terminal failure. A worker failure cancels `work_cancellation` before returning, which wakes a producer blocked behind lifecycle or phrase-queue backpressure. Drop the phrase sender and await the worker on every exit path. A language failure during active speech retains the language error after worker cleanup; a speech or output failure during active generation retains the worker stage after language cleanup.

- [ ] **Step 4: Run focused and regression tests**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-runtime --locked
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-latency-harness --locked
```

Expected: overlap, configured segmentation, ordering, backpressure, exact timing adjacency, validation, stage, cleanup, terminal, and reuse tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/runtime/src/speech_worker.rs crates/runtime/src/lib.rs crates/runtime/tests/turn_flow.rs crates/runtime/tests/cancellation.rs crates/runtime/tests/commands.rs tests/latency/src/lib.rs tests/latency/tests/mock_probe.rs
git commit -m "feat: stream runtime phrases to audio output"
```

---

### Task 7: Add the Integrated Voice Probe

**Files:**
- Create: `tests/voice/Cargo.toml`
- Create: `tests/voice/src/config.rs`
- Create: `tests/voice/src/main.rs`
- Create: `tests/voice/tests/probe_cli.rs`
- Create: `configs/voice.example.toml`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: `OllamaLanguageModel`, `OpenAiCompatibleSpeechSynthesizer`, `MacOsAfplayAudioOutput`, and `ConversationRuntime`.
- Produces: binary `conversation-voice-probe`.

- [ ] **Step 1: Scaffold the testable package boundary**

Add `tests/voice` to workspace members. Create its manifest with the existing workspace dependencies plus `serde` and `toml`, and create an empty behavior-free binary:

```rust
mod config;

fn main() {}
```

Create an empty `config.rs`. Run `cargo metadata --no-deps` once so `Cargo.lock` records only the package graph needed by later RED tests.

- [ ] **Step 2: Write failing configuration and CLI tests**

Configuration tests cover absolute bounded paths, a 64 KiB file limit, schema version 1, unknown fields, generic identifiers, loopback endpoints, explicit language inference controls, speech controls, audio limits, and `--no-play`.

The CLI integration fixture starts loopback Ollama-style NDJSON and speech WAV servers plus a fake player. Assert real observable output:

```rust
assert!(output.status.success());
assert_eq!(String::from_utf8(output.stdout).unwrap(), "First sentence. Second sentence.");
let stderr = String::from_utf8(output.stderr).unwrap();
assert!(stderr.contains("milestone=first_text_delta"));
assert!(stderr.contains("milestone=first_synthesis_request"));
assert!(stderr.contains("milestone=first_playable_audio"));
assert!(stderr.contains("status=completed"));
assert_eq!(std::fs::read_dir(capture_directory).unwrap().count(), 2);
```

Also cover malformed config, HTTP failure stages, `--no-play`, standard input, and signal-driven cleanup with deterministic fixtures.

- [ ] **Step 3: Run the probe tests and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-voice-probe --locked
```

Expected: the package compiles, then behavior assertions fail because the empty binary emits no text, timing, playback, or structured failures.

- [ ] **Step 4: Implement the bounded reference composition**

Add this public example shape:

```toml
schema_version = 1

[language]
endpoint = "http://127.0.0.1:11434"
model = "replace-with-installed-model-id"
thinking = false
temperature = 0.0
seed = 42
num_predict = 128
num_ctx = 8192
max_assistant_content_bytes = 65536

[speech]
endpoint = "http://127.0.0.1:8000/v1"
model = "replace-with-local-speech-model"
voice = "replace-with-local-voice"
speed = 1.0
language = "replace-with-language-hint"
instructions = "Speak naturally and clearly."
max_tokens = 128
repetition_penalty = 1.05
max_text_bytes = 4096
max_audio_bytes = 8388608

[audio]
backend = "macos-afplay"
executable = "/usr/bin/afplay"
temp_directory = "/private/tmp"
max_audio_bytes = 8388608
max_error_bytes = 4096
```

Reject non-loopback plain-HTTP endpoints in this reference probe, while adapter libraries retain their existing configurable HTTP(S) contracts. Parse prompt arguments or non-empty standard input. Print only text deltas to standard output. Print one stable key-value line per timing milestone and terminal status to standard error. `SIGINT` sends `RuntimeCommand::Interrupt` and drains the terminal event.

- [ ] **Step 5: Run focused tests and verify GREEN**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test -p conversation-voice-probe --locked
```

Expected: configuration and loopback integration tests pass without model downloads or real playback.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock tests/voice configs/voice.example.toml
git commit -m "feat: add integrated local voice probe"
```

---

### Task 8: Document and Measure the R2 Slice

**Files:**
- Create: `docs/runtime-text-to-audio-evaluation.md`
- Modify: `README.md`
- Modify: `ROADMAP.md`
- Modify: `docs/architecture.md`
- Modify: `tests/latency/README.md`

**Interfaces:**
- Consumes: the completed voice probe and its stable timing output.
- Produces: reproducible deterministic and Apple Silicon evidence.

- [ ] **Step 1: Run deterministic repository gates**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo fmt --all -- --check
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo clippy --workspace --all-targets --locked -- -D warnings
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --workspace --locked
git diff --check
```

Expected: formatting and Clippy pass; every workspace test passes.

- [ ] **Step 2: Run one local Apple Silicon integration**

Create a private absolute TOML outside the repository using installed exact model identifiers and loopback endpoints. Create a private executable wrapper outside the repository that records the player-process launch timestamp before replacing itself with `afplay`:

```bash
cat > "/absolute/private/path/record-afplay-launch.zsh" <<'SCRIPT'
#!/bin/zsh
zmodload zsh/datetime
if [[ ! -e "$CONVERSATION_PLAYBACK_LAUNCH_FILE" ]]; then
  print -r -- "$EPOCHREALTIME" > "$CONVERSATION_PLAYBACK_LAUNCH_FILE"
fi
exec /usr/bin/afplay "$@"
SCRIPT
chmod 700 "/absolute/private/path/record-afplay-launch.zsh"
```

Point the private TOML audio executable at that wrapper. Start Ollama and MLX-Audio on loopback, build once, and run the binary directly so compilation is excluded:

```bash
rustup run 1.97.1 cargo build --locked -p conversation-voice-probe
zmodload zsh/datetime
RUN_STARTED="$EPOCHREALTIME"
rm -f "/absolute/private/path/playback-launch.txt"
CONVERSATION_PLAYBACK_LAUNCH_FILE="/absolute/private/path/playback-launch.txt" \
  target/debug/conversation-voice-probe \
  --config "/absolute/private/path/voice.toml" \
  "Answer in two short sentences: 你好，请简短介绍你自己。"
PLAYBACK_LAUNCHED="$(cat /absolute/private/path/playback-launch.txt)"
awk -v started="$RUN_STARTED" -v launched="$PLAYBACK_LAUNCHED" \
  'BEGIN { printf "playback_launch_ms=%.3f\n", (launched - started) * 1000 }'
```

Record the exact commit, Rust version, machine profile, language-model identifier and digest, speech snapshot revision and digest, loaded/cold state, first text, first synthesis, first playable, wrapper-observed playback launch, total completion, and cleanup status. Delete the private wrapper and timestamp file after evidence capture. Do not label playback launch as first audible.

- [ ] **Step 3: Update public documentation**

README provides the generic copy-to-private-config workflow and one integrated command. Architecture documents the generic `AudioOutput` boundary and separate media/lifecycle paths. Roadmap marks only verified R2 deliverables complete and keeps first-audible, microphone, ASR, and barge-in pending. Evaluation evidence distinguishes deterministic tests from one machine-specific measurement and labels exact models as benchmark inputs rather than recommendations.

- [ ] **Step 4: Re-run documentation and repository checks**

Run:

```bash
grep -RInE '\\b(TB[D]|TO[D]O|PLACEHOLD[E]R)\\b' README.md ROADMAP.md docs/*.md configs tests/voice || true
grep -RIn '/Users/' README.md ROADMAP.md docs/*.md configs tests/voice || true
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo fmt --all -- --check
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo clippy --workspace --all-targets --locked -- -D warnings
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --workspace --locked
git diff --check
```

Expected: no incomplete markers or private user paths, all code gates pass, and the working tree contains only intended documentation/evidence changes.

- [ ] **Step 5: Commit**

```bash
git add README.md ROADMAP.md docs/architecture.md docs/runtime-text-to-audio-evaluation.md tests/latency/README.md
git commit -m "docs: record runtime text-to-audio integration"
```

---

## Final Review and Integration Gates

- Generate a whole-branch review package from `134419d` to `HEAD`.
- Run a fresh final reviewer against the approved design, this plan, every task report, deferred Minor ledger entries, and the whole-branch diff.
- Resolve every Critical or Important finding and re-review the exact fix diff.
- Run `cargo fmt`, full workspace Clippy, full workspace tests, `git diff --check`, incomplete-marker scan, private-path scan, and `git status`.
- Keep model servers loopback-only and stop temporary benchmark processes.
- Push `feature/runtime-text-to-audio` only after all gates and final review pass.
