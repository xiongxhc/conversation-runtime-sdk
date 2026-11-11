# AGENTS.md — Conversation Runtime SDK

A local-first runtime for interruptible voice conversations: a Rust runtime and
gateway, a Swift macOS voice sidecar, a public TypeScript SDK, and reference
clients (Node example, Tauri desktop app). Models are replaceable adapters; the
runtime owns turns, cancellation, persona, memory and privacy.

Read before designing anything:

- `ROADMAP.md`: authoritative per-release status and open deliverables.
- `docs/architecture.md`: boundaries and the reasons behind them.
- `docs/HUMAN-CHECKLIST.md`: steps that need a person; never fabricate them.
- `docs/superpowers/specs/` and `docs/superpowers/plans/`: approved designs.

## Gates

Run all of these before calling a change done:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
npm ci
npm run build --workspaces
npm test --workspaces
xcrun swift test --package-path platform/macos/voice-sidecar   # when the sidecar changes
```

The diarization sidecar (`platform/diarization-sidecar`) has its own Python
tests; see `docs/HUMAN-CHECKLIST.md` for the virtual-environment setup.

## Already built: grep here before proposing it

| Capability | Where |
|---|---|
| Gateway host, framed stdio protocol, sessions | `apps/runtime-gateway/src/session.rs`, `framing.rs` |
| Strict gateway configuration and local/API toggle | `apps/runtime-gateway/src/config.rs`, `configs/gateway.toggle.example.toml` |
| Provider-host supervision (`[[provider_hosts]]`) | `apps/runtime-gateway/src/provider_supervisor.rs` |
| Media server, leases, admission | `apps/runtime-gateway/src/media_server.rs`, `docs/media-protocol-v2.md` |
| Encrypted timed recording and recovery | `crates/runtime/src/session_recording.rs`, `docs/recording-format-v3.md`, `docs/finalized-recording-recovery.md` |
| Recording reconciliation and alignment | `crates/runtime/src/recording_reconciliation.rs`, `recording_alignment.rs` |
| Structured text processing (`processText`) | `crates/runtime/src/text_processing.rs` |
| Ordered online transcription | `crates/model-adapters/src/openai_speech_recognition.rs`, `docs/online-transcription.md` |
| Anonymous after-the-fact diarization | `crates/model-adapters/src/diarization.rs`, `speechbrain_diarization.rs`, `openai_diarization.rs`, `platform/diarization-sidecar` |
| Speaker identity live inputs (sidecar turn audio, media-bridge audio, name claims) | `apps/runtime-gateway/src/speaker_audio.rs`, `speaker_identity.rs`, `crates/runtime/src/speaker_claim_detector.rs`, `docs/voice-sidecar-protocol.md` |
| Shared typed/spoken conversation context | `crates/runtime/src/conversation_context.rs` |
| Persona, modes, response limits, corrections | `crates/runtime/src/conversation_quality.rs` |
| Controlled local memory | `crates/memory/src/`, `docs/r5-controlled-memory-evaluation.md` |
| Memory candidate extraction | `apps/runtime-gateway/src/memory_extraction.rs` |
| Desktop guided setup commands | `apps/desktop/src-tauri/src/runtime_setup.rs` |
| Public SDK | `packages/typescript/src/` |

## Proven decisions (do not re-litigate)

Each one is enforced in code or tests; these lines point to it, they do not
replace it.

- **Traces and logs are content-free.** No transcript text, model output,
  audio, credentials or names in traces, logs or error details.
- **Protocol versions are frozen once released.** Default startup is protocol
  3; protocol 4 is opt-in. Earlier frames never change; new fields and events go
  only where the version rules in `docs/finalized-recording-recovery.md` allow,
  with matching SDK types and tests.
- **Configuration fails closed.** Unknown keys, out-of-range values and missing
  credentials stop startup. There is no silent upgrade of private configuration
  and no provider or model fallback.
- **`processText` refuses oversized input rather than chunking it.** One call
  answers one schema; merging several answers is the caller's job.
- **Diarization labels are anonymous.** A provider's own speaker label never
  reaches a client; clusters are `cluster-N`. Naming a person from a voice is not
  implemented (see `docs/HUMAN-CHECKLIST.md`).
- **Recording is never degraded by an optional consumer.** Recognition,
  provider or playback failures must not stop or corrupt a recording.
- **No personal settings in the repository.** Personal persona values, model
  choices, endpoints and absolute home-directory paths live in private
  configuration outside the repo. Commits add mechanisms, not someone's chosen
  values; check diffs for absolute user paths before committing.
- **Human observations are never fabricated.** Native audio, visual and acoustic
  acceptance stays on the human checklist until a person performs it.
