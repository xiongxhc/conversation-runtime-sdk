# Task 7: Node Chat Example and Cross-Language Smoke Report

## Status

Complete for the approved first R6 local-gateway slice. The Node chat example,
real Rust-binary-to-Node framed-pipe smokes, public documentation, and workspace
metadata are implemented and verified. No Rust source or core SDK API behavior
was changed.

## Scope

- Added the `conversation-node-chat` npm workspace with strict ESM TypeScript
  build, chat, and test scripts.
- Added one persistent terminal chat client that consumes only
  `@conversation/runtime` public exports.
- Added deterministic CLI tests plus real compiled-gateway completion and
  cancellation smokes.
- Updated the root workspace and lockfile, README, roadmap, and architecture.
- Added `docs/r6-local-gateway-evaluation.md` with deterministic commands,
  evidence limits, and a content-free manual smoke template.

One adjacent Task 6 packaging blocker required a minimal metadata correction:
`@conversation/runtime` advertised `dist/index.*`, but its existing TypeScript
build emits `dist/src/index.*`. The package export and `types` paths now point to
the files actually produced. The public symbols and SDK implementation are
unchanged.

## RED Evidence

The initial required command was run before `examples/node-chat/src/main.ts`
existed:

```text
npm test --workspace conversation-node-chat
```

It failed at TypeScript compilation because the CLI module was missing. The
first run also exposed the pre-existing public-package path mismatch. After the
metadata-only correction, the same command failed solely with:

```text
error TS2307: Cannot find module '../src/main.js'
```

After the minimal CLI implementation, the six-test behavior suite reported five
passing and one failing test. The second-active-`SIGINT` regression returned
exit code `1` instead of `0` because deliberate transport closure rejected the
pending interrupt promise and reclassified expected shutdown as failure. The
callback now reports interruption failure only when shutdown was not already
requested; the focused regression and full CLI suite then passed.

## Implementation

- Requires one absolute `--gateway` path and one absolute `--config` path;
  invalid arguments return a usage failure without spawning a process.
- Starts one `StdioGatewayTransport`, connects one `RuntimeClient`, and reuses
  both across prompts.
- Requests status before the first prompt and prints only content-free
  local-only component state; the model identifier is not displayed.
- Writes only `text_delta` lifecycle payloads as assistant content and prints
  completed, cancelled, or failed terminal state on its own line.
- Maps the first active `SIGINT` to exactly one interrupt command. A second
  active signal, an idle signal, or EOF closes and reaps the gateway.
- Returns nonzero on gateway or turn failure and never forwards raw errors,
  transcripts, provider output, model identifiers, paths, or stderr content to
  diagnostics.

## Cross-Language Smoke

The example test script builds `conversation-runtime-gateway` with the pinned
locked Rust workspace before running Node tests. Each process smoke creates a
temporary absolute configuration and a deterministic loopback-only
Ollama-compatible server.

The completion run uses the public `StdioGatewayTransport` against the actual
compiled Rust binary and observes ready, start acceptance, a non-empty UTF-8
text delta, and completion in order. The independent cancellation run starts a
new provider and gateway process, waits for an active provider request, sends an
interrupt frame, observes interrupt acceptance and cancellation without
completion, and verifies that the provider connection closes. Neither smoke
uses an in-memory transport.

## Documentation

- `README.md` documents build and chat commands, visible privacy status,
  persistent reuse, signal behavior, and the no-listener boundary.
- `docs/architecture.md` shows Node or later Tauri host to framed stdio gateway
  to text runtime to local adapter, with the gateway listener prohibition
  explicit.
- `ROADMAP.md` marks only the first R6 local-gateway slice complete. Tauri and
  React, persona and memory mutation controls, model setup, packaging, signing,
  installation, and later LAN work remain open.
- `docs/r6-local-gateway-evaluation.md` separates deterministic interoperability
  from latency, model-quality, product, and R3 human/device/acoustic evidence.

## Verification

Fresh final commands:

```text
npm ci
  PASS: installed the five pinned workspace packages

npm run build --workspaces
  PASS: @conversation/runtime and conversation-node-chat

npm test --workspaces
  PASS: 40 @conversation/runtime tests
  PASS: 8 conversation-node-chat tests
  PASS: actual Rust gateway completion smoke
  PASS: independent actual Rust gateway cancellation smoke

npm pack --workspace @conversation/runtime --dry-run --json
  PASS: dist/src/index.js and dist/src/index.d.ts are packaged

git diff --check
  PASS
```

The loopback process tests require an environment that permits binding a
temporary `127.0.0.1` test server. The restricted sandbox returned `EPERM`; the
same suite passed when granted loopback permission.

## Commit

The commit containing this report uses:

```text
feat: add local gateway chat example
```

Author and committer are `Chris Xiong <xionghc713@gmail.com>`. No co-author
trailer is present.

## Concerns

- The manual private local-provider smoke is intentionally left to Task 8; no
  deployment model, private path, prompt, response, or quality result was added
  to public documentation.
- R6 remains open beyond this slice, and R3 human-spoken, ten-minute device, and
  external acoustic acceptance remain open.
- The TypeScript package metadata correction should remain covered by a future
  package-artifact verification gate so its declared public entrypoint cannot
  drift from emitted files again.

## Review Fix: Active EOF and Prompt-Boundary SIGINT

### Findings

1. EOF was observed only by the readline iterator between turns. While
   `renderTurn` awaited a stalled provider, active-turn EOF could not reach the
   close path and the client and gateway remained open.
2. The CLI wrote `assistant> ` before calling `startTurn` and assigning the
   active-turn state. A `SIGINT` synchronized to that visible prompt therefore
   took the idle shutdown path instead of sending exactly one interruption.

### RED Evidence

The new focused test command was run before changing `src/main.ts`:

```text
npm run build --workspace conversation-node-chat
node --test \
  --test-name-pattern='visible assistant prompt|stalled active turn on EOF' \
  examples/node-chat/dist/test/cli.test.js
```

Both regressions failed for the reviewed reasons:

```text
assistant prompt SIGINT did not interrupt the active turn
active-turn EOF did not close the client
```

The prompt regression emits `SIGINT` synchronously from the output sink when
the exact `assistant> ` write becomes visible. The EOF regression starts a
stalled accepted turn, ends stdin, and requires bounded completion without an
interrupt command, duplicate close, or diagnostic output.

### Fix

- `runChat` now calls `startTurn` and assigns `active` before writing the
  assistant prompt. Once the prompt is visible, the first `SIGINT` therefore
  always sees the active turn and sends one interrupt command.
- One input `end` observer is installed while the client is running. EOF during
  an active turn calls the same idempotent `stop` function used by second or
  idle `SIGINT`, closes the client and gateway once, and does not synthesize an
  interrupt command.
- The input observer is removed during final cleanup. Expected rejection of the
  active event iterator after client closure remains handled by the existing
  `stopping` path, so no raw error becomes a diagnostic and no losing promise is
  left unhandled.

### Real Process Coverage

A new smoke starts the actual compiled `conversation-runtime-gateway` through
`runChat`, waits for the visible input prompt, starts a turn against the
loopback-only provider fixture, waits until the provider request is active, and
then ends stdin. The client returns success within the bounded deadline, the
gateway closes, and the stalled provider connection is reaped without
diagnostic content.

### Final GREEN Evidence

```text
npm test --workspace conversation-node-chat
  PASS: 11 tests

npm run build --workspaces
  PASS: @conversation/runtime and conversation-node-chat

npm test --workspaces
  PASS: 40 @conversation/runtime tests
  PASS: 11 conversation-node-chat tests
  PASS: compiled-gateway completion, cancellation, and active-EOF smokes
```

The focused signal suite also reruns first-active-`SIGINT`, second-active
`SIGINT`, idle `SIGINT`, and idle EOF behavior. No Rust, SDK, documentation, or
unrelated file changed in this review fix.

### Fix Commit

The commit containing this review fix uses:

```text
fix: harden Node chat shutdown races
```

Author and committer are `Chris Xiong <xionghc713@gmail.com>`. No co-author
trailer is present.
