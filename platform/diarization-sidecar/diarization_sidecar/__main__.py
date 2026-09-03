"""The sidecar's session loop: one recording in over stdin, one answer out on stdout.

The loop owns three rules the runtime depends on:

- **Audio only ever arrives as bytes on the pipe.** Nothing here opens, writes, or is given a
  path to a recording, so no plaintext file exists to remove on success, failure, or
  cancellation.
- **Cancellation is stdin closing.** The runtime drops the pipe; ``readline`` returns nothing;
  the session ends with no result and a zero exit. There is no signal to catch and no partial
  answer to mistake for a whole one.
- **A failure is one code.** Every exception, expected or not, becomes one line from the closed
  list in :mod:`diarization_sidecar.protocol`. The sidecar's own prose never reaches the
  runtime, because a traceback can quote audio, a filename, or a model path.

Importing this module pulls in the protocol and nothing else. The pipeline — and with it numpy,
torch, and SpeechBrain — is imported the first time a ``start`` line arrives.
"""

from __future__ import annotations

import sys
from typing import BinaryIO, Callable, Protocol

from . import protocol


class Pipeline(Protocol):
    """What the session loop needs from a diarizer, and nothing more."""

    def add_window(self, started_at_ms: int, audio: bytes) -> None: ...

    def finish(self) -> list[protocol.Segment]: ...

    def warnings(self) -> list[str]: ...


PipelineFactory = Callable[[protocol.StartRequest], Pipeline]


def run(
    stdin: BinaryIO,
    stdout: BinaryIO,
    pipeline_factory: PipelineFactory | None = None,
) -> int:
    """Runs one session to its end. Returns the process exit status."""
    factory = pipeline_factory or _speechbrain_pipeline
    pipeline: Pipeline | None = None
    started: protocol.StartRequest | None = None
    try:
        while True:
            raw = stdin.readline(protocol.MAX_LINE_BYTES + 2)
            if not raw:
                # The runtime closed the pipe: the reconciliation was cancelled, or it failed
                # somewhere the sidecar cannot see. Either way there is no answer to give.
                return 0
            request = protocol.decode_request(raw)
            if isinstance(request, protocol.StartRequest):
                if pipeline is not None:
                    raise protocol.ProtocolError("request_invalid")
                started = request
                pipeline = factory(request)
                _emit(stdout, protocol.encode_ready())
            elif pipeline is None or started is None:
                raise protocol.ProtocolError("request_invalid")
            elif isinstance(request, protocol.AudioRequest):
                # A window that is not a whole number of samples is audio this sidecar cannot
                # place on a timeline, so it is refused rather than trimmed to fit.
                if len(request.audio) % (2 * started.channels) != 0:
                    raise protocol.ProtocolError("audio_unsupported")
                pipeline.add_window(request.started_at_ms, request.audio)
            else:
                # The warnings are read after `finish`, because that is where the pipeline
                # learns whether it had to coarsen the recording to cluster it at all.
                segments = pipeline.finish()
                _emit(stdout, protocol.encode_result(segments, pipeline.warnings()))
                return 0
    except protocol.ProtocolError as error:
        _emit(stdout, protocol.encode_failure(error.code))
        return 1
    except BaseException:  # noqa: BLE001 - a code, never the exception's own words
        _emit(stdout, protocol.encode_failure("internal_error"))
        return 1


def main(argv: list[str] | None = None) -> int:
    arguments = sys.argv[1:] if argv is None else argv
    if arguments:
        # Every parameter travels in the start line, where the schema bounds it.
        return 2
    return run(sys.stdin.buffer, sys.stdout.buffer)


def _speechbrain_pipeline(start: protocol.StartRequest) -> Pipeline:
    """The real pipeline, imported here so a protocol-only session never loads the stack."""
    from . import pipeline as pipeline_module

    return pipeline_module.build_pipeline(start)


def _emit(stdout: BinaryIO, line: bytes) -> None:
    stdout.write(line)
    stdout.flush()


if __name__ == "__main__":
    raise SystemExit(main())
