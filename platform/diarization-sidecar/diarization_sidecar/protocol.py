"""The JSON Lines protocol the runtime and this sidecar speak over stdio.

One recording is one session: a ``start`` line states the configuration, ``audio`` lines carry
the recording's decrypted samples in media-timeline order, and ``finish`` asks for the answer.
The sidecar replies with ``ready`` once, then exactly one ``result`` or one ``failure``.

Three properties are the protocol's own rather than an implementation's:

- **No identity may be expressed.** Every request field is listed here and anything else is
  refused, so there is no field that could carry a participant, a name, an account id, or an
  enrolled voiceprint. The sidecar cannot be asked "is this Chris" because the question has no
  spelling.
- **No path may be expressed.** Audio arrives as bytes on the pipe. The sidecar is never given
  a plaintext file to open, so a plaintext file never has to exist.
- **Diagnostics are a closed list of codes.** A failure names one of :data:`FAILURE_CODES` and
  nothing else, so a diagnostic can never quote a filename, a transcript, or a credential.

Stdlib only. Importing this module pulls in neither numpy nor the torch stack.
"""

from __future__ import annotations

import base64
import binascii
import json
import re
from dataclasses import dataclass

PROTOCOL_VERSION = 1

#: The longest request line accepted. One reconciliation window is a mebibyte of PCM, which is
#: about 1.4 MB once base64 encoded, so this leaves room for the JSON around it and refuses
#: anything that could only be an attempt to make the sidecar buffer without bound.
MAX_LINE_BYTES = 2 * 1024 * 1024
#: The longest answer emitted. It matches the runtime's own response cap: an answer past this
#: cannot become a result the runtime would accept, so producing one is work on the way to a
#: refusal.
MAX_RESPONSE_BYTES = 512 * 1024
#: Mirrors the runtime's `MAX_DIARIZED_SEGMENTS`.
MAX_SEGMENTS = 1024
#: Mirrors the runtime's `MAX_SPEAKER_CLUSTERS`.
MAX_SPEAKERS = 64
MAX_WARNINGS = 8

MAX_MODEL_BYTES = 256
MAX_CACHE_DIR_BYTES = 1024
MIN_SAMPLE_RATE_HZ = 8_000
MAX_SAMPLE_RATE_HZ = 48_000
MIN_SPEAKERS = 2
MAX_INT64 = 2**63 - 1

SUPPORTED_ENCODINGS = frozenset({"pcm_s16le"})

#: Every diagnostic this sidecar can emit. A code outside the list is a programming error here,
#: not a value a caller can produce.
FAILURE_CODES = frozenset(
    {
        "request_invalid",
        "audio_unsupported",
        "pipeline_unavailable",
        "model_unavailable",
        "result_too_large",
        "internal_error",
    }
)

_SPEAKER_LABEL = re.compile(r"^spk-[1-9][0-9]{0,3}$")
_WARNING_CODE = re.compile(r"^[a-z0-9_]{1,64}$")

_START_FIELDS = frozenset(
    {
        "type",
        "protocol_version",
        "model",
        "model_cache_dir",
        "max_speakers",
        "encoding",
        "sample_rate_hz",
        "channels",
    }
)
_AUDIO_FIELDS = frozenset({"type", "started_at_ms", "audio"})
_FINISH_FIELDS = frozenset({"type"})


class ProtocolError(Exception):
    """A refusal, carrying one code from :data:`FAILURE_CODES` and no other detail."""

    def __init__(self, code: str) -> None:
        if code not in FAILURE_CODES:
            raise ValueError("failure code is not declared")
        super().__init__(code)
        self.code = code


@dataclass(frozen=True)
class StartRequest:
    model: str
    model_cache_dir: str
    max_speakers: int
    encoding: str
    sample_rate_hz: int
    channels: int


@dataclass(frozen=True)
class AudioRequest:
    started_at_ms: int
    audio: bytes


@dataclass(frozen=True)
class FinishRequest:
    pass


@dataclass(frozen=True)
class Segment:
    """One stretch of one voice, on the media timeline.

    ``speaker`` is this sidecar's own anonymous label (``spk-1``) or ``None`` where the audio
    could not be attributed to a single voice. The runtime discards even these labels and mints
    its own, so nothing a clustering run decided can travel out as an identity.
    """

    started_at_ms: int
    ended_at_ms: int
    speaker: str | None


def decode_request(raw: bytes | str) -> StartRequest | AudioRequest | FinishRequest:
    """Parses one request line. Anything unexpected is refused rather than ignored."""
    if isinstance(raw, str):
        raw = raw.encode("utf-8")
    if len(raw) > MAX_LINE_BYTES:
        raise ProtocolError("request_invalid")
    try:
        payload = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ProtocolError("request_invalid") from error
    if not isinstance(payload, dict):
        raise ProtocolError("request_invalid")

    kind = payload.get("type")
    if kind == "start":
        _require_fields(payload, _START_FIELDS)
        _integer(payload["protocol_version"], PROTOCOL_VERSION, PROTOCOL_VERSION)
        if payload["encoding"] not in SUPPORTED_ENCODINGS:
            raise ProtocolError("request_invalid")
        return StartRequest(
            model=_text(payload["model"], MAX_MODEL_BYTES),
            model_cache_dir=_absolute_path(payload["model_cache_dir"]),
            max_speakers=_integer(payload["max_speakers"], MIN_SPEAKERS, MAX_SPEAKERS),
            encoding=payload["encoding"],
            sample_rate_hz=_integer(
                payload["sample_rate_hz"], MIN_SAMPLE_RATE_HZ, MAX_SAMPLE_RATE_HZ
            ),
            channels=_integer(payload["channels"], 1, 1),
        )
    if kind == "audio":
        _require_fields(payload, _AUDIO_FIELDS)
        return AudioRequest(
            started_at_ms=_integer(payload["started_at_ms"], 0, MAX_INT64),
            audio=_samples(payload["audio"]),
        )
    if kind == "finish":
        _require_fields(payload, _FINISH_FIELDS)
        return FinishRequest()
    raise ProtocolError("request_invalid")


def encode_ready() -> bytes:
    return _line({"type": "ready", "protocol_version": PROTOCOL_VERSION})


def encode_result(segments: list[Segment], warnings: list[str]) -> bytes:
    """Encodes the one answer a session produces, refusing one the runtime could not state."""
    if len(segments) > MAX_SEGMENTS or len(warnings) > MAX_WARNINGS:
        raise ProtocolError("result_too_large")
    speakers: set[str] = set()
    previous_end = 0
    for segment in segments:
        if (
            not isinstance(segment, Segment)
            or not 0 <= segment.started_at_ms <= segment.ended_at_ms <= MAX_INT64
            or segment.started_at_ms < previous_end
        ):
            raise ProtocolError("internal_error")
        if segment.speaker is not None:
            if not isinstance(segment.speaker, str) or not _SPEAKER_LABEL.match(segment.speaker):
                raise ProtocolError("internal_error")
            speakers.add(segment.speaker)
        previous_end = segment.ended_at_ms
    if len(speakers) > MAX_SPEAKERS:
        raise ProtocolError("result_too_large")
    for warning in warnings:
        if not isinstance(warning, str) or not _WARNING_CODE.match(warning):
            raise ProtocolError("result_too_large")

    encoded = _line(
        {
            "type": "result",
            "segments": [
                {
                    "start_ms": segment.started_at_ms,
                    "end_ms": segment.ended_at_ms,
                    "speaker": segment.speaker,
                }
                for segment in segments
            ],
            "warnings": list(warnings),
        }
    )
    if len(encoded) > MAX_RESPONSE_BYTES:
        raise ProtocolError("result_too_large")
    return encoded


def encode_failure(code: str) -> bytes:
    if code not in FAILURE_CODES:
        raise ValueError("failure code is not declared")
    return _line({"type": "failure", "code": code})


def _line(payload: dict) -> bytes:
    return (json.dumps(payload, separators=(",", ":"), ensure_ascii=True) + "\n").encode("utf-8")


def _require_fields(payload: dict, allowed: frozenset[str]) -> None:
    if set(payload) != allowed:
        raise ProtocolError("request_invalid")


def _integer(value: object, low: int, high: int) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or not low <= value <= high:
        raise ProtocolError("request_invalid")
    return value


def _text(value: object, maximum_bytes: int) -> str:
    if (
        not isinstance(value, str)
        or not value.strip()
        or len(value.encode("utf-8")) > maximum_bytes
        or any(character.isspace() and character != " " for character in value)
        or any(ord(character) < 0x20 or ord(character) == 0x7F for character in value)
    ):
        raise ProtocolError("request_invalid")
    return value


def _absolute_path(value: object) -> str:
    path = _text(value, MAX_CACHE_DIR_BYTES)
    if not path.startswith("/") or ".." in path.split("/"):
        raise ProtocolError("request_invalid")
    return path


def _samples(value: object) -> bytes:
    if not isinstance(value, str) or not value:
        raise ProtocolError("request_invalid")
    try:
        samples = base64.b64decode(value, validate=True)
    except (binascii.Error, ValueError) as error:
        raise ProtocolError("request_invalid") from error
    if not samples:
        raise ProtocolError("request_invalid")
    return samples
