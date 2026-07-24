# Latency Probe

Run the deterministic mock probe:

```bash
cargo run -p conversation-latency-harness -- "hello runtime"
```

It emits CSV checkpoints for:

- microphone speech-end to final transcript;
- final transcript to first language-model token;
- first token to first playable audio;
- speech-end to first useful audio;
- interruption signal to generation stop;
- interruption signal to playback stop;
- total spoken response duration.

The current probe covers turn start, final transcript, first text delta, speech start, speech completion, and terminal completion. Its sequence is tested, and it uses mock adapters with controlled delays.

Future measurements with real models must include the hardware profile, exact model identifiers, warm or cold start state, and sample count. The harness must store timing values without transcript content by default.

No latency target is considered met until the integrated local voice loop reproduces it on the documented Apple Silicon target.
