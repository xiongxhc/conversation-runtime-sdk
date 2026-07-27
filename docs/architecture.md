# Architecture

## Dependency Direction

```text
protocol <- model-adapters <- runtime
```

`protocol` defines client-visible commands, events, identifiers, and failures. It has no dependency on Tokio, model implementations, or runtime internals.

`model-adapters` defines the capabilities required from language and speech models. Its mock implementations are deterministic test doubles, not deployment backends.

`runtime` owns turn state, adapter coordination, event ordering, and cancellation. Clients should not depend on adapter implementation details.

## Initial Turn Flow

1. A client starts one turn with a completed transcript.
   The client sends `RuntimeCommand::StartTurn` through `ConversationRuntime::execute`.
2. The runtime emits `TurnStarted` and `TranscriptFinal`.
3. The language-model adapter streams text deltas.
4. The runtime forwards every delta and accumulates the final response.
5. The speech adapter synthesizes the response.
6. The runtime emits one terminal event.

ASR begins in the feasibility and voice-loop milestones. Starting the deterministic seam at a completed transcript isolates orchestration behavior from microphone and model availability.

## Runtime Invariants

- A runtime instance owns at most one active turn.
- Clients assign strictly increasing turn identifiers per runtime instance.
- Every observed turn ends with exactly one of `TurnCompleted`, `TurnCancelled`, or `TurnFailed`.
- Interruption cancels the active token shared with downstream adapter work.
- Events from an interrupted turn retain their `TurnId` and cannot become events for a later turn.
- Adapter errors cross the runtime boundary with their failing stage intact.

## Cancellation

The runtime uses a cancellation token for the active turn and child tokens for adapter calls. Every long-running adapter stage participates in `tokio::select!` with cancellation. Cancelling a turn therefore stops generation and synthesis work instead of merely hiding its output.

`TurnEventStream` hides the transport implementation from SDK consumers. Nonterminal lifecycle data uses a bounded channel with cancellation-aware sends, while the terminal event uses an independent one-shot channel. An undrained client therefore applies bounded backpressure without preventing interruption from finalizing the turn.

Terminal selection, publication, and removal of the active turn are serialized by the active-turn lock: if interruption returns accepted, that turn cannot later complete successfully. Real high-rate partial transcripts or audio require explicit aggregation or a separate media transport; lifecycle finalization remains independent of consumer backpressure.

Real audio playback must adopt the same token and prove bounded cancellation latency before the barge-in milestone can pass.

## Relationship Behavior

Model relationships through context and conversation state rather than fixed scripts. Earned behavior is often more memorable than configurable behavior.

Affectionate expressions, special moments, and relationship signals must emerge from shared context, pacing, reciprocity, and rapport. They are not triggered by canned sequences, invisible unlock flags, frequency quotas, or a durable memory record that directly commands an expression. Persona and memory may shape the context available to the response controller, but the current conversational state remains authoritative.

## Public Repository Boundary

The public SDK defines portable contracts, reference adapters, reproducible evaluation methods, and clearly labeled historical measurements. It does not encode an application's models, voices, routing thresholds, personas, or deployment policy.

Exact checkpoint identifiers may appear only when required to reproduce benchmark evidence. They are measurements, not endorsements. Public examples use generic identifiers, while application configuration and deployment decisions remain outside this repository.

## Why the Desktop Shell Is Deferred

Creating the Tauri and React application before runtime contracts exist would couple the first protocol to desktop UI needs. The current boundary is documentation-only until deterministic turn and cancellation tests pass and feasibility benchmarks validate concrete reference adapters.
