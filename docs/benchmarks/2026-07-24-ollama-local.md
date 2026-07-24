# Ollama Local Benchmark Evidence — 2026-07-24

This file preserves the non-sensitive evidence behind `docs/model-benchmarks.md`. The ignored raw artifacts were produced by `tests/ollama/benchmark-local.sh`; the safe evidence needed to audit the summary is retained here.

## Execution Identity

- Source commit: `415fccabf179f2ac78f913599b5b71cc5bfb932d`
- Git tree: clean
- Probe SHA-256: `20f9bcea6a098cdef56080fe8ccf82579826ea2c7d53e10dd3b9561329f3d74c`
- `Cargo.lock` SHA-256: `cc41b402ec5f2b9c901d5fae7e3fef0651f7affa1704cb094be91508d0ed41e2`
- Cargo: 1.97.1
- Rust: 1.97.1
- Machine: MacBook Pro `Mac17,9`
- Chip: Apple M5 Pro
- Logical CPU count: 18
- Unified memory: 68,719,476,736 bytes
- macOS: 26.5
- Ollama: 0.30.10
- Endpoint: `http://127.0.0.1:11434`

No username, serial number, hardware UUID, model-storage path, credential, or private endpoint is recorded.

## Method

Each model used this fixed prompt:

```text
Answer in two short spoken sentences: What makes a conversation feel natural?
```

The probe policy was:

```text
think=false
temperature=0
seed=42
num_predict=128
num_ctx=8192
first_delta_timeout_ms=60000
idle_timeout_ms=30000
total_timeout_ms=120000
```

The committed script:

1. verified a clean source tree;
2. built the exact probe;
3. captured a sanitized installed-model record and `ollama show` metadata;
4. unloaded the selected model and verified its absence;
5. ran one cold warm-up;
6. captured Ollama's loaded-state snapshot;
7. ran three warm measured requests;
8. captured raw responses, wall-clock timings, and Ollama final metrics;
9. unloaded the model and recorded successful cleanup.

`first_delta_ms` measures request start to the first non-empty content delta. `total_ms` measures request start to the final `done` record. Ollama durations are reported in nanoseconds by the local API.

## Qwen 34.7B Q6_K

```text
model=hf.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF:Q6_K
digest=960d4a8b192046de9fd035a66a2769e762a7d5aaff5ba3422d43dc7e6019f6a9
installed_size_bytes=28514153632
loaded_size_bytes=28898345614
loaded_size_vram_bytes=28898345614
loaded_context_length=8192
```

The exact identifier and `ollama show` report Q6_K. The `/api/tags` and loaded-state detail field reported quantization as `unknown`; this metadata disagreement is retained rather than silently normalized.

| Run | First delta | Wall total | Ollama load | Eval count | Eval duration |
|---|---:|---:|---:|---:|---:|
| Cold warm-up | 5,385 ms | 5,844 ms | 5,257,599,375 ns | 31 | 460,114,000 ns |
| Measured 1 | 170 ms | 630 ms | 128,785,667 ns | 31 | 459,823,000 ns |
| Measured 2 | 150 ms | 609 ms | 116,021,167 ns | 31 | 459,260,000 ns |
| Measured 3 | 155 ms | 615 ms | 116,382,708 ns | 31 | 460,768,000 ns |

Warm medians:

```text
first_delta_ms=155
total_ms=615
```

All four seeded runs returned the same response:

> It feels natural when both people are actively listening and responding to each other. You also need that comfortable flow where pauses and topics shift without feeling forced.

Provenance reference: `https://huggingface.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF`

## Qwen 27B Q8_0

```text
model=qwen3.6:27b-q8_0
digest=cd0210c667bffa98ad702668d05fda1f340bcbb0a2c769bd389670d19ad1441b
installed_size_bytes=29970392417
loaded_size_bytes=28798657493
loaded_size_vram_bytes=28798657493
loaded_context_length=8192
embedded_license=Apache-2.0
```

| Run | First delta | Wall total | Ollama load | Eval count | Eval duration |
|---|---:|---:|---:|---:|---:|
| Cold warm-up | 7,434 ms | 10,057 ms | 7,038,902,791 ns | 26 | 2,621,466,000 ns |
| Measured 1 | 274 ms | 2,902 ms | 150,845,792 ns | 26 | 2,628,608,000 ns |
| Measured 2 | 270 ms | 2,919 ms | 143,580,250 ns | 26 | 2,648,576,000 ns |
| Measured 3 | 263 ms | 2,892 ms | 139,909,708 ns | 26 | 2,629,096,000 ns |

Warm medians:

```text
first_delta_ms=270
total_ms=2902
```

All four seeded runs returned the same response:

> It flows easily when both people actively listen and respond with genuine curiosity. That mutual engagement creates a relaxed rhythm that feels effortless.

Provenance reference: `https://registry.ollama.com/library/qwen3.6%3A27b-q8_0`

## Llama 70B Q4_K_M

```text
model=hf.co/mradermacher/Llama-3.3-70B-Instruct-abliterated-i1-GGUF:Q4_K_M
digest=6847e511be1feba3e59b4bae991824f9abbd76229f04b32f4c18f5b9c40fc4b8
installed_size_bytes=42520401458
loaded_size_bytes=45415000964
loaded_size_vram_bytes=45415000964
loaded_context_length=8192
```

| Run | First delta | Wall total | Ollama load | Eval count | Eval duration |
|---|---:|---:|---:|---:|---:|
| Cold warm-up | 10,461 ms | 17,311 ms | 9,744,144,333 ns | 45 | 6,848,929,000 ns |
| Measured 1 | 265 ms | 7,086 ms | 102,200,584 ns | 45 | 6,821,407,000 ns |
| Measured 2 | 266 ms | 7,157 ms | 108,260,333 ns | 45 | 6,888,481,000 ns |
| Measured 3 | 253 ms | 7,125 ms | 96,115,542 ns | 45 | 6,870,133,000 ns |

Warm medians:

```text
first_delta_ms=265
total_ms=7125
```

All four seeded runs returned the same response:

> It's all about finding a comfortable flow and rhythm, like a back-and-forth dance between people. When everyone's being their authentic selves and actively listening, that's when conversations start to feel really natural and effortless.

Provenance reference: `https://huggingface.co/mradermacher/Llama-3.3-70B-Instruct-abliterated-i1-GGUF`

## Superseded Preliminary Runs

These values are legacy manual observations from the exploratory session; their temporary raw files were not retained as final evidence. They explain why the fixed policy was introduced but are not mixed into the auditable final comparison:

- Qwen 34.7B had a 20,078 ms median first delta with model-default thinking.
- Qwen 27B had a 95,241 ms median first delta with model-default thinking.
- Llama 70B loaded an 86 GB footprint at its 131,072-token default context and did not complete in six minutes.

The final fixed policy demonstrates that the Llama failure was configuration-specific: at `num_ctx=8192`, it loaded at 45.42 GB and completed every bounded run.

## Evidence Limits

- This is one short prompt, not a broad instruction-following, safety, or preference evaluation.
- Seeded repeatability on one prompt does not prove deterministic behavior across all hardware or Ollama versions.
- Loaded-state size is an Ollama snapshot after warm-up, not an independently sampled peak-memory trace.
- Power mode, thermal state, and energy use were not instrumented.
- Embedded or upstream license labels are evidence inputs, not legal review or redistribution approval.
