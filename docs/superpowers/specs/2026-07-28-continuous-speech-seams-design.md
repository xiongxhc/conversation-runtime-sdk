# Continuous Speech Seams Design

## Problem

The typed-text-to-audio runtime currently treats every sentence terminator and
newline as an immediate phrase boundary. Each phrase is synthesized only after
the previous phrase has finished playback. This produces multi-second pauses
between short clauses. Because each phrase is an independent neural-TTS request,
voice tone and timbre can also vary between clauses. Markdown control characters
such as `*` and `#` are passed into speech input even though they are formatting,
not spoken content.

The text event stream must remain unchanged. This correction applies only to the
speech path.

## Approaches Considered

### Synthesize the complete response once

This gives the strongest continuity and fewest voice resets, but speech cannot
start until language generation finishes. It discards the runtime's incremental
speech benefit and is not selected.

### Keep phrase boundaries and only prefetch synthesis

This removes most synthesis-induced silence but still creates one independent
voice generation and one playback process per short sentence. Voice drift and
formatting artifacts remain. It is insufficient by itself.

### Coalesce, normalize, and prefetch

Short clauses remain buffered across punctuation and newlines until the
configured soft byte limit, end of generation, or hard byte limit. Speech-only
normalization removes Markdown formatting markers without changing emitted text.
A bounded one-item synthesized-audio prefetch overlaps synthesis of segment
`N + 1` with playback of segment `N`.

This is the selected balance: fewer voice resets and shorter inter-segment gaps
without waiting for the complete response.

## Segmentation

Sentence punctuation and newlines become preferred boundaries rather than
unconditional boundaries.

- Before the soft byte limit, sentence punctuation and newlines remain buffered.
- At or after the soft limit, the chunker emits at the latest preferred
  sentence/newline boundary available within the hard limit.
- If no preferred boundary is available, existing soft whitespace or punctuation
  boundaries may be used.
- The hard byte limit remains authoritative and UTF-8 safe.
- End of generation emits the remaining non-empty speech text.

The default `96`-byte soft and `192`-byte hard limits remain unchanged. A short
answer can therefore be synthesized as one utterance, while a longer answer
still begins speaking before generation completes.

## Speech-Only Normalization

Normalization occurs after phrase selection and before `SpeechRequest` creation.
It does not modify `RuntimeEvent::TextDelta`.

- Remove one-to-six heading `#` markers only at line start when followed by
  whitespace.
- Remove list `*` markers only at line start after optional indentation when
  followed by whitespace.
- Remove paired `*` or `**` emphasis delimiters and paired backtick delimiters
  while preserving their enclosed text.
- Remove triple-backtick fence-only lines.
- Collapse formatting-created whitespace and line breaks to a single space.
- Preserve sentence punctuation and ordinary literal content.
- Preserve literal uses such as `C#`, `#topic`, and `2*3`.
- Reject an empty normalized segment by skipping synthesis for that segment.
- Do not implement a general Markdown renderer or rewrite prose.

The normalizer is a private runtime component with pure tests.

## Bounded Prefetch

The speech path uses two ordered stages:

1. A synthesis stage consumes normalized text segments and produces validated
   typed audio.
2. An output stage plays validated audio in original segment order.

The synthesized-audio channel has capacity one. While segment `N` is playing,
the synthesis stage may prepare only segment `N + 1`. This bounds memory and
backend work while hiding most per-request synthesis time.

`FirstSynthesisRequest` is sampled before the first synthesis call.
`FirstPlayableAudio` is sampled after validation of the first synthesized
segment. `SpeechStarted`, `SpeechCompleted`, and terminal-event ordering remain
unchanged.

## Cancellation and Failure

External interruption or lifecycle receiver closure cancels language generation,
active synthesis, queued synthesis, and active playback. Both speech stages are
awaited before the terminal event resolves.

- A synthesis failure cancels playback work that has not completed and reports
  `RuntimeStage::SpeechSynthesizer`.
- An output failure cancels active or queued synthesis and reports
  `RuntimeStage::AudioOutput`.
- No prefetched audio from a cancelled turn may play later.
- Runtime reuse and exactly-one-terminal-event behavior remain mandatory.

## Verification

Deterministic tests will prove:

- short multilingual sentences remain one speech request across `。！？`;
- Markdown headings, emphasis, list markers, and code fences are not sent to TTS;
- text deltas retain their original formatting;
- hard limits remain UTF-8 safe and authoritative;
- synthesis of segment `N + 1` begins while segment `N` is playing;
- playback remains ordered with at most one prefetched audio segment;
- interruption and synthesis/output failures await both stages and discard
  queued audio;
- runtime reuse and exactly one terminal event remain intact.

One local Apple Silicon check will use fixed multi-sentence Chinese text with
`。，！*#`. It will record request count, runtime milestones, playback-process
launches, completion, and cleanup. Subjective continuity will be reported as a
listening observation, not a deterministic guarantee. Independent neural-TTS
requests may still show some variation; eliminating every seam requires a
stateful or streaming TTS backend and is outside this correction.

## Success Criteria

- A short multi-sentence response is synthesized in one request.
- Longer responses do not wait for full language completion before first
  synthesis.
- Normal punctuation no longer creates multi-second synthesis gaps by default.
- Markdown formatting markers are not spoken.
- Voice resets are materially reduced by reducing independent synthesis calls.
- Existing cancellation, backpressure, timing, privacy, and terminal guarantees
  continue to pass strict repository gates.
