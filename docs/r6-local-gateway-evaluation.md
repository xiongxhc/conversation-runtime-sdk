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

Final verification used the pinned repository toolchains:

```text
rustc 1.97.1 (8bab26f4f 2026-07-14)
cargo 1.97.1 (c980f4866 2026-06-30)
Node.js v18.17.1
npm 9.6.7
```

From the repository root, the complete final gates were:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
npm ci
npm run build --workspaces
npm test --workspaces
git diff --check master...HEAD
```

All final commands exited zero. The Rust workspace run included every target
and doc-test under the default parallel test harness. Loopback fixture tests
were run in an environment that permits temporary numeric-loopback listeners.
The TypeScript build completed for both workspaces. The Node results were:

```text
@conversation/runtime: 40 passed, 0 failed
conversation-node-chat: 11 passed, 0 failed
```

The Node chat suite builds and spawns the real Rust gateway for completion,
cancellation, and active-EOF process smokes.

### Verification Finding and Fix

The initially reported parallel failure in
`crates/model-adapters/tests/ollama.rs` was not an unchanged `master` target.
The file differs from `master`, and R6 commit `14ed03b` added a proxy-bypass
test that temporarily changed process-global proxy environment variables while
its sibling tests ran in parallel.

The focused default-parallel target reproduced the defect with 1 pass and 26
failures. The serial target passed all 27 tests. The smallest test-only fix
moved the proxy environment into a dedicated child test process, retaining the
real proxy-bypass assertion without changing production code. The focused
default-parallel target then passed all 28 tests, and the final default-parallel
Rust workspace gate passed.

One intermediate full run also exposed an unchanged R3 voice pressure test
exceeding its fixed deadline. That file is byte-identical to `master`; the
single test passed alone and the final exact workspace gate passed without any
R3 source change. This observation does not close R3 device or acoustic
acceptance.

### Lifecycle and Privacy Review

- Gateway command intake selects independently over framed input, runtime
  forwarding, and the writer task. Bounded writer lanes use nonblocking sends,
  and deterministic coverage proves interruption cancels and reaps generation
  while output remains blocked.
- Start acceptance is queued before event forwarding begins. Runtime terminal
  delivery is separate from nonterminal backpressure, and completion,
  cancellation, failure, duplicate-terminal rejection, and runtime reuse are
  covered across Rust and TypeScript tests.
- Interruption acceptance is not treated as completion. The authoritative
  terminal remains `turn_cancelled`, and cancelled partial output is excluded
  from completed history.
- EOF, writer failure, framing failure, and client close cancel active work,
  drain or stop forwarding, await owned tasks, and boundedly reap the gateway
  process.
- Production gateway and client source contains no listener. Numeric loopback
  listeners occur only in deterministic provider fixtures; application traffic
  remains framed child-process stdin and stdout.
- Configuration is absolute, bounded, strict-schema, local-only, and limited to
  plain HTTP on a numeric loopback address. Credentials, queries, fragments,
  hostnames, non-loopback addresses, redirects, and implicit model defaults are
  rejected.
- Configured memory opens one absolute existing initialized SQLite store. A
  missing, invalid, remote, or failing memory path fails closed before language
  generation; memory is never silently disabled or substituted.
- Gateway stderr and TypeScript transport failures use static content-free
  categories. Tests exclude transcripts, generated values, provider data,
  model identifiers, and private paths from diagnostics. Telemetry remains off.

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

## Private Local Smoke Result

A separate operator-run smoke used one private absolute configuration outside
the repository. Only content-free observations are retained here:

```text
privacy mode: local-only
language location: local
memory: disabled
telemetry: off
first turn: streamed, completed
second turn: streamed, completed
active-turn direct SIGINT: turn_cancelled
post-cancellation state: ready
idle SIGINT exit: zero
numeric latency timings: not recorded
```

The timing evidence category is event order and terminal outcome only. It does
not support a latency, quality, model-suitability, audible-output, or acoustic
claim. No model identifier, prompt, generated content, provider payload,
private path, or memory content is recorded.

## Reproduction Template

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
