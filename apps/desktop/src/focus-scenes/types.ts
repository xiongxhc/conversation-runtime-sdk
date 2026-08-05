import type { ComponentType } from "react";

export const focusSceneIds = ["soft-aurora", "silk", "threads", "prism", "orb", "still-gradient", "none"] as const;

export type FocusSceneId = (typeof focusSceneIds)[number];

export interface FocusSceneMetadata {
  id: FocusSceneId;
  label: string;
  integratesVoicePresence: boolean;
}

export type VoiceVisualState =
  | "idle"
  | "listening"
  | "thinking"
  | "speaking"
  | "interrupted"
  | "error";

export interface FocusSceneProps {
  state: VoiceVisualState;
  intensity: number;
  reducedMotion: boolean;
}

export type FocusSceneRendererProps = FocusSceneProps;

export type FocusSceneRenderer = ComponentType<FocusSceneProps>;

export interface ResolvedFocusScene extends FocusSceneMetadata {
  Renderer: FocusSceneRenderer;
}
