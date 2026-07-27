# Local Model Benchmark Matrix

**Status:** Reproducible local language-model feasibility measured on 2026-07-24. ASR, TTS, audio, and end-to-end voice latency remain unmeasured.

The 1.2-second time-to-useful-audio goal is a runtime target, not a measured result. These measurements cover text generation only.

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
| `hf.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF:Q6_K` | identifier and `ollama show`: Q6_K; API detail: unknown | 28.51 GB | 28.90 GB | 6.04 s | 164, 149, 158 ms | **158 ms** | 647, 618, 628 ms | **628 ms** | Lowest latency in this measurement |
| `qwen3.6:27b-q8_0` | Q8_0 | 29.97 GB | 28.96 GB | 7.05 s | 286, 264, 268 ms | **268 ms** | 2,968, 2,939, 2,945 ms | **2,945 ms** | Official-registry checkpoint measurement |
| `hf.co/mradermacher/Llama-3.3-70B-Instruct-abliterated-i1-GGUF:Q4_K_M` | identifier and `ollama show`: Q4_K_M; loaded detail: unknown | 42.52 GB | 45.42 GB | 15.07 s | 278, 258, 253 ms | **258 ms** | 7,323, 7,225, 7,227 ms | **7,227 ms** | Feasibility pass at 8K context; largest and slowest completion |

All three fixed-policy responses were relevant, grammatical, and followed the requested two-sentence format. That narrow observation is backed by retained identical seeded output in the evidence file; it is not a behavior or safety review.

The two community `abliterated` checkpoints require provenance, license-chain, and behavior review before any deployment or redistribution decision.

## Interpretation

- The lowest-latency checkpoint in this narrow run had a 158 ms median first text delta and 628 ms median completion.
- The official-registry checkpoint reported Apache-2.0 metadata locally, used a 28.96 GB loaded snapshot, and had a 268 ms median first text delta.
- The largest checkpoint passed at 8K context but used a 45.42 GB loaded snapshot and needed 7.227 seconds median to complete this short response.

These observations validate the adapter and benchmark method only. The public SDK does not select a model. A consuming deployment owns model admission, source and license review, behavioral evaluation, and the inference policy paired with TTS.

## Remaining Capability Matrix

| Layer | Implementation | License review | Loaded memory | Real-time factor | First text delta | First audio | Result |
|---|---|---|---:|---:|---:|---:|---|
| ASR | Not evaluated | Not started | Not measured | Not measured | Not applicable | Not applicable | Pending benchmark |
| LLM | Deployment-configured | Deployment-owned | Benchmark required | Not applicable | Benchmark required | Not applicable | Reference adapter ready |
| TTS | Reference adapter planned | System component | Not measured | Not measured | Not applicable | Not measured | Next chronological benchmark |

## Deployment Admission Evidence

A consuming deployment should admit an implementation only after its exact digest or system version, source, license status, inference policy, hardware consumption, latency, and relevant quality evidence are recorded. Popularity alone is not evidence that an implementation fits the target machine or runtime latency goal.
