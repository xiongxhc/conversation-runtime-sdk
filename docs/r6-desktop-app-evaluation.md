# R6 Desktop App Evaluation

**Date:** 2026-09-02
**Scope:** Public SDK, compiled local gateway voice lane, shared typed/spoken
desktop session model, Session Management and Continuation, live-capable Voice
Focus, and native-acceptance boundary

## Result

The R6 desktop voice-session slice passes the complete mechanical gate. The
desktop now consumes the public voice-session surface rather than presenting an
idle-only shell: explicit Start, acknowledged capture pause/resume, Stop,
shared typed/spoken history, recoverable failure, background microphone status,
Voice Focus exit choices, persona inspection/update, candidate-memory approval,
and revision-bound memory deletion are implemented and independently reviewed.

This is deterministic and compiled integration evidence, not native acoustic
evidence. No current-branch human observation is recorded for microphone
permission, physical input/output devices, audible playback, audible barge-in,
GPU scene quality, first-audible latency, audible-stop latency, or ten-minute
continuity. Those observations remain unverified in this document. R3 was
closed separately by product-owner acceptance on 2026-08-23; the underlying
raw human and acoustic observations are not stored in this checkout.

Task 5 is implemented and independently reviewed. It provides four native
setup commands for bounded loopback discovery, numeric-only model benchmarking,
private configuration preparation, and temporary-provider ownership cleanup.
The guided React setup UI, bundle/DMG, signing/install work, final packaging
and release evidence, and real-device acceptance remain open.

Session Management and Continuation is implemented across desktop schema-2
history, protocol v2, the shared runtime context, the public TypeScript SDK, and
the connected desktop UI. List and detail deletion use revision comparison.
Continue creates a separately persisted branch from an immutable source, copies
at most the latest 16 whole completed nonblank exchanges and 32,768 UTF-8
content bytes while retaining the 16-KiB per-message limit, and presents copied
context separately from new live turns. It does not restore an old provider
session, historical model/persona/memory state, device state, or runtime IDs,
and performs no compression or summarization.

Branch persistence records source provenance, copied-context/live-turn origin,
revision-CAS state, `preparing`/`confirmed`/`unconfirmed` recovery, and the last
opaque operation ID used to reconcile with runtime status. Source deletion does
not cascade into copied branch context.

## Ownership and Protocol Boundary

| Layer | Current responsibility |
| --- | --- |
| Public `@conversation/runtime` SDK | Strict protocol-v1/v2 validation, Ready-version-aware encoding/decoding, bounded framing, request correlation, context seeding, typed/voice lifecycle streams, transport-neutral client, Node stdio transport, browser-safe entry. |
| Rust runtime | Monotonic typed/spoken identifiers, one-active-turn arbitration, shared completed context, persona/quality decisions, optional memory, cancellation, backpressure, terminals. |
| Local gateway reference host | Private config loading, local adapter composition, shared runtime/context ownership, framed stdio, gateway and sidecar cleanup; no network listener. |
| Tauri desktop reference app | Explicit microphone intent, shared conversation presentation, app transcript history, Focus preferences/scenes, accessibility, visible privacy and background voice status. |

The public gateway now speaks protocol v2, including
`conversation_context_seed` and the nullable last seed operation ID. The
updated TypeScript client preserves existing operations against a v1 server and
rejects seed locally there; an old v1-only client binary cannot connect to the
v2 gateway. Gateway configuration schemas v1/v2 and the private sidecar protocol
remain separate version domains. Unsupported versions fail explicitly.

## Testable Surface

| Surface | Current evidence | Boundary |
| --- | --- | --- |
| Shared conversation | Runtime and compiled SDK tests prove typed→spoken→typed turns preserve completed history and monotonic identities through one context. | Deterministic fixture content does not establish model usefulness or quality. |
| Voice start/stop | Desktop tests prove Focus entry is idle, Start is explicit, pending start can be stopped, failure can retry, and Stop rejection restores usable state. | Physical permission and device behavior still require the native checklist. |
| Composer coexistence | Desktop and runtime tests prove pause acknowledgement precedes typed send and same-session resume follows typed terminal. | Human interaction timing and OS focus behavior remain a native observation. |
| Voice Focus exit | Stop, Keep, Cancel, session replacement, pending-stop focus trap, persistent failure, and retry are test-covered. | Audible stop and child observation require native/acoustic checks. |
| Barge-in and cleanup | Rust tests cover generation/synthesis/queue/playback cancellation, blocked output, EOF, repeated Stop, failure, and child reaping. | No external recording proves audible-stop p95. |
| Partial/final text | Partial hypotheses remain transient; final transcript and exact completed assistant text are retained under backpressure. | No private transcript or ASR-quality claim is recorded. |
| History, memory, and persona | App history is local SQLite and separate from optional runtime memory. The public SDK provides memory inspection, candidate approval, revision-bound deletion, persona inspection, and persona update. | Memory creation, editing, pinning, and other management controls remain open. |
| Session deletion and continuation | Desktop tests cover revision-CAS list/detail deletion, immutable-source branch preparation, exact bounded context selection, correlated seeding, operation-ID recovery, carried-context/live-turn separation, branch reopening, and source-deletion survival. | Automated coverage does not establish native focus feel, visual clarity, provider usefulness, spoken hardware behavior, or restart behavior in the packaged app. |
| Focus scenes | Seven scenes, lazy chunks, hidden transcript default, preference migration, and reduced-motion fallback pass automated checks. | Native GPU appearance has not received a recorded human pass. |

## Complete Mechanical Gate

Current Session Management verification observed on 2026-09-02 used Node.js
v24.9.0 and npm 11.6.0 from the explicit PATH below:

```text
cargo fmt --all -- --check
passed

CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo clippy --locked --workspace --all-targets -- -D warnings
passed

CARGO_TARGET_DIR=/Users/cx/Workspace/conversation-runtime-sdk/target cargo test --locked --workspace -- --test-threads=1
passed; all workspace targets and doc tests completed with zero failures

env PATH=/opt/homebrew/bin:/opt/homebrew/opt/rustup/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm test
@conversation/runtime: 109 passed
conversation-node-chat: 14 passed
conversation-desktop: 13 files, 257 passed

env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm run build --workspace @conversation/runtime
passed

env PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin npm run build --workspace conversation-desktop
passed; TypeScript checks, production build, and scene-chunk assertion

git diff --check
passed
```

The following is the historical pre-Session R6 checkpoint from 2026-08-10; it
is retained for provenance and is not the current final gate:

Observed on 2026-08-10:

```text
cargo fmt --all -- --check
passed

cargo clippy --workspace --all-targets --locked -- -D warnings
passed

cargo test --workspace --locked --no-fail-fast
passed; one immutable version-one fixture writer remains intentionally ignored

swift test --package-path platform/macos/voice-sidecar
116 passed

npm test --workspaces
@conversation/runtime: 74 passed
conversation-node-chat: 14 passed
conversation-desktop: 10 files, 136 passed

npm run build --workspaces
TypeScript, desktop type checks, Vite production build, and scene-chunk checks passed

git diff --check
passed
```

The SDK workspace test builds the actual Rust gateway and fake managed sidecar,
creates a disposable gateway configuration and ASR directory, binds temporary
loopback language and speech fixtures, and proves one typed→spoken→typed flow
through the public client. It validates shared completed history and removes its
temporary directory and providers. Rust compiled-gateway tests separately cover
voice accept-to-terminal, request-scoped rejection, EOF child reaping, capture
pause/resume acknowledgement, blocked output, and terminal ordering. Runtime
tests separately cover barge-in cleanup; it is not claimed as compiled-gateway
integration or audible evidence. Typed public-SDK tests exercise `getPersona`
and `updatePersona`, plus revision-bound `approveMemory` and `deleteMemory`,
through a test transport; desktop session and pane tests exercise the same
calls through the desktop boundary. Compiled public-SDK-to-gateway coverage
for persona and runtime-memory mutation is also present. This is automated
interface evidence, not native or human observation.

## Developer Run

Use a private configuration outside the repository. Leave `[voice.*]` commented
for text-only mode, or configure all local voice components explicitly as
described in [the desktop README](../apps/desktop/README.md). Then run:

```bash
npm ci
npm run build --workspace @conversation/runtime
cargo build --locked -p conversation-runtime-gateway
npm run desktop:dev
```

Entering Voice Focus does not start the microphone. Select `Start voice` only
after verifying the displayed component locality. The app never silently falls
back to a remote provider.

## Native Acceptance Status

A fresh debug macOS app bundle was inspected on 2026-09-02. These
non-destructive checks were observed:

```text
Native window and local connection: observed
Session list Open/Delete controls: observed
Named permanent-delete confirmation: observed
Cancel focus restoration: observed
Session detail Continue/Delete controls: observed
Exact bounded continuation preview: observed
Continuation cancel without branch mutation: observed
Dark How-it-responds control contrast/padding: observed
Disconnect and child-process cleanup: observed
```

The check deliberately did not mutate saved data, send a provider request, or
start microphone capture. The following remain `skipped`, not passed:

```text
Microphone permission observed: skipped
Shared typed/spoken transcript: skipped
Audible playback observed: skipped
Audible barge-in observed: skipped
Exit choices observed: skipped
Composer pause/resume observed: skipped
Child cleanup observed by a human: skipped
Permanent list/detail deletion observed: skipped
Typed follow-up after confirmed continuation: skipped
Spoken follow-up with carried context observed: skipped
Branch reopen and source-deletion survival observed: skipped
Restart recovery and v1-unavailable disclosure observed: skipped
```

Product-owner visual/acoustic acceptance remains a separate open gate,
including light/dark review, narrow layout, 200% zoom, keyboard flow, carried
context clarity, Voice Focus cohesion, and real-device acoustic judgment.

Use [the native macOS checklist](r6-desktop-voice-session-native-check.md) and
keep device names, private paths, transcripts, and exact model/voice selections
in an untracked local record.

## Open Work

- Additional runtime-memory management beyond candidate approval and deletion.
- Guided local model setup and benchmark UI (the native setup commands are
  available, but the React UI remains open).
- Packaging, signing, notarization, installation, and upgrade validation.
- Native data-mutating Session deletion/continuation, typed and spoken
  follow-up, branch reopening, source-deletion survival, restart recovery, and
  v1-unavailable disclosure.
- Native microphone, playback, device, GPU scene, and child-process observation.
- R3 post-fix spoken turn, ten-minute run, first-audible measurement,
  audible-stop p95, and 30-sample external acoustic procedure.

## Status

The R6 shared desktop voice-session implementation is mechanically validated
and developer-runnable. R6 remains open for the product and distribution work
above. R3 was closed separately by product-owner acceptance on 2026-08-23;
this document does not independently verify its human, device, latency,
continuity, or acoustic observations.
