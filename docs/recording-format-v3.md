# CRSREC03 timed recording and alignment

Only explicitly timed sessions write `CRSREC03`. Legacy constructor, CRSREC02
bytes, reader and per-frame rounding remain unchanged. The header remains48 bytes:
magic[8], AES256GCM identifier1, zero[3], original source session u64BE, random nonce
prefix[8], random handle binding[16], zero[4]. No reserved bytes are repurposed.
Each record is u32BE ciphertext length then AES256GCM sealed record/tag. Nonce is
prefix || chunk index:u32BE; AAD is complete header || index. Task9 capability
HKDF/HMAC fields/domain and synced-finalization rules remain unchanged.

## Plaintext records

Kinds2 (gap start),3 (gap end),4 (Ended) retain their02 payloads. Kind1 is forbidden
in03; kind10 below can carry absent timing while preserving PCM.

| Kind | Payload following kind byte |
|---|---|
| 9 policy | cadence target, maximum bracket, maximum pair gap, maximum residual: four u64BE ns (=1000000000,5000000,2000000000,10000000) |
| 10 timed frame | Runtime recorder elapsed ms:u64BE, PCM format:u8/rate:u32/channels:u16, timing-present:u8, optional88-byte media provenance, PCM bytes (nonempty sample aligned) |
| 11 epoch | bridge ID, clock domain ID, capture epoch:u64BE, host basis:u8 (=1), same identifier grammar as media2 |
| 12 clock pair | media2 kind7 body without source session |
| 13 discontinuity | capture epoch:u64BE, reason:u8 |
| 14 alignment limit | no payload |

Policy occurs exactly once first. There is exactly one Ended followed immediately
by EOF for authorized recovery. All fields, epoch/sample/sequence ordering and
bounds authenticate in the complete same-file preflight, before any trusted output.
Malformed03 is an error, never `unavailable_legacy`. Authenticated completed02
returns `unavailable_legacy` for alignment while retaining normal reconciliation.

Authenticated epoch history also admits at most256 distinct clock domains per
recording. Repeating the current domain for reconnect is allowed; returning to a
retired domain or exceeding256 is invalid. Writer and complete recovery preflight
enforce the same bounded history before a duplicate timing join key can validate.

Timing metadata<=256 bytes/frame; <=1,000,000 timed records AND <=128MiB metadata
per recording. Count epoch/pair/discontinuity and88-byte frame metadata, before
append. Policy is fixed overhead. Reserve one kind14 marker outside that budget;
on exhaustion append it once, then write kind10 PCM with timing-present=0.
Earlier evidence is retained; no eviction or false completion. Recorder sink
failure keeps existing nonfatal recording-failed/live-capture policy.

The03 media timeline uses exact cumulative sample numerator/rate, plus actual
recorded core gap milliseconds, with integer floor only when projecting an
endpoint. Writer and reader use the same carried fraction, never independent
per-frame rounding. Gaps have no capture samples. Unclosed gap end stays unknown.
Timing-only omission creates no audio gap. Empty recordings have no origin.

## Alignment read control v4

`read_recording_alignment {request_id,capability}` authenticates the original
recording identity, independent of the current control session. It needs idle
voice/text state and configured recording, NOT a diarizer. No provider call.
Pages/completion repeat only source_session_id, recording_handle, sha256 (not tag,
key or PCM). All JSON64-bit values are canonical decimal strings. Entries are
typed epoch/frame/clock_pair/discontinuity/audio_gap/alignment_limit records;
frames retain media sample coordinates, nullable capture/wall mapping and bounded
reasons. Pages contain index, entries and policy, <=64 entries AND <=32768 encoded
bytes. Completion contains page_count, coverage_status available|partial|
unavailable_legacy, bounded warnings and traversal_succeeded=true.

Pages are TENTATIVE until the sole correlated terminal completion validates fixed
identity, contiguous indexes and page count. Error/EOF/missing terminal invalidates
the generation. No capability in ordinary event history or diagnostics.

One implicit credit permits page0 OR zero-page completion OR error after command
acceptance. Only consumer `next()` AFTER yielding page i sends private
`recording_alignment_credit {request_id,consumed_page_index:i}` using the ORIGINAL
request ID, no extra ACK. One exact last-page credit permits one next page OR
completion. Duplicate/future/regressing active credit terminates the read; inactive
or different-target credit/cancel is a no-op. Private
`cancel_recording_alignment {request_id}` cancels and reaps before the original
sole terminal error. SDK `return()` drains that terminal, discards tentative data,
and reports transport failure rather than fabricating cleanup/completion on EOF.
Only one pending `next()` is supported; concurrent pulls reject locally.

After complete Task9 safe-descriptor/private-encrypted-snapshot authentication,
at most TWO monotonic decode traversals share that SAME retained immutable file:
frame/page cursor and pair lookahead cursor. Use independent positioned-read
offsets, not `try_clone`'s shared file position. At most current frame, fixed pair
cache, bounded page, capacity1 channel; no index/extra file/path reopen/rescans.
Cancellation checks apply during verification/read/send; gateway select remains
responsive and awaits its owned reader before releasing admission.

## Exact public and wire shapes

Canonical complete result envelopes are in
`tests/fixtures/timed-capture-v1/output.json`. A page is
`{protocol_version:4,type:"recording_alignment_page",request_id,page:{source,page_index,policy,entries}}`.
Terminal is
`{protocol_version:4,type:"recording_alignment_complete",request_id,completion:{source,page_count,coverage_status,warnings,traversal_succeeded:true}}`.
Source is exactly `{source_session_id,recording_handle,sha256}`. Full envelopes,
not just entries, remain within the32KiB page bound. All fields are exact-key.

The TypeScript public entry points are:

```ts
RuntimeClient<4>.startTimedVoiceSession({
  bridgeSessionId: string,
  profile: RuntimeProfile,
  responseMode: VoiceResponseMode,
}): Promise<TimedVoiceSession>;
RuntimeClient<4>.readRecordingAlignment({
  capability: FinalizedRecordingCapability,
}): AsyncIterable<RecordingAlignmentPage | RecordingAlignmentComplete>;
```

TimedVoiceSession retains existing controls and events and requires mediaLease;
its mediaProtocolVersion is2, timingContract is`capture-clock/v1`, and captureFormat
is the explicit configured four-part format. Existing startVoiceSession remains
unchanged. Public alignment values use camelCase, bigint for every wire64-bit
integer, nullable unknown bounds, and readonly frozen pages/entries. The yielded
types have type`recording_alignment_page` and`recording_alignment_complete`;
page fields are source,pageIndex,policy,entries; terminal fields are
source,pageCount,coverageStatus,warnings,traversalSucceeded. The source identity
is the historical recording, never the current RuntimeClient.currentSessionId.
Store no partial generation as trusted evidence before terminal validation.

Nominal endpoint interpolation cannot reconstruct the producer's internal
piecewise source ledger. A downstream partial-frame utterance needs unique
stable UI evidence across the WHOLE touched source-frame capture span (or
authoritative finer producer evidence). Invented interior boundaries cannot
resolve identity. These APIs do not make the existing untimed transcript events
into capture-timed utterances; provider/finalizer support is a later prerequisite.
