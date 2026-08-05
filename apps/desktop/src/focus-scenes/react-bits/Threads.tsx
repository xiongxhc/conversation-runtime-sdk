import { Color, Mesh, Program, Renderer, Triangle } from "ogl";
import { useEffect, useRef, useState } from "react";

import { StillGradient } from "../StillGradient.js";
import type { FocusSceneProps } from "../types.js";
import {
  clampIntensity,
  createSceneResourceTracker,
  loseWebGlContext,
  startVisibilityAwareAnimation,
  voiceStateColors,
} from "./runtime.js";
import "./Threads.css";

const vertexShader = `
attribute vec2 position;
attribute vec2 uv;
varying vec2 vUv;
void main() {
  vUv = uv;
  gl_Position = vec4(position, 0.0, 1.0);
}
`;

const fragmentShader = `
precision highp float;
uniform float iTime;
uniform vec3 iResolution;
uniform vec3 uColor;
uniform float uAmplitude;
uniform float uDistance;
uniform vec2 uMouse;

#define PI 3.1415926538
const int u_line_count = 40;
const float u_line_width = 7.0;
const float u_line_blur = 10.0;

float Perlin2D(vec2 P) {
  vec2 Pi = floor(P);
  vec4 Pf_Pfmin1 = P.xyxy - vec4(Pi, Pi + 1.0);
  vec4 Pt = vec4(Pi.xy, Pi.xy + 1.0);
  Pt = Pt - floor(Pt * (1.0 / 71.0)) * 71.0;
  Pt += vec2(26.0, 161.0).xyxy;
  Pt *= Pt;
  Pt = Pt.xzxz * Pt.yyww;
  vec4 hash_x = fract(Pt * (1.0 / 951.135664));
  vec4 hash_y = fract(Pt * (1.0 / 642.949883));
  vec4 grad_x = hash_x - 0.49999;
  vec4 grad_y = hash_y - 0.49999;
  vec4 grad_results = inversesqrt(grad_x * grad_x + grad_y * grad_y) * (grad_x * Pf_Pfmin1.xzxz + grad_y * Pf_Pfmin1.yyww);
  grad_results *= 1.4142135623730950;
  vec2 blend = Pf_Pfmin1.xy * Pf_Pfmin1.xy * Pf_Pfmin1.xy * (Pf_Pfmin1.xy * (Pf_Pfmin1.xy * 6.0 - 15.0) + 10.0);
  vec4 blend2 = vec4(blend, vec2(1.0 - blend));
  return dot(grad_results, blend2.zxzx * blend2.wwyy);
}

float pixel(float count, vec2 resolution) {
  return (1.0 / max(resolution.x, resolution.y)) * count;
}

float lineFn(vec2 st, float width, float perc, float offset, vec2 mouse, float time, float amplitude, float distance) {
  float split_offset = perc * 0.4;
  float split_point = 0.1 + split_offset;
  float amplitude_normal = smoothstep(split_point, 0.7, st.x);
  float finalAmplitude = amplitude_normal * 0.5 * amplitude * (1.0 + (mouse.y - 0.5) * 0.2);
  float time_scaled = time / 10.0 + (mouse.x - 0.5);
  float blur = smoothstep(split_point, split_point + 0.05, st.x) * perc;
  float xnoise = mix(
    Perlin2D(vec2(time_scaled, st.x + perc) * 2.5),
    Perlin2D(vec2(time_scaled, st.x + time_scaled) * 3.5) / 1.5,
    st.x * 0.3
  );
  float y = 0.5 + (perc - 0.5) * distance + xnoise / 2.0 * finalAmplitude;
  float line_start = smoothstep(y + (width / 2.0) + (u_line_blur * pixel(1.0, iResolution.xy) * blur), y, st.y);
  float line_end = smoothstep(y, y - (width / 2.0) - (u_line_blur * pixel(1.0, iResolution.xy) * blur), st.y);
  return clamp((line_start - line_end) * (1.0 - smoothstep(0.0, 1.0, pow(perc, 0.3))), 0.0, 1.0);
}

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
  vec2 uv = fragCoord / iResolution.xy;
  float line_strength = 1.0;
  for (int i = 0; i < u_line_count; i++) {
    float p = float(i) / float(u_line_count);
    line_strength *= (1.0 - lineFn(
      uv,
      u_line_width * pixel(1.0, iResolution.xy) * (1.0 - p),
      p,
      PI * p,
      uMouse,
      iTime,
      uAmplitude,
      uDistance
    ));
  }
  float colorVal = 1.0 - line_strength;
  fragColor = vec4(uColor * colorVal, colorVal);
}

void main() {
  mainImage(gl_FragColor, gl_FragCoord.xy);
}
`;

export default function Threads(props: FocusSceneProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [failed, setFailed] = useState(false);
  const intensity = clampIntensity(props.intensity);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const resources = createSceneResourceTracker();

    try {
      const renderer = new Renderer({ alpha: true });
      const gl = renderer.gl;
      resources.add(() => loseWebGlContext(gl));
      gl.clearColor(0, 0, 0, 0);
      gl.enable(gl.BLEND);
      gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
      container.appendChild(gl.canvas);
      resources.add(() => gl.canvas.remove());
      const geometry = new Triangle(gl);
      resources.add(() => geometry.remove());
      const program = new Program(gl, {
        vertex: vertexShader,
        fragment: fragmentShader,
        uniforms: {
          iTime: { value: 0 },
          iResolution: { value: new Color(1, 1, 1) },
          uColor: { value: new Color(...hexToRgb(voiceStateColors[props.state])) },
          uAmplitude: { value: stateAmplitude(props.state) * (0.45 + intensity * 0.55) },
          uDistance: { value: 0 },
          uMouse: { value: new Float32Array([0.5, 0.5]) },
        },
      });
      resources.add(() => program.remove());
      const mesh = new Mesh(gl, { geometry, program });
      const resize = () => {
        const width = container.clientWidth || 1;
        const height = container.clientHeight || 1;
        const baseDpr = Math.min(window.devicePixelRatio || 1, 2);
        const longestSide = Math.max(width, height) * baseDpr;
        renderer.dpr = longestSide > 1920 ? (baseDpr * 1920) / longestSide : baseDpr;
        renderer.setSize(width, height);
        program.uniforms.iResolution.value.set(
          gl.canvas.width,
          gl.canvas.height,
          gl.canvas.width / Math.max(1, gl.canvas.height),
        );
      };

      window.addEventListener("resize", resize);
      resources.add(() => window.removeEventListener("resize", resize));
      resize();
      const stopAnimation = startVisibilityAwareAnimation({
        cancelFrame: cancelAnimationFrame,
        document,
        onError: () => {
          resources.release();
          setFailed(true);
        },
        renderFrame: (time) => {
          program.uniforms.iTime.value = time * 0.001;
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
      className="threads-container"
      data-focus-scene="threads"
      data-voice-state={props.state}
      style={{ opacity: intensity }}
    />
  );
}

function hexToRgb(hex: string): [number, number, number] {
  const value = hex.replace("#", "");
  return [0, 2, 4].map((offset) => parseInt(value.slice(offset, offset + 2), 16) / 255) as [
    number,
    number,
    number,
  ];
}

function stateAmplitude(state: FocusSceneProps["state"]): number {
  if (state === "speaking") return 1.25;
  if (state === "interrupted") return 0.3;
  if (state === "error") return 0.2;
  return 0.75;
}
