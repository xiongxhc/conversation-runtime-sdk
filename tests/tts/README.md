# macOS System-Speech Probe

`conversation-tts-probe` turns typed text into AIFF through the macOS system-speech reference adapter. By default it plays the result through the macOS system player and removes all temporary audio after playback.

Run the audible reference flow:

```bash
cargo run --locked -p conversation-tts-probe -- \
  "This is a local system-speech reference adapter."
```

Synthesize without playback and retain an explicit output:

```bash
cargo run --locked -p conversation-tts-probe -- \
  --no-play \
  --output /tmp/conversation-runtime-reference.aiff \
  "This output path was explicitly requested."
```

If text arguments are omitted, the probe reads a non-empty value from standard input. `--output` must be absolute. Optional overrides are `CONVERSATION_TTS_VOICE`, `CONVERSATION_TTS_RATE`, `CONVERSATION_TTS_TIMEOUT_MS`, `CONVERSATION_TTS_SAY_PATH`, and `CONVERSATION_TTS_PLAYER_PATH`.

The report distinguishes synthesis completion from playback launch. Neither value is a measurement of first audible audio. The system-speech path is a replaceable reference adapter, not a neural TTS recommendation or application voice selection.
