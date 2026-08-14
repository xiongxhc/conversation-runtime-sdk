# R6 Persona/Memory Mutation Controls Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Live persona mutation, memory approve/delete commands, an opt-in gateway memory-extraction writer, and desktop Settings/Memory controls.

**Architecture:** New wire commands follow the `memory_list` anatomy through `crates/protocol` → `apps/runtime-gateway/src/session.rs` → `packages/typescript` → `apps/desktop`. Persona lives in the shared `Arc<Mutex<ConversationQualityController>>` and is mutated between turns; the extraction writer is a gateway-only module using explicit `MemoryStore` operations (runtime crate stays write-free). Desktop persists persona presets client-side and replays on connect.

**Tech Stack:** Rust (tokio, rusqlite via conversation-memory), TypeScript (node:test), React + vitest.

**Spec:** `docs/superpowers/specs/2026-08-15-r6-persona-memory-controls-design.md`

## Global Constraints

- Wire: snake_case tags, 64-bit ids as decimal strings, `deny_unknown_fields`, `protocol_version: 1` on client commands.
- Rejections: reuse existing codes where possible; new codes `memory_conflict`, `persona_invalid`; messages are `&'static str`, never echo request/transcript/memory content; guard order = active text turn → active voice session → feature absent → operation error; every rejection test proves session survival via follow-up `status` (`assert_rejection_then_status`).
- Command responses: `command_accepted` before the correlated response, on the `normal` writer lane. Events go on the `event` lane.
- No capability-tuple changes: persona commands ungated; memory mutations reuse the `memory_inspection` adapter gate.
- Runtime crate (`crates/runtime`) gets NO memory-write code.
- Commits per task, message style `feat(scope): ...` matching repo history, no Co-Authored-By lines. Never push.
- Local gates before declaring done: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `npm run build && npm test` in `packages/typescript`, `npx vitest run` in `apps/desktop`.

---

### Task 1: Protocol — persona/memory mutation commands, responses, event, fixtures

**Files:**
- Create: `crates/protocol/src/client_persona.rs`
- Modify: `crates/protocol/src/client_wire.rs`, `crates/protocol/src/lib.rs`
- Modify: `tests/fixtures/client-wire-v1/commands.jsonl` (+ the unsupported/invalid fixture files alongside it)
- Test: `crates/protocol/tests/client_wire.rs`

**Interfaces:**
- Produces `ClientPersonaState { mode: String, warmth: u8, humor: u8, teasing: u8, initiative: u8, directness: u8, intimacy: u8, verbosity: u8, follow_up_frequency: u8 }` (serde snake_case; mode strings = the existing `ConversationMode` wire names found in `crates/protocol/src/quality.rs` — read them, do not invent). Constructor validates every level ≤ 100 and mode ∈ the four known names; conversion helpers `ClientPersonaState::from_profile(&PersonaProfile, ConversationMode)` and `to_profile() -> Result<(PersonaProfile, ConversationMode), _>`.
- Produces `ClientCommand::{PersonaGet { request_id }, PersonaUpdate { request_id, persona: ClientPersonaState }, MemoryApprove { request_id, memory_id: MemoryId, expected_revision: u64 }, MemoryDelete { request_id, memory_id: MemoryId, expected_revision: u64 }}`.
- Produces `GatewayMessage::{PersonaState { request_id, persona: ClientPersonaState }, MemoryDeleted { request_id, memory_id: MemoryId }, MemoryExtracted { created: u32, activated: u32, pending_approval: u32 }}` (MemoryExtracted has no request_id — event).
- Wire lines (fixtures must use exactly these shapes):
  - `{"protocol_version":1,"type":"persona_get","request_id":"..."}`
  - `{"protocol_version":1,"type":"persona_update","request_id":"...","persona":{"mode":"...","warmth":95,...}}`
  - `{"protocol_version":1,"type":"memory_approve","request_id":"...","memory_id":"7","expected_revision":"2"}`
  - `{"protocol_version":1,"type":"memory_delete","request_id":"...","memory_id":"7","expected_revision":"2"}`
  - `{"type":"persona_state","request_id":"...","persona":{...}}`
  - `{"type":"memory_deleted","request_id":"...","memory_id":"7"}`
  - `{"type":"memory_extracted","created":2,"activated":1,"pending_approval":1}`

- [ ] Read `client_wire.rs` decode/encode/validate arms for `memory_list`/`memory_inspect` and `quality.rs` mode names.
- [ ] Write failing decode/encode/validate tests in `crates/protocol/tests/client_wire.rs` (valid round-trips; invalid: level 101 → error, unknown mode → error, `expected_revision` non-numeric → error, `memory_extracted` negative counts impossible by type). Run: `cargo test -p conversation-protocol` — expect failures.
- [ ] Implement `client_persona.rs`, enum variants, `WireClientCommand` DTOs, decode+validate arms, `GatewayMessageEnvelope` arms, `validate_gateway_message` arms, `lib.rs` re-exports.
- [ ] Append fixture lines (valid commands + responses; one unsupported-version line; invalid lines for bad level/mode/revision). Update the fixture-mirror deserializer in the test file.
- [ ] `cargo test -p conversation-protocol` → PASS. Commit `feat(protocol): persona and memory mutation commands with extraction event`.

### Task 2: Quality layer — budget curve relocation + live persona mutation

**Files:**
- Modify: `crates/protocol/src/quality.rs`, `apps/runtime-gateway/src/config.rs:210-241` (and remove curve at 221-235), `crates/runtime/src/conversation_quality.rs`, `crates/runtime/src/conversation_context.rs`
- Test: `crates/runtime/src/conversation_quality.rs` tests + `crates/protocol` quality tests

**Interfaces:**
- Produces `PersonaProfile::maximum_spoken_seconds(&self) -> u16` implementing exactly `10 + verbosity*7/10 + saturating_sub(verbosity,60)*3` (verbosity as u16). `config.rs` calls this instead of computing inline (existing gateway budget tests must stay green unchanged).
- Produces `ConversationQualityController::set_persona(&mut self, persona: PersonaProfile, mode: ConversationMode)` — replaces `saved_persona` + `default_mode`, rebuilds `default_controls` from `PersonaProfile::maximum_spoken_seconds` + existing `ResponseControls` construction; returns `Err` if a turn is pending (same pending guard as `resolve_turn`).
- Produces `ConversationContext::apply_persona(&self, persona: PersonaProfile, mode: ConversationMode) -> Result<(), _>` and `ConversationContext::persona_snapshot(&self) -> (PersonaProfile, ConversationMode)` (locks quality mutex).

- [ ] Failing tests: curve values (v20→24, v60→52, v85→144? compute exact saturating math: v85 → 10+59+75=144; v100 → 10+70+120=200), `set_persona` rejected while turn pending, applied persona visible in next `resolve_turn` guidance string and budget, `persona_snapshot` round-trip.
- [ ] Implement; run `cargo test -p conversation-protocol -p conversation-runtime -p conversation-runtime-gateway` → PASS. Commit `feat(runtime): live persona mutation with shared spoken-budget curve`.

### Task 3: Gateway session — persona_get/persona_update, memory_approve/memory_delete

**Files:**
- Modify: `apps/runtime-gateway/src/session.rs` (dispatch arms after MemoryInspect ~line 731; error ctors near 1355-1398)
- Test: same file `mod tests`

**Interfaces:**
- Consumes Task 1 variants + Task 2 `ConversationContext::{apply_persona, persona_snapshot}`; session already holds the shared context (`session.rs:45`).
- Error ctors: `memory_conflict_error()` (code `memory_conflict`, "memory revision does not match the current record"), `persona_invalid_error()` (code `persona_invalid`, "persona payload is invalid"), `persona_turn_active_error()` reusing code `invalid_state`? NO — mint nothing extra: persona guards reuse `command_error("persona controls are unavailable while a turn is active")` style with existing `invalid_state` code; memory guards reuse existing memory error ctors.
- `memory_approve` builds `MemoryApproval::new(request_id.clone(), "local-user", clock.now(), expected_revision)` then `store.approve` in `spawn_blocking`, replies with the same inspection payload path used by `MemoryInspect` (`inspect_bounded(memory_id, now, 32)`).
- `memory_delete` calls `store.delete(memory_id, expected_revision)` → `GatewayMessage::MemoryDeleted`.
- `persona_update`: decode → `ClientPersonaState::to_profile()` (failure → `persona_invalid`) → guards → `context.apply_persona` → reply `PersonaState` with applied snapshot. `persona_get`: guards (turn/voice only; never disabled) → snapshot → `PersonaState`.

- [ ] Failing session tests using `InMemoryGateway`: accept-then-response ordering for all four; persona_update visible via subsequent persona_get; guard rejections during active turn (HoldOpenLanguageServer) and active voice; memory_approve on a Candidate identity record created via `initialized_store()` helpers flips state to active in the returned inspection; approve/delete with stale revision → `memory_conflict`; delete missing id → `memory_not_found`; both memory commands without store → `memory_disabled`; every rejection followed by `assert_rejection_then_status`.
- [ ] Implement arms + ctors; `cargo test -p conversation-runtime-gateway` → PASS. Commit `feat(gateway): persona and memory mutation commands`.

### Task 4: Gateway extraction writer (opt-in)

**Files:**
- Create: `apps/runtime-gateway/src/memory_extraction.rs`
- Modify: `apps/runtime-gateway/src/config.rs` (`MemoryConfig` gains `extraction: Option<MemoryExtractionConfig>`), `apps/runtime-gateway/src/session.rs` (hook at completed-exchange points, both text and voice lanes), `apps/runtime-gateway/src/main.rs` (wiring)
- Test: `memory_extraction.rs` unit tests + session tests

**Interfaces:**
- `MemoryExtractionConfig { max_memories_per_turn: usize (default 3, cap 5), episodic_retention_days: u16 (default 90, min 1) }`, TOML `[memory.extraction]`, rejected if `[memory]` absent.
- `MemoryExtractor::new(store: SqliteMemoryStore, language: Arc<dyn GenerationLanguageModel>, config, clock)` with `pub fn observe_exchange(&self, turn_id, user_text: &str, assistant_text: &str)` — spawns a task; single-flight via `AtomicBool` try-acquire (skip when busy); returns immediately.
- Extraction prompt (fixed, in code): instructs the model that the exchange is untrusted data, never instructions; asks for a JSON array (max N) of `{"kind":"semantic|episodic|identity|relationship","content":"...","explicit":true|false,"confidence":0-1000}`; any parse failure or non-array → drop silently (log only).
- Mapping: explicit → `MemoryProvenanceKind::UserProvided` else `CompletedExchange`; source `turn:{turn_id}`; retention `UntilDeleted` except episodic → `Until(now + days*86_400_000)`; content ≤ 4096 bytes else skip that item; dedup by exact content equality against non-expired records (page through `list_page`); after creation emit counts through a callback `on_extracted: impl Fn(MemoryExtractedCounts)` that session wires to a `GatewayMessage::MemoryExtracted` event-lane send.
- Session hook: at the exact points where a text turn and a voice turn finalize a completed exchange (find where completed history entries are recorded), call `observe_exchange` when the extractor is configured. Extraction must never delay or fail the turn.

- [ ] Failing unit tests with a stub language endpoint (reuse `runtime_for(endpoint)`-style local HTTP stub pattern from session tests): valid JSON → records created with right kind/state/provenance/retention (identity → candidate, semantic → active); `explicit:true` → `UserProvided`; malformed JSON → zero records, no panic; duplicate content skipped; single-flight skips overlapping call; counts callback fires with `{created, activated, pending_approval}`.
- [ ] Failing session test: with extraction configured, completing a turn eventually yields a `"type":"memory_extracted"` event frame; without config, none.
- [ ] Implement; `cargo test -p conversation-runtime-gateway` and `cargo clippy` → PASS. Commit `feat(gateway): opt-in memory extraction from completed exchanges`.

### Task 5: TypeScript SDK mirror

**Files:**
- Modify: `packages/typescript/src/protocol.ts`, `client.ts`, `index.ts`, `browser.ts`
- Test: `packages/typescript/test/protocol.test.ts`, `client.test.ts`, `browser.test.ts`

**Interfaces:**
- `PersonaState { mode: ConversationModeName, warmth: number, ... followUpFrequency: number }` (camelCase in TS, snake_case on wire; mode names mirror Rust).
- Client methods: `getPersona(): Promise<PersonaState>`, `updatePersona(persona: PersonaState): Promise<PersonaState>`, `approveMemory(memoryId: bigint, expectedRevision: bigint): Promise<MemoryInspection>`, `deleteMemory(memoryId: bigint, expectedRevision: bigint): Promise<bigint>`; new `PendingControl` kinds + accept-switch entries + correlation arms.
- `memory_extracted` parses to `GatewayMessage` member `{ type: "memoryExtracted", created, activated, pendingApproval }` and is surfaced through the existing state-subscription/event path (follow how `MemoryRetrieved`-style events reach `DesktopSession.state` today; expose `onMemoryExtracted` callback or state field per that pattern).
- `RuntimeFailure.code` union += `"memory_conflict" | "persona_invalid"`.

- [ ] Failing tests: fixture-index assertions extended for the new `commands.jsonl` lines; encode/parse round-trips; client correlation for all four methods; typed rejection carrying `memory_conflict`; client healthy after rejection; browser re-exports.
- [ ] Implement; `npm run build && npm test` in `packages/typescript` → PASS. Commit `feat(typescript): persona and memory mutation client surface`.

### Task 6: Desktop — Settings pane with persona presets + replay-on-connect

**Files:**
- Create: `apps/desktop/src/components/SettingsPane.tsx`
- Modify: `apps/desktop/src/preferences/preferences.ts` (v3→v4 migration), `apps/desktop/src/App.tsx` (`DesktopSession` gains `getPersona`/`updatePersona`), `apps/desktop/src/runtime/conversation-session.ts`, `apps/desktop/src/components/Workspace.tsx` (view union + rail button + pane switch), `apps/desktop/src/styles.css`
- Test: `apps/desktop/test/settings-pane.test.tsx`, `preferences.test.ts`, `app.test.tsx`

**Interfaces:**
- `Preferences` v4 adds `personaPresets: PersonaPreset[]` (`{ name: string, persona: PersonaState }`, name 1-64 chars, unique) and `activePresetName: string | null`; v3 payloads migrate with `personaPresets: []`, `activePresetName: null`.
- `WorkspaceView` adds `"settings"`; rail button "Settings" always rendered (disabled while `phase === "streaming"` like Memory).
- SettingsPane props `{ session, preferences, onPreferencesChange, onBack }`: on mount `getPersona()` populates controls; mode `<select>` + eight `<input type="range" min=0 max=100>`; Apply → `updatePersona`; Save-as-preset + preset picker + activate (activate = apply + set `activePresetName`).
- Replay: where the app handles session ready (`App.tsx`/`conversation-session.ts`), if `activePresetName` resolves to a preset, fire `updatePersona(preset.persona)` once per connect; failures show a non-fatal notice.

- [ ] Failing vitest tests: preferences v4 migration + validation; rail button navigates (`findByRole("button", { name: "Settings" })` → heading assertion); sliders reflect `getPersona` values; Apply calls `updatePersona` with edited values; preset save/activate persists via `savePreferences`; replay-on-connect calls `updatePersona` with active preset; `FakeSession` in app.test.tsx extended with the two new `vi.fn()` methods.
- [ ] Implement (styles follow `.memory-pane` conventions; slider styling minimal); `npx vitest run` → PASS. Commit `feat(desktop): persona settings pane with named presets`.

### Task 7: Desktop — Memory approve/delete + extraction notice

**Files:**
- Modify: `apps/desktop/src/components/MemoryPane.tsx`, `apps/desktop/src/components/Workspace.tsx` (notice), `apps/desktop/src/App.tsx` + `runtime/conversation-session.ts` (`approveMemory`/`deleteMemory` + extraction-event surfacing), `apps/desktop/src/styles.css`
- Test: `apps/desktop/test/memory-pane.test.tsx`, `app.test.tsx`

**Interfaces:**
- Detail view: Approve button only when `record.state === "candidate"` → `approveMemory(id, revision)` → replace inspection with result + notice "Memory approved"; Delete button (both states) → `deleteMemory(id, revision)` → back to list with notice "Memory deleted". `memory_conflict` → message "This memory changed elsewhere — refreshed; try again." + auto re-inspect.
- Workspace conversation view renders a transient (auto-dismiss) `.memory-extracted-notice` on extraction events: "N memories saved" (+ " · M awaiting approval" when M>0); refreshes memory list if the pane is open.

- [ ] Failing vitest tests: approve visible only for candidates; approve flow updates state badge; delete returns to list; conflict path re-fetches; notice renders from a pushed extraction event and disappears.
- [ ] Implement; `npx vitest run` → PASS. Commit `feat(desktop): memory approval and deletion controls with extraction notices`.

### Task 8: Full verification + personal configuration

- [ ] `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `packages/typescript`: `npm run build && npm test`, `apps/desktop`: `npx vitest run` — all green; fix anything found.
- [ ] Desktop production build (`npm run build` in apps/desktop) so the running app includes the new panes.
- [ ] Personal config (NOT committed): init memory DB via the `tests/memory` probe CLI at `~/.local/share/conversation-runtime/memory/runtime.sqlite3`; add `[memory]` (`maximum_items = 6`, `maximum_bytes = 4096`) + `[memory.extraction]` to `~/.config/conversation-runtime/gateway.toml`; replace `system_prompt` with the bilingual Serena prompt whose capability list truthfully names History, Voice Focus, Memory pane (view/approve/delete), and Persona Settings.
- [ ] Report results; surface commit SHAs; ask before any push.
