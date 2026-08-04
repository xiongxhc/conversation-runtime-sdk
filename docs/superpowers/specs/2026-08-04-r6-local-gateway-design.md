# R6 Local Gateway and TypeScript Client Design

**Status:** Approved first slice of R6

## Problem

The repository has verified runtime, local-model, conversation-quality, voice,
and controlled-memory components, but a product client still has to understand
Rust implementation details or invoke purpose-built probes. R6 needs a stable
client boundary that proves a second process can run and interrupt a real local
conversation without coupling the SDK to Tauri, React, Ollama, or desktop UI
code.

## Scope

This slice delivers a text-first, local-only gateway and TypeScript SDK. It is
the prerequisite for the desktop UI, not the desktop UI itself.

Included:

- a persistent Rust gateway process;
- a versioned client command and event schema;
- bounded framed JSON over stdin and stdout;
- local Ollama-compatible language generation through existing adapter
  contracts;
- in-process conversation quality and completed-turn history;
- optional retrieval from one explicitly configured existing local SQLite
  memory store;
- streamed text events, typed failures, cancellation, and privacy status;
- the `@conversation/runtime` TypeScript package;
- a minimal Node chat client that proves a non-desktop client can complete and
  interrupt turns.

Deferred to later R6 slices:

- Tauri and React UI;
- microphone capture, speech playback, and voice-sidecar control through the
  gateway;
- persona editing and memory CRUD commands;
- packaging, signing, model installation, and benchmark UI;
- local sockets, TCP listeners, Bonjour, pairing, TLS, or LAN access.

## Approaches Considered

### 1. Bounded framed stdio

The client spawns one gateway process and exchanges length-prefixed JSON frames
over its standard streams. This is local-only by construction, requires no
listener authentication, maps naturally to a Tauri sidecar and Node child
process, and preserves transport-neutral command/event envelopes for R7.

### 2. Unix domain socket

A Unix socket would allow multiple clients to share one process, but introduces
socket ownership, stale-path cleanup, peer-credential checks, and connection
arbitration before R6 needs them. It is also not the eventual iPhone transport.

### 3. Loopback HTTP or WebSocket

Loopback networking resembles a future LAN gateway, but creates an exposed
listener, origin and authentication questions, port lifecycle, and accidental
non-loopback binding risk. R7 must add explicit pairing and TLS rather than
normalizing an unauthenticated listener in R6.

**Decision:** Use bounded framed stdio for R6. The wire envelopes remain
transport-neutral so a separately authenticated R7 transport can reuse them.

## Repository Structure

```text
crates/
  protocol/
    src/client_wire.rs          # versioned client commands and events
    tests/client_wire.rs        # JSON identity and validation fixtures
  model-adapters/
    src/ollama.rs               # generation-identity adapter implementation
    tests/ollama.rs             # cancellation and identity coverage
  runtime/
    src/text_turn.rs            # text-only lifecycle, quality, and memory
    tests/text_turn.rs          # completion, cancellation, history, failures
apps/
  runtime-gateway/
    Cargo.toml
    src/config.rs               # bounded, explicit local-only configuration
    src/framing.rs              # bounded 4-byte big-endian JSON frames
    src/session.rs              # command routing and event forwarding
    src/main.rs                 # stdio process entry point
    tests/gateway_cli.rs        # process-level protocol tests
packages/
  typescript/
    package.json                # package name @conversation/runtime
    tsconfig.json
    src/protocol.ts             # wire types and validators
    src/framing.ts              # Node stream frame codec
    src/client.ts               # process client and turn API
    src/index.ts                # public exports
    test/*.test.ts              # framing, routing, cancellation, fixtures
examples/
  node-chat/
    package.json
    src/main.ts                 # minimal persistent terminal chat client
configs/
  gateway.example.toml          # generic local-only example
tests/fixtures/client-wire-v1/  # shared valid and invalid frames
```

## Dependency Direction

The existing durable direction remains:

```text
protocol <- model-adapters <- runtime
```

The application layer composes those crates:

```text
protocol <- runtime-gateway -> runtime + model-adapters + memory
protocol <- @conversation/runtime <- node-chat
```

Neither the public wire schema nor the TypeScript SDK contains Ollama request
or response types. The gateway is a reference application package, not a new
core runtime dependency.

## Text Runtime

Add a dedicated `TextTurnRuntime` rather than using fake speech or silent audio.
It owns one active text turn and uses the existing `GenerationLanguageModel`,
`ConversationQualityController`, and optional `MemoryContextProvider`.

For an accepted turn it:

1. validates strictly increasing turn and generation identifiers;
2. resolves persona, mode, response controls, and completed-turn history;
3. retrieves bounded memory after quality resolution when configured;
4. creates the typed language-model input;
5. streams `TurnStarted`, `QualityResolved`, optional `MemoryRetrieved`,
   `TextDelta`, first-text timing, and exactly one terminal event;
6. adds user and assistant messages to history only after successful completion;
7. discards failed turns and marks interrupted turns without adding partial
   assistant output to history.

It emits no speech, playback, synthesis, or first-playable-audio events. The
existing voice runtime remains unchanged.

The Ollama-compatible adapter implements `GenerationLanguageModel` directly by
mapping its existing bounded text stream into identity-tagged deltas. Vendor
types remain inside `model-adapters`.

## Wire Protocol

`CLIENT_PROTOCOL_VERSION` is `1`. Every frame is:

```text
4-byte unsigned big-endian payload length | UTF-8 JSON payload
```

The maximum payload is 512 KiB so one bounded 64 KiB text value remains valid
after worst-case JSON escaping. Individual text and metadata fields retain
their stricter protocol limits. The reader rejects an oversized length before
allocating the payload. Standard output contains frames only; diagnostics use
standard error and exclude transcripts, prompts, generated text, and memory
content.

JSON uses top-level `protocol_version` and `type` fields. Every message carries
`protocol_version = 1`; unsupported versions are rejected before command
dispatch. Numeric SDK identifiers are encoded as decimal strings so JavaScript
never loses `u64` precision.

Client commands:

- `status` with a bounded client-generated `request_id`;
- `start_turn` with `request_id`, `turn_id`, and `transcript`;
- `interrupt_turn` with `request_id` and `turn_id`.

Gateway messages:

- one unsolicited `ready` message after complete configuration and adapter
  initialization;
- `command_accepted` or `command_rejected` for every decoded command;
- `status` with protocol version, gateway transport, privacy mode, configured
  component locations, model identifier, memory enabled state, and capabilities;
- `runtime_event` carrying a transport-safe projection of public runtime events;
- one `fatal` message when framing or process state cannot safely continue.

The gateway acknowledges `start_turn` before forwarding that turn's events.
Unknown command fields and unsupported protocol versions are rejected. A valid
but rejected command does not terminate the process. Invalid framing, invalid
UTF-8, or an unrecoverable stdout writer failure terminates it.

Runtime errors preserve bounded `kind`, `stage`, and `message` fields. Quality
and memory events expose existing content-free decisions and traces, not hidden
prompts or retrieved memory content.

## Concurrency and Cancellation

The stdin command reader remains independent from runtime-event forwarding, so
an `interrupt_turn` command can be processed while a turn is generating or the
client temporarily stops draining events. One bounded writer task serializes all
stdout frames. Runtime event forwarding may apply bounded backpressure, but it
cannot hold the command reader's state lock while awaiting output.

The gateway accepts one active turn. A second start is rejected with typed
invalid-state data. Interrupt acknowledgement means cancellation was accepted;
the corresponding terminal runtime event remains the authoritative completion
signal. Gateway shutdown cancels and reaps owned runtime work before exit.

## Configuration and Privacy

The gateway requires `--config <absolute-path>`. The TOML file is bounded to
64 KiB, uses `schema_version = 1`, and rejects unknown fields.

R6 supports only `privacy_mode = "local-only"`. The language endpoint must use
plain HTTP on a numeric loopback address, redirects remain disabled by the
adapter, and the model identifier is explicit. There is no default model and no
silent endpoint or provider fallback.

Memory is disabled unless the configuration declares one absolute SQLite path.
When enabled, the database must already exist and pass schema validation; the
gateway never discovers, creates, or substitutes a memory store. Memory and
language execution are both reported as local. Telemetry is disabled.

Public examples use generic model identifiers and contain no private paths,
credentials, model weights, or deployment preferences.

## TypeScript SDK

`@conversation/runtime` separates the protocol client from transport. The first
slice includes a Node child-process transport, while a later Tauri host can
bridge the same validated messages without giving browser JavaScript process
access. Its public API exposes:

```ts
interface RuntimeTransport {
  readonly messages: AsyncIterable<unknown>;
  send(message: unknown): Promise<void>;
  close(): Promise<void>;
}

type StdioGatewayOptions = {
  gatewayPath: string;
  configPath: string;
};

class RuntimeClient {
  static connect(transport: RuntimeTransport): Promise<RuntimeClient>;
  status(): Promise<RuntimeStatus>;
  startTurn(transcript: string): RuntimeTurn;
  interrupt(turnId: bigint): Promise<void>;
  close(): Promise<void>;
}

class StdioGatewayTransport implements RuntimeTransport {
  static start(options: StdioGatewayOptions): Promise<StdioGatewayTransport>;
}

type RuntimeTurn = {
  turnId: bigint;
  events: AsyncIterable<RuntimeEvent>;
};
```

The SDK assigns strictly increasing `bigint` turn identifiers per client and
validates all inbound messages at runtime. The stdio transport requires an
absolute gateway path, spawns with `shell: false`, continuously drains bounded
stderr, rejects pending work if the child exits, and never writes arbitrary
data to gateway stdin. The package has no model-provider dependency.

The Node chat example keeps one gateway process alive for multiple turns, prints
text deltas as they arrive, and maps `SIGINT` during generation to interruption.
A second `SIGINT` or end-of-input closes the gateway cleanly.

## Testing

All production behavior is developed test-first.

Deterministic Rust coverage includes:

- wire-schema valid, unknown-field, invalid-version, identifier, and size cases;
- frame fragmentation, multiple frames per read, oversized headers, truncated
  frames, invalid UTF-8, and bounded writer behavior;
- text completion, cancellation, exactly-one terminal event, failed-generation
  history exclusion, bounded event backpressure, runtime reuse, quality context,
  memory retrieval, memory failure, and panic containment;
- loopback-only configuration, missing or uninitialized memory stores, unknown
  fields, and content-free diagnostics;
- process tests using deterministic fake generation, not a live Ollama service.

TypeScript coverage includes:

- shared wire fixtures and `u64`/`bigint` identity;
- fragmented frame parsing and oversize rejection;
- request routing, streamed events, interruption, child exit, malformed frames,
  and cleanup;
- a process smoke against the deterministic Rust gateway mode.

The final verification gate is workspace Rust tests, strict Clippy, Rust format,
TypeScript build and tests, process smoke, and a manual local Ollama turn using
an explicit private configuration. The live Ollama smoke proves interoperability
only; it does not select a public default model or close R3 acoustic acceptance.

## Acceptance Criteria

- A user can start the gateway with one explicit local-only configuration and
  receive a truthful `ready` status before sending commands.
- The Node example completes two consecutive local-model turns in one process
  and streams text before terminal completion.
- The second completed turn receives completed history from the first.
- The client can interrupt an active turn and receives exactly one
  `turn_cancelled` terminal event after accepted interruption.
- Optional existing SQLite memory contributes bounded context and a content-free
  retrieval trace; a missing or invalid configured store prevents startup.
- The Rust gateway and Node client pass the same versioned wire fixtures.
- No TCP or Unix listener opens, no cloud fallback occurs, and no model weights
  or private configuration enter the package.
- The TypeScript client depends only on the public wire contract and can run a
  turn without importing desktop application code.
