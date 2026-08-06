# R6 Desktop Memory Inspection Design

**Date:** 2026-08-06

## Problem

The desktop app can report that semantic memory is local or disabled, but it
cannot show what the runtime actually stores. Reintroducing a `Memory` button
without a real runtime-backed surface would repeat the dead-control problem.
Opening the SQLite file directly from Tauri would also bypass the public SDK
boundary and couple the desktop app to one storage implementation.

The first desktop memory slice therefore needs to expose existing controlled
memory through the same bounded local gateway and TypeScript client used by
text chat. It must remain local-only, backend-neutral at the protocol boundary,
inspectable, paginated, and mutation-free in the UI.

## Approaches Considered

### 1. Open SQLite directly from Tauri

This is the shortest path to a screen, but it duplicates memory schema logic,
bypasses the public runtime interface, and prevents future clients from using
the same inspection contract. It is rejected.

### 2. Spawn `conversation-memory-probe`

This reuses existing behavior but turns human-oriented CLI output into an
application protocol, adds another process lifecycle, and still leaves the
public SDK without memory inspection. It is rejected.

### 3. Add typed inspection commands to the public runtime protocol

This is the selected approach. The gateway retains an inspection handle to the
same local store used by runtime retrieval. Rust projects validated records to
bounded wire DTOs, the TypeScript SDK correlates requests and responses, and
the desktop renders only data returned by that public interface.

## Scope

This slice delivers:

- a capability-gated `Memory` destination in the desktop app;
- keyset-paginated local memory summaries with bounded content previews;
- full record inspection with kind, state, confidence, retention, timestamps,
  pin state, revision, last use, and last retrieval reason;
- the latest 32 provenance entries and latest 32 approval entries in ordered,
  bounded history windows, with explicit truncation indicators and no stored
  digests;
- explicit empty, disabled, not-found, unavailable, and error states;
- public Rust and TypeScript inspection contracts;
- real compiled-gateway interoperability tests using a temporary initialized
  memory database.

This slice does not deliver:

- create, edit, approve, pin, unpin, expire, retrieve, or delete controls;
- automatic transcript capture or automatic memory extraction;
- direct database access from React or Tauri;
- database-path disclosure in the public protocol;
- cloud memory, synchronization, embeddings, or semantic-search claims;
- Persona or Settings screens;
- live microphone or playback activation.

## User Experience

The navigation shows `Memory` only when runtime status includes the
`memory_inspection` capability. A text-only runtime with memory disabled keeps
the control absent and continues to show `Memory off` in runtime status.

The Memory list shows newest-created records first using the stable total order
`id DESC`. Memory IDs are immutable, so expiry and edits cannot reorder an
in-progress traversal. Each row contains:

- a bounded single-paragraph content preview;
- a human-readable kind and state;
- a pinned indicator when applicable;
- the last updated date.

Selecting a row opens a read-only detail view. Full memory content is visible
only in this detail view. The view distinguishes memory from transcript
history and explains that stored memory may be used as fallible context, not
as an instruction or fixed behavior. Identity and relationship entries show
their candidate or approved state; the UI never implies that they authorize an
affectionate expression.

The detail view groups metadata, provenance, and approval history. Source and
confirmation identifiers are shown because they are existing inspectable
provenance. Each history is limited to its latest 32 entries and the UI states
when older entries exist. SHA-256 content digests are not sent to or rendered
by the desktop because they add no user value in this read-only surface.

The first page loads when Memory opens. A `Load more` control appears only when
the gateway returns another cursor. The UI never reports that all records are
visible while another page remains. The cursor is the final immutable ID from
the previous page, not a numeric offset. Newer records created concurrently
appear on refresh. Concurrent deletion may remove a not-yet-loaded record, but
expiry and edits cannot skip or duplicate remaining records.

Memory navigation is disabled while a text turn is active. If a turn starts
between the UI check and the command reaching the gateway, the gateway rejects
inspection as `memory_turn_active`. The user can stop or finish the response
and retry. This prevents inspection housekeeping from contending with runtime
retrieval and ensures interruption commands are never delayed by inspection.

## Lifecycle Semantics

“Read-only” means the desktop offers no user mutation operation. Existing
memory lifecycle behavior remains authoritative: listing or inspecting at the
current clock may transition already-due records to `Expired`. This is expiry
housekeeping defined by R5, not an edit initiated by the UI.

The inspection surface does not call retrieval. It therefore creates no
retrieval trace and does not update `last_used_at` or retrieval reason.

## Public Wire Contract

The shared typed-start slice advances the client protocol from version 2 to
version 3. The gateway-owned start identifier and its correlated acceptance are
not silently added to v2 because existing clients validate exact command and
response shapes. A v2 client therefore rejects a v3 gateway as unsupported,
and a v3 gateway rejects v2 commands. The Rust gateway, shared fixtures,
TypeScript SDK, Node example, and desktop app move together.

The protocol-v3 commands are:

```text
memory_list(request_id, cursor)
memory_inspect(request_id, memory_id)
```

`cursor` is either `null` or an object containing the previous page's final
`before_id`. The gateway owns the fixed page size of 50 records.
`memory_id`, record identifiers, revisions, timestamps, and session identifiers
use canonical non-negative decimal strings on the wire; identifiers and
revisions must also be non-zero. TypeScript parses them as `bigint`, never as
IEEE-754 numbers.

The correlated responses are:

```text
memory_list(request_id, records, next_cursor)
memory_inspection(request_id, inspection)
```

`next_cursor` is either a keyset cursor or `null`. A list record contains
only:

```text
id, content_preview, kind, state, pinned, updated_at_ms
```

An inspected record contains:

```text
id, kind, content, state, confidence, created_at_ms, updated_at_ms,
pinned, revision, retention, last_used_at_ms, last_retrieval_reason
```

Retention is a tagged object containing its kind and only the applicable
expiry timestamp or session identifier. Provenance contains kind, source
identifier, source timestamp, and actor. Approval history contains
confirmation identifier, actor, confirmation timestamp, and approved revision.
The inspection response also carries `sources_truncated` and
`approvals_truncated` booleans beside the two bounded history arrays.

Provenance and approval arrays contain at most 32 entries each and are
oldest-to-newest within that latest-entry window. `sources_truncated` and
`approvals_truncated` state whether older entries exist. The latest provenance
is always last. When current approval evidence exists, the latest approval is
always last. The gateway projects only already-validated `MemoryInspection`
values, preserving the R5 invariant even though content digests are omitted.

`content_preview` is at most 192 UTF-8 bytes. Projection collapses all Unicode
whitespace runs to one ASCII space. If truncation is needed, it cuts only at a
UTF-8 scalar boundary and reserves the final three bytes for `…`, so the entire
encoded preview remains within 192 bytes.

The wire projection is backend-neutral. It names controlled-memory concepts,
not SQLite tables, database files, Ollama, or desktop components.

## Rust Boundaries

`conversation-protocol` owns the new client commands, response DTOs, strict
serialization, identifier encoding, enum projection, and frame-size checks.
Unknown fields, invalid enum values, malformed identifiers, invalid cursor
shapes or `before_id` values, and oversized frames fail closed.

`conversation-memory` adds a bounded keyset page operation and a bounded
inspection operation to `MemoryStore` and the SQLite implementation. List
applies existing expiry housekeeping and queries one page plus one lookahead
record so `next_cursor` is exact without loading all memory content. The SQL
order is `id DESC`; a cursor adds `WHERE id < before_id`. Record updates and
expiry therefore cannot move records across the traversal boundary. Inspection
loads the latest 33 provenance and approval rows separately, drops the lookahead
row, reverses each retained window to oldest-to-newest, and reports whether
older rows were truncated. This prevents unbounded history from exceeding the
512 KiB frame limit or exhausting memory before encoding.

Gateway configuration retains cloned access to one `SqliteMemoryStore` when
memory is configured. The same store path backs both the runtime retrieval
provider and inspection; no second database or schema is introduced.

`GatewaySession` receives an optional backend-neutral inspection handle and a
clock. List and inspect operations run on a blocking worker only when no text
turn is active. The session rejects memory commands before spawning work when a
turn exists, so the command loop remains immediately available for
`interrupt_turn` and inspection cannot contend with that turn's retrieval.

The gateway advertises `memory_inspection` only in protocol-v3 status and only
when the local inspection store exists. `ClientRuntimeError` gains a required,
stable `code`. Memory request codes are `memory_disabled`, `memory_turn_active`,
`memory_not_found`, and `memory_unavailable`. Initialization, schema, path,
busy, and storage failures map to `memory_unavailable` because configuration
validation already prevents a session from starting with an unusable memory
store. Clients branch on the code, never diagnostic text. All memory failures
are request-scoped rejections and do not terminate a healthy gateway session.
Existing non-memory failures use `adapter_failure`, `configuration_invalid`,
or `invalid_state`, preserving the current kind-level meaning while making the
v2 code field exhaustive.

## TypeScript SDK

`@conversation/runtime` adds browser-safe `MemorySummary`, `MemoryPage`,
`MemoryRecord`, `MemoryInspection`, provenance, approval, and retention types.
`RuntimeClient` adds:

```ts
listMemories(cursor?: MemoryCursor): Promise<MemoryPage>
inspectMemory(memoryId: bigint): Promise<MemoryInspection>
```

Timestamps, identifiers, revisions, and session identifiers are `bigint` in
the public TypeScript types. The desktop formatter handles values outside the
JavaScript `Date` range as explicit out-of-range metadata rather than losing
precision.

Both methods use the existing acceptance-before-response correlation rule.
Duplicate, early, unknown, or mismatched responses fail the client rather than
being silently accepted. Closing or transport failure rejects pending memory
requests exactly once.

The browser entry exports these types and methods without exporting Node stdio
or desktop application code.

## Desktop Integration

`ConversationSession` exposes the two SDK methods through the existing desktop
session boundary. React does not invoke Tauri for semantic memory and never
opens the memory database.

The Memory view is a focused component rather than another branch inside the
already large workspace transcript component. It owns list pagination,
selection, loading, retry, and readable metadata formatting. Workspace owns
navigation, disables entry during streaming, and passes the verified session
capability. The gateway remains the authority for the active-turn race.

History and Memory remain separate:

- History is application-owned transcript storage in the Tauri app-data file.
- Memory is optional runtime-owned controlled context returned by the gateway.
- Opening either surface does not restore a past transcript as active model
  context.

## Privacy and Security

- The feature is available only with `privacy_mode=local_only`, local language,
  local memory, telemetry disabled, and the explicit inspection capability.
- Memory content travels only through the child-process framed stdio channel
  and the in-process Tauri bridge.
- There is no network listener and no remote fallback.
- Public examples remain backend-neutral and contain no private paths or model
  selections.
- Logs, diagnostics, rejection messages, and telemetry contain no memory
  content, source identifiers, actors, confirmation identifiers, or paths.
- Frame, 50-record page, 192-byte preview, 32-entry history-window, field,
  cursor, timestamp, and identifier bounds apply before allocation or
  rendering.
- The UI escapes content through normal React text rendering and does not render
  memory content as HTML or Markdown.

## Validation

Rust protocol tests cover exact v2 fixtures, v1/v2 rejection, every new strict
field, decimal identifiers and timestamps, enums, keyset cursors, projection,
preview truncation, and maximum frame behavior.

Memory-store tests cover immutable-ID keyset ordering, exact page boundaries,
lookahead behavior, expiry visibility, concurrent create/edit/delete behavior,
bounded latest-history windows, truncation flags, and no retrieval metadata
changes.

Gateway tests cover disabled rejection, enabled list and inspect, not-found
rejection, continued use after rejection, active-turn rejection followed by
successful interruption, oversized stored history remaining within one frame,
and content-free coded failures. A compiled gateway test uses an initialized
temporary SQLite database through framed stdio.

TypeScript tests cover parsing, encoding, request correlation, transport
failure, close behavior, pagination, and browser-safe exports.

Desktop tests cover hidden navigation when disabled, visible navigation when
enabled, empty memory, paginated summaries, detail metadata, provenance,
approval history, retryable errors, separation from History, and absence of
mutation controls.

The final gate runs formatting, strict Clippy, the full locked Rust workspace,
all npm workspace tests, all npm workspace builds, and a native launch smoke
using temporary configuration and a temporary initialized memory database.
Enabling or inspecting an operator's existing private database remains a
separate explicit action outside this implementation plan.

## Delivery

This feature is stacked on the completed desktop-history fix until that base is
integrated. It uses the intent-based branch
`feature/desktop-memory-inspection`. The implementation is committed only after
independent review and complete local validation. Push and merge remain
separate explicit integration actions.
