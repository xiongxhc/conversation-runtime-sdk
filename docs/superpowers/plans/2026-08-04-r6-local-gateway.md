# R6 Local Gateway and TypeScript Client Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a persistent local-only Rust gateway, transport-neutral TypeScript client, and Node chat example that stream and interrupt real local text turns without desktop-code coupling.

**Architecture:** Add an explicit version-1 client wire projection in `conversation-protocol`, a text-only lifecycle in `conversation-runtime`, and direct generation-identity support in the Ollama-compatible adapter. A reference gateway composes those layers behind bounded framed stdio; `@conversation/runtime` validates the same envelopes and provides an injectable client plus a Node stdio transport.

**Tech Stack:** Rust 2021, Tokio, Serde/Serde JSON, TOML, SQLite through the existing memory crate, TypeScript, Node child processes, Node test runner.

## Global Constraints

- Work only in `feature/r6-local-gateway` at `/Users/cx/Workspace/conversation-runtime-sdk/.worktrees/r6-local-gateway`.
- Preserve `protocol <- model-adapters <- runtime`; the gateway composes core crates and no core crate depends on the gateway or TypeScript package.
- Keep all public content backend-neutral; Ollama-compatible details stay in the reference adapter and gateway configuration.
- R6 is local-only: no listener, LAN binding, socket, discovery, pairing, TLS, cloud fallback, telemetry, or generic host switch.
- Gateway configuration must be an absolute path, bounded to 64 KiB, `schema_version = 1`, and reject unknown fields.
- Language endpoints in local-only mode must be plain HTTP on a numeric loopback address.
- Memory is disabled unless one absolute existing SQLite path is configured; never discover, create, or substitute a store.
- Client frames use a 4-byte big-endian length and a maximum JSON payload of 512 KiB.
- Every client message carries `protocol_version = 1`; Rust `u64` identifiers cross the wire as decimal strings.
- Standard output is protocol-only. Standard error and tests must not log transcripts, prompts, generated text, or memory content.
- Cancellation acceptance is not terminal completion; clients drain until exactly one completed, cancelled, or failed event.
- Use test-first red-green-refactor for every production behavior.
- Use Conventional Commits with `Chris Xiong <xionghc713@gmail.com>` and no co-author trailers.

---

### Task 1: Versioned Client Wire Contract

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/protocol/Cargo.toml`
- Modify: `crates/protocol/src/lib.rs`
- Create: `crates/protocol/src/client_wire.rs`
- Create: `crates/protocol/tests/client_wire.rs`
- Create: `tests/fixtures/client-wire-v1/commands.jsonl`
- Create: `tests/fixtures/client-wire-v1/events.jsonl`
- Create: `tests/fixtures/client-wire-v1/invalid.jsonl`

**Interfaces:**
- Produces: `CLIENT_PROTOCOL_VERSION`, `MAX_CLIENT_FRAME_BYTES`, `ClientCommand`, `GatewayMessage`, `ClientRuntimeEvent`, `RuntimeStatus`, `ClientWireError`, `decode_client_command`, and `encode_gateway_message`.
- Produces: `ClientRuntimeEvent::try_from(RuntimeEvent)` for the text-event subset.
- Consumes: existing `RuntimeEvent`, `RuntimeError`, `QualityDecision`, `MemoryRetrievalTrace`, and identifier getters.

- [ ] **Step 1: Write failing wire-contract tests**

Add tests that require:

```rust
use conversation_protocol::{
    decode_client_command, encode_gateway_message, ClientCommand, GatewayMessage,
    CLIENT_PROTOCOL_VERSION, MAX_CLIENT_FRAME_BYTES,
};

#[test]
fn identifiers_round_trip_as_decimal_strings() {
    let command = decode_client_command(
        br#"{"protocol_version":1,"type":"start_turn","request_id":"req-1","turn_id":"18446744073709551615","transcript":"hello"}"#,
    )
    .unwrap();
    assert!(matches!(command, ClientCommand::StartTurn { turn_id, .. } if turn_id.get() == u64::MAX));
}

#[test]
fn unknown_fields_and_versions_are_rejected() {
    assert!(decode_client_command(
        br#"{"protocol_version":2,"type":"status","request_id":"req-1"}"#
    )
    .is_err());
    assert!(decode_client_command(
        br#"{"protocol_version":1,"type":"status","request_id":"req-1","extra":true}"#
    )
    .is_err());
}

#[test]
fn encoded_messages_never_use_numeric_u64_ids() {
    let encoded = encode_gateway_message(&GatewayMessage::RuntimeEvent {
        event: ClientRuntimeEvent::TurnCompleted { turn_id: TurnId::new(u64::MAX) },
    })
    .unwrap();
    assert!(std::str::from_utf8(&encoded).unwrap().contains("\"18446744073709551615\""));
}
```

Fixture tests must parse every line in `commands.jsonl` and `events.jsonl`, reject every line in `invalid.jsonl`, and assert `MAX_CLIENT_FRAME_BYTES == 512 * 1024`.

- [ ] **Step 2: Run the protocol tests and verify RED**

Run:

```bash
cargo test --locked -p conversation-protocol --test client_wire
```

Expected: compilation fails because the client-wire exports do not exist.

- [ ] **Step 3: Implement explicit wire DTOs and validation**

Add `serde` and `serde_json` to `conversation-protocol`. Implement discriminated enums with `#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]` and explicit `protocol_version` fields.

The command surface is exactly:

```rust
pub enum ClientCommand {
    Status { request_id: String },
    StartTurn { request_id: String, turn_id: TurnId, transcript: String },
    InterruptTurn { request_id: String, turn_id: TurnId },
}

pub fn decode_client_command(payload: &[u8]) -> Result<ClientCommand, ClientWireError>;
pub fn encode_gateway_message(message: &GatewayMessage) -> Result<Vec<u8>, ClientWireError>;
```

Validate request IDs as non-empty UTF-8 strings of at most 64 bytes, transcripts as non-empty values bounded by `MAX_CONVERSATION_MESSAGE_BYTES`, decimal IDs as canonical non-zero `u64`, and encoded payloads against `MAX_CLIENT_FRAME_BYTES`.

Project only these runtime events:

```rust
pub enum ClientRuntimeEvent {
    TurnStarted { turn_id: TurnId },
    QualityResolved { decision: ClientQualityDecision },
    MemoryRetrieved { trace: ClientMemoryTrace },
    TextDelta { turn_id: TurnId, delta: String },
    Timing { turn_id: TurnId, milestone: String, elapsed_ms: u64 },
    TurnCompleted { turn_id: TurnId },
    TurnCancelled { turn_id: TurnId },
    TurnFailed { turn_id: TurnId, error: ClientRuntimeError },
}
```

`ClientQualityDecision` contains mode, response-control names and numeric limits, signal names, history-message count, and context-source names. `ClientMemoryTrace` contains decimal trace and turn IDs, selected item count, and used bytes only. Unsupported speech/playback runtime events return a typed projection error rather than being silently omitted.

`GatewayMessage` includes `Ready`, `CommandAccepted`, `CommandRejected`, `Status`, `RuntimeEvent`, and `Fatal`. Each serialization includes `protocol_version` automatically.

- [ ] **Step 4: Run protocol tests and verify GREEN**

Run:

```bash
cargo test --locked -p conversation-protocol --test client_wire
cargo test --locked -p conversation-protocol
```

Expected: all protocol tests pass.

- [ ] **Step 5: Commit the wire contract**

```bash
git add Cargo.toml Cargo.lock crates/protocol tests/fixtures/client-wire-v1
git commit -m "feat: add versioned client wire contract"
```

### Task 2: Identity-Tagged Ollama Generation

**Files:**
- Modify: `crates/model-adapters/src/ollama.rs`
- Modify: `crates/model-adapters/tests/ollama.rs`
- Modify: `tests/voice/src/session_config.rs`

**Interfaces:**
- Consumes: `GenerationLanguageModel`, `GenerationLanguageRequest`, and `GenerationTextDelta` from `conversation-model-adapters`.
- Produces: `impl GenerationLanguageModel for OllamaLanguageModel`.
- Removes: probe-private `IdentityTaggedLanguageModel` and its duplicate generation buffer.

- [ ] **Step 1: Write failing identity and cancellation tests**

Add an Ollama test server test that calls the generation trait directly:

```rust
let model = OllamaLanguageModel::new(config_for(server.endpoint()));
let request = GenerationLanguageRequest::new(
    TurnId::new(7),
    GenerationId::new(11),
    "hello",
);
let mut deltas = GenerationLanguageModel::stream(&model, request, cancellation.clone());
let delta = deltas.recv().await.unwrap().unwrap();
assert_eq!(delta.turn_id(), TurnId::new(7));
assert_eq!(delta.generation_id(), GenerationId::new(11));
```

Add a cancellation case proving the mapped receiver closes only after the inner request observes cancellation and the fake server connection is reaped.

- [ ] **Step 2: Run the focused adapter test and verify RED**

Run:

```bash
cargo test --locked -p conversation-model-adapters --test ollama generation_language
```

Expected: compilation fails because `OllamaLanguageModel` does not implement `GenerationLanguageModel`.

- [ ] **Step 3: Implement the direct trait mapping**

Implement `GenerationLanguageModel::stream` on `OllamaLanguageModel` by converting the request with `LanguageModelRequest::from_input`, calling the existing `LanguageModel::stream`, and mapping every delta to `GenerationTextDelta::new(turn_id, generation_id, delta)`. Use a bounded channel of 32 items, race sends and receives against cancellation, and stop without an extra task or receiver leak.

Replace the private wrapper in `tests/voice/src/session_config.rs` with `Arc::new(self.language_model()?)` and remove now-unused imports/constants.

- [ ] **Step 4: Run adapter and voice configuration tests and verify GREEN**

Run:

```bash
cargo test --locked -p conversation-model-adapters --test ollama
cargo test --locked -p conversation-voice-probe session_config
```

Expected: all focused tests pass and no duplicate identity adapter remains.

- [ ] **Step 5: Commit the adapter seam**

```bash
git add crates/model-adapters tests/voice/src/session_config.rs
git commit -m "feat: expose identity-tagged local generation"
```

### Task 3: Text-Only Turn Runtime

**Files:**
- Modify: `crates/runtime/src/lib.rs`
- Create: `crates/runtime/src/text_turn.rs`
- Create: `crates/runtime/tests/text_turn.rs`

**Interfaces:**
- Produces: `TextTurnRuntime` and `TextTurnEventStream`.
- Consumes: `GenerationLanguageModel`, `ConversationQualityController`, and optional `MemoryContextProvider`.
- Guarantees: one active turn, strictly increasing turn/generation IDs, completed-only history, bounded events, interruption cleanup, and exactly one terminal event.

- [ ] **Step 1: Write failing completion and history tests**

Use `MockGenerationLanguageModel` and assert this API:

```rust
let runtime = TextTurnRuntime::new(language.clone())
    .with_quality_controller(controller);
let mut first = runtime
    .start_turn(TurnId::new(1), GenerationId::new(1), "hello")
    .await
    .unwrap();
assert_eq!(collect_terminal(&mut first).await, RuntimeEvent::TurnCompleted { turn_id: TurnId::new(1) });

let mut second = runtime
    .start_turn(TurnId::new(2), GenerationId::new(2), "again")
    .await
    .unwrap();
collect_terminal(&mut second).await;
assert_eq!(language.requests()[1].input().history().len(), 2);
```

Assert event order: `TurnStarted`, `QualityResolved`, optional `MemoryRetrieved`, text deltas with one first-text timing after the first delta, then one terminal event.

- [ ] **Step 2: Run the focused runtime test and verify RED**

Run:

```bash
cargo test --locked -p conversation-runtime --test text_turn completion
```

Expected: compilation fails because `TextTurnRuntime` does not exist.

- [ ] **Step 3: Implement minimal successful text lifecycle**

Implement:

```rust
impl TextTurnRuntime {
    pub fn new(language_model: Arc<dyn GenerationLanguageModel>) -> Self;
    pub fn with_quality_controller(self, controller: ConversationQualityController) -> Self;
    pub fn with_session_id(self, session_id: SessionId) -> Self;
    pub fn with_memory_provider(
        self,
        provider: Arc<dyn MemoryContextProvider>,
        language_execution: ExecutionLocation,
    ) -> Result<Self, RuntimeError>;
    pub async fn start_turn(
        &self,
        turn_id: TurnId,
        generation_id: GenerationId,
        transcript: impl Into<String>,
    ) -> Result<TextTurnEventStream, RuntimeError>;
    pub async fn interrupt(
        &self,
        turn_id: TurnId,
        generation_id: GenerationId,
    ) -> Result<(), RuntimeError>;
}
```

Follow the existing streaming runtime's quality → memory → language ordering. Keep generated text in a bounded UTF-8-safe buffer. Emit no speech events. Use a bounded 32-event channel plus an independent terminal one-shot. Serialize quality completion/discard/interruption with active-turn removal.

- [ ] **Step 4: Add failing cancellation and failure tests**

Add tests for cancellation while language output is blocked, a dropped or blocked event consumer, wrong turn/generation interruption, duplicate/lower IDs after completion, adapter error, adapter panic, memory failure before language execution, and runtime reuse after every terminal path.

Each test must assert exactly one terminal event and that cancelled/failed partial output is absent from the next request history.

- [ ] **Step 5: Run the expanded runtime tests and verify RED**

Run:

```bash
cargo test --locked -p conversation-runtime --test text_turn
```

Expected: new edge cases fail until cleanup, ordering, and ID guards are implemented.

- [ ] **Step 6: Complete cleanup and failure handling**

Add cancellation-aware sends, panic containment around adapter entry, receiver draining after cancellation, terminal selection after cleanup, strict monotonic ID state, and memory-error mapping to `RuntimeStage::Memory`. `TextTurnEventStream::recv` must deliver the independent terminal after the bounded event channel closes.

- [ ] **Step 7: Run runtime tests and verify GREEN**

Run:

```bash
cargo test --locked -p conversation-runtime --test text_turn
cargo test --locked -p conversation-runtime
```

Expected: all runtime tests pass.

- [ ] **Step 8: Commit the text runtime**

```bash
git add crates/runtime
git commit -m "feat: add text-only turn runtime"
```

### Task 4: Gateway Configuration and Framing

**Files:**
- Modify: `Cargo.toml`
- Create: `apps/runtime-gateway/Cargo.toml`
- Create: `apps/runtime-gateway/src/lib.rs`
- Create: `apps/runtime-gateway/src/config.rs`
- Create: `apps/runtime-gateway/src/framing.rs`
- Create: `apps/runtime-gateway/tests/config.rs`
- Create: `apps/runtime-gateway/tests/framing.rs`
- Create: `configs/gateway.example.toml`

**Interfaces:**
- Produces: `GatewayConfig::load(&Path)`, `GatewayAdapters`, `FrameReader<R>`, and `FrameWriter<W>`.
- Consumes: public client-wire encode/decode functions and existing Ollama/memory constructors.

- [ ] **Step 1: Write failing configuration tests**

Test an explicit valid local-only file and reject: relative config path, file over 64 KiB, unknown fields, wrong schema, remote or hostname endpoint, HTTPS local endpoint, credentials/query/fragment, empty model, relative memory path, missing memory file, and uninitialized SQLite schema.

The accepted TOML shape is exactly:

```toml
schema_version = 1
privacy_mode = "local-only"

[language]
backend = "ollama-compatible"
endpoint = "http://127.0.0.1:11434"
model = "local-model-id"
thinking = false
temperature = 0.7
seed = 42
num_predict = 1024
num_ctx = 8192
max_assistant_content_bytes = 65536

[persona]
mode = "direct-answer"
warmth = 80
humor = 60
teasing = 40
initiative = 35
directness = 80
intimacy = 30
verbosity = 20
follow_up_frequency = 25

# Optional; when present the file must already exist and be initialized.
# [memory]
# database = "/absolute/path/to/runtime.sqlite3"
# maximum_items = 4
# maximum_bytes = 4096
```

- [ ] **Step 2: Write failing frame-codec tests**

Test fragmented headers/payloads, coalesced frames, zero length, 512 KiB exact limit, oversized header rejection before payload read, truncated EOF, invalid UTF-8 delegated to wire decoding, and writer output containing exactly one big-endian length followed by payload.

- [ ] **Step 3: Run gateway library tests and verify RED**

Run:

```bash
cargo test --locked -p conversation-runtime-gateway --test config --test framing
```

Expected: compilation fails because the gateway package does not exist.

- [ ] **Step 4: Implement bounded configuration and framing**

Register `apps/runtime-gateway` in the workspace. Use `serde(deny_unknown_fields)` for configuration. Reuse public constructors to validate persona and adapter bounds. Validate numeric loopback with `IpAddr::is_loopback` before constructing `OllamaConfig`.

`FrameReader<R: AsyncRead + Unpin>` reads exactly four header bytes, rejects zero or over-limit lengths, allocates only after validation, and distinguishes clean EOF before a header from truncation. `FrameWriter<W: AsyncWrite + Unpin>` refuses oversized payloads and flushes each complete frame.

- [ ] **Step 5: Run gateway library tests and verify GREEN**

Run:

```bash
cargo test --locked -p conversation-runtime-gateway --test config --test framing
```

Expected: all tests pass.

- [ ] **Step 6: Commit configuration and framing**

```bash
git add Cargo.toml Cargo.lock apps/runtime-gateway configs/gateway.example.toml
git commit -m "feat: add local gateway configuration and framing"
```

### Task 5: Persistent Gateway Session Process

**Files:**
- Create: `apps/runtime-gateway/src/session.rs`
- Create: `apps/runtime-gateway/src/main.rs`
- Create: `apps/runtime-gateway/tests/gateway_cli.rs`

**Interfaces:**
- Produces: `GatewaySession::run(reader, writer)` and binary `conversation-runtime-gateway --config <absolute-path>`.
- Consumes: `TextTurnRuntime`, `GatewayConfig`, framed I/O, and client-wire messages.

- [ ] **Step 1: Write failing deterministic process tests**

Add a test-only deterministic generation backend selected only under the integration-test harness, not by public configuration. Spawn the binary with piped stdio and assert:

1. `ready` is the first frame;
2. `status` returns local-only language, disabled or local memory, stdio transport, and telemetry disabled;
3. `start_turn` receives `command_accepted` before `turn_started` and streams text to one terminal completion;
4. two turns share completed history;
5. `interrupt_turn` is acknowledged and ends with one cancelled terminal;
6. malformed JSON produces command rejection without exit;
7. oversized or truncated framing produces one fatal frame when writable and exits nonzero;
8. EOF cancels and reaps active generation;
9. stdout contains frames only and stderr contains no fixture transcript or generated text.

- [ ] **Step 2: Run the process test and verify RED**

Run:

```bash
cargo test --locked -p conversation-runtime-gateway --test gateway_cli
```

Expected: compilation fails because the session and binary entry point do not exist.

- [ ] **Step 3: Implement command routing and writer ownership**

Create one bounded writer channel and one writer task. After successful config, adapter, quality, and optional memory initialization, send `GatewayMessage::Ready`.

The reader loop decodes one command at a time. `status` responds with accepted then status. `start_turn` validates no active forwarding task, starts `TextTurnRuntime`, sends accepted, then spawns a task that drains events and sends projected runtime messages. `interrupt_turn` calls runtime interruption without awaiting the forwarding task, then sends accepted. Rejected commands return a bounded `ClientRuntimeError` and keep the process alive.

Never hold session state or active-task locks while awaiting writer capacity. On stdin EOF or fatal framing, cancel the active turn, await its terminal cleanup, close the writer channel, and await the writer task.

- [ ] **Step 4: Run the process test and verify GREEN**

Run:

```bash
cargo test --locked -p conversation-runtime-gateway --test gateway_cli
cargo test --locked -p conversation-runtime-gateway
```

Expected: all gateway tests pass with no leaked child processes.

- [ ] **Step 5: Commit the gateway process**

```bash
git add apps/runtime-gateway
git commit -m "feat: add persistent local runtime gateway"
```

### Task 6: TypeScript Client and Stdio Transport

**Files:**
- Create: `package.json`
- Create: `package-lock.json`
- Create: `packages/typescript/package.json`
- Create: `packages/typescript/tsconfig.json`
- Create: `packages/typescript/src/protocol.ts`
- Create: `packages/typescript/src/framing.ts`
- Create: `packages/typescript/src/client.ts`
- Create: `packages/typescript/src/stdio.ts`
- Create: `packages/typescript/src/index.ts`
- Create: `packages/typescript/test/protocol.test.ts`
- Create: `packages/typescript/test/framing.test.ts`
- Create: `packages/typescript/test/client.test.ts`
- Create: `packages/typescript/test/stdio.test.ts`

**Interfaces:**
- Produces: package `@conversation/runtime` with `RuntimeTransport`, `RuntimeClient`, `RuntimeTurn`, `StdioGatewayTransport`, and validated protocol unions.
- Consumes: `tests/fixtures/client-wire-v1` and the compiled gateway binary for process smoke.

- [ ] **Step 1: Create package metadata and write failing protocol tests**

Use npm workspaces and pin TypeScript in the root lockfile. The package builds ESM to `dist`, emits declarations, and runs Node's test runner against compiled tests.

Tests must load shared fixtures, parse decimal IDs to `bigint`, reject numeric IDs, unknown fields, unsupported versions, duplicate terminal events, text after terminal, and missing required fields.

- [ ] **Step 2: Run TypeScript tests and verify RED**

Run:

```bash
npm ci
npm test --workspace @conversation/runtime
```

Expected: build or tests fail because protocol and client implementations do not exist.

- [ ] **Step 3: Implement strict protocol types and frame codec**

Define discriminated unions matching Rust fixtures. Runtime validators return typed values rather than using unchecked casts. Convert canonical decimal strings with `BigInt`, reject zero, signs, leading zeros, whitespace, and values above `2n ** 64n - 1n`.

Implement an incremental frame decoder with a 512 KiB cap that accepts fragmented and coalesced `Uint8Array` chunks. Implement frame encoding with a four-byte big-endian length.

- [ ] **Step 4: Add failing client-routing and transport tests**

Use an in-memory `RuntimeTransport` to prove `RuntimeClient.connect`, request correlation, status, turn event async iteration, interruption, exactly-one terminal enforcement, transport failure propagation, and clean close. Use a fake executable process to prove `StdioGatewayTransport` requires absolute gateway/config paths, spawns without a shell, drains stderr with a fixed byte cap, and rejects pending work on exit.

- [ ] **Step 5: Run client tests and verify RED**

Run:

```bash
npm test --workspace @conversation/runtime
```

Expected: routing and process tests fail before client/transport implementation.

- [ ] **Step 6: Implement the transport-neutral client and Node adapter**

Implement exactly:

```ts
export interface RuntimeTransport {
  readonly messages: AsyncIterable<unknown>;
  send(message: ClientCommand): Promise<void>;
  close(): Promise<void>;
}

export class RuntimeClient {
  static connect(transport: RuntimeTransport): Promise<RuntimeClient>;
  status(): Promise<RuntimeStatus>;
  startTurn(transcript: string): RuntimeTurn;
  interrupt(turnId: bigint): Promise<void>;
  close(): Promise<void>;
}

export class StdioGatewayTransport implements RuntimeTransport {
  static start(options: { gatewayPath: string; configPath: string }): Promise<StdioGatewayTransport>;
}
```

Use monotonic `bigint` turn and request counters. Continuously consume transport messages in one routing task. Resolve start only after command acceptance; retain the turn queue until its terminal event. Reject every pending operation on fatal, EOF, malformed input, or process exit.

- [ ] **Step 7: Run TypeScript tests and verify GREEN**

Run:

```bash
npm run build --workspace @conversation/runtime
npm test --workspace @conversation/runtime
```

Expected: all TypeScript tests pass.

- [ ] **Step 8: Commit the TypeScript SDK**

```bash
git add package.json package-lock.json packages/typescript
git commit -m "feat: add TypeScript runtime client"
```

### Task 7: Node Chat Example and Cross-Language Smoke

**Files:**
- Modify: `package.json`
- Create: `examples/node-chat/package.json`
- Create: `examples/node-chat/tsconfig.json`
- Create: `examples/node-chat/src/main.ts`
- Create: `examples/node-chat/test/cli.test.ts`
- Modify: `README.md`
- Modify: `ROADMAP.md`
- Modify: `docs/architecture.md`
- Create: `docs/r6-local-gateway-evaluation.md`

**Interfaces:**
- Produces: `npm run chat --workspace conversation-node-chat -- --gateway <absolute-path> --config <absolute-path>`.
- Consumes: `@conversation/runtime` public exports only.

- [ ] **Step 1: Write failing Node CLI tests**

Test argument validation, two prompts through one fake transport, streamed UTF-8 deltas, `SIGINT` mapping to one interruption, second signal/EOF cleanup, nonzero exit on gateway failure, and absence of transcript text from diagnostics.

- [ ] **Step 2: Run the example tests and verify RED**

Run:

```bash
npm test --workspace conversation-node-chat
```

Expected: package or CLI implementation is missing.

- [ ] **Step 3: Implement the minimal persistent chat CLI**

Use `node:readline/promises`. Require absolute `--gateway` and `--config` arguments. Print privacy status before the first prompt, stream only `text_delta` values to stdout, print terminal state on its own line, and keep the same `RuntimeClient` alive for subsequent turns. During a turn, the first `SIGINT` calls `interrupt`; when idle or after a second signal, close and exit.

- [ ] **Step 4: Add the real cross-language process smoke**

Build `conversation-runtime-gateway`, point the TypeScript stdio transport at its absolute target path, run deterministic test configuration, and assert ready → accepted → text delta → completed plus an independent cancellation run. This smoke must use actual framed pipes across Rust and Node, not an in-memory transport.

- [ ] **Step 5: Run example and smoke tests and verify GREEN**

Run:

```bash
npm run build --workspaces
npm test --workspaces
```

Expected: all Node and cross-language tests pass.

- [ ] **Step 6: Document usage, boundaries, and measured evidence**

Update the architecture diagram with `Node/Tauri client ↔ framed stdio gateway ↔ text runtime ↔ local adapter`, explicitly showing no listener. Mark only this R6 slice complete in `ROADMAP.md`; leave Tauri, persona/memory mutation controls, packaging, and R3 human/acoustic acceptance open.

Document deterministic commands and one manual local smoke template in `docs/r6-local-gateway-evaluation.md`. Do not publish private model IDs, paths, generated conversation text, or claim latency/model quality from the smoke.

- [ ] **Step 7: Commit the example and documentation**

```bash
git add package.json package-lock.json examples/node-chat README.md ROADMAP.md docs
git commit -m "feat: add local gateway chat example"
```

### Task 8: Independent Review and Final Verification

**Files:**
- Modify only files required by concrete review findings.

**Interfaces:**
- Verifies every acceptance criterion in `docs/superpowers/specs/2026-08-04-r6-local-gateway-design.md`.

- [ ] **Step 1: Run focused static and deterministic gates**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
npm ci
npm run build --workspaces
npm test --workspaces
git diff --check master...HEAD
```

Expected: every command exits zero with no warnings.

- [ ] **Step 2: Review lifecycle and privacy invariants**

Inspect and test that blocked client output cannot block command intake, interruption never becomes completion, every accepted start has exactly one terminal event, runtime work is reaped on EOF/exit, no listener opens, local endpoint validation is fail-closed, configured memory never silently disappears, and logs remain content-free.

- [ ] **Step 3: Run an explicit private local Ollama smoke**

Using a private absolute configuration outside the repository, run:

```bash
cargo build --locked -p conversation-runtime-gateway
npm run chat --workspace conversation-node-chat -- \
  --gateway "$PWD/target/debug/conversation-runtime-gateway" \
  --config "/absolute/private/gateway.toml"
```

Expected: ready status reports local-only; two turns stream text and complete; one longer turn can be interrupted. Record only command versions, terminal outcomes, and content-free timings in the evaluation document.

- [ ] **Step 4: Apply review fixes test-first**

For each concrete finding, add a reproducing failing test, run it to confirm RED, make the smallest fix, and rerun the focused and full gates.

- [ ] **Step 5: Commit final review evidence**

```bash
git add -A
git commit -m "docs: record R6 gateway verification"
```

- [ ] **Step 6: Confirm branch readiness without pushing**

Run:

```bash
git status --short
git log --oneline master..HEAD
git diff --stat master...HEAD
```

Expected: clean feature branch with reviewed commits. Do not push until the user says the literal word `push`.
