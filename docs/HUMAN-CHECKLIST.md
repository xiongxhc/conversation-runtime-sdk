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
