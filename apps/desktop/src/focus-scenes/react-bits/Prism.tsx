import { Mesh, Program, Renderer, Triangle } from "ogl";
import { useEffect, useRef, useState } from "react";

import { StillGradient } from "../StillGradient.js";
import type { FocusSceneProps } from "../types.js";
import {
  clampIntensity,
  createSceneResourceTracker,
  loseWebGlContext,
  startVisibilityAwareAnimation,
} from "./runtime.js";
import "./Prism.css";

const vertexShader = `
attribute vec2 position;
void main() {
  gl_Position = vec4(position, 0.0, 1.0);
}
`;

const fragmentShader = `
precision highp float;
uniform vec2 iResolution;
uniform float iTime;
uniform float uHeight;
uniform float uBaseHalf;
uniform mat3 uRot;
uniform int uUseBaseWobble;
uniform float uGlow;
uniform vec2 uOffsetPx;
uniform float uNoise;
uniform float uSaturation;
uniform float uScale;
uniform float uHueShift;
uniform float uColorFreq;
uniform float uBloom;
uniform float uCenterShift;
uniform float uInvBaseHalf;
uniform float uInvHeight;
uniform float uMinAxis;
uniform float uPxScale;
uniform float uTimeScale;

vec4 tanh4(vec4 x) {
  vec4 e2x = exp(2.0 * x);
  return (e2x - 1.0) / (e2x + 1.0);
}

float rand(vec2 co) {
  return fract(sin(dot(co, vec2(12.9898, 78.233))) * 43758.5453123);
}

float sdOctaAnisoInv(vec3 p) {
  vec3 q = vec3(abs(p.x) * uInvBaseHalf, abs(p.y) * uInvHeight, abs(p.z) * uInvBaseHalf);
  float m = q.x + q.y + q.z - 1.0;
  return m * uMinAxis * 0.5773502691896258;
}

float sdPyramidUpInv(vec3 p) {
  return max(sdOctaAnisoInv(p), -p.y);
}

mat3 hueRotation(float a) {
  float c = cos(a), s = sin(a);
  mat3 W = mat3(
    0.299, 0.587, 0.114,
    0.299, 0.587, 0.114,
    0.299, 0.587, 0.114
  );
  mat3 U = mat3(
     0.701, -0.587, -0.114,
    -0.299,  0.413, -0.114,
    -0.300, -0.588,  0.886
  );
  mat3 V = mat3(
     0.168, -0.331,  0.500,
     0.328,  0.035, -0.500,
    -0.497,  0.296,  0.201
  );
  return W + U * c + V * s;
}

void main() {
  vec2 f = (gl_FragCoord.xy - 0.5 * iResolution.xy - uOffsetPx) * uPxScale;
  float z = 5.0;
  float d = 0.0;
  vec3 p;
  vec4 o = vec4(0.0);
  mat2 wob = mat2(1.0);

  if (uUseBaseWobble == 1) {
    float t = iTime * uTimeScale;
    float c0 = cos(t);
    float c1 = cos(t + 33.0);
    float c2 = cos(t + 11.0);
    wob = mat2(c0, c1, c2, c0);
  }

  const int STEPS = 100;
  for (int i = 0; i < STEPS; i++) {
    p = vec3(f, z);
    p.xz = p.xz * wob;
    p = uRot * p;
    vec3 q = p;
    q.y += uCenterShift;
    d = 0.1 + 0.2 * abs(sdPyramidUpInv(q));
    z -= d;
    o += (sin((p.y + z) * uColorFreq + vec4(0.0, 1.0, 2.0, 3.0)) + 1.0) / d;
  }

  o = tanh4(o * o * (uGlow * uBloom) / 1e5);
  vec3 col = o.rgb;
  float n = rand(gl_FragCoord.xy + vec2(iTime));
  col += (n - 0.5) * uNoise;
  col = clamp(col, 0.0, 1.0);
  float luminance = dot(col, vec3(0.2126, 0.7152, 0.0722));
  col = clamp(mix(vec3(luminance), col, uSaturation), 0.0, 1.0);
  if (abs(uHueShift) > 0.0001) {
    col = clamp(hueRotation(uHueShift) * col, 0.0, 1.0);
  }
  gl_FragColor = vec4(col, o.a);
}
`;

export default function Prism(props: FocusSceneProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [failed, setFailed] = useState(false);
  const intensity = clampIntensity(props.intensity);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const resources = createSceneResourceTracker();

    try {
      const height = 3.5;
      const baseHalf = 2.75;
      const scale = 3.6;
      const dpr = Math.min(2, window.devicePixelRatio || 1);
      const renderer = new Renderer({ dpr, alpha: true, antialias: false });
      const gl = renderer.gl;
      resources.add(() => loseWebGlContext(gl));
      gl.disable(gl.DEPTH_TEST);
      gl.disable(gl.CULL_FACE);
      gl.disable(gl.BLEND);
      container.appendChild(gl.canvas);
      resources.add(() => gl.canvas.remove());
      const resolution = new Float32Array(2);
      const offset = new Float32Array(2);
      const rotation = new Float32Array([1, 0, 0, 0, 1, 0, 0, 0, 1]);
      const geometry = new Triangle(gl);
      resources.add(() => geometry.remove());
      const program = new Program(gl, {
        vertex: vertexShader,
        fragment: fragmentShader,
        uniforms: {
          iResolution: { value: resolution },
          iTime: { value: 0 },
          uHeight: { value: height },
          uBaseHalf: { value: baseHalf },
          uUseBaseWobble: { value: 1 },
          uRot: { value: rotation },
          uGlow: { value: 0.35 + intensity * 0.85 },
          uOffsetPx: { value: offset },
          uNoise: { value: props.state === "error" ? 0.05 : 0.25 * intensity },
          uSaturation: { value: props.state === "error" ? 0.2 : 1.5 },
          uScale: { value: scale },
          uHueShift: { value: stateHue(props.state) },
          uColorFreq: { value: 1 },
          uBloom: { value: 0.35 + intensity * 0.65 },
          uCenterShift: { value: height * 0.25 },
          uInvBaseHalf: { value: 1 / baseHalf },
          uInvHeight: { value: 1 / height },
          uMinAxis: { value: Math.min(baseHalf, height) },
          uPxScale: { value: 1 },
          uTimeScale: { value: stateTimeScale(props.state) * (0.25 + intensity * 0.5) },
        },
      });
      resources.add(() => program.remove());
      const mesh = new Mesh(gl, { geometry, program });
      const resize = () => {
        renderer.setSize(container.clientWidth || 1, container.clientHeight || 1);
        resolution[0] = gl.drawingBufferWidth;
        resolution[1] = gl.drawingBufferHeight;
        program.uniforms.uPxScale.value = 1 / ((gl.drawingBufferHeight || 1) * 0.1 * scale);
      };

      window.addEventListener("resize", resize);
      resources.add(() => window.removeEventListener("resize", resize));
      resize();
      const startedAt = performance.now();
      const stopAnimation = startVisibilityAwareAnimation({
        cancelFrame: cancelAnimationFrame,
        document,
        onError: () => {
          resources.release();
          setFailed(true);
        },
        renderFrame: (time) => {
          program.uniforms.iTime.value = (time - startedAt) * 0.001;
          renderer.render({ scene: mesh });
        },
        requestFrame: requestAnimationFrame,
      });
      resources.add(stopAnimation);
    } catch {
      resources.release();
      setFailed(true);
    }

    return () => resources.release();
  }, [intensity, props.state]);

  if (failed) return <StillGradient {...props} intensity={intensity} />;

  return (
    <div
      ref={containerRef}
      className="prism-container"
      data-focus-scene="prism"
      data-voice-state={props.state}
      style={{ opacity: intensity }}
    />
  );
}

function stateHue(state: FocusSceneProps["state"]): number {
  if (state === "listening") return -0.35;
  if (state === "speaking") return 0.5;
  if (state === "interrupted") return 0.1;
  return 0;
}

function stateTimeScale(state: FocusSceneProps["state"]): number {
  if (state === "speaking") return 0.8;
  if (state === "interrupted") return 1;
  if (state === "error") return 0.1;
  return 0.5;
}
