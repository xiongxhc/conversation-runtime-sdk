import assert from "node:assert/strict";
import { execFileSync, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createServer, type Server } from "node:http";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { performance } from "node:perf_hooks";
import test, { type TestContext } from "node:test";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";

import { RuntimeClient } from "../src/client.js";
import { StdioGatewayTransport } from "../src/stdio.js";

const REPO_ROOT = fileURLToPath(new URL("../../../../", import.meta.url));
const GATEWAY_PATH = process.env.RUNTIME_TASK13_GATEWAY_FIXTURE_PATH
  ?? join(REPO_ROOT, "target/debug/conversation-runtime-gateway");
const TIMEOUTS = { eofGraceMs: 5_000, termGraceMs: 1_000, killGraceMs: 1_000 };
const COMPILED_READY_DEADLINE_MS = 2_000;
const COMPILED_BLOCKED_DEADLINE_MS = 2_500;

function gatewayConfig(port: number): string {
  return `schema_version = 1
privacy_mode = "local-only"

[language]
backend = "ollama-compatible"
execution = "local"
provider = "fixture-language"
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

test("private exec retains compiled gateway argv and keeps canary outside parent diagnostics", { timeout: 8_000 }, async (context) => {
  const directory = await mkdtemp(join(tmpdir(), "runtime-task13-exec-"));
  const executable = join(directory, "private-exec");
  const configPath = join(directory, "gateway.toml");
  const syntheticKey = join(directory, "key.txt");
  const observedPath = join(directory, "observed.txt");
  await chmod(directory, 0o700);
  await writeFile(syntheticKey, "synthetic-runtime-secret", { mode: 0o600 });
  await writeFile(configPath, gatewayConfig(1));
  await writeFile(executable, `#!/bin/sh
test -r "${syntheticKey}" || exit 90
RUNTIME_TASK13_EXEC_CANARY=$(cat "${syntheticKey}")
export RUNTIME_TASK13_EXEC_CANARY
test "$RUNTIME_TASK13_EXEC_CANARY" = "synthetic-runtime-secret" || exit 91
printf '%s yes %s\\n' "$$" "$*" > "${observedPath}"
exec "${GATEWAY_PATH}" "$@"
`, { mode: 0o700 });
  let failure: unknown;
  try {
    await runOwnedGatewayFixture(context, COMPILED_READY_DEADLINE_MS, async (owner) => {
      const transport = await owner.start({
        gatewayPath: executable,
        configPath,
        protocolVersion: 4,
        env: { PATH: "/usr/bin:/bin" },
        cleanupTimeouts: TIMEOUTS,
      });
      const client = await owner.connect(4);
      assert.equal((await client.status()).transport, "stdio");
      const observed = await readFile(observedPath, "utf8");
      const pid = Number(observed.split(" ")[0]);
      assert.ok(Number.isInteger(pid) && pid > 0);
      assert.ok(execFileSync("/bin/ps", ["-p", String(pid), "-o", "comm="], { encoding: "utf8" }).includes("conversation-runtime-gateway"));
      assert.ok(observed.includes(" yes "));
      assert.ok(observed.includes(`--config ${configPath} --client-protocol-version 4`));
      assert.equal(observed.includes("synthetic-runtime-secret"), false);
      await owner.close();
      assert.deepEqual(transport.cleanupOutcome, { status: "graceful" });
    });
  } catch (error) {
    failure = error;
  } finally {
    failure = await retainCleanupFailure(failure, () => rm(directory, { recursive: true, force: true }));
  }
  if (failure !== undefined) throw failure;
});

test("compiled gateway cancels blocked work and closes inside selected EOF grace", { timeout: 10_000 }, async (context) => {
  const directory = await mkdtemp(join(tmpdir(), "runtime-task13-eof-"));
  let server: Server | undefined;
  let failure: unknown;
  try {
    let requestStarted!: () => void;
    const started = new Promise<void>((resolve) => { requestStarted = resolve; });
    server = createServer((_request, response) => {
      response.writeHead(200, { "Content-Type": "text/event-stream" });
      response.write("data: ");
      requestStarted();
    });
    await new Promise<void>((resolve) => server!.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    assert.ok(address && typeof address === "object");
    const configPath = join(directory, "gateway.toml");
    await writeFile(configPath, gatewayConfig(address.port));
    await runOwnedGatewayFixture(context, COMPILED_BLOCKED_DEADLINE_MS, async (owner) => {
      const transport = await owner.start({ gatewayPath: GATEWAY_PATH, configPath, cleanupTimeouts: TIMEOUTS });
      const client = await owner.connect();
      const turn = await client.startTurn("fixture blocked operation");
      const events = turn.events[Symbol.asyncIterator]();
      await events.next();
      await Promise.race([started, sleep(3_000).then(() => { throw new Error("provider request did not start"); })]);
      const closedAt = performance.now();
      await owner.close();
      assert.ok(performance.now() - closedAt < TIMEOUTS.eofGraceMs);
      assert.deepEqual(transport.cleanupOutcome, { status: "graceful" });
    });
  } catch (error) {
    failure = error;
  } finally {
    server?.closeAllConnections();
    failure = await retainCleanupFailure(failure, async () => {
      if (server) await new Promise<void>((resolve, reject) => server!.close((error) => error ? reject(error) : resolve()));
    });
    failure = await retainCleanupFailure(failure, () => rm(directory, { recursive: true, force: true }));
  }
  if (failure !== undefined) throw failure;
});

test("compiled fixture reaps its owned child after an injected post-spawn failure", { timeout: 8_000 }, async (context) => {
  const directory = await mkdtemp(join(tmpdir(), "runtime-task13-failure-"));
  const executable = join(directory, "private-exec");
  const configPath = join(directory, "gateway.toml");
  const observedPath = join(directory, "pid.txt");
  await writeFile(configPath, gatewayConfig(1));
  await writeFile(executable, `#!/bin/sh
printf '%s\\n' "$$" > "${observedPath}"
exec "${GATEWAY_PATH}" "$@"
`, { mode: 0o700 });
  let transport: StdioGatewayTransport | undefined;
  let pid: number | undefined;

  const exercise = async (): Promise<void> => {
    await runOwnedGatewayFixture(context, COMPILED_READY_DEADLINE_MS, async (owner) => {
      transport = await owner.start({
        gatewayPath: executable,
        configPath,
        cleanupTimeouts: TIMEOUTS,
      });
      await owner.connect();
      pid = Number(await readFile(observedPath, "utf8"));
      assert.ok(Number.isInteger(pid) && pid > 0);
      throw new Error("injected post-spawn fixture failure");
    });
  };

  let failure: unknown;
  try {
    await assert.rejects(exercise(), /injected post-spawn fixture failure/);
    assert.ok(transport);
    assert.notEqual(transport.cleanupOutcome?.status, "failed");
    assert.ok(pid);
    await waitForPidExit(pid);
  } catch (error) {
    failure = error;
  } finally {
    failure = await retainCleanupFailure(failure, () => rm(directory, { recursive: true, force: true }));
  }
  if (failure !== undefined) throw failure;
});

test("compiled fixture deadline settles no-ready connect and reaps owned child before file removal", { timeout: 2_000 }, async (context) => {
  const directory = await mkdtemp(join(tmpdir(), "runtime-task13-no-ready-"));
  const executable = join(directory, "no-ready");
  const configPath = join(directory, "gateway.toml");
  await writeFile(configPath, gatewayConfig(1));
  await writeFile(executable, `#!/bin/sh
exec /bin/sleep 60
`, { mode: 0o700 });
  let transport: StdioGatewayTransport | undefined;
  let connectAttempt: Promise<RuntimeClient> | undefined;
  let pid: number | undefined;
  let connectSettled = false;
  let operationSettled = false;
  let failure: unknown;
  try {
    await assert.rejects(
      runOwnedGatewayFixture(context, 1_500, async (owner) => {
        try {
          transport = await owner.start({
            gatewayPath: executable,
            configPath,
            cleanupTimeouts: { eofGraceMs: 50, termGraceMs: 50, killGraceMs: 200 },
          });
          pid = ownedChildPidForTest(transport);
          process.kill(pid, 0);
          connectAttempt = owner.connect();
          void connectAttempt.finally(() => { connectSettled = true; }).catch(() => undefined);
          await sleep(25);
          assert.equal(connectSettled, false);
          await connectAttempt;
        } finally {
          operationSettled = true;
        }
      }),
      (error: unknown) => {
        assert.ok(error instanceof AggregateError);
        assert.match(String(error.errors[0]), /fixture operation exceeded 1500 ms/);
        return true;
      },
    );
    assert.equal(operationSettled, true);
    assert.ok(transport);
    assert.notEqual(transport.cleanupOutcome?.status, "failed");
    assert.ok(pid);
    await waitForPidExit(pid);
  } catch (error) {
    failure = error;
  } finally {
    failure = await retainCleanupFailure(failure, async () => {
      if (transport) await transport.close();
      await connectAttempt?.catch(() => undefined);
    });
    failure = await retainCleanupFailure(failure, () => rm(directory, { recursive: true, force: true }));
  }
  if (failure !== undefined) throw failure;
});

class CompiledGatewayFixtureOwner {
  private readonly startupAbortController = new AbortController();
  private startPromise: Promise<StdioGatewayTransport> | undefined;
  private transport: StdioGatewayTransport | undefined;
  private client: RuntimeClient | undefined;
  private closePromise: Promise<void> | undefined;

  async start(options: Parameters<typeof StdioGatewayTransport.start>[0]): Promise<StdioGatewayTransport> {
    if (this.startPromise) throw new Error("compiled gateway fixture owner already started");
    if (this.startupAbortController.signal.aborted) throw fixtureAbortReason(this.startupAbortController.signal);
    this.startPromise = StdioGatewayTransport.start({
      ...options,
      startupAbortSignal: this.startupAbortController.signal,
    });
    this.transport = await this.startPromise;
    return this.transport;
  }

  async connect(protocolVersion?: 1 | 2 | 3 | 4): Promise<RuntimeClient> {
    const transport = this.transport ?? await this.startPromise;
    assert.ok(transport);
    this.client = protocolVersion === undefined
      ? await RuntimeClient.connect(transport)
      : await RuntimeClient.connect(transport, protocolVersion);
    if (this.startupAbortController.signal.aborted) {
      await this.close();
      throw fixtureAbortReason(this.startupAbortController.signal);
    }
    return this.client;
  }

  cancel(reason: unknown): void {
    if (!this.startupAbortController.signal.aborted) this.startupAbortController.abort(reason);
    void this.close().catch(() => undefined);
  }

  close(): Promise<void> {
    this.closePromise ??= this.closeOwnedGateway();
    return this.closePromise;
  }

  private async closeOwnedGateway(): Promise<void> {
    if (!this.startupAbortController.signal.aborted) {
      this.startupAbortController.abort(new Error("compiled gateway fixture owner closed"));
    }
    let transport = this.transport;
    if (!transport && this.startPromise) {
      try {
        transport = await this.startPromise;
      } catch {
        return;
      }
    }
    if (this.client) await this.client.close();
    else if (transport) await transport.close();
  }
}

async function runOwnedGatewayFixture(
  context: TestContext,
  deadlineMs: number,
  body: (owner: CompiledGatewayFixtureOwner) => Promise<void>,
): Promise<void> {
  const owner = new CompiledGatewayFixtureOwner();
  type FixtureOutcome =
    | { kind: "body-complete" }
    | { kind: "body-failed"; error: unknown }
    | { kind: "cancelled"; error: unknown };
  const bodyOutcome: Promise<FixtureOutcome> = Promise.resolve()
    .then(() => body(owner))
    .then(
      () => ({ kind: "body-complete" }),
      (error: unknown) => ({ kind: "body-failed", error }),
    );
  let resolveCancellation!: (outcome: FixtureOutcome) => void;
  const cancellationOutcome = new Promise<FixtureOutcome>((resolve) => { resolveCancellation = resolve; });
  let cancellationRequested = false;
  const requestCancellation = (error: unknown): void => {
    if (cancellationRequested) return;
    cancellationRequested = true;
    owner.cancel(error);
    resolveCancellation({ kind: "cancelled", error });
  };
  const onTestAbort = (): void => {
    requestCancellation(fixtureAbortReason(context.signal));
  };
  context.signal.addEventListener("abort", onTestAbort, { once: true });
  if (context.signal.aborted) onTestAbort();
  const deadline = setTimeout(() => {
    requestCancellation(new Error(`fixture operation exceeded ${deadlineMs} ms`));
  }, deadlineMs);

  const firstOutcome = await Promise.race([bodyOutcome, cancellationOutcome]);
  clearTimeout(deadline);
  context.signal.removeEventListener("abort", onTestAbort);
  let failure = firstOutcome.kind === "body-complete" ? undefined : firstOutcome.error;
  failure = await retainCleanupFailure(failure, () => owner.close());
  if (firstOutcome.kind === "cancelled") {
    failure = await retainCleanupFailure(failure, async () => {
      const settledBody = await bodyOutcome;
      if (settledBody.kind === "body-failed") throw settledBody.error;
    });
  }
  if (failure !== undefined) throw failure;
}

function fixtureAbortReason(signal: AbortSignal): unknown {
  return signal.reason ?? new Error("compiled gateway fixture cancelled");
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

async function waitForPidExit(pid: number): Promise<void> {
  const deadline = performance.now() + 1_000;
  while (performance.now() < deadline) {
    try {
      process.kill(pid, 0);
    } catch {
      return;
    }
    await sleep(10);
  }
  throw new Error("fixture-owned gateway was not reaped");
}

function ownedChildPidForTest(transport: StdioGatewayTransport): number {
  const child = (transport as unknown as { readonly child: ChildProcessWithoutNullStreams }).child;
  assert.ok(Number.isInteger(child.pid) && child.pid! > 0);
  return child.pid!;
}
