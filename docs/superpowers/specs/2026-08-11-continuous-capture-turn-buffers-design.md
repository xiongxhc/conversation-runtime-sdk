# Continuous Capture and Turn-Bounded Recognition Design

## Problem

The macOS sidecar currently bounds long-session recognition by replacing the
WhisperKit stream transcriber after final silence. Because WhisperKit owns the
`AudioProcessing` instance, stopping the old transcriber also stops microphone
capture briefly. Speech that begins during that restart can be clipped, and the
audio-device lifecycle is coupled to one recognition implementation.

The user-visible requirement is continuous listening across turns without stale
transcripts, unbounded audio growth, voice-session errors, or missing first
syllables.

## Decision

Keep the Apple voice-processing capture engine active for the entire voice
session. Insert a logical, turn-bounded `AudioProcessing` implementation between
continuous capture and WhisperKit. Replacing a WhisperKit transcriber resets only
that logical buffer; it never stops or restarts the hardware capture graph.

The public Rust SDK and sidecar protocol remain unchanged and backend-neutral.
This is an internal macOS adapter improvement, not a new provider selection.

## Considered Approaches

### 1. Continue restarting hardware capture

This is the smallest implementation, but it retains a capture gap at every turn
and makes continuity depend on audio-device restart timing. Rejected.

### 2. Fork WhisperKit's stream transcriber

A local transcriber could directly manage rolling timestamps and buffers without
restarting. This offers maximum control but duplicates a large dependency-owned
implementation and increases update and licensing risk. Rejected for this slice.

### 3. Continuous capture plus a logical audio processor

The Apple processor owns hardware for the full session. A small
`TurnAudioProcessor` implements WhisperKit's `AudioProcessing` contract, receives
converted samples from continuous capture, retains a bounded pre-roll, and resets
per transcriber. Selected because it fixes the device gap with a narrow,
independently testable boundary.

## Components

### `VoiceProcessingAudioProcessor`

- Continues to own the Apple voice-processing engine, format conversion, VAD
  windows, discontinuity events, and capture failure reporting.
- Starts once when recognition starts and stops once when the recognition service
  stops.
- Forwards each converted sample batch to a callback without owning WhisperKit's
  turn buffer.
- Removes the temporary preserve-handlers-on-stop mechanism because logical
  transcriber resets no longer stop this processor.

### `TurnAudioProcessor`

- Conforms to WhisperKit's `AudioProcessing` protocol without owning a device.
- Accepts converted 16 kHz mono float samples through `append(_:)`.
- Retains at most `300 ms` (`4,800` samples) of pre-roll while inactive.
- On `startRecordingLive`, initializes the active recognition buffer from the
  pre-roll and begins forwarding append notifications to WhisperKit.
- On `stopRecording`, stops active accumulation while continuous capture remains
  active.
- Before replacement, opens a bounded `30 s` transition accumulator initialized
  from pre-roll so a slow in-flight decode cannot clip new speech.
- Clears active samples between turns and caps one logical turn at `10 min`.
  Either capacity breach fails recognition rather than dropping audio.
- Retains active energy for the complete pending turn so WhisperKit VAD remains
  aligned with pending PCM; the turn cap bounds that state.

### `WhisperKitRecognition`

- Constructs WhisperKit and each `AudioStreamTranscriber` with the logical audio
  processor.
- Starts continuous hardware capture once before starting the first transcriber.
- Replaces a transcriber after final silence as before, but replacement now only
  rotates logical state.
- Cancels a scheduled rotation immediately when new speech is observed.
- Stops the logical transcriber before stopping continuous capture during session
  shutdown.

## Data Flow

```text
Microphone
  -> Apple voice-processing engine (continuous for session)
  -> 16 kHz mono converted samples
     -> VAD windows and barge-in events
     -> TurnAudioProcessor pre-roll and active turn buffer
        -> WhisperKit AudioStreamTranscriber
        -> recognition hypotheses
```

At a final-silence boundary:

```text
continuous capture remains active
  -> bounded transition accumulation begins
  -> old logical transcriber stops
  -> active turn buffer closes
  -> replacement transcriber starts from the complete transition buffer
  -> continuous capture keeps forwarding samples
```

## Failure and Lifecycle Rules

- A logical transcriber restart must never change microphone permission or audio
  graph state.
- New speech during a restart must be available through the transition buffer
  when the replacement starts, including when old inference exceeds pre-roll.
- Transition or turn capacity exhaustion must fail through the existing typed
  recognition path without silently truncating audio.
- A full recognition-service stop must stop both logical recognition and hardware
  capture exactly once.
- Conversion, worker, and publication failures retain their existing typed fatal
  path.
- No audio, transcript, provider detail, or model path is added to diagnostics.

## Verification

Deterministic Swift tests must prove:

- inactive pre-roll is capped at `4,800` samples;
- starting recognition includes the bounded pre-roll;
- stopping and restarting logical recognition does not stop the source processor;
- speech exceeding pre-roll but appended during logical restart is present after
  restart;
- active VAD energy covers all pending turn audio;
- turn-cap exhaustion fails once without exceeding the sample limit;
- active turn samples do not accumulate across turns;
- full recognition shutdown still releases handlers and capture;
- existing multilingual, VAD, interruption, protocol, and lifecycle tests remain
  green.

The complete Rust workspace, Swift package, desktop tests/build, formatting, and
lint gates must pass. Real microphone acceptance remains required before claiming
GPT-class continuity or completing R3.

## Non-Goals

- Semantic end-of-turn inference.
- A native speech-to-speech provider adapter.
- Changing public configuration or protocol schemas.
- Selecting a model, voice, language, or cloud provider.
- Claiming subjective voice quality or acoustic latency improvements without a
  recorded hardware run.
