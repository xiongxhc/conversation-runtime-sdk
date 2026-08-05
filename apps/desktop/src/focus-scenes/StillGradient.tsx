import type { CSSProperties } from "react";

import type { FocusSceneProps } from "./types.js";

export function StillGradient({ intensity }: FocusSceneProps) {
  const boundedIntensity = Number.isFinite(intensity)
    ? Math.min(1, Math.max(0, intensity))
    : 0.55;

  return (
    <div
      aria-hidden="true"
      data-focus-scene="still-gradient"
      style={
        {
          background:
            "radial-gradient(circle at 20% 20%, rgba(104, 156, 255, 0.62), transparent 48%), radial-gradient(circle at 80% 70%, rgba(182, 113, 255, 0.48), transparent 52%), #10131d",
          inset: 0,
          opacity: boundedIntensity,
          pointerEvents: "none",
          position: "absolute",
        } satisfies CSSProperties
      }
    />
  );
}
