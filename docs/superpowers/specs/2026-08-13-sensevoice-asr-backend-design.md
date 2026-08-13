# SenseVoice ASR Backend Design

## Problem

The default voice configuration pins WhisperKit to `language = "zh"` because
its per-turn language detection misroutes mixed-language turns. Pinning mangles
mixed Chinese/English speech instead: English words inside a Chinese sentence
are transliterated or dropped. The user-visible requirement is accurate
transcription of zh/en code-switched speech without changing the sidecar
protocol, the runtime contract, or the default backend.

## Decision

Add an opt-in `sensevoice` ASR backend to the macOS voice sidecar next to the
existing `whisperkit` backend. It runs the FunAudioLLM SenseVoice small int8
model through the sherpa-onnx offline recognizer (SwiftPM package, exact
`1.13.5`), which supports zh, en, ja, ko, and yue with in-sentence
code-switching and built-in `auto` language detection.

The backend reuses the existing capture and turn machinery unchanged:
`VoiceProcessingEngine` capture, `VoiceProcessingAudioProcessor` 16 kHz voice
windows, `EnergyVAD` (`0.04` RMS over `100 ms` windows), the
`RecognitionSpeechGate`, `TurnAudioProcessor` turn accumulation with `300 ms`
pre-roll, the ordered hypothesis pipeline, and the recoverable
`speech_recognizer/recognition_failed` failure taxonomy. Only the recognizer
behind them changes.

## Considered Approaches

### WhisperKit with detection re-enabled

Per-turn detection picks one language for the whole turn, so code-switched
turns still lose the minority language. Rejected.

### Streaming sherpa-onnx model (zipformer transducer)

Streaming models emit incremental tokens but the multilingual code-switch
options are weaker than SenseVoice, and SenseVoice decodes faster than 0.1 RTF
on Apple Silicon, so whole-buffer redecode is affordable. Rejected.

### Offline SenseVoice with periodic whole-buffer redecode

Decode the accumulated unfinalized turn audio about once per second of active
speech for partial hypotheses, and once at each speech end for the engine-final
segment. Simple, accurate, and protocol-identical to the WhisperKit backend.
Selected.

## Sidecar Changes

`SenseVoiceRecognition` implements `SidecarRecognitionService` as a sibling of
`WhisperKitRecognition`, publishing the same `WhisperKitRecognitionEvent`
values through the shared `SidecarRecognitionEventSource` protocol so
`SidecarSession` and the Rust runtime observe identical behavior.

Segment bookkeeping is sample-offset based because offline decoding has no
per-segment alignment. A `SenseVoiceSegmentTracker` keeps the finalized sample
prefix: partial decodes cover only audio after that prefix and reuse the
current segment identifier; each speech end decodes the same span once more,
emits it engine-final, and advances the prefix. Empty or unrecognizable decode
output is never emitted. After configured final silence, the turn buffer resets
from pre-roll and segment numbering restarts at 1, mirroring the WhisperKit
transcriber replacement; a capture discontinuity discards buffered audio and
pending decodes exactly as the WhisperKit path discards its stitched buffer.

Decodes, turn resets, and discontinuity recovery are serialized on a dedicated
decode worker so an in-flight final decode can never race the reset that
follows it. The blocking model call runs off the actor, so voice-window
processing and barge-in observation never stall behind a decode.

The sidecar CLI grows `--asr-backend whisperkit|sensevoice` (default
`whisperkit`). For `sensevoice`, `--model-path` must be an absolute directory
containing `model.int8.onnx` and `tokens.txt`; the preflight failure style
matches the WhisperKit model validation. `--language` accepts `auto`, `zh`,
`en`, `ja`, `ko`, or `yue` and defaults to `auto`.

## Runtime Changes

`MacOsVoiceSidecarConfig` gains a `SidecarAsrBackend` value (default
`Whisperkit`); only `Sensevoice` adds `--asr-backend sensevoice` to the spawn
arguments, so WhisperKit spawns are byte-identical to before. Gateway schema v1
accepts `backend = "sensevoice"` under `[voice.asr]` with the existing
model-path, download, and execution validation unchanged. The voice-loop
harness accepts the same value.

## Model Placement

The expected local model directory is
`~/.local/share/conversation-runtime/models/sensevoice/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17/`
containing `model.int8.onnx`, `tokens.txt`, and the model `LICENSE`. Downloads
stay disabled; the sidecar never fetches models at runtime. Swift tests that
need the model gate on that directory and skip cleanly when it is absent.
