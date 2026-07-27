# Latency Probe

Run the deterministic mock probe:

```bash
cargo run -p conversation-latency-harness -- "hello runtime"
```

It emits CSV checkpoints for turn start, final transcript, first text delta, speech start, first synthesis request, first playable audio, speech completion, and terminal completion. Its sequence and causal milestone ordering are tested, and it uses mock adapters with controlled delays.

`first_playable_audio` is the runtime timestamp after typed audio validation and immediately before the output-adapter call. It is not playback-process launch or first audible sound.

The integrated `conversation-voice-probe` reports the same three runtime milestones for real configured adapters. Machine-specific evidence can add an external playback-launch observer and total completion timing without changing lifecycle semantics. See [the runtime text-to-audio evaluation](../../docs/runtime-text-to-audio-evaluation.md).

Future microphone-to-speaker measurements must add speech-end, first audible, interruption-to-generation-stop, interruption-to-playback-stop, and total spoken-duration metrics. Every real-model record must include the source commit, hardware profile, exact benchmark inputs and digests, warm or cold state, sample count, and cleanup status. Timing storage excludes transcript content by default unless a reproducible benchmark explicitly publishes its prompt.

No latency target is considered met until a representative integrated local voice loop measures first audible output on the documented Apple Silicon target.
