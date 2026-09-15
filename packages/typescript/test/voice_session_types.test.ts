import assert from "node:assert/strict";
import test from "node:test";

import { RuntimeClient, type RuntimeTransport } from "../src/client.js";

function acceptsBigint(_value: bigint): void {}

declare const transport: RuntimeTransport;

async function legacyFinalTurnIdIsNonNull(): Promise<void> {
  const session = await (await RuntimeClient.connect(transport, 1)).startVoiceSession();
  for await (const event of session.events()) {
    if (event.type === "voice_transcript_final") {
      acceptsBigint(event.turnId);
    }
  }
}

async function versionThreeFinalTurnIdRequiresNullHandling(): Promise<void> {
  const session = await (await RuntimeClient.connect(transport, 3)).startVoiceSession();
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

async function unpinnedFinalTurnIdRequiresNullHandling(): Promise<void> {
  const session = await (await RuntimeClient.connect(transport)).startVoiceSession();
  for await (const event of session.events()) {
    if (event.type === "voice_transcript_final") {
      // @ts-expect-error an unpinned client may negotiate v3.
      acceptsBigint(event.turnId);
    }
  }
}

async function onlyPinnedFourExposesCaptureEvidence(): Promise<void> {
  const four = await (await RuntimeClient.connect(transport, 4)).startVoiceSession();
  for await (const event of four.events()) {
    if (event.type === "voice_transcript_final" && event.capture) {
      acceptsBigint(event.capture.sourceStartSample);
    }
  }
  const three = await (await RuntimeClient.connect(transport, 3)).startVoiceSession();
  for await (const event of three.events()) {
    if (event.type === "voice_transcript_final") {
      // @ts-expect-error capture evidence is not part of the v3 public event.
      acceptsBigint(event.capture.sourceStartSample);
    }
  }
}

test("version-typed voice sessions retain final-turn nullability", () => {
  assert.ok(legacyFinalTurnIdIsNonNull);
  assert.ok(versionThreeFinalTurnIdRequiresNullHandling);
  assert.ok(unpinnedFinalTurnIdRequiresNullHandling);
  assert.ok(onlyPinnedFourExposesCaptureEvidence);
});
