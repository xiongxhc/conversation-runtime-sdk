# Ollama Local Benchmark Evidence — 2026-07-24

This file preserves the non-sensitive evidence behind `docs/model-benchmarks.md`. The ignored raw artifacts were produced by `tests/ollama/benchmark-local.sh`; the safe evidence needed to audit the summary is retained here.

## Execution Identity

- Source commit: `803c66d050f4080184b09798fb8f4123b92cd6fc`
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
3. captured the installed model digest;
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
| Cold warm-up | 5,852 ms | 6,352 ms | 5,720,550,583 ns | 31 | 500,432,000 ns |
| Measured 1 | 173 ms | 672 ms | 128,773,875 ns | 31 | 499,449,000 ns |
| Measured 2 | 157 ms | 660 ms | 120,990,667 ns | 31 | 503,584,000 ns |
| Measured 3 | 160 ms | 680 ms | 122,352,541 ns | 31 | 520,046,000 ns |

Warm medians:

```text
first_delta_ms=160
total_ms=672
```

All four seeded runs returned the same response:

> It feels natural when both people are actively listening and responding to each other. You also need that comfortable flow where pauses and topics shift without feeling forced.

Provenance reference: `https://huggingface.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF`

## Qwen 27B Q8_0

```text
model=qwen3.6:27b-q8_0
digest=cd0210c667bffa98ad702668d05fda1f340bcbb0a2c769bd389670d19ad1441b
installed_size_bytes=29970392417
loaded_size_bytes=28958062017
loaded_size_vram_bytes=28958062017
loaded_context_length=8192
embedded_license=Apache-2.0
```

| Run | First delta | Wall total | Ollama load | Eval count | Eval duration |
|---|---:|---:|---:|---:|---:|
| Cold warm-up | 7,208 ms | 9,914 ms | 6,802,892,375 ns | 26 | 2,703,975,000 ns |
| Measured 1 | 281 ms | 3,009 ms | 155,348,709 ns | 26 | 2,725,828,000 ns |
| Measured 2 | 274 ms | 3,034 ms | 145,173,042 ns | 26 | 2,759,203,000 ns |
| Measured 3 | 269 ms | 3,115 ms | 141,386,917 ns | 26 | 2,846,221,000 ns |

Warm medians:

```text
first_delta_ms=274
total_ms=3034
```

All four seeded runs returned the same response:

> It flows easily when both people actively listen and respond with genuine curiosity. That mutual engagement creates a relaxed rhythm that feels effortless.

Provenance reference: `https://registry.ollama.com/library/qwen3.6%3A27b-q8_0`

## Llama 70B Q4_K_M

```text
model=hf.co/mradermacher/Llama-3.3-70B-Instruct-abliterated-i1-GGUF:Q4_K_M
digest=6847e511be1feba3e59b4bae991824f9abbd76229f04b32f4c18f5b9c40fc4b8
installed_size_bytes=42520401458
loaded_size_bytes=44823992073
loaded_size_vram_bytes=44823992073
loaded_context_length=8192
```

| Run | First delta | Wall total | Ollama load | Eval count | Eval duration |
|---|---:|---:|---:|---:|---:|
| Cold warm-up | 10,995 ms | 18,316 ms | 10,212,882,958 ns | 45 | 7,321,951,000 ns |
| Measured 1 | 273 ms | 7,462 ms | 109,181,625 ns | 45 | 7,189,059,000 ns |
| Measured 2 | 271 ms | 7,658 ms | 95,622,000 ns | 45 | 7,386,331,000 ns |
| Measured 3 | 277 ms | 7,614 ms | 97,787,917 ns | 45 | 7,336,753,000 ns |

Warm medians:

```text
first_delta_ms=273
total_ms=7614
```

All four seeded runs returned the same response:

> It's all about finding a comfortable flow and rhythm, like a back-and-forth dance between people. When everyone's being their authentic selves and actively listening, that's when conversations start to feel really natural and effortless.

Provenance reference: `https://huggingface.co/mradermacher/Llama-3.3-70B-Instruct-abliterated-i1-GGUF`

## Superseded Preliminary Runs

These values are legacy manual observations from the exploratory session; their temporary raw files were not retained as final evidence. They explain why the fixed policy was introduced but are not mixed into the auditable final comparison:

- Qwen 34.7B had a 20,078 ms median first delta with model-default thinking.
- Qwen 27B had a 95,241 ms median first delta with model-default thinking.
- Llama 70B loaded an 86 GB footprint at its 131,072-token default context and did not complete in six minutes.

The final fixed policy demonstrates that the Llama failure was configuration-specific: at `num_ctx=8192`, it loaded at 44.82 GB and completed every bounded run.

## Evidence Limits

- This is one short prompt, not a broad instruction-following, safety, or preference evaluation.
- Seeded repeatability on one prompt does not prove deterministic behavior across all hardware or Ollama versions.
- Loaded-state size is an Ollama snapshot after warm-up, not an independently sampled peak-memory trace.
- Power mode, thermal state, and energy use were not instrumented.
- Embedded or upstream license labels are evidence inputs, not legal review or redistribution approval.
