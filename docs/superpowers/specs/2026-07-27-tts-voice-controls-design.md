# TTS Voice Controls and Profiles Design

## Problem

The macOS speech probe supports voice and rate configuration, but only through environment variables. A user cannot discover installed voices, select a voice from the command itself, or save named TTS setups that a consuming application can switch without code changes.

## Options Considered

### Exact voice names with discovery

Add `--voice <name>`, `--rate <words-per-minute>`, and `--list-voices`. Voice names map directly to the installed macOS system voices.

This is the selected approach because it is explicit, deterministic, and does not claim that locale, accent, gender, or quality can be inferred beyond information reported by macOS.

### Locale-based selection

Add `--locale en_GB` and choose an installed voice automatically. This is not selected because multiple voices can share a locale and an automatic choice would be unstable across machines.

### Friendly accent aliases

Add aliases such as `--accent british`. This is not selected because accent labels are subjective, incomplete, and do not map reliably to system voice identifiers.

## Command Interface

The existing typed-text flow remains unchanged:

```bash
cargo run --locked -p conversation-tts-probe -- "Hello."
```

The probe adds:

```text
--voice <name>       Select an exact installed macOS voice
--rate <wpm>         Set a non-zero speaking rate in words per minute
--config <path>      Load named TTS profiles from an absolute TOML path
--profile <id>       Select one profile from the configured file
--list-voices        Print the voices reported by macOS and exit
--help               Print usage and exit
```

Examples:

```bash
cargo run --locked -p conversation-tts-probe -- \
  --voice "Daniel" \
  --rate 190 \
  "Hello with a British English system voice."

cargo run --locked -p conversation-tts-probe -- --list-voices

cargo run --locked -p conversation-tts-probe -- \
  --config /absolute/path/to/speech.toml \
  --profile mandarin \
  "你好，这是本地中文语音。"
```

Command-line values override environment variables, environment variables override the selected profile, and the selected profile overrides system defaults. The environment variables remain supported for automation and backward compatibility.

## Named TTS Profiles

The reference probe accepts a versioned TOML file:

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

`--config` must be an absolute path and the file must not exceed 64 KiB. `--profile` requires `--config`; when omitted, the file's `default_profile` is selected. Profile identifiers are user-defined.

Stage one supports only `backend = "macos-system"`. Unknown backends, schema versions, profiles, and fields are rejected explicitly. The project does not expose configuration for a neural backend until that backend can actually synthesize and pass cancellation, output-bound, licensing, provenance, and local benchmark checks.

The loader belongs to the reference probe rather than the core protocol. Consuming applications may use their own configuration representation while preserving the same adapter boundary.

## Voice and Accent Semantics

The selected voice determines the locale and accent characteristics available from macOS. The SDK does not expose a separate accent control and does not classify voices by gender or quality.

`--list-voices` reports the system-provided voice name, locale, and sample text without maintaining a hardcoded registry. Availability can differ by machine and installed macOS voice downloads.

The default remains the user's current macOS system voice. The project does not prescribe a product voice.

## Downloadable Neural TTS Boundary

Downloaded neural TTS implementations are separate `SpeechSynthesizer` adapters. The public SDK does not bundle model weights or hardcode one provider. A future neural profile can select an installed adapter, exact model identifier and revision, local endpoint or model directory, language, voice profile, and consented reference-audio identity.

The next neural-TTS milestone must first benchmark candidate implementations on the target Apple Silicon machine. Qwen3-TTS 0.6B and CosyVoice 3 0.5B are evaluation candidates, not SDK recommendations. Model files stay in a user-controlled location outside Git, and registry entries record provenance, digest, license review, and benchmark status.

Voice cloning requires explicit consent and provenance. The repository must not contain private reference audio, reusable clone embeddings, or model weights.

## Errors and Safety

- Missing values for `--voice` or `--rate` produce a concise usage error.
- Empty voice names and zero or non-numeric rates are rejected.
- Relative or oversized config files, unsupported schema versions, unknown profiles, unsupported backends, and unknown profile fields are rejected.
- Unknown arguments are rejected rather than treated as speech text.
- `--list-voices` does not synthesize, play, or persist audio.
- Existing absolute-output-path, timeout, cancellation, bounded-output, and temporary-file cleanup behavior remains unchanged.

## Testing

- Parser tests cover voice, rate, config, profile, precedence, missing values, invalid values, help, and unknown options.
- Profile tests cover default and selected profiles, multilingual voice names, unsupported schemas and backends, unknown fields, and malformed TOML.
- A CLI test verifies that `--list-voices` prints injected system output without starting synthesis or playback.
- Existing TTS adapter and probe tests continue to pass.
- Full workspace formatting, linting, and tests must pass before merge.
