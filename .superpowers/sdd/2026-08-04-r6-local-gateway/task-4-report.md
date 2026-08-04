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

## Fix Round 1 — Local Proxy and Config-Path Hardening

### RED

The new focused gateway run failed before implementation because a FIFO blocked
configuration loading and a leaf symlink was followed:

```text
test rejects_a_fifo_configuration_before_opening_it ... FAILED
test rejects_a_leaf_configuration_symlink_before_reading_its_target ... FAILED
```

The new adapter regression failed to compile before implementation because the
required direct constructor was absent:

```text
no associated function or constant named `new_direct` found for struct `OllamaLanguageModel`
```

### Resolution

- Added public `OllamaLanguageModel::new_direct`, which uses
  `reqwest::ClientBuilder::no_proxy()` while preserving `new` unchanged for
  existing consumers.
- Gateway adapter construction uses only `new_direct`, so local-only prompts
  cannot use `HTTP_PROXY`, `http_proxy`, `ALL_PROXY`, or `all_proxy`.
- Gateway configuration now calls `symlink_metadata` before opening and accepts
  only a regular, non-symlink leaf; Unix regression tests cover a leaf symlink
  and FIFO.
- The missing-memory test now proves rejection does not create the absent path.

### GREEN

- `cargo test --locked -p conversation-runtime-gateway --test config --test framing -- --test-threads=1` passed: 24 tests.
- `cargo test --locked -p conversation-model-adapters --test ollama -- --test-threads=1` passed: 27 tests.
- `cargo fmt --all -- --check` passed.
- `cargo clippy --locked -p conversation-runtime-gateway --tests -- -D warnings` passed.
- `cargo clippy --locked -p conversation-model-adapters --test ollama -- -D warnings` passed.
- `git diff --check` passed.

### Environment Note

- The adapter suite requires loopback fixture binding and therefore ran outside
  the filesystem sandbox after its restricted run failed with `PermissionDenied`.

## Fix Round 2 — Atomic Configuration Descriptor Open

### RED

The focused gateway suite failed after the special-file regressions required the
stable descriptor-open error. The metadata-then-open implementation returned a
path-specific error instead:

```text
test rejects_a_fifo_configuration_before_opening_it ... FAILED
test rejects_a_leaf_configuration_symlink_before_reading_its_target ... FAILED
left: "gateway configuration path must be a regular file and not a symbolic link"
right: "gateway configuration file could not be opened"
```

### Resolution

- Added a direct Unix-only `libc` dependency for the required open flags.
- On Unix, configuration loading now opens once with read-only
  `O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC`, then validates the opened descriptor
  with `file.metadata().file_type().is_file()` before bounded reading.
- On non-Unix platforms, configuration loading fails closed until a secure
  Windows implementation exists; there is no racy fallback.
- Symlink and FIFO tests now assert the stable, content-free open error.

### GREEN

- `cargo test --locked -p conversation-runtime-gateway --test config --test framing -- --test-threads=1` passed: 24 tests.
- `cargo test --locked -p conversation-model-adapters --test ollama -- --test-threads=1` passed: 27 tests.
- `cargo fmt --all -- --check` passed.
- `cargo clippy --locked -p conversation-runtime-gateway --tests -- -D warnings` passed.
- `cargo clippy --locked -p conversation-model-adapters --test ollama -- -D warnings` passed.
- `git diff --check` passed.

### Deterministic Race Coverage

- No path-replacement race test was added: without a test-only scheduling hook,
  forcing the replacement between lookup and open requires timing or sleeps.
  The implementation removes that lookup-open gap by validating the descriptor
  returned from the single atomic open.
