# SDD ledger — plan: docs/superpowers/plans/2026-07-28-continuous-speech-seams.md

Baseline: cargo build passed; 217 workspace tests passed; branch fix/continuous-speech-seams at 6ef1abc.

Task 1: implemented preferred punctuation/newline boundary selection, added chunker and runtime coalescing regressions, and preserved the existing UTF-8 hard-limit assertions. Focused chunker tests and `turn_flow` pass; strict runtime Clippy passes. The full runtime command now has five cancellation-test timeouts that assume sub-soft phrases start speech immediately; those fixtures are outside Task 1 ownership and require explicit eligible phrase limits in a follow-up.

Preflight correction: at the soft limit, a prior soft boundary is retained only for the hard-limit fallback; it cannot be selected when a later ordinary character reaches the soft limit. The new selector-level regression proves a leading space is not selected in that case.

Task 1: fix round 1/5 — latest in-cap sentence/newline selection and stale report scope addressed by 50dd6eb; scoped re-review found no new Critical or Important breakage.
Task 1: complete (commits d398619..50dd6eb, review clean).

Task 2: fix round 1/5 — unmatched delimiter preservation and unsupported hash-run preservation addressed by 284b78f; scoped re-review found no new Critical or Important breakage.
Task 2: complete (commits 08c3c80..284b78f, review clean).

Task 2: normalized speech-only formatting after phrase selection and before speech segment indexing. Raw `TextDelta` values remain byte-for-byte original; literal `C#`, `#topic`, and `2*3` are preserved; formatting-only output skips speech lifecycle events. TDD red run, focused normalizer/runtime checks, 69 runtime tests, strict runtime Clippy, format check, and diff check pass. Report: `task-2-report.md`.

Task 1 follow-up: expanded ownership only to the five affected cancellation fixtures. Added `small_phrase_chunking_config()` returning `PhraseChunkingConfig::new(4, 192).unwrap()` and applied it only where active synthesis/output must begin before held-open generation. The five fixtures, full runtime suite, strict runtime Clippy, format check, and diff check pass.

Task 1 review round 1/5: fixed boundary selection to scan the buffered window through the hard cap before selecting the latest retained sentence/newline boundary. Added the reviewer-provided `PhraseChunkingConfig::new(5, 24)` sentence and newline regressions, updated the one-delta ordered-emission test for latest-boundary semantics, and aligned one cancellation fixture's expected coalesced speech text without changing its cleanup or terminal assertions. Focused tests, full runtime, strict Clippy, format, and diff checks pass.

Task 2 fix round 1/5: addressed both Important review findings. Unmatched ASCII and UTF-8 `*`, `**`, and backtick delimiters now retain their bytes and text; unmatched `**` cannot fall through to its second star. The formatting-only hash shortcut is exactly `#`, preserving `##` and `#######` while retaining the one-to-six-plus-whitespace heading rule. TDD red, 6 normalizer tests, 72 runtime tests, strict Clippy, format, and diff checks pass. Report evidence appended to `task-2-report.md`.

Task 3: added a capacity-one prepared-audio boundary that reserves before synthesis, overlaps exactly one synthesized segment with ordered playback, observes synthesis failure during active output without masking its stage, maps synthesis-task `JoinError` to a static synthesizer failure, and preserves lifecycle/timing publication plus panic cleanup. Two red/green concurrency regressions, the JoinError regression, 75 runtime tests, strict runtime Clippy, format check, and diff check pass. Report: `task-3-report.md`.

Task 3 review round 1/5: fixed the inner adapter priority so ready synthesis errors and contained panics win over simultaneous internal stop while external interruption and lifecycle closure remain first. Strengthened the overlap regression with a saturated-lifecycle gate proving `FirstPlayableAudio` publication blocks output invocation. Focused regressions, 77 runtime tests, strict runtime Clippy, format, and diff checks pass. Evidence appended to `task-3-report.md`.
