// @vitest-environment jsdom

import { cleanup, render, waitFor } from "@testing-library/react";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  createLazySceneRenderer,
  focusScenes,
  registerSceneRenderer,
  resetSceneRenderersForTests,
  resolveScene,
} from "../src/focus-scenes/registry.js";
import { startVisibilityAwareAnimation } from "../src/focus-scenes/react-bits/runtime.js";
import type { FocusSceneProps } from "../src/focus-scenes/types.js";

const rendererConstructor = vi.hoisted(() => vi.fn());

vi.mock("ogl", async (importOriginal) => {
  const actual = await importOriginal<typeof import("ogl")>();
  return { ...actual, Renderer: rendererConstructor };
});

afterEach(() => {
  cleanup();
  rendererConstructor.mockReset();
  resetSceneRenderersForTests();
});

describe("focus scene registry", () => {
  it("defines seven unique scene identifiers", () => {
    expect(focusScenes.map((scene) => scene.id)).toEqual([
      "soft-aurora",
      "silk",
      "threads",
      "prism",
      "orb",
      "still-gradient",
      "none",
    ]);
    expect(new Set(focusScenes.map((scene) => scene.id))).toHaveLength(7);
  });

  it("falls back to Soft Aurora for an unknown scene", () => {
    expect(resolveScene("unknown").id).toBe("soft-aurora");
  });

  it("marks only Orb as integrating voice presence", () => {
    expect(resolveScene("orb").integratesVoicePresence).toBe(true);
    expect(focusScenes.filter((scene) => scene.id !== "orb").every((scene) => !scene.integratesVoicePresence)).toBe(true);
  });

  it("renders Still Gradient and None statically", () => {
    expect(renderToStaticMarkup(sceneElement("still-gradient"))).toContain(
      'data-focus-scene="still-gradient"',
    );
    expect(renderToStaticMarkup(sceneElement("none"))).toBe("");
  });

  it("starts each lazy built-in behind Still Gradient", () => {
    for (const id of ["soft-aurora", "silk", "threads", "prism", "orb"] as const) {
      const firstRender = renderToStaticMarkup(sceneElement(id, false));

      expect(firstRender).toContain('data-focus-scene="still-gradient"');
    }
  });

  it("resolves a lazy renderer without loading it more than once", async () => {
    const loader = vi.fn(async () => ({
      default: () => <div data-focus-scene="lazy-test" />,
    }));
    registerSceneRenderer("silk", createLazySceneRenderer(loader));

    expect(renderToStaticMarkup(sceneElement("silk", false))).toContain(
      'data-focus-scene="still-gradient"',
    );

    await vi.waitFor(() => {
      expect(renderToStaticMarkup(sceneElement("silk", false))).toContain(
        'data-focus-scene="lazy-test"',
      );
    });
    expect(loader).toHaveBeenCalledOnce();
  });

  it("uses Still Gradient for reduced motion without rendering an animated scene", () => {
    const AnimatedScene = vi.fn(() => <div data-focus-scene="silk" />);
    registerSceneRenderer("silk", AnimatedScene);

    expect(renderToStaticMarkup(sceneElement("silk", true))).toContain(
      'data-focus-scene="still-gradient"',
    );
    expect(AnimatedScene).not.toHaveBeenCalled();
  });

  it("exposes all five adapted renderer modules", async () => {
    const modules = await Promise.all([
      import("../src/focus-scenes/react-bits/SoftAurora.js"),
      import("../src/focus-scenes/react-bits/Silk.js"),
      import("../src/focus-scenes/react-bits/Threads.js"),
      import("../src/focus-scenes/react-bits/Prism.js"),
      import("../src/focus-scenes/react-bits/Orb.js"),
    ]);

    expect(modules.every((module) => typeof module.default === "function")).toBe(true);
  });

  it("bounds scene state and intensity at the renderer boundary", () => {
    const received: FocusSceneProps[] = [];
    registerSceneRenderer("threads", (props) => {
      received.push(props);
      return <div data-focus-scene="threads" />;
    });

    renderToStaticMarkup(
      sceneElement("threads", false, {
        intensity: 2,
        state: "unknown" as FocusSceneProps["state"],
      }),
    );

    expect(received).toEqual([{ intensity: 1, reducedMotion: false, state: "idle" }]);
    expect(
      renderToStaticMarkup(
        sceneElement("still-gradient", false, { intensity: -1 }),
      ),
    ).toContain("opacity:0");
  });

  it("falls back to Still Gradient when a lazy scene fails to load", async () => {
    const FailedScene = createLazySceneRenderer(async () => {
      throw new Error("WebGL scene failed to load");
    });
    registerSceneRenderer("prism", FailedScene);

    expect(renderToStaticMarkup(sceneElement("prism", false))).toContain(
      'data-focus-scene="still-gradient"',
    );

    await vi.waitFor(() => {
      expect(renderToStaticMarkup(sceneElement("prism", false))).toContain(
        'data-focus-scene="still-gradient"',
      );
    });
  });

  it("falls back to Still Gradient when WebGL construction fails", async () => {
    rendererConstructor.mockImplementationOnce(() => {
      throw new Error("WebGL unavailable");
    });
    const { default: SoftAurora } = await import(
      "../src/focus-scenes/react-bits/SoftAurora.js"
    );

    const view = render(
      <SoftAurora intensity={0.55} reducedMotion={false} state="idle" />,
    );

    await waitFor(() => {
      expect(
        view.container.querySelector('[data-focus-scene="still-gradient"]'),
      ).not.toBeNull();
    });
    expect(rendererConstructor).toHaveBeenCalledOnce();
  });

  it("contains renderer failures without leaving the static fallback", async () => {
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => {});
    registerSceneRenderer("silk", () => {
      throw new Error("renderer crashed");
    });
    const scene = resolveScene("silk");

    const view = render(
      createElement(scene.Renderer, {
        intensity: 0.55,
        reducedMotion: false,
        state: "idle",
      }),
    );

    await waitFor(() => {
      expect(
        view.container.querySelector('[data-focus-scene="still-gradient"]'),
      ).not.toBeNull();
    });
    consoleError.mockRestore();
  });

  it("pauses animation while hidden and removes lifecycle hooks on cleanup", () => {
    const visibility = new FakeVisibilityDocument();
    const scheduled = new Map<number, FrameRequestCallback>();
    const cancelFrame = vi.fn((id: number) => scheduled.delete(id));
    const requestFrame = vi.fn((callback: FrameRequestCallback) => {
      const id = requestFrame.mock.calls.length;
      scheduled.set(id, callback);
      return id;
    });
    const renderFrame = vi.fn();

    const cleanup = startVisibilityAwareAnimation({
      cancelFrame,
      document: visibility,
      renderFrame,
      requestFrame,
    });

    expect(scheduled.size).toBe(1);
    visibility.setVisibility("hidden");
    expect(cancelFrame).toHaveBeenCalledOnce();
    expect(scheduled.size).toBe(0);

    visibility.setVisibility("visible");
    expect(scheduled.size).toBe(1);
    const [frameId, frame] = [...scheduled.entries()][0];
    scheduled.delete(frameId);
    frame(16);
    expect(renderFrame).toHaveBeenCalledWith(16);

    cleanup();
    expect(visibility.listenerCount).toBe(0);
    expect(scheduled.size).toBe(0);
  });

  it("releases visibility state when scheduling the next frame fails", () => {
    const visibility = new FakeVisibilityDocument();
    let firstFrame: FrameRequestCallback | undefined;
    const requestFrame = vi.fn((callback: FrameRequestCallback) => {
      if (!firstFrame) {
        firstFrame = callback;
        return 41;
      }
      throw new Error("RAF unavailable");
    });
    const onError = vi.fn();

    const cleanup = startVisibilityAwareAnimation({
      cancelFrame: vi.fn(),
      document: visibility,
      onError,
      renderFrame: vi.fn(),
      requestFrame,
    });

    expect(() => firstFrame?.(16)).not.toThrow();
    expect(onError).toHaveBeenCalledOnce();
    expect(visibility.listenerCount).toBe(0);
    cleanup();
  });

  it("renders Orb as the only central voice presence", async () => {
    renderToStaticMarkup(sceneElement("orb", false, { state: "speaking" }));

    await vi.waitFor(() => {
      const markup = renderToStaticMarkup(
        sceneElement("orb", false, { state: "speaking" }),
      );

      expect(markup.match(/data-voice-presence="integrated"/g)).toHaveLength(1);
      expect(markup).not.toContain('data-voice-presence="separate"');
    });
  });
});

function sceneElement(
  id: string,
  reducedMotion = true,
  overrides: Partial<FocusSceneProps> = {},
) {
  const scene = resolveScene(id);
  return createElement(scene.Renderer, {
    intensity: 0.55,
    reducedMotion,
    state: "idle",
    ...overrides,
  });
}

class FakeVisibilityDocument {
  visibilityState: DocumentVisibilityState = "visible";
  readonly #listeners = new Set<() => void>();

  get listenerCount() {
    return this.#listeners.size;
  }

  addEventListener(type: string, listener: () => void) {
    if (type === "visibilitychange") this.#listeners.add(listener);
  }

  removeEventListener(type: string, listener: () => void) {
    if (type === "visibilitychange") this.#listeners.delete(listener);
  }

  setVisibility(visibilityState: DocumentVisibilityState) {
    this.visibilityState = visibilityState;
    for (const listener of this.#listeners) listener();
  }
}
