# Task 2 Report — Normalize Speech-Only Formatting

## Scope

- Added a private line-aware speech normalizer in `crates/runtime/src/speech_text.rs`.
- Registered it in the runtime and normalize only after phrase selection, before `SpeechSegment` construction and index assignment.
- Updated the Task 1 runtime assertion to preserve original `TextDelta` while synthesizing normalized text.
- Added a formatting-only turn regression that completes once without speech lifecycle or synthesis events.
- Left Task 1 implementation and plan corrections unchanged.

## TDD Evidence

1. Added the normalizer unit tests and runtime assertions before implementing the normalizer.
2. RED command:

   ```bash
   PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime speech_text::tests
   ```

   Result: failed as expected with `E0432`, because `normalize_speech_text` was not yet defined.

3. The first implementation used Rust 2024 let-chain syntax, which the workspace edition rejects. Replaced those conditionals with the existing edition-compatible nested form. The next focused run exposed the missing bare-`#` formatting-only case; the normalizer now discards that marker before queueing speech.

## Verification

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime speech_text
```

Result: passed — 3 normalizer tests.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime short_sentences_are_one_speech_request_without_changing_text_deltas
```

Result: passed — original text delta and normalized queued speech assertion.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime formatting_only_output_preserves_text_deltas_and_skips_speech_lifecycle
```

Result: passed — original text delta, no synthesis, no speech lifecycle events, exactly one completion.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo fmt --all
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo clippy --locked -p conversation-runtime --all-targets -- -D warnings
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo fmt --all -- --check
git diff --check
```

Result: passed — 69 runtime tests, strict Clippy, formatting check, and whitespace diff check.

## Self-Review

- `RuntimeEvent::TextDelta` still sends the untouched model delta before the phrase chunker receives it; only selected speech phrases are normalized.
- Streaming phrases and the final chunker remainder use the same normalizer. A segment index advances only after normalized speech is available.
- The normalizer removes only the requested heading/list markers, thematic breaks, fence-only lines, and balanced backtick/star delimiters. Unit coverage preserves literal `C#`, `#topic`, and `2*3`.
- Formatting-only output creates no `SpeechSegment`; the existing speech worker therefore emits neither speech lifecycle event nor synthesis timing.
- Scoped diff contains only Task 2 runtime files plus this report and the shared SDD ledger update.

## Concern

- The normalizer is intentionally not a general Markdown parser; unsupported Markdown syntax remains spoken literally.

## Fix Round 1/5

### Findings Addressed

- **Important — unmatched strong delimiters lose content:** addressed. An unmatched `**` run is preserved as two literal characters and the scanner continues after the run, so it cannot fall through to the single-star branch or pair against its own second star. ASCII and UTF-8 regressions cover unmatched `*`, `**`, and backtick delimiters.
- **Important — unsupported/bare hash runs are deleted:** addressed. The formatting-only shortcut now matches exactly `#`; `##` and `#######` remain spoken literally, while the supported one-to-six marker plus whitespace rule remains covered at six markers.

### TDD Evidence

Added the unmatched-delimiter and hash-run tests before changing the scanner.

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime speech_text::tests
```

RED result: failed as expected — ASCII and UTF-8 `**unfinished` cases lost the double-star delimiter, and `##` was skipped.

### Verification

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo fmt --all
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime speech_text::tests
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --locked -p conversation-runtime
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo clippy --locked -p conversation-runtime --all-targets -- -D warnings
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo fmt --all -- --check
git diff --check
```

Result: passed — 6 normalizer tests, 72 runtime tests, strict Clippy, formatting check, and whitespace diff check.

### Self-Review

- The unmatched `**` path appends both delimiter bytes and continues before the single-star branch, preserving the reviewed ASCII and UTF-8 cases.
- Unmatched single-star and backtick delimiters continue through the scanner unchanged; their tests assert the delimiter and enclosed text are retained.
- Only the exact bare `#` fixture is formatting-only. Unsupported runs remain unchanged unless the existing one-to-six-plus-whitespace heading predicate applies.
- Runtime queue integration, original `TextDelta` handling, segment indexing, and speech lifecycle behavior are unchanged by this repair.

## Final Review Fix

The whole-branch review identified two remaining delimiter cases. Tests were
added first and failed against the reviewed implementation:

- literal ASCII and multilingual `*` pairs separated from content by
  whitespace were deleted;
- inline-code delimiters nested inside emphasis remained in speech input.

The scanner now requires non-whitespace content after an emphasis opener,
matches exact one- or two-asterisk runs, recursively normalizes supported
delimiters inside emphasis, and preserves inline-code content without
reinterpreting literal markers inside that code span. Numeric expressions such
as `2*3`, unmatched delimiters, unsupported three-asterisk and hash runs, and
the original emitted text remain unchanged.

The first re-review found that closing-emphasis search still entered paired
inline-code spans and that failed or unsupported star runs could be retried
from a shorter suffix. Added regressions failed for ``*say `2*3`*``,
`***nested**`, and `**unfinished*`. Closing search now skips paired backtick
spans, and the main scanner preserves a failed or unsupported star run
atomically before continuing.
