# Latency Probe

Run the deterministic mock probe:

```bash
cargo run -p conversation-latency-harness -- "hello runtime"
```

It emits CSV checkpoints for turn start, final transcript, first text delta, speech start, speech completion, and terminal completion. Its sequence is tested, and it uses mock adapters with controlled delays.

The real voice-loop harness will later add microphone speech-end, first playable audio, interruption-to-generation-stop, interruption-to-playback-stop, and total spoken-duration metrics.

Future measurements with real models must include the hardware profile, exact model identifiers, warm or cold start state, and sample count. The harness must store timing values without transcript content by default.

No latency target is considered met until the integrated local voice loop reproduces it on the documented Apple Silicon target.
