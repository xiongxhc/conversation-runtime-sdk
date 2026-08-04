import assert from "node:assert/strict";
import test from "node:test";

import { FrameDecoder, MAX_FRAME_BYTES, encodeFrame } from "../src/framing.js";

test("decodes a frame fragmented across arbitrary chunks", () => {
  const encoded = encodeFrame(new TextEncoder().encode("hello"));
  const decoder = new FrameDecoder();

  assert.deepEqual(decoder.push(encoded.subarray(0, 2)), []);
  assert.deepEqual(decoder.push(encoded.subarray(2, 6)), []);
  assert.deepEqual(
    decoder.push(encoded.subarray(6)).map((frame) => new TextDecoder().decode(frame)),
    ["hello"],
  );
  decoder.finish();
});

test("decodes coalesced frames", () => {
  const first = encodeFrame(new Uint8Array([1]));
  const second = encodeFrame(new Uint8Array([2, 3]));
  const bytes = new Uint8Array(first.length + second.length);
  bytes.set(first);
  bytes.set(second, first.length);

  const decoded = new FrameDecoder().push(bytes);
  assert.deepEqual(decoded, [new Uint8Array([1]), new Uint8Array([2, 3])]);
});

test("rejects empty and oversized frame payloads before encoding", () => {
  assert.throws(() => encodeFrame(new Uint8Array()));
  assert.throws(() => encodeFrame(new Uint8Array(MAX_FRAME_BYTES + 1)));
});

test("rejects oversized frame headers before accumulating a payload", () => {
  const decoder = new FrameDecoder();
  const header = new Uint8Array(4);
  new DataView(header.buffer).setUint32(0, MAX_FRAME_BYTES + 1);

  assert.throws(() => decoder.push(header));
});

test("rejects a stream ending with an incomplete frame", () => {
  const decoder = new FrameDecoder();
  decoder.push(new Uint8Array([0, 0, 0, 2, 1]));

  assert.throws(() => decoder.finish());
});

test("decodes a maximal frame delivered one byte at a time", { timeout: 5_000 }, () => {
  const payload = new Uint8Array(MAX_FRAME_BYTES).fill(7);
  const frame = encodeFrame(payload);
  const decoder = new FrameDecoder();
  let decoded: Uint8Array[] = [];

  for (const byte of frame) {
    decoded = decoder.push(new Uint8Array([byte]));
  }

  assert.deepEqual(decoded, [payload]);
  decoder.finish();
});

test("compacts consumed chunks during sustained misaligned frame traffic", () => {
  const decoder = new FrameDecoder();
  const frames = Array.from({ length: 4_097 }, (_, index) => encodeFrame(new Uint8Array([index % 256])));
  const metric = decoder as unknown as { retainedStorageChunkCount(): number };

  decoder.push(frames[0]!.subarray(0, 1));
  for (let index = 0; index < 4_096; index += 1) {
    const current = frames[index]!;
    const next = frames[index + 1]!;
    const chunk = new Uint8Array(current.length - 1 + 1);
    chunk.set(current.subarray(1));
    chunk.set(next.subarray(0, 1), current.length - 1);
    decoder.push(chunk);
    assert.ok(metric.retainedStorageChunkCount() <= 1_024);
  }
});
