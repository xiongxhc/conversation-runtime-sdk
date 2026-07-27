# macOS System-Speech Probe

`conversation-tts-probe` turns typed text into AIFF through the macOS system-speech reference adapter. By default it plays the result through the macOS system player and removes all temporary audio after playback.

Run the audible reference flow:

```bash
cargo run --locked -p conversation-tts-probe -- \
  "This is a local system-speech reference adapter."
```

List installed macOS voices without synthesizing or playing audio:

```bash
cargo run --locked -p conversation-tts-probe -- --list-voices
```

Select an installed voice and speaking rate directly:

```bash
cargo run --locked -p conversation-tts-probe -- \
  --voice "Tingting" \
  --rate 180 \
  "你好，这是本地中文语音。"
```

Select a named profile from an absolute TOML configuration file:

```bash
cargo run --locked -p conversation-tts-probe -- \
  --config "$PWD/configs/speech.example.toml" \
  --profile mandarin \
  "你好，这是命名语音配置。"
```

Downloaded Apple voices become visible after installation, and profile availability differs by machine. macOS voice selection does not provide arbitrary voice cloning.

Synthesize without playback and retain an explicit output:

```bash
cargo run --locked -p conversation-tts-probe -- \
  --no-play \
  --output /tmp/conversation-runtime-reference.aiff \
  "This output path was explicitly requested."
```

If text arguments are omitted, the probe reads a non-empty value from standard input. `--output` must be absolute. Optional overrides are `CONVERSATION_TTS_VOICE`, `CONVERSATION_TTS_RATE`, `CONVERSATION_TTS_TIMEOUT_MS`, `CONVERSATION_TTS_SAY_PATH`, and `CONVERSATION_TTS_PLAYER_PATH`.

Configuration precedence for voice and rate is:

```text
CLI > environment > selected profile > macOS system defaults
```

Only `backend = "macos-system"` is accepted now. `--profile` requires `--config`; if no profile is selected, the configuration file's `default_profile` is used. Configuration paths must be absolute, and configuration files are bounded to 64 KiB.

The report distinguishes synthesis completion from playback launch. Neither value is a measurement of first audible audio. The system-speech path is a replaceable reference adapter, not a neural TTS recommendation or application voice selection.

Downloadable neural TTS is a separate adapter milestone, not a currently available backend. Before adding one, the project must record the exact model revision and digest, complete license review and consent provenance, support cancellation and bounded output, and run Apple Silicon benchmarks. No neural TTS adapter exists in this probe today.
