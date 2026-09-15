# Finalized recording recovery (control protocol v4)

Default gateway startup still announces protocol **3**, with unchanged v1–v3
frames and per-command compatibility rules. Opt in before starting the gateway:

```ts
const transport = await StdioGatewayTransport.start({
  gatewayPath, configPath, protocolVersion: 4,
});
const client = await RuntimeClient.connect(transport, 4);
const currentSessionId = client.currentSessionId;
if (currentSessionId === undefined) throw new Error("client is closed");
```

The equivalent CLI option is `--client-protocol-version 4`, after `--config PATH`.
Unsupported selections fail before configuration loading or provider startup.
An explicit-v4 connection remains pinned to 4; a later v1–v3 command is rejected
without changing queued response encoding. Default startup never upgrades to 4.

After stopping voice capture, v4 `finalizeRecording({ sessionId })` returns the
existing recording summary plus `recoveryCapability`. Its readonly fields are
`sourceSessionId: bigint`, `recordingHandle`, `sha256`, `keyId`, and opaque
`authorization`. The wire result adds the sibling `recovery_capability`; old
versions neither emit nor accept it. Failed finalization issues no capability.

Persist this bearer capability only in private application state. Never put it in
logs, general event history, shared meeting artifacts, command arguments, or
environment variables. Neither it nor the summary contains the encryption key.

After the original gateway exits, start another explicit-v4 connection:

```ts
const reconciliation = await client.reconcileFinalizedRecording({ capability });
const processed = await client.processText({
  sessionId: currentSessionId, instructions, input, outputSchema, maxOutputBytes,
});
```

`capability.sourceSessionId` always names the original recording's provenance.
`client.currentSessionId` names the current gateway connection and is cleared on
close/fatal failure. Batch text processing uses the latter without starting a
dummy voice session. Recovery requires idle voice and text lanes plus configured
recording and diarization. Legacy `reconcileRecording` retains both its current
gateway ID guard and original file-header provenance guard.

The same original root recording key must remain configured after restart.
`keyId` is an identifier, not a key-ring lookup or rotation mechanism. CRSREC02 and
its AES derivation are unchanged. A separate HMAC-SHA256 key is derived using ring
HKDF-SHA256 from decoded canonical root key bytes, with salt and info both
`conversation-runtime/finalized-recording-capability/v1\0`. The authenticated
message is that domain followed by big-endian u64 source identity, 16 decoded
handle bytes, 32 digest bytes, and 8 key-identity bytes. Tag verification uses ring's
constant-time verifier; Rust Debug and malformed-capability diagnostics redact it.

Finalized recovery accepts at most **8 GiB (8 × 1024³ bytes) of ciphertext**.
Oversized files are rejected before snapshot allocation/copy or v4 capability
issuance, including repeated finalization. Legacy v3 recording/reconciliation has
no new size policy. Unix source admission uses a no-follow, nonblocking descriptor
and verifies the opened handle is regular; Windows opens the reparse point itself
and rejects reparse/device/non-regular handles. Other platforms fail closed.

Recovery opens the canonical handle once, copies only ciphertext through a fixed
64 KiB hashing buffer, authenticates every chunk, requires exactly one terminal
Ended and immediate EOF, rehashes the snapshot, and rewinds its retained descriptor.
Only then may the diarizer be called or receive plaintext windows. The snapshot is
exclusive/private and unlinked immediately on Unix (delete-on-close on Windows),
so source rename or same-inode mutation cannot alter its second pass, and process
death cannot leave a named extra copy. Cancellation joins recovery cleanup.
No plaintext audio is written to disk; v3's partial-recording behavior is unchanged.

## Sample provenance in reconciled v4 segments

`reconcileFinalizedRecording` snapshots the supplied capability at the call boundary.
Its result includes `source: { sourceSessionId, recordingHandle, sha256 }`, a frozen
non-secret identity matching the original request. It contains no keyId or authorization.
Later caller edits to the capability cannot change the dispatched request or this identity.
Use `source` to join the separately authenticated recording alignment, never the current
connection ID or a sample position reconstructed from milliseconds.

Every v4 segment has `mediaSampleRange`, either null or:

```ts
{
  startSampleInclusive: bigint,
  endSampleExclusive: bigint,
  sampleRateHz: number,
  gapOffsetMs: bigint,
  quantizationPolicy: "provider_seconds_outward_samples/v1"
}
```

The wire uses `media_sample_range` and snake_case fields; all u64 values are canonical
decimal strings. v1–v3 encodings are unchanged and omit this key. Legacy CRSREC02,
direct windows using `MeetingAudioWindow::new`, and adapters without exact provenance
produce null, not reverse-converted millisecond estimates.

For CRSREC03, the reader supplies `MediaSampleOrigin` directly from retained cumulative
PCM samples minus the nonempty window's sample count, its fixed sample rate, and prior
closed core-gap durations. `new_with_sample_origin` validates rate, checked end position
and millisecond projection. A window never crosses a core gap; format changes are refused.
Legacy02 retains its old per-frame millisecond floors and is not called exact timing.

The online adapter admits raw JSON-double endpoints only when finite and
`0 <= start < end <= N as f64 / R`, before rounding. There is no epsilon or one-second
allowance. This is strict admission in the documented f64 domain, not exact original
decimal fidelity or physical/provider accuracy. Relative start is floored to a sample;
relative evidence end is ceiled, bounded at N only after raw endpoint admission. Checked
integer addition preserves origins above 2^53. These are outward sample-cell enclosures:
up to one boundary cell at each end may be only partly speech, not exact word boundaries.
Consumers must not promote speaker identity thresholds using that enclosure slack.

Legacy millisecond points remain separate: floor the relative endpoint to a sample,
checked-add the exact absolute sample origin, then floor the COMBINED sample to ms and
add the gap offset. Do not sum independently floored origin and relative milliseconds,
and do not derive old end-ms from the outward-ceiled evidence end. Positive sub-ms speech
may therefore have equal start/end-ms while preserving nonempty wording/sample evidence.

The 1024-segment/128 KiB-text limits still apply, but enriched metadata can make the actual
serialized response exceed 512 KiB. The gateway validates the entire escaped frame before
acceptance, rejects it whole with a bounded error, and keeps the connection usable. No
segment/text truncation or silently lowered limit is used. Canonical v3/v4 vectors are in
`tests/fixtures/diarization-v4/events.jsonl` and consumed by Rust and TypeScript tests.

## Online batch transport and verification boundary

The configured API base appends `audio/transcriptions` once. The real adapter sends a
multipart file named `window.wav` with MIME `audio/wav`, exact configured model,
`response_format=diarized_json`, `chunking_strategy=auto`, and optional singular language.
Unsupported PCM is rejected; signed 16-bit LE input is copied into a checked 44-byte RIFF/WAVE
header without resampling, padding, path upload, enrollment fields or plaintext tempfiles.
The independent PCM cap is 1 MiB; the allowed WAV is 1 MiB + 44 bytes. Validation precedes
credential lookup. One request runs at a time, redirects/retries are disabled, and one
absolute deadline covers upload, headers and the entire bounded 512 KiB streamed response.

Peak plaintext includes current PCM 1 MiB, queued PCM 1 MiB, reader builder 1 MiB,
WAV 1 MiB + 44 bytes, response 512 KiB, plus bounded parsed result and transport/serialization overhead.
It is bounded independently of recording length, not a one- or two-window resident claim.
Required response fields and raw sample extent are validated; response duration is metadata,
not permission to extend the window. Provider IDs/labels are limited locally to 256 UTF-8 bytes,
with unique IDs per response. Anonymous clusters are request-local: A in two requests does
not establish the same speaker. Unknown response metadata is ignored, never trusted as
warning prose or identity evidence.

All automated provider evidence is scripted loopback HTTP. The restart test uses an
unmodified compiled gateway to capture/finalize/exit, then a separately executed test-only
consumer built through public GatewaySession/adapter APIs for recovery and alignment.
That consumer is not an unmodified deployment CLI; production URL/TLS validation is not
bypassed or weakened. The separate offline two-CLI restart proof remains. Actual remote
provider, credentials, paid requests, deployment TLS and real-device acceptance are human
gates, not claims established by these fixtures.
