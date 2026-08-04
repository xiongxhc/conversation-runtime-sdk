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
