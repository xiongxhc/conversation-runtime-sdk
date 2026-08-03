# R5 Controlled Memory Design

**Date:** 2026-08-02

## Problem

Conversation memory is useful only when the user can tell what was stored, why
it was selected, and how to remove it. Persisting every exchange or hiding
retrieval behind an opaque prompt would turn a local-first runtime into an
uninspectable surveillance layer. R5 therefore needs a real SQLite path and a
runtime integration seam without automatic durable capture, invisible
promotion, unbounded context, or a model-specific memory backend.

## Approaches Considered

### 1. Put SQLite directly inside `conversation-runtime`

This minimizes the number of crates, but couples turn orchestration to one
storage engine and makes later application or test clients import the full
audio runtime for memory controls.

### 2. Keep memory entirely application-owned

This preserves a small runtime, but does not prove the SDK boundary, retrieval
trace, or deletion-to-context guarantee required by R5. Each application would
also reinvent the same safety rules.

### 3. Add a typed memory contract and a separate SQLite reference crate

This is the selected approach. `conversation-protocol` owns portable memory
types, `conversation-memory` owns the store contract and SQLite reference
implementation, and `conversation-runtime` consumes a replaceable context
provider. SQLite remains the first local implementation rather than becoming a
requirement for every future backend.

## Scope

R5 delivers:

- explicit initialization of a local SQLite database;
- working, episodic, semantic, identity, and relationship records;
- inspection, editing, pinning, approval, expiration, and hard deletion;
- provenance, confidence, timestamps, retention, last use, and retrieval reason;
- deterministic bounded retrieval with content-free traces;
- optional turn integration that adds retrieved records as typed context;
- a local CLI probe for exercising every control without a desktop app.

R5 does not deliver:

- automatic transcript capture;
- model-based extraction or summarization;
- embeddings, a vector database, or semantic similarity claims;
- cloud synchronization, organization memory, or iPhone-owned memory;
- encryption keys, credentials, or pairing secrets in SQLite;
- a desktop memory editor, which remains R6.

## Public Types

`conversation-protocol` adds validated, non-interchangeable types for:

- `MemoryId` and `RetrievalTraceId`;
- `MemoryKind::{Working, Episodic, Semantic, Identity, Relationship}`;
- `MemoryState::{Candidate, Active, Expired}`;
- `MemoryRetention::{Working, Session, Until, UntilDeleted}`;
- `MemoryProvenanceKind::{UserProvided, UserEdited, CompletedExchange,
  ApplicationImported}`;
- `MemoryProvenance`, with source identifier, source timestamp, actor, and an
  optional content digest but no hidden supporting excerpt;
- `MemoryApproval`, with explicit confirmation identifier, actor, confirmation
  timestamp, and expected record revision;
- `MemoryApprovalEvidence`, which binds that confirmation to the approved
  revision and SHA-256 content digest while preserving content provenance;
- `MemoryConfidence`, represented as an integer from `0` through `1000`;
- `UnixTimestampMillis`, which must be non-negative;
- `MemoryRecord`, `MemoryDraft`, `MemoryPatch`, and `MemoryInspection`;
- `MemoryRetrievalRequest`, `MemoryContextItem`, `MemoryRetrievalReason`, and
  `MemoryRetrievalTrace`.

Record content is non-empty and at most `4 KiB`. One retrieval may request at
most eight records and `8 KiB`; the default runtime budget is four records and
`4 KiB`. Public trace data contains identifiers, kinds, reasons, byte counts,
and exclusion counts, but never the query or memory content.

## Store Boundary

`conversation-memory` exposes a backend-neutral `MemoryStore` contract for
CRUD, approval, expiration, and retrieval. `SqliteMemoryStore` is the local
reference implementation.

The runtime consumes `MemoryContextProvider`, not `rusqlite` or a SQLite
connection. The SQLite provider opens a bounded operation on a blocking worker,
uses a short busy timeout, and returns typed errors. A deployment can replace
it without changing the turn, language, or protocol contracts.

Database creation is explicit. `initialize(path)` rejects relative paths,
creates the parent directory only as part of that explicit operation, enables
foreign keys and WAL mode, and applies committed migrations transactionally.
`open(path)` requires an existing database with a supported schema. Tests use
temporary directories.

The documented macOS default remains:

`~/Library/Application Support/Conversation Runtime/runtime.sqlite3`

The library accepts an absolute path and does not discover a home directory.
The CLI or future application resolves the default path and displays it before
initialization. No database is created merely by starting a voice session.

## Schema

Migration `0001_controlled_memory.sql` creates:

- `schema_migrations` for exact schema versions;
- `memories` for current inspectable records and retrieval metadata;
- `memory_sources` for inspectable provenance and approval evidence;
- `retrieval_traces` for content-free per-turn budgets and totals;
- `retrieval_items` for selected memory identifiers, order, and reasons.

Enum values use checked text constraints. Booleans use checked integers.
Confidence, byte counts, revisions, and timestamps have checked numeric bounds.
The database sets an SQLite application identifier, pins a migration checksum,
and rejects modified, foreign, or newer schemas. `foreign_key_check` must pass
on open. Retrieval items and provenance rows reference memories with
`ON DELETE CASCADE`, so hard deletion removes the record, source and approval
evidence, and identifier-bearing trace rows. Aggregate trace counts may remain
because they contain no content or memory identifier.

Every mutating operation accepts `expected_revision`, runs in one immediate
transaction, and increments the record revision. A stale operation fails rather
than overwriting newer user changes. The current record plus source rows provide
the inspectable state; R5 does not retain content-bearing revision history.
Retrieval selects and logs records, updates last-use metadata, and commits those
changes atomically only if cancellation remains clear.

Explicit initialization rejects symbolic-link database leaves, creates its
parent with owner-only permissions, and creates the database with owner-only
permissions. Existing parent directories are never chmod-ed implicitly.

## Write and Promotion Rules

The runtime never writes conversation content automatically. A caller must use
an explicit store operation to create a record.

- Working records require an expiry no more than 24 hours after creation.
- Expired working records become `Expired` before retrieval; inspection returns
  them with their expired state so the user can inspect or delete them.
- Working records cannot be pinned. Preserving one requires an explicit new
  episodic or semantic record rather than defeating automatic expiry.
- Session retention carries a session identifier and expires at explicit
  session-end cleanup.
- `Until` expires when `now >= expires_at`; candidate records expire under the
  same retention rule as active records.
- Episodic and semantic records may be created active only through an explicit
  caller operation with visible provenance and retention.
- Identity and relationship records are always created as `Candidate`.
- Only `approve(memory_id, expected_revision, approval)` can activate an
  identity or relationship candidate.
- Approval requires explicit user-confirmation evidence and appends it as a
  source row. The runtime itself has no approval API and can never synthesize
  confirmation evidence.
- Pinning non-working records changes retention to `UntilDeleted`, preserves the
  previous retention for unpin, and never approves a candidate.
- One completed exchange therefore cannot silently become durable identity or
  relationship state.

Relationship records are fallible context. They never modify persona, response
controls, or affection policy directly, and they never command a scripted
expression.

## Retrieval

The SQLite reference retriever is deterministic and lexical. It is not called
semantic search. It considers only active, unexpired records and ranks:

1. pinned records with an explicit query match;
2. exact phrase matches;
3. shared normalized terms or Unicode character pairs;
4. recent working records;
5. confidence, last use, creation time, and identifier as stable tie-breakers.

Pinned records do not bypass relevance, the turn byte budget, or item budget. A record larger than
the remaining budget is skipped rather than truncated. The trace reports the
selected order, reason, used bytes, and counts excluded by state, expiry,
relevance, item limit, and byte limit. Query text is used only for the current
operation and is never written to SQLite or telemetry.

## Runtime Flow

When no provider is configured, the current R4 path is unchanged. In R5,
retrieved memory may enter generation only when both the store and language
model are local. Hybrid or cloud sessions may keep a local store disabled, but
they cannot export memory content to a remote language adapter; explicit export
consent is deferred. When a local provider is explicitly configured:

1. privacy and memory configuration are validated before microphone access;
2. the finalized transcript becomes a bounded retrieval request;
3. retrieval completes before the language request is constructed;
4. the conversation quality controller remains independent of retrieved content
   and does not mutate persona, response controls, or completed history;
5. `LanguageModelInput` carries typed memory items separately from recent
   conversation history;
6. the language adapter labels them as untrusted, fallible data, never
   instructions or system policy;
7. a reliable content-free `MemoryRetrieved` event exposes trace identifier,
   item count, and used bytes before generation begins;
8. deletion or expiration prevents the record from entering every later turn.

Memory failure is a typed `RuntimeStage::Memory` failure. There is no silent
fallback to an empty result, remote store, or untraced prompt injection.
The provider uses an injected clock and a deterministic scan capped at `1024`
candidate rows. Retrieval runs
inside the active turn task, after cancellation tokens and terminal ownership
exist but before the language request is constructed. Cancellation is
checked before the query, during row processing, and immediately before any
trace or last-use write. A cancelled retrieval rolls back and leaves no metadata
mutation; the runtime awaits blocking-worker cleanup before publishing its
terminal result.

Retrieval traces and last-use fields are automatic content-free control metadata
required for inspection. They never store transcript, query, or memory content,
and they are distinct from prohibited automatic conversation-content capture.

Memory has a per-turn item and byte budget and also participates in a `40 KiB`
total language-input byte limit covering transcript, recent history, runtime
guidance, and memory. Memory items are skipped whole to fit the remaining total
budget; individual records are never truncated. This byte limit is a deterministic
safety bound, not a claim about provider tokenization or model context quality.

## Voice Configuration

Schema-v2 voice configuration gains an optional `[memory_store]` table with:

- `database_path`, which must resolve to an absolute local path;
- `maximum_items`, from one through eight;
- `maximum_bytes`, from one through `8192`.

It is valid only with exactly one enabled `[[memory]]` descriptor whose provider
is `sqlite` and execution is `local`, and with a local language-model descriptor.
A descriptor without store configuration,
store configuration without a descriptor, remote execution, a missing database,
or an unsupported schema fails before sidecar spawn. Memory remains disabled by
default.

## CLI Probe

`conversation-memory-probe` provides explicit commands:

- `default-path`;
- `init --database <absolute-path>`;
- `add`, `list`, and `inspect`;
- `edit`, `pin`, `unpin`, `approve`, `expire`, and `delete`;
- `retrieve --query <text> --maximum-items <n> --maximum-bytes <n>`.

Output is stable key-value or JSON without implicit transcript logging. The
probe makes R5 testable before R6 supplies graphical controls.

## Testing and Acceptance

Deterministic tests cover:

- validation and serialization of every public type;
- migration identity and unsupported-schema rejection;
- absolute-path and explicit-initialization rules;
- CRUD, revisions, pinning, approval, expiration, and hard deletion;
- two-step identity and relationship activation;
- explicit approval evidence and stale-revision rejection;
- working-memory expiry at the exact boundary;
- working-memory pin rejection and session-end expiration;
- multilingual lexical retrieval, stable ranking, and strict budgets;
- content-free traces and query non-persistence;
- retrieval cancellation with no trace or last-use mutation and busy-timeout failure;
- runtime injection, adapter serialization, deletion-to-context behavior, and
  unchanged no-provider behavior;
- pre-microphone voice configuration rejection;
- CLI end-to-end controls on a temporary database.

SQLite hard deletion removes all application-visible rows and future retrieval
access. It is not a cryptographic secure-erasure claim for previously allocated
SQLite pages, WAL files, filesystem snapshots, or storage media. The evaluation
documents checkpoint/WAL cleanup behavior and this limit explicitly.

R5 is complete when the roadmap exit criteria pass through these public controls.
This milestone does not claim that lexical retrieval equals model-dependent
semantic relevance or that a desktop user experience exists.

Deleting a record prevents every later retrieval. It cannot retract a copy
already placed into an in-flight language request; removing memory from the
current turn requires cancelling that turn.
