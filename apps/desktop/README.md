# Desktop Reference App

This macOS Tauri reference app exercises the public browser-safe runtime SDK
against the compiled local gateway. It supports local text chat, explicit local
voice sessions, shared typed/spoken conversation state, app-owned transcript
history, optional runtime-memory inspection/approval/deletion, persona update,
and Voice Focus scenes.

The desktop is a reference application, not the SDK boundary. It owns UI state,
setup paths, Focus preferences, and transcript-history presentation. The public
SDK owns protocol validation and lifecycle handles; the Rust runtime owns turn
identifiers, context, persona/quality decisions, optional memory, and
cancellation; the gateway owns private configuration and child cleanup.

## Run from a Clean Checkout

From the repository root, install the exact lockfile dependencies and build the
browser-safe SDK plus gateway:

```bash
npm ci
npm run build --workspace @conversation/runtime
cargo build --locked -p conversation-runtime-gateway
```

Copy the example gateway configuration to a private absolute path, then edit
the loopback endpoint and generic model placeholder for a local service already
running on this Mac:

```bash
PRIVATE_GATEWAY_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/conversation-runtime/gateway.toml"
mkdir -p "$(dirname "$PRIVATE_GATEWAY_CONFIG")"
cp configs/gateway.example.toml "$PRIVATE_GATEWAY_CONFIG"
${EDITOR:-vi} "$PRIVATE_GATEWAY_CONFIG"
```

For text-only use, leave the optional `[voice.*]` blocks commented. For voice:

1. Build the managed macOS sidecar.
2. Set `[voice.asr].model_path` to an existing local ASR model directory.
3. Set `[voice.audio].sidecar_executable` to the printed absolute path.
4. Set the speech endpoint, model, and voice to an explicitly selected local
   service; public placeholders are not deployment defaults.
5. Uncomment the complete voice subtree together.

```bash
swift build -c release --package-path platform/macos/voice-sidecar
printf '%s/conversation-voice-sidecar\n' "$(swift build -c release \
  --package-path platform/macos/voice-sidecar --show-bin-path)"
```

Configuration loading validates voice paths and policy but does not access the
microphone, spawn the sidecar, or contact providers. The app requests capture
only after explicit `Start voice`. Local-only status is enforced and there is
no silent fallback to a remote provider.

Launch the native development app:

```bash
npm run desktop:dev
```

In setup, enter these two absolute paths:

```text
<repository>/target/debug/conversation-runtime-gateway
<value printed by: printf '%s\n' "$PRIVATE_GATEWAY_CONFIG">
```

For copyable values:

```bash
printf 'Gateway: %s\nConfig: %s\n' \
  "$PWD/target/debug/conversation-runtime-gateway" \
  "$PRIVATE_GATEWAY_CONFIG"
```

The gateway can connect before the configured loopback language service is
running, but the service must be available before the first text or spoken turn.

## Voice Behavior

- Entering `Voice Focus` never starts the microphone. Select `Start voice` to
  request permission and wait for the sidecar's capture acknowledgement.
- Typed and finalized spoken turns appear in one transcript and use one bounded
  runtime context. Partial ASR text is display-only and is not persisted.
- Focusing the composer pauses capture before typed send. Send remains disabled
  until pause is acknowledged. Capture resumes only after that typed turn is
  terminal, the draft is empty, and the same voice session still owns the pause.
- `Exit Focus` offers Stop, Keep, and Cancel. Keep returns to Conversation with
  a visible microphone status; Stop waits for voice cleanup; Cancel remains in
  Focus.
- Speaking during playback produces barge-in lifecycle state and cancels old
  generation, synthesis, queued audio, and active playback in the runtime.
- Recoverable voice failures leave typed chat usable and expose retry. Fatal
  gateway failures return to setup.
- App close uses bounded voice and gateway cleanup rather than silently leaving
  the sidecar running.

When voice is not configured, `Preview Voice Focus` remains available without
capture. Soft Aurora is the default; Silk, Threads, Prism, Orb, Still Gradient,
and None are also selectable. Transcript visibility is hidden by default and
can be remembered explicitly. Reduced-motion uses static fallbacks.

## History and Memory

Conversation transcripts are stored by the native app in
`conversations.sqlite3` under the operating system's private app-data directory.
The exact resolved path is shown at the bottom of `History`. History is separate
from runtime semantic memory, and opening a saved transcript is read-only: it
does not restore a provider session or change the model's active context.

Sessions can be revision-safely deleted from either the list or detail view.
**Continue as new conversation** checks the saved source revision, leaves that
source immutable, and creates a separately persisted branch. It copies only the
latest whole completed nonblank exchanges that fit all three limits: 16 pairs,
32,768 UTF-8 content bytes, and 16 KiB for each individual user or assistant
message. It never truncates, summarizes, compresses, or skips across an
oversized gap. The copied context is labelled separately from new live turns;
future turns use the currently connected model, current response persona, and
currently active and eligible memory policy.

The branch records source provenance, copied-context versus live-turn origin,
and revision-checked `preparing`, `confirmed`, or `unconfirmed` recovery state.
The runtime exposes only the last opaque context-seed operation ID for
reconciliation. Deleting the source later does not delete or mutate its copied
branch context.

Runtime memory is opt-in. Initialize a chosen SQLite database explicitly with
`conversation-memory-probe`, then configure its absolute path in the gateway.
The desktop neither creates that database nor automatically captures
conversations into it. `Memory` appears only when the gateway reports enabled
local memory and `memory_inspection`. The desktop reads summaries
and records through the public SDK, approves candidate memories, and deletes
records with their expected revision; it has no runtime SQLite access. Settings
loads and updates the runtime persona through the same public SDK surface.
Typed SDK tests cover those commands separately from the compiled
SDK-to-gateway smoke; compiled public-SDK persona and memory mutation coverage
is present.

## Protocol Compatibility

The current gateway speaks protocol v2. The updated `@conversation/runtime`
client reads the Ready version once and uses the matching strict codec: its
existing status, text, voice, persona, and memory operations continue to work
with a v1 server, while context seeding is locally unavailable unless v2 and
`conversation_context_seed` are advertised. An old v1-only client binary cannot
connect to the v2 gateway.

## Developer Checks

```bash
npm test --workspace conversation-desktop
cargo test -p conversation-desktop
npm run build --workspace conversation-desktop
```

For hardware-dependent observations, use
[the native macOS checklist](../../docs/r6-desktop-voice-session-native-check.md).
Automated gates do not prove microphone selection, audible playback, subjective
voice quality, first-audible latency, audible-stop latency, or ten-minute device
continuity.

## Open Work

- Additional runtime-memory management beyond candidate approval and deletion.
- Model setup and benchmark UI.
- Packaging, signing, notarization, installation, and upgrade validation.
- Native data-mutating Session deletion/continuation, typed and spoken
  follow-up, branch reopening, source-deletion survival, restart reconciliation,
  and v1-unavailable disclosure. A fresh debug bundle has received a
  non-destructive inspection of the controls, confirmation/cancel focus, and
  continuation preview.
- Native microphone, playback, GPU-scene, ten-minute, first-audible,
  audible-stop, and 30-sample acoustic acceptance.

See [the desktop evaluation](../../docs/r6-desktop-app-evaluation.md) for current
automated evidence and remaining R6/R3 boundaries.
