# R6 Session Management and Continuation Design

**Status:** Approved by the product owner on 2026-09-02.

## User Problem

People can see saved Sessions, but they cannot manage them where they are
listed or continue one with the model carrying its recent context. The current
read-only detail view makes a locally saved conversation look more useful than
it is, while deletion is needlessly hidden and context behavior is unclear.

## Decision

R6 gains a Session Management and Continuation phase after the UI/UX Foundation
and before Guided Setup.

This phase:

- adds safe, accessible deletion to the Sessions list while preserving detail
  deletion;
- makes deletion durably delete the SQLite conversation and its turns;
- adds **Continue as new conversation** as a bounded, capability-gated context
  seed;
- carries at most the **16 most recent completed exchanges** whose combined
  transcript and response content is at most **32 KiB (32,768 UTF-8 bytes)**;
- keeps the source Session immutable and creates a separately persisted branch;
- uses the currently connected model, current persona, and current memory
  policy rather than pretending to restore historical model state; and
- adds no summarization or compression in this phase.

Continuation is a runtime operation with an explicit protocol capability. It
is not implemented by pasting history into the next user prompt, reopening an
old provider session, or letting the gateway read the desktop SQLite database.

## Scope and Ordering

This becomes Task 7 of the R6 completion plan. Existing Guided Setup,
Packaging, and Mechanical Evidence tasks move to Tasks 8, 9, and 10.

The phase changes desktop history persistence, the Rust/TypeScript public
protocol, the runtime conversation-context API, the gateway command handler,
the browser SDK, and desktop presentation/state. It does not change provider
configuration, model selection, memory approval policy, memory extraction,
voice device ownership, or setup flows.

## Current State

- Session detail deletion already invokes `delete_conversation_history`, which
  deletes the SQLite parent row. Foreign keys are enabled and its saved turns
  are removed by `ON DELETE CASCADE`.
- The Sessions list has only an Open action.
- Opening a Session reads a transcript copy into the detail pane. It never
  changes the active runtime context.
- The desktop history contains transcript turns, not a provider checkpoint: it
  has no historical model instance, provider session, persona snapshot, or
  memory snapshot.
- Text and voice already share one gateway-owned `ConversationContext`.
- The runtime currently retains eight completed exchanges within 16 KiB. This
  phase deliberately changes that active-history bound to 16 exchanges within
  32 KiB for both ordinary conversation and continued context.

## Terminology

- A **source Session** is the immutable saved conversation selected by the
  person.
- A **completed exchange** is one persisted turn with a completed state,
  nonblank user transcript, and nonblank final assistant response.
- A **context seed** is the ordered bounded copy sent to the runtime.
- A **branch** is the new active and newly persisted conversation created from
  that seed.
- **Live turns** are turns produced after the branch is created.

The product never labels the operation Resume or Restore because those words
would imply historical provider or model state that the app does not possess.

## Session List Deletion

Each Session list row becomes a non-interactive row wrapper containing sibling
**Open** and **Delete session** buttons. Buttons are never nested.

Delete is a two-step inline action:

1. **Delete session** reveals a confirmation naming the selected Session.
2. **Delete permanently** invokes the same persistence path used by detail
   deletion. **Cancel** makes no persistence call.

While deletion is pending, duplicate Open/Delete/confirmation actions for that
row are disabled. A failed deletion keeps the row visible, exposes a
`role="alert"` message, and returns focus to the initiating Delete button. Cancel
also returns focus to that button. After success, focus moves to the next
remaining Session's Open button, the previous row if there is no next row, or
the Sessions heading when the list is empty.

Detail deletion keeps the same two-step wording, pending-state protection,
error behavior, and backend delete operation.

## Context Seed Selection

The desktop derives candidates only from the source Session snapshot. A turn
is eligible if and only if:

- `state === "completed"`;
- `transcript.trim()` is nonempty; and
- `response.trim()` is nonempty.

Streaming, cancelled, failed, partial-recognition, and failure-message content
is excluded. Titles, timestamps, identifiers, separators, metadata, and wire
encoding overhead do not count as model context.

For each candidate:

```text
exchange_bytes = UTF8(transcript).byteLength + UTF8(response).byteLength
```

Whitespace trimming is used only to decide whether content is nonblank. The
original untrimmed transcript and response are copied and counted.

Selection walks eligible exchanges from newest to oldest and retains whole
pairs until adding another pair would exceed either 16 exchanges or 32,768
bytes. The retained pairs are restored to oldest-to-newest order before being
sent. A user transcript is never retained without its assistant response, and
content is never truncated.

Each transcript and response must also fit the existing 16 KiB individual
`ConversationMessage` limit. That limit is not increased. If the newest
eligible exchange violates either individual-message limit or alone exceeds
32 KiB, continuation is rejected with: “The latest exchange is too large to
continue without shortening or compression.” If an older exchange cannot fit
after one or more newer pairs were selected, selection stops before that
exchange and does not scan past it. The implementation never silently skips a
gap and seeds discontinuous context.

The TypeScript preview selector, native SQLite preparation command, protocol
parser, gateway, and Rust runtime apply the same count and byte rules. The
native command re-reads the source under its transaction and the runtime
validator remains authoritative for what can enter active model context.

## Continue User Flow

Session detail exposes **Continue as new conversation** only when the connected
runtime advertises the continuation capability. A v1-only runtime keeps the
control visible but unavailable and explains that the connected runtime cannot
continue saved context. No prompt-based fallback is offered.

The first activation opens a confirmation that states:

- up to the latest 16 completed exchanges and 32 KiB will be carried over;
- the new conversation uses the current model, current response persona, and
  memories currently active and eligible under the runtime's retrieval policy;
- the saved source remains unchanged; and
- this does not restore the exact historical model state.

The confirmation reports the actual selected exchange count and byte size. If
no eligible exchange exists, or the newest exchange is oversized, the person
gets an actionable error and no branch is prepared.

After confirmation succeeds, the app switches to Conversation and shows:

1. a labelled, read-only **Context carried over from _Session title_** section
   containing exactly the exchanges sent to the runtime; then
2. the new branch's live transcript and composer.

The carried context is visually distinct and may be collapsed, but its count,
source title, and disclosure remain visible. The previous active transcript is
not merged into this branch; it remains in its own saved Session. Starting a
new live turn appends below the carried context.

## Operation Safety and Recovery

Continue is one serialized desktop saga with two atomic boundaries: SQLite
branch preparation and runtime context replacement. SQLite and the gateway do
not form one distributed transaction, so the design records enough state to
recover instead of claiming impossible end-to-end atomicity.

It must reject before snapshot or context mutation when:

- a typed turn is pending or streaming;
- voice is starting, active, stopping, pausing, paused, or resuming;
- a voice session identifier is still live or its terminal event is draining;
  or
- the runtime/session is failed, closing, or closed.

It never cancels, stops, or discards active work as a side effect. The error
explains the immediate action required, such as waiting for the response or
ending Voice.

Once the person confirms, a `continuationInProgress` gate blocks typed send,
voice start, persona changes, another continuation, and disconnect until the
saga reaches a correlated result or the transport fails. This closes the
interval between SQLite preparation and gateway replacement; no new work can
race the seed.

The successful sequence is:

```text
confirm while idle
  -> flush the previous active conversation's pending history writes
  -> native SQLite transaction snapshots source revision
     and creates a preparing branch with copied context + operation ID
  -> gateway atomically validates and replaces bounded runtime history
  -> correlated success marks the branch confirmed
     and replaces the desktop's active transcript presentation
  -> focus moves to the composer
```

The source snapshot and branch creation occur in one SQLite transaction. The
native command re-runs selection against that snapshot rather than trusting
content sent back from the browser. It returns the canonical branch, source and
branch revisions, exact seed, and a random continuation operation ID. The
branch receives a new random ID and creation timestamp before any runtime
change. If the runtime returns a correlated rejection, the desktop compensates
by deleting the preparing branch, leaves the current active transcript/context
unchanged, stays on Session detail, and reports the failure. The source is never
modified.

The gateway command validates all input and lifecycle conditions before taking
the context write lock. It replaces runtime history in one critical section and
records the opaque operation ID as the active seed identity. Repeating the same
operation ID with the same seed is idempotent; reuse with different content is
rejected. Turn, generation, utterance, and voice-session identifiers are not
restored or reset; future identifiers remain runtime-owned and monotonic.

A successful correlated runtime result is the commit point from the user's
perspective. The prepared branch already exists at that point; marking it
confirmed is idempotent. Protocol v2 status exposes only the opaque last seed
operation ID, never content. If the transport closes or the app exits before a
correlated result, startup/reconnect compares that status value with preparing
branches. A match confirms the branch; a mismatch marks it unconfirmed and
keeps it as a valid local copy. The app does not switch transcript presentation
or guess that runtime context changed without either a correlated success or a
matching status value. An unconfirmed branch names its state and can be opened
to retry explicitly.

## Delete, Save, and Continue Races

History persistence adds an opaque `revision` to each conversation and uses
native compare-and-write operations instead of unconditional upsert for an
existing record.

- A save updates only the expected revision and returns the next revision.
- A new conversation inserts only when its ID does not exist.
- Delete compares the expected revision, deletes the parent row and cascading
  turns in one transaction, and reports not-found/conflict rather than silent
  success.
- Preparing a continuation compares the source revision and copies the selected
  seed into the new branch in one transaction.

These rules define the winner:

- If delete commits first, continuation reports “That saved conversation no
  longer exists” and does not create or seed a branch.
- If branch preparation commits first, later source deletion removes only the
  source. The branch retains its copied seed and remains usable.
- A queued stale save after deletion fails its revision comparison and cannot
  recreate the deleted ID.

The existing per-Workspace write queue remains useful for ordered UI updates,
but correctness no longer depends on one React instance being the only writer.

## Branch Persistence and Provenance

The native history schema gains:

- conversation `revision`;
- nullable `continuedFromId` provenance with no cascading foreign key to the
  source;
- continuation operation ID and `preparing`, `confirmed`, or `unconfirmed`
  recovery state for branch rows; and
- a persisted origin on each turn: `continued_context` or `live`.

On open, the store starts an immediate transaction and reads
`PRAGMA user_version`. A new database is created directly at schema 2. A
legacy schema-0 database receives additive columns with safe defaults, backfills
revision 1/no continuation state/live-origin turns, recreates affected indexes,
sets `PRAGMA user_version = 2`, and commits. Any failed step rolls back. Existing
databases remain readable without export/reimport. Deleting a source does not
delete, blank, or mutate copied branch context.

The prepared branch initially contains exactly the selected context exchanges,
marked `continued_context`. Subsequent turns are saved as `live`. Reopening the
branch presents both sections truthfully, but opening it remains read-only
until the person explicitly chooses Continue again.

The branch title is `Continued: <source title>`, truncated only at a valid UTF-8
boundary when needed to remain within the existing title byte limit. The source
ID is forensic provenance, not a promise that the source still exists.

## Runtime and Memory Semantics

Context seeding replaces only the runtime's bounded completed-exchange history.
It does not replace the `ConversationContext` object or any adapter. Therefore:

- the current model/provider configuration remains in use;
- the current persona and response controls apply to future turns;
- current active, unexpired memory selected by the retriever is used normally
  for each future turn, whether it was user-approved or policy-activated;
- no historical persona, mode, model, memory snapshot, device state, or runtime
  identifier is restored;
- seeding emits no memory-extraction proposal and performs no memory mutation;
  and
- the next typed or spoken turn receives the same seeded history.

The ordinary runtime history bound changes from eight exchanges/16 KiB to the
same 16 exchanges/32 KiB contract. Normal completed turns evict the oldest whole
exchange as needed. Existing persona-change and rapid-topic-change behavior may
still clear active history; the UI wording for those controls remains
authoritative.

The protocol history limit therefore changes from 16 to 32 messages and gains a
distinct 32 KiB aggregate history-byte constant. The individual 16 KiB message
constant remains unchanged. The model-adapter content envelope increases from
40 KiB to 64 KiB so it can contain, at the existing maxima, a 16 KiB current
message, 32 KiB history, 4 KiB runtime guidance, and 8 KiB retrieved memory.
These are byte safety limits, not a claim that every configured model has enough
tokens. A provider may still reject a later turn for its configured token
budget; the runtime reports that failure and never silently shrinks or
compresses the seed.

Seeded text is passed as structured ordinary user/assistant history. It is not
concatenated into deployment or runtime guidance and cannot acquire a system
role through desktop metadata.

No summarization, semantic retrieval over old transcripts, provider-managed
conversation ID, or automatic compaction is added. Those require a later design
with explicit quality, privacy, provenance, and failure acceptance criteria.

## Alternatives Rejected

- **Paste the transcript into the next user prompt:** a small code change, but
  it loses roles, lets historical text masquerade as instructions, bypasses
  runtime bounds, and falsely labels one oversized prompt as restored context.
- **Let the gateway open the desktop SQLite database:** avoids a wire command,
  but violates application/runtime ownership and couples a public gateway to a
  Tauri schema and local filesystem path.
- **Restore the old Session in place:** impossible from the stored transcript
  because provider state, model cache, persona snapshot, memory snapshot, and
  runtime identifiers were never persisted.
- **Summarize automatically:** could carry more history, but introduces a new
  model operation, lossy provenance, latency, failure, and privacy policy. It is
  intentionally deferred until separately specified and measured.

## Public Protocol and SDK

The wire protocol advances to v2 because current parsers require exact version,
command, key, and capability vocabularies. Silently adding the command to v1
would make old clients reject the same gateway without a truthful version
signal.

V2 adds:

- capability `conversation_context_seed`;
- client command `seed_conversation_context` with an ordered list of
  `{ user, assistant }` exchanges plus an opaque operation ID;
- `lastContextSeedOperationId` in v2 status for recovery; and
- one correlated success or typed failure result for every successfully decoded
  seed command with a valid request ID. Frames too malformed to preserve a
  valid request ID use the existing `invalid-command` rejection path.

The public TypeScript API adds:

```ts
type ConversationContextExchange = {
  user: string;
  assistant: string;
};

RuntimeClient.seedConversationContext(
  exchanges: readonly ConversationContextExchange[],
  operationId: string,
): Promise<void>;
```

The desktop `ConversationSession` adds a continuation operation that serializes
the command, publishes the carried-context presentation state only after
success, clears prior live turns from the active presentation, and preserves
runtime identifier monotonicity.

There is no version negotiation today: the server's initial `ready` message is
authoritative. The updated TypeScript client first parses that message with a
small version discriminator, stores version 1 or 2 on the connection, then uses
that version's exact encoder and decoder for every later command and event.
With v1 it preserves normal status, text, voice, persona, and memory behavior,
exposes no continuation capability, and rejects a direct seed call locally. The
updated gateway speaks v2. Existing v1-only client binaries are not compatible
with the v2 gateway; this synchronized breaking contract change must be called
out in release notes and examples rather than hidden.

Neither the protocol nor gateway receives desktop history IDs, titles,
revisions, database paths, turn states, or failure messages.

## Failure Language

Normal UI uses these bounded messages:

- busy text: “Wait for the current response before continuing a Session.”
- voice owned: “End Voice before continuing a Session.”
- missing source: “That saved conversation no longer exists.”
- changed source: “That saved conversation changed. Open it again to continue.”
- no eligible context: “This Session has no completed exchanges to continue.”
- oversized latest exchange: “The latest exchange is too large to continue
  without shortening or compression.”
- unsupported runtime: “The connected runtime cannot continue saved context.”
- correlated runtime rejection: “The new conversation could not be started.
  Your current conversation and saved Session were not changed.”
- lost connection before confirmation: “The runtime connection ended before
  continuation could be confirmed. A local continuation copy was saved; open it
  after reconnect to try again.”

Technical details remain available in Diagnostics rather than replacing the
recovery instruction.

## Accessibility

- Session list Open and Delete actions are separate keyboard stops with visible
  focus and names tied to the row title through `aria-describedby`.
- Confirmation moves focus to its heading or destructive action, traps no
  unrelated navigation, and restores focus according to the deletion rules.
- Pending, success, and failure states are announced without relying on color.
- The carried-context section has a heading and explicit source/count text; a
  collapse control exposes `aria-expanded` and does not remove the context from
  the runtime.
- Destructive controls use the shared attention tokens in both light and dark
  modes. Disabled capability states remain legible and explain their reason.

## Testing Strategy

### Desktop component and persistence tests

- Open and Delete are sibling controls and independently keyboard reachable.
- First Delete activation makes no persistence call; Cancel restores focus.
- Confirm removes only the intended row after native success.
- Failure retains the row, announces the error, and permits retry.
- Focus moves to next, previous, or Sessions heading after successful deletion.
- Detail deletion preserves the same behavior.
- Pending saves, delete, and new-conversation saves cannot resurrect a deleted
  ID.
- Continuation disclosure reports actual exchange count/bytes and current-state
  semantics.
- Success shows only copied context plus new live turns under a new ID.
- Failure, stale revision, or missing source leaves the current presentation and
  source unchanged.
- Send, voice start, persona changes, disconnect, and a second Continue remain
  blocked when execution is paused between SQLite preparation and the gateway
  result.
- Reopening a branch preserves copied context and provenance after source
  deletion.
- Startup/reconnect reconciles preparing branches against the runtime's opaque
  last seed operation ID and labels unmatched branches unconfirmed.
- Crash/restart before preparation, after preparation, after runtime replace,
  and after local confirmation produces the specified recoverable state.
- SQLite migration preserves existing histories and cascade deletion.

### Selector, protocol, and SDK tests

- Completed, nonblank pairs alone are eligible.
- Selection is newest-first, whole-pair, then sent oldest-to-newest.
- Exactly 16 exchanges and exactly 32,768 UTF-8 bytes pass.
- The preview/native selector stops before a seventeenth or over-budget older
  pair; direct protocol input over either limit rejects the entire command.
- A transcript or response over the unchanged 16 KiB individual-message limit
  rejects when newest and stops selection when encountered farther back.
- Multibyte Unicode is measured as UTF-8 rather than UTF-16 code units.
- An oversized newest exchange rejects without truncation or skipping.
- V1 has no seed capability; its normal status, text, voice, persona, and memory
  exchanges still use the v1 codec. V2 exact schemas round-trip in Rust and
  TypeScript; unknown keys and malformed pairs fail closed.
- History accepts 32 structured messages/32 KiB while preserving the individual
  16 KiB limit, and the model adapter accepts the resulting 64 KiB aggregate
  content envelope.
- Each successfully decoded seed command with a valid request ID yields exactly
  one correlated terminal result; unrecoverable IDs use `invalid-command`.

### Runtime and gateway tests

- Seed replaces completed history atomically only while fully idle.
- Active/pending text and every non-idle/live voice condition reject without
  cancellation or partial mutation.
- Repeating one operation ID with identical content is idempotent; reusing it
  with different content rejects.
- Current persona, model adapter, memory provider, and monotonic identifiers are
  preserved.
- Seeding emits no memory extraction or mutation.
- The next typed and spoken turns both receive the identical seeded context.
- Normal history retains at most 16 whole exchanges and 32 KiB.
- Provider token-budget rejection remains visible and does not silently shrink
  or compress valid byte-bounded history.

### Verification gates

- Run desktop tests, TypeScript typecheck, production build, and scene-chunk
  check.
- Run Rust formatting, protocol/runtime/gateway/desktop native tests, Clippy,
  and the complete workspace gate.
- Run database migration tests against both a fresh database and the current v1
  fixture.
- Run the real Tauri app against a local gateway and verify delete, Continue,
  one typed follow-up, one spoken follow-up, branch reopening, and source
  deletion.
- Product-owner visual review separately checks light/dark confirmation
  contrast, row spacing, keyboard/focus flow, branch-context clarity, and
  narrow-window behavior. Automated checks do not claim subjective acceptance.

## Acceptance Criteria

- A person can delete any Session from the list without first opening it, with
  a named confirmation and deterministic keyboard focus.
- Successful deletion removes the SQLite conversation and all saved turns;
  stale saves cannot recreate it.
- A capable, idle runtime can continue a selected Session as a new branch using
  exactly the bounded completed context disclosed in the UI.
- The source Session is never modified, and deleting it later does not damage
  the branch.
- The active transcript switches only after runtime seeding succeeds and never
  merges the previous active transcript with the selected Session.
- Continuation uses no more than 16 completed exchanges or 32,768 UTF-8 content
  bytes, retains only whole pairs, and never compresses or silently truncates.
- Current model, persona, and active memory retrieval policy govern new turns;
  the UI never claims exact historical state restoration.
- Both the next typed turn and the next spoken turn receive the seeded context.
- V1 behavior is explicit and capability-gated; the protocol change is
  versioned rather than smuggled into v1.
- Mechanical gates pass, and real-device visual/interaction acceptance remains
  explicitly owned by the product owner.

## Explicit Non-Goals

- Reopening an historical provider session or preserving a provider cache.
- Unlimited context growth.
- Summary generation, compression, embeddings, or semantic transcript search.
- Restoring historical persona, model, memory snapshot, voice device, or runtime
  identifiers.
- Mutating the source Session when a branch is continued.
- Allowing continuation to interrupt active text or voice work.
- Making the gateway aware of Tauri, SQLite, desktop paths, or history IDs.
