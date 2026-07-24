# Ollama Benchmark Probe

`conversation-ollama-probe` streams a prompt through the local Ollama adapter. It writes text
deltas immediately to standard output and machine-readable timing and Ollama metrics to standard
error. It is a text-only feasibility tool and performs no prompt or response file writes itself.
Prompts supplied as arguments may still be visible in shell history or process inspection.

The probe uses a fixed benchmark policy: `think=false`, temperature `0`, seed `42`,
`num_predict=128`, and `num_ctx=8192`. Other adapter callers retain their model defaults unless
they configure them explicitly.

Run it against the default local endpoint:

```bash
cargo run --locked -p conversation-ollama-probe -- \
  "qwen3.6:27b-q8_0" \
  "Answer briefly: hello"
```

Set `OLLAMA_ENDPOINT` to use a different endpoint. The parser unit tests do not contact
Ollama:

```bash
cargo test -p conversation-ollama-probe --locked
```

To avoid putting a sensitive prompt in shell history or process arguments, omit the prompt
arguments and provide a non-empty prompt on standard input:

```bash
printf '%s\n' 'Answer briefly: hello' | \
  cargo run --locked -p conversation-ollama-probe -- "qwen3.6:27b-q8_0"
```

The probe bounds each request with 60 seconds to first text delta, 30 seconds idle time after a
delta, and 120 seconds total time. Override these for controlled experiments with non-zero
millisecond values in `OLLAMA_FIRST_DELTA_TIMEOUT_MS`, `OLLAMA_IDLE_TIMEOUT_MS`, and
`OLLAMA_TOTAL_TIMEOUT_MS`. A timeout exits non-zero and reports `status=timeout`,
`timeout_stage`, and `elapsed_ms`; configuration or adapter failures exit non-zero with
`status=error`, `stage`, `elapsed_ms`, and a sanitized single-line `error`.

Successful reports include `model`, `status=ok`, wall-clock `first_delta_ms` and `total_ms`, the
benchmark policy, and Ollama's final total/load/prompt-evaluation/response-evaluation metrics.
Metric fields read `unavailable` only when the server omits them.

For an identifiable, repeatable local benchmark, use the repository script from a clean Git tree.
It verifies that the selected model is unloaded, records one cold warm-up plus three warm measured
runs, captures the loaded model state, model digest, source commit, lockfile and binary hashes, and
toolchain versions, and writes raw metrics and responses under ignored `artifacts/ollama/`:

```bash
tests/ollama/benchmark-local.sh "qwen3.6:27b-q8_0"
```

The optional second argument is a new direct child name under `artifacts/ollama/`; parent
traversal, existing destinations, and symbolic-link artifact roots are rejected. The script uses
owner-only artifact permissions, bounded build/API/probe operations, recorded cleanup status, and
a loopback-only endpoint. It intentionally persists the fixed benchmark prompt, raw response
text, safe machine profile, and metrics in the artifact directory. Inspect that directory before
sharing it or using a different prompt.
