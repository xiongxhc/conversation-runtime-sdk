import { useEffect, useMemo, useRef, useState } from "react";
import {
  ACESFilmicToneMapping,
  Color,
  Mesh,
  PerspectiveCamera,
  PlaneGeometry,
  Scene,
  ShaderMaterial,
  SRGBColorSpace,
  WebGLRenderer,
  type IUniform,
} from "three";

import { StillGradient } from "../StillGradient.js";
import type { FocusSceneProps } from "../types.js";
import {
  clampIntensity,
  createSceneResourceTracker,
  startVisibilityAwareAnimation,
  voiceStateColors,
} from "./runtime.js";
import "./Silk.css";

interface SilkUniforms {
  uSpeed: IUniform<number>;
  uScale: IUniform<number>;
  uNoiseIntensity: IUniform<number>;
  uColor: IUniform<Color>;
  uRotation: IUniform<number>;
  uTime: IUniform<number>;
  [uniform: string]: IUniform;
}

const vertexShader = `
varying vec2 vUv;
varying vec3 vPosition;
void main() {
  vPosition = position;
  vUv = uv;
  gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
}
`;

const fragmentShader = `
varying vec2 vUv;
varying vec3 vPosition;
uniform float uTime;
uniform vec3 uColor;
uniform float uSpeed;
uniform float uScale;
uniform float uRotation;
uniform float uNoiseIntensity;
const float e = 2.71828182845904523536;

float noise(vec2 texCoord) {
  float G = e;
  vec2 r = (G * sin(G * texCoord));
  return fract(r.x * r.y * (1.0 + texCoord.x));
}

vec2 rotateUvs(vec2 uv, float angle) {
  float c = cos(angle);
  float s = sin(angle);
  return mat2(c, -s, s, c) * uv;
}

void main() {
  float rnd = noise(gl_FragCoord.xy);
  vec2 uv = rotateUvs(vUv * uScale, uRotation);
  vec2 tex = uv * uScale;
  float tOffset = uSpeed * uTime;
  tex.y += 0.03 * sin(8.0 * tex.x - tOffset);
  float pattern = 0.6 + 0.4 * sin(5.0 * (tex.x + tex.y + cos(3.0 * tex.x + 5.0 * tex.y) + 0.02 * tOffset) + sin(20.0 * (tex.x + tex.y - 0.1 * tOffset)));
  vec4 col = vec4(uColor, 1.0) * vec4(pattern) - rnd / 15.0 * uNoiseIntensity;
  col.a = 1.0;
  gl_FragColor = col;
}
`;

export default function Silk(props: FocusSceneProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const rendererRef = useRef<WebGLRenderer | null>(null);
  const [failed, setFailed] = useState(false);
  const intensity = clampIntensity(props.intensity);
  const color = voiceStateColors[props.state];
  const uniforms = useMemo<SilkUniforms>(
    () => ({
      uSpeed: { value: 2 + intensity * stateSpeed(props.state) },
      uScale: { value: 1 },
      uNoiseIntensity: { value: 0.5 + intensity },
      uColor: { value: new Color(color) },
      uRotation: { value: 0 },
      uTime: { value: 0 },
    }),
    [],
  );

  useEffect(() => {
    uniforms.uSpeed.value = 2 + intensity * stateSpeed(props.state);
    uniforms.uNoiseIntensity.value = 0.5 + intensity;
    uniforms.uColor.value.set(color);
  }, [color, intensity, props.state, uniforms]);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const resources = createSceneResourceTracker();
    const handleFailure = () => {
      resources.release();
      setFailed(true);
    };

    try {
      const renderer = new WebGLRenderer({
        alpha: true,
        antialias: true,
        powerPreference: "high-performance",
      });
      rendererRef.current = renderer;
      resources.add(() => disposeSilkRenderer(rendererRef));
      renderer.outputColorSpace = SRGBColorSpace;
      renderer.toneMapping = ACESFilmicToneMapping;
      renderer.setClearColor(0x000000, 0);
      container.appendChild(renderer.domElement);
      resources.add(() => renderer.domElement.remove());

      const scene = new Scene();
      const camera = new PerspectiveCamera(75, 1, 0.1, 1000);
      camera.position.z = 5;
      camera.lookAt(0, 0, 0);
      const geometry = new PlaneGeometry(1, 1, 1, 1);
      resources.add(() => geometry.dispose());
      const material = new ShaderMaterial({
        fragmentShader,
        uniforms,
        vertexShader,
      });
      resources.add(() => material.dispose());
      const mesh = new Mesh(geometry, material);
      scene.add(mesh);
      resources.add(() => scene.remove(mesh));

      const resize = () => {
        const bounds = container.getBoundingClientRect();
        const width = bounds.width || container.clientWidth || 1;
        const height = bounds.height || container.clientHeight || 1;
        const pixelRatio = Math.min(2, Math.max(1, window.devicePixelRatio || 1));
        renderer.setPixelRatio(pixelRatio);
        renderer.setSize(width, height);
        camera.aspect = width / height;
        camera.updateProjectionMatrix();
        const viewportHeight = 2 * Math.tan((camera.fov * Math.PI) / 360) * camera.position.z;
        mesh.scale.set(viewportHeight * camera.aspect, viewportHeight, 1);
      };
      const handleResize = () => {
        try {
          resize();
        } catch {
          handleFailure();
        }
      };

      window.addEventListener("resize", handleResize);
      resources.add(() => window.removeEventListener("resize", handleResize));
      resize();

      let previousTime: number | undefined;
      const stopAnimation = startVisibilityAwareAnimation({
        cancelFrame: cancelAnimationFrame,
        document,
        onError: handleFailure,
        renderFrame: (time) => {
          const delta = previousTime === undefined ? 0 : (time - previousTime) / 1000;
          previousTime = time;
          uniforms.uTime.value += 0.1 * delta;
          renderer.render(scene, camera);
        },
        requestFrame: requestAnimationFrame,
      });
      resources.add(stopAnimation);
    } catch {
      handleFailure();
    }

    return () => resources.release();
  }, [uniforms]);

  if (failed) return <StillGradient {...props} intensity={intensity} />;

  return (
    <div
      ref={containerRef}
      className="silk-container"
      data-focus-scene="silk"
      data-voice-state={props.state}
      style={{ opacity: intensity }}
    />
  );
}

export function disposeSilkRenderer<T extends Pick<WebGLRenderer, "dispose" | "domElement" | "forceContextLoss">>(
  rendererRef: { current: T | null },
): void {
  const renderer = rendererRef.current;
  if (!renderer) return;
  rendererRef.current = null;
  safelyDispose(() => renderer.dispose());
  safelyDispose(() => renderer.forceContextLoss());
  safelyDispose(() => renderer.domElement.remove());
}

function stateSpeed(state: FocusSceneProps["state"]): number {
  if (state === "speaking") return 5;
  if (state === "interrupted") return 6;
  if (state === "error") return 0.5;
  return 3;
}

function safelyDispose(dispose: () => void): void {
  try {
    dispose();
  } catch {}
}
