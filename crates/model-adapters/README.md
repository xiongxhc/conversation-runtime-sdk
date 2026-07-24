# Model Adapter Boundaries

The initial executable turn defines language-model and speech-synthesis contracts because it begins from a completed transcript.

The ASR boundary is deliberately documented rather than frozen in code. R0 benchmarks must first select the audio sample format, chunk size, partial-transcript semantics, timestamps, and finalization behavior supported by the first local ASR backend. R3 then adds the `SpeechRecognizer` contract and its deterministic test double before integrating microphone input.

This sequencing avoids publishing an audio interface based on assumptions that have not been tested against the target Apple Silicon hardware and selected ASR runtime.

## Ollama Language Adapter

`OllamaLanguageModel` implements the existing `LanguageModel` contract without exposing Ollama request or response types outside this crate. `OllamaConfig` selects the exact model identifier and can configure endpoint, system prompt, keep-alive duration, temperature, and thinking.

The default endpoint is `http://127.0.0.1:11434`. Keep Ollama loopback-only; future LAN clients use an application-owned runtime gateway.

The generic adapter leaves thinking unset unless the caller chooses a value. The benchmark probe explicitly calls `.with_thinking(false)` because spoken-response latency must measure time to useful content rather than hidden reasoning. The future R2 voice loop must preserve that explicit policy when comparing against the recorded benchmark.

The adapter uses a bounded output channel and incrementally parses newline-delimited chat records. It rejects non-success HTTP responses, malformed or oversized records, truncated streams, and streams that end without Ollama's final `done` record. Cancellation stops response processing and closes the HTTP stream.
