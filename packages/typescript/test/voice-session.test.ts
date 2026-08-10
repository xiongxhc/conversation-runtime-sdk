import assert from "node:assert/strict";
import { execFile, spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

import { RuntimeClient, type RuntimeTurn } from "../src/client.js";
import type { RuntimeEvent, VoiceSessionEvent } from "../src/protocol.js";
import { StdioGatewayTransport } from "../src/stdio.js";

const execFileAsync = promisify(execFile);

// This test spawns the *compiled* gateway binary against the real fake voice sidecar binary,
// so it needs a `cargo build` (usually a fast no-op against the already-built target/debug used
// by apps/runtime-gateway/tests/voice_session.rs) plus a full voice round trip.
const CARGO_BUILD_TIMEOUT_MS = 5 * 60 * 1000;
const TEST_TIMEOUT_MS = 60 * 1000;

const REPO_ROOT = fileURLToPath(new URL("../../../../", import.meta.url));
const FIXTURE_MODEL_ID = "fixture-private-local-model";
const SCENARIO_ENV = "CONVERSATION_FAKE_VOICE_SIDECAR_SCENARIO";

test(
  "typed then spoken then typed turns share one context through the public SDK",
  { timeout: TEST_TIMEOUT_MS },
  async () => {
    const [gatewayPath, sidecarPath] = await Promise.all([
      buildCargoBinary("conversation-runtime-gateway", "conversation-runtime-gateway"),
      buildCargoBinary("conversation-voice-probe", "conversation-fake-voice-sidecar"),
    ]);

    const chat = await FakeChatServer.start();
    const speech = await FakeSpeechServer.start();
    const directory = await mkdtemp(join(tmpdir(), "conversation-runtime-voice-"));

    try {
      const modelPath = join(directory, "asr-model");
      await mkdir(modelPath);
      const configPath = join(directory, "gateway.toml");
      await writeFile(
        configPath,
        gatewayConfigContents({
          languageEndpoint: chat.endpoint,
          speechEndpoint: speech.endpoint,
          modelPath,
          sidecarExecutable: sidecarPath,
        }),
      );

      const transport = await spawnGateway({ gatewayPath, configPath, scenario: "partial-final" });
      const client = await RuntimeClient.connect(transport);

      const firstTurn = await client.startTurn("first typed request");
      const firstEvents = await drainTurn(firstTurn);
      assert.equal(lastEventType(firstEvents), "turn_completed");

      const session = await client.startVoiceSession();
      const beforeStop: VoiceSessionEvent[] = [];
      for await (const event of session.events()) {
        beforeStop.push(event);
        if (event.type === "voice_turn_event" && event.event.type === "turn_completed") {
          break;
        }
      }
      assert.ok(
        beforeStop.some(
          (event) => event.type === "voice_transcript_final" && event.text === "hello",
        ),
        "voice session never produced the scripted final transcript",
      );
      assert.ok(
        beforeStop.some(
          (event) => event.type === "voice_turn_event" && event.event.type === "turn_completed",
        ),
        "voice turn never completed",
      );

      const stopPromise = session.stop();
      const afterStop: VoiceSessionEvent[] = [];
      for await (const event of session.events()) {
        afterStop.push(event);
      }
      await stopPromise;
      assert.equal(lastEventType(afterStop), "voice_session_ended");

      const thirdTurn = await client.startTurn("third typed request");
      const thirdEvents = await drainTurn(thirdTurn);
      assert.equal(lastEventType(thirdEvents), "turn_completed");

      await client.close();

      assert.equal(chat.requestBodies().length, 3, "expected one language request per turn");
      const finalMessages = extractUserAndAssistantMessages(chat.requestBodies()[2]);
      assert.deepEqual(
        subsequenceStartingAt(finalMessages, "user", "first typed request"),
        [
          { role: "user", content: "first typed request" },
          { role: "assistant", content: "fixture-answer" },
          { role: "user", content: "hello" },
          { role: "assistant", content: "fixture-answer" },
          { role: "user", content: "third typed request" },
        ],
        `final request did not carry the prior typed and spoken exchanges in order: ${JSON.stringify(finalMessages)}`,
      );
    } finally {
      await Promise.all([chat.close(), speech.close()]);
      await rm(directory, { force: true, recursive: true });
    }
  },
);

async function spawnGateway(options: {
  gatewayPath: string;
  configPath: string;
  scenario: string;
}): Promise<StdioGatewayTransport> {
  const child = spawn(options.gatewayPath, ["--config", options.configPath], {
    shell: false,
    stdio: ["pipe", "pipe", "pipe"],
    env: { ...process.env, [SCENARIO_ENV]: options.scenario },
  }) as ChildProcessWithoutNullStreams;
  await new Promise<void>((resolve, reject) => {
    child.once("spawn", resolve);
    child.once("error", () => reject(new Error("gateway spawn failed")));
  });
  const testable = StdioGatewayTransport as unknown as {
    startWithChildForTest(childProcess: ChildProcessWithoutNullStreams): StdioGatewayTransport;
  };
  return testable.startWithChildForTest(child);
}

async function drainTurn(turn: RuntimeTurn): Promise<RuntimeEvent[]> {
  const events: RuntimeEvent[] = [];
  for await (const event of turn.events) {
    events.push(event);
  }
  return events;
}

function lastEventType(events: Array<{ type: string }>): string | undefined {
  return events.at(-1)?.type;
}

/// Builds (or locates, if already fresh) a workspace binary the same way
/// `apps/runtime-gateway/tests/support/mod.rs::build_fake_sidecar_binary` does for Rust: run
/// `cargo build --message-format=json-render-diagnostics` and parse the compiler-artifact
/// executable path, so this stays correct under any `CARGO_TARGET_DIR` customization.
async function buildCargoBinary(packageName: string, binName: string): Promise<string> {
  const { stdout } = await execFileAsync(
    "cargo",
    ["build", "--message-format=json-render-diagnostics", "-p", packageName, "--bin", binName],
    { cwd: REPO_ROOT, timeout: CARGO_BUILD_TIMEOUT_MS, maxBuffer: 64 * 1024 * 1024 },
  );
  for (const line of stdout.split("\n")) {
    if (line.trim() === "") {
      continue;
    }
    let message: unknown;
    try {
      message = JSON.parse(line);
    } catch {
      continue;
    }
    if (
      typeof message === "object"
      && message !== null
      && (message as { reason?: unknown }).reason === "compiler-artifact"
      && (message as { target?: { name?: unknown } }).target?.name === binName
      && typeof (message as { executable?: unknown }).executable === "string"
    ) {
      return (message as { executable: string }).executable;
    }
  }
  throw new Error(`cargo build did not produce an executable for ${binName}`);
}

/// Duplicates the `[voice]` config shape from
/// `apps/runtime-gateway/tests/support/mod.rs::voice_lane_config_contents` for the TypeScript
/// test stack, which stays independent from the Rust harness by design (per the task brief).
function gatewayConfigContents(options: {
  languageEndpoint: string;
  speechEndpoint: string;
  modelPath: string;
  sidecarExecutable: string;
}): string {
  return `schema_version = 1
privacy_mode = "local-only"

[language]
backend = "ollama-compatible"
execution = "local"
provider = "fixture-language"
endpoint = "${options.languageEndpoint}"
model = "${FIXTURE_MODEL_ID}"
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
provider = "fixture-speech-recognition"
model_path = "${options.modelPath}"
download = false

[voice.speech]
backend = "openai-compatible"
execution = "local"
provider = "fixture-speech-synthesis"
mode = "buffered"
endpoint = "${options.speechEndpoint}/v1"
model = "fixture-speech-model"
voice = "fixture-voice"
speed = 1.0
language = "auto"
instructions = "Speak clearly."
max_tokens = 128
repetition_penalty = 1.0
max_text_bytes = 4096
max_audio_bytes = 8388608

[voice.audio]
backend = "managed-sidecar"
execution = "local"
provider = "fixture-audio"
sidecar_executable = "${options.sidecarExecutable}"
max_error_bytes = 65536
`;
}

/// A loopback ollama-compatible chat fixture that captures every request body it receives, so
/// the test can inspect the `messages` array the gateway sent for the third turn and prove it
/// carries the prior typed and spoken exchanges — the shared-context proof this test exists for.
class FakeChatServer {
  private readonly bodies: unknown[] = [];

  private constructor(
    private readonly server: Server,
    readonly endpoint: string,
  ) {}

  static async start(): Promise<FakeChatServer> {
    const server = createServer();
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    if (address === null || typeof address === "string") {
      throw new Error("fake chat server failed to bind");
    }
    const instance = new FakeChatServer(server, `http://127.0.0.1:${address.port}`);
    server.on("request", (request, response) => instance.handle(request, response));
    return instance;
  }

  requestBodies(): readonly unknown[] {
    return this.bodies;
  }

  async close(): Promise<void> {
    await new Promise<void>((resolve, reject) => {
      this.server.close((error) => (error ? reject(error) : resolve()));
    });
  }

  private handle(request: IncomingMessage, response: ServerResponse): void {
    const chunks: Buffer[] = [];
    request.on("data", (chunk: Buffer) => chunks.push(chunk));
    request.on("end", () => {
      const raw = Buffer.concat(chunks).toString("utf8");
      try {
        this.bodies.push(JSON.parse(raw));
      } catch {
        this.bodies.push(raw);
      }
      response.writeHead(200, { "Content-Type": "application/x-ndjson", Connection: "close" });
      response.write(
        `${JSON.stringify({ message: { role: "assistant", content: "fixture-answer" }, done: false })}\n`,
      );
      response.end(`${JSON.stringify({ message: { role: "assistant", content: "" }, done: true })}\n`);
    });
  }
}

/// A loopback OpenAI-compatible speech synthesis fixture matching
/// `apps/runtime-gateway/tests/support/mod.rs::FakeTtsServer`'s WAV response shape.
class FakeSpeechServer {
  private constructor(
    private readonly server: Server,
    readonly endpoint: string,
  ) {}

  static async start(): Promise<FakeSpeechServer> {
    const server = createServer();
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    const address = server.address();
    if (address === null || typeof address === "string") {
      throw new Error("fake speech server failed to bind");
    }
    const instance = new FakeSpeechServer(server, `http://127.0.0.1:${address.port}`);
    server.on("request", (request, response) => instance.handle(request, response));
    return instance;
  }

  async close(): Promise<void> {
    await new Promise<void>((resolve, reject) => {
      this.server.close((error) => (error ? reject(error) : resolve()));
    });
  }

  private handle(request: IncomingMessage, response: ServerResponse): void {
    request.resume();
    request.on("end", () => {
      const wav = pcmWav();
      response.writeHead(200, { "Content-Type": "audio/wav", "Content-Length": String(wav.length) });
      response.end(wav);
    });
  }
}

/// A minimal valid 8 kHz mono 16-bit PCM WAV body, matching the Rust harness's fixture shape.
function pcmWav(): Buffer {
  const sampleRate = 8_000;
  const samples = 160;
  const dataBytes = samples * 2;
  const wav = Buffer.alloc(44 + dataBytes);
  wav.write("RIFF", 0, "ascii");
  wav.writeUInt32LE(36 + dataBytes, 4);
  wav.write("WAVEfmt ", 8, "ascii");
  wav.writeUInt32LE(16, 16);
  wav.writeUInt16LE(1, 20);
  wav.writeUInt16LE(1, 22);
  wav.writeUInt32LE(sampleRate, 24);
  wav.writeUInt32LE(sampleRate * 2, 28);
  wav.writeUInt16LE(2, 32);
  wav.writeUInt16LE(16, 34);
  wav.write("data", 36, "ascii");
  wav.writeUInt32LE(dataBytes, 40);
  return wav;
}

type ChatMessage = { role: string; content: string };

function extractUserAndAssistantMessages(body: unknown): ChatMessage[] {
  if (typeof body !== "object" || body === null || !("messages" in body)) {
    return [];
  }
  const messages = (body as { messages: unknown }).messages;
  if (!Array.isArray(messages)) {
    return [];
  }
  return messages.filter(
    (message): message is ChatMessage =>
      typeof message === "object"
      && message !== null
      && (message as { role?: unknown }).role !== undefined
      && ((message as { role: unknown }).role === "user" || (message as { role: unknown }).role === "assistant")
      && typeof (message as { content?: unknown }).content === "string",
  );
}

function subsequenceStartingAt(messages: ChatMessage[], role: string, content: string): ChatMessage[] {
  const startIndex = messages.findIndex((message) => message.role === role && message.content === content);
  return startIndex === -1 ? [] : messages.slice(startIndex);
}
