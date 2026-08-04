export const MAX_FRAME_BYTES = 512 * 1024;

export class FrameError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "FrameError";
  }
}

export class FrameDecoder {
  private readonly chunks: Uint8Array[] = [];
  private available = 0;
  private chunkOffset = 0;
  private head = 0;

  push(chunk: Uint8Array): Uint8Array[] {
    if (chunk.length === 0) {
      return [];
    }
    this.chunks.push(chunk);
    this.available += chunk.length;

    const frames: Uint8Array[] = [];
    while (this.available >= 4) {
      const length = this.peekLength();
      validateFrameLength(length);
      if (this.available < 4 + length) {
        break;
      }
      this.consume(4);
      frames.push(this.read(length));
    }
    return frames;
  }

  finish(): void {
    if (this.available !== 0) {
      throw new FrameError("framed input ended before a complete frame");
    }
  }

  private retainedStorageChunkCount(): number {
    return this.chunks.length;
  }

  private peekLength(): number {
    let value = 0;
    let remaining = 4;
    let chunkIndex = this.head;
    let offset = this.chunkOffset;
    while (remaining > 0) {
      const chunk = this.chunks[chunkIndex]!;
      const count = Math.min(remaining, chunk.length - offset);
      for (let index = 0; index < count; index += 1) {
        value = (value << 8) | chunk[offset + index]!;
      }
      remaining -= count;
      chunkIndex += 1;
      offset = 0;
    }
    return value >>> 0;
  }

  private read(length: number): Uint8Array {
    const output = new Uint8Array(length);
    let written = 0;
    while (written < length) {
      const chunk = this.chunks[this.head]!;
      const count = Math.min(length - written, chunk.length - this.chunkOffset);
      output.set(chunk.subarray(this.chunkOffset, this.chunkOffset + count), written);
      this.consume(count);
      written += count;
    }
    return output;
  }

  private consume(length: number): void {
    let remaining = length;
    while (remaining > 0) {
      const chunk = this.chunks[this.head]!;
      const count = Math.min(remaining, chunk.length - this.chunkOffset);
      this.chunkOffset += count;
      this.available -= count;
      remaining -= count;
      if (this.chunkOffset === chunk.length) {
        this.head += 1;
        this.chunkOffset = 0;
      }
    }
    if (this.available === 0) {
      this.chunks.length = 0;
      this.head = 0;
      this.chunkOffset = 0;
    } else if (this.head >= 1_024 && this.head * 2 >= this.chunks.length) {
      this.chunks.splice(0, this.head);
      this.head = 0;
    }
  }
}

export function encodeFrame(payload: Uint8Array): Uint8Array {
  validateFrameLength(payload.length);
  const frame = new Uint8Array(4 + payload.length);
  new DataView(frame.buffer, frame.byteOffset, 4).setUint32(0, payload.length);
  frame.set(payload, 4);
  return frame;
}

function validateFrameLength(length: number): void {
  if (!Number.isInteger(length) || length < 1 || length > MAX_FRAME_BYTES) {
    throw new FrameError(`frame length must be within 1..=${MAX_FRAME_BYTES}`);
  }
}
