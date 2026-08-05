// @vitest-environment jsdom

import { act, cleanup, render, waitFor } from "@testing-library/react";
import type { ComponentType } from "react";
import { PlaneGeometry, Scene, ShaderMaterial } from "three";
import { afterEach, describe, expect, it, vi } from "vitest";

import Orb from "../src/focus-scenes/react-bits/Orb.js";
import Prism from "../src/focus-scenes/react-bits/Prism.js";
import Silk from "../src/focus-scenes/react-bits/Silk.js";
import SoftAurora from "../src/focus-scenes/react-bits/SoftAurora.js";
import Threads from "../src/focus-scenes/react-bits/Threads.js";
import type { FocusSceneProps } from "../src/focus-scenes/types.js";

const oglConstructors = vi.hoisted(() => ({
  Mesh: vi.fn(),
  Program: vi.fn(),
  Renderer: vi.fn(),
  Triangle: vi.fn(),
}));

const threeConstructors = vi.hoisted(() => ({
  WebGLRenderer: vi.fn(),
}));
const uncaughtErrorListeners = new Set<EventListener>();
const unhandledRejectionListeners = new Set<EventListener>();

vi.mock("ogl", async (importOriginal) => {
  const actual = await importOriginal<typeof import("ogl")>();
  return { ...actual, ...oglConstructors };
});

vi.mock("three", async (importOriginal) => {
  const actual = await importOriginal<typeof import("three")>();
  return { ...actual, ...threeConstructors };
});

const oglScenes: Array<[string, ComponentType<FocusSceneProps>]> = [
  ["soft-aurora", SoftAurora],
  ["threads", Threads],
  ["prism", Prism],
  ["orb", Orb],
];

afterEach(() => {
  cleanup();
  for (const listener of uncaughtErrorListeners) {
    window.removeEventListener("error", listener);
  }
  uncaughtErrorListeners.clear();
  for (const listener of unhandledRejectionListeners) {
    window.removeEventListener("unhandledrejection", listener);
  }
  unhandledRejectionListeners.clear();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  for (const constructor of Object.values(oglConstructors)) constructor.mockReset();
  threeConstructors.WebGLRenderer.mockReset();
});

describe("focus scene renderer lifecycle", () => {
  it.each(oglScenes)(
    "%s releases partial renderer, canvas, and geometry when program creation fails",
    async (_id, Scene) => {
      const harness = installOglHarness("program");

      const view = render(<Scene intensity={0.55} reducedMotion={false} state="idle" />);

      await expectStillGradient(view.container);
      expect(harness.appendCanvas).toHaveBeenCalledWith(harness.canvas);
      expect(harness.canvas.parentNode).toBeNull();
      expect(harness.geometry.remove).toHaveBeenCalledOnce();
      expect(harness.loseContext).toHaveBeenCalledOnce();
    },
  );

  it.each(oglScenes)(
    "%s releases listeners and GL resources when initial resize fails",
    async (_id, Scene) => {
      const harness = installOglHarness("resize");

      const view = render(<Scene intensity={0.55} reducedMotion={false} state="idle" />);

      await expectStillGradient(view.container);
      expect(harness.removeWindowListener).toHaveBeenCalledWith(
        "resize",
        harness.resizeListener,
      );
      expect(harness.canvas.parentNode).toBeNull();
      expect(harness.program.remove).toHaveBeenCalledOnce();
      expect(harness.geometry.remove).toHaveBeenCalledOnce();
      expect(harness.loseContext).toHaveBeenCalledOnce();
    },
  );

  it.each(oglScenes)(
    "%s cancels RAF and releases every resource on normal unmount",
    async (_id, Scene) => {
      const harness = installOglHarness();

      const view = render(<Scene intensity={0.55} reducedMotion={false} state="idle" />);
      await waitFor(() => expect(view.container.querySelector("canvas")).not.toBeNull());
      view.unmount();

      expect(harness.cancelFrame).toHaveBeenCalledWith(41);
      expect(harness.removeWindowListener).toHaveBeenCalledWith(
        "resize",
        harness.resizeListener,
      );
      expect(harness.canvas.parentNode).toBeNull();
      expect(harness.program.remove).toHaveBeenCalledOnce();
      expect(harness.geometry.remove).toHaveBeenCalledOnce();
      expect(harness.loseContext).toHaveBeenCalledOnce();
    },
  );

  it("renders Still Gradient when Silk renderer construction fails", async () => {
    threeConstructors.WebGLRenderer.mockImplementation(function rendererConstructor() {
      throw new Error("WebGL unavailable");
    });

    const view = render(<Silk intensity={0.55} reducedMotion={false} state="idle" />);

    await expectStillGradient(view.container);
    expect(view.container.querySelector('[data-focus-scene="silk"]')).toBeNull();
  });

  it("falls back and fully cleans Silk after post-construction setup fails", async () => {
    const harness = installSilkHarness("setup");

    const view = render(<Silk intensity={0.55} reducedMotion={false} state="idle" />);

    await expectStillGradient(view.container);
    expectSilkReleased(harness, false);
    expect(harness.requestFrame).not.toHaveBeenCalled();
    expect(harness.unhandledRejection).not.toHaveBeenCalled();
  });

  it("falls back and fully cleans Silk after a later render frame throws", async () => {
    const harness = installSilkHarness("second-render");

    const view = render(<Silk intensity={0.55} reducedMotion={false} state="speaking" />);
    await waitFor(() => expect(view.container.querySelector("canvas")).not.toBeNull());
    await waitFor(() => expect(harness.requestFrame).toHaveBeenCalled());

    expect(() => act(() => harness.runNextFrame(16))).not.toThrow();
    expect(() => act(() => harness.runNextFrame(32))).not.toThrow();

    await expectStillGradient(view.container);
    expectSilkReleased(harness, true);
    expect(harness.pendingFrames()).toBe(0);
    expect(harness.unhandledRejection).not.toHaveBeenCalled();
  });

  it("falls back and fully cleans Silk after a post-mount resize throws", async () => {
    const harness = installSilkHarness("resize");

    const view = render(<Silk intensity={0.55} reducedMotion={false} state="listening" />);
    await waitFor(() => expect(view.container.querySelector("canvas")).not.toBeNull());
    await waitFor(() => expect(harness.requestFrame).toHaveBeenCalled());

    expect(() => act(() => window.dispatchEvent(new Event("resize")))).not.toThrow();

    await expectStillGradient(view.container);
    expectSilkReleased(harness, true);
    expect(harness.pendingFrames()).toBe(0);
    expect(harness.uncaughtError).not.toHaveBeenCalled();
    expect(harness.unhandledRejection).not.toHaveBeenCalled();
  });

  it("fully disposes the real Silk scene on normal unmount", async () => {
    const harness = installSilkHarness();

    const view = render(<Silk intensity={0.55} reducedMotion={false} state="idle" />);
    await waitFor(() => expect(view.container.querySelector("canvas")).not.toBeNull());
    await waitFor(() => expect(harness.requestFrame).toHaveBeenCalled());
    view.unmount();

    expectSilkReleased(harness, true);
    expect(harness.pendingFrames()).toBe(0);
    expect(harness.unhandledRejection).not.toHaveBeenCalled();
  });
});

type FailureStage = "program" | "resize";

function installOglHarness(failureStage?: FailureStage) {
  const canvas = document.createElement("canvas");
  const loseContext = vi.fn();
  const geometry = { remove: vi.fn() };
  const program = { remove: vi.fn(), uniforms: {} as Record<string, { value: unknown }> };
  const setSize = vi.fn(() => {
    if (failureStage === "resize") throw new Error("resize failed");
  });
  const renderer = {
    dpr: 1,
    gl: {
      BLEND: 1,
      CULL_FACE: 2,
      DEPTH_TEST: 3,
      ONE_MINUS_SRC_ALPHA: 4,
      SRC_ALPHA: 5,
      blendFunc: vi.fn(),
      canvas,
      clearColor: vi.fn(),
      disable: vi.fn(),
      drawingBufferHeight: 100,
      drawingBufferWidth: 100,
      enable: vi.fn(),
      getExtension: vi.fn(() => ({ loseContext })),
    },
    render: vi.fn(),
    setSize,
  };
  oglConstructors.Renderer.mockImplementation(function rendererConstructor() {
    return renderer;
  });
  oglConstructors.Triangle.mockImplementation(function triangleConstructor() {
    return geometry;
  });
  oglConstructors.Program.mockImplementation(function programConstructor(_, options) {
    if (failureStage === "program") throw new Error("program failed");
    program.uniforms = options.uniforms;
    return program;
  });
  oglConstructors.Mesh.mockImplementation(function meshConstructor() {
    return {};
  });

  const addWindowListener = vi.spyOn(window, "addEventListener");
  const removeWindowListener = vi.spyOn(window, "removeEventListener");
  const appendCanvas = vi.spyOn(HTMLElement.prototype, "appendChild");
  const requestFrame = vi.fn(() => 41);
  const cancelFrame = vi.fn();
  vi.stubGlobal("requestAnimationFrame", requestFrame);
  vi.stubGlobal("cancelAnimationFrame", cancelFrame);

  return {
    addWindowListener,
    appendCanvas,
    cancelFrame,
    canvas,
    geometry,
    loseContext,
    program,
    removeWindowListener,
    get resizeListener() {
      return addWindowListener.mock.calls.find(([type]) => type === "resize")?.[1];
    },
  };
}

function installMeasuredCanvas() {
  class TestResizeObserver {
    readonly #callback: ResizeObserverCallback;

    constructor(callback: ResizeObserverCallback) {
      this.#callback = callback;
    }

    observe(target: Element) {
      this.#callback(
        [{ contentRect: measuredRect(), target } as ResizeObserverEntry],
        this as unknown as ResizeObserver,
      );
    }

    disconnect() {}
    unobserve() {}
  }

  vi.stubGlobal("ResizeObserver", TestResizeObserver);
  vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockReturnValue(
    measuredRect(),
  );
}

type SilkFailureStage = "resize" | "setup" | "second-render";

function installSilkHarness(failureStage?: SilkFailureStage) {
  installMeasuredCanvas();
  const canvas = document.createElement("canvas");
  const geometryDispose = vi.spyOn(PlaneGeometry.prototype, "dispose");
  const materialDispose = vi.spyOn(ShaderMaterial.prototype, "dispose");
  const sceneRemove = vi.spyOn(Scene.prototype, "remove");
  const renderer = {
    dispose: vi.fn(),
    domElement: canvas,
    forceContextLoss: vi.fn(),
    render: vi.fn(),
    setClearColor: vi.fn(),
    setPixelRatio: vi.fn(),
    setSize: vi.fn(),
    shadowMap: { enabled: false, type: 0 },
  };
  if (failureStage === "setup") {
    renderer.setSize.mockImplementation(() => {
      throw new Error("renderer setup failed");
    });
  } else if (failureStage === "resize") {
    renderer.setSize
      .mockImplementationOnce(() => {})
      .mockImplementationOnce(() => {
        throw new Error("post-mount resize failed");
      });
  } else if (failureStage === "second-render") {
    renderer.render
      .mockImplementationOnce(() => {})
      .mockImplementationOnce(() => {
        throw new Error("render frame failed");
      });
  }
  threeConstructors.WebGLRenderer.mockImplementation(function rendererConstructor() {
    return renderer;
  });

  const pendingCallbacks = new Map<number, FrameRequestCallback>();
  let nextFrameId = 1;
  const requestFrame = vi.fn((callback: FrameRequestCallback) => {
    const frameId = nextFrameId;
    nextFrameId += 1;
    pendingCallbacks.set(frameId, callback);
    return frameId;
  });
  const cancelFrame = vi.fn((frameId: number) => pendingCallbacks.delete(frameId));
  vi.stubGlobal("requestAnimationFrame", requestFrame);
  vi.stubGlobal("cancelAnimationFrame", cancelFrame);

  const addWindowListener = vi.spyOn(window, "addEventListener");
  const removeWindowListener = vi.spyOn(window, "removeEventListener");
  const addDocumentListener = vi.spyOn(document, "addEventListener");
  const removeDocumentListener = vi.spyOn(document, "removeEventListener");
  const uncaughtError = vi.fn((event: Event) => event.preventDefault());
  const unhandledRejection = vi.fn();
  window.addEventListener("error", uncaughtError);
  window.addEventListener("unhandledrejection", unhandledRejection);
  uncaughtErrorListeners.add(uncaughtError);
  unhandledRejectionListeners.add(unhandledRejection);

  return {
    addDocumentListener,
    addWindowListener,
    canvas,
    geometryDispose,
    materialDispose,
    pendingFrames: () => pendingCallbacks.size,
    removeDocumentListener,
    removeWindowListener,
    renderer,
    requestFrame,
    runNextFrame(time: number) {
      const entry = pendingCallbacks.entries().next().value as
        | [number, FrameRequestCallback]
        | undefined;
      if (!entry) throw new Error("No Silk animation frame is pending");
      pendingCallbacks.delete(entry[0]);
      entry[1](time);
    },
    sceneRemove,
    uncaughtError,
    unhandledRejection,
  };
}

function expectSilkReleased(
  harness: ReturnType<typeof installSilkHarness>,
  visibilityStarted: boolean,
) {
  expect(harness.renderer.dispose).toHaveBeenCalledOnce();
  expect(harness.renderer.forceContextLoss).toHaveBeenCalledOnce();
  expect(harness.canvas.parentNode).toBeNull();
  expect(harness.geometryDispose).toHaveBeenCalledOnce();
  expect(harness.materialDispose).toHaveBeenCalledOnce();
  expect(harness.sceneRemove).toHaveBeenCalledOnce();
  const resizeListener = harness.addWindowListener.mock.calls.find(
    ([type]) => type === "resize",
  )?.[1];
  expect(resizeListener).toBeDefined();
  expect(harness.removeWindowListener).toHaveBeenCalledWith("resize", resizeListener);
  const visibilityListener = harness.addDocumentListener.mock.calls.find(
    ([type]) => type === "visibilitychange",
  )?.[1];
  if (visibilityStarted) {
    expect(visibilityListener).toBeDefined();
    expect(harness.removeDocumentListener).toHaveBeenCalledWith(
      "visibilitychange",
      visibilityListener,
    );
  } else {
    expect(visibilityListener).toBeUndefined();
  }
}

function measuredRect(): DOMRect {
  return {
    bottom: 100,
    height: 100,
    left: 0,
    right: 100,
    top: 0,
    width: 100,
    x: 0,
    y: 0,
    toJSON: () => ({}),
  } as DOMRect;
}

async function expectStillGradient(container: HTMLElement) {
  await waitFor(() => {
    expect(
      container.querySelector('[data-focus-scene="still-gradient"]'),
    ).not.toBeNull();
  });
}
