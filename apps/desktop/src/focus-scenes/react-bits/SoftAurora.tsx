import { Mesh, Program, Renderer, Triangle } from "ogl";
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
import "./SoftAurora.css";

const vertexShader = `
attribute vec2 uv;
attribute vec2 position;
varying vec2 vUv;
void main() {
  vUv = uv;
  gl_Position = vec4(position, 0, 1);
}
`;

const fragmentShader = `
precision highp float;

uniform float uTime;
uniform vec3 uResolution;
uniform float uSpeed;
uniform float uScale;
uniform float uBrightness;
uniform vec3 uColor1;
uniform vec3 uColor2;
uniform float uNoiseFreq;
uniform float uNoiseAmp;
uniform float uBandHeight;
uniform float uBandSpread;
uniform float uOctaveDecay;
uniform float uLayerOffset;
uniform float uColorSpeed;

#define TAU 6.28318

vec3 gradientHash(vec3 p) {
  p = vec3(
    dot(p, vec3(127.1, 311.7, 234.6)),
    dot(p, vec3(269.5, 183.3, 198.3)),
    dot(p, vec3(169.5, 283.3, 156.9))
  );
  vec3 h = fract(sin(p) * 43758.5453123);
  float phi = acos(2.0 * h.x - 1.0);
  float theta = TAU * h.y;
  return vec3(cos(theta) * sin(phi), sin(theta) * cos(phi), cos(phi));
}

float quinticSmooth(float t) {
  float t2 = t * t;
  float t3 = t * t2;
  return 6.0 * t3 * t2 - 15.0 * t2 * t2 + 10.0 * t3;
}

vec3 cosineGradient(float t, vec3 a, vec3 b, vec3 c, vec3 d) {
  return a + b * cos(TAU * (c * t + d));
}

float perlin3D(float amplitude, float frequency, float px, float py, float pz) {
  float x = px * frequency;
  float y = py * frequency;

  float fx = floor(x); float fy = floor(y); float fz = floor(pz);
  float cx = ceil(x);  float cy = ceil(y);  float cz = ceil(pz);

  vec3 g000 = gradientHash(vec3(fx, fy, fz));
  vec3 g100 = gradientHash(vec3(cx, fy, fz));
  vec3 g010 = gradientHash(vec3(fx, cy, fz));
  vec3 g110 = gradientHash(vec3(cx, cy, fz));
  vec3 g001 = gradientHash(vec3(fx, fy, cz));
  vec3 g101 = gradientHash(vec3(cx, fy, cz));
  vec3 g011 = gradientHash(vec3(fx, cy, cz));
  vec3 g111 = gradientHash(vec3(cx, cy, cz));

  float d000 = dot(g000, vec3(x - fx, y - fy, pz - fz));
  float d100 = dot(g100, vec3(x - cx, y - fy, pz - fz));
  float d010 = dot(g010, vec3(x - fx, y - cy, pz - fz));
  float d110 = dot(g110, vec3(x - cx, y - cy, pz - fz));
  float d001 = dot(g001, vec3(x - fx, y - fy, pz - cz));
  float d101 = dot(g101, vec3(x - cx, y - fy, pz - cz));
  float d011 = dot(g011, vec3(x - fx, y - cy, pz - cz));
  float d111 = dot(g111, vec3(x - cx, y - cy, pz - cz));

  float sx = quinticSmooth(x - fx);
  float sy = quinticSmooth(y - fy);
  float sz = quinticSmooth(pz - fz);

  float lx00 = mix(d000, d100, sx);
  float lx10 = mix(d010, d110, sx);
  float lx01 = mix(d001, d101, sx);
  float lx11 = mix(d011, d111, sx);
  float ly0 = mix(lx00, lx10, sy);
  float ly1 = mix(lx01, lx11, sy);
  return amplitude * mix(ly0, ly1, sz);
}

float auroraGlow(float t, vec2 shift) {
  vec2 uv = gl_FragCoord.xy / uResolution.y;
  uv += shift;
  float noiseVal = 0.0;
  float freq = uNoiseFreq;
  float amp = uNoiseAmp;
  vec2 samplePos = uv * uScale;

  for (float i = 0.0; i < 3.0; i += 1.0) {
    noiseVal += perlin3D(amp, freq, samplePos.x, samplePos.y, t);
    amp *= uOctaveDecay;
    freq *= 2.0;
  }

  float yBand = uv.y * 10.0 - uBandHeight * 10.0;
  return 0.3 * max(exp(uBandSpread * (1.0 - 1.1 * abs(noiseVal + yBand))), 0.0);
}

void main() {
  vec2 uv = gl_FragCoord.xy / uResolution.xy;
  float t = uSpeed * 0.4 * uTime;
  vec3 col = vec3(0.0);
  col += 0.99 * auroraGlow(t, vec2(0.0)) * cosineGradient(uv.x + uTime * uSpeed * 0.2 * uColorSpeed, vec3(0.5), vec3(0.5), vec3(1.0), vec3(0.3, 0.20, 0.20)) * uColor1;
  col += 0.99 * auroraGlow(t + uLayerOffset, vec2(0.0)) * cosineGradient(uv.x + uTime * uSpeed * 0.1 * uColorSpeed, vec3(0.5), vec3(0.5), vec3(2.0, 1.0, 0.0), vec3(0.5, 0.20, 0.25)) * uColor2;
  col *= uBrightness;
  float alpha = clamp(length(col), 0.0, 1.0);
  gl_FragColor = vec4(col, alpha);
}
`;

export default function SoftAurora(props: FocusSceneProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [failed, setFailed] = useState(false);
  const intensity = clampIntensity(props.intensity);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const resources = createSceneResourceTracker();

    try {
      const renderer = new Renderer({ alpha: true, premultipliedAlpha: false });
      const gl = renderer.gl;
      resources.add(() => loseWebGlContext(gl));
      gl.clearColor(0, 0, 0, 0);
      container.appendChild(gl.canvas);
      resources.add(() => gl.canvas.remove());
      const geometry = new Triangle(gl);
      resources.add(() => geometry.remove());
      const program = new Program(gl, {
        vertex: vertexShader,
        fragment: fragmentShader,
        uniforms: {
          uTime: { value: 0 },
          uResolution: { value: [1, 1, 1] },
          uSpeed: { value: stateSpeed(props.state) * (0.35 + intensity * 0.45) },
          uScale: { value: 1.5 },
          uBrightness: { value: 0.45 + intensity * 0.9 },
          uColor1: { value: hexToVec3(voiceStateColors[props.state]) },
          uColor2: { value: hexToVec3("#7356b8") },
          uNoiseFreq: { value: 2.5 },
          uNoiseAmp: { value: 0.6 + intensity * 0.4 },
          uBandHeight: { value: 0.5 },
          uBandSpread: { value: 1 },
          uOctaveDecay: { value: 0.1 },
          uLayerOffset: { value: 0 },
          uColorSpeed: { value: 1 },
        },
      });
      resources.add(() => program.remove());
      const mesh = new Mesh(gl, { geometry, program });

      const resize = () => {
        renderer.setSize(container.clientWidth || 1, container.clientHeight || 1);
        program.uniforms.uResolution.value = [
          gl.canvas.width,
          gl.canvas.height,
          gl.canvas.width / Math.max(1, gl.canvas.height),
        ];
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
          program.uniforms.uTime.value = time * 0.001;
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
      className="soft-aurora-container"
      data-focus-scene="soft-aurora"
      data-voice-state={props.state}
      style={{ opacity: intensity }}
    />
  );
}

function hexToVec3(hex: string): [number, number, number] {
  const value = hex.replace("#", "");
  return [0, 2, 4].map((offset) => parseInt(value.slice(offset, offset + 2), 16) / 255) as [
    number,
    number,
    number,
  ];
}

function stateSpeed(state: FocusSceneProps["state"]): number {
  if (state === "speaking") return 1.25;
  if (state === "interrupted") return 1.4;
  if (state === "error") return 0.25;
  return 0.6;
}
