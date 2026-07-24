# Initial Conversation Runtime Scaffold Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create a minimal Rust workspace that proves typed conversation events, replaceable model adapters, deterministic turn orchestration, and interruption semantics.

**Architecture:** Keep the public protocol independent, place model capabilities behind asynchronous adapter traits, and let the runtime coordinate one active turn through Tokio channels and a `CancellationToken`. Use deterministic mock adapters so the first test seam requires no microphone, model download, or model-specific runtime. `TurnEventStream` combines bounded nonterminal data with an independent terminal path so consumer backpressure cannot deadlock cancellation finalization.

**Tech Stack:** Rust 2021 edition, Tokio, Tokio Util, standard Rust tests, TOML configuration examples.

## Global Constraints

- Runtime and adapter contracts must remain cross-platform.
- The first validated hardware target is macOS on Apple Silicon.
- The initial scaffold must not require model downloads or audio hardware.
- A turn must emit exactly one terminal event: completed, cancelled, or failed.
- Interruption must cancel generation and synthesis rather than only suppress playback.
- Do not commit changes unless the user explicitly requests a commit.
- Use the Rust toolchain pinned by `rust-toolchain.toml`.

---

### Task 1: Workspace and Public Protocol

**Files:**
- Create: `.gitignore`
- Create: `Cargo.toml`
- Modify: `README.md`
- Create: `crates/protocol/Cargo.toml`
- Create: `crates/protocol/src/lib.rs`
- Create: `crates/protocol/src/ids.rs`
- Create: `crates/protocol/src/command.rs`
- Create: `crates/protocol/src/error.rs`
- Create: `crates/protocol/src/event.rs`

**Interfaces:**
- Consumes: no project code.
- Produces: `TurnId`, `RuntimeCommand`, `RuntimeStage`, `RuntimeErrorKind`, `RuntimeError`, and `RuntimeEvent`.

- [ ] **Step 1: Add a protocol terminal-state test**

```rust
#[test]
fn only_terminal_events_report_terminal_state() {
    let turn_id = TurnId::new(1);
    assert!(!RuntimeEvent::TurnStarted { turn_id }.is_terminal());
    assert!(RuntimeEvent::TurnCompleted { turn_id }.is_terminal());
    assert!(RuntimeEvent::TurnCancelled { turn_id }.is_terminal());
}
```

- [ ] **Step 2: Run the focused test and verify the missing API**

Run: `cargo test -p conversation-protocol only_terminal_events_report_terminal_state`

Expected before implementation: compilation fails because the protocol types do not exist.

- [ ] **Step 3: Implement the protocol types**

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TurnId(u64);

impl TurnId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeCommand {
    StartTurn { turn_id: TurnId, transcript: String },
    Interrupt { turn_id: TurnId },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeStage {
    Runtime,
    LanguageModel,
    SpeechSynthesizer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeErrorKind {
    Adapter,
    Configuration,
    InvalidState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeError {
    pub kind: RuntimeErrorKind,
    pub stage: RuntimeStage,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeEvent {
    TurnStarted { turn_id: TurnId },
    TranscriptFinal { turn_id: TurnId, text: String },
    TextDelta { turn_id: TurnId, delta: String },
    SpeechStarted { turn_id: TurnId },
    SpeechCompleted { turn_id: TurnId },
    TurnCompleted { turn_id: TurnId },
    TurnCancelled { turn_id: TurnId },
    TurnFailed { turn_id: TurnId, error: RuntimeError },
}

impl RuntimeEvent {
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::TurnCompleted { .. }
                | Self::TurnCancelled { .. }
                | Self::TurnFailed { .. }
        )
    }
}
```

- [ ] **Step 4: Run protocol tests**

Run: `cargo test -p conversation-protocol`

Expected after implementation: all protocol tests pass.

### Task 2: Replaceable Model Adapters

**Files:**
- Create: `crates/model-adapters/Cargo.toml`
- Create: `crates/model-adapters/src/lib.rs`
- Create: `crates/model-adapters/src/language_model.rs`
- Create: `crates/model-adapters/src/speech.rs`
- Create: `crates/model-adapters/src/mock.rs`

**Interfaces:**
- Consumes: `conversation_protocol::TurnId`.
- Produces: `LanguageModel`, `LanguageModelRequest`, `SpeechSynthesizer`, `SpeechRequest`, `AdapterError`, `AdapterFuture`, `MockLanguageModel`, and `MockSpeechSynthesizer`.

- [ ] **Step 1: Add deterministic mock-adapter tests**

```rust
#[tokio::test]
async fn mock_language_model_streams_configured_deltas() {
    let model = MockLanguageModel::new(["hello", " there"]);
    let mut stream = model.stream(
        LanguageModelRequest::new(TurnId::new(1), "hi"),
        CancellationToken::new(),
    );

    assert_eq!(stream.recv().await.unwrap().unwrap(), "hello");
    assert_eq!(stream.recv().await.unwrap().unwrap(), " there");
    assert!(stream.recv().await.is_none());
}
```

- [ ] **Step 2: Run the adapter test and verify the missing API**

Run: `cargo test -p conversation-model-adapters mock_language_model_streams_configured_deltas`

Expected before implementation: compilation fails because the adapter interfaces do not exist.

- [ ] **Step 3: Implement cancellation-aware adapter contracts**

```rust
pub type AdapterFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, AdapterError>> + Send + 'a>>;

pub trait LanguageModel: Send + Sync {
    fn stream(
        &self,
        request: LanguageModelRequest,
        cancellation: CancellationToken,
    ) -> mpsc::Receiver<Result<String, AdapterError>>;
}

pub trait SpeechSynthesizer: Send + Sync {
    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, Vec<u8>>;
}
```

The mock language model spawns one Tokio task, sends configured deltas through a bounded channel, and exits when cancellation resolves. The mock synthesizer returns configured bytes unless cancellation resolves first.

- [ ] **Step 4: Run adapter tests**

Run: `cargo test -p conversation-model-adapters`

Expected after implementation: all adapter tests pass.

### Task 3: Conversation Runtime and Cancellation

**Files:**
- Create: `crates/runtime/Cargo.toml`
- Create: `crates/runtime/src/lib.rs`
- Create: `crates/runtime/tests/turn_flow.rs`
- Create: `crates/runtime/tests/cancellation.rs`

**Interfaces:**
- Consumes: `LanguageModel`, `SpeechSynthesizer`, `TurnId`, `RuntimeEvent`, and Tokio `CancellationToken`.
- Produces: `ConversationRuntime::new`, `ConversationRuntime::execute`, and `RuntimeCommandResult`.

- [ ] **Step 1: Add the successful-turn integration test**

```rust
#[tokio::test]
async fn emits_an_ordered_completed_turn() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::new(["hello", " there"])),
        Arc::new(MockSpeechSynthesizer::new([1, 2, 3])),
    );
    let turn_id = TurnId::new(1);
    let mut events = match runtime
        .execute(RuntimeCommand::StartTurn {
            turn_id,
            transcript: "hi".into(),
        })
        .await
        .unwrap()
    {
        RuntimeCommandResult::TurnStarted { events } => events,
        _ => panic!("start command must return a turn event stream"),
    };
    let mut observed = Vec::new();

    while let Some(event) = events.recv().await {
        observed.push(event);
    }

    assert_eq!(
        observed,
        vec![
            RuntimeEvent::TurnStarted { turn_id },
            RuntimeEvent::TranscriptFinal {
                turn_id,
                text: "hi".into(),
            },
            RuntimeEvent::TextDelta {
                turn_id,
                delta: "hello".into(),
            },
            RuntimeEvent::TextDelta {
                turn_id,
                delta: " there".into(),
            },
            RuntimeEvent::SpeechStarted { turn_id },
            RuntimeEvent::SpeechCompleted { turn_id },
            RuntimeEvent::TurnCompleted { turn_id },
        ]
    );
    assert_eq!(
        observed.iter().filter(|event| event.is_terminal()).count(),
        1
    );
}
```

- [ ] **Step 2: Add the interruption integration test**

```rust
#[tokio::test]
async fn interruption_emits_one_cancelled_terminal_event() {
    let runtime = ConversationRuntime::new(
        Arc::new(MockLanguageModel::delayed(["late"], Duration::from_secs(5))),
        Arc::new(MockSpeechSynthesizer::new([1])),
    );
    let turn_id = TurnId::new(7);
    let mut events = match runtime
        .execute(RuntimeCommand::StartTurn {
            turn_id,
            transcript: "stop".into(),
        })
        .await
        .unwrap()
    {
        RuntimeCommandResult::TurnStarted { events } => events,
        _ => panic!("start command must return a turn event stream"),
    };

    assert!(matches!(
        events.recv().await,
        Some(RuntimeEvent::TurnStarted { .. })
    ));
    assert!(matches!(
        runtime
            .execute(RuntimeCommand::Interrupt { turn_id })
            .await
            .unwrap(),
        RuntimeCommandResult::InterruptAccepted
    ));

    let mut terminal_events = Vec::new();
    while let Some(event) = events.recv().await {
        if event.is_terminal() {
            terminal_events.push(event);
        }
    }

    assert_eq!(
        terminal_events,
        vec![RuntimeEvent::TurnCancelled { turn_id }]
    );
}
```

- [ ] **Step 3: Run runtime tests and verify the missing API**

Run: `cargo test -p conversation-runtime`

Expected before implementation: compilation fails because `ConversationRuntime` does not exist.

- [ ] **Step 4: Implement single-active-turn orchestration**

`execute` accepts typed start and interrupt commands. A start command creates `TurnEventStream`, stores the active turn's cancellation token, and spawns the worker. The stream uses bounded nonterminal data plus an independent terminal channel. The worker emits start and transcript events, forwards language-model deltas, synthesizes the accumulated response, and emits exactly one terminal event. Every awaited adapter stage is paired with `cancellation.cancelled()` in `tokio::select!`. Terminal selection, publication, and active-turn removal are serialized so an accepted interruption cannot race with successful completion.

- [ ] **Step 5: Run runtime and workspace tests**

Run: `cargo test -p conversation-runtime`

Expected after implementation: successful-turn and interruption tests pass.

Run: `cargo test --workspace`

Expected after implementation: all workspace tests pass.

### Task 4: Configuration, Documentation, and Roadmap

**Files:**
- Create: `ROADMAP.md`
- Create: `apps/desktop/README.md`
- Create: `configs/persona.example.toml`
- Create: `configs/runtime.example.toml`
- Create: `docs/architecture.md`
- Create: `docs/model-benchmarks.md`
- Create: `models/README.md`
- Create: `models/registry.example.toml`
- Create: `tests/latency/README.md`

**Interfaces:**
- Consumes: the approved design and implemented public types.
- Produces: setup guidance, architectural boundaries, safe configuration examples, benchmark schema, and outcome-based milestones.

- [ ] **Step 1: Document the implemented boundaries**

`docs/architecture.md` must state the dependency direction `protocol <- model-adapters <- runtime`, the one-active-turn rule, event ordering, exactly-one-terminal-event invariant, and the reason the desktop shell is deferred.

- [ ] **Step 2: Add safe example configuration**

The examples contain product budgets and persona dimensions only. They contain no local paths, credentials, model weights, or claims that the latency target has been reached.

- [ ] **Step 3: Add the benchmark template**

The model matrix records hardware, model identifier, source, license, quantization, memory use, real-time factor, first-token latency, first-audio latency, and result. Its initial status explicitly says no benchmark has been executed.

- [ ] **Step 4: Write outcome-based milestones**

`ROADMAP.md` defines measurable exit criteria for toolchain and feasibility setup, deterministic runtime contracts, the local voice loop, barge-in, response control, controlled memory, SDK extraction, and later cross-platform expansion.

- [ ] **Step 5: Run documentation and repository checks**

Run: `rg -n "TBD|TODO|implement later|fill in" README.md ROADMAP.md apps configs docs models tests`

Expected: no unresolved placeholders.

Run: `git diff --check`

Expected: no whitespace errors.

Run: `cargo fmt --all -- --check`

Expected after installing the Rust toolchain: formatting check passes.

Run: `cargo test --workspace`

Expected after installing the Rust toolchain and resolving dependencies: all tests pass.
