# Typed Text-to-Speech Probe

`conversation-tts-probe` turns typed text into AIFF through the macOS system-speech reference adapter or WAV through an explicitly configured OpenAI-compatible local HTTP adapter. By default it plays the result through the macOS system player and removes all temporary audio after playback.

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

Voice precedence is `CLI > environment > selected profile > macOS system defaults`. macOS system-speech rate follows the same precedence; `--rate` and `CONVERSATION_TTS_RATE` are not valid for local HTTP profiles.

```text
CLI > environment > selected profile > macOS system defaults
```

`backend = "macos-system"` and `backend = "openai-compatible"` are accepted. `--profile` requires `--config`; if no profile is selected, the configuration file's `default_profile` is used. Configuration paths must be absolute, and configuration files are bounded to 64 KiB.

## Local Neural Evaluation

Start the verified MLX-Audio server on loopback only:

```bash
uv tool install --force "mlx-audio[server]" --prerelease=allow
mlx_audio.server --host 127.0.0.1 --port 8000
```

Run the explicitly labeled fast evaluation candidate:

```bash
rustup run 1.97.1 cargo run --locked -p conversation-tts-probe -- \
  --config "$PWD/configs/speech.mlx-audio.example.toml" \
  --profile local-neural-fast \
  "你好，这是本地神经语音测试。"
```

`configs/speech.mlx-audio.example.toml` contains two measured Apple Silicon candidates, not SDK defaults. Both constrain `max_tokens` to `128` and set `repetition_penalty` to `1.05`, because the host's uncapped default produced impractically long output in evaluation. The first request downloads and loads model files into the model host's external cache, not this repository.

The report distinguishes synthesis completion from playback launch. Neither value is a measurement of first playable audio. The system-speech and local HTTP paths are replaceable reference adapters, not a neural-TTS recommendation, application voice selection, or model-quality decision. Exact measurements and remaining evidence gates are in [docs/neural-tts-evaluation.md](../../docs/neural-tts-evaluation.md).
