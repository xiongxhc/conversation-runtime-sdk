# Runtime Text-to-Audio Evaluation — 2026-07-28

## Scope

This record separates deterministic repository validation from a historical machine-specific Apple Silicon benchmark and a later isolated process-level continuity check. The exact language and speech models below are reproducible benchmark inputs, not SDK recommendations, deployment defaults, or application voice selections.

The integration begins with typed text. It does not include microphone capture,
VAD, ASR, first audible sound, or user-speech-driven barge-in. One separately
labeled listening observation is included; it is not a representative
subjective voice-quality evaluation.

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
response_format=wav
```

Runtime speech pipeline:

```text
phrase_soft_limit_bytes=96
phrase_hard_limit_bytes=192
phrase_queue_capacity=2
```

The surviving benchmark evidence does not retain the private configuration's
exact speech `instructions`, `max_text_bytes`, or speech-response
`max_audio_bytes`, and no sanitized effective-config digest was recorded. The
public example template cannot establish those measured values. Exact replay of
the effective speech request is therefore not possible from the retained
evidence.

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

Runtime milestones use the monotonic origin captured inside the runtime when
the turn starts:

| Runtime milestone | Elapsed from runtime turn start |
| --- | ---: |
| First text delta | 8,966 ms |
| First synthesis request | 8,982 ms |
| First playable audio | 15,317 ms |

The wrapper and controller values use the separate shell timestamp captured
immediately before the probe process was launched:

| External observation | Elapsed from process launch |
| --- | ---: |
| Wrapper-observed playback-process launch | 16,456.780 ms |
| Total turn completion | 39,812.784 ms |

`FirstPlayableAudio` means the first non-empty encoded segment passed typed-audio validation. Its timestamp is captured before lifecycle publication and output handoff, and it causally precedes the first output-adapter call. The runtime-turn and process-launch origins have no recorded offset, so values from the two tables cannot be subtracted to establish an interval. Neither first playable nor playback-process launch is first audible sound. Total completion includes sequential synthesis and playback for all response segments plus owned cleanup.

This cold cached run does not validate the roadmap's 1.2-second time-to-useful-audio goal. It is one sample, its timing origins do not match a measured speech-end-to-audible interval, and the required first-audible measurement is absent.

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

## Continuous-Speech Deterministic Correction — 2026-07-28

This correction preserves the historical measurement above and adds
deterministic coverage for speech continuity seams. The integrated loopback
probe now returns the exact model text:

```text
# 问候
你好。今天很好！*保持自然*，C# 和 2*3 不变。
```

The probe proves that standard output remains byte-for-byte unchanged while the
speech fixture receives exactly one request containing:

```text
"input":"问候. 你好。今天很好！保持自然，C# 和 2*3 不变。"
```

The same deterministic run requires one captured WAV, one fake-player launch,
an empty playback temporary directory, exactly one occurrence of each runtime
milestone in causal order, and a final `status=completed`. Runtime regressions
also cover short-clause coalescing, speech-only normalization, capacity-one
synthesized-audio prefetch, ordered playback, cross-stage cleanup, queued-audio
discard, exactly-one terminal publication, and runtime reuse.

An additional loopback regression preserves this story Markdown byte-for-byte
on standard output:

```text
### 故事名：《第25小时的雨》

#### 1. 初遇：咖啡馆的旧雨伞

林默是一家专门
```

Its two bounded speech requests contain:

```text
故事名，第25小时的雨。 初遇，咖啡馆的旧雨伞。
林默是一家专门
```

They contain no heading markers, section numbering, or decorative title
brackets.

The focused probe CLI suite passed `13` tests. The complete locked workspace
suite passed `247` tests with no failures under `--no-fail-fast`; formatting,
strict all-target workspace Clippy, whitespace validation, incomplete-marker
scanning, and concrete private-path scanning also passed.

## Continuous-Speech Apple Silicon Check — 2026-07-28

One isolated check ran the committed `conversation-voice-probe` binary at
`2977023` with SHA-256
`e97454e8d9a0a5ae763578813b6753d825ac0ff1fb0e76194b0821ddc6a679f3`.
The Ollama process was running with no model loaded before the check. The
speech service started cold from a locally verified, cached model snapshot.

The language model returned the exact requested text:

```text
# 问候
你好。今天很好！*保持自然*，C# 和 2*3 不变。
```

The speech service recorded one successful request, the playback wrapper
recorded one launch, and the probe reported:

| Observation | Result |
|---|---:|
| First text delta | 7,912 ms |
| First synthesis request | 8,295 ms |
| First playable audio | 14,016 ms |
| Outer command wall time | 26.1639 s |
| Speech requests | 1 |
| Playback launches | 1 |

The outer command wall time comes from the command runner rather than a runtime
milestone. It includes generation, synthesis, playback, and process cleanup.
It is not a first-audible measurement.

After completion, the playback process was reaped, the audio temporary
directory was empty, port `18000` had no listener, and the Ollama loaded-model
state was restored to empty. The pre-existing Ollama daemon remained running.

This sample removes the previous runtime-created seam between independent
punctuation-triggered speech requests because the full response is synthesized
and played once.

An additional listening check used the prompt `给我讲故事` from the correction
branch. It reported first text at `186 ms`, first synthesis at `486 ms`, first
playable audio at `5,328 ms`, and completed successfully. The longer response
required seven independent speech requests. The listener reported that pauses
were better and that one voice change remained, describing the result as much
better overall.

This is one subjective observation, not a deterministic quality guarantee.
The remaining voice change is consistent with the unresolved boundary between
independent synthesis requests for responses that exceed one speech segment.
