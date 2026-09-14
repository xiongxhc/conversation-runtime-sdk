# Timed media v2 (`capture-clock/v1`)

This opt-in contract is independent of unchanged media v1. Control defaults to
v3; only `start_timed_voice_session` on explicitly pinned control v4 issues a
media2 lease. The lease adds `capture_format` and `timing_contract` to the legacy
fields. Format grammar is `pcm_s16le|pcm_f32le` `/` positive decimal sample rate
`/` positive decimal channels `/` positive decimal packet milliseconds `ms`.
There are no inferred format defaults. Runtime source `session_id` and opaque
`bridge_session_id` are different identities.

One timed source/recording admits at most256 distinct clock-domain IDs. A normal
reconnect in the current domain consumes no new domain. Returning to a retired
domain (A→B→A) or declaring the257th distinct domain fails closed before timing or
frame admission; a rejected declaration cannot reset existing watermarks. This
per-source bounded history survives network reconnect and is checked again by
full authenticated03 recovery. It is not a global registry: independent recordings
have independent histories. Producer domains still must be random per process.
This protocol/resource failure is separate from metadata-byte/count exhaustion,
whose explicit limit marker continues PCM with absent timing.

The configured `max_frames_per_second` counts PCM frames only. Epoch, clock-pair
and discontinuity records use a separate fixed128-record/s budget. Each budget
has an independent one-second window retained by the timed reservation; reconnect,
epoch and domain changes reset neither. The exact cap is admitted and the next
record refused until that budget's window resets. Authentication/playback grammar
and legacy media-v1 admission remain unchanged; metadata is never unbounded.

## Framing

Integers are big-endian; PCM alone uses the declared little-endian format.
Each v2 packet starts `4d454432 02 KIND LENGTH:u32`, where LENGTH is the body
length. Read/validate lengths before allocation; maximum body is 65,536+256 bytes.
Identifiers are length:u8 plus 1..64 UTF-8 bytes, no control characters. No
trailing bytes, unknown kinds, flags, enum values or reserved bits are accepted.

| Kind | Body, in order |
|---|---|
| 1 authentication | source session:u64, credential length:u8, credential (1..128 bytes), bridge ID |
| 2 capture frame | source session:u64, sample format:u8 (1 s16, 2 f32), rate:u32, channels:u16, timing (88 bytes below), PCM length:u32, PCM |
| 6 epoch start | source session:u64, bridge ID, clock domain ID, capture epoch:u64, host basis:u8 (=1, host_monotonic_ns) |
| 7 clock pair | source session:u64, clock domain ID, wall epoch:u64, sequence:u64, host before:u64, local Unix ns:i64, host after:u64, wall resolution:u64, external accuracy present:u8, external accuracy:u64 |
| 8 discontinuity | source session:u64, capture epoch:u64, reason:u8 |

Positive epoch/sequence; domain is newly random for each producer process.
Reasons: 1 source_drop, 2 sequence_hole, 3 reconnect, 4 pause, 5 epoch_reset,
6 missing_timing, 7 inconsistent_coordinates, 8 source_discontinuity,
9 alignment_limit_exceeded. Clock pairs require positive before/after with
before<=after; zero-width brackets are allowed. An absent external bound has
presence=0 and zero slot. Presence=1 permits an actual zero bound.

Playback/ack/flush records retain the existing media-v1 byte grammar (kinds
2 playback direction, 3 playback ACK, 4 flush request, 5 flush ACK), also on an
authenticated timed connection. Legacy capture frames are never admitted on a
timed connection. New timed capture records are never admitted on a legacy one.
Authentication version, source session, token and bridge identity must all match
the reservation before accepting any capture or metadata. Epoch start is required
on each connection; reconnect always closes prior coverage.

## Fixed capture provenance (88 bytes)

| Offset | Bytes | Field |
|---|---:|---|
| 0 | 1 | layout=1 |
| 1 | 1 | flags: bit0 nominal host span present, bit1 capture uncertainty present |
| 2 | 1 | quality: 1 nominal_frame_endpoint_interpolation, 2 unknown_clock, 3 unknown_priming, 4 filter_edge, 5 incomplete_source_coverage |
| 3 | 1 | reserved=0 |
| 4 | 8 | capture epoch |
| 12 | 8 | frame sequence |
| 20 | 8 | output start sample (within capture epoch) |
| 28 | 4 | positive sample count (per channel) |
| 32 | 8 | source start numerator |
| 40 | 8 | source end numerator |
| 48 | 4 | source denominator (=output PCM rate) |
| 52 | 4 | source rate Hz |
| 56 | 8 | nominal host start ns |
| 64 | 8 | nominal host end ns |
| 72 | 8 | directed arithmetic rounding envelope ns |
| 80 | 8 | nullable capture uncertainty ns |

Absent slots are zero, never an assertion of zero error. Bit1 requires bit0;
quality1 iff bit0. A present span is positive, half-open and increasing. Check
`outputEnd=outputStart+sampleCount`, `sourceStart=outputStart*sourceRate`, and
`sourceEnd=outputEnd*sourceRate` without overflow; source denominator equals
negotiated output rate. PCM length equals sample count * channels * sample bytes.
Format must equal the negotiated lease; packet duration is a packetization target,
not permission to invent timestamps or reject a valid shorter final packet.

These are EXACT sample positions/rationals and FRAME-LEVEL NOMINAL endpoint
coordinates, **not** the producer's internal piecewise source-buffer map. The Mac
converter may retain up to64 local pieces and a distinct filter-support envelope;
neither is transmitted. The producer must validate complete nominal source
coverage/continuity, with no hidden gap/reset, before supplying endpoints. Missing
or inconsistent internal evidence supplies absent timing. Quality deliberately
differs from `piecewiseNominalSourceInterpolation` in local converter diagnostics.
No prime/filter-support fields are timing authority on this wire.

Nominal interior interpolation must stay labelled as such. Splitting is done by
the producer with its retained source ledger; Runtime must not manufacture exact
subframe provenance from endpoint interpolation. Consumers attributing a partial
frame require stable unique UI evidence over the WHOLE touched source-frame span
(or independently authoritative finer producer evidence). A rounding envelope is
arithmetic only; capture/model physical accuracy remains unknown when unestablished.

Source sample PTS/end must be converted through the capture synchronization clock
to host before enqueueing. Callback, network receipt, Runtime receipt, provider
receipt, occurrence start and session startup are forbidden timestamp substitutes.
Future Swift/AX producers use the same process clock domain; no live-device proof
is implied by the synthetic vectors.

## Continuity and local wall evidence

Sequence increases per epoch. Holes, drops, reconnect, pause, epoch/domain reset,
explicit discontinuity and missing timing close coverage; PCM is preserved when
only timing is missing. Host endpoints advance across epochs within one domain.
At most1ns adjoining overlap is allowed for directed rounding; larger overlap or
regression produces an unmapped barrier, never a lookahead rewind.

`local-wall-estimate/v1`: cadence target1s, bracket<=5ms, adjacent midpoint
separation<=2s, absolute wall/host delta residual<=10ms. Signed local Unix values
are not certified UTC. Map only within two admitted same-domain/wall-epoch pairs;
no extrapolation. A reversed wall delta or residual above budget invalidates the
whole interval. A frame crossing intervals is wall-mappable only when ALL are
admitted in one wall run. Preserve separate known bracket/resolution/model10ms/
rounding and nullable capture/external bounds; unknown is never added as zero.
Overall physical/UTC accuracy is unknown, including accepted nominal estimates.
Every estimate carries `local_wall_timestamp_estimate` warning.

Canonical producer/consumer vectors live in `tests/fixtures/timed-capture-v1/`.
