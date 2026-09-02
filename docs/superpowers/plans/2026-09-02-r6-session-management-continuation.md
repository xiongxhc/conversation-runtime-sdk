# R6 Session Management and Continuation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let people delete saved Sessions from the list and continue a saved Session as a new, bounded conversation branch with truthful runtime context.

**Architecture:** Desktop SQLite remains the owner of saved transcripts and prepares revision-checked branch copies. Protocol v2 adds a bounded, structured context-seed command; the gateway atomically replaces only runtime history in the shared `ConversationContext`, while the desktop presents copied context separately from new live turns and records recoverable saga state.

**Tech Stack:** Rust, Tokio, rusqlite, Serde, React 19, TypeScript, Tauri 2, Vitest, Testing Library.

**Spec:** `docs/superpowers/specs/2026-09-02-r6-session-management-continuation-design.md`

## Global Constraints

- Continue means **Continue as new conversation**, never Resume or Restore.
- Seed at most 16 completed exchanges and 32,768 UTF-8 content bytes.
- Preserve the existing 16 KiB limit for each individual user or assistant message.
- Select whole completed pairs newest-to-oldest, then send oldest-to-newest; never skip across an oversized gap, truncate, summarize, or compress.
- Keep the source Session immutable and create a new persisted branch ID.
- Use the current model, persona, and active/unexpired memory retrieval policy; never restore historical provider state, memory snapshots, device state, or runtime IDs.
- Reject continuation during all pending/active text and non-idle/live voice states; never cancel work implicitly.
- Keep desktop SQLite and Tauri details out of the public protocol and gateway.
- Preserve strict v1 behavior for an updated client connected to a v1 server; the updated gateway emits v2.
- Use decimal strings for desktop history revisions across the Tauri JSON boundary; convert with checked `u64` parsing in Rust and never rely on unsafe JavaScript numbers.
- No new runtime, UI, or network dependency.
- Keep all changes uncommitted and do not push.
- Automated checks do not claim product-owner visual, acoustic, or real-device acceptance.

## File Structure and Dependency Direction

```text
apps/desktop/src/components/Workspace.tsx
  -> apps/desktop/src/runtime/conversation-session.ts
       -> packages/typescript/src/client.ts
            -> packages/typescript/src/protocol.ts
                 -> protocol v2 wire
  -> apps/desktop/src/history/conversation-history.ts
       -> Tauri commands in apps/desktop/src-tauri/src/lib.rs
            -> apps/desktop/src-tauri/src/history_store.rs

apps/runtime-gateway/src/session.rs
  -> crates/runtime/src/conversation_context.rs
       -> crates/runtime/src/conversation_quality.rs
            -> crates/protocol/src/quality.rs

crates/model-adapters/src/language_model.rs
  -> protocol history/message bounds
```

Tasks 1 and 2 are independent foundations and may execute in parallel. Task 3
consumes Task 1. Tasks 4 and 5 consume Tasks 1 and 3 and may then execute in
parallel. Task 6 consumes Tasks 3–4. Task 7 consumes Tasks 2 and 6. Task 8 is
the integration and evidence gate.

---

### Task 1: Expand the Shared History Envelope Without Widening Messages

**Files:**
- Modify: `crates/protocol/src/quality.rs`
- Modify: `crates/protocol/src/lib.rs`
- Modify: `crates/protocol/tests/quality_contracts.rs`
- Modify: `crates/runtime/src/conversation_quality.rs`
- Modify: `crates/runtime/tests/conversation_quality.rs`
- Modify: `crates/model-adapters/src/language_model.rs`
- Modify: `crates/model-adapters/tests/ollama.rs`
- Modify: `crates/model-adapters/tests/voice_contracts.rs`

**Interfaces:**
- Produces: `MAX_HISTORY_MESSAGE_COUNT = 32`, `MAX_HISTORY_BYTES = 32 * 1024`, `MAX_CONVERSATION_MESSAGE_BYTES = 16 * 1024`, and `MAX_LANGUAGE_MODEL_INPUT_BYTES = 64 * 1024`.
- Consumes: existing `ConversationMessage`, `CompletedExchange`, and `LanguageModelInput` validation paths.

- [x] **Step 1: Write failing protocol and quality-bound tests**

  Add assertions equivalent to:

  ```rust
  assert_eq!(MAX_HISTORY_MESSAGE_COUNT, 32);
  assert_eq!(MAX_HISTORY_BYTES, 32 * 1024);
  assert!(ConversationMessage::new(ConversationRole::User, "x".repeat(16 * 1024)).is_ok());
  assert!(ConversationMessage::new(ConversationRole::User, "x".repeat(16 * 1024 + 1)).is_err());
  ```

  Extend `conversation_quality` tests to complete 17 small exchanges and prove
  only the newest 16 whole pairs remain, plus a multibyte UTF-8 case capped at
  32,768 bytes.

- [x] **Step 2: Run focused tests and verify RED**

  ```bash
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked -p conversation-protocol --test quality_contracts
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked -p conversation-runtime --test conversation_quality
  ```

  Expected: assertions fail on the current 16-message/eight-exchange/16-KiB
  history constants.

- [x] **Step 3: Introduce the distinct history constants**

  In `quality.rs`, keep the individual-message constant and add/export:

  ```rust
  pub const MAX_HISTORY_MESSAGE_COUNT: usize = 32;
  pub const MAX_HISTORY_BYTES: usize = 32 * 1024;
  pub const MAX_CONVERSATION_MESSAGE_BYTES: usize = 16 * 1024;
  ```

  In `conversation_quality.rs`, set `MAX_HISTORY_EXCHANGES` to 16 and consume
  the shared `MAX_HISTORY_BYTES` rather than a second private value. Preserve
  oldest-whole-pair eviction and completed-only storage.

- [x] **Step 4: Write the failing model-envelope tests**

  Add tests constructing 32 valid history messages totaling 32 KiB, a 16 KiB
  current transcript, 4 KiB guidance, and 8 KiB memory. Assert the exact 60 KiB
  maximum-valid content total is accepted and the aggregate constant is exactly
  64 KiB. The component caps make a valid over-64-KiB input unconstructible; do
  not bypass public constructors to manufacture a dead-branch test. Keep
  explicit reachable overflow tests for every component cap.

- [x] **Step 5: Implement the model-envelope change**

  Import `MAX_HISTORY_BYTES`, validate history total against it, change
  `MAX_LANGUAGE_MODEL_INPUT_BYTES` to `64 * 1024`, and update exact diagnostic
  strings from eight/16 KiB to sixteen/32 KiB. Do not change current-message,
  runtime-guidance, or memory limits.

- [x] **Step 6: Verify GREEN**

  ```bash
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked -p conversation-protocol --test quality_contracts
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked -p conversation-runtime --test conversation_quality
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked -p conversation-model-adapters
  ```

  Expected: all focused suites exit 0.

### Task 2A: Add Schema-2 Migration and Revisioned History CRUD

**Files:**
- Modify: `apps/desktop/src-tauri/src/history_store.rs`
- Modify: `apps/desktop/src-tauri/tests/history_store.rs`

**Interfaces:**
- Produces:
  - checked native `HistoryRevision(u64)` serialized as a decimal string at the later Tauri boundary;
  - `ConversationOrigin = "continued_context" | "live"`.
  - `ContinuationState = "preparing" | "confirmed" | "unconfirmed"`.
  - native `save(write, expected_revision)` and `delete(id, expected_revision)` compare-and-write operations.
- Consumes: current SQLite parent/turn schema.

- [x] **Step 1: Write migration and compare-and-write tests first**

  Create a legacy schema-0 fixture with the current tables, reopen it through
  `ConversationHistoryStore::open`, and assert `PRAGMA user_version = 2`,
  revision 1, `live` origins, and unchanged content. Add fresh-schema coverage.

  Add native tests for:

  ```rust
  let saved = store.save(new_history, None)?;              // revision 1
  let updated = store.save(changed, Some(saved.revision))?; // revision 2
  assert_conflict(store.save(stale, Some(saved.revision)));
  store.delete(id, updated.revision)?;
  assert_not_found(store.get(id)?);
  assert_conflict(store.save(stale_after_delete, Some(updated.revision)));
  ```

  Confirm parent deletion cascades turns after closing and reopening the store.

- [x] **Step 2: Run native history tests and verify RED**

  ```bash
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked -p conversation-desktop --test history_store -- --test-threads=1
  ```

  Expected: new fields/methods are missing and legacy migration assertions fail.

- [x] **Step 3: Implement schema-2 migration and revisioned CRUD**

  Replace unconditional initialization/upsert with a transaction that:

  ```text
  read PRAGMA user_version
  if new database: create schema 2 directly
  if legacy schema 0: add revision/provenance/recovery/origin columns,
                      backfill safe defaults, rebuild affected indexes
  set PRAGMA user_version = 2
  commit, or roll back the whole migration
  ```

  Internally use checked `u64` revisions stored as SQLite INTEGER. Serialize
  them to decimal strings in Tauri DTOs. Existing saves execute `UPDATE ...
  WHERE id = ? AND revision = ?`; new saves use `INSERT` only. Replace all turn
  rows only after the parent comparison succeeds in the same immediate
  transaction. The Workspace write queue reads the latest canonical revision at
  execution time, so two queued snapshots do not both attempt a new insert.
  Delete must distinguish not-found from revision conflict.

- [x] **Step 4: Verify native GREEN**

  ```bash
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked -p conversation-desktop --test history_store -- --test-threads=1
  ```

  Expected: migration, revision, stale-save, not-found, and cascade tests pass.

### Task 2B: Add Native Continuation Preparation and TypeScript Adapters

**Files:**
- Modify: `apps/desktop/src-tauri/src/history_store.rs`
- Modify: `apps/desktop/src-tauri/src/lib.rs`
- Modify: `apps/desktop/src-tauri/tests/history_store.rs`
- Modify: `apps/desktop/src/history/conversation-history.ts`
- Modify: `apps/desktop/test/conversation-history.test.ts`

**Interfaces:**
- Consumes: Task 2A schema/revision CRUD.
- Produces:
  - `HistoryRevision = string` at the TypeScript/Tauri boundary.
  - `ConversationHistoryWrite = Omit<ConversationHistory, "revision">` and `HistorySaveResult = { revision: HistoryRevision }`.
  - `PreparedContinuation = { branch: ConversationHistory; seed: ConversationContextExchange[]; operationId: string }`.
  - store methods `save(write, expectedRevision?)`, `delete(id, expectedRevision)`, `prepareContinuation(sourceId, expectedRevision)`, and `setContinuationState(branchId, expectedRevision, state)`.

- [x] **Step 1: Write native continuation-selection tests**

  Cover completed/nonblank-only selection, original whitespace preservation,
  UTF-8 byte counting, newest-first retention returned oldest-first, exact
  16/32-KiB limits, per-message 16-KiB rejection, oversized older-gap stopping,
  UTF-8-safe branch-title truncation, source-revision conflict, delete-first,
  prepare-first then source-delete, and copied-context survival.

- [x] **Step 2: Run native tests and verify RED**

  ```bash
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked -p conversation-desktop --test history_store -- --test-threads=1
  ```

  Expected: continuation preparation/state methods and selection behavior are
  absent while Task 2A migration/revision tests remain green.

- [x] **Step 3: Implement transactional branch preparation**

  Add a native method with the concrete boundary:

  ```rust
  pub fn prepare_continuation(
      &self,
      source_id: &str,
      expected_revision: u64,
      now_ms: i64,
      branch_id: &str,
      operation_id: &str,
  ) -> Result<PreparedContinuation, HistoryStoreError>;
  ```

  It re-reads and revision-checks the source inside one immediate transaction,
  applies the canonical selector, inserts a `preparing` branch with copied
  `continued_context` turns, and returns the exact seed. Add idempotent
  `set_continuation_state` and revision-checked cleanup/delete.

- [x] **Step 4: Add Tauri and TypeScript adapters test-first**

  Update adapter tests to assert exact native command/argument mappings and
  decimal revision strings. Then register/update `save_conversation_history`,
  `delete_conversation_history`, `prepare_conversation_continuation`, and
  `set_conversation_continuation_state` in `lib.rs` and expose the typed store
  interface in `conversation-history.ts`.

- [x] **Step 5: Verify GREEN**

  ```bash
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked -p conversation-desktop --test history_store -- --test-threads=1
  env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm test --workspace conversation-desktop -- --run test/conversation-history.test.ts
  ```

### Task 3: Define Strict Protocol v2 and Preserve the V1 Codec

**Files:**
- Modify: `crates/protocol/src/client_wire.rs`
- Modify: `crates/protocol/src/lib.rs`
- Modify: `crates/protocol/tests/client_wire.rs`
- Modify: `packages/typescript/src/protocol.ts`
- Modify: `packages/typescript/test/protocol.test.ts`

**Interfaces:**
- Produces:
  - `LEGACY_CLIENT_PROTOCOL_VERSION = 1` and `CLIENT_PROTOCOL_VERSION = 2`.
  - `ConversationContextExchange { user, assistant }`.
  - v2 command `SeedConversationContext { request_id, operation_id, exchanges }`.
  - capability `conversation_context_seed`.
  - v2 `RuntimeStatus.last_context_seed_operation_id` / TypeScript `lastContextSeedOperationId`.
  - version-discriminating ready parser plus strict version-specific encode/decode functions.
- Consumes: Task 1's 32-message, 32-KiB-history, and 16-KiB-message limits.

- [x] **Step 1: Write cross-language v2 contract tests**

  Add the exact v2 wire fixture:

  ```json
  {
    "protocol_version": 2,
    "type": "seed_conversation_context",
    "request_id": "request-1",
    "operation_id": "continue-1",
    "exchanges": [{"user":"hello","assistant":"hi"}]
  }
  ```

  Assert exact keys, ordered capability validation, empty/malformed pairs,
  duplicate/empty/oversized operation IDs, 17 pairs, 32-KiB-plus-one history,
  16-KiB-plus-one individual content, and unknown fields all reject. Assert v2
  status round-trips a nullable last-operation ID.

- [x] **Step 2: Add explicit v1 regression fixtures and verify RED**

  Exercise v1 ready/status/text/voice/persona/memory encode and parse paths with
  their original exact schemas. Assert v1 never accepts or advertises seed.
  Run:

  ```bash
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked -p conversation-protocol --test client_wire
  env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm test --workspace @conversation/runtime -- --test-name-pattern="protocol"
  ```

  Expected: v2 fixtures fail against the single-version implementation.

- [x] **Step 3: Implement versioned Rust wire types and validation**

  Add the exchange type with constructors/accessors that enforce nonblank,
  individual, count, and aggregate byte limits before building a client
  command. Keep `invalid-command` behavior for frames whose request ID cannot be
  recovered. For valid decoded seed requests, ensure exactly one correlated
  accepted or rejected terminal result is representable.

- [x] **Step 4: Implement versioned TypeScript codecs**

  Expose:

  ```ts
  type ClientProtocolVersion = 1 | 2;
  parseReadyMessage(raw: unknown): { version: ClientProtocolVersion; message: ReadyMessage };
  parseGatewayMessage(raw: unknown, version: ClientProtocolVersion): GatewayMessage;
  encodeClientCommand(command: ClientCommand, version: ClientProtocolVersion): unknown;
  ```

  Both codecs keep `requireExactKeys`. The v1 encoder must output version 1 for
  every existing command; the v2 encoder outputs version 2 and alone accepts
  `seed_conversation_context`.

- [x] **Step 5: Verify GREEN and Rust/TS fixture parity**

  Run both focused protocol suites. Compare at least one accepted seed, one
  rejected seed, one v1 status, and one v2 status fixture byte-for-byte at the
  JSON field level.

### Task 4: Add Atomic Runtime History Seeding and Gateway Handling

**Files:**
- Modify: `crates/runtime/src/conversation_quality.rs`
- Modify: `crates/runtime/src/conversation_context.rs`
- Modify: `crates/runtime/tests/conversation_quality.rs`
- Modify: `crates/runtime/tests/conversation_context.rs`
- Modify: `crates/runtime/tests/text_turn.rs`
- Modify: `crates/runtime/tests/streaming_turn.rs`
- Modify: `apps/runtime-gateway/src/session.rs`
- Modify: `apps/runtime-gateway/src/config.rs`

**Interfaces:**
- Consumes: Task 1 bounds and Task 3 `ConversationContextExchange`/v2 command.
- Produces: `ConversationQualityController::replace_completed_history`, `ConversationContext::seed_completed_history`, gateway capability/handler, idempotent operation identity, and v2 status recovery value.

- [x] **Step 1: Write failing runtime replacement tests**

  Add tests proving valid seed replacement, exact count/byte boundaries,
  all-input validation before mutation, no partial state on malformed input,
  rejection with a pending/active turn, unchanged persona and memory provider,
  monotonic next turn ID, and no memory extraction.

- [x] **Step 2: Implement controller and context APIs minimally**

  Use concrete APIs:

  ```rust
  pub fn replace_completed_history(
      &mut self,
      exchanges: &[ConversationContextExchange],
  ) -> Result<(), RuntimeError>;

  pub async fn seed_completed_history(
      &self,
      operation_id: &str,
      exchanges: &[ConversationContextExchange],
  ) -> Result<(), RuntimeError>;
  ```

  Build a temporary `VecDeque<CompletedExchange>` and byte total first, then
  swap only after validation. Context locks lifecycle before quality, rejects
  any active turn, preserves `sequence`, and stores the last operation ID plus
  seed for same-ID idempotency/different-content conflict.

- [x] **Step 3: Prove typed and spoken consumers see identical seed**

  Extend text and streaming/voice tests so the first post-seed provider input
  contains the exact ordered 32-message maximum where applicable. Assert the
  seed command itself emits no turn or memory-extraction event.

- [x] **Step 4: Write failing gateway lifecycle tests**

  Cover idle success; active text; voice starting, listening, paused, stopping,
  and terminal-pending; same-operation idempotency; different-content conflict;
  v2 status last-operation recovery; and exactly one correlated terminal result
  for every valid decoded command.

- [x] **Step 5: Implement the gateway command and capability**

  Add `conversation_context_seed` immediately after `text` in the canonical
  capability order. In `GatewaySession::handle_command`, reuse existing active
  text and voice terminal/ownership guards before calling
  `runtime.context().seed_completed_history`. Send one `CommandAccepted` on
  success or one `CommandRejected` on failure. Do not create an
  `ActiveForwarder` and do not call `MemoryExtractor`.

- [x] **Step 6: Verify GREEN**

  ```bash
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked -p conversation-runtime
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked -p conversation-runtime-gateway
  ```

### Task 5: Make the Public TypeScript Client Version-Aware and Seed-Capable

**Files:**
- Modify: `packages/typescript/src/client.ts`
- Modify: `packages/typescript/src/stdio.ts`
- Modify: `packages/typescript/src/index.ts`
- Modify: `packages/typescript/src/browser.ts`
- Modify: `packages/typescript/test/client.test.ts`
- Modify: `packages/typescript/test/browser.test.ts`
- Modify: `packages/typescript/test/stdio.test.ts`
- Modify: `packages/typescript/test/voice-session.test.ts`
- Modify: `apps/desktop/src/runtime/tauri-transport.ts`
- Modify: `apps/desktop/test/tauri-transport.test.ts`

**Interfaces:**
- Consumes: Task 3's `ClientProtocolVersion`, codecs, exchange type, seed command, and v2 status.
- Produces: `RuntimeClient.seedConversationContext(exchanges, operationId): Promise<void>` and preserved v1 existing operations.

- [x] **Step 1: Write client state-machine tests first**

  Add tests that connect to v1 and v2 fake transports. For v1, assert status,
  text, voice, persona, and memory commands remain version-1 encoded and seed
  rejects locally without sending. For v2, assert seed validates before send,
  waits for correlated acceptance, rejects correlated failure, rejects duplicate
  terminal messages, and exposes `lastContextSeedOperationId` through status.

- [x] **Step 2: Run the package suite and verify RED**

  ```bash
  env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm test --workspace @conversation/runtime
  ```

  Expected: connection-version storage and seed controls do not exist.

- [x] **Step 3: Store the ready version and route through its codec**

  Replace the unconditional `parseGatewayMessage(raw)` loop with ready-first
  discrimination. Store the selected version once; duplicate ready or any
  pre-ready non-ready frame remains fatal. Change the transport boundary to:

  ```ts
  interface RuntimeTransport {
    readonly messages: AsyncIterable<unknown>;
    send(command: ClientCommand, version: ClientProtocolVersion): Promise<void>;
    close(): Promise<void>;
  }
  ```

  `RuntimeClient` supplies its stored version on every send. Update
  `StdioGatewayTransport`, `TauriGatewayTransport`, and fake transports to call
  `encodeClientCommand(command, version)` and test the emitted protocol number.

- [x] **Step 4: Add the seed control state**

  Add a `seed_conversation_context` member to `PendingControl`, validate the
  operation ID/exchanges, reject when connection version is 1 or the ready
  status lacks capability, resolve only on correlated acceptance, and clean the
  control map on rejection/close. Export the exchange/status types from both
  Node and browser entrypoints.

- [x] **Step 5: Verify GREEN and built exports**

  ```bash
  env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm test --workspace @conversation/runtime
  env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm run build --workspace @conversation/runtime
  rg -n "seedConversationContext|ConversationContextExchange" packages/typescript/dist
  ```

### Task 6: Add Desktop Continuation State and Locking

**Files:**
- Modify: `apps/desktop/src/App.tsx`
- Modify: `apps/desktop/src/runtime/conversation-session.ts`
- Modify: `apps/desktop/test/conversation-session.test.ts`

**Interfaces:**
- Consumes: Task 5 `RuntimeClient.seedConversationContext` and exchange type.
- Produces:
  - `CarriedConversationContext = { sourceId; sourceTitle; operationId; exchanges; bytes }`.
  - `ConversationSessionState.continuation = { inProgress; carriedContext? }`.
  - `ConversationSession.continueWithSeed(context): Promise<void>`.
  - matching `DesktopSession` surface in `App.tsx`.

- [x] **Step 1: Write state and race tests first**

  Test success publishes carried context and clears old live presentation only
  after client success. Inject a deferred seed promise and assert `send`, voice
  start, persona update, close/disconnect, and a second continuation reject
  while pending. Cover active text, every live/non-idle voice state, correlated
  failure, and transport failure without local presentation mutation.

- [x] **Step 2: Run focused tests and verify RED**

  ```bash
  env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm test --workspace conversation-desktop -- --run test/conversation-session.test.ts
  ```

- [x] **Step 3: Implement one continuation gate**

  Add `continuationInProgress` beside `startPending`. Centralize the idle check
  so continuation and the methods it blocks share one state predicate. In
  `continueWithSeed`, set the gate before awaiting the SDK, clear it in
  `finally`, and mutate `turns`/`carriedContext` only after success. Preserve
  all runtime IDs; copied exchanges never become synthetic runtime turns.

- [x] **Step 4: Verify GREEN and existing voice/persona regressions**

  Run `conversation-session.test.ts` and `voice-session.test.ts`. Expected: all
  previous lifecycle assertions remain green.

### Task 7: Integrate List Deletion, Branch Continuation, and Recovery UI

**Files:**
- Modify: `apps/desktop/src/components/Workspace.tsx`
- Modify: `apps/desktop/src/styles.css`
- Modify: `apps/desktop/test/app.test.tsx`

**Interfaces:**
- Consumes: Task 2 revisioned store/preparation DTOs and Task 6 continuation operation/state.
- Produces: sibling list Open/Delete actions, shared two-step deletion flow, continuation confirmation/disclosure, branch persistence, carried-context rendering, and startup/reconnect saga reconciliation.

- [x] **Step 1: Write list-deletion behavior tests first**

  Test separate sibling Open/Delete buttons, keyboard order, first-click no
  delete, named confirmation, Cancel focus restoration, duplicate-action
  blocking, failed-delete alert/retry focus, successful next/previous/heading
  focus, detail-delete parity, SQLite revision arguments, and the existing
  save→delete→new-ID ordering without resurrection.

- [x] **Step 2: Implement the shared revisioned deletion flow**

  Change `onDelete` to `(id: string, revision: HistoryRevision)`, keep one
  `deletePendingId` and one `confirmDeleteId`, and queue the operation after
  pending saves. Update UI state only after native success. Preserve the active
  conversation offset/new-ID behavior while making native revision comparison
  the final stale-save guard.

- [x] **Step 3: Write continuation UI/integration tests first**

  Cover capability-visible/unavailable states, actual count/UTF-8-byte preview,
  current-model/persona/memory wording, no-eligible and newest-oversized errors,
  source revision conflict, prepare-before-seed ordering, pending operation
  controls, correlated failure cleanup, success navigation/focus, immutable
  source/new branch IDs, copied context plus only new live turns, source deletion
  survival, branch reopening, and preparing-state reconciliation after restart.

- [x] **Step 4: Implement the continuation saga**

  On confirm:

  ```text
  assert desktop session idle
  await historyWrite.current
  prepareContinuation(selected.id, selected.revision)
  await session.continueWithSeed(prepared seed + operation ID)
  setContinuationState(branch.id, branch.revision, "confirmed")
  set currentConversation to canonical branch
  reset live persistence baseline without synthetic runtime IDs
  switch to Conversation and focus composer
  ```

  On correlated rejection, revision-delete the preparing branch. On transport
  ambiguity, retain/name it as unconfirmed. On startup/reconnect, compare
  preparing operation IDs with `status.lastContextSeedOperationId` and mark
  confirmed or unconfirmed. Never infer success without the correlated result or
  matching status.

- [x] **Step 5: Render context and accessible controls**

  Replace the list row button with a wrapper and sibling Open/Delete controls.
  Add a labelled, collapsible carried-context section before live turns, with
  source title, exact count, and no implication that collapse removes runtime
  context. Apply shared light/dark semantic tokens, 42px-or-larger action
  targets, visible focus, destructive attention styling, and narrow-layout
  wrapping.

- [x] **Step 6: Verify focused desktop GREEN**

  ```bash
  env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm test --workspace conversation-desktop -- --run test/app.test.tsx test/conversation-session.test.ts test/conversation-history.test.ts
  env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm run typecheck --workspace conversation-desktop
  ```

### Task 8: Reconcile R6 Planning and Run the End-to-End Gate

**Files:**
- Modify: `ROADMAP.md`
- Modify: `docs/superpowers/plans/2026-08-24-r6-completion.md`
- Modify: `apps/desktop/README.md`
- Modify: `docs/r6-desktop-app-evaluation.md`
- Modify: `docs/r6-local-gateway-evaluation.md`

**Interfaces:**
- Consumes: Tasks 1–7 and their evidence.
- Produces: Task 7 Session phase, renumbered Guided Setup/Packaging/Evidence tasks, v2 compatibility disclosure, exact verification record, and open human gate.

- [x] **Step 1: Update roadmap and operator documentation**

  Insert Session Management and Continuation as R6 Task 7; renumber existing
  Tasks 7–9 to 8–10. Document v2 gateway/old-v1-client incompatibility, updated
  SDK v1-server compatibility, 16-exchange/32-KiB/no-compression semantics, and
  deletion provenance without claiming packaging or release.

- [x] **Step 2: Run formatting and focused static gates**

  ```bash
  cargo fmt --all -- --check
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo clippy --locked --workspace --all-targets -- -D warnings
  env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm run build --workspace @conversation/runtime
  env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm run build --workspace conversation-desktop
  ```

- [x] **Step 3: Run complete automated tests**

  ```bash
  CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked --workspace -- --test-threads=1
  env PATH=/opt/homebrew/bin:/opt/homebrew/opt/rustup/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm test
  ```

  Expected: all workspace tests pass. If loopback tests fail with sandbox
  `EPERM`, rerun the exact affected command with the already approved elevated
  test prefix and distinguish sandbox failure from source failure.

- [x] **Step 4: Inspect semantic and worktree boundaries**

  Run `git diff --check`, inspect the complete diff, built TypeScript exports,
  schema migration tests, protocol fixture parity, branch/remote status, and all
  untracked files. Confirm no unrelated pre-existing Task 5/6 work was reverted,
  staged, committed, or pushed.

- [ ] **Step 5: Perform native application verification**

  Start one v2 local gateway and the Tauri app on a free Vite port. Verify list
  delete, detail delete, source cascade deletion, Continue confirmation, one
  typed follow-up, one spoken follow-up, branch reopening, source deletion after
  continuation, app restart reconciliation, and v1-unavailable disclosure.
  Record any hardware/provider limitation instead of substituting unit tests.

- [ ] **Step 6: Hand off the human visual gate honestly**

  Product-owner checks light/dark confirmation contrast, Session row padding,
  keyboard/focus order, carried-context clarity, 200% zoom, and narrow layout in
  the real app. Mechanical completion remains distinct from this subjective
  acceptance.
