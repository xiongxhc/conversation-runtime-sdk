import { isAbsolute } from "node:path";
import { createInterface, type Interface } from "node:readline/promises";
import type { Readable, Writable } from "node:stream";
import { pathToFileURL } from "node:url";

import {
  RuntimeClient,
  StdioGatewayTransport,
  type RuntimeStatus,
  type RuntimeTransport,
  type RuntimeTurn,
} from "@conversation/runtime";

const USAGE = "usage: conversation-node-chat --gateway <absolute-path> --config <absolute-path>\n";

export interface SignalSource {
  on(event: "SIGINT", listener: () => void): unknown;
  off(event: "SIGINT", listener: () => void): unknown;
}

export interface ChatIo {
  readonly input: Readable;
  readonly output: Writable;
  readonly diagnostics: Writable;
  readonly signals: SignalSource;
}

export interface ChatDependencies {
  startTransport(options: {
    gatewayPath: string;
    configPath: string;
  }): Promise<RuntimeTransport>;
}

const defaultDependencies: ChatDependencies = {
  startTransport: (options) => StdioGatewayTransport.start(options),
};

export async function runChat(
  arguments_: readonly string[],
  io: ChatIo,
  dependencies: ChatDependencies = defaultDependencies,
): Promise<number> {
  const options = parseArguments(arguments_);
  if (!options) {
    io.diagnostics.write(USAGE);
    return 2;
  }

  let client: RuntimeClient | undefined;
  let lines: Interface | undefined;
  let active: ActiveTurn | undefined;
  let closePromise: Promise<void> | undefined;
  let exitCode = 0;
  let failureReported = false;
  let stopping = false;

  const reportFailure = (): void => {
    if (!failureReported) {
      failureReported = true;
      io.diagnostics.write("chat failed\n");
    }
  };
  const stop = (code = 0): void => {
    exitCode = Math.max(exitCode, code);
    if (stopping) {
      return;
    }
    stopping = true;
    lines?.close();
    if (client) {
      closePromise = client.close();
    }
  };
  const onSigint = (): void => {
    if (active && !active.interruptRequested) {
      active.interruptRequested = true;
      void client?.interrupt(active.turn.turnId).catch(() => {
        if (!stopping) {
          reportFailure();
          stop(1);
        }
      });
      return;
    }
    stop();
  };

  try {
    const transport = await dependencies.startTransport(options);
    client = await RuntimeClient.connect(transport);
    lines = createInterface({ input: io.input, crlfDelay: Infinity });
    io.signals.on("SIGINT", onSigint);

    const status = await client.status();
    assertLocalOnly(status);
    io.output.write(privacyLine(status));

    const iterator = lines[Symbol.asyncIterator]();
    while (!stopping) {
      io.output.write("you> ");
      const next = await iterator.next();
      if (next.done || stopping) {
        break;
      }
      if (next.value.length === 0) {
        continue;
      }

      io.output.write("assistant> ");
      const turn = client.startTurn(next.value);
      active = { turn, interruptRequested: false };
      const terminal = await renderTurn(turn, io.output);
      active = undefined;
      if (terminal === "failed") {
        reportFailure();
        stop(1);
      }
    }
  } catch {
    if (!stopping) {
      reportFailure();
      exitCode = Math.max(exitCode, 1);
    }
  } finally {
    active = undefined;
    io.signals.off("SIGINT", onSigint);
    lines?.close();
    if (client && !closePromise) {
      closePromise = client.close();
    }
    try {
      await closePromise;
    } catch {
      if (!stopping) {
        reportFailure();
        exitCode = Math.max(exitCode, 1);
      }
    }
  }

  return exitCode;
}

type ActiveTurn = {
  readonly turn: RuntimeTurn;
  interruptRequested: boolean;
};

type ChatOptions = {
  readonly gatewayPath: string;
  readonly configPath: string;
};

function parseArguments(arguments_: readonly string[]): ChatOptions | undefined {
  if (arguments_.length !== 4) {
    return undefined;
  }
  const values = new Map<string, string>();
  for (let index = 0; index < arguments_.length; index += 2) {
    const key = arguments_[index];
    const value = arguments_[index + 1];
    if ((key !== "--gateway" && key !== "--config") || !value || values.has(key)) {
      return undefined;
    }
    values.set(key, value);
  }
  const gatewayPath = values.get("--gateway");
  const configPath = values.get("--config");
  if (!gatewayPath || !configPath || !isAbsolute(gatewayPath) || !isAbsolute(configPath)) {
    return undefined;
  }
  return { gatewayPath, configPath };
}

function assertLocalOnly(status: RuntimeStatus): void {
  if (
    status.privacyMode !== "local_only"
    || status.languageLocation !== "local"
    || status.telemetryEnabled !== false
    || (status.memoryEnabled && status.memoryLocation !== "local")
  ) {
    throw new Error("gateway did not report local-only status");
  }
}

function privacyLine(status: RuntimeStatus): string {
  const memory = status.memoryEnabled ? "local" : "disabled";
  return `privacy: local-only; language: local; memory: ${memory}; telemetry: off\n`;
}

async function renderTurn(turn: RuntimeTurn, output: Writable): Promise<"completed" | "cancelled" | "failed"> {
  let lineOpen = true;
  for await (const event of turn.events) {
    if (event.type === "text_delta") {
      output.write(event.delta);
      lineOpen = !event.delta.endsWith("\n");
      continue;
    }
    if (event.type === "turn_completed") {
      writeTerminal(output, "completed", lineOpen);
      return "completed";
    }
    if (event.type === "turn_cancelled") {
      writeTerminal(output, "cancelled", lineOpen);
      return "cancelled";
    }
    if (event.type === "turn_failed") {
      writeTerminal(output, "failed", lineOpen);
      return "failed";
    }
  }
  throw new Error("turn ended without a terminal event");
}

function writeTerminal(
  output: Writable,
  terminal: "completed" | "cancelled" | "failed",
  lineOpen: boolean,
): void {
  if (lineOpen) {
    output.write("\n");
  }
  output.write(`[${terminal}]\n`);
}

const entrypoint = process.argv[1] ? pathToFileURL(process.argv[1]).href : undefined;
if (entrypoint === import.meta.url) {
  void runChat(process.argv.slice(2), {
    input: process.stdin,
    output: process.stdout,
    diagnostics: process.stderr,
    signals: process,
  }).then((code) => {
    process.exitCode = code;
  });
}
