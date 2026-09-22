// The installed final-frame reopen proof. Nothing here imports `../src`: the oracle
// must run against the packaged tarball and the packaged gateway, so this file only
// installs that tarball into a fresh consumer directory outside the repository, runs
// `scripts/installed-final-frame-reopen.mjs` there, and asserts the recorded wire and
// reopen evidence. Set RUNTIME14_INSTALLED_ARTIFACTS to the artifacts directory that
// holds `stage/conversation-runtime-0.1.0.tgz` and
// `target/debug/conversation-runtime-gateway`.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { copyFile, mkdtemp, mkdir, readFile, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const run = promisify(execFile);
// This file executes as `dist/test/…`, so the package root is two levels up.
const DRIVER = fileURLToPath(new URL("../../scripts/installed-final-frame-reopen.mjs", import.meta.url));
const ENV_VAR = "RUNTIME14_INSTALLED_ARTIFACTS";
const TARBALL = "stage/conversation-runtime-0.1.0.tgz";
const GATEWAY = "target/debug/conversation-runtime-gateway";
const SCENARIO_TIMEOUT_MS = 180_000;
/** The server's own capture-finish budget. This proof never widens it. */
const SERVER_FINISH_BUDGET_MS = 10_000;

interface FrameEntry {
  readonly kind: string;
  readonly mediaStartSample: string;
  readonly mediaEndSample: string;
  readonly sequence: string | null;
  readonly captureStatus: string;
  readonly reasons: readonly string[];
  readonly wall: { readonly localStartNs: string; readonly localEndNs: string } | null;
}

interface LeaseProbe {
  readonly reauthenticated: boolean;
  readonly resumedCaptureAcknowledged: boolean;
  readonly authenticationError?: string;
  readonly resumedCaptureError?: string;
}

interface ScenarioResult {
  readonly scenario: string;
  readonly failure?: string;
  readonly lease: { readonly mediaProtocolVersion: number; readonly captureFormat: string; readonly timingContract: string; readonly sessionId: string };
  readonly declaredBoundary: Record<string, unknown>;
  readonly finish: Record<string, unknown>;
  readonly leaseAfterRejection?: LeaseProbe;
  readonly leaseAfterDuplicate?: LeaseProbe;
  readonly gatewayAfterBudget?: string;
  readonly stopResult: string;
  readonly recording: { readonly durationMs: string; readonly format: string; readonly gapCount: number };
  readonly firstCleanup: { readonly status: string };
  readonly secondCleanup: { readonly status: string };
  readonly reopen: { readonly newControlIdentity: string; readonly differsFromCaptureSession: boolean };
  readonly alignment: {
    readonly entries: readonly (FrameEntry | { readonly kind: string })[];
    readonly completion: { readonly pageCount: string; readonly coverageStatus: string; readonly warnings: readonly string[] } | null;
  };
  readonly providerTraffic: { readonly healthProbes: number; readonly providerRequests: readonly string[] };
  readonly lastFrameSpan: { readonly hostStartNs: string; readonly hostEndNs: string };
}

function artifacts(): { tarball: string; gateway: string } {
  const root = process.env[ENV_VAR];
  if (root === undefined || root.trim() === "") {
    throw new Error(
      `${ENV_VAR} is required: set it to the Runtime14 artifacts directory holding ${TARBALL} and ${GATEWAY}. This proof must run against the packaged tarball and the packaged gateway, so it cannot be skipped.`,
    );
  }
  const tarball = join(root, TARBALL);
  const gateway = join(root, GATEWAY);
  for (const path of [tarball, gateway]) {
    if (!existsSync(path)) throw new Error(`${ENV_VAR} does not contain ${path}`);
  }
  return { tarball, gateway };
}

let consumer: Promise<string> | undefined;

/** One consumer directory outside the repository, with the exact tarball installed. */
function installedConsumer(tarball: string): Promise<string> {
  consumer ??= (async () => {
    const directory = await mkdtemp(join(tmpdir(), "runtime14-installed-reopen-"));
    await writeFile(
      join(directory, "package.json"),
      `${JSON.stringify({ name: "runtime14-installed-reopen-consumer", private: true, version: "0.0.0", type: "module" }, null, 2)}\n`,
    );
    await run("npm", ["install", "--no-audit", "--no-fund", "--ignore-scripts", tarball], { cwd: directory });
    await copyFile(DRIVER, join(directory, "driver.mjs"));
    return directory;
  })();
  return consumer;
}

async function scenario(name: string): Promise<ScenarioResult> {
  const { tarball, gateway } = artifacts();
  const directory = await installedConsumer(tarball);
  const runDir = join(directory, name);
  await mkdir(runDir, { recursive: true });
  const optionsPath = join(runDir, "options.json");
  const resultPath = join(runDir, "result.json");
  await writeFile(optionsPath, JSON.stringify({ scenario: name, gatewayPath: gateway, runDir, resultPath }));
  let failure: unknown;
  try {
    await run(process.execPath, [join(directory, "driver.mjs"), optionsPath], { cwd: directory, timeout: SCENARIO_TIMEOUT_MS });
  } catch (error) {
    failure = error;
  }
  if (!existsSync(resultPath)) throw failure ?? new Error(`${name} produced no result`);
  const result = JSON.parse(await readFile(resultPath, "utf8")) as ScenarioResult;
  if (result.failure !== undefined) throw new Error(`${name} failed: ${result.failure}`);
  return result;
}

function frames(result: ScenarioResult): readonly FrameEntry[] {
  return result.alignment.entries.filter((entry): entry is FrameEntry => entry.kind === "frame");
}

/** Every scenario shares this: a v3 lease, a real reopen, and no provider traffic. */
function assertInstalledShape(result: ScenarioResult): void {
  assert.equal(result.lease.mediaProtocolVersion, 3);
  assert.equal(result.lease.captureFormat, "pcm_s16le/24000/1/20ms");
  assert.equal(result.lease.timingContract, "capture-clock/v1");
  assert.equal(result.stopResult, "stopped");
  assert.equal(result.firstCleanup.status, "graceful");
  assert.equal(result.secondCleanup.status, "graceful");
  assert.equal(result.reopen.differsFromCaptureSession, true);
  assert.deepEqual(result.providerTraffic.providerRequests, [], "timed capture must not reach any provider endpoint");
  assert.equal(result.providerTraffic.healthProbes, 2, "one readiness probe per gateway process");
}

/** The four captured frames survive to the reopened recording with exact sample ranges. */
function assertRetainedPcm(result: ScenarioResult): void {
  assert.deepEqual(
    frames(result)
      .slice(0, 4)
      .map((frame) => [frame.sequence, frame.mediaStartSample, frame.mediaEndSample]),
    [
      ["1", "0", "2"],
      ["2", "2", "4"],
      ["3", "4", "6"],
      ["4", "6", "8"],
    ],
  );
}

test("the installed SDK reopens the packaged gateway's last captured frame with its retained wall mapping", { timeout: SCENARIO_TIMEOUT_MS }, async () => {
  const result = await scenario("nominal");
  assertInstalledShape(result);
  assert.equal(result.finish.outcome, "acknowledged");
  assert.equal(result.finish.ackBytes, 51);
  assert.equal(result.finish.echoesDeclaredBoundary, true);
  assert.equal(result.finish.totalAckPackets, 1);
  assert.equal(result.finish.bytesAfterEof, 0, "nothing followed the acknowledgement");
  assertRetainedPcm(result);
  assert.equal(frames(result).length, 4, "no frame exists beyond the acknowledged boundary");

  const last = frames(result)[3]!;
  assert.equal(last.captureStatus, "available");
  assert.deepEqual(last.reasons, []);
  assert.notEqual(last.wall, null);
  // The delayed final clock pair is the only interval that can bracket this frame.
  assert.equal(last.wall!.localStartNs, "2100250000");
  assert.equal(last.wall!.localEndNs, "2100333334");
  assert.equal(result.lastFrameSpan.hostEndNs, "1000333334");
  // Planned completion: no synthesized reconnect or disconnect evidence.
  assert.equal(result.alignment.completion?.coverageStatus, "available");
  assert.deepEqual(result.alignment.completion?.warnings, ["local_wall_timestamp_estimate"]);
});

test("an unread finish acknowledgement ends bounded and unconfirmed with one acknowledgement and retained PCM", { timeout: SCENARIO_TIMEOUT_MS }, async () => {
  const result = await scenario("unread-ack");
  assertInstalledShape(result);
  assert.equal(result.finish.outcome, "unconfirmed");
  assert.equal(result.finish.producerReadAcknowledgement, false);
  assert.equal(result.finish.producerSentEof, false);
  assert.equal(result.finish.ackPacketCount, 1, "exactly one acknowledgement ever reached the producer");
  assert.equal(result.finish.bytesDelivered, 51);
  assert.ok(
    Number(result.finish.waitedMs) > SERVER_FINISH_BUDGET_MS,
    `the producer waited past the server's own ${SERVER_FINISH_BUDGET_MS} ms budget`,
  );
  // The gateway is still serving control after its own budget expired.
  assert.equal(result.gatewayAfterBudget, "stdio");
  assertRetainedPcm(result);
  // Unplanned close, so the recording carries the discontinuity rather than a clean coverage.
  assert.equal(result.alignment.completion?.coverageStatus, "partial");
  assert.ok(result.alignment.completion?.warnings.includes("reconnect"));
});

test("a mismatched finish is refused without an acknowledgement and leaves stop and the recording intact", { timeout: SCENARIO_TIMEOUT_MS }, async () => {
  const result = await scenario("mismatched-finish");
  assertInstalledShape(result);
  assert.equal(result.finish.outcome, "rejected");
  assert.equal(result.finish.ackPacketCount, 0, "a mismatched boundary is never acknowledged");
  assert.equal(result.finish.bytesDelivered, 0);
  assertRetainedPcm(result);
  // Observed gateway behaviour, reported rather than patched: a refused boundary does
  // not mark the lease finished, so the producer reconnects on the same credential,
  // resumes capture and earns an acknowledgement for the later boundary. The brief
  // expected the lease to be dead for media here; only an accepted finish does that.
  assert.equal(result.leaseAfterRejection?.reauthenticated, true);
  assert.equal(result.leaseAfterRejection?.resumedCaptureAcknowledged, true);
  assert.equal(frames(result).length, 5, "the resumed frame is recorded after the refused finish");
});

test("a duplicate finish yields no second acknowledgement and leaves the lease dead for media", { timeout: SCENARIO_TIMEOUT_MS }, async () => {
  const result = await scenario("duplicate-finish");
  assertInstalledShape(result);
  assert.equal(result.finish.outcome, "acknowledged");
  assert.equal(result.finish.echoesDeclaredBoundary, true);
  assert.equal(result.finish.bytesAfterDuplicate, 0);
  assert.equal(result.finish.totalAckPackets, 1);
  assert.equal(result.leaseAfterDuplicate?.reauthenticated, false, "an accepted finish closes the lease to media");
  assert.equal(result.leaseAfterDuplicate?.resumedCaptureAcknowledged, false);
  assertRetainedPcm(result);
  assert.equal(frames(result).length, 4, "no frame extends the acknowledged boundary");
});

test("a withheld final clock pair leaves the last frame unmapped while PCM and the acknowledgement stand", { timeout: SCENARIO_TIMEOUT_MS }, async () => {
  const result = await scenario("missing-final-pair");
  assertInstalledShape(result);
  assert.equal(result.finish.outcome, "acknowledged");
  assert.equal(result.finish.echoesDeclaredBoundary, true);
  assertRetainedPcm(result);
  const last = frames(result)[3]!;
  assert.equal(last.wall, null, "no wall time may be invented for an unbracketed final frame");
  assert.equal(last.captureStatus, "available", "the frame itself stays captured");
  assert.deepEqual(last.reasons, ["wall_alignment_missing", "missing_clock_pair"]);
  assert.equal(result.alignment.completion?.coverageStatus, "partial");
});

test("a final clock pair beyond the two-second gap bound leaves the last frame unmapped", { timeout: SCENARIO_TIMEOUT_MS }, async () => {
  const result = await scenario("pair-gap");
  assertInstalledShape(result);
  assert.equal(result.finish.outcome, "acknowledged");
  assert.equal(result.finish.echoesDeclaredBoundary, true);
  assertRetainedPcm(result);
  const last = frames(result)[3]!;
  assert.equal(last.wall, null);
  assert.deepEqual(last.reasons, ["wall_alignment_missing", "clock_pair_gap"]);
  assert.equal(result.alignment.completion?.coverageStatus, "partial");
});
