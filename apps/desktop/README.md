# Desktop Reference App

This macOS Tauri reference app exercises the public browser-safe runtime SDK
against the compiled local gateway. The desktop app supports local text chat,
locally persisted transcript history, optional read-only runtime-memory
inspection, and an idle Voice Focus preview. It does not activate a microphone
or play audio.

## Run from a Clean Checkout

From the repository root, install the exact lockfile dependencies and build the
browser-safe SDK plus gateway:

```bash
npm ci
npm run build --workspace @conversation/runtime
cargo build --locked -p conversation-runtime-gateway
```

Copy the example gateway configuration to a private absolute path, then edit
the loopback endpoint and model placeholder for a local service already running
on this Mac:

```bash
PRIVATE_GATEWAY_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/conversation-runtime/gateway.toml"
mkdir -p "$(dirname "$PRIVATE_GATEWAY_CONFIG")"
cp configs/gateway.example.toml "$PRIVATE_GATEWAY_CONFIG"
${EDITOR:-vi} "$PRIVATE_GATEWAY_CONFIG"
```

Launch the native development app with the documented root command:

```bash
npm run desktop:dev
```

In setup, enter these two absolute paths:

```text
<repository>/target/debug/conversation-runtime-gateway
<value printed by: printf '%s\n' "$PRIVATE_GATEWAY_CONFIG">
```

For copyable values, run this from the same shell:

```bash
printf 'Gateway: %s\nConfig: %s\n' \
  "$PWD/target/debug/conversation-runtime-gateway" \
  "$PRIVATE_GATEWAY_CONFIG"
```

The app requires the configured loopback model service to be running before
`Connect local runtime` can succeed. It never falls back to a remote service.

## Developer Checks

- Send local text turns, observe streamed assistant text, stop an active turn,
  close the runtime, and reconnect through setup.
- Open `History`, reopen a prior transcript read-only, verify the displayed
  SQLite storage path, and delete a saved conversation.
- When the connected local gateway explicitly advertises memory inspection,
  open `Memory`, page through at most 50 summaries at a time, and open a
  record's read-only detail. The detail shows at most the latest 32 provenance
  entries and 32 approval entries, with an explicit notice when older entries
  are truncated.
- Open `Preview Voice Focus` and switch among Soft Aurora, Silk, Threads,
  Prism, Orb, Still Gradient, and None. Soft Aurora is the default.
- Verify the Focus transcript is hidden by default, reveal it explicitly, and
  leave Focus with `Escape` or `Exit Focus`.
- Verify reduced-motion selects the static fallback for animated scenes.

`Preview Voice Focus` is intentionally idle. The production gateway does not
advertise voice capabilities, so live Focus cannot imply listening,
recognition, speech playback, or barge-in.

Conversation transcripts are stored by the native app in
`conversations.sqlite3` under the operating system's private app-data
directory. The exact resolved path is shown at the bottom of `History`.
Transcript history is separate from the runtime's optional semantic memory,
and opening a past transcript does not restore it to the model's active
context.

## Optional Runtime Memory

Runtime memory is opt-in. Initialize a chosen SQLite database explicitly with
`conversation-memory-probe`, then configure its absolute path in the gateway;
the desktop neither creates the database nor captures conversations into it.
History remains app-owned transcript storage, while runtime memory remains
runtime-owned semantic storage with its own provenance, approval, retention,
and retrieval rules.

The Memory destination appears only when the connected local gateway reports
enabled local memory and the protocol-v3 `memory_inspection` capability. The
desktop reads summaries and individual records only through the public
browser-safe SDK and local framed-stdio protocol. It has no SQLite access and
offers no create, edit, approval, pin, expiry, deletion, or retrieval control.
Inspection can apply due expiry before returning a record, so a due item may no
longer be active when displayed.

## Open Work

- Live microphone and playback activation through typed voice-session events.
- Persona inspection and mutation, plus runtime-memory mutation controls.
- Packaging, signing, notarization, installation, and upgrade validation.
- R3 human-spoken, ten-minute, first-audible, audible-stop, and acoustic
  acceptance.

## Focused Validation

```bash
npm test --workspace conversation-desktop
cargo test -p conversation-desktop
npm run build --workspace conversation-desktop
npm run desktop:dev -- --help
```

See [the desktop evaluation](../../docs/r6-desktop-app-evaluation.md) for the
validated evidence and remaining R6 and R3 gates.
