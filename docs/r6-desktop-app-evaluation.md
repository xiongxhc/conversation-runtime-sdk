# R6 Desktop App Evaluation

**Date:** 2026-08-06
**Scope:** macOS Tauri text-chat, Voice Focus preview, and protocol-v1
read-only runtime-memory inspection

## Result

The R6 desktop surface is developer-runnable with bounded automated checks.
Memory inspection is optional and explicit: the desktop uses the public local
runtime protocol only after an existing initialized local memory store is
configured. R6 is not complete: the production gateway reports text and, when
enabled, memory-inspection capabilities only; no live voice, persona or memory
mutation, packaged release, or R3 human/acoustic acceptance claim is made.

## Testable Surface

| Surface | Current evidence | Boundary |
| --- | --- | --- |
| Desktop launch | The root `desktop:dev` command starts Vite and the native Tauri binary. | The launch smoke was stopped after startup; it was not a human visual acceptance run. |
| Text chat | Setup, verified local-only status, send, streamed deltas, Stop, close, failure recovery, and reconnect are covered by desktop tests. | A person still needs a running configured loopback model service for an interactive turn. |
| Voice Focus shell | Preview entry, scene selection, hidden transcript default, explicit transcript reveal, `Escape`, reduced motion, and scene failure fallbacks are covered by tests and the production build. | Preview is intentionally idle and cannot imply microphone or playback activity. |
| Focus scenes | Soft Aurora, Silk, Threads, Prism, Orb, Still Gradient, and None are selectable; Soft Aurora is the default. | The five animated scenes have separate lazy chunks; final human GPU visual review remains open. |
| Local gateway bridge | Absolute-path validation, idempotent close, process reaping, and reopen ordering pass Rust tests. | The desktop validation in this report does not claim model quality, latency, or acoustic behavior. |
| Runtime memory inspection | With enabled local memory and advertised protocol-v1 `memory_inspection`, the Memory destination lists at most 50 summaries per page and opens read-only details through the browser-safe SDK. Provenance and approval histories retain at most their latest 32 entries and visibly mark truncation. | The desktop has no SQLite access, does not initialize memory, and does not copy conversations into it. Due expiry may be applied while inspecting; persona and all memory mutation remain open. |

## Reproduce the Developer Run

Run these commands from the repository root on macOS:

```bash
npm ci
npm run build --workspace @conversation/runtime
cargo build --locked -p conversation-runtime-gateway

PRIVATE_GATEWAY_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/conversation-runtime/gateway.toml"
mkdir -p "$(dirname "$PRIVATE_GATEWAY_CONFIG")"
cp configs/gateway.example.toml "$PRIVATE_GATEWAY_CONFIG"
${EDITOR:-vi} "$PRIVATE_GATEWAY_CONFIG"

printf 'Gateway: %s\nConfig: %s\n' \
  "$PWD/target/debug/conversation-runtime-gateway" \
  "$PRIVATE_GATEWAY_CONFIG"

npm run desktop:dev
```

Before connecting, start the loopback model service configured in the private
file and replace `local-model-id` with one exact installed model identifier.
Enter the two printed absolute paths in the setup screen. The gateway rejects
remote endpoints and the app does not silently fall back to cloud execution.

After connecting:

1. Send a text turn and observe streamed assistant text.
2. Use `Stop` during an active turn, then send another turn.
3. Open `Preview Voice Focus` and select Soft Aurora, Silk, Threads, Prism,
   Orb, Still Gradient, and None.
4. Confirm the transcript starts hidden, reveal it explicitly, and leave Focus
   with `Escape`.
5. Close the runtime and reconnect through setup.

To use the optional Memory destination, first initialize a disposable or
operator-chosen database explicitly with `conversation-memory-probe`, then add
its absolute path under `[memory]` in the selected gateway configuration. The
desktop shows Memory only when gateway status reports enabled local memory and
the `memory_inspection` capability. Open the list and a detail to verify
read-only navigation and any truncation notice. History is a separate
app-owned transcript store and does not become runtime memory automatically.

## Focused Validation Evidence

Observed on 2026-08-06 with Node `v24.14.0`, npm `9.6.7`, and Rust `1.97.1`:

```text
$ cargo fmt --all -- --check
$ cargo clippy --workspace --all-targets --locked -- -D warnings
$ cargo test --workspace --locked --no-fail-fast
all workspace, integration, and doc-test targets passed

$ npm test --workspaces
@conversation/runtime: 58 passed
conversation-node-chat: 11 passed
conversation-desktop: 10 files and 108 passed

$ npm run build --workspaces
TypeScript, desktop type checks, Vite production build, and scene-chunk checks passed

$ node --input-type=module ...
compiled TypeScript client: status returned ["text", "memory_inspection"]
compiled gateway: listed and inspected the one disposable memory record

$ npm run desktop:dev
Vite served the local development app and started target/debug/conversation-desktop
The process was closed cleanly. The Mac was locked, so this run did not make a
human visual claim about the Memory list, detail, or truncation presentation.
```

## Open Work and Acceptance Boundaries

- **Live voice activation:** typed desktop voice-session events and production
  microphone capture, recognition, playback, and barge-in are not connected.
- **Persona and memory mutation:** the app does not inspect or mutate persona;
  runtime memory is inspectable only, with no create, edit, approval, pin,
  expiry, deletion, or retrieval control.
- **Distribution:** packaging, model-free bundle review, signing,
  notarization, installation, and upgrade flows are not validated.
- **Human visual review:** final scene appearance and GPU behavior in the Tauri
  window have not received a recorded human acceptance pass; the current native
  launch could not be visually inspected while the Mac was locked.
- **Interactive model run:** this evaluation does not record a live local-model
  text turn or make latency, usefulness, or model-quality claims.
- **R3 acceptance:** a post-fix human-spoken turn, ten-minute device run,
  first-audible measurement, audible-stop p95, and the 30-sample external
  acoustic procedure remain open. Desktop shell work does not satisfy them.

## Status

The first R6 desktop slice is testable and documented. R6 remains open for the
work above, and R3 remains `ACCEPTANCE BLOCKED` pending its separately defined
human and acoustic evidence.
