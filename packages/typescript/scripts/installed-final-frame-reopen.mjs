// Consumer driver for the installed final-frame reopen proof. It is copied into a
// fresh directory outside the repository that has `npm install`ed the packaged
// tarball, so every import below resolves to the installed package and never to
// `../src`. It drives the packaged gateway binary over stdio and a real media-v3
// producer over TCP, then reopens the finalized recording through its authenticated
// capability in a second gateway process. Every recorded field comes from a wire
// read, a gateway message, or a clock; nothing is inferred.
import { mkdir, writeFile, readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { connect } from "node:net";
import { join } from "node:path";
import { performance } from "node:perf_hooks";

import { RuntimeClient, StdioGatewayTransport } from "@conversation/runtime";

const KEY_ENV = "RUNTIME14_INSTALLED_REOPEN_SYNTHETIC_KEY";
const KEY = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const BRIDGE = "bridge-independent";
const DOMAIN = "source-process";
const RATE_HZ = 24_000;
const SOURCE_RATE_HZ = 48_000;
const SAMPLES_PER_FRAME = 2;
const FRAME_COUNT = 4;
const PCM = Buffer.from("0000ff7f", "hex");
const FIRST_PAIR_HOST_NS = 900_000_000n;
const FIRST_PAIR_WALL_NS = 2_000_000_000n;
const FINAL_PAIR_HOST_NS = 1_000_400_000n;
const WIDE_GAP_PAIR_HOST_NS = 3_500_000_000n;
const FINISH_KIND = 10;
const ACK_PREFIX_HEX = "4d454433030b";
const ACK_BYTES = 51;
const CONFIRMATION_BYTES = 10 + 8 + 1 + BRIDGE.length;
const CLEANUP_TIMEOUTS = { eofGraceMs: 5_000, termGraceMs: 1_000, killGraceMs: 1_000 };

// --- media-v3 wire -------------------------------------------------------------

const u64 = (value) => {
  const bytes = Buffer.alloc(8);
  bytes.writeBigUInt64BE(BigInt(value));
  return bytes;
};
const i64 = (value) => {
  const bytes = Buffer.alloc(8);
  bytes.writeBigInt64BE(BigInt(value));
  return bytes;
};
const u32 = (value) => {
  const bytes = Buffer.alloc(4);
  bytes.writeUInt32BE(value);
  return bytes;
};
const u16 = (value) => {
  const bytes = Buffer.alloc(2);
  bytes.writeUInt16BE(value);
  return bytes;
};
const id = (text) => Buffer.concat([Buffer.from([Buffer.byteLength(text)]), Buffer.from(text, "utf8")]);
const med3 = (kind, body) =>
  Buffer.concat([Buffer.from("MED3", "ascii"), Buffer.from([3, kind]), u32(body.length), body]);

const encodeEpoch = (captureEpoch) =>
  Buffer.concat([id(BRIDGE), id(DOMAIN), u64(captureEpoch), Buffer.from([1])]);

const encodePair = ({ sequence, hostNs, wallNs }) =>
  Buffer.concat([id(DOMAIN), u64(1), u64(sequence), u64(hostNs), i64(wallNs), u64(hostNs), u64(1), Buffer.from([0]), u64(0)]);

/** The exact 88-byte capture timing record: version 1, span present, no uncertainty. */
function encodeProvenance({ sequence, outputStartSample, hostStartNs, hostEndNs }) {
  const bytes = Buffer.alloc(88);
  bytes[0] = 1;
  bytes[1] = 1;
  bytes[2] = 1; // nominal_frame_endpoint_interpolation
  bytes.writeBigUInt64BE(1n, 4);
  bytes.writeBigUInt64BE(BigInt(sequence), 12);
  bytes.writeBigUInt64BE(BigInt(outputStartSample), 20);
  bytes.writeUInt32BE(SAMPLES_PER_FRAME, 28);
  bytes.writeBigUInt64BE(BigInt(outputStartSample) * BigInt(SOURCE_RATE_HZ), 32);
  bytes.writeBigUInt64BE((BigInt(outputStartSample) + BigInt(SAMPLES_PER_FRAME)) * BigInt(SOURCE_RATE_HZ), 40);
  bytes.writeUInt32BE(RATE_HZ, 48);
  bytes.writeUInt32BE(SOURCE_RATE_HZ, 52);
  bytes.writeBigUInt64BE(BigInt(hostStartNs), 56);
  bytes.writeBigUInt64BE(BigInt(hostEndNs), 64);
  bytes.writeBigUInt64BE(4n, 72);
  return bytes;
}

const finishBody = ({ sessionId, captureEpoch, framePresent, sequence, outputEnd, pairSequence }) =>
  Buffer.concat([
    u64(sessionId),
    u64(captureEpoch),
    Buffer.from([framePresent ? 1 : 0]),
    u64(sequence),
    u64(outputEnd),
    u64(pairSequence),
  ]);

/** The nominal host span of frame `index`, in the recorder's own integer arithmetic. */
function frameSpan(index) {
  const nanos = BigInt(SAMPLES_PER_FRAME) * 1_000_000_000n;
  const rate = BigInt(RATE_HZ);
  const endNumerator = BigInt(index + 1) * nanos;
  const divided = endNumerator / rate;
  return {
    hostStartNs: 1_000_000_000n + (BigInt(index) * nanos) / rate,
    hostEndNs: 1_000_000_000n + (endNumerator % rate === 0n ? divided : divided + 1n),
  };
}

function countAcks(bytes) {
  let count = 0;
  for (let offset = 0; offset + ACK_BYTES <= bytes.length; ) {
    if (bytes.subarray(offset, offset + 6).toString("hex") === ACK_PREFIX_HEX) {
      count += 1;
      offset += ACK_BYTES;
    } else offset += 1;
  }
  return count;
}

// --- producer socket -----------------------------------------------------------

/** One owned media producer. `allowHalfOpen` keeps EOF under the producer's control. */
class Producer {
  #socket;
  #buffer = Buffer.alloc(0);
  #waiters = [];
  #finished = false;
  #closed = false;
  #error;

  constructor(socket) {
    this.#socket = socket;
    socket.on("data", (chunk) => {
      this.#buffer = Buffer.concat([this.#buffer, chunk]);
      this.#wake();
    });
    socket.on("end", () => {
      this.#finished = true;
      this.#wake();
    });
    socket.on("close", () => {
      this.#finished = true;
      this.#closed = true;
      this.#wake();
    });
    socket.on("error", (error) => {
      this.#error = error;
      this.#wake();
    });
  }

  static connect(endpoint) {
    const [host, port] = endpoint.replace("tcp://", "").split(":");
    return new Promise((resolve, reject) => {
      const socket = connect({ host, port: Number(port), allowHalfOpen: true }, () => resolve(new Producer(socket)));
      socket.once("error", reject);
    });
  }

  #wake() {
    const waiters = this.#waiters;
    this.#waiters = [];
    for (const waiter of waiters) waiter();
  }

  #sleep(remaining, capMs) {
    return new Promise((resolve) => {
      const timer = setTimeout(resolve, Math.min(remaining, capMs));
      this.#waiters.push(() => {
        clearTimeout(timer);
        resolve();
      });
    });
  }

  get received() {
    return Buffer.from(this.#buffer);
  }
  get socketError() {
    return this.#error === undefined ? null : (this.#error.code ?? this.#error.message);
  }

  write(bytes) {
    return new Promise((resolve, reject) => {
      this.#socket.write(bytes, (error) => (error ? reject(error) : resolve()));
    });
  }

  /** Reads exactly `length` bytes, or fails when the peer finishes or the deadline passes. */
  async readExact(length, deadlineMs) {
    const expiry = performance.now() + deadlineMs;
    while (this.#buffer.length < length) {
      if (this.#finished) throw new Error(`peer finished after ${this.#buffer.length} of ${length} bytes`);
      const remaining = expiry - performance.now();
      if (remaining <= 0) throw new Error(`read of ${length} bytes exceeded ${deadlineMs} ms`);
      await this.#sleep(remaining, 25);
    }
    const taken = Buffer.from(this.#buffer.subarray(0, length));
    this.#buffer = this.#buffer.subarray(length);
    return taken;
  }

  /** Waits for the peer's FIN (a half-close counts), or fails at the deadline. */
  async awaitPeerFin(deadlineMs) {
    const expiry = performance.now() + deadlineMs;
    while (!this.#finished) {
      const remaining = expiry - performance.now();
      if (remaining <= 0) throw new Error(`peer kept its write side open for ${deadlineMs} ms`);
      await this.#sleep(remaining, 50);
    }
  }

  /** Waits for a full socket close, which a half-close alone does not satisfy. */
  async awaitClose(deadlineMs) {
    const expiry = performance.now() + deadlineMs;
    while (!this.#closed) {
      const remaining = expiry - performance.now();
      if (remaining <= 0) throw new Error(`connection stayed open for ${deadlineMs} ms`);
      await this.#sleep(remaining, 50);
    }
  }

  end() {
    this.#socket.end();
  }
  destroy() {
    this.#socket.destroy();
  }
}

async function authenticate(producer, sessionId, credential) {
  const token = Buffer.from(credential, "utf8");
  await producer.write(med3(1, Buffer.concat([u64(sessionId), Buffer.from([token.length]), token, id(BRIDGE)])));
  const confirmation = await producer.readExact(CONFIRMATION_BYTES, 5_000);
  const expected = Buffer.concat([
    Buffer.from("MED3", "ascii"),
    Buffer.from([3, 9]),
    u32(8 + 1 + BRIDGE.length),
    u64(sessionId),
    id(BRIDGE),
  ]);
  if (!confirmation.equals(expected)) throw new Error("media authentication confirmation did not match");
}

function frameRecord(sessionId, index) {
  const timing = encodeProvenance({
    sequence: index + 1,
    outputStartSample: index * SAMPLES_PER_FRAME,
    ...frameSpan(index),
  });
  return med3(
    2,
    Buffer.concat([u64(sessionId), Buffer.from([1]), u32(RATE_HZ), u16(1), timing, u32(PCM.length), PCM]),
  );
}

// --- gateway fixtures ----------------------------------------------------------

/**
 * A loopback provider endpoint. The gateway probes `/health` at startup; timed
 * capture must never reach any other path.
 */
async function startLanguageFixture() {
  const healthProbes = [];
  const providerRequests = [];
  const server = createServer((request, response) => {
    const url = request.url ?? "";
    (url === "/health" ? healthProbes : providerRequests).push(url);
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({ model: "fixture-model", response: "{}", done: true }));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    endpoint: `http://127.0.0.1:${server.address().port}`,
    healthProbes,
    providerRequests,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

const offlineConfig = ({ languageEndpoint, modelCache, recordings }) => `schema_version = 3
profile = "offline"
privacy_mode = "local-only"

[[provider_hosts]]
id = "fixture"
ownership = "external"
readiness_url = "${languageEndpoint}/health"
startup_timeout_ms = 5000
environment = "inherit"

[language]
backend = "ollama-compatible"
execution = "local"
provider = "fixture-language"
provider_host = "fixture"
endpoint = "${languageEndpoint}"
model = "fixture-model"
thinking = false
temperature = 0.0
seed = 1
num_predict = 128
num_ctx = 1024
max_assistant_content_bytes = 65536

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

[voice.capture]
device = "system-default"

[voice.turn]
speech_start_ms = 200
final_silence_ms = 600

[voice.asr]
backend = "whisperkit"
execution = "local"
provider = "fixture-asr"
model_path = "${modelCache}"
download = false

[voice.speech]
backend = "openai-compatible"
execution = "local"
provider = "fixture-speech"
provider_host = "fixture"
mode = "buffered"
endpoint = "${languageEndpoint}/v1"
model = "fixture"
voice = "fixture"
speed = 1.0
language = "auto"
instructions = "fixture"
max_tokens = 128
repetition_penalty = 1.0
max_text_bytes = 4096
max_audio_bytes = 8388608

[voice.audio]
backend = "managed-sidecar"
execution = "local"
provider = "fixture-audio"
sidecar_executable = "/bin/sh"
max_error_bytes = 65536

[recording]
directory = "${recordings}"
key_env = "${KEY_ENV}"

[media]
capture_format = "pcm_s16le/24000/1/20ms"
bind = "127.0.0.1:0"
advertised_endpoint = "tcp://127.0.0.1:0"
credential_lifetime_ms = 60000
reconnect_window_ms = 10000
max_frames_per_second = 100
max_payload_bytes = 16384
max_outbound_queue_bytes = 262144
max_outbound_queue_ms = 10000
`;

async function startGateway(gatewayPath, configPath) {
  const transport = await StdioGatewayTransport.start({
    gatewayPath,
    configPath,
    protocolVersion: 4,
    env: { PATH: "/usr/bin:/bin", [KEY_ENV]: KEY },
    cleanupTimeouts: CLEANUP_TIMEOUTS,
  });
  const client = await RuntimeClient.connect(transport, 4);
  return { transport, client };
}

// --- alignment ----------------------------------------------------------------

const alignmentView = (entries) =>
  entries.map((entry) =>
    entry.kind === "frame"
      ? {
          kind: "frame",
          mediaStartSample: String(entry.mediaStartSample),
          mediaEndSample: String(entry.mediaEndSample),
          sequence: entry.provenance === null ? null : String(entry.provenance.sequence),
          captureStatus: entry.captureStatus,
          reasons: [...entry.reasons],
          wall:
            entry.wall === null
              ? null
              : { localStartNs: String(entry.wall.localStartNs), localEndNs: String(entry.wall.localEndNs) },
        }
      : { kind: entry.kind },
  );

async function readAlignment(client, capability) {
  const entries = [];
  let completion = null;
  for await (const item of client.readRecordingAlignment({ capability })) {
    if (item.type === "recording_alignment_page") entries.push(...item.entries);
    else
      completion = {
        pageCount: String(item.pageCount),
        coverageStatus: item.coverageStatus,
        warnings: [...item.warnings],
      };
  }
  return { entries: alignmentView(entries), completion };
}

// --- scenario -----------------------------------------------------------------

async function run({ scenario, gatewayPath, runDir }) {
  const observations = { scenario };
  const modelCache = join(runDir, "models");
  const recordings = join(runDir, "recordings");
  const configPath = join(runDir, "gateway.toml");
  await mkdir(modelCache, { recursive: true });
  await mkdir(recordings, { recursive: true });
  const language = await startLanguageFixture();
  await writeFile(configPath, offlineConfig({ languageEndpoint: language.endpoint, modelCache, recordings }));

  const first = await startGateway(gatewayPath, configPath);
  const session = await first.client.startDrainableTimedVoiceSession({
    bridgeSessionId: BRIDGE,
    profile: "offline",
    responseMode: "external_gate",
  });
  const lease = session.mediaLease;
  const sessionId = lease.sessionId;
  observations.lease = {
    mediaProtocolVersion: lease.mediaProtocolVersion,
    captureFormat: lease.captureFormat,
    timingContract: lease.timingContract,
    sessionId: String(sessionId),
  };

  const producer = await Producer.connect(lease.connectEndpoint);
  await authenticate(producer, sessionId, lease.credential);
  await producer.write(med3(6, Buffer.concat([u64(sessionId), encodeEpoch(1)])));
  await producer.write(
    med3(7, Buffer.concat([u64(sessionId), encodePair({ sequence: 1, hostNs: FIRST_PAIR_HOST_NS, wallNs: FIRST_PAIR_WALL_NS })])),
  );
  for (let index = 0; index < FRAME_COUNT; index += 1) await producer.write(frameRecord(sessionId, index));

  // The delayed final clock pair arrives near the tail, after every frame.
  const withholdPair = scenario === "missing-final-pair";
  const finalPairHostNs = scenario === "pair-gap" ? WIDE_GAP_PAIR_HOST_NS : FINAL_PAIR_HOST_NS;
  if (!withholdPair) {
    await new Promise((resolve) => setTimeout(resolve, 40));
    await producer.write(
      med3(
        7,
        Buffer.concat([
          u64(sessionId),
          encodePair({
            sequence: 2,
            hostNs: finalPairHostNs,
            wallNs: FIRST_PAIR_WALL_NS + (finalPairHostNs - FIRST_PAIR_HOST_NS),
          }),
        ]),
      ),
    );
  }

  const exact = {
    sessionId,
    captureEpoch: 1,
    framePresent: true,
    sequence: FRAME_COUNT,
    outputEnd: FRAME_COUNT * SAMPLES_PER_FRAME,
    pairSequence: withholdPair ? 1 : 2,
  };
  const declared = scenario === "mismatched-finish" ? { ...exact, sequence: FRAME_COUNT - 1 } : exact;
  observations.declaredBoundary = { ...declared, sessionId: String(sessionId) };

  const finishSentAt = performance.now();
  await producer.write(med3(FINISH_KIND, finishBody(declared)));

  if (scenario === "unread-ack") {
    // The producer never consumes the acknowledgement as an acknowledgement and
    // never sends its own EOF. The server's own absolute finish budget must end
    // the connection without a second packet and without wedging the gateway.
    await producer.awaitPeerFin(15_000);
    const peerFinMs = Math.round(performance.now() - finishSentAt);
    await new Promise((resolve) => setTimeout(resolve, 11_000));
    observations.finish = {
      outcome: "unconfirmed",
      peerFinMs,
      waitedMs: Math.round(performance.now() - finishSentAt),
      producerReadAcknowledgement: false,
      producerSentEof: false,
      bytesDelivered: producer.received.length,
      ackPacketCount: countAcks(producer.received),
      socketError: producer.socketError,
    };
    observations.gatewayAfterBudget = (await first.client.status()).transport;
    producer.destroy();
  } else if (scenario === "mismatched-finish") {
    await producer.awaitPeerFin(15_000);
    observations.finish = {
      outcome: "rejected",
      closeMs: Math.round(performance.now() - finishSentAt),
      bytesDelivered: producer.received.length,
      ackPacketCount: countAcks(producer.received),
      socketError: producer.socketError,
    };
    producer.destroy();
    observations.leaseAfterRejection = await probeLease(lease, sessionId);
  } else {
    const ack = await producer.readExact(ACK_BYTES, 15_000);
    observations.finish = {
      outcome: "acknowledged",
      ackLatencyMs: Math.round(performance.now() - finishSentAt),
      ackBytes: ack.length,
      ackHex: ack.toString("hex"),
      echoesDeclaredBoundary: ack.equals(med3(11, finishBody(declared))),
    };
    if (scenario === "duplicate-finish") {
      await producer.write(med3(FINISH_KIND, finishBody(declared)));
      // The server already half-closed after the ACK, so the only observable
      // facts are that nothing further arrives and that the socket is dead.
      await new Promise((resolve) => setTimeout(resolve, 750));
      observations.finish.bytesAfterDuplicate = producer.received.length;
      observations.finish.totalAckPackets = 1 + countAcks(producer.received);
      producer.destroy();
      observations.leaseAfterDuplicate = await probeLease(lease, sessionId);
    } else {
      await producer.awaitPeerFin(10_000);
      producer.end();
      await producer.awaitClose(10_000);
      observations.finish.bytesAfterEof = producer.received.length;
      observations.finish.totalAckPackets = 1 + countAcks(producer.received);
    }
  }

  observations.stopResult = await session.stopIfActive();
  const finalized = await first.client.finalizeRecording({ sessionId });
  observations.recording = {
    durationMs: String(finalized.durationMs),
    format: finalized.format,
    gapCount: finalized.gaps.length,
  };
  const capability = finalized.recoveryCapability;
  await first.client.close();
  observations.firstCleanup = first.transport.cleanupOutcome;

  const second = await startGateway(gatewayPath, configPath);
  observations.reopen = {
    newControlIdentity: String(second.client.currentSessionId),
    differsFromCaptureSession: second.client.currentSessionId !== sessionId,
  };
  observations.alignment = await readAlignment(second.client, capability);
  await second.client.close();
  observations.secondCleanup = second.transport.cleanupOutcome;
  observations.providerTraffic = {
    healthProbes: language.healthProbes.length,
    providerRequests: [...language.providerRequests],
  };
  await language.close();
  observations.lastFrameSpan = {
    hostStartNs: String(frameSpan(FRAME_COUNT - 1).hostStartNs),
    hostEndNs: String(frameSpan(FRAME_COUNT - 1).hostEndNs),
  };
  return observations;
}

/**
 * Measures whether the lease is still usable for media after a finish outcome:
 * a fresh connection, a resumed epoch and pair, one more frame, and a finish that
 * matches the resumed watermark exactly.
 */
async function probeLease(lease, sessionId) {
  const result = { reauthenticated: false, resumedCaptureAcknowledged: false };
  const probe = await Producer.connect(lease.connectEndpoint);
  try {
    await authenticate(probe, sessionId, lease.credential);
    result.reauthenticated = true;
  } catch (error) {
    result.authenticationError = String(error.message);
    probe.destroy();
    return result;
  }
  try {
    await probe.write(med3(6, Buffer.concat([u64(sessionId), encodeEpoch(1)])));
    await probe.write(
      med3(
        7,
        Buffer.concat([
          u64(sessionId),
          encodePair({ sequence: 3, hostNs: 1_000_600_000n, wallNs: 2_100_600_000n }),
        ]),
      ),
    );
    await probe.write(frameRecord(sessionId, FRAME_COUNT));
    await probe.write(
      med3(
        FINISH_KIND,
        finishBody({
          sessionId,
          captureEpoch: 1,
          framePresent: true,
          sequence: FRAME_COUNT + 1,
          outputEnd: (FRAME_COUNT + 1) * SAMPLES_PER_FRAME,
          pairSequence: 3,
        }),
      ),
    );
    await probe.readExact(ACK_BYTES, 8_000);
    result.resumedCaptureAcknowledged = true;
    probe.end();
  } catch (error) {
    result.resumedCaptureError = String(error.message);
  }
  probe.destroy();
  return result;
}

const options = JSON.parse(await readFile(process.argv[2], "utf8"));
const serialize = (value) => JSON.stringify(value, (_, item) => (typeof item === "bigint" ? String(item) : item), 2);
try {
  await writeFile(options.resultPath, serialize(await run(options)));
  process.exit(0);
} catch (error) {
  await writeFile(options.resultPath, serialize({ scenario: options.scenario, failure: String(error?.stack ?? error) }));
  process.exit(1);
}
