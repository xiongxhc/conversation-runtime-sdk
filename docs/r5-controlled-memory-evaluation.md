# R5 Controlled Memory Evaluation

## Status

`COMPLETE FOR THE DETERMINISTIC LOCAL CONTROL SURFACE`

R5 proves explicit storage, conservative promotion, bounded retrieval, runtime
injection, and operator controls without claiming automatic memory selection or
subjective long-term conversation quality.

## Implemented Boundary

- Portable record, approval, retention, retrieval-item, and trace types live in
  `conversation-protocol`.
- `conversation-memory` exposes backend-neutral store and async context-provider
  contracts plus a bundled-SQLite reference implementation.
- Database creation is explicit and accepts only an absolute path. Voice startup
  opens an existing supported database and never initializes one.
- The runtime retrieves after quality resolution and before language generation,
  publishes a reliable content-free trace, and fails closed at `Memory` when a
  configured provider fails.
- The Ollama reference adapter sends retrieved items as a separate untrusted-data
  message before the current input. The no-memory request remains unchanged.
- `conversation-memory-probe` exposes manual initialization, lifecycle,
  approval, expiration, deletion, and retrieval commands.
- Schema-v2 voice configuration opts in only when one local SQLite descriptor
  and one matching `[memory_store]` are both present with a local language model.

## Control and Privacy Guarantees

The runtime never persists a transcript or generated response automatically.
Every durable record is created through an explicit store or probe operation.
Identity and relationship records begin as candidates. Approval binds a
confirmation identifier, actor, timestamp, expected revision, and SHA-256
content digest; later content edits require reapproval.

Retrieval defaults to four items and `4096` content bytes and cannot exceed
eight items or `8192` bytes. The deterministic scan is capped, oversized items
are skipped whole, pinned records still require relevance, and queries are not
stored. Traces contain identifiers, reasons, budgets, selected totals, and
exclusion counts but no query or memory content.

`LocalOnly` rejects remote memory and remote language execution before voice
capture. Configured store failures do not silently continue with empty memory.
Sensitive content remains excluded from runtime metrics.

## Deterministic Evidence

Focused suites cover:

- schema identity, absolute paths, owner-only new-file permissions, symbolic-link
  rejection, and modified or unsupported migrations;
- CRUD round trips, expected revisions, source and approval inspection, pin
  invariants, exact expiry, session expiry, and cascading hard deletion;
- confirmation-backed identity and relationship activation and reapproval after
  edits;
- multilingual lexical ranking, stable tie-breaks, scan/item/byte caps, future
  record exclusion, cancellation rollback, and query non-persistence;
- async blocking-provider cleanup and local execution descriptors;
- language-input aggregate limits and separate untrusted-memory serialization;
- retrieval before generation, reliable trace ordering, failure stages,
  interruption cleanup, deletion before later turns, and voice-session transfer;
- the full probe lifecycle and content-safe rejected-mutation output; and
- voice descriptor/store pairing, local-only enforcement, database and budget
  preflight before sidecar spawn, real SQLite retrieval, and content-free CLI
  trace rendering.

The milestone gate is:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
tests/voice/acceptance-macos.test.sh
VOICE_SIDECAR_FIXTURES_DIR="$PWD/tests/fixtures/voice-sidecar-v1" \
  xcrun swift test --package-path platform/macos/voice-sidecar \
  --parallel --num-workers 1
git diff --check
```

## Current Verification

The 2026-08-02 gate passed. The complete Rust workspace passed with one
intentionally ignored immutable-fixture writer, strict Clippy and formatting
passed, the deterministic acceptance-harness suite passed, and the macOS Swift
package passed all `109` tests. The repository-wide placeholder/private-path
scan found no new matches in the R5 diff. Remaining `abliterated` identifiers
are pre-existing historical benchmark evidence and are explicitly not SDK model
recommendations.

## Deletion Boundary

Hard deletion removes the memory row, provenance and approval rows, and stored
retrieval-item references. It does not overwrite SQLite pages, WAL remnants,
filesystem snapshots, backups, or storage-device history, so it is not a
cryptographic secure-erasure claim.

Deletion also cannot retract memory already copied into an in-flight provider
request. The operator must interrupt that turn before deleting when immediate
exclusion is required. Deterministic tests prove that a deleted record is absent
from later retrieval and language input.

## Excluded Claims

R5 does not provide a desktop editor, automatic transcript capture, embedding or
semantic-search quality, encrypted SQLite independent of platform disk
encryption, remote memory export consent, team memory, or iPhone/LAN access.
It does not complete R3 human, ten-minute, first-audible, or acoustic barge-in
acceptance. Those evidence classes remain separate.
