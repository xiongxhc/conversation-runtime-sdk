import assert from "node:assert/strict";
import { access, chmod, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

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

async function withGateway(
  body: string,
  run: (paths: { gatewayPath: string; configPath: string; directory: string }) => Promise<void>,
): Promise<void> {
  const directory = await mkdtemp(join(tmpdir(), "conversation-runtime-"));
  const gatewayPath = join(directory, "fake-gateway.mjs");
  const configPath = join(directory, "gateway.toml");
  const program = `#!/usr/bin/env node
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
