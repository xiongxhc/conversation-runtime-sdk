# TTS Voice Controls and Profiles Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the macOS speech probe discoverable and configurable through direct CLI controls and bounded, named TOML profiles without pretending that unimplemented neural TTS backends are available.

**Architecture:** Keep the core `SpeechSynthesizer` contract unchanged. Add CLI actions and a probe-local profile loader around the existing macOS adapter, with precedence `CLI > environment > selected profile > system defaults`. Treat downloadable neural models as separate adapters that reuse the profile-selection boundary only after implementation and benchmark validation.

**Tech Stack:** Rust 2024, Tokio, Serde, TOML, macOS `/usr/bin/say`, existing `conversation-model-adapters`.

## Global Constraints

- Public SDK content stays model-neutral; Qwen3-TTS and CosyVoice are evaluation candidates only.
- Stage one accepts only `backend = "macos-system"`.
- Config paths are absolute and config files are bounded to 64 KiB.
- Unknown fields, schema versions, profiles, backends, and CLI options fail explicitly.
- Model weights, private reference audio, and clone embeddings never enter Git.
- Existing cancellation, timeout, output-size, and temporary-file cleanup behavior remains unchanged.
- New behavior follows test-driven development: run each focused test red before production changes and green afterward.

---

## File Map

- `tests/tts/src/main.rs`: Orchestrate CLI actions, source precedence, synthesis, playback, and process exit.
- `tests/tts/src/profile.rs`: Parse and validate versioned named speech profiles.
- `tests/tts/Cargo.toml`: Add probe-local Serde and TOML dependencies.
- `tests/tts/tests/probe_cli.rs`: Verify terminal CLI behavior using injected fake executables.
- `configs/speech.example.toml`: Show system-default, British English, and Mandarin profiles.
- `tests/tts/README.md`: Document commands, precedence, multilingual profiles, and neural-backend boundary.
- `README.md`: Expose the simplest voice listing and profile commands.
- `ROADMAP.md`: Record the profile boundary and separate neural-TTS benchmark milestone.
- `models/registry.example.toml`: Clarify that neural speech entries stay disabled until implemented and reviewed.

### Task 1: Direct CLI Controls and Voice Discovery

**Files:**
- Modify: `tests/tts/src/main.rs`
- Modify: `tests/tts/tests/probe_cli.rs`

**Interfaces:**
- Consumes: `MacOsSystemSpeechConfig::with_voice`, `MacOsSystemSpeechConfig::with_rate`.
- Produces: `ProbeAction`, CLI voice/rate overrides, `--help`, and bounded `--list-voices`.

- [ ] **Step 1: Add failing parser tests**

Add tests that express the desired action model before changing parsing:

```rust
#[test]
fn parses_voice_rate_and_text() {
    let action = parse_arguments(
        [
            "conversation-tts-probe",
            "--voice",
            "Daniel",
            "--rate",
            "190",
            "hello",
        ],
        Cursor::new(""),
    )
    .unwrap();

    assert_eq!(
        action,
        ProbeAction::Run(ProbeArguments {
            text: "hello".to_owned(),
            output: None,
            play: true,
            voice: Some("Daniel".to_owned()),
            rate: Some(190),
            config_path: None,
            profile_id: None,
        })
    );
}

#[test]
fn parses_help_and_list_voices_without_text() {
    assert_eq!(
        parse_arguments(["probe", "--help"], Cursor::new("")).unwrap(),
        ProbeAction::Help
    );
    assert_eq!(
        parse_arguments(["probe", "--list-voices"], Cursor::new("")).unwrap(),
        ProbeAction::ListVoices
    );
}
```

Add table-driven failures for duplicate flags, missing values, zero/non-numeric rate, text combined with terminal actions, and unknown options.

- [ ] **Step 2: Run parser tests red**

Run:

```bash
cargo test --locked -p conversation-tts-probe parses_voice_rate_and_text
cargo test --locked -p conversation-tts-probe parses_help_and_list_voices_without_text
```

Expected: compilation failure because `ProbeAction` and the new fields do not exist.

- [ ] **Step 3: Implement the minimal action parser**

Introduce:

```rust
const USAGE: &str = "Usage: conversation-tts-probe [OPTIONS] [--] [TEXT ...]\n\
Options:\n\
  --voice <name>       Select an exact installed macOS voice\n\
  --rate <wpm>         Set a non-zero speaking rate\n\
  --config <path>      Load profiles from an absolute TOML file\n\
  --profile <id>       Select a configured profile\n\
  --list-voices        List installed macOS voices and exit\n\
  --no-play            Synthesize without playback\n\
  --output <path>      Persist AIFF to an absolute path\n\
  --help               Print this help";

#[derive(Debug, Eq, PartialEq)]
enum ProbeAction {
    Run(ProbeArguments),
    ListVoices,
    Help,
}
```

Parse `--voice`, `--rate`, `--config`, and `--profile` only while flag parsing is active. Preserve `--` for text beginning with dashes. Require terminal actions to appear without run-only options or text.

- [ ] **Step 4: Run parser tests green**

Run:

```bash
cargo test --locked -p conversation-tts-probe parses_voice_rate_and_text
cargo test --locked -p conversation-tts-probe parses_help_and_list_voices_without_text
cargo test --locked -p conversation-tts-probe rejects
```

Expected: all matching tests pass.

- [ ] **Step 5: Add a failing voice-list CLI test**

Extend `tests/tts/tests/probe_cli.rs` with a fake `say` executable that prints one voice for `-v ?` and writes a marker if synthesis arguments are received:

```rust
#[test]
fn lists_voices_without_starting_synthesis_or_playback() {
    let output = Command::new(env!("CARGO_BIN_EXE_conversation-tts-probe"))
        .arg("--list-voices")
        .env("CONVERSATION_TTS_SAY_PATH", fake_say)
        .env("CONVERSATION_TTS_PLAYER_PATH", unused_player)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "Tingting zh_CN # 你好\n");
    assert!(!synthesis_marker.exists());
    assert!(!playback_marker.exists());
}
```

- [ ] **Step 6: Run the voice-list test red**

Run:

```bash
cargo test --locked -p conversation-tts-probe --test probe_cli lists_voices_without_starting_synthesis_or_playback
```

Expected: failure because `--list-voices` is not dispatched.

- [ ] **Step 7: Implement bounded voice listing and help dispatch**

Add `list_voices(executable: &Path) -> Result<Vec<u8>, String>` using `tokio::process::Command` with arguments `-v` and `?`, null stdin, piped stdout/stderr, `kill_on_drop(true)`, and a 64 KiB stdout bound. Reject non-zero exit and oversized output. In `main`, print `USAGE` for help and print the returned voice bytes for listing before constructing synthesis configuration.

- [ ] **Step 8: Run Task 1 tests green**

Run:

```bash
cargo test --locked -p conversation-tts-probe
```

Expected: all probe unit and integration tests pass.

- [ ] **Step 9: Commit Task 1**

```bash
git add tests/tts/src/main.rs tests/tts/tests/probe_cli.rs
git commit -m "feat: add TTS voice CLI controls"
```

### Task 2: Versioned Named Speech Profiles

**Files:**
- Create: `tests/tts/src/profile.rs`
- Modify: `tests/tts/src/main.rs`
- Modify: `tests/tts/Cargo.toml`
- Create: `configs/speech.example.toml`

**Interfaces:**
- Consumes: CLI `config_path`, `profile_id`, `voice`, and `rate`.
- Produces: `SpeechProfile::load(path, selected_id)` returning validated optional voice and rate.

- [ ] **Step 1: Add Serde and TOML dependencies**

Add workspace-compatible probe dependencies:

```toml
serde = { version = "1", features = ["derive"] }
toml = "0.8"
```

- [ ] **Step 2: Write failing profile tests**

Create `tests/tts/src/profile.rs` with tests declared before implementation:

```rust
#[test]
fn loads_default_and_selected_multilingual_profiles() {
    let file = write_profile(
        r#"
schema_version = 1
default_profile = "british"

[profiles.british]
backend = "macos-system"
voice = "Daniel"
rate_wpm = 190

[profiles.mandarin]
backend = "macos-system"
voice = "Tingting"
rate_wpm = 180
"#,
    );

    assert_eq!(
        SpeechProfile::load(file.path(), None).unwrap(),
        SpeechProfile {
            voice: Some("Daniel".to_owned()),
            rate_wpm: Some(190),
        }
    );
    assert_eq!(
        SpeechProfile::load(file.path(), Some("mandarin")).unwrap().voice,
        Some("Tingting".to_owned())
    );
}
```

Add separate failures for a relative path, files over 64 KiB, malformed TOML, schema version `2`, unknown fields, missing default profile, unknown selected profile, unsupported backend, empty voice, and zero rate.

- [ ] **Step 3: Run profile tests red**

Run:

```bash
cargo test --locked -p conversation-tts-probe profile::tests
```

Expected: compilation failure because `SpeechProfile` does not exist.

- [ ] **Step 4: Implement the bounded profile loader**

Use private deserialization types with strict fields:

```rust
const MAX_PROFILE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpeechProfilesFile {
    schema_version: u32,
    default_profile: String,
    profiles: BTreeMap<String, RawSpeechProfile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpeechProfile {
    backend: SpeechBackend,
    voice: Option<String>,
    rate_wpm: Option<u32>,
}

#[derive(Debug, Deserialize)]
enum SpeechBackend {
    #[serde(rename = "macos-system")]
    MacOsSystem,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SpeechProfile {
    pub(crate) voice: Option<String>,
    pub(crate) rate_wpm: Option<u32>,
}
```

Check metadata length before reading, use `std::fs::read_to_string`, require schema version `1`, resolve the explicit or default profile, and validate voice/rate using the same constraints as the adapter.

- [ ] **Step 5: Run profile tests green**

Run:

```bash
cargo test --locked -p conversation-tts-probe profile::tests
```

Expected: all profile tests pass.

- [ ] **Step 6: Write failing precedence tests**

Add tests around a pure resolver:

```rust
#[test]
fn resolves_cli_over_environment_over_profile() {
    let resolved = resolve_speech_settings(
        SpeechProfile {
            voice: Some("Tingting".to_owned()),
            rate_wpm: Some(180),
        },
        Some("Daniel".to_owned()),
        Some(190),
        Some("Samantha".to_owned()),
        Some(200),
    )
    .unwrap();

    assert_eq!(resolved.voice.as_deref(), Some("Samantha"));
    assert_eq!(resolved.rate_wpm, Some(200));
}
```

The parameter order is profile voice/rate, environment voice/rate, then CLI voice/rate.

- [ ] **Step 7: Run precedence test red**

Run:

```bash
cargo test --locked -p conversation-tts-probe resolves_cli_over_environment_over_profile
```

Expected: compilation failure because `resolve_speech_settings` does not exist.

- [ ] **Step 8: Apply resolved settings to the existing adapter**

Load the selected profile only for `ProbeAction::Run`, parse environment values, apply the pure precedence resolver, then call `with_voice` and `with_rate` once on `MacOsSystemSpeechConfig`. Preserve existing executable, player, timeout, and cancellation setup.

- [ ] **Step 9: Add the example profile file**

Create `configs/speech.example.toml` exactly as:

```toml
schema_version = 1
default_profile = "system-default"

[profiles.system-default]
backend = "macos-system"

[profiles.british-english]
backend = "macos-system"
voice = "Daniel"
rate_wpm = 190

[profiles.mandarin]
backend = "macos-system"
voice = "Tingting"
rate_wpm = 180
```

- [ ] **Step 10: Run Task 2 tests green**

Run:

```bash
cargo test --locked -p conversation-tts-probe
```

Expected: all probe tests pass, including profile and precedence coverage.

- [ ] **Step 11: Commit Task 2**

```bash
git add Cargo.lock tests/tts/Cargo.toml tests/tts/src/main.rs tests/tts/src/profile.rs configs/speech.example.toml
git commit -m "feat: add named TTS profiles"
```

### Task 3: Public Documentation and Neural Adapter Boundary

**Files:**
- Modify: `README.md`
- Modify: `tests/tts/README.md`
- Modify: `ROADMAP.md`
- Modify: `models/registry.example.toml`

**Interfaces:**
- Consumes: tested CLI and profile behavior from Tasks 1 and 2.
- Produces: copy-paste setup instructions and an honest neural-TTS roadmap boundary.

- [ ] **Step 1: Document current commands**

Add copy-paste examples for:

```bash
cargo run --locked -p conversation-tts-probe -- --list-voices

cargo run --locked -p conversation-tts-probe -- \
  --voice "Tingting" \
  --rate 180 \
  "你好，这是本地中文语音。"

cargo run --locked -p conversation-tts-probe -- \
  --config "$PWD/configs/speech.example.toml" \
  --profile mandarin \
  "你好，这是命名语音配置。"
```

State that downloaded Apple voices become visible after installation, profile availability differs by machine, and macOS voice selection does not provide arbitrary cloning.

- [ ] **Step 2: Document configuration precedence and boundaries**

Record:

```text
CLI > environment > selected profile > macOS system defaults
```

State that only `macos-system` is accepted now. Describe downloadable neural TTS as a separate adapter milestone requiring exact model revision, digest, license review, consent provenance, cancellation, bounded output, and Apple Silicon benchmarks.

- [ ] **Step 3: Tighten the example model registry**

Keep the speech synthesizer disabled and add neutral metadata keys:

```toml
provenance = "https://example.invalid/model-card"
digest = "not-recorded"
license_status = "review-required"
benchmark_status = "not-run"
enabled = false
```

Do not name a venture model in the example registry.

- [ ] **Step 4: Verify documentation consistency**

Run:

```bash
grep -R "CONVERSATION_TTS_VOICE" README.md tests/tts/README.md docs configs models
grep -R "backend = \"macos-system\"" README.md tests/tts/README.md docs configs
git diff --check
```

Expected: all user-facing control paths are documented, the supported backend is named consistently, and no whitespace errors are reported.

- [ ] **Step 5: Commit Task 3**

```bash
git add README.md tests/tts/README.md ROADMAP.md models/registry.example.toml
git commit -m "docs: explain configurable TTS profiles"
```

### Task 4: Full Verification

**Files:**
- Verify: entire workspace

**Interfaces:**
- Consumes: all implementation and documentation changes.
- Produces: fresh evidence that the branch is ready for review.

- [ ] **Step 1: Format the workspace**

Run:

```bash
cargo fmt --all
cargo fmt --all -- --check
```

Expected: formatting check exits successfully.

- [ ] **Step 2: Run strict linting**

Run:

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Expected: no warnings or errors.

- [ ] **Step 3: Run all tests**

Run:

```bash
cargo test --workspace --locked
```

Expected: all unit, integration, and documentation tests pass.

- [ ] **Step 4: Exercise real local discovery**

Run:

```bash
cargo run --locked -p conversation-tts-probe -- --list-voices
```

Expected: installed macOS voices print and the command exits without synthesizing audio.

- [ ] **Step 5: Exercise a silent Mandarin profile**

Run:

```bash
cargo run --locked -p conversation-tts-probe -- \
  --config "$PWD/configs/speech.example.toml" \
  --profile mandarin \
  --no-play \
  "你好，这是本地中文语音配置测试。"
```

Expected: `status=ok`, AIFF synthesis completes, no playback starts, and temporary files are cleaned.

- [ ] **Step 6: Review the final diff**

Run:

```bash
git status --short
git diff master...HEAD --stat
git diff master...HEAD --check
```

Expected: only scoped TTS controls, profiles, tests, and documentation differ from `master`.
