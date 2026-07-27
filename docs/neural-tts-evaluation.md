# Local Neural TTS Evaluation Evidence

## Scope

This record evaluates two Apple Silicon MLX-Audio candidates through the local OpenAI-compatible speech endpoint. It is evidence for the listed machine, snapshot revisions, and manifest digests only; it does not select an SDK default, deployment backend, or application voice.

- Server: MLX-Audio `0.4.6`, installed as a uv tool with the `server` extra. In the verified `0.4.6` package, the base MLX-Audio install does not include the server runtime dependency `uvicorn`, so the extra is required. The package version is recorded; an upstream source commit was not captured.
- Verified installation: `uv tool install --force 'mlx-audio[server]' --prerelease=allow`.
- Verified health endpoint: `127.0.0.1:8000`.
- Upstream references: [MLX-Audio](https://github.com/Blaizzy/mlx-audio) and [Qwen3-TTS](https://github.com/QwenLM/Qwen3-TTS).
- Machine: MacBook Pro Mac17,9; Apple M5 Pro, 18 cores, 64 GB; macOS 26.5.
- Audio format: 24 kHz mono PCM WAV.

The first request can download and load model files into the model host's external cache. It does not write model files into this repository. Run the server on `127.0.0.1`, not a network-facing address.

## Candidate Identity and Evidence

| Candidate only | Snapshot revision | Manifest SHA-256 | License | Input | State | Synthesis | WAV duration | RTF |
| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: |
| `mlx-community/Qwen3-TTS-12Hz-0.6B-CustomVoice-bf16` | `6415d95f88be018ff9e46813119dc3bc12261328` | `231d4108164d6bf4418c997a312b8071a12f3f393144d4314084b6999656f9dd` | Apache-2.0 | Mandarin | Warm | 1.84 s | 4.24 s | about 0.43 |
| `mlx-community/Qwen3-TTS-12Hz-1.7B-CustomVoice-6bit` | `1c6c0ff58c43afa8df571facde2efa077efd85e2` | `16a8e49ec64f87318d647dbd2b9d03bd83b3bc1246d7da27650410f176be14e4` | Apache-2.0 | Mandarin | Warm | 1.38 s | 4.24 s | about 0.33 |
| `mlx-community/Qwen3-TTS-12Hz-1.7B-CustomVoice-6bit` | `1c6c0ff58c43afa8df571facde2efa077efd85e2` | `16a8e49ec64f87318d647dbd2b9d03bd83b3bc1246d7da27650410f176be14e4` | Apache-2.0 | English | Warm | 1.36 s | 4.00 s | 0.34 |
| `mlx-community/Qwen3-TTS-12Hz-1.7B-CustomVoice-6bit` | `1c6c0ff58c43afa8df571facde2efa077efd85e2` | `16a8e49ec64f87318d647dbd2b9d03bd83b3bc1246d7da27650410f176be14e4` | Apache-2.0 | Cached cold start | 1.7B restarted, no download | 3.06 s | 3.76 s | about 0.81 |

### Reproduce a Manifest Digest

Run the following from the selected snapshot root without publishing its cache path. It follows Hugging Face snapshot symlinks to hash each regular snapshot file, writes sorted `<sha256><two spaces><relative-path>` lines to a temporary manifest, requires the measured 12-file set, then hashes that manifest. `LC_ALL=C` fixes the path ordering.

```bash
manifest="$(mktemp)"
find -L . -type f -print | sed 's#^\./##' | LC_ALL=C sort | \
  while IFS= read -r path; do shasum -a 256 "$path"; done > "$manifest"
test "$(wc -l < "$manifest" | tr -d '[:space:]')" = 12
shasum -a 256 "$manifest"
rm -f "$manifest"
```

Record both the exact snapshot revision and the final manifest SHA-256. Do not substitute one for the other.

The first 1.7B request took 38.62 s while downloading and loading. It is not a clean cold-start latency measurement. After fully stopping and restarting MLX-Audio with the 1.7B 6-bit model already cached, its first request completed in 3.06 s and produced 3.76 s of audio. This is the cached cold start (model load plus generation, no download).

After sequential 0.6B and 1.7B loading, the observed process had a 7.1 GB physical footprint and 28.2 GB peak; this is a shared-process upper bound, not per-model memory.

The host's uncapped backend default, `max_tokens = 1200`, generated 96.0 s of WAV in 41.92 s for one short Mandarin sentence. The runnable profiles therefore set `max_tokens = 128` and `repetition_penalty = 1.05`.

## Rust Probe End-to-End Evidence

The controller completed the following runs through the actual Rust probe against MLX-Audio at `127.0.0.1`. These measurements prove complete-response synthesis and, where noted, the launch of local playback; they do not measure first playable or first audible audio.

| Profile | Text | Playback | Result | Encoded WAV | Synthesis completed | Playback launched | Audio metadata |
| --- | --- | --- | --- | ---: | ---: | ---: | --- |
| `local-neural-quality` | `你好，这是 Conversation Runtime SDK 的本地神经语音测试。` | `--no-play`, explicit WAV output | `status=ok format=wav` | 268,844 bytes | 2.194 s | unavailable | 24 kHz mono PCM; 5.60 s |
| `local-neural-quality` | `你好，Chris。这是现在接入 SDK 的本地神经语音。` | `afplay` | `status=ok format=wav` | 491,564 bytes | 3.908 s | 3.910 s | Not captured |

The `afplay` value records process launch, not a verified audible-output time. First playable audio, first audible audio, and user listening or subjective quality verdicts remain pending.

## Required Decision Evidence

Record these fields for every candidate and comparison before selecting a consuming deployment's model or voice:

| Evidence gate | Record |
| --- | --- |
| Server revision | Package version, source revision when available, installation command, endpoint, and server configuration. |
| Model identity | Full identifier, exact snapshot revision, defined manifest SHA-256, quantization, and cache policy without publishing private paths. |
| License review | License text/source, obligations, reviewer, review date, and approved use scope. |
| Voice and reference consent | Voice name, source/reference provenance, consent or license evidence, reviewer, and approved use scope. |
| Machine profile | Hardware model, chip, core count, memory, OS, audio route, toolchain, and loaded-state sequence. |
| Cold synthesis | Cache state, server/model start state, request start, first playable audio, synthesis completion, WAV duration, and sample count. |
| Warm synthesis | Warm-up procedure, request start, first playable audio, synthesis completion, WAV duration, RTF, and sample count. |
| Memory | Process identifier, measurement tool, physical footprint, peak, loaded models, and whether the process is shared. |
| Quality notes | Separate English and Chinese listening notes, speaker consistency, intelligibility, prosody, artifacts, truncation, and evaluator/date. |

First playable audio has not been measured. User listening and subjective quality comparison are pending; this record makes no GPT Voice parity claim and selects no default.
