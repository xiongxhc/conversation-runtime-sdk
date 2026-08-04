# Task 4 Evidence Report — Gateway Configuration and Framing

## Scope

- Registered the private `conversation-runtime-gateway` workspace package.
- Added fail-closed local-only TOML configuration loading and adapter construction.
- Added bounded stdio frame reader and writer primitives.
- Added generic public gateway example configuration.

## TDD Evidence

### RED

After adding the configuration and framing integration tests and before registering the package, this required command was run:

```text
cargo test --locked -p conversation-runtime-gateway --test config --test framing
```

It failed as expected because the package did not yet exist:

```text
error: package ID specification `conversation-runtime-gateway` did not match any packages
```

### GREEN

After implementation and formatting, the required locked command passed:

```text
cargo test --locked -p conversation-runtime-gateway --test config --test framing
```

Results:

- `config`: 13 passed, 0 failed.
- `framing`: 9 passed, 0 failed.

## Validation

- `cargo fmt --all -- --check` passed.
- `cargo clippy --locked -p conversation-runtime-gateway --tests -- -D warnings` passed.
- `git diff --check` passed.

## Requirement Coverage

- Configuration requires an absolute file path, reads at most 64 KiB, is strict TOML/UTF-8, and rejects unknown fields.
- Local-only configuration accepts only numeric loopback plain-HTTP language endpoints without credentials, query, or fragment.
- The model is explicit and validated through `OllamaConfig`; persona and memory limits use their public constructors.
- Optional memory requires an absolute, existing SQLite database accepted by `SqliteMemoryStore::open`.
- Frames use a four-byte big-endian length, reject zero and values over 512 KiB before allocation, and distinguish clean EOF from truncation.
- Framing preserves raw payload bytes so client-wire decoding owns UTF-8 validation; writer flushes each complete frame.

## Concerns

- No concerns within Task 4 scope.
