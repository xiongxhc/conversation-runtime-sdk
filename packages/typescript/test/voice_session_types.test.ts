import assert from "node:assert/strict";
import test from "node:test";

import { RuntimeClient, type RuntimeTransport } from "../src/client.js";

function acceptsBigint(_value: bigint): void {}

declare const transport: RuntimeTransport;

async function legacyFinalTurnIdIsNonNull(): Promise<void> {
  const session = await (await RuntimeClient.connect(transport)).startVoiceSession();
  for await (const event of session.events()) {
    if (event.type === "voice_transcript_final") {
      acceptsBigint(event.turnId);
    }
  }
}

async function versionThreeFinalTurnIdRequiresNullHandling(): Promise<void> {
  const session = await (await RuntimeClient.connect<3>(transport)).startVoiceSession();
  for await (const event of session.events()) {
    if (event.type === "voice_transcript_final") {
      // @ts-expect-error v3 external-gate finals may not have a turn yet.
      acceptsBigint(event.turnId);
      if (event.turnId !== null) {
        acceptsBigint(event.turnId);
      }
    }
  }
}

test("version-typed voice sessions retain final-turn nullability", () => {
  assert.ok(legacyFinalTurnIdIsNonNull);
  assert.ok(versionThreeFinalTurnIdRequiresNullHandling);
});
