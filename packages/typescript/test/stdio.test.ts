import assert from "node:assert/strict";
import { EventEmitter } from "node:events";
import { access, chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { setTimeout } from "node:timers/promises";

import { RuntimeClient } from "../src/client.js";
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

test("discards buffered ready and responses when the process fails", async () => {
  await withGateway(
    "emit({ type: 'ready', protocol_version: 1, status }); setTimeout(() => { writeFileSync(`${process.argv[3]}.exit`, ''); process.exit(1); }, 10);",
    async ({ gatewayPath, configPath }) => {
      const transport = await StdioGatewayTransport.start({ gatewayPath, configPath });
      await waitForFile(`${configPath}.exit`);
      await setTimeout(10);
      await assert.rejects(RuntimeClient.connect(transport), /gateway (stdout ended|process exited)/);
    },
  );
});

test("discards buffered status responses when exit follows", async () => {
  await withGateway(
    "emit({ type: 'ready', protocol_version: 1, status }); process.stdin.once('data', () => { emit({ type: 'command_accepted', protocol_version: 1, request_id: 'request-1' }); emit({ type: 'status', protocol_version: 1, request_id: 'request-1', status }); writeFileSync(`${process.argv[3]}.exit`, ''); process.exit(1); });",
    async ({ gatewayPath, configPath }) => {
      const transport = await StdioGatewayTransport.start({ gatewayPath, configPath });
      const iterator = transport.messages[Symbol.asyncIterator]();
      await iterator.next();
      await transport.send({ type: "status", requestId: "request-1" });
      await waitForFile(`${configPath}.exit`);
      await setTimeout(10);

      await assert.rejects(iterator.next(), /gateway (stdout ended|process exited)/);
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
const status = { transport: 'stdio', privacy_mode: 'local_only', language_location: 'local', model_id: 'local-model', memory_enabled: false, memory_location: null, telemetry_enabled: false, capabilities: ['text'] };
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

async function waitForFile(path: string): Promise<void> {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    try {
      await access(path);
      return;
    } catch {
      await setTimeout(10);
    }
  }
  throw new Error("child did not write its exit marker");
}

function startWithChild(child: FakeChild): StdioGatewayTransport {
  const testable = StdioGatewayTransport as unknown as {
    startWithChildForTest(child: unknown): StdioGatewayTransport;
  };
  return testable.startWithChildForTest(child);
}

function framedReady(): Buffer {
  const payload = Buffer.from(JSON.stringify({ type: "ready", protocol_version: 1, status: {
    transport: "stdio",
    privacy_mode: "local_only",
    language_location: "local",
    model_id: "local-model",
    memory_enabled: false,
    memory_location: null,
    telemetry_enabled: false,
    capabilities: ["text"],
  } }));
  const header = Buffer.alloc(4);
  header.writeUInt32BE(payload.length);
  return Buffer.concat([header, payload]);
}

class FakeChild extends EventEmitter {
  readonly stderr = new EventEmitter();
  readonly stdin = new FakeStdin();
  readonly stdout = new EventEmitter();
  exitCode: number | null = null;
  killCount = 0;

  kill(): boolean {
    if (this.exitCode !== null) {
      return false;
    }
    this.killCount += 1;
    this.exitCode = 1;
    queueMicrotask(() => this.emit("exit"));
    return true;
  }
}

class FakeStdin extends EventEmitter {
  destroy(): this {
    return this;
  }

  end(): this {
    return this;
  }

  write(_frame: Uint8Array, callback: (error: Error | null | undefined) => void): boolean {
    callback(undefined);
    return true;
  }
}
