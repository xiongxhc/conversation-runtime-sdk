# Local Model Benchmark Matrix

**Status:** Local language-model feasibility measured on 2026-07-24. ASR, TTS, audio, and end-to-end voice latency remain unmeasured.

The 1.2-second time-to-useful-audio goal is a product target, not a measured result. These measurements cover text generation only.

## Hardware and Runtime

- Machine: MacBook Pro
- Chip: Apple M5 Pro
- CPU: 18 cores
- Unified memory: 64 GB
- Operating system: macOS 26.5
- Ollama: 0.30.10
- Endpoint: loopback `http://127.0.0.1:11434`

No serial number, hardware UUID, username, model-storage path, or other private identifier is recorded.

## Method

The probe used the exact installed model identifier and this prompt:

```text
Answer in two short spoken sentences: What makes a conversation feel natural?
```

Each successful model received one warm-up request followed by three measured requests. `first_delta_ms` is measured from request start to the first non-empty `message.content` delta; `total_ms` ends at Ollama's final `done` record.

The spoken-latency probe sent `think: false`. Preliminary runs using model-default thinking were much slower and are not mixed into the final samples: Qwen 34.7B had a 20,078 ms median first delta and Qwen 27B had a 95,241 ms median first delta.

The safe execution record, local metadata observations, exact samples, and evidence limits are preserved in [benchmarks/2026-07-24-ollama-local.md](benchmarks/2026-07-24-ollama-local.md).

## Language-Model Results

| Exact model identifier | Provenance | Reported license evidence | Quantization | Local size | Warm-up first / total | Measured first-delta samples | Median first delta | Measured total samples | Median total | Result |
|---|---|---|---|---:|---:|---|---:|---|---:|---|
| `hf.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF:Q6_K` | Community Hugging Face GGUF loaded by Ollama | No license shown by local `ollama show`; review required | Q6_K | 28.5 GB | 7,128 / 7,529 ms | 161, 153, 173 ms | **161 ms** | 507, 763, 758 ms | **758 ms** | Feasibility pass; fastest measured candidate |
| `qwen3.6:27b-q8_0` | Official Ollama library model | Local metadata reports Apache-2.0; source/package review still required | Q8_0 | 30.0 GB | 9,425 / 12,409 ms | 352, 330, 369 ms | **352 ms** | 3,284, 3,325, 3,384 ms | **3,325 ms** | Feasibility pass; provisional official candidate |
| `hf.co/mradermacher/Llama-3.3-70B-Instruct-abliterated-i1-GGUF:Q4_K_M` | Community Hugging Face GGUF loaded by Ollama | No license shown by local `ollama show`; review required | Q4_K_M | 42.5 GB | Did not complete | Not measured | Not measured | Not measured | Not measured | Attempt failed on this profile: 86 GB reported loaded footprint and only 11 response bytes after more than six minutes |

All six measured Qwen responses followed the requested two-sentence spoken style and stayed on topic. Qwen 34.7B was concise and substantially faster end-to-end. Qwen 27B was similarly coherent but generated the short response more slowly. These are narrow quality observations from one prompt, not a behavior or safety evaluation.

The two community `abliterated` checkpoints are development candidates only. Their provenance, license chain, and behavior require review before any product default or redistribution decision.

## Current Decision

- **Provisional R2 candidate:** `qwen3.6:27b-q8_0`, because it has official Ollama provenance, locally reported Apache-2.0 metadata, and a measured 352 ms median first text delta with `think: false`.
- **Fast development candidate:** the local Qwen 34.7B Q6_K checkpoint, with a measured 161 ms median first text delta and 758 ms median completion.
- **Rejected for this Mac profile:** the installed Llama 70B Q4 checkpoint because its loaded footprint exceeds physical memory and the warm-up did not complete.

Qwen 27B is not yet admitted to the integrated voice loop. That requires explicit `think: false` runtime configuration, source and license-chain review, Qwen memory measurement, TTS time-to-first-playable-phrase measurement, and broader instruction-following and spoken-response evaluation.

## Remaining Model Matrix

| Layer | Candidate | License reviewed | Peak memory | Real-time factor | First text delta | First audio | Result |
|---|---|---|---:|---:|---:|---:|---|
| ASR | Not selected | No | Not measured | Not measured | Not applicable | Not applicable | Pending benchmark |
| LLM | `qwen3.6:27b-q8_0` provisional R2 candidate | Partial; local Apache-2.0 metadata only | Not measured | Not applicable | 352 ms median | Not applicable | Conditional text feasibility pass |
| TTS | Not selected | No | Not measured | Not measured | Not applicable | Not measured | Next chronological benchmark |

## Selection Rule

A model can enter the first integrated voice loop only after its exact checkpoint, source, license, quantization, hardware consumption, and latency measurements are recorded. Model popularity alone is not evidence that it fits the target machine or product latency.
