import { Component, createElement, lazy, Suspense, type ReactNode } from "react";

import { NoneScene } from "./NoneScene.js";
import { StillGradient } from "./StillGradient.js";
import {
  focusSceneIds,
  type FocusSceneId,
  type FocusSceneMetadata,
  type FocusSceneProps,
  type FocusSceneRenderer,
  type ResolvedFocusScene,
  type VoiceVisualState,
} from "./types.js";

export const focusScenes: readonly FocusSceneMetadata[] = [
  { id: "soft-aurora", label: "Soft Aurora", integratesVoicePresence: false },
  { id: "silk", label: "Silk", integratesVoicePresence: false },
  { id: "threads", label: "Threads", integratesVoicePresence: false },
  { id: "prism", label: "Prism", integratesVoicePresence: false },
  { id: "orb", label: "Orb", integratesVoicePresence: true },
  { id: "still-gradient", label: "Still Gradient", integratesVoicePresence: false },
  { id: "none", label: "None", integratesVoicePresence: false },
];

export type AnimatedFocusSceneId = Exclude<FocusSceneId, "still-gradient" | "none">;

const renderedSceneIds = new Set<FocusSceneId>(focusSceneIds);
const injectedRenderers = new Map<AnimatedFocusSceneId, FocusSceneRenderer>();
const softAurora = focusScenes[0];
const voiceStates = new Set<VoiceVisualState>([
  "idle",
  "listening",
  "thinking",
  "speaking",
  "interrupted",
  "error",
]);

type SceneModule = { default: FocusSceneRenderer };
type SceneModuleLoader = () => Promise<SceneModule>;

interface SceneErrorBoundaryProps {
  children?: ReactNode;
  fallback: ReactNode;
}

interface SceneErrorBoundaryState {
  failed: boolean;
}

class SceneErrorBoundary extends Component<
  SceneErrorBoundaryProps,
  SceneErrorBoundaryState
> {
  state: SceneErrorBoundaryState = { failed: false };

  static getDerivedStateFromError(): SceneErrorBoundaryState {
    return { failed: true };
  }

  render() {
    return this.state.failed ? this.props.fallback : this.props.children;
  }
}

export function createLazySceneRenderer(loader: SceneModuleLoader): FocusSceneRenderer {
  return lazy(async () => {
    try {
      return await loader();
    } catch {
      return { default: StillGradient };
    }
  });
}

const builtInRenderers: Record<AnimatedFocusSceneId, FocusSceneRenderer> = {
  "soft-aurora": createLazySceneRenderer(() => import("./react-bits/SoftAurora.js")),
  silk: createLazySceneRenderer(() => import("./react-bits/Silk.js")),
  threads: createLazySceneRenderer(() => import("./react-bits/Threads.js")),
  prism: createLazySceneRenderer(() => import("./react-bits/Prism.js")),
  orb: createLazySceneRenderer(() => import("./react-bits/Orb.js")),
};

const animatedSceneRenderers = Object.fromEntries(
  (focusSceneIds.filter(
    (id): id is AnimatedFocusSceneId => id !== "still-gradient" && id !== "none",
  )).map((id) => [id, createAnimatedSceneRenderer(id)]),
) as Record<AnimatedFocusSceneId, FocusSceneRenderer>;

export function isFocusSceneId(value: unknown): value is FocusSceneId {
  return typeof value === "string" && renderedSceneIds.has(value as FocusSceneId);
}

export function registerSceneRenderer(id: AnimatedFocusSceneId, renderer: FocusSceneRenderer): void {
  injectedRenderers.set(id, renderer);
}

export function resetSceneRenderersForTests(): void {
  injectedRenderers.clear();
}

export function resolveScene(id: unknown): ResolvedFocusScene {
  const scene = isFocusSceneId(id) ? focusScenes.find((candidate) => candidate.id === id) ?? softAurora : softAurora;

  if (scene.id === "none") {
    return { ...scene, Renderer: NoneScene };
  }

  if (scene.id === "still-gradient") {
    return { ...scene, Renderer: StillGradient };
  }

  return { ...scene, Renderer: animatedSceneRenderers[scene.id] };
}

function createAnimatedSceneRenderer(id: AnimatedFocusSceneId): FocusSceneRenderer {
  return function AnimatedSceneRenderer(props) {
    const normalizedProps = normalizeSceneProps(props);
    if (normalizedProps.reducedMotion) {
      return createElement(StillGradient, normalizedProps);
    }

    const Renderer = injectedRenderers.get(id) ?? builtInRenderers[id];
    return createElement(
      SceneErrorBoundary,
      { fallback: createElement(StillGradient, normalizedProps) },
      createElement(
        Suspense,
        { fallback: createElement(StillGradient, normalizedProps) },
        createElement(Renderer, normalizedProps),
      ),
    );
  };
}

function normalizeSceneProps(props: FocusSceneProps): FocusSceneProps {
  return {
    intensity: Number.isFinite(props.intensity)
      ? Math.min(1, Math.max(0, props.intensity))
      : 0.55,
    reducedMotion: props.reducedMotion === true,
    state: voiceStates.has(props.state) ? props.state : "idle",
  };
}
