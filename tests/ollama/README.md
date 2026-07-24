# Ollama Benchmark Probe

`conversation-ollama-probe` streams a prompt through the local Ollama adapter and writes
first-delta and total timings to standard error. It is a text-only feasibility tool and does
not persist prompts or responses.

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
