# Runtime Text-to-Audio Evaluation — 2026-07-28

## Scope

This record separates deterministic repository validation from one machine-specific Apple Silicon integration. The exact language and speech models below are reproducible benchmark inputs, not SDK recommendations, deployment defaults, or application voice selections.

The integration begins with typed text. It does not include microphone capture, VAD, ASR, first audible sound, user-speech-driven barge-in, or subjective voice quality.

## Deterministic Repository Evidence

The required gates ran against source commit `6b0b76c1c4c936e1253819fc3934a18e2c14d72f` with Rust `1.97.1`:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
git diff --check
```

The final baseline run passed 211 tests with no failures, including deterministic coverage for:

- UTF-8-safe phrase segmentation and bounded phrase-queue backpressure;
- causal first-text, first-synthesis, and first-playable timing;
- ordered synthesis and audio output before language generation completes;
- cancellation during generation, synthesis, queued speech, and active output;
- cleanup before terminal cancellation or stage-aware failure;
- exactly one terminal event and runtime reuse after every terminal outcome;
- loopback probe composition, stable output, signal handling, and temporary-file cleanup.

These tests use mocks, loopback fixtures, fake executables, and valid audio containers. They require no model download, speaker, microphone, or cloud service and are not hardware-latency evidence.

## Apple Silicon Measurement

### Execution Identity

- Measured source commit: `6b0b76c1c4c936e1253819fc3934a18e2c14d72f`
- Probe binary SHA-256: `4335d3311f8882cef14a352e8e24fab6c01e7e4c8bc357efd18dccc0f924b0cb`
- `Cargo.lock` SHA-256: `c48f67a054bd1274300aefd345d4f372f3abd05a48e40960775c9340abc26d57`
- Rust: `rustc 1.97.1 (8bab26f4f 2026-07-14)`, `aarch64-apple-darwin`
- Cargo: `1.97.1`
- Machine: MacBook Pro `Mac17,9`
- Chip: Apple M5 Pro, 18 cores
- Unified memory: 64 GB
- Operating system: macOS 26.5, Darwin 25.5.0
- Default output route during the run: built-in MacBook Pro Speakers, 48 kHz

The probe was built once before measurement and then invoked directly from `target/debug`, excluding compilation from all reported timing.

### Benchmark Inputs

Language generation:

```text
service=Ollama 0.30.10
endpoint=http://127.0.0.1:11434
model=hf.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF:Q6_K
digest=960d4a8b192046de9fd035a66a2769e762a7d5aaff5ba3422d43dc7e6019f6a9
think=false
temperature=0
seed=42
num_predict=128
num_ctx=8192
```

Speech synthesis:

```text
service=MLX-Audio 0.4.6
endpoint=http://127.0.0.1:8000/v1
model=mlx-community/Qwen3-TTS-12Hz-0.6B-CustomVoice-bf16
snapshot_revision=6415d95f88be018ff9e46813119dc3bc12261328
snapshot_file_count=12
snapshot_manifest_sha256=231d4108164d6bf4418c997a312b8071a12f3f393144d4314084b6999656f9dd
voice=vivian
language=Chinese
speed=1.0
max_tokens=128
repetition_penalty=1.05
```

The private configuration used the absolute installed speech snapshot and an absolute playback wrapper path outside the repository. No private path is retained in this record.

### State and Method

- Ollama's loopback daemon was already running, but `ollama ps` showed no loaded model before the request.
- MLX-Audio was started on loopback for this measurement. The exact snapshot was already cached and digest-verified, but no speech model was loaded before the request.
- Service-process startup and probe compilation were excluded. Both first-request model loads were included.
- The prompt was `Answer in two short sentences: 你好，请简短介绍你自己。`.
- A private wrapper recorded the first player-process launch timestamp immediately before replacing itself with `/usr/bin/afplay`.
- The wrapper observed process launch only. No microphone or acoustic sensor observed first audible output.

The generated text was:

> 你好！我是你的AI助手，旨在通过高效、精准的回答来协助你解决各类问题。无论是查询信息还是辅助创作，我随时准备为你提供帮助。

### Results

| Measurement | Elapsed from probe start |
| --- | ---: |
| First text delta | 8,966 ms |
| First synthesis request | 8,982 ms |
| First playable audio | 15,317 ms |
| Wrapper-observed playback-process launch | 16,456.780 ms |
| Total turn completion | 39,812.784 ms |

`FirstPlayableAudio` means the first non-empty encoded segment passed typed-audio validation and was ready for the output adapter. Playback-process launch occurred 1,139.780 ms later. Neither value is first audible sound. Total completion includes sequential synthesis and playback for all response segments plus owned cleanup.

This cold cached run does not validate the roadmap's 1.2-second time-to-useful-audio goal. It is one sample, both readiness proxies exceed that target, and the required first-audible measurement is still absent.

## Cleanup Evidence

After the probe completed:

- the audio temporary directory contained zero entries;
- the temporary MLX-Audio listener was stopped and port 8000 had no listener;
- the benchmark language model was unloaded and `ollama ps` was empty;
- no probe, playback wrapper, or `afplay` process remained;
- the private configuration, wrapper, timestamp, logs, and temporary directory were deleted;
- the pre-existing Ollama daemon remained bound only to `127.0.0.1:11434`.

## Evidence Limits

- One prompt and one cold cached run do not establish representative latency, reliability, or quality.
- The selected benchmark inputs are not a backend, model, or voice recommendation.
- The language-model variant requires its own provenance, behavior, and license review before deployment use.
- Snapshot digest verification proves the measured file set, not model quality or suitability.
- No subjective English or Chinese listening evaluation was recorded.
- No first-audible, microphone, ASR, VAD, barge-in, power, thermal, or peak-memory measurement was recorded.
