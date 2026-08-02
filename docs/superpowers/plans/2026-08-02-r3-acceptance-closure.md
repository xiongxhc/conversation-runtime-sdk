# R3 Acceptance Closure Implementation Plan

> **For agentic workers:** Use test-driven development and verification-before-completion. Repository tooling may become complete without claiming R3 complete until the human and calibrated acoustic evidence passes.

**Goal:** Prevent duration-only acceptance from passing, provide a privacy-safe calibrated acoustic analyzer, and record R3 status from actual evidence.

**Architecture:** The shell harness remains the process/device supervisor and counts content-free lifecycle markers. A small Rust binary validates bounded absolute-timestamp CSV recordings and calculates deterministic latency percentiles. Documentation separates deterministic, device, and acoustic evidence.

**Tech Stack:** POSIX shell, Rust 2021, Serde JSON, existing macOS voice sidecar and CLI.

## Constraints

- Silence or process uptime alone never passes conversation acceptance.
- Acceptance output contains counts and timings, never transcripts or audio.
- Acoustic input uses absolute timestamps and explicit validity fields.
- Excluded samples remain visible by identifier and reason.
- The analyzer requires at least 30 valid samples and uses nearest-rank percentiles.
- R3 remains `ACCEPTANCE BLOCKED` until both the ten-minute device run and calibrated acoustic set pass.

### Task 1: Serialize Global Swift Network-Trap Tests

**Files:**
- Modify: `platform/macos/voice-sidecar/Tests/ConversationVoiceSidecarTests/RecognitionMappingTests.swift`

- [ ] Add a serialized suite around the two tests that share `NetworkTrapURLProtocol` state.
- [ ] Run the focused Swift suite and confirm both tests pass together.

### Task 2: Require Observed Conversation Activity

**Files:**
- Modify: `tests/voice/acceptance-macos.sh`
- Modify: `tests/voice/acceptance-macos.test.sh`

- [ ] Add failing shell tests for a silent duration-only run and unmet completed-turn or interruption thresholds.
- [ ] Add `--minimum-completed-turns` and `--minimum-interruptions` parsing with bounded non-negative integers.
- [ ] Count completed, cancelled, and failed turns from content-free lifecycle output.
- [ ] Include declared thresholds at session start and observed counts in the final summary.
- [ ] Fail acceptance when either declared minimum is unmet.
- [ ] Run the adversarial harness test suite.

### Task 3: Add the Acoustic Report Contract

**Files:**
- Create: `tests/voice/src/bin/conversation-acoustic-report.rs`
- Create: `tests/voice/tests/acoustic_report_cli.rs`
- Modify: `tests/voice/Cargo.toml`

- [ ] Add failing CLI tests for 30 valid samples, nearest-rank p95, exclusions, duplicate identifiers, malformed values, timestamp ordering, overflow, and content-free output.
- [ ] Parse a bounded CSV with the approved timestamp and validity columns.
- [ ] Compute audible-stop and speech-end-to-first-audible distributions using checked subtraction.
- [ ] Emit deterministic JSON with counts, p50, p95, maximum, threshold, and pass status.
- [ ] Reject fewer than 30 valid samples and fail the command when p95 audible stop exceeds 500 ms.
- [ ] Run the focused acoustic-report test target.

### Task 4: Make the Procedure Reproducible

**Files:**
- Modify: `tests/voice/acoustic/README.md`
- Modify: `docs/r3-real-time-voice-evaluation.md`
- Modify: `ROADMAP.md`

- [ ] Document the canonical ten-minute interaction profile and threshold flags.
- [ ] Document calibrated recording, timestamp annotation, CSV generation, exclusions, and analyzer invocation.
- [ ] Record deterministic verification separately from device and acoustic evidence.
- [ ] Leave R3 `ACCEPTANCE BLOCKED` unless current-run evidence actually passes.

### Task 5: Verify R3 Closure Tooling

- [ ] Run `tests/voice/acceptance-macos.test.sh`.
- [ ] Run `cargo test --locked -p conversation-voice-probe --test acoustic_report_cli`.
- [ ] Run the complete Rust workspace suite.
- [ ] Run the serialized Swift suite.
- [ ] Run the ten-minute and calibrated procedures if the required human/device setup is available; otherwise record the exact remaining gate.

