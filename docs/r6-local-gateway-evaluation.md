# R6 Local Gateway Evaluation

## Status

The first R6 local-gateway slice is complete for deterministic text
interoperability. It includes the persistent Rust gateway, versioned bounded
framed stdio, the public TypeScript client, and the minimal Node chat example.

R6 is not complete overall. Tauri and React, microphone and playback controls,
persona and memory mutation controls, model setup and benchmark UI, packaging,
signing, and installation remain open. R3 human-spoken, ten-minute device, and
external acoustic acceptance also remain open and are not affected by this
text-only evidence.

## Deterministic Verification

From the repository root, install the pinned Node dependencies and run the
workspace gates:

```bash
npm ci
cargo build --locked -p conversation-runtime-gateway
npm run build --workspaces
npm test --workspaces
```

The Node chat suite covers:

- absolute gateway and configuration argument validation;
- visible local-only status before the first prompt;
- two prompts through one persistent `RuntimeClient` and gateway transport;
- streamed UTF-8 `text_delta` values and terminal states on separate lines;
- first active `SIGINT` interruption, second or idle `SIGINT` shutdown, and EOF
  cleanup;
- nonzero failure status with content-free diagnostics; and
- omission of transcript and provider data from diagnostics.

The process smoke builds and spawns the actual
`conversation-runtime-gateway` binary. A temporary deterministic
Ollama-compatible server binds only to loopback, and a temporary absolute
configuration selects that endpoint. The completion run observes this order:

```text
ready -> start accepted -> UTF-8 text delta -> completed
```

A separate gateway process and provider connection prove cancellation:

```text
ready -> start accepted -> provider request active -> interrupt accepted -> cancelled
```

Both runs use the Node `StdioGatewayTransport` and real length-prefixed pipes.
The cross-language smoke has no in-memory transport substitution. The loopback
test provider is a fixture; it is not a gateway listener or public deployment
default.

These checks prove framing, protocol validation, command correlation, streamed
text interoperability, terminal completion, interruption, connection cleanup,
and process reuse. They do not measure first-token latency, response quality,
model suitability, audible output, or acoustic interruption.

## Manual Local Smoke Template

Keep deployment choices in a private configuration outside the repository:

```bash
npm ci
cargo build --locked -p conversation-runtime-gateway
npm run build --workspaces

PRIVATE_GATEWAY_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/conversation-runtime/gateway.toml"
mkdir -p "$(dirname "$PRIVATE_GATEWAY_CONFIG")"
cp configs/gateway.example.toml "$PRIVATE_GATEWAY_CONFIG"
```

Edit the private copy to select one installed local model and one loopback-only
Ollama-compatible endpoint. Do not commit the file. Then start one persistent
chat process:

```bash
npm run chat --workspace conversation-node-chat -- \
  --gateway "$PWD/target/debug/conversation-runtime-gateway" \
  --config "$PRIVATE_GATEWAY_CONFIG"
```

Use this content-free observation template:

```text
toolchain versions recorded: yes/no
privacy status reports local-only: yes/no
first turn terminal: completed/cancelled/failed
second turn terminal: completed/cancelled/failed
active-turn SIGINT terminal: cancelled/failed
gateway closes after idle SIGINT or EOF: yes/no
unexpected diagnostic content observed: yes/no
```

Do not record prompts, generated text, model identifiers, private paths, memory
contents, or provider payloads in this document. Record latency or model-quality
claims only in a separate controlled evaluation with an explicit measurement
method and reproducible deployment configuration.

## Boundaries

- The gateway accepts only `local-only` configuration and a loopback HTTP
  provider endpoint. It performs no cloud or remote fallback.
- The gateway opens no TCP, HTTP, WebSocket, Unix-domain, Bonjour, or LAN
  listener. All client traffic uses child-process stdin and stdout.
- Optional memory opens one explicitly configured existing local SQLite store;
  it is never created or silently substituted by the gateway.
- Public examples remain backend-neutral and contain no deployment model,
  private path, prompt, response, persona, or memory content.
- The completed slice is a client/runtime interoperability boundary, not a
  desktop product, deployment package, model recommendation, or R3 acceptance
  result.
