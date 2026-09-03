"""The clustering pipeline, driven by synthetic embeddings instead of a model.

Every test here injects its own embedder, so no model is downloaded, no network is touched,
and neither torch nor SpeechBrain is imported. The last of those is asserted rather than
assumed.
"""

from __future__ import annotations

import inspect
import math
import sys

import pytest

from diarization_sidecar import pipeline, protocol

SAMPLE_RATE_HZ = 16_000
FRAME_MS = 1_000
FRAME_BYTES = SAMPLE_RATE_HZ * 2 * FRAME_MS // 1_000

# Three mutually distinct unit directions plus the midpoint of the first two.
VOICE_A = [1.0, 0.0, 0.0]
VOICE_B = [0.0, 1.0, 0.0]
VOICE_C = [0.0, 0.0, 1.0]
OVERLAP_AB = [0.5 * math.sqrt(2.0), 0.5 * math.sqrt(2.0), 0.0]


class ScriptedEmbedder:
    """Answers with the next scripted embedding for each frame it is handed."""

    def __init__(self, embeddings: list[list[float]]) -> None:
        self.embeddings = list(embeddings)
        self.frame_lengths: list[int] = []

    def __call__(self, frames: list[bytes]) -> list[list[float]]:
        self.frame_lengths.extend(len(frame) for frame in frames)
        taken = self.embeddings[: len(frames)]
        self.embeddings = self.embeddings[len(frames) :]
        assert len(taken) == len(frames), "the script must cover every frame"
        return taken


def build(embeddings: list[list[float]], max_speakers: int = 12) -> tuple[pipeline.DiarizationPipeline, ScriptedEmbedder]:
    embedder = ScriptedEmbedder(embeddings)
    return (
        pipeline.DiarizationPipeline(
            embed=embedder,
            sample_rate_hz=SAMPLE_RATE_HZ,
            channels=1,
            max_speakers=max_speakers,
            frame_ms=FRAME_MS,
        ),
        embedder,
    )


def audio(seconds: int) -> bytes:
    return b"\x01\x02" * (SAMPLE_RATE_HZ * seconds)


def test_synthetic_embeddings_cluster_deterministically() -> None:
    first = pipeline.cluster_embeddings([VOICE_A, VOICE_A, VOICE_B, VOICE_B], max_speakers=12)
    again = pipeline.cluster_embeddings([VOICE_A, VOICE_A, VOICE_B, VOICE_B], max_speakers=12)

    assert first == again, "the same embeddings must always produce the same clusters"
    assert first[0] == first[1] and first[2] == first[3]
    assert first[0] != first[2]
    assert sorted(set(first)) == [0, 1]


def test_three_voices_are_separated_and_never_exceed_the_configured_maximum() -> None:
    labels = pipeline.cluster_embeddings(
        [VOICE_A, VOICE_A, VOICE_B, VOICE_B, VOICE_C, VOICE_C], max_speakers=12
    )
    assert len({label for label in labels if label is not None}) == 3

    capped = pipeline.cluster_embeddings(
        [VOICE_A, VOICE_A, VOICE_B, VOICE_B, VOICE_C, VOICE_C], max_speakers=2
    )
    assert len({label for label in capped if label is not None}) <= 2


def test_an_overlapping_frame_is_reported_without_a_cluster_rather_than_guessed() -> None:
    labels = pipeline.cluster_embeddings(
        [VOICE_A, VOICE_A, VOICE_A, OVERLAP_AB, VOICE_B, VOICE_B, VOICE_B], max_speakers=12
    )

    assert labels[3] is None, f"a frame between two voices is not assigned to either: {labels}"
    assert all(label is not None for index, label in enumerate(labels) if index != 3)


def test_windows_become_timed_segments_without_importing_a_model() -> None:
    diarization, embedder = build([VOICE_A, VOICE_A, VOICE_B, VOICE_B])
    diarization.add_window(0, audio(2))
    diarization.add_window(2_000, audio(2))
    segments = diarization.finish()

    assert embedder.frame_lengths == [FRAME_BYTES] * 4
    assert segments == [
        protocol.Segment(0, 2_000, "spk-1"),
        protocol.Segment(2_000, 4_000, "spk-2"),
    ], "adjacent frames of one voice become one segment"
    assert "torch" not in sys.modules and "speechbrain" not in sys.modules


def test_a_window_that_does_not_continue_the_previous_one_starts_a_new_frame() -> None:
    diarization, _ = build([VOICE_A, VOICE_B])
    diarization.add_window(0, audio(1))
    # A recording gap: the next window's media offset is later than the audio so far.
    diarization.add_window(9_000, audio(1))
    segments = diarization.finish()

    assert [(segment.started_at_ms, segment.ended_at_ms) for segment in segments] == [
        (0, 1_000),
        (9_000, 10_000),
    ]


def test_a_partial_frame_is_carried_into_the_next_contiguous_window() -> None:
    diarization, embedder = build([VOICE_A, VOICE_A])
    diarization.add_window(0, b"\x01\x02" * (SAMPLE_RATE_HZ * 3 // 2))
    diarization.add_window(1_500, b"\x01\x02" * (SAMPLE_RATE_HZ // 2))
    segments = diarization.finish()

    assert embedder.frame_lengths == [FRAME_BYTES, FRAME_BYTES]
    assert segments == [protocol.Segment(0, 2_000, "spk-1")]


def test_the_frame_budget_bounds_a_long_recording() -> None:
    frames = pipeline.MAX_EMBEDDING_FRAMES + 10
    diarization, _ = build([VOICE_A] * frames)
    for index in range(frames):
        diarization.add_window(index * FRAME_MS, audio(1))
    segments = diarization.finish()

    assert segments == [protocol.Segment(0, frames * FRAME_MS, "spk-1")]
    assert diarization.frame_count() <= pipeline.MAX_EMBEDDING_FRAMES


def test_a_recording_with_no_audio_produces_no_segments() -> None:
    diarization, _ = build([])
    assert diarization.finish() == []


def test_the_pipeline_accepts_no_identity_and_no_reference_embedding() -> None:
    forbidden = (
        "identity",
        "name",
        "speaker_name",
        "participant",
        "participants",
        "reference",
        "reference_embedding",
        "enrolled",
        "enrollment",
        "voiceprint",
        "profile",
    )
    public = [
        value
        for name, value in vars(pipeline).items()
        if not name.startswith("_") and (inspect.isfunction(value) or inspect.isclass(value))
        and getattr(value, "__module__", "") == pipeline.__name__
    ]
    assert public, "the module must expose something to inspect"
    for value in public:
        callables = [value] if inspect.isfunction(value) else [value.__init__]
        for entry in callables:
            for parameter in inspect.signature(entry).parameters:
                assert parameter not in forbidden, f"{value.__name__}({parameter})"


def test_a_pipeline_refuses_audio_it_cannot_align() -> None:
    diarization, _ = build([VOICE_A])
    with pytest.raises(protocol.ProtocolError) as error:
        diarization.add_window(0, b"\x01\x02\x03")
    assert error.value.code == "audio_unsupported"
