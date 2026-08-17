# R6 Persona and Memory Mutation Controls + Opt-In Memory Extraction

Date: 2026-08-15
Status: Approved

## Goal

Deliver the remaining R6 deliverable "persona inspection and mutation controls,
plus runtime-memory mutation controls backed by actual runtime state", and add
an explicitly opt-in gateway-side memory extraction writer so completed
exchanges can produce memory records without violating the R5 rule that the
runtime never writes conversation content automatically.

Three tracks:

1. **Protocol + gateway mutation commands** — `persona_get`, `persona_update`,
   `memory_approve`, `memory_delete`, plus a content-free `memory_extracted`
   gateway event.
2. **Opt-in extraction writer** — a gateway module, enabled only by a
   `[memory.extraction]` config section, that turns completed exchanges into
   `MemoryDraft`s via the configured local language model.
3. **Desktop controls** — a Settings pane with named persona presets
   (client-persisted, replayed on connect) and Approve/Delete controls in the
   Memory pane.

Out of scope (follow-ups, not in this change): live TTS voice/speed switching,
per-persona system prompts or voices, memory edit/pin UI, voice-driven memory
deletion, per-persona memory stores.

## Design constraints honored

- R5: the **runtime crate stays write-free**. The extraction writer lives in
  `apps/runtime-gateway` and uses explicit `MemoryStore` operations.
- The store's approval invariants stand: Identity/Relationship drafts always
  enter as `Candidate`; the gateway never synthesizes approval evidence from
  transcripts. Approval evidence comes only from an explicit
  `memory_approve` command (a user click).
- Rejection conventions: closed error-code set, `&'static str` messages, no
  request/transcript content echoed, request-scoped rejections, guard order
  `active turn → active voice → feature absent → operation error`.
- The gateway remains a read-only-config process; persona persistence is
  client-side (desktop preferences), replayed on connect.
- Capabilities tuple stays unchanged: persona commands are always available
  (every gateway has a persona); memory mutation commands reuse the existing
  `memory_inspection` gate.

## 1. Protocol (crates/protocol)

New wire commands, following the `memory_list` anatomy in `client_wire.rs`
(domain enum variant, `WireClientCommand` DTO, decode+validate arm, response
envelope, outgoing validation, shared JSONL fixtures):

- `persona_get { request_id }` → response
  `persona_state { request_id, persona }`.
- `persona_update { request_id, persona }` → `command_accepted`, then
  `persona_state { request_id, persona }` echoing the applied state.
  `persona` is a full replacement: `{ mode, warmth, humor, teasing,
  initiative, directness, intimacy, verbosity, follow_up_frequency }`, all
  levels validated 0–100 (`PersonaLevel`), mode one of the four
  `ConversationMode` wire names. Partial updates are not supported.
- `memory_approve { request_id, memory_id, expected_revision }` →
  `command_accepted`, then `memory_inspection { request_id, inspection }`
  with the post-approval record (reuses the existing inspection response
  shape). The gateway builds `MemoryApproval` with
  `confirmation_id = request_id`, `actor = "local-user"`,
  `confirmed_at = now`.
- `memory_delete { request_id, memory_id, expected_revision }` →
  `command_accepted`, then `memory_deleted { request_id, memory_id }`.
- New gateway **event** (event lane, content-free):
  `memory_extracted { created, activated, pending_approval }` — counts only,
  no memory content, mirroring the `MemoryRetrieved` content-free trace
  convention.

New/reused error codes: reuse `memory_turn_active`, `memory_disabled`,
`memory_not_found`, `memory_unavailable`. Add `memory_conflict` (revision
mismatch on approve/delete/edit-family operations) and `persona_invalid`
(rejected persona payload that passed wire shape but failed domain
validation). Both added to the mirrored TS `RuntimeFailure.code` union.

A persona snapshot DTO (`ClientPersonaState`) lives in a new
`crates/protocol/src/client_persona.rs`, re-exported from `lib.rs`.

## 2. Gateway (apps/runtime-gateway)

### Persona mutation

- `ConversationQualityController` gains `set_persona(profile, mode)` which
  replaces `saved_persona`, `default_mode`, and recomputes
  `default_controls`. To make that possible the verbosity→spoken-budget
  curve (`10 + v*7/10 + max(v-60,0)*3`, currently `config.rs:221-235`) moves
  into the quality layer (single source of truth; `config.rs` calls it).
- `ConversationContext` gains an accessor the gateway session can use to
  apply a persona between turns; application is rejected while a turn is
  pending (existing `resolve_turn` pending guard semantics).
- Session dispatch arms for `persona_get` / `persona_update` guard exactly
  like memory commands (no active text turn, no active voice session), then
  read/apply via the shared context and reply with `persona_state`.
  Updates take effect on the next turn; nothing is persisted server-side.

### Memory mutation

- Session gains `memory_approve` / `memory_delete` arms using the existing
  `MemoryInspectionAdapters` store handle inside `spawn_blocking`.
  `MemoryStoreErrorKind::Conflict → memory_conflict`,
  `NotFound → memory_not_found`, others → `memory_unavailable`.

### Extraction writer (`apps/runtime-gateway/src/memory_extraction.rs`)

- Enabled only when config contains `[memory.extraction]` (requires
  `[memory]`): fields `max_memories_per_turn` (default 3, cap 5) and
  `episodic_retention_days` (default 90).
- Hooked where the session observes a completed exchange (both text and
  voice lanes). Runs as a spawned async task; **single-flight** — if the
  previous extraction has not finished, the new exchange is skipped.
  Failures are logged and never affect the conversation or session.
- Sends one generation request to the configured language model: a fixed
  instruction prompt that treats the exchange as untrusted data and asks for
  a JSON array of at most N facts, each
  `{ kind: semantic|episodic|identity|relationship, content, explicit,
  confidence }` where `explicit` marks a deliberate user "remember this"
  request. Malformed output → extraction is dropped for that turn.
- Mapping to `MemoryDraft`:
  - provenance kind: `UserProvided` when `explicit`, else
    `CompletedExchange`; source names the turn id.
  - retention: `UntilDeleted` for semantic/identity/relationship;
    `Until(now + episodic_retention_days)` for episodic.
  - `Working` kind is never created by extraction.
  - store rules then yield the hybrid flow: identity/relationship →
    `Candidate`, others → `Active`.
- Dedup: before creating, the writer scans existing records via the store
  list API and skips exact content matches against non-expired records.
- After creation it emits the `memory_extracted` event with counts.

## 3. TypeScript SDK (packages/typescript)

Mirror of the protocol additions: types + parse/encode/validate in
`protocol.ts`, client methods `getPersona()`, `updatePersona(persona)`,
`approveMemory(memoryId, expectedRevision)`,
`deleteMemory(memoryId, expectedRevision)` using the `Deferred`/pending-map
pattern, a `memoryExtracted` event surfaced through the existing state
subscription, exports in both `index.ts` and `browser.ts`.

## 4. Desktop (apps/desktop)

- **Settings pane**: new `WorkspaceView` `"settings"` + rail button (always
  visible; no capability gate). Contents: preset picker (named persona
  presets), mode `<select>`, eight `<input type="range">` sliders (0–100),
  Apply/Save. Presets live in `Preferences` (version bump 3→4) as
  `personaPresets: { name, persona }[]` plus `activePresetName`; on session
  ready the app replays the active preset via `updatePersona` so settings
  survive gateway restarts. Initial state comes from `getPersona()` so the
  pane shows actual runtime state (R6 exit criterion).
- **MemoryPane**: detail view gains Approve (only when
  `state === "candidate"`) and Delete buttons, both sending the record's
  current `revision`; `memory_conflict` gets a human retry message that
  re-fetches the inspection. `memory_extracted` events show a quiet
  transient notice ("N memories saved · M awaiting approval") in the
  conversation view and refresh the memory list if open.
- Error copy extends `memoryErrorMessage`; pre-flight guards extend
  `ensureMemoryReady` symmetry for the new commands.

## 5. Testing

TDD throughout, following existing patterns:

- Shared fixtures: append the four commands + two responses + event to
  `tests/fixtures/client-wire-v1/*.jsonl` (+ unsupported/invalid lines);
  both `crates/protocol/tests/client_wire.rs` and
  `packages/typescript/test/protocol.test.ts` consume them.
- Gateway `mod tests`: accept-before-response ordering, guard rejections
  (turn active, voice active, disabled), conflict and not-found mapping,
  rejection-then-status survival, persona update visible on next turn's
  guidance/budget, extraction single-flight and failure isolation
  (extraction module unit-tested with a stub language endpoint).
- TS client tests: correlation, typed rejections, health after rejection.
- Desktop vitest: settings pane render/gating/apply, preset persistence +
  replay-on-connect, MemoryPane approve/delete flows, extraction notice.
- Quality gates before done: `cargo test --workspace`,
  `cargo clippy --workspace` (strict), TS build + `node --test`,
  `vitest run` — all locally.

## 6. Enabling the feature in a deployment (outside the repo)

Deployment configuration is private and never committed. To turn the feature
on: initialize a memory database with the `tests/memory` probe CLI, add
`[memory]` (+ `[memory.extraction]`) to the private gateway configuration, and
keep the configured `system_prompt`'s capability description truthful as
features land (see `configs/gateway.example.toml` for the generic shape).
