"""Speaker embeddings and spectral clustering, from windows of PCM to timed anonymous turns.

The pipeline never transcribes. SpeechBrain's ECAPA-TDNN model answers *who sounds like whom*,
not *what was said*, so a segment leaves here with a cluster and a span and no text. The runtime
fills the text field with an empty string rather than letting a diarizer appear to have heard
words.

Two boundaries are deliberate:

- **The torch stack is imported inside :func:`load_speechbrain_embedder`**, so a session that
  never starts — and every test in this package — costs no model load, no CUDA probe, and no
  network. numpy is imported at module scope because the clustering *is* linear algebra.
- **The embedder is a parameter.** :class:`DiarizationPipeline` is handed a callable that turns
  frames of PCM into vectors, so the clustering can be exercised with synthetic embeddings and
  no checkpoint on disk.

The model is loaded from the configured cache directory with Hugging Face's offline switch set,
so a missing checkpoint fails closed instead of quietly reaching the network mid-meeting.
"""

from __future__ import annotations

from typing import Callable, Sequence

import numpy

from . import protocol

#: How much audio one embedding covers. 1.5 s is the usual window for ECAPA speaker embeddings:
#: shorter is too little voice to characterise, longer smears a speaker change into one vector.
DEFAULT_FRAME_MS = 1_500
#: A trailing remainder shorter than this is dropped rather than embedded — the vector it would
#: produce says more about the window's edge than about a voice.
MIN_FRAME_MS = 500
#: The most frames one recording may cluster. The affinity matrix is frames squared, so this is
#: what keeps a long meeting's memory bounded; beyond it, adjacent frames are merged and the
#: effective resolution halves rather than the recording being refused.
MAX_EMBEDDING_FRAMES = 2_048
#: How close the second-best cluster may come before a frame is left unattributed. Overlapping
#: speech embeds between two voices and belongs to neither, and a guess there would put words in
#: someone's mouth. It is a judgement, not a measurement: see the human checklist.
AMBIGUITY_RATIO = 0.8
#: k-means is run to a fixed iteration count so one recording always clusters the same way.
KMEANS_ITERATIONS = 50
#: The deterministic VAD is deliberately before the speaker embedder. It rejects an energetic
#: signal unless it also has voice-like envelope variation and is neither DC nor a single steady
#: spectral line. It is a conservative local guard, not identity inference or a model download.
VAD_RMS_THRESHOLD = 256
VAD_MIN_ACTIVE_BLOCK_RATIO = 0.6
VAD_MIN_ENVELOPE_VARIATION = 0.08
VAD_MAX_DC_RATIO = 0.35
VAD_MAX_TONAL_PEAK_RATIO = 0.45

Embedder = Callable[[list[bytes]], Sequence[Sequence[float]]]


class DiarizationPipeline:
    """Accumulates windows of PCM and answers with timed, anonymously clustered turns."""

    def __init__(
        self,
        embed: Embedder,
        sample_rate_hz: int,
        channels: int,
        max_speakers: int,
        frame_ms: int = DEFAULT_FRAME_MS,
    ) -> None:
        self._embed = embed
        self._sample_rate_hz = sample_rate_hz
        self._channels = channels
        self._max_speakers = max_speakers
        self._frame_ms = frame_ms
        self._sample_bytes = 2 * channels
        self._frame_bytes = self._sample_bytes * sample_rate_hz * frame_ms // 1_000
        self._buffer = bytearray()
        self._buffer_started_at_ms = 0
        self._frames: list[tuple[int, int, Sequence[float]]] = []
        self._reduced = False

    def add_window(self, started_at_ms: int, audio: bytes) -> None:
        """Takes one window of decrypted audio at its media-timeline offset."""
        if not audio or len(audio) % self._sample_bytes != 0:
            raise protocol.ProtocolError("audio_unsupported")
        if self._buffer and started_at_ms != self._buffer_end_ms():
            # A recording gap, or a window the runtime cut elsewhere: the samples held back do
            # not continue into these, so they are closed off before the new window begins.
            self._flush_remainder()
        if not self._buffer:
            self._buffer_started_at_ms = started_at_ms
        self._buffer.extend(audio)
        self._cut_whole_frames()

    def finish(self) -> list[protocol.Segment]:
        """Clusters everything accumulated so far and returns the meeting's turns."""
        self._flush_remainder()
        self._reduce_to_budget()
        labels = cluster_embeddings(
            [embedding for _, _, embedding in self._frames], self._max_speakers
        )
        return _segments([(start, end) for start, end, _ in self._frames], labels)

    def frame_count(self) -> int:
        return len(self._frames)

    def warnings(self) -> list[str]:
        """What made this answer less than a full account of the recording.

        A reduced recording was clustered at coarser frames than the configured resolution, so
        a short turn may have been merged into a neighbour. The caller is told rather than left
        to assume the resolution it configured is the one it got.
        """
        return ["frames_reduced"] if self._reduced else []

    def _buffer_end_ms(self) -> int:
        return self._buffer_started_at_ms + self._duration_ms(len(self._buffer))

    def _duration_ms(self, byte_count: int) -> int:
        samples = byte_count // self._sample_bytes
        return samples * 1_000 // self._sample_rate_hz

    def _cut_whole_frames(self) -> None:
        cut = len(self._buffer) // self._frame_bytes
        if cut == 0:
            return
        frames = [
            bytes(self._buffer[index * self._frame_bytes : (index + 1) * self._frame_bytes])
            for index in range(cut)
        ]
        started_at_ms = self._buffer_started_at_ms
        del self._buffer[: cut * self._frame_bytes]
        self._buffer_started_at_ms = started_at_ms + cut * self._frame_ms
        self._append(started_at_ms, frames, self._frame_ms)

    def _flush_remainder(self) -> None:
        remainder = bytes(self._buffer)
        started_at_ms = self._buffer_started_at_ms
        self._buffer = bytearray()
        duration_ms = self._duration_ms(len(remainder))
        if duration_ms >= MIN_FRAME_MS:
            self._append(started_at_ms, [remainder], duration_ms)

    def _append(self, started_at_ms: int, frames: list[bytes], frame_ms: int) -> None:
        speech_frames = [
            (started_at_ms + index * frame_ms, frame)
            for index, frame in enumerate(frames)
            if _contains_speech(frame, self._sample_rate_hz, self._channels)
        ]
        if not speech_frames:
            return
        embeddings = self._embed([frame for _, frame in speech_frames])
        if len(embeddings) != len(speech_frames):
            raise protocol.ProtocolError("internal_error")
        for (start, _), embedding in zip(speech_frames, embeddings):
            self._frames.append((start, start + frame_ms, embedding))

    def _reduce_to_budget(self) -> None:
        """Halves the resolution until the frame count fits, merging contiguous frames only."""
        while len(self._frames) > MAX_EMBEDDING_FRAMES:
            merged: list[tuple[int, int, Sequence[float]]] = []
            index = 0
            while index < len(self._frames):
                current = self._frames[index]
                following = self._frames[index + 1] if index + 1 < len(self._frames) else None
                if following is not None and following[0] == current[1]:
                    average = [
                        (left + right) / 2.0 for left, right in zip(current[2], following[2])
                    ]
                    merged.append((current[0], following[1], average))
                    index += 2
                else:
                    merged.append(current)
                    index += 1
            if len(merged) == len(self._frames):
                # Nothing was contiguous, so nothing can be merged. The recording is stated as
                # it is rather than clustered past the memory bound.
                raise protocol.ProtocolError("audio_unsupported")
            self._frames = merged
            self._reduced = True


def _contains_speech(frame: bytes, sample_rate_hz: int, channels: int) -> bool:
    """Conservative deterministic VAD for the local embedding boundary.

    Energy alone calls a DC offset, a sine tone, or stationary noise speech. This compact
    classifier first rejects those non-speech shapes, then requires sustained active 20 ms
    blocks with a changing envelope. It is deterministic and entirely local so tests do not
    need SpeechBrain, torch, a checkpoint, or a network connection.
    """
    samples = numpy.frombuffer(frame, dtype="<i2").astype(numpy.float64)
    if samples.size < channels:
        return False
    samples = samples[: samples.size - samples.size % channels].reshape(-1, channels).mean(axis=1)
    absolute_mean = float(numpy.mean(numpy.abs(samples)))
    if absolute_mean == 0.0:
        return False
    dc_ratio = abs(float(numpy.mean(samples))) / absolute_mean
    if dc_ratio > VAD_MAX_DC_RATIO:
        return False

    centered = samples - numpy.mean(samples)
    if float(numpy.sqrt(numpy.mean(centered * centered))) < VAD_RMS_THRESHOLD:
        return False

    block_samples = max(1, sample_rate_hz // 50)
    blocks = centered[: centered.size - centered.size % block_samples].reshape(-1, block_samples)
    if blocks.size == 0:
        return False
    envelope = numpy.sqrt(numpy.mean(blocks * blocks, axis=1))
    if float(numpy.mean(envelope > VAD_RMS_THRESHOLD)) < VAD_MIN_ACTIVE_BLOCK_RATIO:
        return False
    envelope_mean = float(numpy.mean(envelope))
    if envelope_mean == 0.0 or float(numpy.std(envelope)) / envelope_mean < VAD_MIN_ENVELOPE_VARIATION:
        return False

    windowed = centered * numpy.hanning(centered.size)
    power = numpy.abs(numpy.fft.rfft(windowed)) ** 2
    non_dc_power = power[1:]
    total_power = float(numpy.sum(non_dc_power))
    if total_power == 0.0:
        return False
    return float(numpy.max(non_dc_power)) / total_power < VAD_MAX_TONAL_PEAK_RATIO


def cluster_embeddings(
    embeddings: Sequence[Sequence[float]], max_speakers: int
) -> list[int | None]:
    """Spectral clustering over cosine affinity. Ambiguous frames are left unattributed.

    Returns one label per embedding: an integer cluster index, or ``None`` where the frame sits
    between two clusters — which is what overlapping speech looks like once it is one vector.
    """
    if len(embeddings) == 0:
        return []
    if len(embeddings) == 1:
        return [0]

    # macOS' Accelerate BLAS raises the floating-point flags on large exact matmuls, so the
    # flags are ignored and the property they stand for is asserted directly instead.
    with numpy.errstate(all="ignore"):
        unit = _unit_rows(numpy.asarray(embeddings, dtype=numpy.float64))
        affinity = numpy.clip(unit @ unit.T, 0.0, 1.0)
        numpy.fill_diagonal(affinity, 0.0)
        if not numpy.isfinite(affinity).all():
            raise protocol.ProtocolError("internal_error")
        count = _speaker_count(affinity, max_speakers)
        if count <= 1:
            return [0] * len(embeddings)

        spectral = _unit_rows(_spectral_embedding(affinity, count))
        labels = _kmeans(spectral, count)
        return _withhold_ambiguous(unit, labels, count)


def build_pipeline(start: protocol.StartRequest) -> DiarizationPipeline:
    """The pipeline a real session runs. Loading the model is the first heavy thing that happens."""
    return DiarizationPipeline(
        embed=load_speechbrain_embedder(start.model, start.model_cache_dir),
        sample_rate_hz=start.sample_rate_hz,
        channels=start.channels,
        max_speakers=start.max_speakers,
    )


def load_speechbrain_embedder(model: str, model_cache_dir: str) -> Embedder:
    """Loads the pretrained ECAPA-TDNN speaker encoder from the configured local cache.

    The torch stack is imported here and nowhere else. Hugging Face's offline switch is set
    first, so a checkpoint the operator has not fetched fails closed rather than reaching the
    network in the middle of a meeting.
    """
    import os

    os.environ["HF_HUB_OFFLINE"] = "1"
    os.environ["TRANSFORMERS_OFFLINE"] = "1"
    try:
        import torch
        from speechbrain.inference.speaker import EncoderClassifier
    except ImportError as error:
        raise protocol.ProtocolError("pipeline_unavailable") from error

    try:
        classifier = EncoderClassifier.from_hparams(
            source=model,
            savedir=model_cache_dir,
            run_opts={"device": "cpu"},
        )
    except Exception as error:
        raise protocol.ProtocolError("model_unavailable") from error

    def embed(frames: list[bytes]) -> Sequence[Sequence[float]]:
        if len({len(frame) for frame in frames}) != 1:
            return [embed([frame])[0] for frame in frames]
        batch = torch.stack(
            [
                torch.frombuffer(bytearray(frame), dtype=torch.int16).to(torch.float32) / 32_768.0
                for frame in frames
            ]
        )
        with torch.no_grad():
            return classifier.encode_batch(batch).squeeze(1).tolist()

    return embed


def _unit_rows(matrix: numpy.ndarray) -> numpy.ndarray:
    norms = numpy.linalg.norm(matrix, axis=1, keepdims=True)
    return matrix / numpy.maximum(norms, 1e-12)


def _spectral_embedding(affinity: numpy.ndarray, count: int) -> numpy.ndarray:
    _, vectors = numpy.linalg.eigh(_normalized_laplacian(affinity))
    embedding = vectors[:, :count]
    # eigh fixes an eigenvector only up to sign, so each column is oriented by its largest
    # component. Without this the k-means start would depend on the LAPACK build.
    for column in range(embedding.shape[1]):
        if embedding[numpy.argmax(numpy.abs(embedding[:, column])), column] < 0:
            embedding[:, column] *= -1.0
    return embedding


def _normalized_laplacian(affinity: numpy.ndarray) -> numpy.ndarray:
    degree = numpy.maximum(affinity.sum(axis=1), 1e-12)
    scale = 1.0 / numpy.sqrt(degree)
    return numpy.eye(affinity.shape[0]) - (scale[:, None] * affinity * scale[None, :])


def _speaker_count(affinity: numpy.ndarray, max_speakers: int) -> int:
    """The eigen-gap heuristic, capped by the operator's maximum.

    Ties resolve to the *smaller* count on purpose: under-splitting merges two voices into one
    cluster, while over-splitting invents a speaker who was never in the room.
    """
    values = numpy.linalg.eigvalsh(_normalized_laplacian(affinity))
    considered = min(max_speakers, len(values) - 1)
    gaps = values[1 : considered + 1] - values[:considered]
    return int(numpy.argmax(gaps)) + 1


def _kmeans(points: numpy.ndarray, count: int) -> numpy.ndarray:
    centroids = points[_farthest_first(points, count)]
    labels = numpy.zeros(points.shape[0], dtype=int)
    for _ in range(KMEANS_ITERATIONS):
        distances = numpy.linalg.norm(points[:, None, :] - centroids[None, :, :], axis=2)
        assigned = numpy.argmin(distances, axis=1)
        if numpy.array_equal(assigned, labels):
            break
        labels = assigned
        for cluster in range(count):
            members = points[labels == cluster]
            if len(members):
                centroids[cluster] = members.mean(axis=0)
    return labels


def _contains_speech(frame: bytes) -> bool:
    """A deterministic PCM energy VAD for the bounded frames sent to the encoder."""
    samples = numpy.frombuffer(frame, dtype="<i2").astype(numpy.int32)
    return float(numpy.abs(samples).mean()) >= SPEECH_ACTIVITY_THRESHOLD


def _farthest_first(points: numpy.ndarray, count: int) -> list[int]:
    """Deterministic seeding: the point farthest from the mean, then farthest from the chosen."""
    chosen = [int(numpy.argmax(numpy.linalg.norm(points - points.mean(axis=0), axis=1)))]
    while len(chosen) < count:
        distances = numpy.linalg.norm(points[:, None, :] - points[chosen][None, :, :], axis=2)
        chosen.append(int(numpy.argmax(distances.min(axis=1))))
    return chosen


def _withhold_ambiguous(
    unit: numpy.ndarray, labels: numpy.ndarray, count: int
) -> list[int | None]:
    """Drops the label of any frame whose runner-up cluster is nearly as close as its own."""
    centroids = numpy.stack(
        [
            unit[labels == cluster].mean(axis=0) if numpy.any(labels == cluster) else unit[0] * 0.0
            for cluster in range(count)
        ]
    )
    similarity = _unit_rows(unit) @ _unit_rows(centroids).T
    ordered = numpy.sort(similarity, axis=1)
    best = numpy.maximum(ordered[:, -1], 1e-12)
    runner_up = ordered[:, -2]
    return [
        None if runner_up[index] / best[index] > AMBIGUITY_RATIO else int(label)
        for index, label in enumerate(labels)
    ]


def _segments(
    spans: list[tuple[int, int]], labels: list[int | None]
) -> list[protocol.Segment]:
    """Merges contiguous frames that agree, and names each cluster in order of first appearance."""
    names: dict[int, str] = {}
    segments: list[protocol.Segment] = []
    for (start, end), label in zip(spans, labels):
        speaker = None
        if label is not None:
            speaker = names.setdefault(label, f"spk-{len(names) + 1}")
        if segments and segments[-1].ended_at_ms == start and segments[-1].speaker == speaker:
            segments[-1] = protocol.Segment(segments[-1].started_at_ms, end, speaker)
        else:
            segments.append(protocol.Segment(start, end, speaker))
    return segments
