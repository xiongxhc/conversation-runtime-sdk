# Model Adapter Boundaries

The initial executable turn defines language-model and speech-synthesis contracts because it begins from a completed transcript.

The ASR boundary is deliberately documented rather than frozen in code. R0 benchmarks must first establish the audio sample format, chunk size, partial-transcript semantics, timestamps, and finalization behavior supported by an evaluated local ASR implementation. R3 then adds the `SpeechRecognizer` contract and its deterministic test double before integrating microphone input.

This sequencing avoids publishing an audio interface based on assumptions that have not been tested against the target Apple Silicon hardware and a concrete ASR runtime.

## Ollama Language Adapter

`OllamaLanguageModel` implements the existing `LanguageModel` contract without exposing Ollama request or response types to the protocol or runtime crates. `OllamaConfig` selects the exact model identifier and can configure endpoint, system prompt, keep-alive duration, temperature, deterministic seed, prediction limit, context window, boolean or level-valued thinking, and assistant-content byte limit.

The default endpoint is `http://127.0.0.1:11434`. Keep Ollama loopback-only; future LAN clients use an application-owned runtime gateway.

The generic adapter always serializes its temperature, which defaults to `0.7`. Thinking, seed, prediction limit, and context window remain unset unless the caller chooses values. The benchmark probe uses a fixed policy because spoken-response latency must measure a repeatable path to useful content. Models may differ in which thinking values they support; the recorded policy is evidence only for the exact benchmarked digests.

The adapter disables HTTP redirects so a configured endpoint cannot replay prompts to another origin. It uses a bounded output channel, caps cumulative assistant content, caps the complete response stream at 8 MiB, bounds non-success bodies, and incrementally parses newline-delimited chat records. It rejects malformed or oversized records, truncated streams, and streams that end without Ollama's final `done` record. Cancellation stops response processing and closes the HTTP stream. The runtime independently caps cumulative response bytes for every language-model implementation.

The Ollama-specific `stream_chat` path additionally exposes final load, prompt-evaluation, and response-evaluation metrics for diagnostics and benchmarking. These types remain inside `conversation-model-adapters`; the generic `LanguageModel` contract and runtime stay vendor-neutral.
