import assert from "node:assert/strict";
import { spawn as spawnChild, type ChildProcess } from "node:child_process";
import { EventEmitter, once } from "node:events";
import { access, chmod, mkdtemp, open, readFile, rm, stat, writeFile, type FileHandle } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { performance } from "node:perf_hooks";
import test from "node:test";
import { setTimeout } from "node:timers/promises";

import { RuntimeClient } from "../src/client.js";
import { type ClientProtocolVersion } from "../src/protocol.js";
import { StdioGatewayTransport } from "../src/stdio.js";

test("requires absolute gateway and configuration paths", async () => {
  await assert.rejects(
    StdioGatewayTransport.start({ gatewayPath: "./gateway", configPath: "/tmp/gateway.toml" }),
    /absolute gateway path/,
  );
  await assert.rejects(
    StdioGatewayTransport.start({ gatewayPath: "/tmp/gateway", configPath: "./gateway.toml" }),
    /absolute configuration path/,
  );
});

test("spawns without a shell", async () => {
  await withGateway(
    "emit({ type: 'ready', protocol_version: 1, status }); process.stdin.once('end', () => process.exit(0));",
    async ({ gatewayPath, directory }) => {
      const marker = join(directory, "shell-executed");
      const configPath = join(directory, `$(touch ${marker})`);
      const transport = await StdioGatewayTransport.start({ gatewayPath, configPath });
      await transport.messages[Symbol.asyncIterator]().next();
      await assert.rejects(access(marker));
      await transport.close();
    },
  );
});

test("explicit v4 is passed as argv while absent selection preserves default argv", async () => {
  await withGateway(
    "emit({ argv: process.argv.slice(2) }); process.stdin.resume(); process.stdin.once('end', () => process.exit(0));",
    async ({ gatewayPath, configPath }) => {
      for (const protocolVersion of [undefined, 4] as const) {
        const transport = await StdioGatewayTransport.start({ gatewayPath, configPath, ...(protocolVersion === undefined ? {} : { protocolVersion }) });
        const message = await transport.messages[Symbol.asyncIterator]().next();
        assert.deepEqual(message.value, { argv: ["--config", configPath, ...(protocolVersion === 4 ? ["--client-protocol-version", "4"] : [])] });
        await transport.close();
      }
    },
  );
  await assert.rejects(StdioGatewayTransport.start({ gatewayPath: "/nonexistent", configPath: "/nonexistent", protocolVersion: 3 as 4 }), /protocol selection is invalid/);
});

test("explicit environment replaces inherited values while omission preserves inheritance", async () => {
  const inheritedName = "RUNTIME_TASK13_INHERITED_CANARY";
  const explicitName = "RUNTIME_TASK13_EXPLICIT_CANARY";
  const previous = process.env[inheritedName];
  process.env[inheritedName] = "synthetic-parent-value";
  try {
    await withGateway(
      `emit({ inherited: process.env.${inheritedName} ?? null, explicit: process.env.${explicitName} ?? null }); process.stdin.resume(); process.stdin.once('end', () => process.exit(0));`,
      async ({ gatewayPath, configPath }) => {
        const inherited = await StdioGatewayTransport.start({ gatewayPath, configPath });
        assert.deepEqual((await inherited.messages[Symbol.asyncIterator]().next()).value, {
          inherited: "synthetic-parent-value",
          explicit: null,
        });
        await inherited.close();

        const replacement = { PATH: process.env.PATH ?? "/usr/bin:/bin", [explicitName]: "synthetic-runtime-value" };
        const isolated = await StdioGatewayTransport.start({ gatewayPath, configPath, env: replacement });
        replacement[explicitName] = "mutated-after-spawn";
        assert.deepEqual((await isolated.messages[Symbol.asyncIterator]().next()).value, {
          inherited: null,
          explicit: "synthetic-runtime-value",
        });
        await isolated.close();
      },
    );
  } finally {
    if (previous === undefined) delete process.env[inheritedName];
    else process.env[inheritedName] = previous;
  }
});

test("validates cleanup budgets and environment before spawning", async () => {
  for (const cleanupTimeouts of [
    { eofGraceMs: 0, termGraceMs: 1, killGraceMs: 1 },
    { eofGraceMs: 1, termGraceMs: 30_001, killGraceMs: 1 },
    { eofGraceMs: 30_000, termGraceMs: 30_000, killGraceMs: 1 },
    { eofGraceMs: 1.5, termGraceMs: 1, killGraceMs: 1 },
  ]) {
    await assert.rejects(
      StdioGatewayTransport.start({ gatewayPath: "/does-not-run", configPath: "/config", cleanupTimeouts }),
      /gateway cleanup timeouts are invalid/,
    );
  }
  await assert.rejects(
    StdioGatewayTransport.start({
      gatewayPath: "/does-not-run",
      configPath: "/config",
      env: { CANARY: 42 as unknown as string },
    }),
    (error: Error) => error.message === "gateway environment is invalid" && !error.message.includes("CANARY"),
  );
});

test("an already aborted startup does not spawn", async () => {
  const controller = new AbortController();
  controller.abort();
  await assert.rejects(
    StdioGatewayTransport.start({
      gatewayPath: "/does-not-run",
      configPath: "/config",
      startupAbortSignal: controller.signal,
    }),
    (error: Error) => error.name === "AbortError" && error.message === "gateway startup aborted",
  );
});

test("startup abort after the spawn request reaps the owned child", { timeout: 2_000 }, async () => {
  await withGateway(
    "process.stdin.resume(); process.on('SIGTERM', () => {}); setInterval(() => {}, 1000);",
    async ({ gatewayPath, configPath }) => {
      const controller = new AbortController();
      const starting = StdioGatewayTransport.start({
        gatewayPath,
        configPath,
        startupAbortSignal: controller.signal,
        cleanupTimeouts: { eofGraceMs: 5, termGraceMs: 5, killGraceMs: 200 },
      });
      controller.abort();
      await assert.rejects(starting, /gateway startup aborted/);
    },
  );
});

test("a retained transport lets an owner cancel a no-ready client startup", { timeout: 2_000 }, async () => {
  await withGateway(
    "writeFileSync(new URL('./pid.txt', import.meta.url), String(process.pid)); process.stdin.resume(); process.stdin.once('end', () => process.exit(0));",
    async ({ gatewayPath, configPath, directory }) => {
      let transport: StdioGatewayTransport | undefined;
      let failure: unknown;
      let pid: number | undefined;
      try {
        transport = await StdioGatewayTransport.start({
          gatewayPath,
          configPath,
          cleanupTimeouts: { eofGraceMs: 20, termGraceMs: 20, killGraceMs: 100 },
        });
        pid = await waitForPidFile(join(directory, "pid.txt"));
        assert.ok(Number.isInteger(pid) && pid > 0);
        const connecting = RuntimeClient.connect(transport);
        const rejected = assert.rejects(connecting, /runtime transport ended before a terminal message/);
        let cancelled = false;
        await setTimeout(20);
        cancelled = true;
        await transport.close();
        await rejected;
        assert.equal(cancelled, true);
        assert.notEqual(transport.cleanupOutcome?.status, "failed");
        await waitForPidExit(pid);
      } catch (error) {
        failure = error;
      } finally {
        failure = await retainCleanupFailure(failure, async () => {
          if (transport) await transport.close();
        });
        failure = await retainCleanupFailure(failure, async () => {
          if (pid !== undefined) await waitForPidExit(pid);
        });
      }
      if (failure !== undefined) throw failure;
    },
  );
});

test("continuously drains bounded stderr until the child exits", async () => {
  await withGateway(
    "emit({ type: 'ready', protocol_version: 1, status }); (async () => { const chunk = 'x'.repeat(65536); for (let index = 0; index < 8; index += 1) { if (!process.stderr.write(chunk)) await new Promise((resolve) => process.stderr.once('drain', resolve)); } process.exit(0); })();",
    async ({ gatewayPath, configPath }) => {
      const transport = await StdioGatewayTransport.start({ gatewayPath, configPath });
      const iterator = transport.messages[Symbol.asyncIterator]();
      assert.match(JSON.stringify((await iterator.next()).value), /\"type\":\"ready\"/);
      await assert.rejects(iterator.next(), /gateway (stdout ended|process exited)/);
    },
  );
});

test("rejects client work when the gateway exits", async () => {
  await withGateway(
    "emit({ type: 'ready', protocol_version: 1, status }); process.stdin.once('data', () => setTimeout(() => process.exit(1), 10));",
    async ({ gatewayPath, configPath }) => {
      const transport = await StdioGatewayTransport.start({ gatewayPath, configPath });
      const client = await RuntimeClient.connect(transport);

      await assert.rejects(client.status(), /gateway (stdout ended|process exited)/);
      await client.close();
    },
  );
});

test("discards a buffered ready message when the process exits", async () => {
  const child = new FakeChild();
  const transport = startWithChild(child);
  child.stdout.emit("data", framedReady());
  child.exitCode = 1;
  child.emit("exit");
  child.stdin.emit("close");
  child.stdout.emit("close");
  child.stderr.emit("close");

  await assert.rejects(RuntimeClient.connect(transport), /gateway process exited/);
  await transport.close();
});

test("discards buffered status responses when exit follows", async () => {
  const child = new FakeChild();
  const transport = startWithChild(child);
  const iterator = transport.messages[Symbol.asyncIterator]();
  child.stdout.emit("data", framedReady());
  await iterator.next();
  await transport.send({ type: "status", requestId: "request-1" }, 1);
  child.stdout.emit("data", Buffer.concat([
    framedMessage({
      type: "command_accepted",
      protocol_version: 1,
      request_id: "request-1",
    }),
    framedMessage({
      type: "status",
      protocol_version: 1,
      request_id: "request-1",
      status: wireStatus(),
    }),
  ]));
  child.exitCode = 1;
  child.emit("exit");
  child.stdin.emit("close");
  child.stdout.emit("close");
  child.stderr.emit("close");

  await assert.rejects(iterator.next(), /gateway process exited/);
  await transport.close();
});

test("encodes stdio commands with the negotiated protocol version", async () => {
  const child = new FakeChild();
  const transport = startWithChild(child);

  await transport.send({ type: "status", requestId: "request-1" }, 2);

  assert.deepEqual(decodeWrittenFrame(child.stdin.frames[0]!), {
    protocol_version: 2,
    type: "status",
    request_id: "request-1",
  });
  await transport.close();
});

test("close is finite and stable when the child never exits", { timeout: 1_000 }, async () => {
  const child = new FakeChild({ killResult: false });
  const transport = startWithChild(child, { eofGraceMs: 5, termGraceMs: 5, killGraceMs: 10 });
  const first = transport.close();
  const second = transport.close();
  assert.equal(first, second);
  await assert.rejects(first, /gateway cleanup failed/);
  assert.deepEqual(transport.cleanupOutcome, { status: "failed", reason: "signal_failed" });
});

test("close is finite when signaling throws", { timeout: 1_000 }, async () => {
  const child = new FakeChild({ killThrows: true });
  const transport = startWithChild(child, { eofGraceMs: 5, termGraceMs: 5, killGraceMs: 10 });
  await assert.rejects(transport.close(), /gateway cleanup failed/);
  assert.deepEqual(transport.cleanupOutcome, { status: "failed", reason: "signal_failed" });
});

test("wall-clock rollback cannot extend the total cleanup budget", { timeout: 1_000 }, async () => {
  const child = new FakeChild({ killResult: false });
  const transport = startWithChild(child, { eofGraceMs: 10, termGraceMs: 10, killGraceMs: 10 });
  const realNow = Date.now;
  let offsetMs = 0;
  Date.now = () => realNow() + offsetMs;
  const startedAt = performance.now();
  const rollback = globalThis.setTimeout(() => { offsetMs = -200; }, 5);
  try {
    await assert.rejects(transport.close(), /gateway cleanup failed/);
    assert.ok(performance.now() - startedAt < 100, "cleanup exceeded its monotonic total budget");
  } finally {
    globalThis.clearTimeout(rollback);
    Date.now = realNow;
  }
});

test("wall-clock jump cannot truncate a cleanup phase", { timeout: 1_000 }, async () => {
  const child = new FakeChild({ termExitDelayMs: 20 });
  const transport = startWithChild(child, { eofGraceMs: 10, termGraceMs: 40, killGraceMs: 10 });
  const realNow = Date.now;
  let offsetMs = 0;
  Date.now = () => realNow() + offsetMs;
  const jump = globalThis.setTimeout(() => { offsetMs = 200; }, 5);
  try {
    await transport.close();
    assert.deepEqual(child.signals, ["SIGTERM"]);
    assert.deepEqual(transport.cleanupOutcome, { status: "forced" });
  } finally {
    globalThis.clearTimeout(jump);
    Date.now = realNow;
  }
});

test("close rejects when process exit precedes owned pipe closure", { timeout: 1_000 }, async () => {
  const child = new FakeChild({ closePipesOnExit: false });
  const transport = startWithChild(child, { eofGraceMs: 5, termGraceMs: 5, killGraceMs: 10 });
  child.exitCode = 0;
  child.emit("exit");
  await assert.rejects(transport.close(), /gateway cleanup failed/);
  assert.deepEqual(transport.cleanupOutcome, { status: "failed", reason: "owned_pipes_not_closed" });
  assert.equal(child.stdout.destroyed, true);
  assert.equal(child.stderr.destroyed, true);
});

test("exit during pipe drain does not send a signal to an exited child", { timeout: 1_000 }, async () => {
  const child = new FakeChild({ closePipesOnExit: false });
  const transport = startWithChild(child, { eofGraceMs: 5, termGraceMs: 5, killGraceMs: 10 });
  queueMicrotask(() => {
    child.exitCode = 0;
    child.emit("exit");
  });
  await assert.rejects(transport.close(), /gateway cleanup failed/);
  assert.equal(child.killCount, 0);
  assert.deepEqual(transport.cleanupOutcome, { status: "failed", reason: "owned_pipes_not_closed" });
});

test("close fences a queued write before submission", { timeout: 1_000 }, async () => {
  const child = new FakeChild({ holdWrites: true });
  const transport = startWithChild(child, { eofGraceMs: 5, termGraceMs: 5, killGraceMs: 10 });
  const send = transport.send({ type: "status", requestId: "request-1" }, 1);
  const close = transport.close();
  await assert.rejects(send, /gateway transport is closed/);
  assert.equal(child.stdin.frames.length, 0);
  await close;
  await assert.rejects(transport.send({ type: "status", requestId: "request-2" }, 1), /gateway transport is closed/);
});

test("close settles an in-flight write once and a late callback cannot resurrect work", { timeout: 1_000 }, async () => {
  const child = new FakeChild({ holdWrites: true });
  const transport = startWithChild(child, { eofGraceMs: 5, termGraceMs: 5, killGraceMs: 10 });
  const iterator = transport.messages[Symbol.asyncIterator]();
  let settlements = 0;
  const send = transport.send({ type: "status", requestId: "request-1" }, 1).then(
    () => { settlements += 1; },
    (error: unknown) => { settlements += 1; throw error; },
  );
  await child.stdin.waitForWrite();
  assert.equal(child.stdin.frames.length, 1);

  const close = transport.close();
  await assert.rejects(send, /gateway transport is closed/);
  assert.equal(settlements, 1);
  child.stdin.completeWrite();
  await close;
  await Promise.resolve();
  child.stdout.emit("data", framedReady());
  assert.equal(settlements, 1);
  assert.deepEqual(await iterator.next(), { value: undefined, done: true });
});

test("reports forced cleanup only after exit and all owned pipes close", async () => {
  const child = new FakeChild();
  const transport = startWithChild(child, { eofGraceMs: 1, termGraceMs: 10, killGraceMs: 10 });
  await transport.close();
  assert.deepEqual(transport.cleanupOutcome, { status: "forced" });
});

test("late output cannot resurrect a closed input iterator", async () => {
  const child = new FakeChild();
  const transport = startWithChild(child, { eofGraceMs: 1, termGraceMs: 10, killGraceMs: 10 });
  const iterator = transport.messages[Symbol.asyncIterator]();
  await transport.close();
  child.stdout.emit("data", framedReady());
  assert.deepEqual(await iterator.next(), { value: undefined, done: true });
});

test("close discards retained framed input and settles readers", async () => {
  const child = new FakeChild();
  const transport = startWithChild(child, { eofGraceMs: 1, termGraceMs: 10, killGraceMs: 10 });
  child.stdout.emit("data", framedReady());
  const iterator = transport.messages[Symbol.asyncIterator]();
  await transport.close();
  assert.deepEqual(await iterator.next(), { value: undefined, done: true });
});

test("actual child exit with an inherited stdout holder is not clean pipe completion", { timeout: 3_000 }, async () => {
  await withGateway(
    "const { spawn } = await import('node:child_process'); const holder = spawn(process.execPath, ['-e', 'setTimeout(() => {}, 5000)'], { stdio: ['ignore', process.stdout, 'ignore'] }); emit({ holder_pid: holder.pid }); process.stdin.resume(); process.stdin.once('end', () => process.exit(0));",
    async ({ gatewayPath, configPath }) => {
      const transport = await StdioGatewayTransport.start({
        gatewayPath,
        configPath,
        cleanupTimeouts: { eofGraceMs: 20, termGraceMs: 20, killGraceMs: 100 },
      });
      const message = (await transport.messages[Symbol.asyncIterator]().next()).value as { holder_pid: number };
      try {
        await assert.rejects(transport.close(), /gateway cleanup failed/);
        assert.deepEqual(transport.cleanupOutcome, { status: "failed", reason: "owned_pipes_not_closed" });
      } finally {
        process.kill(message.holder_pid, "SIGKILL");
        await waitForPidExit(message.holder_pid);
      }
    },
  );
});

test("actual SDK launcher excludes application fd3 while mutation retains it", { timeout: 4_000 }, async (context) => {
  if (process.platform !== "darwin") {
    context.skip("macOS lockf descriptor proof");
    return;
  }
  await withGateway(
    "process.stdin.resume(); process.stdin.once('end', () => setTimeout(() => process.exit(0), 300));",
    async ({ gatewayPath, configPath, directory }) => {
      const lockPath = join(directory, "application.lock");
      await writeFile(lockPath, "");
      const inode = (await stat(lockPath)).ino;
      let owner: FileHandle | undefined;
      let transport: StdioGatewayTransport | undefined;
      let mutatingOwner: FileHandle | undefined;
      let holder: ChildProcess | undefined;
      let failure: unknown;
      try {
        owner = await openLockOwner(lockPath);
        transport = await StdioGatewayTransport.start({ gatewayPath, configPath });
        await owner.close();
        owner = undefined;
        assert.equal(await contendForLock(lockPath), 0);
        assert.equal((await stat(lockPath)).ino, inode);
        await transport.close();

        mutatingOwner = await openLockOwner(lockPath);
        holder = spawnChild(process.execPath, ["-e", "setTimeout(() => {}, 5000)"], {
          stdio: ["ignore", "ignore", "ignore", mutatingOwner.fd],
        });
        await once(holder, "spawn");
        await mutatingOwner.close();
        mutatingOwner = undefined;
        assert.equal(await contendForLock(lockPath), 75);
        assert.equal((await stat(lockPath)).ino, inode);
        await killAndWait(holder);
        assert.equal(await contendForLock(lockPath), 0);
        assert.equal((await stat(lockPath)).ino, inode);
      } catch (error) {
        failure = error;
      } finally {
        failure = await retainCleanupFailure(failure, async () => {
          if (transport) await transport.close();
        });
        failure = await retainCleanupFailure(failure, async () => {
          if (owner) await owner.close();
        });
        failure = await retainCleanupFailure(failure, async () => {
          if (mutatingOwner) await mutatingOwner.close();
        });
        failure = await retainCleanupFailure(failure, async () => {
          if (holder) await killAndWait(holder);
        });
      }
      if (failure !== undefined) throw failure;
    },
  );
});

test("closes an EOF-ignoring child with bounded termination and reaping", { timeout: 2_000 }, async () => {
  await withGateway(
    "process.stdin.resume(); process.on('SIGTERM', () => {}); setTimeout(() => process.exit(0), 700); emit({ type: 'ready', protocol_version: 1, status });",
    async ({ gatewayPath, configPath }) => {
      const transport = await StdioGatewayTransport.start({ gatewayPath, configPath });
      await transport.messages[Symbol.asyncIterator]().next();
      await Promise.race([
        transport.close(),
        setTimeout(500).then(() => Promise.reject(new Error("close did not finish within deadline"))),
      ]);
    },
  );
});

test("uses content-free errors for spawn and child stream failures", async () => {
  const privatePath = join(tmpdir(), "private-gateway-path");
  await assert.rejects(
    StdioGatewayTransport.start({ gatewayPath: privatePath, configPath: join(tmpdir(), "private-config.toml") }),
    (error: Error) => error.message === "gateway spawn failed" && !error.message.includes(privatePath),
  );

  await withGateway(
    "emit({ type: 'ready', protocol_version: 1, status }); process.stdin.once('data', () => { process.stderr.write('private-stderr-value'); process.exit(1); });",
    async ({ gatewayPath, configPath }) => {
      const transport = await StdioGatewayTransport.start({ gatewayPath, configPath });
      const client = await RuntimeClient.connect(transport);
      await assert.rejects(
        client.status(),
        (error: Error) => /gateway (stdout ended|process exited|stdin failed)/.test(error.message) && !error.message.includes("private-stderr-value"),
      );
      await client.close();
    },
  );
});

test("maps synchronous spawn exceptions to content-free errors", async () => {
  const privatePath = "/private/\u0000gateway";
  await assert.rejects(
    StdioGatewayTransport.start({ gatewayPath: privatePath, configPath: "/private/config.toml" }),
    (error: Error) => error.message === "gateway spawn failed" && !error.message.includes(privatePath),
  );
});

for (const streamName of ["stdout", "stderr", "stdin"] as const) {
  test(`maps emitted ${streamName} errors to one content-free transport failure`, async () => {
    const child = new FakeChild();
    const transport = startWithChild(child);
    child.stdout.emit("data", framedReady());
    const client = await RuntimeClient.connect(transport);
    const pending = client.status();
    const privateMessage = `private-${streamName}-error`;
    child[streamName].emit("error", new Error(privateMessage));

    await assert.rejects(
      pending,
      (error: Error) => error.message === `gateway ${streamName} failed` && !error.message.includes(privateMessage),
    );
    await client.close();
    assert.equal(child.killCount, 1);
  });
}

async function withGateway(
  body: string,
  run: (paths: { gatewayPath: string; configPath: string; directory: string }) => Promise<void>,
): Promise<void> {
  const directory = await mkdtemp(join(tmpdir(), "conversation-runtime-"));
  const gatewayPath = join(directory, "fake-gateway.mjs");
  const configPath = join(directory, "gateway.toml");
  const program = `#!/usr/bin/env node
import { writeFileSync } from 'node:fs';
const status = { transport: 'stdio', privacy_mode: 'local_only', language_location: 'local', model_id: 'local-model', memory_enabled: false, memory_location: null, telemetry_enabled: false, capabilities: ['text'], components: [{ kind: 'language_model', execution_location: 'local', provider_label: 'Local language' }] };
const emit = (value) => { const payload = Buffer.from(JSON.stringify(value)); const header = Buffer.alloc(4); header.writeUInt32BE(payload.length); process.stdout.write(Buffer.concat([header, payload])); };
${body}
`;
  await writeFile(gatewayPath, program);
  await chmod(gatewayPath, 0o755);
  await writeFile(configPath, "schema_version = 1\n");
  try {
    await run({ gatewayPath, configPath, directory });
  } finally {
    await rm(directory, { force: true, recursive: true });
  }
}

function startWithChild(
  child: FakeChild,
  cleanupTimeouts?: { eofGraceMs: number; termGraceMs: number; killGraceMs: number },
): StdioGatewayTransport {
  const testable = StdioGatewayTransport as unknown as {
    startWithChildForTest(child: unknown, cleanupTimeouts?: { eofGraceMs: number; termGraceMs: number; killGraceMs: number }): StdioGatewayTransport;
  };
  return testable.startWithChildForTest(child, cleanupTimeouts);
}

function framedReady(): Buffer {
  return framedMessage({ type: "ready", protocol_version: 1, status: wireStatus() });
}

function wireStatus(): object {
  return {
    transport: "stdio",
    privacy_mode: "local_only",
    language_location: "local",
    model_id: "local-model",
    memory_enabled: false,
    memory_location: null,
    telemetry_enabled: false,
    capabilities: ["text"],
    components: [
      { kind: "language_model", execution_location: "local", provider_label: "Local language" },
    ],
  };
}

function framedMessage(message: object): Buffer {
  const payload = Buffer.from(JSON.stringify(message));
  const header = Buffer.alloc(4);
  header.writeUInt32BE(payload.length);
  return Buffer.concat([header, payload]);
}

class FakeChild extends EventEmitter {
  readonly stderr = new FakeReadable();
  readonly stdin: FakeStdin;
  readonly stdout = new FakeReadable();
  exitCode: number | null = null;
  killCount = 0;
  readonly signals: NodeJS.Signals[] = [];

  constructor(private readonly options: { killResult?: boolean; killThrows?: boolean; closePipesOnExit?: boolean; holdWrites?: boolean; termExitDelayMs?: number } = {}) {
    super();
    this.stdin = new FakeStdin(options.holdWrites ?? false);
  }

  kill(signal: NodeJS.Signals = "SIGTERM"): boolean {
    if (this.options.killThrows) throw new Error("private kill failure");
    if (this.exitCode !== null) {
      return false;
    }
    this.killCount += 1;
    this.signals.push(signal);
    if (this.options.killResult === false) return false;
    if (signal === "SIGTERM" && this.options.termExitDelayMs !== undefined) {
      globalThis.setTimeout(() => {
        if (this.exitCode === null) this.exit();
      }, this.options.termExitDelayMs);
      return true;
    }
    this.exit();
    return true;
  }

  private exit(): void {
    this.exitCode = 1;
    queueMicrotask(() => {
      this.emit("exit");
      if (this.options.closePipesOnExit !== false) {
        this.stdin.emit("close");
        this.stdout.emit("close");
        this.stderr.emit("close");
        this.emit("close");
      }
    });
  }
}

async function waitForPidExit(pid: number): Promise<void> {
  const deadline = performance.now() + 1_000;
  while (performance.now() < deadline) {
    try {
      process.kill(pid, 0);
    } catch {
      return;
    }
    await setTimeout(10);
  }
  throw new Error("test-owned holder was not reaped");
}

async function waitForPidFile(path: string): Promise<number> {
  const deadline = performance.now() + 1_000;
  while (performance.now() < deadline) {
    try {
      const pid = Number(await readFile(path, "utf8"));
      if (Number.isInteger(pid) && pid > 0) return pid;
      throw new Error("fixture-owned gateway wrote an invalid PID");
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
    }
    await setTimeout(10);
  }
  throw new Error("fixture-owned gateway did not record its PID");
}

async function killAndWait(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, "exit");
  if (!child.kill("SIGKILL")) throw new Error("test-owned holder signal failed");
  await exited;
}

async function retainCleanupFailure(failure: unknown, cleanup: () => Promise<unknown>): Promise<unknown> {
  try {
    await cleanup();
    return failure;
  } catch (cleanupError) {
    return failure === undefined
      ? cleanupError
      : new AggregateError([failure, cleanupError], "fixture operation and cleanup both failed");
  }
}

async function openLockOwner(lockPath: string): Promise<FileHandle> {
  const descriptor = await open(lockPath, "r+");
  const owner = spawnChild("/usr/bin/lockf", ["-s", "-t", "0", "3"], {
    stdio: ["ignore", "ignore", "ignore", descriptor.fd],
  });
  const [code] = await once(owner, "exit");
  if (code !== 0) {
    await descriptor.close();
    throw new Error("lock owner failed");
  }
  return descriptor;
}

async function contendForLock(lockPath: string): Promise<number | null> {
  const contender = spawnChild("/usr/bin/lockf", ["-s", "-k", "-t", "0", lockPath, "/usr/bin/true"], {
    stdio: ["ignore", "ignore", "ignore"],
  });
  const [code] = await once(contender, "exit");
  return code;
}

class FakeStdin extends EventEmitter {
  readonly frames: Uint8Array[] = [];
  destroyed = false;
  private heldCallback: ((error: Error | null | undefined) => void) | undefined;
  private readonly writeStarted: Promise<void>;
  private markWriteStarted!: () => void;

  constructor(private readonly holdWrites: boolean) {
    super();
    this.writeStarted = new Promise<void>((resolve) => { this.markWriteStarted = resolve; });
  }

  destroy(): this {
    this.destroyed = true;
    queueMicrotask(() => this.emit("close"));
    return this;
  }

  end(): this {
    return this;
  }

  write(frame: Uint8Array, callback: (error: Error | null | undefined) => void): boolean {
    this.frames.push(frame);
    this.markWriteStarted();
    if (this.holdWrites) this.heldCallback = callback;
    else callback(undefined);
    return true;
  }

  waitForWrite(): Promise<void> {
    return this.writeStarted;
  }

  completeWrite(error?: Error): void {
    assert.ok(this.heldCallback, "no held write callback");
    const callback = this.heldCallback;
    this.heldCallback = undefined;
    callback(error);
  }
}

class FakeReadable extends EventEmitter {
  destroyed = false;

  destroy(): this {
    this.destroyed = true;
    queueMicrotask(() => this.emit("close"));
    return this;
  }
}

function decodeWrittenFrame(frame: Uint8Array): unknown {
  const view = Buffer.from(frame);
  const length = view.readUInt32BE(0);
  return JSON.parse(view.subarray(4, 4 + length).toString("utf8"));
}
