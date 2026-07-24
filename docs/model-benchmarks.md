# Local Model Benchmark Matrix

**Status:** Reproducible local language-model feasibility measured on 2026-07-24. ASR, TTS, audio, and end-to-end voice latency remain unmeasured.

The 1.2-second time-to-useful-audio goal is a product target, not a measured result. These measurements cover text generation only.

## Hardware and Policy

- MacBook Pro `Mac17,9`
- Apple M5 Pro, 18 logical CPU cores
- 64 GiB unified memory
- macOS 26.5
- Ollama 0.30.10 on loopback
- `think=false`, temperature `0`, seed `42`
- `num_predict=128`, `num_ctx=8192`
- one verified-cold warm-up, then three warm measured runs

The exact source identity, model digests, raw samples, final Ollama metrics, loaded-state snapshots, identical seeded outputs, and evidence limits are preserved in [benchmarks/2026-07-24-ollama-local.md](benchmarks/2026-07-24-ollama-local.md).

## Language-Model Results

| Exact model | Quantization evidence | Installed size | Loaded snapshot | Cold load | Warm first-delta samples | Median first delta | Warm total samples | Median total | Result |
|---|---|---:|---:|---:|---|---:|---|---:|---|
| `hf.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF:Q6_K` | identifier and `ollama show`: Q6_K; API detail: unknown | 28.51 GB | 28.90 GB | 5.72 s | 173, 157, 160 ms | **160 ms** | 672, 660, 680 ms | **672 ms** | Fastest measured development candidate |
| `qwen3.6:27b-q8_0` | Q8_0 | 29.97 GB | 28.96 GB | 6.80 s | 281, 274, 269 ms | **274 ms** | 3,009, 3,034, 3,115 ms | **3,034 ms** | Official-provenance provisional R2 candidate |
| `hf.co/mradermacher/Llama-3.3-70B-Instruct-abliterated-i1-GGUF:Q4_K_M` | identifier and `ollama show`: Q4_K_M; loaded detail: unknown | 42.52 GB | 44.82 GB | 10.21 s | 273, 271, 277 ms | **273 ms** | 7,462, 7,658, 7,614 ms | **7,614 ms** | Feasibility pass at 8K context; largest and slowest completion |

All three fixed-policy responses were relevant, grammatical, and followed the requested two-sentence format. That narrow observation is backed by retained identical seeded output in the evidence file; it is not a behavior or safety review.

The two community `abliterated` checkpoints remain development candidates only. Their provenance, license chain, and behavior require review before any product default or redistribution decision.

## Current Decision

- **Provisional R2 integration candidate:** `qwen3.6:27b-q8_0`, because it has official Ollama provenance, locally reported Apache-2.0 metadata, a 28.96 GB loaded snapshot, and a measured 274 ms median first text delta.
- **Fast local development candidate:** Qwen 34.7B Q6_K, with a 160 ms median first text delta and 672 ms median completion, pending community-checkpoint review.
- **Viable comparison, not preferred:** Llama 70B Q4_K_M now passes at 8K context, but uses a 44.82 GB loaded snapshot and needs 7.614 seconds median to complete this short response.

No model is a permanent product default. Qwen 27B is not admitted to the voice loop until source/license review is complete and TTS measures time to the first playable phrase under the same explicit inference policy.

## Remaining Model Matrix

| Layer | Candidate | License reviewed | Loaded memory | Real-time factor | First text delta | First audio | Result |
|---|---|---|---:|---:|---:|---:|---|
| ASR | Not selected | No | Not measured | Not measured | Not applicable | Not applicable | Pending benchmark |
| LLM | `qwen3.6:27b-q8_0` provisional R2 candidate | Partial; local Apache-2.0 metadata only | 28.96 GB snapshot | Not applicable | 274 ms median | Not applicable | Conditional text feasibility pass |
| TTS | Not selected | No | Not measured | Not measured | Not applicable | Not measured | Next chronological benchmark |

## Selection Rule

A model can enter the first integrated voice loop only after its exact digest, source, license status, inference policy, hardware consumption, latency, and relevant quality evidence are recorded. Model popularity alone is not evidence that it fits the target machine or product latency.
