# R6 Desktop App Evaluation

**Date:** 2026-08-05
**Branch:** `feature/r6-desktop-app`
**Scope:** first macOS Tauri text-chat and Voice Focus visual slice

## Result

The first R6 desktop slice is developer-runnable and its bounded automated
checks pass. R6 is not complete: the production gateway still reports
text-only capabilities, and no live voice, persona/memory mutation, packaged
release, or R3 human/acoustic acceptance claim is made.

## Testable Surface

| Surface | Current evidence | Boundary |
| --- | --- | --- |
| Desktop launch | The root `desktop:dev` command starts Vite and the native Tauri binary. | The launch smoke was stopped after startup; it was not a human visual acceptance run. |
| Text chat | Setup, verified local-only status, send, streamed deltas, Stop, close, failure recovery, and reconnect are covered by desktop tests. | A person still needs a running configured loopback model service for an interactive turn. |
| Voice Focus shell | Preview entry, scene selection, hidden transcript default, explicit transcript reveal, `Escape`, reduced motion, and scene failure fallbacks are covered by tests and the production build. | Preview is intentionally idle and cannot imply microphone or playback activity. |
| Focus scenes | Soft Aurora, Silk, Threads, Prism, Orb, Still Gradient, and None are selectable; Soft Aurora is the default. | The five animated scenes have separate lazy chunks; final human GPU visual review remains open. |
| Local gateway bridge | Absolute-path validation, idempotent close, process reaping, and reopen ordering pass Rust tests. | The desktop validation in this report does not claim model quality, latency, or acoustic behavior. |

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

## Focused Validation Evidence

Observed on the branch above with Node `v24.9.0`, npm `11.6.0`, and Rust
`1.97.1`:

```text
$ npm test --workspace conversation-desktop
Test Files  8 passed (8)
Tests       82 passed (82)

$ cargo test -p conversation-desktop
gateway_bridge.rs: 5 passed; 0 failed
all package and doc-test targets passed

$ npm run build --workspace conversation-desktop
TypeScript app check passed
TypeScript config check passed
Vite 8.2.0 transformed 112 modules
SoftAurora, Silk, Threads, Prism, and Orb lazy-chunk assertion passed

$ npm run desktop:dev -- --help
Reached the pinned Tauri 2.11.0 `dev` command

$ npm run desktop:dev
Ran the configured Vite before-dev command
Vite served http://localhost:1420/
Started target/debug/conversation-desktop
Process was then stopped intentionally
```

## Open Work and Acceptance Boundaries

- **Live voice activation:** typed desktop voice-session events and production
  microphone capture, recognition, playback, and barge-in are not connected.
- **Persona and memory:** the app displays bounded status but does not inspect
  or mutate persona or memory through actual runtime controls.
- **Distribution:** packaging, model-free bundle review, signing,
  notarization, installation, and upgrade flows are not validated.
- **Human visual review:** final scene appearance and GPU behavior in the Tauri
  window have not received a recorded human acceptance pass.
- **Interactive model run:** this evaluation does not record a live local-model
  text turn or make latency, usefulness, or model-quality claims.
- **R3 acceptance:** a post-fix human-spoken turn, ten-minute device run,
  first-audible measurement, audible-stop p95, and the 30-sample external
  acoustic procedure remain open. Desktop shell work does not satisfy them.

## Status

The first R6 desktop slice is testable and documented. R6 remains open for the
work above, and R3 remains `ACCEPTANCE BLOCKED` pending its separately defined
human and acoustic evidence.
