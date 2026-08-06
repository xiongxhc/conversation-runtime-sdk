# R6 Desktop Memory Inspection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a real read-only desktop Memory screen backed by the existing local controlled-memory store through a versioned public runtime protocol.

**Architecture:** Protocol v2 adds correlated memory-list and memory-inspect requests with strict DTOs, bigint-safe decimal fields, typed error codes, and immutable-ID keyset pagination. The gateway retains a clone of the configured local store for inspection only while no turn is active; the browser-safe TypeScript SDK exposes typed methods, and the desktop renders a capability-gated read-only Memory view without opening SQLite directly.

**Tech Stack:** Rust 1.97.1, Tokio, rusqlite, serde, Tauri 2.11, TypeScript 5.5, React 19.2, Vitest 4.1, Testing Library 16.3.

## Global Constraints

- Advance the framed client protocol from version 1 to version 2; v1 and v2 reject each other rather than silently negotiating incompatible capabilities.
- Keep the public protocol backend-neutral; do not expose SQLite paths, Ollama identifiers, desktop components, or private deployment choices.
- Memory inspection is available only when status verifies `local_only`, local language, local memory, telemetry disabled, and `memory_inspection` capability.
- Do not expose create, edit, approve, pin, unpin, expire, retrieve, or delete operations.
- Do not inspect memory while a text turn is active; the gateway is authoritative for the race and must leave interruption handling immediate.
- Use a fixed 50-record page and immutable `id DESC` keyset traversal.
- Limit normalized list previews to 192 UTF-8 bytes, including the final ellipsis when truncated.
- Limit provenance and approval histories to their latest 32 entries each, expose truncation flags, and prove the largest valid response remains within one frame.
- Encode identifiers, revisions, timestamps, and session identifiers as canonical decimal strings; TypeScript represents them as `bigint`.
- Keep provenance and approval arrays oldest-to-newest with the current evidence last.
- Omit content digests from client DTOs, UI, logs, errors, and telemetry.
- React renders memory content as text only, never HTML or Markdown.
- Opening Memory may apply existing due-expiry housekeeping, but must not perform retrieval or update retrieval metadata.
- Public source and documentation remain backend- and venture-neutral.
- Push and merge remain separate explicit integration actions.

---

## File Structure

### Public Rust protocol

- `crates/protocol/src/client_memory.rs` — browser-facing memory DTOs, projections, cursor, preview normalization, enum names, and decimal serialization.
- `crates/protocol/src/client_wire.rs` — protocol-v3 commands, correlated responses, stable error codes, strict decode/encode, and frame validation.
- `crates/protocol/src/lib.rs` — exports client memory DTOs and page/preview constants.
- `crates/protocol/tests/client_wire.rs` — v2 command/message fixtures, v1 rejection, identifiers, timestamps, cursors, errors, and frame bounds.
- `tests/fixtures/client-wire-v3/commands.jsonl` — canonical valid v3 commands.
- `tests/fixtures/client-wire-v3/events.jsonl` — canonical valid v3 gateway messages.
- `tests/fixtures/client-wire-v3/invalid.jsonl` — malformed v3 commands and messages.
- `tests/fixtures/client-wire-v1/*` — retained historical fixtures used only to prove v2 rejects v1.

### Controlled memory store

- `crates/memory/src/store.rs` — `MemoryPage`, `BoundedMemoryInspection`, and bounded list/inspect contracts.
- `crates/memory/src/sqlite.rs` — expiry-aware keyset paging and latest-history queries with one-record lookahead.
- `crates/memory/src/lib.rs` — exports inspection paging types.
- `crates/memory/tests/inspection.rs` — page boundaries, stable traversal, bounded history, expiry, and retrieval-metadata invariants.

### Gateway

- `apps/runtime-gateway/src/config.rs` — retain one cloned `SqliteMemoryStore` beside the retrieval provider.
- `apps/runtime-gateway/src/main.rs` — attach optional inspection store and advertise protocol-v3 capability.
- `apps/runtime-gateway/src/session.rs` — memory commands, no-active-turn gate, blocking store work, typed rejections, and correlated responses.
- `apps/runtime-gateway/tests/config.rs` — configured store/provider share the validated path.
- `apps/runtime-gateway/tests/gateway_cli.rs` — compiled framed-stdio memory list/inspect and rejection behavior.

### TypeScript SDK

- `packages/typescript/src/protocol.ts` — v2 memory types, parser/encoder, bigint fields, capabilities, cursor, and error codes.
- `packages/typescript/src/client.ts` — `listMemories` and `inspectMemory` pending-control lifecycle.
- `packages/typescript/src/browser.ts` — browser-safe memory exports.
- `packages/typescript/src/index.ts` — root memory exports.
- `packages/typescript/test/protocol.test.ts` — strict v2 wire and v1 rejection.
- `packages/typescript/test/client.test.ts` — acceptance, correlation, close, and failure semantics.
- `packages/typescript/test/browser.test.ts` — browser entry exposes memory types without Node transport.
- `packages/typescript/test/stdio.test.ts` — migrate raw transport fixtures to protocol v2.
- `examples/node-chat/test/cli.test.ts` — update protocol fixture status to v2 without adding memory UI to the CLI.

### Desktop

- `apps/desktop/src/runtime/conversation-session.ts` — typed memory list/inspect forwarding with ready-state enforcement.
- `apps/desktop/src/components/MemoryPane.tsx` — list, keyset pagination, detail, provenance, approvals, retry, and empty states.
- `apps/desktop/src/components/Workspace.tsx` — capability-gated navigation and active-turn disablement.
- `apps/desktop/src/styles.css` — responsive light/dark Memory layout and accessible state styles.
- `apps/desktop/test/conversation-session.test.ts` — desktop session forwarding and active-turn rejection.
- `apps/desktop/test/tauri-transport.test.ts` — migrate framed transport fixture to protocol v2.
- `apps/desktop/test/memory-pane.test.tsx` — focused rendering and pagination behavior.
- `apps/desktop/test/app.test.tsx` — navigation visibility, History separation, and no mutation controls.

### Documentation and temporary acceptance state

- `apps/desktop/README.md` — explain real Memory versus History and local setup.
- `README.md` — document protocol-v3 inspection and initialization commands.
- `ROADMAP.md` — mark read-only Memory inspection complete while mutation, Persona, voice, and packaging remain open.
- `docs/r6-desktop-app-evaluation.md` — record exact deterministic, compiled, native, and unvalidated evidence.
- Temporary gateway configuration and initialized SQLite database — created under a test temporary directory and removed after acceptance.
- Existing operator configuration and databases — never read, changed, or inspected by this plan; live opt-in is a separate explicit action.

---

### Task 1: Protocol v2 Memory Wire Contract

**Files:**
- Create: `crates/protocol/src/client_memory.rs`
- Create: `tests/fixtures/client-wire-v3/commands.jsonl`
- Create: `tests/fixtures/client-wire-v3/events.jsonl`
- Create: `tests/fixtures/client-wire-v3/invalid.jsonl`
- Modify: `crates/protocol/src/client_wire.rs`
- Modify: `crates/protocol/src/lib.rs`
- Modify: `crates/protocol/tests/client_wire.rs`

**Interfaces:**
- Produces: `CLIENT_PROTOCOL_VERSION = 3`, `MAX_MEMORY_LIST_PAGE_ITEMS = 50`, `MAX_MEMORY_PREVIEW_BYTES = 192`, and `MAX_MEMORY_INSPECTION_HISTORY_ITEMS = 32`.
- Produces commands: `ClientCommand::MemoryList { request_id, before_id }` and `ClientCommand::MemoryInspect { request_id, memory_id }`.
- Produces responses: `GatewayMessage::MemoryList { request_id, records, next_cursor }` and `GatewayMessage::MemoryInspection { request_id, inspection }`.
- Produces DTOs: `ClientMemorySummary`, `ClientMemoryPage`, `ClientMemoryRecord`, `ClientMemoryInspection`, `ClientMemoryProvenance`, `ClientMemoryApproval`, `ClientMemoryRetention`.
- Produces required `ClientRuntimeError.code` values: `adapter_failure`, `configuration_invalid`, `invalid_state`, `memory_disabled`, `memory_turn_active`, `memory_not_found`, `memory_unavailable`.

- [ ] **Step 1: Write failing v2 command and compatibility tests**

Add tests that decode these exact shapes and reject the retained v1 fixture lines:

```rust
let list = decode_client_command(
    br#"{"protocol_version":3,"type":"memory_list","request_id":"req-1","cursor":null}"#,
).unwrap();
assert!(matches!(list, ClientCommand::MemoryList { before_id: None, .. }));

let inspect = decode_client_command(
    br#"{"protocol_version":3,"type":"memory_inspect","request_id":"req-2","memory_id":"7"}"#,
).unwrap();
assert!(matches!(inspect, ClientCommand::MemoryInspect { memory_id, .. } if memory_id.get() == 7));

for line in include_str!("../../../tests/fixtures/client-wire-v1/commands.jsonl").lines() {
    assert!(decode_client_command(line.as_bytes()).is_err());
}
```

- [ ] **Step 2: Run the focused protocol test and verify RED**

Run: `cargo test --locked -p conversation-protocol --test client_wire`

Expected: failure because protocol v2 variants and DTOs do not exist.

- [ ] **Step 3: Write failing DTO projection tests**

Construct a bounded identity `MemoryInspection` projection with approval and two provenance rows. Assert:

```rust
assert_eq!(wire.record.id, "7");
assert_eq!(wire.record.revision, "3");
assert_eq!(wire.record.created_at_ms, "9007199254740993");
assert_eq!(wire.sources[0].kind, "user_provided");
assert_eq!(wire.sources[1].kind, "user_edited");
assert_eq!(wire.approvals.last().unwrap().approved_revision, "2");
assert!(!wire.sources_truncated);
assert!(!wire.approvals_truncated);
```

Add preview cases for ASCII, CJK, collapsed whitespace, an exact 192-byte value, and a value truncated at a UTF-8 scalar boundary with the ellipsis included in the limit.
Add a maximum-shape encoding test with 32 provenance rows, 32 approval rows, maximum-length identifiers and actors, and maximum content; assert the encoded gateway response remains below the 512 KiB frame limit.

- [ ] **Step 4: Implement the protocol-v3 DTOs and strict wire**

Use private serde wire envelopes and explicit conversion functions. Do not derive serde directly on domain records. Represent all decimal fields as strings and validate cursor `before_id` as canonical non-zero u64.

Implement preview projection with this contract:

```rust
pub fn memory_preview(content: &str) -> String {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_utf8_with_ellipsis(&normalized, MAX_MEMORY_PREVIEW_BYTES)
}
```

The truncation helper must avoid allocating a string larger than the validated 4 KiB memory-content bound.

- [ ] **Step 5: Add canonical v2 fixtures and strict invalid cases**

Valid command fixtures include status, start, interrupt, first-page list, next-page list, and inspect. Valid gateway fixtures include ready, accepted, rejected with code, status, memory list, bounded memory inspection with truncation flags, runtime event, and fatal. Invalid fixtures cover v1, unknown fields, numeric IDs/timestamps, zero IDs/revisions, malformed cursor, invalid enums, missing error code, oversized preview, oversized history arrays, and missing truncation flags.

- [ ] **Step 6: Run protocol tests and formatting**

Run: `cargo test --locked -p conversation-protocol && cargo fmt --all -- --check`

Expected: all protocol unit, integration, and fixture tests pass.

- [ ] **Step 7: Check the protocol boundary without committing**

Run: `git diff --check -- crates/protocol tests/fixtures/client-wire-v3`

Expected: the protocol slice is reviewable and clean; hold it for the feature-level validation and independent review gate.

### Task 2: Bounded Memory Inspection Paging

**Files:**
- Create: `crates/memory/tests/inspection.rs`
- Modify: `crates/memory/src/store.rs`
- Modify: `crates/memory/src/sqlite.rs`
- Modify: `crates/memory/src/lib.rs`

**Interfaces:**
- Produces: `MemoryPage { records: Vec<MemoryRecord>, next_before_id: Option<MemoryId> }`.
- Produces: `BoundedMemoryInspection { inspection: MemoryInspection, sources_truncated: bool, approvals_truncated: bool }`.
- Adds: `MemoryStore::list_page(now, before_id, limit) -> MemoryStoreResult<MemoryPage>`.
- Adds: `MemoryStore::inspect_bounded(memory_id, now, history_limit) -> MemoryStoreResult<BoundedMemoryInspection>`.
- Guarantees: `1 <= limit <= MAX_MEMORY_LIST_PAGE_ITEMS`, `1 <= history_limit <= MAX_MEMORY_INSPECTION_HISTORY_ITEMS`, `id DESC` list traversal, latest bounded history windows returned oldest-to-newest, one-record lookahead, no retrieval trace writes, and no `last_used_at` changes.

- [ ] **Step 1: Write failing page-boundary tests**

Create 52 semantic records and assert:

```rust
let first = store.list_page(now, None, 50).unwrap();
assert_eq!(first.records().len(), 50);
assert_eq!(first.records()[0].id().get(), 52);
assert_eq!(first.records()[49].id().get(), 3);
assert_eq!(first.next_before_id().unwrap().get(), 3);

let second = store.list_page(now, first.next_before_id(), 50).unwrap();
assert_eq!(second.records().iter().map(|record| record.id().get()).collect::<Vec<_>>(), vec![2, 1]);
assert!(second.next_before_id().is_none());
```

Add exact 50, empty database, zero limit, and limit 51 cases.

- [ ] **Step 2: Run the focused memory test and verify RED**

Run: `cargo test --locked -p conversation-memory --test inspection`

Expected: compile failure because `MemoryPage` and `list_page` do not exist.

- [ ] **Step 3: Write failing lifecycle, traversal, and history-bound tests**

Cover these sequences:

- a due working record becomes expired on the first page;
- expiry changes state but not ID order;
- editing an unseen record does not move it across an ID cursor;
- creating record 53 after page one does not duplicate or skip IDs 1–2, and record 53 appears after refresh;
- deleting ID 2 between pages returns ID 1 without duplicate IDs;
- list paging leaves retrieval trace count, `last_used_at`, and retrieval reason unchanged;
- 33 provenance rows return the latest 32 in oldest-to-newest order with `sources_truncated = true` and current provenance last;
- 33 approval rows return the latest 32 in oldest-to-newest order with `approvals_truncated = true` and current approval last;
- exact 32, empty approval history, zero history limit, and history limit 33 cases;
- bounded inspection leaves retrieval trace count, `last_used_at`, and retrieval reason unchanged.

- [ ] **Step 4: Implement `MemoryPage` and SQLite keyset query**

After `expire_due`, query:

```sql
SELECT ... FROM memories m
WHERE (?1 IS NULL OR m.id < ?1)
ORDER BY m.id DESC
LIMIT ?2
```

Bind `limit + 1`, remove the lookahead row, and set `next_before_id` to the last returned ID only when lookahead existed. Reuse `row_to_record`; do not call retrieval or write a trace.

For bounded inspection, load the record and query each history independently:

```sql
SELECT ... FROM memory_sources
WHERE memory_id = ?1 AND kind != 'user_approved'
ORDER BY id DESC
LIMIT ?2
```

Use the analogous approval query, bind `history_limit + 1`, remove each lookahead row, reverse each retained window, then construct the validated `MemoryInspection`. Keep the existing unbounded `inspect_with_sources` contract for current internal/probe callers; the gateway must use only `inspect_bounded`.

- [ ] **Step 5: Run memory tests and strict Clippy**

Run: `cargo test --locked -p conversation-memory && cargo clippy --locked -p conversation-memory --all-targets -- -D warnings`

- [ ] **Step 6: Check bounded inspection without committing**

Run: `git diff --check -- crates/memory`

Expected: the store slice is reviewable and clean; hold it for the feature-level validation and independent review gate.

### Task 3: Gateway Memory Inspection Commands

**Files:**
- Modify: `apps/runtime-gateway/src/config.rs`
- Modify: `apps/runtime-gateway/src/main.rs`
- Modify: `apps/runtime-gateway/src/session.rs`
- Modify: `apps/runtime-gateway/tests/config.rs`
- Modify: `apps/runtime-gateway/tests/gateway_cli.rs`

**Interfaces:**
- `GatewayAdapters` exposes `memory_provider: Option<SqliteMemoryContextProvider>` and `memory_store: Option<SqliteMemoryStore>` created by cloning one validated store instance.
- `GatewaySession::with_memory_inspection(store: Arc<dyn MemoryStore>, clock: Arc<dyn MemoryClock>) -> Self` attaches inspection.
- Status capabilities are `["text"]` without memory and `["text", "memory_inspection"]` with memory.
- Memory commands are rejected before blocking work when `active.is_some()`.

- [ ] **Step 1: Write failing configuration ownership tests**

Load a temporary initialized database and assert both retrieval and inspection handles exist. Prove shared backing behavior without exposing a path accessor: create a matching active semantic record through the returned store, retrieve through the returned provider, and assert that record is selected. Load a configuration without `[memory]` and assert both handles are absent.

- [ ] **Step 2: Write failing session command tests**

Use framed in-memory IO and an initialized temporary store. Cover:

```text
memory_list -> command_accepted -> memory_list response
memory_inspect(existing) -> command_accepted -> memory_inspection response
memory_inspect(missing) -> command_rejected(code=memory_not_found)
memory command without store -> command_rejected(code=memory_disabled)
memory command during turn -> command_rejected(code=memory_turn_active)
interrupt after active-turn rejection -> command_accepted -> one cancelled terminal
memory_inspect(oversized stored history) -> one bounded response below MAX_FRAME_SIZE
```

Assert the session still answers `status` after every request-scoped rejection.

- [ ] **Step 3: Run gateway tests and verify RED**

Run: `cargo test --locked -p conversation-runtime-gateway`

- [ ] **Step 4: Retain the store and advertise capability**

Clone `SqliteMemoryStore` before constructing `SqliteMemoryContextProvider`. Main attaches the provider to `TextTurnRuntime`, attaches the store and `SystemMemoryClock` to `GatewaySession`, and includes `memory_inspection` only when both are present.

- [ ] **Step 5: Implement request-scoped inspection**

For memory commands:

1. Reject `memory_turn_active` if `active.is_some()`.
2. Reject `memory_disabled` if no inspection store exists.
3. Clone store/clock and run only `list_page(..., 50)` or `inspect_bounded(..., 32)` in `tokio::task::spawn_blocking`.
4. On success, send `CommandAccepted`, then the correlated response.
5. Map `NotFound` to `memory_not_found`; map every other store/join failure to `memory_unavailable`.
6. Keep diagnostics static and content-free.

Do not send `CommandAccepted` before a fallible store result exists because the current client treats rejection after acceptance as a protocol violation.

- [ ] **Step 6: Add compiled gateway CLI coverage**

Start the actual compiled gateway with a temporary config and initialized memory database. Send framed v2 list and inspect commands, assert exact correlation and content, then send status and close stdin. Add a separate active-turn case proving memory rejection does not delay interruption or process cleanup.

- [ ] **Step 7: Run gateway tests and check the slice**

Run: `cargo test --locked -p conversation-runtime-gateway && cargo clippy --locked -p conversation-runtime-gateway --all-targets -- -D warnings`

Run: `git diff --check -- apps/runtime-gateway`

Expected: the gateway slice is reviewable and clean; hold it for the feature-level validation and independent review gate.

### Task 4: Browser-Safe TypeScript Memory SDK

**Files:**
- Modify: `packages/typescript/src/protocol.ts`
- Modify: `packages/typescript/src/client.ts`
- Modify: `packages/typescript/src/browser.ts`
- Modify: `packages/typescript/src/index.ts`
- Modify: `packages/typescript/test/protocol.test.ts`
- Modify: `packages/typescript/test/client.test.ts`
- Modify: `packages/typescript/test/browser.test.ts`
- Modify: `packages/typescript/test/stdio.test.ts`
- Modify: `examples/node-chat/test/cli.test.ts`

**Interfaces:**
- Produces: `MemoryCursor`, `MemorySummary`, `MemoryPage`, `MemoryRecord`, `MemoryInspection`, `MemoryProvenance`, `MemoryApproval`, and `MemoryRetention`.
- Produces: `RuntimeClient.listMemories(cursor?)` and `RuntimeClient.inspectMemory(memoryId)`.
- Updates: `RuntimeFailure.code` and `RuntimeStatus.capabilities` for protocol v2.

- [ ] **Step 1: Write failing strict parser/encoder tests**

Assert v2 commands encode with snake-case wire keys and decimal strings, then parse list/inspection fixtures to camel-case values and `bigint`:

```ts
assert.deepEqual(parseGatewayMessage(memoryListFixture), {
  type: "memory_list",
  requestId: "request-1",
  records: [{ id: 7n, contentPreview: "Local preference", kind: "semantic", state: "active", pinned: false, updatedAtMs: 9007199254740993n }],
  nextCursor: { beforeId: 7n },
});
```

For inspection fixtures, assert `sourcesTruncated` and `approvalsTruncated` are required booleans and each retained array preserves oldest-to-newest ordering.

Reject v1, unknown capability values, missing error codes, numeric decimal fields, zero IDs/revisions, negative timestamps, malformed cursor shape, provenance or approval arrays above 32 entries, missing truncation flags, invalid history ordering/current-state correspondence, and preview payloads above 192 UTF-8 bytes.

- [ ] **Step 2: Run SDK protocol tests and verify RED**

Run: `npm run build --workspace @conversation/runtime && node --test packages/typescript/dist/test/protocol.test.js`

- [ ] **Step 3: Write failing RuntimeClient lifecycle tests**

Cover acceptance-before-response, list pagination, inspect, request rejection preserving client health, early response fatality, mismatched response fatality, duplicate response fatality, transport failure, and close rejecting pending memory requests exactly once.

- [ ] **Step 4: Implement v2 types, parsers, encoders, and client controls**

Add pending-control variants:

```ts
| { kind: "memory_list"; accepted: boolean; result: Deferred<MemoryPage>; fail(error: Error): void }
| { kind: "memory_inspect"; accepted: boolean; result: Deferred<MemoryInspection>; fail(error: Error): void }
```

`listMemories` defaults to `cursor: null`; `inspectMemory` rejects IDs outside `1n..MAX_U64` before registering work. Route each response only to a matching accepted control and remove it exactly once.

- [ ] **Step 5: Update browser exports and all raw v1 fixtures**

Export all memory types from `./browser` and root. Keep `StdioGatewayTransport` absent from the browser entry. Update TypeScript stdio tests and Node chat test statuses/fixtures to protocol v2 without adding memory behavior to either transport or CLI. Retain only the explicit protocol-compatibility cases that intentionally prove v1 rejection.

- [ ] **Step 6: Run SDK and Node tests/builds**

Run: `npm test --workspace @conversation/runtime && npm test --workspace conversation-node-chat && npm run build --workspace @conversation/runtime && npm run build --workspace conversation-node-chat`

- [ ] **Step 7: Check the SDK slice without committing**

Run: `git diff --check -- packages/typescript examples/node-chat`

Expected: the SDK slice is reviewable and clean; hold it for the feature-level validation and independent review gate.

### Task 5: Desktop Memory Screen

**Files:**
- Create: `apps/desktop/src/components/MemoryPane.tsx`
- Create: `apps/desktop/test/memory-pane.test.tsx`
- Modify: `apps/desktop/src/App.tsx`
- Modify: `apps/desktop/src/runtime/conversation-session.ts`
- Modify: `apps/desktop/src/components/Workspace.tsx`
- Modify: `apps/desktop/src/styles.css`
- Modify: `apps/desktop/test/conversation-session.test.ts`
- Modify: `apps/desktop/test/tauri-transport.test.ts`
- Modify: `apps/desktop/test/app.test.tsx`

**Interfaces:**
- `DesktopSession` adds `listMemories(cursor?)` and `inspectMemory(memoryId)`.
- `ConversationSession` forwards both methods only while phase is `ready`.
- `MemoryPane` receives `session`, `onBack`, and a verified local status; it owns loading, page accumulation, selection, retry, and detail formatting.

- [ ] **Step 1: Write failing ConversationSession tests**

Assert ready-state forwarding and active-turn refusal:

```ts
await expect(session.listMemories()).resolves.toEqual(page);
await expect(session.inspectMemory(7n)).resolves.toEqual(inspection);
session.send("active turn");
await expect(session.listMemories()).rejects.toThrow("finish or stop the active response");
```

- [ ] **Step 2: Write failing MemoryPane component tests**

Cover:

- initial loading and empty state;
- one page and no `Load more`;
- two pages without duplicate rows;
- detail content, metadata, retention, provenance, and approval ordering;
- explicit older-entry notices when provenance or approval history is truncated;
- timestamp outside JavaScript Date range shown as `Timestamp out of range`;
- typed `memory_not_found` returns to list with a refresh message;
- typed `memory_unavailable` shows a retry action;
- no create, edit, approve, pin, retrieve, or delete button.

- [ ] **Step 3: Run focused desktop tests and verify RED**

Run: `npm test --workspace conversation-desktop -- --run test/conversation-session.test.ts test/memory-pane.test.tsx test/app.test.tsx`

- [ ] **Step 4: Implement session forwarding and focused MemoryPane**

Use normal React text nodes for content. Format confidence as a percentage derived from `0..1000`, but preserve the exact numeric value in an accessible label. Render retention by kind and only its applicable timestamp/session identifier. Render source and approval arrays in received oldest-to-newest order. When a truncation flag is true, render `Older provenance entries are not shown` or `Older approval entries are not shown` beside the matching history.

Do not import Tauri, SQLite, Node APIs, or the transcript history store into `MemoryPane`.

- [ ] **Step 5: Add truthful navigation and active-turn behavior**

Show Memory only when all are true:

```ts
status.memoryEnabled &&
status.memoryLocation === "local" &&
status.capabilities.includes("memory_inspection")
```

Disable it while `sessionState.phase === "streaming"` and show the accessible explanation `Finish or stop the active response before opening Memory.` Keep `History` available because it is application-owned and does not touch runtime memory.

- [ ] **Step 6: Implement responsive light/dark styling**

Use existing design tokens and patterns. Preserve keyboard focus, `aria-current`, readable long content, wrapping identifiers, reduced motion, and both color schemes. Do not add another ambient scene or settings drawer.

- [ ] **Step 7: Run desktop tests/build and check the slice**

Run: `npm test --workspace conversation-desktop && npm run build --workspace conversation-desktop`

Run: `git diff --check -- apps/desktop`

Expected: the desktop slice is reviewable and clean; hold it for the feature-level validation and independent review gate.

### Task 6: Documentation, Temporary Acceptance, and Final Verification

**Files:**
- Modify: `apps/desktop/README.md`
- Modify: `README.md`
- Modify: `ROADMAP.md`
- Modify: `docs/r6-desktop-app-evaluation.md`
- Temporary only: gateway configuration and initialized database under a new `mktemp` directory.

**Interfaces:**
- Public docs explain protocol-v3 read-only inspection, History/Memory separation, explicit memory initialization, and remaining mutation/Persona/voice work.
- Acceptance uses disposable local state; the operator's existing configuration and database remain untouched unless separately authorized after delivery.

- [ ] **Step 1: Update public documentation and roadmap truthfully**

Document:

- Memory is optional and explicit opt-in.
- The desktop reads it only through the public local runtime protocol.
- Memory and History are separate stores with different ownership.
- The first screen is read-only and does not automatically capture conversations.
- Inspection shows at most the latest 32 provenance and approval entries and clearly labels truncated older history.
- Due expiry may be applied during inspection.
- Persona mutation, memory mutation, live voice, packaging, and R3 acoustic evidence remain open.

- [ ] **Step 2: Create disposable acceptance state**

Run:

```bash
ACCEPTANCE_DIR="$(mktemp -d)"
MEMORY_DB="$ACCEPTANCE_DIR/runtime.sqlite3"
cargo run --locked -p conversation-memory-probe -- --database "$MEMORY_DB" init
cargo run --locked -p conversation-memory-probe -- --database "$MEMORY_DB" add \
  --kind semantic --content "Disposable acceptance memory" \
  --source-id acceptance-fixture --actor local-test
cp configs/gateway.example.toml "$ACCEPTANCE_DIR/gateway.toml"
cat >> "$ACCEPTANCE_DIR/gateway.toml" <<EOF

[memory]
database = "$MEMORY_DB"
maximum_items = 4
maximum_bytes = 4096
EOF
chmod 600 "$ACCEPTANCE_DIR/gateway.toml" "$MEMORY_DB"
```

Do not substitute `/Users/cx/.config/conversation-runtime/gateway.toml` or any existing database. Record the temporary directory for cleanup after acceptance.

- [ ] **Step 3: Run the complete mechanical gate**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
npm test --workspaces
npm run build --workspaces
git diff --check
```

Run socket-based Rust and Node integration tests with loopback permission when the sandbox blocks local test listeners.
Search all active Rust, TypeScript, Node, and desktop tests for raw `protocol_version: 1`; only explicit v1-rejection cases and retained historical fixtures may remain.

- [ ] **Step 4: Run compiled and native acceptance with disposable state**

Start the compiled gateway with `$ACCEPTANCE_DIR/gateway.toml` and exercise `status`, memory list, and inspect through the compiled TypeScript client. Then run `npm run desktop:dev`, connect with the same disposable config, verify Memory appears, verify list/detail/truncation presentation, and close the app cleanly. Active-turn disablement and stop behavior remain deterministic automated acceptance requirements using the existing fake local adapter rather than the operator's live model.

Record exact observed behavior only. Do not claim subjective quality, R3 acoustic closure, signing, installation, or memory mutation.

- [ ] **Step 5: Request independent review and fix concrete findings**

Review protocol compatibility, frame, preview and history bounds, request correlation, active-turn interruption, SQLite contention, privacy-safe errors, bigint precision, navigation truthfulness, and private/public boundary. Re-run focused tests after every correction, then rerun the complete mechanical gate and compiled acceptance.

- [ ] **Step 6: Clean disposable state and verify no private changes**

```bash
rm -rf "$ACCEPTANCE_DIR"
git status --short
```

Expected: only intended repository files are modified; no operator config, application-support database, temporary database, generated Tauri schema, model file, or credential is present.

- [ ] **Step 7: Commit the reviewed feature at its scope boundary**

```bash
git add crates/protocol crates/memory apps/runtime-gateway packages/typescript examples/node-chat apps/desktop README.md ROADMAP.md docs/r6-desktop-app-evaluation.md tests/fixtures/client-wire-v3
git diff --cached --check
git commit -m "feat(desktop): add local memory inspection"
```

- [ ] **Step 8: Verify final branch state**

Run:

```bash
git status --short --branch
git log --oneline --decorate -8
git diff master...HEAD --check
```

Expected: clean `feature/desktop-memory-inspection`, stacked after `4c53c25`, `2d3eb1b`, and the implementation-plan commit, with one reviewed feature commit and no private configuration, SQLite database, generated Tauri schemas, model files, or credentials staged.
