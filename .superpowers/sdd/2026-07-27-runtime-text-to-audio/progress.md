# SDD ledger — plan: docs/superpowers/plans/2026-07-27-runtime-text-to-audio.md

Baseline: 140 tests passed; cargo build passed; branch feature/runtime-text-to-audio at 614e36e.

Task 1: complete
- Commit: 41bf34deaaae319874915a2db6d35c50412ec4b8
- Review: spec compliant; quality approved; no findings.
- Tests: model-adapters 63, runtime 19, latency 1; fmt and diff check passed.

Task 2: complete
- Commits: 576a159 (typed validation), 917e775 (TTS fixture compatibility).
- Review: shared validation approved; one Important fixture finding fixed and re-reviewed clean.
- Tests: adapters 65, runtime 19, latency 1, TTS 44, full workspace; fmt and diff check passed.

Task 3: complete
- Commits: dc5b75c (macOS output adapter), 5948e50 (bounded cleanup/static errors).
- Review: two Important process-boundary findings fixed and re-reviewed clean.
- Tests: macos_afplay 13, model-adapters 78; fmt and diff check passed.

Task 4: complete
- Commits: 9334b45 (phrase segmentation), aae8327 (UTF-8 limit validation), 4be0fd9 (warning-free prerequisite).
- Review: two Important findings fixed; scoped re-review approved.
- Task 4: minor (deferred): remove temporary PhraseChunker dead_code and run_turn too_many_arguments allowances when Task 6 consumes/refactors them.
- Tests: phrase 13, runtime 32; exact strict Clippy, fmt, diff check passed.

Task 5: complete
- Commit: 5ff8eac.
- Review: spec compliant; no findings.
- Tests: protocol 2; fmt and diff check passed.

Task 6: complete
- Commits: 175e822 (concurrent pipeline), dfb7fa0 (panic/closure/atomic timing hardening).
- Review: Critical panic containment and three Important race findings fixed; scoped re-review approved.
- Deferred Task 4 allowances removed.
- Tests: runtime 57, latency 2, adapters 78; cancellation race 10/10; strict Clippy, fmt, diff check passed.

Task 7: complete
- Commits: ad9a79b (integrated voice probe), 6764b34 (responsive interruption/output cleanup).
- Review: Critical stdout/SIGINT and two Important cleanup/race findings fixed; re-review merge-ready.
- Task 7: minor (deferred): replace queued-terminal test's fixed 50ms sleep with a condition.
- Task 7: minor (resolved): 6b0b76c disarms PlayerGuard after successful cleanup to avoid stale-PID SIGKILL risk.
- Tests: voice 12, repeated 60/60, workspace 211; strict Clippy, fmt, diff check passed.

Task 8: complete
- Baseline: strict formatting, Clippy, diff check, and 211 workspace tests passed at 6b0b76c.
- Apple Silicon evidence: one cold cached integrated run completed with separate first-text, first-synthesis, first-playable, playback-launch, total-completion, and cleanup observations.
- Documentation: owned README, roadmap, architecture, latency, and evaluation files updated.
- Commit: dc7c6e0.
- Final gates: scans empty; strict formatting and Clippy passed; 211 workspace tests passed; diff check passed.
- Report: `.superpowers/sdd/2026-07-27-runtime-text-to-audio/task-8-report.md`.
- Review package: `review-134419d..dc7c6e0.diff` (8,813 lines; SHA-256 `aa7b9cea558fd706d239fef96dd4b4f698aafce7dbf26254ce61f2f8ac1e03ba`).
- Final whole-branch review blocker: external review was rejected for sensitive egress; the local-only read-only reviewer did not finish within the bounded window and was terminated without findings. Reviewer process, model, and temporary output cleanup passed.
- Fix round 1/5: both Important timing-semantics findings addressed by 0b87f20; scoped spec review passed and quality approved with no new Critical or Important breakage.
- Task review: clean after fix round 1.

Task 8 fix round 1: complete
- Commit: 0b87f20.
- Review: 0 Critical, 2 Important, 0 Minor.
- Important 1 fixed: runtime-turn-origin milestones and process-launch-origin observations are separated; invalid cross-origin subtraction removed.
- Important 2 fixed: first playable is timestamped after typed-audio validation, before lifecycle publication/output handoff, and causally precedes the first output call.
- Checks: rejected-wording scan empty; incomplete-marker scan empty; private-path scan empty; cargo fmt and git diff checks passed.
- Deferred Task 7 queued-terminal fixed 50 ms sleep remains unchanged.

Final review fix wave: complete
- Starting head: 0b87f20.
- Findings closed: 2 Important and 4 Minor from `final-review.md`.
- Commits: 485fda4, 018e441, d9e1d93, a74bccd.
- Phrase hard cap: pure and runtime multibyte sentence, soft punctuation, and Unicode-whitespace regressions pass.
- Audio cleanup: every post-spawn result uses one finalizer; injected capture/wait failures and awaited stderr-task termination pass.
- Test quality: queued-terminal test uses an explicit terminal-queued hook; descendant-stderr test uses liveness rather than wall-clock timing.
- Configuration: the default temporary directory is validated as absolute.
- Benchmark evidence: supported neutral controls are preserved; unsupported private instructions and bounds are explicitly marked unavailable.
- Final gates: strict formatting, strict workspace Clippy, 217 workspace tests, doc tests, and diff check passed.
- Scans: public and final-wave incomplete-marker/private-path scans are empty; timing-assumption scan is empty.
- Cleanup: no owned probe/player/wrapper/runner process, no port-8000 listener, no loaded Ollama model, and no adapter-owned random temporary audio input remained.
- Report: `.superpowers/sdd/2026-07-27-runtime-text-to-audio/final-fix-report.md`.
