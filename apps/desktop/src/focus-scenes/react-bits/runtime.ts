import { useEffect, useState } from "react";

import type { VoiceVisualState } from "../types.js";

export interface VisibilityDocument {
  readonly visibilityState: DocumentVisibilityState;
  addEventListener(type: "visibilitychange", listener: () => void): void;
  removeEventListener(type: "visibilitychange", listener: () => void): void;
}

export interface VisibilityAwareAnimationOptions {
  cancelFrame: (id: number) => void;
  document: VisibilityDocument;
  onError?: (error: unknown) => void;
  renderFrame: FrameRequestCallback;
  requestFrame: (callback: FrameRequestCallback) => number;
}

export interface SceneResourceTracker {
  add(cleanup: () => void): void;
  release(): void;
}

export const voiceStateColors: Record<VoiceVisualState, string> = {
  idle: "#b8a3de",
  listening: "#8ed6d0",
  thinking: "#b8a3de",
  speaking: "#eaa58d",
  interrupted: "#f6f3ec",
  error: "#777b82",
};

export function clampIntensity(intensity: number): number {
  return Number.isFinite(intensity) ? Math.min(1, Math.max(0, intensity)) : 0.55;
}

export function createSceneResourceTracker(): SceneResourceTracker {
  const cleanups: Array<() => void> = [];
  let released = false;

  return {
    add(cleanup) {
      if (released) {
        safelyRun(cleanup);
        return;
      }
      cleanups.push(cleanup);
    },
    release() {
      if (released) return;
      released = true;
      for (let index = cleanups.length - 1; index >= 0; index -= 1) {
        safelyRun(cleanups[index]);
      }
      cleanups.length = 0;
    },
  };
}

export function startVisibilityAwareAnimation({
  cancelFrame,
  document: visibilityDocument,
  onError,
  renderFrame,
  requestFrame,
}: VisibilityAwareAnimationOptions): () => void {
  let frameId: number | undefined;
  let disposed = false;

  const schedule = () => {
    if (!disposed && frameId === undefined && visibilityDocument.visibilityState === "visible") {
      frameId = requestFrame(frame);
    }
  };

  const frame: FrameRequestCallback = (time) => {
    frameId = undefined;
    if (disposed || visibilityDocument.visibilityState !== "visible") return;

    try {
      renderFrame(time);
    } catch (error) {
      handleFailure(error);
      return;
    }

    try {
      schedule();
    } catch (error) {
      handleFailure(error);
    }
  };

  const handleVisibilityChange = () => {
    if (visibilityDocument.visibilityState !== "visible") {
      if (frameId !== undefined) cancelFrame(frameId);
      frameId = undefined;
      return;
    }

    try {
      schedule();
    } catch (error) {
      handleFailure(error);
    }
  };

  const handleFailure = (error: unknown) => {
    disposed = true;
    if (frameId !== undefined) cancelFrame(frameId);
    frameId = undefined;
    visibilityDocument.removeEventListener("visibilitychange", handleVisibilityChange);
    onError?.(error);
  };

  visibilityDocument.addEventListener("visibilitychange", handleVisibilityChange);
  try {
    schedule();
  } catch (error) {
    disposed = true;
    visibilityDocument.removeEventListener("visibilitychange", handleVisibilityChange);
    throw error;
  }

  return () => {
    disposed = true;
    if (frameId !== undefined) cancelFrame(frameId);
    frameId = undefined;
    visibilityDocument.removeEventListener("visibilitychange", handleVisibilityChange);
  };
}

export function useDocumentVisible(): boolean {
  const [visible, setVisible] = useState(() => document.visibilityState === "visible");

  useEffect(() => {
    const handleVisibilityChange = () => setVisible(document.visibilityState === "visible");
    document.addEventListener("visibilitychange", handleVisibilityChange);
    return () => document.removeEventListener("visibilitychange", handleVisibilityChange);
  }, []);

  return visible;
}

export function loseWebGlContext(gl: WebGLRenderingContext | WebGL2RenderingContext): void {
  gl.getExtension("WEBGL_lose_context")?.loseContext();
}

function safelyRun(cleanup: () => void): void {
  try {
    cleanup();
  } catch {}
}
