"""The sidecar's JSON Lines protocol: what it accepts, what it refuses, and how a session ends.

Nothing here imports torch, SpeechBrain, or numpy. The protocol is stdlib only, so these tests
run on a machine that has never downloaded a model.
"""

from __future__ import annotations

import base64
import io
import json

import pytest

from diarization_sidecar import protocol
from diarization_sidecar.__main__ import run


def line(payload: dict) -> bytes:
    return (json.dumps(payload) + "\n").encode("utf-8")


def start_payload(**overrides: object) -> dict:
    payload = {
        "type": "start",
        "protocol_version": protocol.PROTOCOL_VERSION,
        "model": "speechbrain/spkrec-ecapa-voxceleb",
        "model_cache_dir": "/tmp/does-not-need-to-exist",
        "max_speakers": 12,
        "encoding": "pcm_s16le",
        "sample_rate_hz": 16_000,
        "channels": 1,
    }
    payload.update(overrides)
    return payload


def audio_payload(**overrides: object) -> dict:
    payload = {
        "type": "audio",
        "started_at_ms": 0,
        "audio": base64.b64encode(b"\x00\x01" * 16).decode("ascii"),
    }
    payload.update(overrides)
    return payload


def test_a_start_request_states_the_whole_recording_configuration() -> None:
    request = protocol.decode_request(line(start_payload()))

    assert isinstance(request, protocol.StartRequest)
    assert request.model == "speechbrain/spkrec-ecapa-voxceleb"
    assert request.model_cache_dir == "/tmp/does-not-need-to-exist"
    assert request.max_speakers == 12
    assert request.encoding == "pcm_s16le"
    assert request.sample_rate_hz == 16_000
    assert request.channels == 1


def test_an_audio_request_carries_raw_samples_and_its_media_offset() -> None:
    request = protocol.decode_request(
        line(audio_payload(started_at_ms=61_000, audio=base64.b64encode(b"ab").decode("ascii")))
    )

    assert isinstance(request, protocol.AudioRequest)
    assert request.started_at_ms == 61_000
    assert request.audio == b"ab"


def test_a_finish_request_carries_nothing_else() -> None:
    assert isinstance(protocol.decode_request(line({"type": "finish"})), protocol.FinishRequest)
    with pytest.raises(protocol.ProtocolError):
        protocol.decode_request(line({"type": "finish", "reason": "done"}))


def test_a_request_naming_a_person_or_a_reference_embedding_is_refused() -> None:
    # The hard non-goal, asserted as a property of the wire: this sidecar has no field that
    # could carry who someone is, and no field that could carry an enrolled voiceprint.
    identity_fields = {
        "speaker_name": "Chris Xiong",
        "speaker_names": ["Chris Xiong"],
        "participants": ["Chris Xiong"],
        "participant_ids": ["ou_1234567890"],
        "feishu_user_id": "ou_1234567890",
        "identity": "Chris Xiong",
        "reference_embedding": [0.1, 0.2, 0.3],
        "reference_embeddings": {"Chris Xiong": [0.1]},
        "enrolled_speakers": {"Chris Xiong": [0.1]},
        "voiceprint": "AAAA",
        "speaker_labels": ["Chris Xiong"],
    }
    for field, value in identity_fields.items():
        with pytest.raises(protocol.ProtocolError) as start_error:
            protocol.decode_request(line(start_payload(**{field: value})))
        assert start_error.value.code == "request_invalid"

        with pytest.raises(protocol.ProtocolError) as audio_error:
            protocol.decode_request(line(audio_payload(**{field: value})))
        assert audio_error.value.code == "request_invalid"

        with pytest.raises(protocol.ProtocolError):
            protocol.decode_request(line({"type": "finish", field: value}))


def test_unknown_and_missing_request_fields_are_refused() -> None:
    with pytest.raises(protocol.ProtocolError):
        protocol.decode_request(line(start_payload(unexpected="value")))
    for absent in ("model", "model_cache_dir", "max_speakers", "encoding", "sample_rate_hz"):
        payload = start_payload()
        del payload[absent]
        with pytest.raises(protocol.ProtocolError):
            protocol.decode_request(line(payload))
    for absent in ("started_at_ms", "audio"):
        payload = audio_payload()
        del payload[absent]
        with pytest.raises(protocol.ProtocolError):
            protocol.decode_request(line(payload))


def test_unusable_request_values_fail_closed() -> None:
    refused = [
        start_payload(protocol_version=2),
        start_payload(protocol_version="1"),
        start_payload(model=""),
        start_payload(model="m" * 257),
        start_payload(model="line\nbreak"),
        start_payload(model_cache_dir="relative/cache"),
        start_payload(model_cache_dir="/cache/../escape"),
        start_payload(model_cache_dir=""),
        start_payload(max_speakers=1),
        start_payload(max_speakers=65),
        start_payload(max_speakers=True),
        start_payload(encoding="pcm_f32le"),
        start_payload(sample_rate_hz=4_000),
        start_payload(sample_rate_hz=192_000),
        start_payload(channels=2),
        audio_payload(started_at_ms=-1),
        audio_payload(started_at_ms=2**63),
        audio_payload(audio=""),
        audio_payload(audio="not base64 !!"),
        audio_payload(audio=123),
        {"type": "unknown"},
        {"type": 7},
        {},
    ]
    for payload in refused:
        with pytest.raises(protocol.ProtocolError) as error:
            protocol.decode_request(line(payload))
        assert error.value.code == "request_invalid", payload

    for raw in (b"not json\n", b"[]\n", b'"text"\n', b"\n", b"\xff\xfe\n"):
        with pytest.raises(protocol.ProtocolError):
            protocol.decode_request(raw)


def test_a_request_line_larger_than_the_limit_is_refused() -> None:
    oversized = base64.b64encode(b"\x00" * (protocol.MAX_LINE_BYTES)).decode("ascii")
    with pytest.raises(protocol.ProtocolError) as error:
        protocol.decode_request(line(audio_payload(audio=oversized)))
    assert error.value.code == "request_invalid"


def test_a_result_states_timed_anonymous_clusters() -> None:
    encoded = protocol.encode_result(
        [
            protocol.Segment(0, 1_500, "spk-1"),
            protocol.Segment(1_500, 3_000, None),
            protocol.Segment(3_000, 4_500, "spk-2"),
        ],
        ["frames_reduced"],
    )

    assert encoded.endswith(b"\n")
    assert json.loads(encoded) == {
        "type": "result",
        "segments": [
            {"start_ms": 0, "end_ms": 1_500, "speaker": "spk-1"},
            {"start_ms": 1_500, "end_ms": 3_000, "speaker": None},
            {"start_ms": 3_000, "end_ms": 4_500, "speaker": "spk-2"},
        ],
        "warnings": ["frames_reduced"],
    }


def test_a_result_the_runtime_could_not_state_is_refused_rather_than_truncated() -> None:
    too_many = [
        protocol.Segment(index * 10, index * 10 + 5, "spk-1")
        for index in range(protocol.MAX_SEGMENTS + 1)
    ]
    with pytest.raises(protocol.ProtocolError) as error:
        protocol.encode_result(too_many, [])
    assert error.value.code == "result_too_large"

    with pytest.raises(protocol.ProtocolError):
        protocol.encode_result([], ["w" * 4096])

    for unusable in (
        protocol.Segment(10, 5, "spk-1"),
        protocol.Segment(-1, 5, "spk-1"),
        protocol.Segment(0, 5, "Chris Xiong"),
        protocol.Segment(0, 5, "spk-0"),
        protocol.Segment(0, 5, ""),
    ):
        with pytest.raises(protocol.ProtocolError):
            protocol.encode_result([unusable], [])

    with pytest.raises(protocol.ProtocolError):
        protocol.encode_result(
            [protocol.Segment(0, 10, "spk-1"), protocol.Segment(5, 20, "spk-2")], []
        )


def test_a_failure_names_a_code_from_the_closed_list() -> None:
    assert json.loads(protocol.encode_failure("model_unavailable")) == {
        "type": "failure",
        "code": "model_unavailable",
    }
    for code in protocol.FAILURE_CODES:
        assert json.loads(protocol.encode_failure(code))["code"] == code
    with pytest.raises(ValueError):
        protocol.encode_failure("a code nobody declared")


class StubPipeline:
    """Stands in for the SpeechBrain pipeline. Records what it was handed."""

    def __init__(self, segments: list[protocol.Segment] | None = None) -> None:
        self.windows: list[tuple[int, int]] = []
        self.segments = segments or [protocol.Segment(0, 1_000, "spk-1")]

    def add_window(self, started_at_ms: int, audio: bytes) -> None:
        self.windows.append((started_at_ms, len(audio)))

    def finish(self) -> list[protocol.Segment]:
        return self.segments


def session(lines: list[bytes], pipeline: StubPipeline | None = None) -> tuple[int, list[dict]]:
    pipeline = pipeline or StubPipeline()
    stdout = io.BytesIO()
    status = run(
        io.BytesIO(b"".join(lines)),
        stdout,
        pipeline_factory=lambda _start: pipeline,
    )
    answers = [json.loads(raw) for raw in stdout.getvalue().splitlines() if raw]
    return status, answers


def test_a_session_answers_one_result_for_the_whole_recording() -> None:
    pipeline = StubPipeline([protocol.Segment(0, 2_000, "spk-1"), protocol.Segment(2_000, 3_000, None)])
    status, answers = session(
        [
            line(start_payload()),
            line(audio_payload(started_at_ms=0)),
            line(audio_payload(started_at_ms=33_000)),
            line({"type": "finish"}),
        ],
        pipeline,
    )

    assert status == 0
    assert answers[0] == {"type": "ready", "protocol_version": protocol.PROTOCOL_VERSION}
    assert answers[1]["type"] == "result"
    assert [segment["speaker"] for segment in answers[1]["segments"]] == ["spk-1", None]
    assert pipeline.windows == [(0, 32), (33_000, 32)]
    assert len(answers) == 2, "one recording is answered exactly once"


def test_closing_stdin_before_finish_ends_the_session_without_a_result() -> None:
    status, answers = session([line(start_payload()), line(audio_payload())])

    assert status == 0
    assert answers == [{"type": "ready", "protocol_version": protocol.PROTOCOL_VERSION}]


def test_a_session_out_of_order_or_misaligned_fails_closed() -> None:
    status, answers = session([line(audio_payload())])
    assert status != 0
    assert answers == [{"type": "failure", "code": "request_invalid"}]

    status, answers = session([line(start_payload()), line(start_payload())])
    assert status != 0
    assert answers[-1] == {"type": "failure", "code": "request_invalid"}

    odd = base64.b64encode(b"\x00\x01\x02").decode("ascii")
    status, answers = session([line(start_payload()), line(audio_payload(audio=odd))])
    assert status != 0
    assert answers[-1] == {"type": "failure", "code": "audio_unsupported"}


def test_a_pipeline_that_cannot_load_is_reported_as_a_bounded_code() -> None:
    def refuse(_start: protocol.StartRequest) -> StubPipeline:
        raise protocol.ProtocolError("model_unavailable")

    stdout = io.BytesIO()
    status = run(io.BytesIO(line(start_payload())), stdout, pipeline_factory=refuse)

    assert status != 0
    assert json.loads(stdout.getvalue()) == {"type": "failure", "code": "model_unavailable"}


def test_an_unexpected_pipeline_failure_never_quotes_its_own_prose() -> None:
    def explode(_start: protocol.StartRequest) -> StubPipeline:
        raise RuntimeError("/Users/cx/private/meeting.wav could not be opened")

    stdout = io.BytesIO()
    status = run(io.BytesIO(line(start_payload())), stdout, pipeline_factory=explode)

    assert status != 0
    assert json.loads(stdout.getvalue()) == {"type": "failure", "code": "internal_error"}
    assert b"meeting.wav" not in stdout.getvalue()
