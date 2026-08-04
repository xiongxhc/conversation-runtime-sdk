export const MAX_FRAME_BYTES = 512 * 1024;

export class FrameError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "FrameError";
  }
}

export class FrameDecoder {
  private buffered = new Uint8Array();

  push(chunk: Uint8Array): Uint8Array[] {
    if (chunk.length === 0) {
      return [];
    }

    const combined = new Uint8Array(this.buffered.length + chunk.length);
    combined.set(this.buffered);
    combined.set(chunk, this.buffered.length);
    this.buffered = combined;

    const frames: Uint8Array[] = [];
    let offset = 0;
    while (this.buffered.length - offset >= 4) {
      const length = new DataView(
        this.buffered.buffer,
        this.buffered.byteOffset + offset,
        4,
      ).getUint32(0);
      validateFrameLength(length);
      if (this.buffered.length - offset - 4 < length) {
        break;
      }
      frames.push(this.buffered.slice(offset + 4, offset + 4 + length));
      offset += 4 + length;
    }
    this.buffered = this.buffered.slice(offset);
    return frames;
  }

  finish(): void {
    if (this.buffered.length !== 0) {
      throw new FrameError("framed input ended before a complete frame");
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
