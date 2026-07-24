# Latency Tests

The latency harness will record:

- microphone speech-end to final transcript;
- final transcript to first language-model token;
- first token to first playable audio;
- speech-end to first useful audio;
- interruption signal to generation stop;
- interruption signal to playback stop;
- total spoken response duration.

Measurements must include the hardware profile, exact model identifiers, warm or cold start state, and sample count. The harness must store timing values without transcript content by default.

No latency target is considered met until the integrated local voice loop reproduces it on the documented Apple Silicon target.
