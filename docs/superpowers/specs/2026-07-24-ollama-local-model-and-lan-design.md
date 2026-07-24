# Ollama Local Model and LAN Architecture Design

**Date:** 2026-07-24

## Problem

The deterministic runtime is validated, but it still uses mock language and speech adapters. The next chronological task is to connect the existing local Ollama installation without coupling the public protocol or runtime to Ollama, then measure the installed models before selecting a default for the audible voice loop.

The longer-term product must present as a custom macOS application and allow an iPhone to participate over the local network without exposing Ollama directly or duplicating private memory across devices prematurely.

## Approved Decisions

- Build a configurable Ollama language-model adapter and text benchmark probe before integrating speech.
- Keep Ollama bound to loopback. Future LAN clients communicate with an application-owned runtime gateway, never directly with Ollama.
- Use the Mac as the initial runtime and memory authority.
- Store the future SQLite memory database under the macOS application-data directory, outside the repository.
- Add iPhone as a paired LAN client after the Mac voice loop is validated.
- Keep Windows and Linux as later platform milestones after the Mac runtime and client boundary are proven.

## Current Local Baseline

The target development machine is an Apple Silicon MacBook Pro with an Apple M5 Pro and 64 GB of memory. Ollama `0.30.10` is installed and exposes its local API at `http://127.0.0.1:11434`.

The currently installed models are:

| Model | Parameters | Quantization | Local size |
|---|---:|---|---:|
| `hf.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF:Q6_K` | 34.7B MoE | reported as unknown by Ollama | 28.5 GB |
| `qwen3.6:27b-q8_0` | 27.8B | Q8_0 | 30.0 GB |
| `hf.co/mradermacher/Llama-3.3-70B-Instruct-abliterated-i1-GGUF:Q4_K_M` | 70.6B | Q4_K_M | 42.5 GB |

No model is selected as the product default until the benchmark probe records load time, first text delta, total generation time, and output quality notes. Community `abliterated` variants remain development candidates rather than implicit production defaults.

## Ollama Adapter

`conversation-model-adapters` gains an `OllamaLanguageModel` implementation behind the existing `LanguageModel` trait.

Configuration contains:

- endpoint URL, defaulting to `http://127.0.0.1:11434`;
- exact model identifier;
- optional system prompt;
- optional keep-alive duration;
- generation temperature.

The adapter calls `POST /api/chat` with streaming enabled and one user message containing the completed transcript. Ollama returns newline-delimited JSON objects. Each non-empty assistant content field becomes one language-model delta. The final `done` record closes the stream.

HTTP status failures, malformed stream records, and Ollama error records become `AdapterError` values sent through the existing adapter channel. Cancelling the turn stops response processing and drops the HTTP stream. Ollama-specific request and response types remain private to `model-adapters`.

## Benchmark Probe

A new `conversation-ollama-probe` binary exercises the adapter directly rather than inventing fake speech events.

The probe accepts:

- model identifier;
- prompt text;
- optional endpoint through `OLLAMA_ENDPOINT`.

It prints the streamed response to standard output and emits machine-readable timing fields for:

- request start;
- first text delta;
- completed response;
- total elapsed time.

The probe is a feasibility tool, not a product client. It does not persist prompts or responses. Deterministic tests use a local fake HTTP server; one manual live run validates the installed Ollama service.

## macOS Application and SQLite

The future custom macOS app owns the runtime, audio devices, configuration, and memory controls. SQLite is embedded in the application process; no database server is installed.

The default database location is:

`~/Library/Application Support/Conversation Runtime/runtime.sqlite3`

SQLite WAL and shared-memory files remain beside the database. Schema migrations are committed to the repository, while the database and its contents are never committed. Tests use temporary databases.

Credentials, LAN pairing secrets, and private keys belong in macOS Keychain, not SQLite. FileVault provides the initial disk-at-rest boundary; database-level encryption is deferred until the threat model demonstrates a need.

The database is not created in this milestone. Controlled memory remains R5, after the live voice loop and conversation controls establish what should be remembered.

## iPhone and LAN Boundary

The initial iPhone app is a thin client. The Mac remains the source of truth for inference and memory.

```text
iPhone app
  -> paired application protocol over the LAN
  -> Mac runtime gateway
     -> conversation runtime
     -> Ollama on 127.0.0.1
     -> local audio and SQLite memory
```

The runtime gateway will provide:

- Bonjour discovery;
- explicit pairing with a short-lived code;
- mutually authenticated sessions;
- TLS-protected control and event transport;
- a future low-latency audio channel, evaluated with WebRTC before choosing a custom stream.

The gateway binds only after the user enables LAN access. Ollama remains loopback-only. The iPhone stores pairing credentials in Keychain and may keep disposable UI cache, but it does not own durable conversation memory in the first release.

Remote internet access, cloud relay, and multi-device memory synchronization are excluded from the first LAN client.

## Future Windows and Linux Support

The runtime, protocol, adapter, and gateway contracts remain portable. Platform expansion begins only after:

- the Apple Silicon voice loop passes R3;
- the Mac application and a second client prove the SDK boundary in R6;
- measured demand justifies platform-specific audio, packaging, acceleration, and support costs.

Linux receives one documented audio and acceleration profile. Windows receives one documented WASAPI and acceleration profile. Neither platform is declared supported from compilation alone.

## Testing

The adapter milestone must include:

- request serialization tests;
- streamed NDJSON delta tests;
- HTTP and malformed-response error tests;
- cancellation tests proving stream processing stops;
- unchanged deterministic runtime tests;
- one live probe against an installed local model;
- benchmark documentation that distinguishes measured values from product targets.

## Exit Criteria

- Any installed Ollama model can be selected by exact identifier without code changes.
- The probe streams real local model output and records first-delta and completion timing.
- Cancellation stops adapter stream processing.
- Ollama types do not enter `conversation-protocol` or `conversation-runtime`.
- Tests do not require Ollama, downloaded models, or network access.
- The roadmap clearly sequences Ollama benchmarking, local speech, the Mac app, iPhone LAN access, and later Windows/Linux support.
