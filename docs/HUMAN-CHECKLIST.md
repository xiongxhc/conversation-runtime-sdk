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
export GATEWAY_ONLINE_LATENCY_REAL_PROVIDER=1
export GATEWAY_ONLINE_LATENCY_CONFIG=/absolute/path/to/private/online-gateway.toml
export GATEWAY_ONLINE_LATENCY_CAPTURE_COMMAND=/absolute/path/to/private/capture-s16le-pcm
export GATEWAY_ONLINE_LATENCY_SAMPLES=5 # must be at least 2
cargo test -p conversation-runtime-gateway --test online_latency -- --ignored --nocapture
```

The private configuration must report the explicit `online`/`cloud`/remote profile and advertise
a loopback TCP media endpoint. `GATEWAY_ONLINE_LATENCY_CAPTURE_COMMAND` is a human-gated private
helper: every invocation must write at least 32,768 and at most 1,048,576 bytes of raw signed-16-bit
little-endian mono 16 kHz PCM capture to stdout, then exit successfully. The harness invokes it
once for initial speech and once for barge-in speech; it passes no lease credential, discards its
stderr, and never prints or persists its path or PCM. The helper receives a cleared environment
with only `LC_ALL=C`; use an absolute executable with all device/private configuration supplied
by that executable, and do not rely on inherited `PATH`, `HOME`, or provider credentials.
Capture and reap are bounded. EOF with a still-live helper fails closed and terminates it;
the helper must exit when its finite capture completes, without a lingering stdout-closed process.
The shared runner splits capture using the private configuration's `media.max_payload_bytes`
(rounded down to an even byte count), continuously acknowledges playback, and waits for the
actual flush acknowledgment before stopping the sample. The output contains only count, p50, p95,
and max for the internal handoffs. `first_synthesis_request→first_playable_audio` ends at
validated PCM ready for output, not audibility; `barge_in_onset→playback_flush_acknowledged` ends
at an internal output-path flush acknowledgement, not the 300 ms audible-stop claim.

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

The current adapter deliberately namespaces labels returned by separate bounded requests. Do
not remove that isolation unless the selected provider proves that one request/session preserves
speaker identity across every window.

```bash
cd /Users/cx/Workspace/conversation-runtime-sdk
export OPENAI_API_KEY=...            # never commit; never echo
cargo test -p conversation-model-adapters --test openai_diarization -- --ignored --nocapture
```

Expected evidence: the provider documentation or captured response identifies one explicit
diarization session spanning all uploaded windows, or returns speaker embeddings suitable for
one global clustering pass. If neither is true, leave labels window-scoped and keep final
attribution anonymous across windows.

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
- **The PCM energy VAD threshold** (`SPEECH_ACTIVITY_THRESHOLD = 256`) is exercised only with
  deterministic synthetic PCM. Run the installed sidecar against a consented real recording
  containing silence, steady room noise, separated speech, and overlap; expected evidence is
  no segment for silence/background, separate timestamped turns across silence, and `null`
  speaker labels for overlap. Do not change the threshold from a model-only or synthetic run.

## 7. Validate the private TLS media bridge, client token, and dual capture fan-out

This is deliberately unexecuted by automated tests. The tests use only plaintext loopback fake
bridges. Run this once on the authorised agent-box and again with a consented real meeting device;
do not run it from CI, a developer laptop on a public network, or an untrusted bridge.

1. Have the agent-box operator install a server certificate and private key at the service-owned
   paths selected for the deployment. The key must be readable only by the gateway service
   account; do not place certificate contents, key contents, lease credentials, or audio in shell
   history, tickets, logs, packet captures, or the configuration repository. Confirm the
   certificate's subject/SAN matches the private address the bridge will use and that the
   agent-box firewall allows the media port only from the bridge network.

2. In the already-valid schema-v3 gateway configuration, add a bounded non-loopback `[media]`
   section using the actual private address and the two installed file references. The following
   is a shape-only example, not a deployable endpoint or filesystem location:

   ```toml
   [media]
   bind = "10.0.0.25:9443"
   advertised_endpoint = "tls://10.0.0.25:9443"
   credential_lifetime_ms = 60000
   reconnect_window_ms = 10000
   max_frames_per_second = 100
   max_payload_bytes = 32768
   max_outbound_queue_bytes = 262144
   max_outbound_queue_ms = 10000
   packet_read_timeout_ms = 5000
   tls_certificate_path = "/service-managed/certificates/media-server.pem"
   tls_private_key_path = "/service-managed/certificates/media-server-key.pem"
   ```

   Keep the reconnect window no greater than the credential lifetime and the queue time no
   greater than the reconnect window. `packet_read_timeout_ms` bounds only completion of an
   already-started authenticated media packet; an idle authenticated bridge remains connected
   until normal cancellation, disconnect, or lease expiry. Confirm that removing either TLS file
   reference makes startup fail, and that the listener does not start before the configuration is
   valid.

3. Start the gateway under the normal service manager, then use a protocol-v3 control client to
   start exactly one voice session. Retain the returned `connect_endpoint`, authoritative session
   id, and opaque credential only in the bridge process's volatile memory. Verify that a v1/v2
   start is request-scoped rejected for this media-configured deployment, and that no status or
   error line displays the credential.

4. From the authorised bridge, verify the TLS certificate chain and private endpoint before
   sending any media bytes. Only after that TLS handshake completes, authenticate the bridge with
   the exact session id and short-lived credential returned by its own v3 acceptance. A plaintext
   connection, a token before TLS, a token from another session, an expired token, and a replay
   after stop must each be refused without media delivery. This token is the required client
   authentication mechanism; do not add or depend on mTLS unless the deployment separately adopts
   it.

5. With a consented real input device, send a small sequence of known capture frames, then induce
   one controlled disconnect and reconnect within the lease window. Retain only metadata evidence:
   TLS connection/packet counts and timing, session id, frame sequence range, timestamps, byte
   counts, and a bridge-computed checksum for each test frame. Do not capture or retain raw packet
   payloads. The expected result is monotonic capture sequence, one explicit recording gap at the
   disconnect, no fabricated silence, no duplicated pre-disconnect frame after reconnect, and
   refusal after the reconnect window expires.

6. Stop the voice session and finalize its recording through the v3 control client. On the
   authorised agent-box, use the runtime's normal encrypted-recording reader with the service key
   available only to that host; confirm its decrypted frame sequence and checksums equal the
   bridge metadata from step 5. Confirm the recording directory contains the encrypted artifact
   only and that no plaintext capture bytes were written to temporary storage.

7. For the same frames, confirm the recognizer's `AudioCapture` consumer receives the matching
   sequence/byte-count/checksum evidence and, once the separately-authorised online recognizer is
   configured, produces the expected transcript from the consented device. The recorder check in
   step 6 and this recognizer check must both pass for the same session: a successful transcript
   alone is not evidence that encrypted recording received capture, and a finalized recording
   alone is not evidence that recognition received capture. Record only aggregate checksums and
   transcript approval evidence, never raw audio or a bearer credential.
