# Model Adapter Boundaries

The initial executable turn defines language-model and speech-synthesis contracts because it begins from a completed transcript.

The ASR boundary is deliberately documented rather than frozen in code. R0 benchmarks must first select the audio sample format, chunk size, partial-transcript semantics, timestamps, and finalization behavior supported by the first local ASR backend. R3 then adds the `SpeechRecognizer` contract and its deterministic test double before integrating microphone input.

This sequencing avoids publishing an audio interface based on assumptions that have not been tested against the target Apple Silicon hardware and selected ASR runtime.
