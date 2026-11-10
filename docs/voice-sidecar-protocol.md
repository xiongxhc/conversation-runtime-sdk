# Voice sidecar protocol

The gateway and the macOS voice sidecar exchange binary frames. Version 1 is
specified in `docs/superpowers/specs/2026-07-28-r3-real-time-voice-loop-design.md`.
This document covers what version 2 adds.

## Channels and header

Two channels carry frames. Standard input carries gateway-to-sidecar frames and
standard output carries sidecar-to-gateway frames. Captured and rendered media
travels on a separate media channel and never on standard output.

Every frame starts with an 8-byte big-endian header:

| Offset | Field | Type |
|---|---|---|
| 0 | `version` | u16 |
| 2 | `kind` | u16 |
| 4 | `payload_length` | u32 |

The payload follows. Frame kinds, control payloads and limits are those of
version 1.

## Version selection

Version 2 is version 1 plus one frame kind, `TurnAudio`. Every other frame keeps
its version 1 payload exactly.

The gateway selects the version for a session by stamping it on the first
frame, `StartSession`. The sidecar accepts 1 or 2 there, answers every frame in
that same version (including the failure that refuses the `StartSession`
itself), and treats a later frame in a different version, on either channel, as
a failure: it sends a `malformed_frame` failure in the session's version, as
for an unknown version. A failure sent before any `StartSession` is at
version 1. The gateway does the same for frames it reads. A disagreement fails
the session; there is no fallback to another version. Any other version number
is refused.

`TurnAudio` is not a version 1 frame. Kind `0x800b` at version 1 is an unknown
kind, on decode and on encode.

## TurnAudio

Kind `0x800b`, sidecar to gateway, on standard output. The payload is a 24-byte
big-endian header followed by signed 16-bit little-endian samples at 16 kHz,
mono.

| Offset | Field | Type | Rule |
|---|---|---|---|
| 0 | `session_id` | u64 | the session's id |
| 8 | `segment_id` | u64 | the turn's last engine-final segment |
| 16 | `sample_count` | u32 | 1 through 160,000 |
| 20 | `final_segments` | u16 | at least 1; distinct engine-final segments in the turn |
| 22 | `flags` | u16 | bit 0: playback overlapped the turn; other bits 0 |
| 24 | samples | s16le | exactly `sample_count` samples |

The payload limit for this kind is 320,024 bytes. A payload whose length is not
exactly 24 plus twice `sample_count`, a zero or oversized sample count, a zero
`final_segments`, or a set reserved flag bit is malformed.

## When the sidecar sends it

At most one `TurnAudio` frame is sent per turn, at the final-silence reset that
ends the turn. The samples are the turn buffer without its last
`final_silence_ms` of audio (the silence that ended the turn), limited to the
last 160,000 samples, clamped to [-1, 1], scaled by 32,767 and rounded to the
nearest integer.

Nothing is sent for a turn with no engine final, an empty result, a capture
discontinuity, or a version 1 session. A turn that ended before a capture
discontinuity but whose end reaches the session after it sends nothing, and its
remaining finals count for no later turn. Pausing capture, or restarting
recognition after a recoverable recognition failure, discards the turn in
progress, which then sends nothing either. In a version 1 session the
recognition backend copies no turn audio at all.

The playback flag is set when playback was active in any voice window in which
the person was speaking, or when a speech start was sent while playback was
active. Voice windows reach the session sooner than the turn end, which follows
the turn's last hypothesis, so a window can arrive before the end of the turn
before its own. Each turn end therefore names the earliest time the next turn's
audio can begin (the backend's 300 ms pre-roll plus one second for capture
buffering), and any overlap seen at or after that time also marks the next turn.
A turn can be marked for overlap that falls just outside its audio; it is never
left unmarked for overlap inside it that reached the session before its end.

Both recognition backends queue the turn's end behind the turn's last
hypothesis and ahead of the next turn's first, and standard output preserves
frame order, so a turn's `TurnAudio` frame follows every hypothesis of that turn
and precedes every hypothesis of the next.

## Segment ids in a version 2 session

The sidecar restarts segment ids at 1 on every turn and numbers them from 1.
In a version 2 session the gateway gives every hypothesis and every `TurnAudio`
frame a session-unique id: the sidecar's id plus the highest id used by the
turns before it, partial hypotheses included. A turn ends at a `TurnAudio`
frame, at a capture discontinuity, or when a hypothesis arrives with an id at or
below one already final in the same turn; that hypothesis begins the next turn.
A segment id of 0 is malformed in a version 2 session. Version 1 sessions keep
the sidecar's ids.

The gateway keeps at most one turn's audio, replaced by the next `TurnAudio`. A
request for a segment takes the audio only when the ids match, gets nothing once
that segment's turn or the session has ended without that audio, and otherwise
waits.
