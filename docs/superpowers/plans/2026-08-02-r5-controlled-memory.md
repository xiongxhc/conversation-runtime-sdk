# R5 Controlled Memory Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver an explicitly initialized, inspectable SQLite memory store with conservative promotion, strict retrieval budgets and traces, runtime context injection, and a local control probe.

**Architecture:** Portable memory records and trace types live in `conversation-protocol`. A new `conversation-memory` crate provides the backend-neutral store/provider contracts and a `rusqlite 0.40.1` bundled-SQLite reference implementation. The runtime optionally retrieves typed context before generation; voice configuration and a dedicated probe opt into the store explicitly.

**Tech Stack:** Rust 2021, Tokio, `rusqlite 0.40.1` with bundled SQLite, Serde/TOML in existing probe crates, temporary-file integration tests.

## Global Constraints

- The runtime never persists transcripts or generated responses automatically.
- Identity and relationship records require explicit confirmation evidence and a separate approval operation.
- Relationship memory is fallible context and never directly commands affectionate behavior.
- The database path must be absolute; initialization is explicit and voice startup never creates a database.
- Query text and memory content never enter retrieval logs or telemetry.
- Retrieval allows at most eight items and `8192` content bytes; the runtime defaults to four items and `4096` bytes.
- `LocalOnly` never falls back to a remote or empty memory result after a configured local store fails.
- R5 never injects memory into a remote language adapter; export consent is deferred.
- Working memory cannot be pinned and always expires.
- Every mutation uses an expected revision and stale writes fail.
- R3 acoustic and ten-minute human acceptance remain blocked until their documented external evidence exists.
- Public examples remain backend- and venture-neutral.

---

### Task 1: Portable Memory Contracts

**Files:**
- Create: `crates/protocol/src/memory.rs`
- Modify: `crates/protocol/src/lib.rs`
- Modify: `crates/protocol/src/quality.rs`
- Modify: `crates/protocol/src/error.rs`
- Test: `crates/protocol/tests/memory_contracts.rs`
- Test: `crates/protocol/tests/quality_contracts.rs`

**Interfaces:**
- Produces: `MemoryId`, `RetrievalTraceId`, `MemoryKind`, `MemoryState`, `MemoryRetention`, `MemoryProvenanceKind`, `MemoryProvenance`, `MemoryApproval`, `MemoryApprovalEvidence`, `MemoryConfidence`, `UnixTimestampMillis`, `MemoryDraft`, `MemoryPatch`, `MemoryRecord`, `MemoryRetrievalRequest`, `MemoryContextItem`, `MemoryRetrievalReason`, and `MemoryRetrievalTrace`.
- Produces: `RuntimeStage::Memory` and reliable `RuntimeEvent::MemoryRetrieved` metadata.

- [x] **Step 1: Write failing validation and serialization tests**

Test exact enum strings, non-zero identifiers, timestamp/confidence bounds,
visible source/actor/timestamp provenance, approval evidence, `4 KiB` record
content, working-expiry requirements, retrieval limits, unique context items,
and content-free trace JSON.

- [x] **Step 2: Run the protocol tests and confirm RED**

Run: `cargo test --locked -p conversation-protocol --test memory_contracts`

Expected: compilation fails because `conversation_protocol::memory` and its
public types do not exist.

- [x] **Step 3: Implement the minimal public types**

Use private fields plus validated constructors and getters. Keep JSON metric
formatting explicit and content-free, matching the existing quality-event style.

- [x] **Step 4: Add retrieved-memory context and memory failure stage**

Extend existing exhaustive string mappings and tests without changing old enum
strings or terminal behavior.

- [x] **Step 5: Run focused tests and confirm GREEN**

Run: `cargo test --locked -p conversation-protocol --test memory_contracts --test quality_contracts`

- [x] **Step 6: Commit the contract boundary**

```bash
git add crates/protocol
git commit -m "feat: add controlled memory contracts"
```

### Task 2: SQLite Store and Migrations

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `crates/memory/Cargo.toml`
- Create: `crates/memory/src/lib.rs`
- Create: `crates/memory/src/error.rs`
- Create: `crates/memory/src/store.rs`
- Create: `crates/memory/src/sqlite.rs`
- Create: `crates/memory/migrations/0001_controlled_memory.sql`
- Test: `crates/memory/tests/sqlite_lifecycle.rs`
- Test: `crates/memory/tests/sqlite_schema.rs`

**Interfaces:**
- Consumes: Task 1 memory protocol types.
- Produces: synchronous `MemoryStore` CRUD/control trait.
- Produces: `SqliteMemoryStore::{initialize, open, create, list, inspect, edit, pin, approve, expire, delete}` with expected-revision mutations.
- Produces: static `SCHEMA_VERSION: u32 = 1`.

- [x] **Step 1: Write failing lifecycle tests**

Tests require an absolute path, prove `open` never creates a file, prove
`initialize` creates the parent and schema explicitly, and reject foreign or
newer schema versions, changed migration checksums, symbolic-link leaves,
unsafe new-file permissions, and failed foreign-key validation.

- [x] **Step 2: Run lifecycle tests and confirm RED**

Run: `cargo test --locked -p conversation-memory --test sqlite_lifecycle`

Expected: package or store types are missing.

- [x] **Step 3: Add the crate, migration, and connection policy**

Pin `rusqlite = { version = "0.40.1", features = ["bundled"] }`, enable foreign
keys, WAL, a `250 ms` busy timeout, checked schema identity, and immediate
transactions for mutations.

- [x] **Step 4: Write failing CRUD, revision, and deletion tests**

Prove all record metadata round-trips, stale revisions fail, pin/unpin restores
retention without approving, and hard deletion removes record, source,
approval, and retrieval-item references.

- [x] **Step 5: Implement minimal CRUD and inspection controls**

Map checked SQLite values to typed protocol constructors. Do not expose a raw
connection or accept unvalidated enum strings.

- [x] **Step 6: Run store tests and confirm GREEN**

Run: `cargo test --locked -p conversation-memory --test sqlite_lifecycle --test sqlite_schema`

- [x] **Step 7: Commit the durable store boundary**

```bash
git add Cargo.toml Cargo.lock crates/memory
git commit -m "feat: add explicit SQLite memory store"
```

### Task 3: Promotion, Expiration, and Bounded Retrieval

**Files:**
- Modify: `crates/memory/src/store.rs`
- Modify: `crates/memory/src/sqlite.rs`
- Create: `crates/memory/src/retrieval.rs`
- Test: `crates/memory/tests/promotion.rs`
- Test: `crates/memory/tests/retrieval.rs`

**Interfaces:**
- Produces: `MemoryStore::retrieve(MemoryRetrievalRequest) -> MemoryRetrieval`.
- Produces: `MemoryStore::prune_expired(now) -> usize`.
- Produces: two-step candidate approval and deterministic lexical ranking.

- [x] **Step 1: Write failing promotion and exact-boundary expiry tests**

Prove identity/relationship inserts remain candidates, approval without explicit
evidence fails, confirmed approval activates, a single completed exchange cannot
approve, working memory cannot be pinned, working memory requires expiry within
24 hours, session-end cleanup expires session records, and `now == expires_at`
is expired.

- [x] **Step 2: Run promotion tests and confirm RED**

Run: `cargo test --locked -p conversation-memory --test promotion`

- [x] **Step 3: Implement conservative states and expiration**

Perform state transitions transactionally, write revisions, and run expiry before
inspection and retrieval results are returned.

- [x] **Step 4: Write failing multilingual ranking and budget tests**

Cover exact phrases, normalized Latin terms, Chinese character pairs, pinned
matching records, stable tie-breakers, oversized-record skipping, item/byte caps,
query non-persistence, and trace exclusion counts.

- [x] **Step 5: Run retrieval tests and confirm RED**

Run: `cargo test --locked -p conversation-memory --test retrieval`

- [x] **Step 6: Implement deterministic retrieval and traces**

Rank only active, unexpired records. Commit selected items, trace rows, and
last-use metadata atomically only when cancellation is clear. Never truncate
content or persist the query.

- [x] **Step 7: Run all memory tests and confirm GREEN**

Run: `cargo test --locked -p conversation-memory`

- [x] **Step 8: Commit controlled retrieval**

```bash
git add crates/memory
git commit -m "feat: add bounded memory retrieval"
```

### Task 4: Runtime and Language Adapter Integration

**Files:**
- Modify: `crates/memory/src/lib.rs`
- Create: `crates/memory/src/provider.rs`
- Modify: `crates/runtime/Cargo.toml`
- Create: `crates/runtime/src/memory_context.rs`
- Modify: `crates/runtime/src/lib.rs`
- Modify: `crates/runtime/src/conversation_quality.rs`
- Modify: `crates/runtime/src/streaming_turn.rs`
- Modify: `crates/model-adapters/src/language_model.rs`
- Modify: `crates/model-adapters/src/ollama.rs`
- Test: `crates/runtime/tests/memory_context.rs`
- Test: `crates/runtime/tests/streaming_turn.rs`
- Test: `crates/model-adapters/tests/ollama.rs`

**Interfaces:**
- Produces: cancellation-aware `MemoryContextProvider` returning a boxed future.
- Produces: `SqliteMemoryContextProvider` using bounded blocking operations.
- Produces: `StreamingTurnRuntime::with_memory_provider` with explicit language execution and `VoiceSessionAdapters::with_memory_provider`.
- Produces: `LanguageModelInput::with_quality_and_memory` and `memory_items()`.

- [x] **Step 1: Write failing adapter serialization tests**

Prove memory items are a separate ordered context message, are labeled fallible
and non-instructional, fit within the declared memory budget, and do not alter
the existing no-memory request body.

- [x] **Step 2: Run adapter tests and confirm RED**

Run: `cargo test --locked -p conversation-model-adapters --test ollama memory_`

- [x] **Step 3: Implement typed language input and Ollama translation**

Preserve deployment guidance, runtime guidance, recent history, memory context,
and current input in that order. Never merge memory into the user transcript.

- [x] **Step 4: Write failing runtime provider tests**

Prove optional-provider behavior, retrieval-before-generation, content-free
trace event publication, memory-stage failure, pre/post-operation cancellation,
no metadata writes after cancellation, reliable trace delivery before generation,
total `40 KiB` input enforcement, and deletion preventing later context.

- [x] **Step 5: Run runtime tests and confirm RED**

Run: `cargo test --locked -p conversation-runtime --test memory_context`

- [x] **Step 6: Implement provider and turn integration**

Keep the existing path byte-for-byte equivalent when no provider is configured.
Register active cancellation and terminal ownership before retrieving inside the
turn task. Retrieve before constructing `LanguageModelInput`; configured failures fail
closed rather than silently continuing with empty memory. Reject memory injection
when the active language descriptor is remote.

- [x] **Step 7: Run focused integration tests and confirm GREEN**

Run: `cargo test --locked -p conversation-model-adapters --test ollama memory_`

Run: `cargo test --locked -p conversation-runtime --test memory_context --test streaming_turn`

- [x] **Step 8: Commit runtime memory context**

```bash
git add crates/memory crates/runtime crates/model-adapters
git commit -m "feat: inject traced memory context"
```

### Task 5: Voice Configuration and Memory Probe

**Files:**
- Modify: `Cargo.toml`
- Create: `tests/memory/Cargo.toml`
- Create: `tests/memory/src/main.rs`
- Create: `tests/memory/tests/probe_cli.rs`
- Modify: `tests/voice/Cargo.toml`
- Modify: `tests/voice/src/session_config.rs`
- Modify: `tests/voice/src/bin/conversation-voice-loop.rs`
- Modify: `tests/voice/tests/continuous_cli.rs`
- Modify: `configs/voice-session.example.toml`

**Interfaces:**
- Produces: binary `conversation-memory-probe` with explicit init/CRUD/retrieve commands.
- Produces: schema-v2 `[memory_store]` configuration and local SQLite provider wiring.

- [x] **Step 1: Write failing probe CLI tests**

Use one temporary database to exercise `init`, `add`, `list`, `inspect`, `edit`,
`pin`, `approve`, `expire`, `delete`, and `retrieve`. Assert stable output and
non-zero exits without echoing rejected content.

- [x] **Step 2: Run probe tests and confirm RED**

Run: `cargo test --locked -p conversation-memory-probe --test probe_cli`

- [x] **Step 3: Implement the minimal manual parser and commands**

Match existing probe conventions, require explicit absolute database paths, and
keep `default-path` read-only.

- [x] **Step 4: Write failing voice preflight and turn tests**

Reject descriptor/store mismatches, remote language or memory descriptors,
multiple memory descriptors,
relative paths, missing databases, unsupported schemas, zero/oversized budgets,
and any failure after sidecar spawn. Prove one configured fake turn receives a
retrieved record and emits a content-free trace.

- [x] **Step 5: Run voice tests and confirm RED**

Run: `cargo test --locked -p conversation-voice-probe --test continuous_cli memory_`

- [x] **Step 6: Implement voice configuration and provider wiring**

Open and validate the existing local database before the sidecar factory starts.
Memory remains absent unless both the local descriptor and store table are present.

- [x] **Step 7: Run probe and voice tests and confirm GREEN**

Run: `cargo test --locked -p conversation-memory-probe --test probe_cli`

Run: `cargo test --locked -p conversation-voice-probe --test continuous_cli memory_`

- [x] **Step 8: Commit the testable control surface**

```bash
git add Cargo.toml Cargo.lock tests/memory tests/voice configs/voice-session.example.toml
git commit -m "feat: add local memory controls"
```

### Task 6: Documentation and Full Verification

**Files:**
- Modify: `README.md`
- Modify: `ROADMAP.md`
- Modify: `docs/architecture.md`
- Create: `docs/r5-controlled-memory-evaluation.md`
- Modify: `docs/superpowers/specs/2026-08-02-r5-controlled-memory-design.md`
- Modify: `docs/superpowers/plans/2026-08-02-r5-controlled-memory.md`

**Interfaces:**
- Produces: public setup, inspection, deletion, voice opt-in, and acceptance instructions.

- [x] **Step 1: Document the actual implemented commands and boundaries**

Include the default macOS location, explicit `init`, no automatic capture,
confirmation-backed identity/relationship approval, working pin rejection,
strict budgets, trace interpretation,
database sidecar files, FileVault boundary, and voice configuration.
Document that deletion cannot retract memory already copied into an in-flight
language request; cancelling the current turn is required.

- [x] **Step 2: Record R5 source status honestly**

Mark only deterministic criteria demonstrated by tests. Explain that hard delete
is not cryptographic secure erasure. Do not claim desktop UI, semantic-search
quality, encrypted SQLite, or R3 human/acoustic acceptance.

- [x] **Step 3: Run focused and full gates**

Run: `cargo fmt --all -- --check`

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`

Run: `cargo test --workspace --locked --no-fail-fast`

Run: `tests/voice/acceptance-macos.test.sh`

Run: `VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" swift test --package-path platform/macos/voice-sidecar`

- [x] **Step 4: Run repository hygiene checks**

Run: `git diff --check`

Run: `grep -R -n -E 'TBD|TODO|FIXME|/Users/cx|xionghc|abliterated' --exclude-dir=.git --exclude-dir=target --exclude-dir=.build .`

Classify pre-existing historical benchmark identifiers separately; remove new
private paths, placeholders, or venture-specific content.

- [x] **Step 5: Commit the verified milestone record**

```bash
git add README.md ROADMAP.md docs
git commit -m "docs: record controlled memory milestone"
```
