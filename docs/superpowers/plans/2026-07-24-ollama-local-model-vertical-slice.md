# Ollama Local Model Vertical Slice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Connect the existing local Ollama installation through the replaceable language-model boundary, provide a streaming benchmark probe, and record measured results for the installed models.

**Architecture:** `conversation-model-adapters` owns all Ollama HTTP and NDJSON details behind `LanguageModel`. A separate `conversation-ollama-probe` binary exercises the adapter without inventing speech events, while deterministic tests use a loopback fake server and never require Ollama or downloaded models.

**Tech Stack:** Rust 1.97.1, Tokio, Reqwest 0.13 with rustls, Serde, newline-delimited JSON, Ollama local REST API.

## Global Constraints

- Ollama remains bound to `127.0.0.1`; no task exposes it directly to the LAN.
- Ollama-specific types remain private to `conversation-model-adapters`.
- The exact model identifier is deployment configuration, never a hardcoded SDK default.
- Deterministic tests require no Ollama process, model download, internet access, or audio device.
- The probe performs no prompt or response file writes. The explicit benchmark harness may persist its fixed prompt, responses, and metrics only under ignored local artifacts for reproducibility.
- SQLite, the Mac app, iPhone gateway, TTS, ASR, and real audio remain outside this implementation plan.

---

### Task 1: Ollama Request and Stream Adapter

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/model-adapters/Cargo.toml`
- Modify: `crates/model-adapters/src/lib.rs`
- Create: `crates/model-adapters/src/ollama.rs`
- Create: `crates/model-adapters/tests/ollama.rs`

**Interfaces:**
- Consumes: `LanguageModel`, `LanguageModelRequest`, `AdapterError`, and `CancellationToken`.
- Produces: `OllamaConfig::new`, `OllamaConfig::with_endpoint`, `OllamaConfig::with_system_prompt`, `OllamaConfig::with_keep_alive`, `OllamaConfig::with_temperature`, and `OllamaLanguageModel::new`.

- [ ] **Step 1: Add the fake-server tests before production code**

Create integration tests that:

```rust
#[tokio::test]
async fn streams_chat_content_and_serializes_the_request() {
    let server = FakeOllamaServer::streaming([
        r#"{"message":{"role":"assistant","content":"hello"},"done":false}"#,
        r#"{"message":{"role":"assistant","content":" world"},"done":false}"#,
        r#"{"message":{"role":"assistant","content":""},"done":true}"#,
    ])
    .await;

    let model = OllamaLanguageModel::new(
        OllamaConfig::new("test-model")
            .with_endpoint(server.endpoint())
            .unwrap(),
    );
    let mut output = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    assert_eq!(output.recv().await.unwrap().unwrap(), "hello");
    assert_eq!(output.recv().await.unwrap().unwrap(), " world");
    assert!(output.recv().await.is_none());
    assert_eq!(server.request_json().await["model"], "test-model");
    assert_eq!(server.request_json().await["stream"], true);
}
```

Add separate tests for:

- HTTP `500` becoming one `AdapterError`;
- malformed NDJSON becoming one `AdapterError`;
- cancellation closing the adapter receiver before a delayed second chunk;
- empty model identifiers and invalid endpoint URLs being rejected.

- [ ] **Step 2: Run tests and verify the missing API**

Run:

```bash
cargo test -p conversation-model-adapters --test ollama --locked
```

Expected: compilation fails because `OllamaConfig` and `OllamaLanguageModel` do not exist.

- [ ] **Step 3: Add only the required dependencies**

Add workspace dependencies:

```toml
reqwest = { version = "0.13", default-features = false, features = ["json", "rustls"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

Extend Tokio workspace features with `io-util` and `net` for the fake server. Consume the HTTP and serialization dependencies only from `conversation-model-adapters`.

- [ ] **Step 4: Implement configuration and private wire types**

Implement:

```rust
#[derive(Clone, Debug)]
pub struct OllamaConfig {
    endpoint: reqwest::Url,
    model: String,
    system_prompt: Option<String>,
    keep_alive: Option<String>,
    temperature: f32,
}

#[derive(Clone)]
pub struct OllamaLanguageModel {
    client: reqwest::Client,
    config: OllamaConfig,
}
```

Validate non-empty model identifiers, parse endpoint URLs once during configuration, and construct the chat URL with `Url::join("api/chat")`.

Private request types contain `model`, `messages`, `stream: true`, `keep_alive`, and `options.temperature`. Private response types contain optional `message.content`, `done`, and optional `error`.

- [ ] **Step 5: Implement cancellation-aware NDJSON streaming**

`LanguageModel::stream` creates a bounded channel and spawns one task. The task:

1. sends `POST /api/chat`;
2. rejects non-success status codes with the response body;
3. reads response chunks with `Response::chunk()`;
4. buffers split network chunks until newline boundaries;
5. parses each complete non-empty line;
6. sends non-empty assistant content as deltas;
7. returns on `done: true`;
8. selects every network read and channel send against cancellation.

On failure, send one `AdapterError` unless cancellation has already occurred.

- [ ] **Step 6: Run focused and package tests**

Run:

```bash
cargo test -p conversation-model-adapters --test ollama --locked
cargo test -p conversation-model-adapters --locked
```

Expected: all adapter tests pass without a running Ollama service.

- [ ] **Step 7: Commit the adapter**

```bash
git add Cargo.toml Cargo.lock crates/model-adapters
git commit -m "feat: add streaming Ollama language adapter"
```

---

### Task 2: Live Ollama Benchmark Probe

**Files:**
- Modify: `Cargo.toml`
- Create: `tests/ollama/Cargo.toml`
- Create: `tests/ollama/src/main.rs`
- Create: `tests/ollama/README.md`

**Interfaces:**
- Consumes: `OllamaConfig`, `OllamaLanguageModel`, `LanguageModel::stream`, `LanguageModelRequest`, and `TurnId`.
- Produces: the `conversation-ollama-probe` CLI.

- [ ] **Step 1: Add an argument-parsing unit test**

Extract:

```rust
struct ProbeArguments {
    model: String,
    prompt: String,
}

fn parse_arguments(arguments: impl IntoIterator<Item = String>) -> Result<ProbeArguments, String>;
```

Test that the first argument is the exact model identifier, remaining arguments form the prompt, and missing values return a usage error.

- [ ] **Step 2: Run the test and verify failure**

Run:

```bash
cargo test -p conversation-ollama-probe --locked
```

Expected: compilation fails because the probe package and parser do not exist.

- [ ] **Step 3: Implement the probe**

The binary:

1. reads `OLLAMA_ENDPOINT`, defaulting to `http://127.0.0.1:11434`;
2. parses model and prompt arguments;
3. starts `OllamaLanguageModel`;
4. prints streamed text deltas immediately to stdout;
5. records first-delta and completion durations with `Instant`;
6. prints timing fields to stderr:

```text
model=<exact identifier>
first_delta_ms=<milliseconds>
total_ms=<milliseconds>
```

Return a non-zero exit status for adapter or argument errors.

- [ ] **Step 4: Run deterministic probe tests**

Run:

```bash
cargo test -p conversation-ollama-probe --locked
```

Expected: argument tests pass without contacting Ollama.

- [ ] **Step 5: Commit the probe**

```bash
git add Cargo.toml Cargo.lock tests/ollama
git commit -m "feat: add local Ollama benchmark probe"
```

---

### Task 3: Benchmark Installed Models

Before recording final measurements, add a focused follow-up discovered by the initial live run:

- add explicit thinking configuration behind a failing serialization test;
- serialize it as the optional top-level Ollama `think` field;
- leave the generic adapter default unset;
- make the spoken-latency probe call `.with_thinking(false)`;
- rerun all installed models with thinking disabled, fixed generation options, and an 8K context.

**Files:**
- Modify: `docs/model-benchmarks.md`
- Create: `docs/benchmarks/2026-07-24-ollama-local.md`
- Modify: `models/registry.example.toml`

**Interfaces:**
- Consumes: the live Ollama service and `conversation-ollama-probe`.
- Produces: measured feasibility evidence and copyable disabled registry entries.

- [ ] **Step 1: Record the safe machine profile**

Document:

- MacBook Pro;
- Apple M5 Pro;
- 18 CPU cores;
- 64 GB unified memory;
- macOS 26.5;
- Ollama 0.30.10.

Do not record serial number, hardware UUID, username, model storage path, or other private identifiers.

- [ ] **Step 2: Run one warm-up and three measured prompts per model**

Use the same short conversational prompt for each model:

```text
Answer in two short spoken sentences: What makes a conversation feel natural?
```

Run the probe once as warm-up, then three measured runs for:

```text
hf.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF:Q6_K
qwen3.6:27b-q8_0
hf.co/mradermacher/Llama-3.3-70B-Instruct-abliterated-i1-GGUF:Q4_K_M
```

Capture first-delta and total milliseconds. Stop and record a failure rather than hiding a model that cannot load within the available memory.

- [ ] **Step 3: Update the benchmark table**

For each model, record exact identifier, provenance, reported license status, quantization, local size, warm/cold state, first-delta samples, total-time samples, median values, and concise quality notes.

Do not interpret latency alone as an SDK recommendation. Mark community `abliterated` models as requiring provenance and behavior review.

- [ ] **Step 4: Add disabled registry examples**

Add all three model identifiers with:

```toml
capability = "language-model"
source = "ollama-local"
license_status = "review-required"
enabled = false
```

Use `benchmark_status = "measured"` for all three final 8K-context measurements. Preserve an immutable digest and upstream `provenance` URL for every entry.

- [ ] **Step 5: Commit benchmark evidence**

```bash
git add docs/model-benchmarks.md docs/benchmarks/2026-07-24-ollama-local.md models/registry.example.toml
git commit -m "docs: record local Ollama feasibility results"
```

---

### Task 4: Roadmap and End-to-End Verification

**Files:**
- Modify: `README.md`
- Modify: `ROADMAP.md`
- Modify: `crates/model-adapters/README.md`

**Interfaces:**
- Consumes: completed adapter, probe, and measured model results.
- Produces: exact user workflow and updated chronological roadmap.

- [ ] **Step 1: Document the live workflow**

Add:

```bash
cargo run --locked -p conversation-ollama-probe -- \
  "<installed-model-id>" \
  "Answer briefly: hello"
```

State that the probe uses real local inference but is text-only. Do not claim TTS, ASR, microphone, desktop UI, memory, or product latency completion.

- [ ] **Step 2: Update chronological roadmap state**

Mark:

- R1 complete;
- Ollama adapter and probe as the first completed part of R0/R2 feasibility;
- a local TTS reference adapter and text-to-audio as the next task;
- microphone/ASR/barge-in after text-to-audio;
- macOS app after the voice loop;
- paired iPhone LAN client after the app gateway;
- Linux and Windows after the SDK boundary is proven.

- [ ] **Step 3: Run the full gate**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo run --locked -p conversation-ollama-probe -- \
  "<installed-model-id>" \
  "Answer in one short sentence: confirm local inference."
git diff --check
```

Expected: all deterministic checks pass and the live probe returns streamed local text with timing values.

- [ ] **Step 4: Request independent pre-merge review**

Review the full branch against `master`, emphasizing:

- cancellation and stream framing;
- accidental prompt persistence;
- Ollama leakage into public runtime types;
- documentation overclaims;
- benchmark reproducibility.

Fix all critical and important findings, then rerun Step 3.

- [ ] **Step 5: Commit documentation**

```bash
git add README.md ROADMAP.md crates/model-adapters/README.md
git commit -m "docs: document Ollama development workflow"
```

- [ ] **Step 6: Push and create the PR**

```bash
git push -u origin feature/ollama-local-model-vertical-slice
gh pr create \
  --base master \
  --head feature/ollama-local-model-vertical-slice \
  --title "feat: connect local Ollama language models"
```
