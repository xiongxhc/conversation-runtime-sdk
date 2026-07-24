# Ollama Local Benchmark Evidence — 2026-07-24

This file preserves the non-sensitive evidence behind `docs/model-benchmarks.md`. Response content is intentionally omitted.

## Execution Context

- Repository commit used by the probe: `7a02549`
- Probe: `target/debug/conversation-ollama-probe`
- Request policy: top-level `think: false`
- Endpoint: `http://127.0.0.1:11434`
- Prompt: `Answer in two short spoken sentences: What makes a conversation feel natural?`
- Method: one warm-up request, then three measured requests per successful model
- Machine observation: MacBook Pro, Apple M5 Pro, 18 CPU cores, 64 GB unified memory, macOS 26.5
- Runtime observation: Ollama 0.30.10

The probe records request-to-first-non-empty-content-delta and request-to-final-`done` durations. It does not isolate Ollama's model load duration, prompt evaluation, token count, or peak memory.

## Qwen 34.7B Q6_K

Identifier:

```text
hf.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF:Q6_K
```

Local `ollama show` observation:

```text
architecture=qwen35moe
parameters=34.7B
context_length=262144
quantization=Q6_K
capabilities=tools,thinking,completion
embedded_license=not_shown
local_model_size_bytes=28514153632
```

Probe timings:

```text
warmup first_delta_ms=7128 total_ms=7529
run_1 first_delta_ms=161 total_ms=507
run_2 first_delta_ms=153 total_ms=763
run_3 first_delta_ms=173 total_ms=758
median first_delta_ms=161 total_ms=758
```

Preliminary model-default-thinking timings, retained only to explain the configuration decision:

```text
run_1 first_delta_ms=20078 total_ms=20696
run_2 first_delta_ms=21770 total_ms=22427
run_3 first_delta_ms=16924 total_ms=17503
median first_delta_ms=20078 total_ms=20696
```

Provenance reference: `https://huggingface.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF`

## Qwen 27B Q8_0

Identifier:

```text
qwen3.6:27b-q8_0
```

Local `ollama show` observation:

```text
architecture=qwen35
parameters=27.8B
context_length=262144
quantization=Q8_0
capabilities=completion,vision,tools,thinking
embedded_license=Apache-2.0
local_model_size_bytes=29970392417
```

Probe timings:

```text
warmup first_delta_ms=9425 total_ms=12409
run_1 first_delta_ms=352 total_ms=3284
run_2 first_delta_ms=330 total_ms=3325
run_3 first_delta_ms=369 total_ms=3384
median first_delta_ms=352 total_ms=3325
```

Preliminary model-default-thinking timings:

```text
run_1 first_delta_ms=168951 total_ms=173908
run_2 first_delta_ms=95241 total_ms=99048
run_3 first_delta_ms=58657 total_ms=63194
median first_delta_ms=95241 total_ms=99048
```

Provenance reference: `https://registry.ollama.com/library/qwen3.6%3A27b-q8_0`

## Llama 70B Q4_K_M

Identifier:

```text
hf.co/mradermacher/Llama-3.3-70B-Instruct-abliterated-i1-GGUF:Q4_K_M
```

Local `ollama show` observation:

```text
architecture=llama
parameters=70.6B
context_length=131072
quantization=Q4_K_M
capabilities=completion,tools
embedded_license=not_shown
local_model_size_bytes=42520401458
```

The warm-up did not complete and no timing record was emitted. During the attempt, `ollama ps` reported an 86 GB loaded footprint with a 38% CPU / 62% GPU split. The probe was manually interrupted after more than six minutes with only 11 response bytes written, then the model was unloaded.

Provenance reference: `https://huggingface.co/mradermacher/Llama-3.3-70B-Instruct-abliterated-i1-GGUF`

## Evidence Limits

- Model sizes and metadata are local Ollama observations, not independent upstream verification.
- The Apache-2.0 label is embedded metadata for the installed Qwen 27B package; the source and redistribution chain still require review.
- No peak-memory instrumentation was collected for either Qwen model.
- Warm-up request duration includes model loading and generation and is not an isolated load-time metric.
- Quality notes in the summary derive from one prompt and are not a behavior or safety evaluation.
