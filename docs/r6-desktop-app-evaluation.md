# R6 Desktop App Evaluation

**Date:** 2026-08-24
**Scope:** Public SDK, compiled local gateway voice lane, shared typed/spoken
desktop session model, live-capable Voice Focus, and native-acceptance boundary

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

## Ownership and Protocol Boundary

| Layer | Current responsibility |
| --- | --- |
| Public `@conversation/runtime` SDK | Protocol-v1 validation, bounded framing, request correlation, typed/voice lifecycle streams, transport-neutral client, Node stdio transport, browser-safe entry. |
| Rust runtime | Monotonic typed/spoken identifiers, one-active-turn arbitration, shared completed context, persona/quality decisions, optional memory, cancellation, backpressure, terminals. |
| Local gateway reference host | Private config loading, local adapter composition, shared runtime/context ownership, framed stdio, gateway and sidecar cleanup; no network listener. |
| Tauri desktop reference app | Explicit microphone intent, shared conversation presentation, app transcript history, Focus preferences/scenes, accessibility, visible privacy and background voice status. |

Public gateway protocol v1 now carries text, memory-inspection, and voice-session
commands/events. Gateway configuration schema v1 and the private sidecar
protocol are separate version domains. Unsupported versions fail explicitly.

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
| Focus scenes | Seven scenes, lazy chunks, hidden transcript default, preference migration, and reduced-motion fallback pass automated checks. | Native GPU appearance has not received a recorded human pass. |

## Complete Mechanical Gate

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
calls through the desktop boundary. This is separate from the compiled
SDK-to-gateway smoke, which currently covers typed/spoken conversation flow
only. A compiled SDK-to-gateway persona and memory mutation test remains R6
completion work.

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

The current branch has no new human native run. The following remain `skipped`,
not passed:

```text
Native window observed: skipped
Microphone permission observed: skipped
Shared typed/spoken transcript: skipped
Audible playback observed: skipped
Audible barge-in observed: skipped
Exit choices observed: skipped
Composer pause/resume observed: skipped
Child cleanup observed by a human: skipped
```

Use [the native macOS checklist](r6-desktop-voice-session-native-check.md) and
keep device names, private paths, transcripts, and exact model/voice selections
in an untracked local record.

## Open Work

- Additional runtime-memory management beyond candidate approval and deletion.
- Compiled SDK-to-gateway coverage for persona and runtime-memory mutation.
- Local model setup and benchmark UI.
- Packaging, signing, notarization, installation, and upgrade validation.
- Native microphone, playback, device, GPU scene, and child-process observation.
- R3 post-fix spoken turn, ten-minute run, first-audible measurement,
  audible-stop p95, and 30-sample external acoustic procedure.

## Status

The R6 shared desktop voice-session implementation is mechanically validated
and developer-runnable. R6 remains open for the product and distribution work
above. R3 was closed separately by product-owner acceptance on 2026-08-23;
this document does not independently verify its human, device, latency,
continuity, or acoustic observations.
