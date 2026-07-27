# Local Neural TTS HTTP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a tested, provider-neutral local neural TTS path through an OpenAI-compatible HTTP endpoint while retaining macOS system speech as fallback.

**Architecture:** `conversation-model-adapters` gains a bounded WAV-producing HTTP synthesizer. The existing TTS probe resolves backend-tagged profiles into either the macOS process adapter or the HTTP adapter, while playback and persistence remain backend-independent.

**Tech Stack:** Rust 2021, Reqwest 0.13, Tokio, Tokio cancellation tokens, Serde, Serde JSON, TOML, deterministic loopback TCP fixtures.

## Global Constraints

- Public protocol and runtime crates contain no MLX-Audio, Qwen, model, voice, or HTTP request types.
- Default HTTP binding is loopback and redirects are disabled.
- Request text, response audio, and HTTP error bodies are bounded.
- Cancellation must win and all owned work must be released before returning.
- The adapter returns complete WAV in this milestone; streaming audio remains separate.
- macOS system speech remains available as a zero-download fallback.
- Exact model revisions, digests, licenses, and measurements are evidence, not SDK defaults.

---

### Task 1: Bounded HTTP Speech Adapter

**Files:**
- Create: `crates/model-adapters/src/openai_compatible_speech.rs`
- Create: `crates/model-adapters/tests/openai_compatible_speech.rs`
- Modify: `crates/model-adapters/src/lib.rs`
- Modify: `crates/model-adapters/src/speech.rs`

**Interfaces:**
- Consumes: `SpeechSynthesizer`, `SpeechRequest`, `SynthesizedAudio`, `AdapterFuture`, `AdapterError`.
- Produces: `OpenAiCompatibleSpeechConfig`, `OpenAiCompatibleSpeechSynthesizer`, and `AudioFormat::Wav`.

- [ ] **Step 1: Write failing serialization and WAV tests**

Add loopback TCP tests that construct:

```rust
let speech = OpenAiCompatibleSpeechSynthesizer::new(
    OpenAiCompatibleSpeechConfig::new("local-model")
        .unwrap()
        .with_endpoint(server.endpoint_with_base_path("/v1"))
        .unwrap()
        .with_voice("local-voice")
        .unwrap()
        .with_speed(1.1)
        .unwrap()
        .with_language("Chinese")
        .unwrap()
        .with_instructions("Warm and calm.")
        .unwrap()
        .with_max_tokens(128)
        .unwrap()
        .with_repetition_penalty(1.05)
        .unwrap(),
);
```

Assert the request target is `/v1/audio/speech`, the literal JSON fields are `model`, `input`, `voice`, `speed`, `lang_code`, `instruct`, `max_tokens`, `repetition_penalty`, and `response_format = "wav"`, and the returned bytes are typed as `AudioFormat::Wav`.

- [ ] **Step 2: Run tests and verify RED**

Run:

```bash
rustup run 1.97.1 cargo test --locked -p conversation-model-adapters --test openai_compatible_speech
```

Expected: compilation fails because the HTTP speech types and WAV format do not exist.

- [ ] **Step 3: Implement validated configuration and success path**

Create:

```rust
pub struct OpenAiCompatibleSpeechConfig {
    endpoint: reqwest::Url,
    model: String,
    voice: Option<String>,
    speed: Option<f32>,
    language: Option<String>,
    instructions: Option<String>,
    max_tokens: Option<usize>,
    repetition_penalty: Option<f32>,
    max_text_bytes: usize,
    max_audio_bytes: usize,
}

pub struct OpenAiCompatibleSpeechSynthesizer {
    client: reqwest::Client,
    config: OpenAiCompatibleSpeechConfig,
}
```

Use `http://127.0.0.1:8000/v1` as the default endpoint, preserve configured base paths, clear query and fragment, append `/audio/speech`, disable redirects, serialize the request privately, and return a non-empty bounded WAV.

- [ ] **Step 4: Run focused tests and verify GREEN**

Run the Task 1 test command and expect all current tests in the file to pass.

- [ ] **Step 5: Add failing safety tests**

Add independent tests proving:

- empty and oversized text produce no server request;
- redirects return an HTTP failure and never forward text;
- successful audio exceeding the configured cap is rejected;
- empty successful audio is rejected;
- failure bodies are truncated at 4 KiB and contain no input text;
- cancellation resolves a stalled request as `speech synthesis cancelled`.

- [ ] **Step 6: Run safety tests and verify RED**

Run the Task 1 test command and confirm each new branch fails because the protection is absent.

- [ ] **Step 7: Implement bounded errors, output, and cancellation**

Read response chunks with a biased cancellation select. Track total bytes before extending the buffer. For non-success status, read only a 4 KiB prefix and append `[truncated]` when additional bytes exist. Use stable sanitized errors and never include `SpeechRequest::text()`.

- [ ] **Step 8: Run adapter tests and verify GREEN**

Run:

```bash
rustup run 1.97.1 cargo test --locked -p conversation-model-adapters
```

Expected: all adapter unit and integration tests pass.

- [ ] **Step 9: Commit adapter scope**

```bash
git add crates/model-adapters/src/openai_compatible_speech.rs crates/model-adapters/tests/openai_compatible_speech.rs crates/model-adapters/src/lib.rs crates/model-adapters/src/speech.rs
git commit -m "feat: add local HTTP speech adapter"
```

### Task 2: Backend-Aware TTS Profiles

**Files:**
- Modify: `tests/tts/src/profile.rs`
- Modify: `tests/tts/src/main.rs`
- Modify: `tests/tts/tests/probe_cli.rs`
- Modify: `configs/speech.example.toml`

**Interfaces:**
- Consumes: Task 1 `OpenAiCompatibleSpeechConfig`, `OpenAiCompatibleSpeechSynthesizer`, `AudioFormat::Wav`.
- Produces: a backend-tagged `SpeechProfile` and one existing probe capable of macOS AIFF or HTTP WAV.

- [ ] **Step 1: Write failing profile tests**

Replace the flat test-only expectation with:

```rust
enum SpeechProfile {
    MacOsSystem {
        voice: Option<String>,
        rate_wpm: Option<u32>,
    },
    OpenAiCompatible {
        endpoint: String,
        model: String,
        voice: Option<String>,
        speed: Option<f32>,
        language: Option<String>,
        instructions: Option<String>,
        max_tokens: Option<usize>,
        repetition_penalty: Option<f32>,
    },
}
```

Add tests for one valid local HTTP profile and rejections for missing model, invalid endpoint, empty voice/language/instructions, non-positive speed, zero generation-token limit, non-positive repetition penalty, and backend-incompatible fields.

- [ ] **Step 2: Run profile tests and verify RED**

Run:

```bash
rustup run 1.97.1 cargo test --locked -p conversation-tts-probe profile::
```

Expected: tests fail because only `macos-system` profiles are deserializable.

- [ ] **Step 3: Implement tagged profile parsing**

Use Serde's backend tag with backend-specific raw structs, retain `deny_unknown_fields`, retain the 64 KiB file cap, and convert validated raw profiles to the internal `SpeechProfile` enum.

- [ ] **Step 4: Run profile tests and verify GREEN**

Run the profile test command and expect all profile tests to pass.

- [ ] **Step 5: Write failing probe backend tests**

Add a CLI integration test with a fake loopback speech server and a profile containing:

```toml
[profiles.local-neural]
backend = "openai-compatible"
endpoint = "http://127.0.0.1:PORT/v1"
model = "local-model"
voice = "local-voice"
language = "Chinese"
instructions = "Warm and calm."
speed = 1.0
max_tokens = 128
repetition_penalty = 1.05
```

Run the probe with `--no-play`, assert success, assert `format=wav`, and inspect the captured request JSON.

- [ ] **Step 6: Run probe integration test and verify RED**

Run:

```bash
rustup run 1.97.1 cargo test --locked -p conversation-tts-probe --test probe_cli
```

Expected: the neural profile is rejected or the probe still constructs only macOS speech.

- [ ] **Step 7: Implement synthesizer selection and WAV handling**

Resolve profile, environment, and CLI values only where meaningful for the selected backend. Construct a boxed `Arc<dyn SpeechSynthesizer>`. Map `AudioFormat::Aiff` to `.aiff` and `AudioFormat::Wav` to `.wav` for playback files and report output. Keep `--list-voices` restricted to macOS system speech.

- [ ] **Step 8: Run probe tests and verify GREEN**

Run:

```bash
rustup run 1.97.1 cargo test --locked -p conversation-tts-probe
```

Expected: all profile, argument, timeout, playback, and CLI tests pass.

- [ ] **Step 9: Commit probe scope**

```bash
git add tests/tts/src/profile.rs tests/tts/src/main.rs tests/tts/tests/probe_cli.rs configs/speech.example.toml
git commit -m "feat: support local neural TTS profiles"
```

### Task 3: Setup and Quality-Evaluation Documentation

**Files:**
- Modify: `README.md`
- Modify: `ROADMAP.md`
- Modify: `tests/tts/README.md`
- Modify: `crates/model-adapters/README.md`
- Create: `configs/speech.mlx-audio.example.toml`
- Create: `docs/neural-tts-evaluation.md`

**Interfaces:**
- Consumes: the Task 2 profile and probe commands.
- Produces: a reproducible user path and an evidence template without selecting a public default model.

- [ ] **Step 1: Document the runnable local flow**

Document:

```bash
uv tool install --force "mlx-audio[server]==0.4.6" --prerelease=allow
mlx_audio.server --host 127.0.0.1 --port 8000
rustup run 1.97.1 cargo run --locked -p conversation-tts-probe -- \
  --config "$PWD/configs/speech.mlx-audio.example.toml" \
  --profile local-neural-fast \
  "你好，这是本地神经语音测试。"
```

State that the first request downloads and loads model files into the model host's external cache, not the repository. Keep host binding at `127.0.0.1`.

Create `configs/speech.mlx-audio.example.toml` with two explicitly labeled evaluation candidates:

- `local-neural-fast`: `mlx-community/Qwen3-TTS-12Hz-0.6B-CustomVoice-bf16`, voice `vivian`;
- `local-neural-quality`: `mlx-community/Qwen3-TTS-12Hz-1.7B-CustomVoice-6bit`, voice `vivian`.

Both profiles use `language = "Chinese"`, `max_tokens = 128`, and `repetition_penalty = 1.05`. The quality profile includes a conversational delivery instruction. Comments must state that these are measured Apple Silicon candidates, not SDK defaults.

- [ ] **Step 2: Add objective evaluation gates**

Create an evidence template for exact server revision, model revision and digest, license review, voice/reference consent, machine profile, cold and warm synthesis latency, first playable audio, real-time factor, peak memory, generated duration, and English/Chinese quality notes. Label 0.6B and 1.7B names as candidates only.

- [ ] **Step 3: Update roadmap truthfully**

Mark deterministic local HTTP adapter work complete only after tests pass. Keep phrase-level streaming, first-audio measurement, integrated runtime speech, and model-quality selection pending.

- [ ] **Step 4: Validate documentation**

Run:

```bash
git diff --check
grep -RInE 'TBD|TODO|PLACEHOLDER' README.md ROADMAP.md tests/tts/README.md crates/model-adapters/README.md docs/neural-tts-evaluation.md
```

Expected: no whitespace errors and no placeholders.

- [ ] **Step 5: Commit documentation scope**

```bash
git add README.md ROADMAP.md tests/tts/README.md crates/model-adapters/README.md configs/speech.mlx-audio.example.toml docs/neural-tts-evaluation.md
git commit -m "docs: explain local neural TTS setup"
```

### Task 4: Workspace and Live Validation

**Files:**
- Modify after evidence exists: `docs/neural-tts-evaluation.md`

**Interfaces:**
- Consumes: all prior tasks and a locally running MLX-Audio server.
- Produces: deterministic workspace verification plus explicitly labeled live-model evidence.

- [ ] **Step 1: Run focused formatting and lint**

Run:

```bash
rustup run 1.97.1 cargo fmt --all -- --check
rustup run 1.97.1 cargo clippy --workspace --all-targets --locked -- -D warnings
```

Expected: both commands exit successfully with no warnings.

- [ ] **Step 2: Run the complete deterministic suite**

Run:

```bash
rustup run 1.97.1 cargo test --workspace --locked
```

Expected: every workspace test passes without a model server or network download.

- [ ] **Step 3: Start loopback MLX-Audio**

Install the server outside the repository if needed, then start:

```bash
mlx_audio.server --host 127.0.0.1 --port 8000
```

Confirm the server is reachable only through loopback.

- [ ] **Step 4: Run fast-candidate smoke test**

Run the `local-neural-fast` profile with one English and one Mandarin sentence, retain generated audio only in a temporary directory, listen to both, and record exact measured evidence.

- [ ] **Step 5: Evaluate quality candidate**

If memory and download cost are acceptable, run the `local-neural-quality` profile with the same sentences and compare latency, memory, pronunciation, prosody, stability, and subjective quality. Do not claim GPT Voice parity.

- [ ] **Step 6: Record evidence or explicit blocker**

Update `docs/neural-tts-evaluation.md` with measured results. If installation, download, runtime, or quality validation cannot complete, preserve deterministic test results and label live model validation blocked with the exact reason.

- [ ] **Step 7: Commit validation evidence**

```bash
git add docs/neural-tts-evaluation.md
git commit -m "docs: record local neural TTS evaluation"
```
