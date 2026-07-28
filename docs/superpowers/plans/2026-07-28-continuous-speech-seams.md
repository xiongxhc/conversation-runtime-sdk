# Continuous Speech Seams Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove punctuation-driven multi-second pauses and reduce neural-voice resets while preserving incremental speech, bounded work, cancellation, and the original text event stream.

**Architecture:** The runtime will buffer short sentence boundaries until the existing soft byte limit, normalize only the speech copy of each selected phrase, and split speech handling into a synthesis producer plus ordered output consumer. A one-item validated-audio channel permits synthesis of segment `N + 1` during playback of segment `N` without allowing unbounded audio or backend work.

**Tech Stack:** Rust 1.97.1, Tokio tasks and bounded channels, existing `SpeechSynthesizer` and `AudioOutput` traits, deterministic runtime mocks, local Ollama and loopback MLX-Audio for the final machine-specific check.

## Global Constraints

- `RuntimeEvent::TextDelta` must preserve the model's original text and formatting.
- Default phrase limits remain `soft_limit_bytes = 96` and `hard_limit_bytes = 192`.
- The hard byte limit remains authoritative and UTF-8 safe.
- The synthesized-audio prefetch capacity is exactly one.
- Output order remains ascending `segment_index`; prefetched audio from a cancelled turn never plays.
- External interruption, lifecycle receiver closure, synthesis failure, and output failure await cleanup of both speech stages before terminal resolution.
- Public protocol types remain backend-neutral; no model, provider, voice, Markdown, or playback-process type enters `protocol`.
- Markdown normalization is speech-only and preserves literal `C#`, `#topic`, and `2*3`.
- Exact model identifiers may appear only in clearly labeled reproducible evidence, not defaults or recommendations.
- Do not infer first audible sound from playback-process launch.

---

## File Map

- `crates/runtime/src/phrase_chunker.rs`: select bounded phrase boundaries without immediately flushing every punctuation mark or newline.
- `crates/runtime/src/speech_text.rs`: private, pure speech-only Markdown normalization.
- `crates/runtime/src/lib.rs`: preserve original text events, normalize selected speech phrases, and enqueue only non-empty contiguous speech segments.
- `crates/runtime/src/speech_worker.rs`: bounded synthesis producer, ordered output consumer, timing, cancellation, failure propagation, and cleanup.
- `crates/runtime/tests/turn_flow.rs`: phrase coalescing, text preservation, normalization, prefetch overlap, order, and capacity regressions.
- `crates/runtime/tests/cancellation.rs`: interruption and cross-stage failure cleanup with active and prefetched work.
- `tests/voice/tests/probe_cli.rs`: integrated formatting regression without real models or speakers.
- `docs/runtime-text-to-audio-evaluation.md`: one labeled local continuity measurement and its limits.
- `README.md`: explain speech-only formatting normalization and bounded prefetch behavior.
- `ROADMAP.md`: record the verified R2 continuity correction without changing R3 scope.

---

### Task 1: Coalesce Short Sentence Boundaries

**Files:**
- Modify: `crates/runtime/src/phrase_chunker.rs`
- Modify: `crates/runtime/tests/turn_flow.rs`

**Interfaces:**
- Consumes: `PhraseChunkingConfig { soft_limit_bytes, hard_limit_bytes }`.
- Produces: unchanged `PhraseChunker::push_delta(&mut self, &str) -> Vec<String>` and `PhraseChunker::finish(self) -> Option<String>` with new boundary-selection semantics.

- [ ] **Step 1: Write failing pure chunker tests**

Add tests that make punctuation and newlines preferred boundaries rather than unconditional boundaries:

```rust
#[test]
fn short_sentence_boundaries_wait_for_more_context() {
    let mut chunker = PhraseChunker::default();

    assert!(chunker.push_delta("你好。").is_empty());
    assert!(chunker.push_delta("今天很好！").is_empty());
    assert_eq!(
        chunker.finish().as_deref(),
        Some("你好。今天很好！")
    );
}

#[test]
fn latest_sentence_boundary_is_used_after_the_soft_limit() {
    let config = PhraseChunkingConfig::new(12, 24).unwrap();
    let mut chunker = PhraseChunker::new(config);

    assert_eq!(
        chunker.push_delta("甲。乙乙乙。丙丙"),
        vec!["甲。乙乙乙。"]
    );
    assert_eq!(chunker.finish().as_deref(), Some("丙丙"));
}

#[test]
fn short_newlines_are_buffered_but_consumed_at_finish() {
    let mut chunker = PhraseChunker::default();

    assert!(chunker.push_delta("# 标题\n第一行\n第二行").is_empty());
    assert_eq!(
        chunker.finish().as_deref(),
        Some("# 标题\n第一行\n第二行")
    );
}
```

Update `one_delta_can_flush_multiple_phrases` so its input exceeds the configured soft limit and still proves multiple ordered emissions. Keep every existing UTF-8 hard-limit assertion.

- [ ] **Step 2: Run the pure tests and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime phrase_chunker::tests
```

Expected: the three new tests fail because punctuation and newlines currently emit immediately.

- [ ] **Step 3: Implement preferred-boundary selection**

Change `next_segment_end` to scan the buffered text while retaining the latest usable sentence/newline boundary and latest soft boundary:

```rust
fn next_segment_end(&self) -> Option<usize> {
    let mut preferred_end = None;
    let mut soft_end = None;

    for (index, character) in self.buffer.char_indices() {
        let end = index + character.len_utf8();
        if end > self.config.hard_limit_bytes {
            return preferred_end
                .or(soft_end)
                .or_else(|| Some(self.hard_split_end()));
        }

        if character == '\n' || Self::is_sentence_boundary(character) {
            preferred_end = Some(end);
        }
        if Self::is_soft_boundary(character) {
            soft_end = Some(end);
        }

        if end >= self.config.soft_limit_bytes {
            if let Some(end) = preferred_end.or(soft_end) {
                return Some(end);
            }
        }
        if end >= self.config.hard_limit_bytes {
            return Some(self.hard_split_end());
        }
    }

    None
}
```

If the exact implementation differs, it must preserve these invariants:

- never return a byte index above `hard_limit_bytes`;
- prefer the latest sentence/newline boundary seen within the hard limit;
- use a soft whitespace/punctuation boundary only when no preferred boundary exists;
- return `None` below the soft limit;
- leave `finish` responsible for the final remainder.

- [ ] **Step 4: Add the runtime-level coalescing regression**

Add to `crates/runtime/tests/turn_flow.rs`:

```rust
#[tokio::test]
async fn short_sentences_are_one_speech_request_without_changing_text_deltas() {
    let original = "# 问候\n你好。今天很好！*保持自然*";
    let (speech_calls, mut synthesized_text) = mpsc::unbounded_channel();
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new([original])),
        Arc::new(RecordingSpeechSynthesizer {
            audio: SynthesizedAudio::new(minimal_aiff(), AudioFormat::Aiff),
            calls: speech_calls,
        }),
        Arc::new(DiscardAudioOutput),
    );
    let turn_id = TurnId::new(60);
    let mut events = start_turn(&runtime, turn_id, "coalesce").await;

    let observed = drain_events(&mut events).await;
    let deltas = observed
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect::<String>();

    assert_eq!(deltas, original);
    assert_eq!(synthesized_text.recv().await.as_deref(), Some(original));
    assert!(synthesized_text.try_recv().is_err());
}
```

This test will be updated in Task 2 to expect normalized speech text while retaining the original `TextDelta`.

- [ ] **Step 5: Run focused and full runtime tests**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --locked -p conversation-runtime --all-targets -- -D warnings
```

Expected: all runtime tests and strict runtime Clippy pass.

- [ ] **Step 6: Commit**

```bash
git add crates/runtime/src/phrase_chunker.rs crates/runtime/tests/turn_flow.rs
git commit -m "fix: coalesce short speech phrases"
```

---

### Task 2: Normalize Speech-Only Formatting

**Files:**
- Create: `crates/runtime/src/speech_text.rs`
- Modify: `crates/runtime/src/lib.rs`
- Modify: `crates/runtime/tests/turn_flow.rs`

**Interfaces:**
- Consumes: one selected phrase as UTF-8 text.
- Produces: `pub(super) fn normalize_speech_text(input: &str) -> Option<String>`.

- [ ] **Step 1: Add failing normalizer tests**

Create `speech_text.rs` with tests first:

```rust
#[cfg(test)]
mod tests {
    use super::normalize_speech_text;

    #[test]
    fn removes_supported_markdown_markers_and_collapses_layout() {
        assert_eq!(
            normalize_speech_text(
                "# 标题\n* 第一项\n这是**重点**，也是`代码`。\n```\n示例\n```"
            )
            .as_deref(),
            Some("标题 第一项 这是重点，也是代码。 示例")
        );
    }

    #[test]
    fn preserves_literal_hash_star_and_hashtag_content() {
        assert_eq!(
            normalize_speech_text("C#、#topic 和 2*3 保持原样。").as_deref(),
            Some("C#、#topic 和 2*3 保持原样。")
        );
    }

    #[test]
    fn formatting_only_input_is_skipped() {
        assert_eq!(normalize_speech_text("#\n***\n```"), None);
    }
}
```

- [ ] **Step 2: Run tests and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime speech_text::tests
```

Expected: compilation fails because `normalize_speech_text` is not implemented and the module is not registered.

- [ ] **Step 3: Implement the private normalizer**

Register `mod speech_text;` in `lib.rs`. Implement a small line-aware scanner, not a general Markdown parser:

```rust
pub(super) fn normalize_speech_text(input: &str) -> Option<String> {
    let mut normalized = Vec::new();

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") && trimmed.trim_matches('`').is_empty() {
            continue;
        }

        let line = strip_line_prefixes(trimmed);
        let line = strip_paired_delimiters(line);
        if !line.trim().is_empty() {
            normalized.push(line.trim().to_owned());
        }
    }

    let normalized = normalized.join(" ");
    (!normalized.is_empty()).then_some(normalized)
}
```

`strip_line_prefixes` must remove:

- one-to-six leading `#` characters only when the run is followed by whitespace;
- one leading `*` only when followed by whitespace.
- formatting-only thematic-break lines composed of at least three `*`, `-`, or
  `_` characters.

`strip_paired_delimiters` must remove balanced `` `...` ``, `*...*`, and
`**...**` delimiters while retaining enclosed text. It must not remove the
characters from `C#`, `#topic`, or `2*3`.

- [ ] **Step 4: Normalize before assigning speech segment indices**

In `run_turn`, apply `normalize_speech_text` to every phrase before constructing
`SpeechSegment`. Increment `segment_index` only when normalized text is queued:

```rust
if let Some(text) = normalize_speech_text(&phrase) {
    let segment = SpeechSegment {
        index: segment_index,
        text,
    };
    segment_index += 1;
    // existing cancellation-aware bounded send
}
```

Use the same path for phrases emitted during streaming and the final remainder.
Formatting-only phrases produce no speech lifecycle event when no other spoken
text exists.

- [ ] **Step 5: Update runtime integration assertions**

Update the Task 1 runtime test:

```rust
assert_eq!(deltas, original);
assert_eq!(
    synthesized_text.recv().await.as_deref(),
    Some("问候 你好。今天很好！保持自然")
);
assert!(synthesized_text.try_recv().is_err());
```

Add a formatting-only turn case that still emits original `TextDelta`, skips
`SpeechStarted`, and completes exactly once.

- [ ] **Step 6: Run focused and full runtime gates**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime speech_text
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --locked -p conversation-runtime --all-targets -- -D warnings
```

Expected: all tests and strict Clippy pass.

- [ ] **Step 7: Commit**

```bash
git add crates/runtime/src/speech_text.rs crates/runtime/src/lib.rs crates/runtime/tests/turn_flow.rs
git commit -m "fix: normalize speech-only formatting"
```

---

### Task 3: Add One-Segment Synthesized-Audio Prefetch

**Files:**
- Modify: `crates/runtime/src/speech_worker.rs`
- Modify: `crates/runtime/tests/turn_flow.rs`

**Interfaces:**
- Consumes: ordered `mpsc::Receiver<SpeechSegment>`.
- Produces: private `PreparedAudio { index: u64, audio: SynthesizedAudio }` over `mpsc::channel(1)`.
- Preserves: `SpeechWorker::run(self) -> SpeechWorkerOutcome`.

- [ ] **Step 1: Write a failing overlap test**

Add controlled test adapters to `turn_flow.rs`:

```rust
struct PrefetchRecordingSpeech {
    started: mpsc::UnboundedSender<String>,
}

struct GatedRecordingOutput {
    started: mpsc::UnboundedSender<u64>,
    first_release: Mutex<Option<oneshot::Receiver<()>>>,
    played: Arc<Mutex<Vec<u64>>>,
}
```

`PrefetchRecordingSpeech::synthesize` sends `request.text()` immediately and
returns valid minimal AIFF. `GatedRecordingOutput::play` records the segment;
segment `0` waits on `first_release`, and later segments complete immediately.

Add:

```rust
#[tokio::test]
async fn synthesizes_one_segment_ahead_while_current_audio_is_playing() {
    // Use phrase limits that deterministically create segments 0, 1, and 2.
    // Start the turn and wait until output segment 0 is blocked.
    // Assert synthesis requests for segments 0 and 1 have started.
    // Assert segment 2 has not started because prepared-audio capacity is one.
    // Release output 0, drain the turn, and assert played order [0, 1, 2].
}
```

The exact text must be:

```rust
"First segment. Second segment. Third segment."
```

Configure `PhraseChunkingConfig::new(14, 20)` so the test does not depend on
defaults.

- [ ] **Step 2: Run the overlap test and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime \
  synthesizes_one_segment_ahead_while_current_audio_is_playing -- --nocapture
```

Expected: timeout or assertion failure because the current worker does not start
segment `1` synthesis until output `0` completes.

- [ ] **Step 3: Introduce the private prepared-audio boundary**

In `speech_worker.rs`, add:

```rust
const PREPARED_AUDIO_CAPACITY: usize = 1;

struct PreparedAudio {
    index: u64,
    audio: SynthesizedAudio,
}

enum SynthesisStageOutcome {
    Completed { synthesized_any: bool },
    Interrupted,
    Stopped,
    EventStreamClosed,
    Failed(AdapterError),
}
```

Move synthesis, typed-audio validation, `SpeechStarted`,
`FirstSynthesisRequest`, and `FirstPlayableAudio` into a private synthesis
stage. Before starting each synthesis request, the producer must reserve one
slot from the prepared-audio sender with `reserve()`. It sends `PreparedAudio`
through that permit only after validation. Reserving before synthesis ensures
that channel capacity one means exactly one segment can be synthesized ahead;
the producer cannot synthesize segment `N + 2` while `N + 1` occupies the
prefetch slot.

- [ ] **Step 4: Make `SpeechWorker::run` the ordered output consumer**

`SpeechWorker::run` must:

1. create `mpsc::channel(PREPARED_AUDIO_CAPACITY)`;
2. spawn the synthesis stage with cloned dependencies and cancellation tokens;
3. receive `PreparedAudio` in order;
4. call `AudioOutput::play` sequentially;
5. while output is active, also observe early synthesis-stage completion so a
   synthesis failure cancels and awaits active playback instead of waiting for
   playback to finish naturally;
6. on output failure or panic, cancel `work_cancellation`, close the prepared
   receiver, and await the synthesis stage before returning;
7. after prepared audio closes, await the synthesis stage outcome;
8. emit `SpeechCompleted` only when synthesis completed successfully and at
   least one segment was synthesized.

Keep `wait_for_adapter` or split it into stage-specific helpers only if each
helper preserves panic containment, cancellation priority, receiver-closure
handling, and cleanup-before-return.

- [ ] **Step 5: Verify timing and capacity behavior**

Extend the overlap test to assert:

- exactly one `FirstSynthesisRequest`;
- exactly one `FirstPlayableAudio`;
- first playable precedes output request `0`;
- output indices are `[0, 1, 2]`;
- segment `2` synthesis begins only after output `0` releases capacity.

Keep existing atomic pair tests for `SpeechStarted` and
`FirstSynthesisRequest`. If their helper moves into the synthesis stage, update
their construction without weakening the saturated-channel assertions.

- [ ] **Step 6: Run runtime gates**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime \
  synthesizes_one_segment_ahead_while_current_audio_is_playing
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --locked -p conversation-runtime --all-targets -- -D warnings
```

Expected: overlap regression, all runtime tests, and strict Clippy pass.

- [ ] **Step 7: Commit**

```bash
git add crates/runtime/src/speech_worker.rs crates/runtime/tests/turn_flow.rs
git commit -m "fix: prefetch synthesized speech audio"
```

---

### Task 4: Harden Cross-Stage Cancellation and Failures

**Files:**
- Modify: `crates/runtime/tests/cancellation.rs`
- Modify: `crates/runtime/src/speech_worker.rs`

**Interfaces:**
- Consumes: the Task 3 synthesis/output two-stage pipeline.
- Produces: cleanup-before-terminal guarantees across active playback,
  prefetched audio, active synthesis, output failure, and synthesis failure.

- [ ] **Step 1: Add a failing interruption regression**

Add controlled adapters:

```rust
struct TwoStageCleanupSpeech {
    started: mpsc::UnboundedSender<String>,
    second_cleanup: Arc<AtomicBool>,
}

struct ActiveCleanupOutput {
    started: mpsc::UnboundedSender<u64>,
    cleanup: Arc<AtomicBool>,
}
```

The first synthesis returns valid audio. The second synthesis waits for
cancellation, marks `second_cleanup`, and returns cancellation. Output waits for
cancellation, marks `cleanup`, and returns cancellation.

Test:

```rust
#[tokio::test]
async fn interruption_cleans_active_output_and_prefetched_synthesis_before_terminal() {
    // Start a turn with at least two deterministic segments.
    // Wait until output 0 and synthesis 1 are both active.
    // Interrupt the turn.
    // Require InterruptAccepted.
    // Drain exactly one TurnCancelled.
    // Assert both cleanup flags before accepting the terminal.
    // Start and complete a second turn to prove reuse.
}
```

- [ ] **Step 2: Add failing synthesis/output failure regressions**

Add two tests:

```rust
#[tokio::test]
async fn second_synthesis_failure_cancels_active_output_before_turn_failed() {
    // Output 0 is active when synthesis 1 fails.
    // Expect RuntimeStage::SpeechSynthesizer.
    // Assert output cleanup completed before the terminal.
}

#[tokio::test]
async fn first_output_failure_cancels_active_next_synthesis_before_turn_failed() {
    // Synthesis 1 is active when output 0 fails.
    // Expect RuntimeStage::AudioOutput.
    // Assert synthesis cleanup completed before the terminal.
}
```

- [ ] **Step 3: Run the new tests and verify RED**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime \
  interruption_cleans_active_output_and_prefetched_synthesis_before_terminal
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime \
  second_synthesis_failure_cancels_active_output_before_turn_failed
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime \
  first_output_failure_cancels_active_next_synthesis_before_turn_failed
```

Expected: at least one test fails until Task 3 cleanup propagation is complete.

- [ ] **Step 4: Complete failure arbitration**

In `speech_worker.rs`, enforce:

- external interruption wins over adapter results that become ready
  simultaneously;
- synthesis failure cancels `work_cancellation`, awaits active output, and
  returns `RuntimeStage::SpeechSynthesizer`;
- output failure cancels `work_cancellation`, awaits the synthesis task, and
  returns `RuntimeStage::AudioOutput`;
- lifecycle receiver closure cancels and awaits both stages;
- queued `PreparedAudio` is dropped after cancellation and never played;
- adapter panic messages remain static and stage-specific.

Do not detach a Tokio task or return while an owned adapter future is still
running.

- [ ] **Step 5: Run cancellation races repeatedly**

Run:

```bash
for iteration in 1 2 3 4 5; do
  PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
    cargo test --locked -p conversation-runtime --test cancellation --quiet
done
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-runtime
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --locked -p conversation-runtime --all-targets -- -D warnings
```

Expected: every repetition, the full runtime suite, and strict Clippy pass.

- [ ] **Step 6: Commit**

```bash
git add crates/runtime/src/speech_worker.rs crates/runtime/tests/cancellation.rs
git commit -m "fix: clean up prefetched speech work"
```

---

### Task 5: Verify the Integrated Voice Path and Document Evidence

**Files:**
- Modify: `tests/voice/tests/probe_cli.rs`
- Modify: `README.md`
- Modify: `ROADMAP.md`
- Modify: `docs/runtime-text-to-audio-evaluation.md`

**Interfaces:**
- Consumes: completed coalescing, normalization, prefetch, and cleanup behavior.
- Produces: deterministic CLI evidence plus one labeled Apple Silicon listening
  check.

- [ ] **Step 1: Add an integrated CLI formatting regression**

Extend the loopback fixture to return this exact model output:

```text
# 问候
你好。今天很好！*保持自然*，C# 和 2*3 不变。
```

Capture the local speech fixture's request bodies and assert:

```rust
assert_eq!(speech_requests.len(), 1);
assert_eq!(
    speech_requests[0]["input"],
    "问候 你好。今天很好！保持自然，C# 和 2*3 不变。"
);
assert_eq!(
    String::from_utf8(output.stdout).unwrap(),
    "# 问候\n你好。今天很好！*保持自然*，C# 和 2*3 不变。"
);
```

The fake player must be launched once, temporary audio must be removed, and
structured stderr must still contain one copy of each timing milestone followed
by `status=completed`.

- [ ] **Step 2: Run the CLI regression and full deterministic gates**

Run:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo test --locked -p conversation-voice-probe --test probe_cli
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo fmt --all -- --check
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --workspace --all-targets --locked -- -D warnings
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --workspace --locked
git diff --check
```

Expected: formatting, strict Clippy, every workspace test, and diff check pass.

- [ ] **Step 3: Run one fixed local Apple Silicon listening check**

Use the existing private configuration outside the repository. Start the
configured speech service on loopback in a persistent terminal, build once, and
invoke the binary directly:

```bash
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo build --locked -p conversation-voice-probe
target/debug/conversation-voice-probe \
  --config "${XDG_CONFIG_HOME:-$HOME/.config}/conversation-runtime/voice.toml" \
  "请原样输出：第一句。第二句，继续自然地说！*不要读星号* # 不要读井号"
```

Record:

- exact commit and binary SHA-256;
- loaded/cold state;
- observed generated text;
- speech POST request count;
- first text, first synthesis, first playable, total completion;
- playback-process launch count;
- process and temporary-file cleanup;
- whether punctuation still creates multi-second silent gaps;
- whether voice continuity is materially improved.

Listening observations are subjective evidence. Do not call playback launch
first audible and do not claim that independent synthesis requests guarantee an
identical voice.

- [ ] **Step 4: Update public documentation**

Update:

- `README.md`: explain that short punctuation-separated clauses are coalesced,
  supported Markdown markers are removed only from speech input, and one
  synthesized segment may be prefetched.
- `ROADMAP.md`: add the verified continuity correction to R2 current state;
  leave microphone, ASR, VAD, and barge-in in R3.
- `docs/runtime-text-to-audio-evaluation.md`: add deterministic results and the
  exact labeled local check with request/playback counts and evidence limits.

- [ ] **Step 5: Run final scans and gates**

Run:

```bash
if grep -RInE '\b(TB[D]|TO[D]O|PLACEHOLD[E]R)\b' \
  README.md ROADMAP.md docs/*.md configs tests/voice; then exit 1; fi
if grep -RInE '/Users/[A-Za-z0-9._-]+' \
  README.md ROADMAP.md docs/*.md configs tests/voice; then exit 1; fi
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo fmt --all -- --check
PATH="/opt/homebrew/opt/rustup/bin:$PATH" \
  cargo clippy --workspace --all-targets --locked -- -D warnings
PATH="/opt/homebrew/opt/rustup/bin:$PATH" cargo test --workspace --locked
git diff --check
git status --short
```

Expected: no incomplete marker or concrete private path, all repository gates
pass, and only intended documentation/evidence changes remain.

- [ ] **Step 6: Commit**

```bash
git add tests/voice/tests/probe_cli.rs README.md ROADMAP.md \
  docs/runtime-text-to-audio-evaluation.md
git commit -m "test: verify continuous speech output"
```

---

## Final Review and Integration Gates

- Generate a whole-branch review package from the merge base with `master` to
  `HEAD`.
- Run a fresh final reviewer against the approved design, this plan, every task
  report, deferred findings, and the exact whole-branch diff.
- Resolve every Critical or Important finding and re-review the exact fix range.
- Run formatting, strict workspace Clippy, all workspace tests, `git diff
  --check`, incomplete-marker scan, private-path scan, process cleanup, speech
  server cleanup, and `git status` on the final reviewed commit.
- Push `fix/continuous-speech-seams` only after all gates and final review pass.
- Merge and push `master` only after explicit user authorization.
- After this correction lands, begin a separate R3 design for microphone
  capture, VAD, streaming ASR, transcript finalization, barge-in, and
  first-audible measurement.
