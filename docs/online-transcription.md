# Ordered online transcription

This profile uses one transcription WebSocket, not the audio-upload HTTP API.
Configure the complete `wss://api.openai.com/v1/realtime?intent=transcription`
endpoint, a transcription model and credential **environment-variable name**.
The endpoint path/query is preserved; credentials in URLs, fragments, redirects
and non-TLS remote endpoints are rejected. Plain WS is limited to local scripted
testing. No provider/model fallback or automatic reconnection/audio replay occurs.

An explicit `[voice.asr.server_vad]` is required: `threshold = 0.5`,
`prefix_padding_ms = 300`, `silence_duration_ms = 500`. Threshold accepts `0..1`;
only that prefix/silence profile is supported. Manual/conversation-only options
and the known VAD-incompatible `gpt-realtime-whisper` model fail closed.
See `configs/gateway.meeting-online.example.toml`. Existing private configuration
is not silently upgraded. The old HTTP chunk/retry builders no longer exist.

The adapter verifies `session.created`, sends transcription `session.update`,
and verifies the matching `session.updated` before uploading any audio. Capture
must already be aligned PCM16 little-endian mono 24 kHz. Append messages contain
base64 raw PCM, never a WAV header or per-frame manual commit. Conversion belongs
to the capture producer. Startup shares 30 s across capture/connect/update; send
progress is bounded 5 s and owned cancellation/reap 2 s. These limits cannot be widened
through configuration.

## Wording and response admission

Provider commit predecessors order completed items. Completed text replaces the
provisional delta candidate; deltas alone never emit a final. A later item that
completes first waits for its predecessor. Unknown/forked/conflicting order or
expired progress yields an explicit incomplete recognition failure.

The new provider-neutral `FinalizedUtterance` bypasses the offline hypothesis/
silence accumulator. Its ordered final is published immediately, including while
an older response plays. Automatic response admission queues at most 32, starts
only while Listening, and emits one `ResponseAuthorized` segment-to-turn link
when it starts. It never republishes the final or overwrites an active turn.
ExternalGate retains 32 pending segments and rejects authorization while another
response is active, leaving the segment pending. Overflow/start failure/shutdown
with unadmitted work is explicit. Legacy offline hypotheses are unchanged.

Socket/provider failure or actual capture loss ends the recognition epoch; there
is no replay. The media-owned capacity-one forwarding/drain worker retains the
original network consumer after the provider ends, so encrypted recording keeps
receiving the independent atomic fan-out copy. It holds at most one queued and
one pending 64 KiB frame beyond the existing network queue. Parent cancellation
joins media, drain and event-relay cleanup concurrently. Timing-only metadata is
recorder-only, not an ASR discontinuity or fabricated silence.

## Opt-in v4 evidence

Legacy v1-v3 output/default 3 and explicit version pinning are unchanged. Opt-in
v4 finals may include `capture`, `live_coverage`, and a closed
`capture_unavailable_reason`. The canonical literal vectors are
`tests/fixtures/transcription-v4/events.jsonl`, consumed by Rust and TypeScript.
The SDK exports `SourceSampleRange`, `WholeFrameNominalEnvelope`,
`UtteranceCaptureEvidence`, `CaptureUnavailableReason`, and
`VersionFourVoiceTranscriptFinalEvent`; wire snake_case integer strings become
camelCase `bigint` fields. `sampleRateHz` remains a number. Evidence is at most
2048 UTF-8 bytes and unknown fields/reasons/malformed ranges fail validation.

`source_sample_range/v1` identifies exact half-open output PCM sample positions,
clock domain, capture epoch and first/last touched frame sequence, at 24 kHz. It is
derived from provider input-audio offsets through **submitted**, not merely
received, PCM. These offsets include provider VAD padding/silence; they are not
word boundaries. Missing/evicted/out-of-range evidence leaves wording intact with
a reason. No receipt-time, text-length or recording lookup fills the gap.

`whole_touched_frame_nominal_host_envelope/v1` is deliberately coarser: it covers
every touched producer frame, including partial boundary frames. Runtime validates
all interior metadata before folding. Its quality is
`nominal_frame_endpoint_interpolation`, not exact interior/source-ledger timing.
`firstFrameHostEndNs` freezes the first boundary frame; known arithmetic rounding
is separate from nullable physical capture uncertainty. It is not a wall clock,
word timestamp, speaker identity or exact acoustic capture claim. A consumer may
attribute a partial-frame utterance only with stable UI evidence covering the
whole touched source-frame span (or equally authoritative finer producer evidence).

## Bounded state and limits of proof

One connection holds 32 unresolved items/edges, 64 retired item identities,
256-byte provider IDs, 16 KiB candidate/completed text, 128 KiB WS frames/messages,
16 outbound frames and 1 MiB raw PCM, and 4096 submitted metadata spans. Early-known
long speech folds evicted prefixes; a late boundary outside that ring is unknown.
No full recording/audio ledger or unbounded index is retained.

Each unresolved item retains 64 recent exact delta event IDs (maximum
32*64*256 ID bytes plus fixed hashes/container overhead). In-window duplicates
neither append nor refresh deadlines; conflicting payloads fail. Evicted replays
may alter only the bounded provisional candidate, never completed replacement or
final ordering. This is not unlimited historical deduplication/exactly-once.

Speech-start alone has no completion timer. Stopped/committed counterparts wait 5 s;
committed completion/predecessor waits 30 s; delta/completed without commit waits 5 s.
Missing stop may preserve completed ordered wording with unknown timing after 5 s.
Only content index 0 is supported; a missing optional delta index is allowed only
for an already bound item. Required completed/failed indices are not guessed.

Automated tests use local scripted sockets and fake credentials. They do not
verify real account/model acceptance, live provider latency or physical devices.
