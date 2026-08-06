import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { access, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer, type ServerResponse } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { PassThrough, Writable } from "node:stream";
import test from "node:test";
import { setImmediate } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import {
  parseGatewayMessage,
  StdioGatewayTransport,
  ClientCommand,
  type GatewayMessage,
  RuntimeStatus,
  type RuntimeTransport,
} from "@conversation/runtime";

import {
  runChat,
  type ChatDependencies,
  type ChatIo,
} from "../src/main.js";

const validArguments = [
  "--gateway",
  "/tmp/conversation-runtime-gateway",
  "--config",
  "/tmp/gateway.toml",
];

test("rejects missing, relative, duplicate, and unknown arguments without starting a gateway", async () => {
  for (const arguments_ of [
    [],
    ["--gateway", "/tmp/gateway"],
    ["--gateway", "./gateway", "--config", "/tmp/gateway.toml"],
    ["--gateway", "/tmp/gateway", "--config", "./gateway.toml"],
    [...validArguments, "--config", "/tmp/other.toml"],
    [...validArguments, "--unknown", "value"],
  ]) {
    const fixture = chatFixture(new ScriptedTransport("complete"));
    const exitCode = await runChat(arguments_, fixture.io, fixture.dependencies);

    assert.equal(exitCode, 2);
    assert.equal(fixture.startCount(), 0);
    assert.equal(fixture.output.text, "");
    assert.equal(fixture.diagnostics.text, "usage: conversation-node-chat --gateway <absolute-path> --config <absolute-path>\n");
  }
});

test("streams UTF-8 deltas for two prompts through one persistent client", async () => {
  const transport = new ScriptedTransport("complete");
  const fixture = chatFixture(transport);
  fixture.input.end("first prompt\nsecond prompt\n");

  const exitCode = await runChat(validArguments, fixture.io, fixture.dependencies);

  assert.equal(exitCode, 0);
  assert.equal(fixture.startCount(), 1);
  assert.equal(transport.closeCount, 1);
  assert.deepEqual(
    transport.sent
      .filter((command): command is Extract<ClientCommand, { type: "start_turn" }> => command.type === "start_turn")
      .map((command) => command.transcript),
    ["first prompt", "second prompt"],
  );
  assert.ok(fixture.output.text.indexOf("privacy: local-only") < fixture.output.text.indexOf("you> "));
  assert.match(fixture.output.text, /assistant> 你好🙂\n\[completed\]/);
  assert.match(fixture.output.text, /assistant> مرحبا\n\[completed\]/);
  assert.equal((fixture.output.text.match(/\[completed\]/g) ?? []).length, 2);
  assert.doesNotMatch(fixture.output.text, /fixture-model/);
  assert.equal(fixture.diagnostics.text, "");
});

test("maps the first active SIGINT to exactly one interruption", async () => {
  const transport = new ScriptedTransport("cancel-on-interrupt");
  const fixture = chatFixture(transport);
  const running = runChat(validArguments, fixture.io, fixture.dependencies);
  fixture.input.write("interrupt this turn\n");
  await transport.waitFor("start_turn");

  fixture.signals.emit("SIGINT");
  await transport.waitFor("interrupt_turn");
  fixture.input.end();

  assert.equal(await running, 0);
  assert.equal(transport.commands("interrupt_turn").length, 1);
  assert.match(fixture.output.text, /\[cancelled\]/);
  assert.doesNotMatch(fixture.diagnostics.text, /interrupt this turn/);
});

test("queues one SIGINT while turn allocation is pending", async () => {
  const transport = new ScriptedTransport("hold-start");
  const fixture = chatFixture(transport);
  const running = runChat(validArguments, fixture.io, fixture.dependencies);
  fixture.input.write("pending allocation\n");
  await transport.waitFor("start_turn");
  await setImmediate();

  fixture.signals.emit("SIGINT");
  assert.equal(transport.commands("interrupt_turn").length, 0);
  transport.releaseStart();
  await transport.waitFor("interrupt_turn");
  await waitForOutput(fixture.output, "[cancelled]");
  fixture.input.write("second turn\n");
  await waitForOutput(fixture.output, "[completed]");
  fixture.input.end();

  assert.equal(await running, 0);
  assert.equal(transport.commands("interrupt_turn").length, 1);
  assert.equal((fixture.output.text.match(/\[cancelled\]/g) ?? []).length, 1);
  assert.equal((fixture.output.text.match(/\[completed\]/g) ?? []).length, 1);
  assert.equal(transport.commands("start_turn").length, 2);
});

test("second SIGINT closes while turn allocation is pending", async () => {
  const transport = new ScriptedTransport("hold-start");
  const fixture = chatFixture(transport);
  const running = runChat(validArguments, fixture.io, fixture.dependencies);
  fixture.input.write("pending allocation\n");
  await transport.waitFor("start_turn");
  await setImmediate();

  fixture.signals.emit("SIGINT");
  fixture.signals.emit("SIGINT");

  assert.equal(await running, 0);
  assert.equal(transport.commands("interrupt_turn").length, 0);
  assert.equal(transport.closeCount, 1);
});

test("EOF closes while turn allocation is pending", async () => {
  const transport = new ScriptedTransport("hold-start");
  const fixture = chatFixture(transport);
  const running = runChat(validArguments, fixture.io, fixture.dependencies);
  fixture.input.write("pending allocation\n");
  await transport.waitFor("start_turn");
  await setImmediate();

  fixture.input.end();

  assert.equal(await running, 0);
  assert.equal(transport.commands("interrupt_turn").length, 0);
  assert.equal(transport.closeCount, 1);
});

test("closes an active gateway on a second SIGINT without sending another interruption", async () => {
  const transport = new ScriptedTransport("stall-after-interrupt");
  const fixture = chatFixture(transport);
  const running = runChat(validArguments, fixture.io, fixture.dependencies);
  fixture.input.write("long turn\n");
  await transport.waitFor("start_turn");

  fixture.signals.emit("SIGINT");
  await transport.waitFor("interrupt_turn");
  fixture.signals.emit("SIGINT");

  assert.equal(await running, 0);
  assert.equal(transport.commands("interrupt_turn").length, 1);
  assert.equal(transport.closeCount, 1);
});

test("treats SIGINT at the visible assistant prompt as an active-turn interruption", async () => {
  const transport = new ScriptedTransport("cancel-on-interrupt");
  const fixture = chatFixture(transport, { signalAtAssistantPrompt: true });
  const running = runChat(validArguments, fixture.io, fixture.dependencies);
  fixture.input.write("prompt boundary turn\n");

  await withDeadline(
    transport.waitFor("interrupt_turn"),
    250,
    "assistant prompt SIGINT did not interrupt the active turn",
  );
  fixture.input.end();

  assert.equal(await running, 0);
  assert.equal(transport.commands("interrupt_turn").length, 1);
  assert.equal(transport.closeCount, 1);
  assert.match(fixture.output.text, /assistant> \n\[cancelled\]/);
  assert.equal(fixture.diagnostics.text, "");
});

test("closes a stalled active turn on EOF without sending an interruption", async () => {
  const transport = new ScriptedTransport("stall-after-interrupt");
  const fixture = chatFixture(transport);
  const running = runChat(validArguments, fixture.io, fixture.dependencies);
  fixture.input.write("active EOF turn\n");
  await transport.waitFor("start_turn");

  fixture.input.end();

  assert.equal(
    await withDeadline(running, 250, "active-turn EOF did not close the client"),
    0,
  );
  assert.equal(transport.closeCount, 1);
  assert.equal(transport.commands("interrupt_turn").length, 0);
  assert.equal(fixture.diagnostics.text, "");
});

test("closes cleanly on idle SIGINT and EOF", async () => {
  const signalTransport = new ScriptedTransport("complete");
  const signalFixture = chatFixture(signalTransport);
  const signalRun = runChat(validArguments, signalFixture.io, signalFixture.dependencies);
  await signalTransport.waitFor("status");
  signalFixture.signals.emit("SIGINT");

  assert.equal(await signalRun, 0);
  assert.equal(signalTransport.closeCount, 1);
  assert.equal(signalTransport.commands("interrupt_turn").length, 0);

  const eofTransport = new ScriptedTransport("complete");
  const eofFixture = chatFixture(eofTransport);
  eofFixture.input.end();

  assert.equal(await runChat(validArguments, eofFixture.io, eofFixture.dependencies), 0);
  assert.equal(eofTransport.closeCount, 1);
});

test("returns nonzero on gateway failure without exposing transcript or provider data", async () => {
  const transport = new ScriptedTransport("fail-turn");
  const fixture = chatFixture(transport);
  fixture.input.end("diagnostic secret\n");

  const exitCode = await runChat(validArguments, fixture.io, fixture.dependencies);

  assert.equal(exitCode, 1);
  assert.equal(fixture.diagnostics.text, "chat failed\n");
  assert.doesNotMatch(fixture.diagnostics.text, /diagnostic secret|provider secret|fixture-model/);
});

test("streams completion through the compiled Rust gateway over framed pipes", { timeout: 10_000 }, async () => {
  await withGatewayFixture("complete", async (fixture) => {
    const transport = await StdioGatewayTransport.start({
      gatewayPath: fixture.gatewayPath,
      configPath: fixture.configPath,
    });
    try {
      const messages = transport.messages[Symbol.asyncIterator]();
      const ready = await nextGatewayMessage(messages);
      assert.equal(ready.type, "ready");
      assert.equal(ready.status.privacyMode, "local_only");

      await transport.send({
        type: "start_turn",
        requestId: "completion-start",
        transcript: "completion fixture",
      });
      const observed = await collectUntilTerminal(messages);

      assert.equal(observed[0]?.type, "command_accepted");
      assert.equal(
        observed
          .filter((message) => message.type === "runtime_event" && message.event.type === "text_delta")
          .map((message) => message.type === "runtime_event" && message.event.type === "text_delta" ? message.event.delta : "")
          .join(""),
        "framed ✓",
      );
      assert.equal(terminalType(observed), "turn_completed");
      assert.equal(fixture.requestCount(), 1);
    } finally {
      await transport.close();
    }
  });
});

test("cancels independently through a second compiled Rust gateway process", { timeout: 10_000 }, async () => {
  await withGatewayFixture("stall", async (fixture) => {
    const transport = await StdioGatewayTransport.start({
      gatewayPath: fixture.gatewayPath,
      configPath: fixture.configPath,
    });
    try {
      const messages = transport.messages[Symbol.asyncIterator]();
      assert.equal((await nextGatewayMessage(messages)).type, "ready");
      await transport.send({
        type: "start_turn",
        requestId: "cancellation-start",
        transcript: "cancellation fixture",
      });

      const observed: GatewayMessage[] = [await nextGatewayMessage(messages)];
      assert.equal(observed[0]?.type, "command_accepted");
      await fixture.waitForRequestStart();

      await transport.send({
        type: "interrupt_turn",
        requestId: "cancellation-interrupt",
        turnId: 1n,
      });
      while (
        !observed.some((message) => message.type === "command_accepted" && message.requestId === "cancellation-interrupt")
        || terminalType(observed) === undefined
      ) {
        observed.push(await nextGatewayMessage(messages));
      }

      assert.equal(terminalType(observed), "turn_cancelled");
      assert.equal(
        observed.some((message) => message.type === "runtime_event" && message.event.type === "turn_completed"),
        false,
      );
      await fixture.waitForConnectionClose();
    } finally {
      await transport.close();
    }
  });
});

test("active EOF closes a compiled Rust gateway with a stalled provider", { timeout: 10_000 }, async () => {
  await withGatewayFixture("stall", async (fixture) => {
    const input = new PassThrough();
    let resolvePrompt!: () => void;
    const promptVisible = new Promise<void>((resolve) => {
      resolvePrompt = resolve;
    });
    const output = new TextSink((text) => {
      if (text === "you> ") {
        resolvePrompt();
      }
    });
    const diagnostics = new TextSink();
    const signals = new EventEmitter();
    const running = runChat(
      ["--gateway", fixture.gatewayPath, "--config", fixture.configPath],
      { input, output, diagnostics, signals },
    );
    await withDeadline(promptVisible, 2_000, "compiled gateway chat prompt was not ready");
    input.write("compiled gateway EOF turn\n");
    await fixture.waitForRequestStart();

    input.end();

    assert.equal(
      await withDeadline(running, 2_000, "compiled gateway did not close after active EOF"),
      0,
    );
    await fixture.waitForConnectionClose();
    assert.equal(diagnostics.text, "");
  });
});

type Script = "complete" | "cancel-on-interrupt" | "stall-after-interrupt" | "fail-turn" | "hold-start";

type ProviderScript = "complete" | "stall";

const repositoryRoot = fileURLToPath(new URL("../../../../", import.meta.url));
const compiledGatewayPath = join(
  repositoryRoot,
  "target",
  "debug",
  process.platform === "win32" ? "conversation-runtime-gateway.exe" : "conversation-runtime-gateway",
);

async function withGatewayFixture(
  script: ProviderScript,
  run: (fixture: {
    configPath: string;
    gatewayPath: string;
    requestCount(): number;
    waitForConnectionClose(): Promise<void>;
    waitForRequestStart(): Promise<void>;
  }) => Promise<void>,
): Promise<void> {
  await access(compiledGatewayPath);
  const directory = await mkdtemp(join(tmpdir(), "conversation-node-chat-"));
  let requests = 0;
  let resolveConnectionClose!: () => void;
  let resolveRequestStart!: () => void;
  const connectionClosed = new Promise<void>((resolve) => {
    resolveConnectionClose = resolve;
  });
  const requestStarted = new Promise<void>((resolve) => {
    resolveRequestStart = resolve;
  });
  const server = createServer(async (request, response) => {
    requests += 1;
    for await (const _chunk of request) {
    }
    resolveRequestStart();
    response.on("close", resolveConnectionClose);
    response.writeHead(200, { "content-type": "application/x-ndjson" });
    if (script === "complete") {
      writeProviderRecord(response, { message: { role: "assistant", content: "framed " }, done: false });
      writeProviderRecord(response, { message: { role: "assistant", content: "✓" }, done: true });
      response.end();
    } else {
      response.flushHeaders();
    }
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("deterministic provider did not bind a loopback port");
  }
  const configPath = join(directory, "gateway.toml");
  await writeFile(configPath, gatewayConfig(address.port));

  try {
    await run({
      configPath,
      gatewayPath: compiledGatewayPath,
      requestCount: () => requests,
      waitForConnectionClose: () => withDeadline(
        connectionClosed,
        2_000,
        "provider connection was not cancelled",
      ),
      waitForRequestStart: () => withDeadline(
        requestStarted,
        2_000,
        "provider request did not start",
      ),
    });
  } finally {
    server.closeAllConnections();
    await new Promise<void>((resolve) => server.close(() => resolve()));
    await rm(directory, { force: true, recursive: true });
  }
}

function writeProviderRecord(response: ServerResponse, value: unknown): void {
  response.write(`${JSON.stringify(value)}\n`);
}

function gatewayConfig(port: number): string {
  return `schema_version = 2
privacy_mode = "local-only"

[language]
backend = "ollama-compatible"
execution = "local"
provider = "local-language"
endpoint = "http://127.0.0.1:${port}"
model = "fixture-model"
thinking = false
temperature = 0.0
seed = 1
num_predict = 32
num_ctx = 1024
max_assistant_content_bytes = 4096

[persona]
mode = "direct-answer"
warmth = 50
humor = 50
teasing = 50
initiative = 50
directness = 50
intimacy = 50
verbosity = 50
follow_up_frequency = 50
`;
}

async function nextGatewayMessage(
  messages: AsyncIterator<unknown>,
): Promise<GatewayMessage> {
  const next = await withDeadline(messages.next(), 3_000, "gateway message timed out");
  if (next.done) {
    throw new Error("gateway messages ended before the expected terminal event");
  }
  return parseGatewayMessage(next.value);
}

async function collectUntilTerminal(
  messages: AsyncIterator<unknown>,
): Promise<GatewayMessage[]> {
  const observed: GatewayMessage[] = [];
  while (terminalType(observed) === undefined) {
    observed.push(await nextGatewayMessage(messages));
  }
  return observed;
}

function terminalType(
  messages: readonly GatewayMessage[],
): "turn_completed" | "turn_cancelled" | "turn_failed" | undefined {
  for (const message of messages) {
    if (
      message.type === "runtime_event"
      && (
        message.event.type === "turn_completed"
        || message.event.type === "turn_cancelled"
        || message.event.type === "turn_failed"
      )
    ) {
      return message.event.type;
    }
  }
  return undefined;
}

function withDeadline<T>(operation: Promise<T>, milliseconds: number, message: string): Promise<T> {
  return new Promise<T>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(message)), milliseconds);
    void operation.then(
      (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      (error: unknown) => {
        clearTimeout(timer);
        reject(error);
      },
    );
  });
}

async function waitForOutput(output: TextSink, expected: string): Promise<void> {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    if (output.text.includes(expected)) {
      return;
    }
    await setImmediate();
  }
  throw new Error(`chat output did not contain ${expected}`);
}

class ScriptedTransport implements RuntimeTransport {
  private readonly inbox = new AsyncChannel<unknown>();
  private readonly waiters = new Map<ClientCommand["type"], Array<() => void>>();
  private turnCounter = 0n;
  private heldStart: Extract<ClientCommand, { type: "start_turn" }> | undefined;
  private startReleased = false;
  readonly messages = this.inbox;
  readonly sent: ClientCommand[] = [];
  closeCount = 0;

  constructor(private readonly script: Script) {
    this.inbox.push({ type: "ready", protocol_version: 3, status: wireStatus() });
  }

  async send(command: ClientCommand): Promise<void> {
    this.sent.push(command);
    for (const resolve of this.waiters.get(command.type)?.splice(0) ?? []) {
      resolve();
    }
    queueMicrotask(() => this.respond(command));
  }

  async close(): Promise<void> {
    if (this.closeCount > 0) {
      return;
    }
    this.closeCount = 1;
    this.inbox.finish();
  }

  commands<T extends ClientCommand["type"]>(
    type: T,
  ): Array<Extract<ClientCommand, { type: T }>> {
    return this.sent.filter(
      (command): command is Extract<ClientCommand, { type: T }> => command.type === type,
    );
  }

  waitFor(type: ClientCommand["type"]): Promise<void> {
    if (this.sent.some((command) => command.type === type)) {
      return Promise.resolve();
    }
    return new Promise<void>((resolve) => {
      const waiters = this.waiters.get(type) ?? [];
      waiters.push(resolve);
      this.waiters.set(type, waiters);
    });
  }

  releaseStart(): void {
    const command = this.heldStart;
    if (!command) {
      throw new Error("no start command is awaiting allocation");
    }
    this.heldStart = undefined;
    this.startReleased = true;
    queueMicrotask(() => this.respond(command));
  }

  private respond(command: ClientCommand): void {
    if (command.type === "start_turn" && this.script === "hold-start" && !this.startReleased) {
      this.heldStart = command;
      return;
    }
    this.inbox.push({
      type: "command_accepted",
      protocol_version: 3,
      request_id: command.requestId,
      ...(command.type === "start_turn" ? { turn_id: (++this.turnCounter).toString() } : {}),
    });
    if (command.type === "status") {
      this.inbox.push({
        type: "status",
        protocol_version: 3,
        request_id: command.requestId,
        status: wireStatus(),
      });
      return;
    }
    if (command.type === "interrupt_turn") {
      if (this.script === "cancel-on-interrupt" || this.script === "hold-start") {
        this.inbox.push(runtimeEvent({
          type: "turn_cancelled",
          turn_id: command.turnId.toString(),
        }));
      }
      return;
    }
    if (command.type !== "start_turn") {
      return;
    }
    const turnId = this.turnCounter;

    this.inbox.push(runtimeEvent({
      type: "turn_started",
      request_id: command.requestId,
      turn_id: turnId.toString(),
    }));
    if (this.script === "complete" || (this.script === "hold-start" && turnId > 1n)) {
      const deltas = turnId === 1n ? ["你", "好🙂"] : ["مرح", "با"];
      for (const delta of deltas) {
        this.inbox.push(runtimeEvent({
          type: "text_delta",
          turn_id: turnId.toString(),
          delta,
        }));
      }
      this.inbox.push(runtimeEvent({
        type: "turn_completed",
        turn_id: turnId.toString(),
      }));
    } else if (this.script === "fail-turn") {
      this.inbox.fail(new Error("provider secret for diagnostic secret"));
    }
  }
}

class AsyncChannel<T> implements AsyncIterable<T> {
  private readonly values: T[] = [];
  private readonly waiters: Array<{
    resolve: (result: IteratorResult<T>) => void;
    reject: (error: Error) => void;
  }> = [];
  private error: Error | undefined;
  private ended = false;

  push(value: T): void {
    if (this.ended) {
      return;
    }
    const waiter = this.waiters.shift();
    if (waiter) {
      waiter.resolve({ value, done: false });
    } else {
      this.values.push(value);
    }
  }

  finish(error?: Error): void {
    if (this.ended) {
      return;
    }
    this.ended = true;
    this.error = error;
    this.values.length = 0;
    for (const waiter of this.waiters.splice(0)) {
      if (error) {
        waiter.reject(error);
      } else {
        waiter.resolve({ value: undefined, done: true });
      }
    }
  }

  fail(error: Error): void {
    this.finish(error);
  }

  [Symbol.asyncIterator](): AsyncIterator<T> {
    return {
      next: () => {
        if (this.error) {
          return Promise.reject(this.error);
        }
        const value = this.values.shift();
        if (value !== undefined) {
          return Promise.resolve({ value, done: false });
        }
        if (this.ended) {
          return Promise.resolve({ value: undefined, done: true });
        }
        return new Promise<IteratorResult<T>>((resolve, reject) => {
          this.waiters.push({ resolve, reject });
        });
      },
    };
  }
}

class TextSink extends Writable {
  text = "";

  constructor(private readonly onWrite?: (text: string) => void) {
    super();
  }

  override _write(
    chunk: Buffer | string,
    _encoding: BufferEncoding,
    callback: (error?: Error | null) => void,
  ): void {
    const text = chunk.toString();
    this.text += text;
    this.onWrite?.(text);
    callback();
  }
}

function chatFixture(
  transport: ScriptedTransport,
  options: { signalAtAssistantPrompt?: boolean } = {},
): {
  dependencies: ChatDependencies;
  diagnostics: TextSink;
  input: PassThrough;
  io: ChatIo;
  output: TextSink;
  signals: EventEmitter;
  startCount(): number;
} {
  const input = new PassThrough();
  const signals = new EventEmitter();
  const output = new TextSink((text) => {
    if (options.signalAtAssistantPrompt && text === "assistant> ") {
      signals.emit("SIGINT");
    }
  });
  const diagnostics = new TextSink();
  let starts = 0;
  const dependencies: ChatDependencies = {
    async startTransport() {
      starts += 1;
      return transport;
    },
  };
  return {
    dependencies,
    diagnostics,
    input,
    io: { input, output, diagnostics, signals },
    output,
    signals,
    startCount: () => starts,
  };
}

function runtimeEvent(event: Record<string, unknown>): Record<string, unknown> {
  return { type: "runtime_event", protocol_version: 3, event };
}

function wireStatus(): RuntimeStatusWire {
  return {
    transport: "stdio",
    privacy_mode: "local_only",
    language_location: "local",
    model_id: "fixture-model",
    memory_enabled: false,
    memory_location: null,
    telemetry_enabled: false,
    capabilities: ["text"],
    components: [
      { kind: "language_model", execution_location: "local", provider_label: "Local language" },
    ],
  };
}

type RuntimeStatusWire = {
  transport: RuntimeStatus["transport"];
  privacy_mode: "local_only";
  language_location: "local";
  model_id: string;
  memory_enabled: boolean;
  memory_location: "local" | null;
  telemetry_enabled: false;
  capabilities: ["text"];
  components: [{
    kind: "language_model";
    execution_location: "local";
    provider_label: string;
  }];
};
