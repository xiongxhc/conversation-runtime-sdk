# Human-gated steps — online meeting profile

Steps in the online-profile plan that an automated run must not attempt. Each is
recorded with the exact command or action a person runs. The code and tests around
each step are implemented; only the step itself is deferred.

## 1. Confirm the real streaming transcription wire format

The streaming online recognizer in `crates/model-adapters/src/openai_speech_recognition.rs`
speaks a chunked POST per audio chunk with a streamed NDJSON response. That shape was
chosen because this workspace's `reqwest` has no `stream` feature and the workspace has
no WebSocket crate, so a streaming request body and a realtime WebSocket session are both
unavailable. It was verified only against a local scripted server.

Before the first real meeting, confirm the shape against the provider that will actually
be configured. If the provider speaks OpenAI Realtime over a WebSocket, the two DTOs
`TranscriptionRequest` and `ProviderEvent` must be replaced. The session loop, segment-id
discipline, cancellation binding, and credential redaction are independent of the wire
DTOs and survive that change.

```bash
# Run with a real endpoint and credential, outside CI, on a machine authorised to call it.
cd /Users/cx/Workspace/conversation-runtime-sdk
export OPENAI_API_KEY=...            # never commit; never echo
cargo test -p conversation-model-adapters --test openai_speech_recognition -- --ignored
```

## 2. Real-provider latency measurement

Task 8's real-provider latency tests are opt-in and must never run unattended. They call
a paid provider and report distributions rather than a pass/fail from one sample.

```bash
cd /Users/cx/Workspace/conversation-runtime-sdk
export OPENAI_API_KEY=...
cargo test -p conversation-runtime-gateway --test online_latency -- --ignored --nocapture
```

## 4. Confirm the structured-output directive shape against the real provider

`processText` sends the caller's JSON Schema to the provider as a structured-output
directive: `response_format.json_schema` with `strict: true` on the OpenAI-compatible
endpoint, and `format` on Ollama. Neither shape has been exercised against a live
provider; both were verified only against scripted local servers.

OpenAI's `strict: true` mode requires every object in the schema to carry
`additionalProperties: false` and to list every property in `required`. This runtime's
validator neither requires nor rejects that, so a schema the runtime accepts can be one
the provider refuses outright rather than degrades. Confirm before the first real note
synthesis, and decide whether the runtime should reject such a schema up front.

```bash
cd /Users/cx/Workspace/conversation-runtime-sdk
export OPENAI_API_KEY=...            # never commit; never echo
cargo test -p conversation-runtime-gateway --test text_processing -- --ignored --nocapture
```

## 5. Confirm the batch diarization request shape against the real provider

`reconcileRecording` posts to `{endpoint}/audio/diarizations` with a request body this
repository defined itself. No real vendor is known to speak it. Confirm the endpoint and
body against the provider that will actually be configured before the first real meeting.

## 3. Feishu client, test tenant, and real meeting acceptance

Not in scope for this repository. Conversation Runtime is exercised here with scripted
local servers and fake bridges only. The real-device acceptance run belongs to the
Meeting Agent deployment plan.

## 6. Install the local diarization sidecar and fetch its model

`platform/diarization-sidecar/` holds the local diarization backend. Its clustering and its
protocol are exercised by tests that import neither torch nor SpeechBrain, so an automated run
installs neither and downloads no model. The steps below are the only ones that do, and the
model download is the reason they are human-gated: it reaches Hugging Face, once, and everything
after it runs offline.

```bash
cd /Users/cx/Workspace/conversation-runtime-sdk/platform/diarization-sidecar
python3.12 -m venv .venv
.venv/bin/pip install -e .                   # pulls SpeechBrain and the torch stack, ~2.5 GB

# One-time model fetch, while online. The sidecar itself never downloads: it sets
# HF_HUB_OFFLINE=1 before importing, so a missing checkpoint fails closed mid-meeting.
mkdir -p ~/.cache/conversation-runtime/diarization
.venv/bin/python - <<'PY'
from speechbrain.inference.speaker import EncoderClassifier

EncoderClassifier.from_hparams(
    source="speechbrain/spkrec-ecapa-voxceleb",
    savedir="/Users/cx/.cache/conversation-runtime/diarization",
    run_opts={"device": "cpu"},
)
PY
```

Then point the gateway at the installed console script and that directory:

```toml
[voice.diarization]
backend = "speechbrain-sidecar"
execution = "local"
provider = "speechbrain"
sidecar_executable = "/Users/cx/Workspace/conversation-runtime-sdk/platform/diarization-sidecar/.venv/bin/diarization-sidecar"
model = "speechbrain/spkrec-ecapa-voxceleb"
model_cache_dir = "/Users/cx/.cache/conversation-runtime/diarization"
max_speakers = 12
```

The test-only install, which is what an automated run does, deliberately skips the heavy stack:

```bash
cd /Users/cx/Workspace/conversation-runtime-sdk/platform/diarization-sidecar
python3.12 -m venv .venv
.venv/bin/pip install --no-deps -e .
.venv/bin/pip install "pytest>=8" "numpy>=1.26,<3"
.venv/bin/pytest
```

Two things in the pipeline have never met real audio and should be checked on the first real
meeting rather than trusted:

- **`AMBIGUITY_RATIO = 0.8`** decides when a frame is left unattributed instead of assigned to a
  speaker. It is calibrated against synthetic embeddings, where a mixed frame sits exactly
  between two voices. Real overlapped speech does not. If real meetings leave too much
  unattributed, or attribute overlap to whoever is louder, this is the number to move.
- **The frame budget** merges adjacent frames once a recording passes 2 048 of them, so a
  meeting beyond about 51 minutes is clustered at 3-second resolution and one beyond 102
  minutes at 6-second resolution. Confirm that is acceptable for real meeting lengths before
  raising the budget, which costs memory as the square of the frame count.
