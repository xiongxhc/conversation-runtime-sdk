# Local TTS Vertical Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a backend-neutral typed audio result, a bounded macOS system-speech reference adapter, and a command-line probe that turns typed text into audible local speech.

**Architecture:** `conversation-model-adapters` owns the typed audio value and the command-compatible macOS reference adapter. `conversation-runtime` awaits speech-adapter cancellation cleanup instead of dropping the adapter future. A separate `conversation-tts-probe` owns output-file persistence and `/usr/bin/afplay` playback so audio bytes and operating-system commands never enter `conversation-protocol`.

**Tech Stack:** Rust 1.97.1, Tokio 1.x process/filesystem/signal APIs, Tokio cancellation tokens, `tempfile`, macOS `/usr/bin/say`, macOS `/usr/bin/afplay`.

## Global Constraints

- Public protocol and runtime types remain independent of any TTS vendor, model, voice, or operating-system command.
- The macOS system-speech implementation is a reference adapter, not a preferred deployment backend.
- Public examples use generic backend and voice identifiers.
- Venture model and voice choices, routing thresholds, and deployment policy remain outside this repository.
- Exact identifiers appear only in clearly labeled reproducibility evidence.
- Child processes are started directly without a shell.
- Cancellation wins when cancellation and process completion are simultaneously ready.
- Every spawned child is awaited after termination.
- Generated audio and prompt text never appear in structured errors.
- Tests never invoke real speech tools or audio hardware.
- The slice does not claim neural quality, phrase streaming, first audible audio, microphone input, ASR, or barge-in.

---

### Task 1: Typed Audio and Cleanup-Aware Runtime

**Files:**
- Modify: `crates/model-adapters/src/speech.rs`
- Modify: `crates/model-adapters/src/lib.rs`
- Modify: `crates/model-adapters/src/mock.rs`
- Modify: `crates/runtime/src/lib.rs`
- Modify: `crates/runtime/tests/cancellation.rs`
- Modify: `crates/runtime/tests/turn_flow.rs`

**Interfaces:**
- Consumes: existing `SpeechRequest`, `SpeechSynthesizer`, and runtime cancellation token.
- Produces: `AudioFormat`, `SynthesizedAudio`, and `SpeechSynthesizer::synthesize(...) -> AdapterFuture<'_, SynthesizedAudio>`.

- [ ] **Step 1: Write failing typed-audio tests**

Add tests in `crates/model-adapters/src/speech.rs` that require the following public API:

```rust
#[test]
fn synthesized_audio_exposes_declared_format_and_optional_metadata() {
    let audio = SynthesizedAudio::new([1, 2, 3], AudioFormat::Aiff)
        .with_sample_rate_hz(22_050)
        .with_channels(1);

    assert_eq!(audio.bytes(), &[1, 2, 3]);
    assert_eq!(audio.format(), AudioFormat::Aiff);
    assert_eq!(audio.sample_rate_hz(), Some(22_050));
    assert_eq!(audio.channels(), Some(1));
}
```

Update one mock test to require:

```rust
let speech = MockSpeechSynthesizer::new([1, 2, 3]);
let audio = speech
    .synthesize(
        SpeechRequest::new(TurnId::new(1), "hello"),
        CancellationToken::new(),
    )
    .await
    .unwrap();

assert_eq!(audio.bytes(), &[1, 2, 3]);
assert_eq!(audio.format(), AudioFormat::Aiff);
```

- [ ] **Step 2: Run the adapter tests and verify RED**

Run:

```bash
cargo test -p conversation-model-adapters synthesized_audio --locked
```

Expected: compilation fails because `AudioFormat` and `SynthesizedAudio` do not exist.

- [ ] **Step 3: Implement the typed audio contract**

Add to `crates/model-adapters/src/speech.rs`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AudioFormat {
    Aiff,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SynthesizedAudio {
    bytes: Vec<u8>,
    format: AudioFormat,
    sample_rate_hz: Option<u32>,
    channels: Option<u16>,
}
```

Implement `new`, `bytes`, `format`, `sample_rate_hz`, `channels`, `with_sample_rate_hz`, and `with_channels`. Change `SpeechSynthesizer` to return `AdapterFuture<'a, SynthesizedAudio>`, export both types from `lib.rs`, and make `MockSpeechSynthesizer` wrap its configured bytes as `AudioFormat::Aiff`.

- [ ] **Step 4: Run adapter tests and verify GREEN**

Run:

```bash
cargo test -p conversation-model-adapters --locked
```

Expected: all model-adapter tests pass.

- [ ] **Step 5: Write the failing runtime cleanup test**

Add a `CleanupAwareSpeech` test adapter in `crates/runtime/tests/cancellation.rs`:

```rust
struct CleanupAwareSpeech {
    started: Arc<AtomicBool>,
    cleanup_completed: Arc<AtomicBool>,
}

impl SpeechSynthesizer for CleanupAwareSpeech {
    fn synthesize<'a>(
        &'a self,
        _request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio> {
        Box::pin(async move {
            self.started.store(true, Ordering::Release);
            cancellation.cancelled().await;
            tokio::task::yield_now().await;
            self.cleanup_completed.store(true, Ordering::Release);
            Err(AdapterError::new("speech synthesis cancelled"))
        })
    }
}
```

Start a turn, wait for `started`, interrupt it, consume the terminal event, and assert `cleanup_completed` is true before the event stream ends.

- [ ] **Step 6: Run the cleanup test and verify RED**

Run:

```bash
cargo test -p conversation-runtime waits_for_speech_cleanup_before_cancellation_completes --locked
```

Expected: the cleanup assertion fails because the current runtime drops the synthesis future when the outer cancellation branch wins.

- [ ] **Step 7: Make runtime synthesis cleanup-aware**

In `run_turn`, create a child speech token, await `synthesize` directly, then check the parent token before interpreting the adapter result:

```rust
let speech_cancellation = cancellation.child_token();
let speech_result = speech_synthesizer
    .synthesize(
        SpeechRequest::new(turn_id, response),
        speech_cancellation,
    )
    .await;

if cancellation.is_cancelled() {
    return RuntimeEvent::TurnCancelled { turn_id };
}
```

Only report an adapter failure when the parent turn was not cancelled. Update custom test synthesizers to return `SynthesizedAudio`.

- [ ] **Step 8: Run runtime tests and verify GREEN**

Run:

```bash
cargo test -p conversation-runtime --locked
```

Expected: all runtime tests pass, including cleanup at the synthesis boundary.

- [ ] **Step 9: Commit the typed contract scope**

```bash
git add crates/model-adapters/src/speech.rs crates/model-adapters/src/lib.rs crates/model-adapters/src/mock.rs crates/runtime/src/lib.rs crates/runtime/tests/cancellation.rs crates/runtime/tests/turn_flow.rs
git commit -m "feat: add typed synthesized audio"
```

---

### Task 2: macOS System-Speech Reference Adapter

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/model-adapters/Cargo.toml`
- Create: `crates/model-adapters/src/macos_system_speech.rs`
- Modify: `crates/model-adapters/src/lib.rs`
- Create: `crates/model-adapters/tests/macos_system_speech.rs`

**Interfaces:**
- Consumes: `SpeechRequest`, `SynthesizedAudio`, `AudioFormat`, `AdapterError`, and `CancellationToken`.
- Produces: `MacOsSystemSpeechConfig` and `MacOsSystemSpeechSynthesizer`.

- [ ] **Step 1: Add process-test dependencies**

Enable Tokio `fs`, `process`, and `signal` features in the workspace dependency. Add `tempfile = "3"` to workspace dependencies and to `conversation-model-adapters`.

- [ ] **Step 2: Write failing configuration tests**

Create `crates/model-adapters/tests/macos_system_speech.rs` with tests requiring:

```rust
assert!(MacOsSystemSpeechConfig::new("relative/say").is_err());
assert!(MacOsSystemSpeechConfig::new("/absolute/say")
    .unwrap()
    .with_voice("bad\nvoice")
    .is_err());
assert!(MacOsSystemSpeechConfig::new("/absolute/say")
    .unwrap()
    .with_rate(0)
    .is_err());
assert!(MacOsSystemSpeechConfig::new("/absolute/say")
    .unwrap()
    .with_max_text_bytes(0)
    .is_err());
```

Also assert that `system_default()` resolves to `/usr/bin/say` on macOS and returns a platform error elsewhere.

- [ ] **Step 3: Run configuration tests and verify RED**

Run:

```bash
cargo test -p conversation-model-adapters --test macos_system_speech --locked
```

Expected: compilation fails because the configuration and adapter types do not exist.

- [ ] **Step 4: Implement validated configuration**

Implement defaults:

```rust
const DEFAULT_MAX_TEXT_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_AUDIO_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_STDERR_BYTES: usize = 8 * 1024;
```

`MacOsSystemSpeechConfig` stores an absolute executable path, optional voice, optional non-zero words-per-minute rate, non-zero text/audio/stderr caps, and an absolute temporary directory. Builder methods validate immediately and never include rejected values in errors.

- [ ] **Step 5: Write failing direct-command and success tests**

On Unix, create an owner-executable fake `say` script under a `tempfile::TempDir`. The script parses `-o`, optional `-v`, optional `-r`, and `--`, records each argument to a sibling capture file, and writes `FORM-fake-aiff` to the output path.

Use text containing shell metacharacters:

```rust
let marker = temp_dir.path().join("must-not-exist");
let text = format!("hello; touch {}", marker.display());
```

Assert synthesis returns `AudioFormat::Aiff`, returns the fixture bytes, records the text as one argument, does not create `marker`, and leaves no generated audio file in the configured temporary directory.

- [ ] **Step 6: Run success test and verify RED**

Run:

```bash
cargo test -p conversation-model-adapters --test macos_system_speech synthesizes_aiff_without_shell_interpretation --locked
```

Expected: compilation fails because `SpeechSynthesizer` is not implemented for `MacOsSystemSpeechSynthesizer`.

- [ ] **Step 7: Implement bounded synthesis**

Create a unique owner-only `.aiff` file with `tempfile::Builder`. Build the child process exactly as:

```rust
let mut command = tokio::process::Command::new(config.executable());
command.arg("-o").arg(output.path());
if let Some(voice) = config.voice() {
    command.arg("-v").arg(voice);
}
if let Some(rate) = config.rate() {
    command.arg("-r").arg(rate.to_string());
}
command.arg("--").arg(request.text());
```

Set stdin/stdout to null, stderr to piped, and `kill_on_drop(true)`. Read stderr concurrently while retaining at most the configured prefix. Use a biased `tokio::select!`: cancellation calls `child.kill().await`, while normal completion awaits `child.wait()`. Await the stderr reader in both paths.

After a successful exit, read at most `max_audio_bytes + 1`; reject empty or oversized output. Return `SynthesizedAudio::new(bytes, AudioFormat::Aiff)`. Rely on the temporary-file guard for cleanup on every return path.

- [ ] **Step 8: Run success tests and verify GREEN**

Run:

```bash
cargo test -p conversation-model-adapters --test macos_system_speech --locked
```

Expected: configuration, argument, and successful synthesis tests pass.

- [ ] **Step 9: Write failing failure and cancellation tests**

Add fake scripts for:

- successful exit with empty output;
- successful exit with one byte over the configured audio cap;
- non-zero exit with stderr larger than the configured stderr cap;
- long-running synthesis that records its PID and output path.

For cancellation, wait for the PID marker, cancel the token, await the adapter, assert the error is `speech synthesis cancelled`, assert `/bin/kill -0 <pid>` fails, and assert the temporary output directory is empty.

- [ ] **Step 10: Run failure tests and verify RED**

Run:

```bash
cargo test -p conversation-model-adapters --test macos_system_speech rejects_ --locked
cargo test -p conversation-model-adapters --test macos_system_speech cancellation_ --locked
```

Expected: at least the empty, oversized, bounded-stderr, or cancellation assertion fails until all branches are implemented.

- [ ] **Step 11: Implement distinct sanitized failures**

Use stable error prefixes:

- `invalid macOS system speech configuration`
- `failed to start speech synthesis`
- `speech synthesis cancelled`
- `speech synthesis process failed`
- `speech synthesis output was empty`
- `speech synthesis output exceeded the configured limit`
- `failed to read speech synthesis output`

Include only bounded sanitized stderr for non-zero exits. Never include request text, output bytes, or temporary paths.

- [ ] **Step 12: Run adapter tests and verify GREEN**

Run:

```bash
cargo test -p conversation-model-adapters --locked
```

Expected: all adapter unit and integration tests pass without invoking `/usr/bin/say`.

- [ ] **Step 13: Commit the reference adapter scope**

```bash
git add Cargo.toml Cargo.lock crates/model-adapters/Cargo.toml crates/model-adapters/src/lib.rs crates/model-adapters/src/macos_system_speech.rs crates/model-adapters/tests/macos_system_speech.rs
git commit -m "feat: add macOS system speech adapter"
```

---

### Task 3: Typed-Text-to-Audio Probe

**Files:**
- Modify: `Cargo.toml`
- Create: `tests/tts/Cargo.toml`
- Create: `tests/tts/src/main.rs`
- Create: `tests/tts/tests/probe_cli.rs`
- Create: `tests/tts/README.md`

**Interfaces:**
- Consumes: `MacOsSystemSpeechConfig`, `MacOsSystemSpeechSynthesizer`, `SpeechRequest`, `SynthesizedAudio`, and `CancellationToken`.
- Produces: `conversation-tts-probe` with typed input, optional output persistence, optional playback, and structured timing.

- [ ] **Step 1: Write failing argument tests**

Define this CLI:

```text
conversation-tts-probe [--no-play] [--output <absolute-path>] [text...]
```

If `text...` is absent, read one non-empty text value from standard input. Reject an empty input, a relative output path, duplicate flags, missing output values, and control characters in environment voice configuration.

Require these environment variables:

```text
CONVERSATION_TTS_VOICE
CONVERSATION_TTS_RATE
CONVERSATION_TTS_TIMEOUT_MS
CONVERSATION_TTS_SAY_PATH
CONVERSATION_TTS_PLAYER_PATH
```

The executable overrides must be absolute. The default timeout is 30 seconds.

- [ ] **Step 2: Run CLI argument tests and verify RED**

Run:

```bash
cargo test -p conversation-tts-probe --locked
```

Expected: Cargo reports that the package does not exist.

- [ ] **Step 3: Implement package and argument parsing**

Add `tests/tts` to the workspace. Implement `ProbeArguments { text, output, play }`, environment parsing, and structured failure reporting with `status=error`, `stage`, `elapsed_ms`, and sanitized `error`.

- [ ] **Step 4: Run argument tests and verify GREEN**

Run:

```bash
cargo test -p conversation-tts-probe --locked
```

Expected: argument and configuration tests pass.

- [ ] **Step 5: Write failing playback cleanup tests**

Add an internal `PlayerConfig` with an absolute executable and temporary directory. Use a fake player script that records its PID and received audio path.

Test:

- successful playback receives one absolute `.aiff` path and the file is removed after exit;
- cancellation kills and awaits the player and removes the playback file;
- non-zero playback reports `audio playback process failed` without including audio bytes;
- `--no-play` never starts the player;
- `--output` writes the returned bytes to the exact caller-supplied absolute path.

- [ ] **Step 6: Run playback tests and verify RED**

Run:

```bash
cargo test -p conversation-tts-probe playback_ --locked
```

Expected: compilation fails because playback does not exist.

- [ ] **Step 7: Implement bounded playback and persistence**

For playback, create an owner-only temporary `.aiff`, write and flush `SynthesizedAudio::bytes()`, then invoke the player directly with the file path as its only argument. Use `kill_on_drop(true)`, biased cancellation, `child.kill().await`, and bounded stderr capture.

For `--output`, use `tokio::fs::write` only after synthesis succeeds. Never create a persistent file implicitly.

- [ ] **Step 8: Run playback tests and verify GREEN**

Run:

```bash
cargo test -p conversation-tts-probe --locked
```

Expected: all probe tests pass without invoking `/usr/bin/say`, `/usr/bin/afplay`, or audio hardware.

- [ ] **Step 9: Write failing end-to-end fake-process test**

Run the built probe with fake `say` and player paths, argument text containing shell metacharacters, and a bounded timeout. Assert:

```text
status=ok
format=aiff
encoded_bytes=<fixture length>
synthesis_completed_ms=<non-negative integer>
playback_launched_ms=<non-negative integer>
```

Assert the player ran, no shell marker was created, and no temporary audio remains.

- [ ] **Step 10: Run the end-to-end test and verify RED**

Run:

```bash
cargo test -p conversation-tts-probe --test probe_cli --locked
```

Expected: the success report or full cleanup assertion fails until orchestration is complete.

- [ ] **Step 11: Implement probe orchestration and deadline cancellation**

Build system defaults from `/usr/bin/say` and `/usr/bin/afplay` on macOS. Start one monitor task that cancels the shared token on the configured deadline or `tokio::signal::ctrl_c()`. Await synthesis and playback cleanup directly; never wrap them in a timeout that drops their futures. Abort the monitor only after all owned work completes.

Report synthesis completion separately from playback launch and explicitly avoid naming either value “first audible audio.”

- [ ] **Step 12: Run probe tests and verify GREEN**

Run:

```bash
cargo test -p conversation-tts-probe --locked
```

Expected: all probe tests pass.

- [ ] **Step 13: Commit the probe scope**

```bash
git add Cargo.toml Cargo.lock tests/tts
git commit -m "feat: add local TTS playback probe"
```

---

### Task 4: Documentation, Live Audio, and Release Gate

**Files:**
- Modify: `README.md`
- Modify: `ROADMAP.md`
- Modify: `docs/architecture.md`
- Modify: `docs/model-benchmarks.md`
- Modify: `docs/superpowers/specs/2026-07-27-local-tts-vertical-slice-design.md`

**Interfaces:**
- Consumes: completed adapter and probe behavior.
- Produces: public-neutral usage instructions, measured local plumbing evidence, and a clean feature branch.

- [ ] **Step 1: Update public-neutral documentation**

Document:

```bash
cargo run --locked -p conversation-tts-probe -- \
  "This is a local system-speech reference adapter."
```

Explain `--no-play`, absolute `--output`, generic environment configuration, macOS-only system defaults, and replacement through `SpeechSynthesizer`. State that the reference path is not a neural TTS recommendation.

Update the roadmap to mark typed text-to-audio plumbing complete while leaving phrase streaming, first audible audio, neural evaluation, ASR, microphone input, and barge-in pending.

- [ ] **Step 2: Record the reviewed cancellation correction**

Update the design and architecture documents to state that the runtime awaits a cancellation-aware speech adapter so subprocess cleanup can finish before terminal cancellation publication.

- [ ] **Step 3: Run the deterministic release gate**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
git diff --check
```

Expected: all commands exit zero.

- [ ] **Step 4: Run bounded real synthesis without playback**

Run:

```bash
cargo run --locked -p conversation-tts-probe -- \
  --no-play \
  --output /tmp/conversation-runtime-reference.aiff \
  "Conversation Runtime local speech synthesis is working."
```

Verify the report identifies AIFF bytes and synthesis completion without claiming first audible audio. Verify `/usr/bin/file /tmp/conversation-runtime-reference.aiff` reports AIFF, then remove the file.

- [ ] **Step 5: Run one audible macOS probe**

Run:

```bash
cargo run --locked -p conversation-tts-probe -- \
  "Conversation Runtime local speech playback is working."
```

Expected: the Mac plays the sentence once, the report distinguishes synthesis completion from playback launch, and no temporary audio remains.

- [ ] **Step 6: Review public neutrality**

Search public files for venture-selected model or voice language and specific identifiers outside retained benchmark evidence:

```bash
grep -RInE 'preferred model|preferred voice|product default|venture.*(model|voice|routing)' README.md ROADMAP.md docs models tests crates
```

Expected: only explicit neutrality statements or historical benchmark labels remain.

- [ ] **Step 7: Commit the validated documentation**

```bash
git add README.md ROADMAP.md docs
git commit -m "docs: document local speech reference flow"
```

- [ ] **Step 8: Push and open the conventional pull request**

Push `feature/local-tts-vertical-slice`, open a ready pull request with a concise summary and exact validation commands, wait for checks, address review findings, squash merge to `master`, and delete the merged feature branch locally and remotely.
