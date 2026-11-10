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
that same version, and treats a later frame in a different version as a failure.
The gateway does the same for frames it reads. A disagreement fails the session;
there is no fallback to another version. Any other version number is refused.

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
discontinuity, or a version 1 session.

Standard output preserves frame order, so a turn's `TurnAudio` frame follows the
finals of that turn.
