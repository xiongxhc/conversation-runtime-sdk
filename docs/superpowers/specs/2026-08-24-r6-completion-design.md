# R6 Completion Design

**Status:** Approved by the product-owner request to complete R6 on 2026-08-24.

## Problem

The desktop reference app proves the public runtime boundary, but a user still
has to build binaries, edit TOML, find absolute paths, and separately start
providers. That is a developer integration flow rather than a complete desktop
reference experience. R6 is complete only when the app can prepare and launch a
private local runtime installation, report whether the selected local model is
suitable on the current machine, and produce a distributable macOS artifact
without bundling model weights or private configuration.

## Scope

R6 completion includes:

- truthful documentation of the already implemented persona and memory controls;
- guided local model discovery, selection, benchmark, and private config creation;
- optional, explicit supervision of a user-selected local provider executable;
- bundled gateway and managed voice-sidecar executables with an advanced
  manual-path mode;
- deterministic packaging checks, an installable DMG, and optional
  deployment-credential signing and notarization;
- a native acceptance checklist that separates mechanical evidence from human
  microphone, playback, acoustic, and visual judgment.

R6 does not include model downloads, model weights, cloud fallback, LAN access,
iPhone pairing, Apple signing credentials, or fabricated human observations.
Signing and notarization workflows may be mechanically prepared, but a signed
and notarized release requires deployment-owned Apple credentials.

## Approaches Considered

### Recommended: bundled runtime, external models

Bundle the gateway and voice sidecar, generate private configuration under the
app's data directory, discover models through a loopback-only provider API, and
optionally supervise an explicitly selected provider executable without a
shell. This removes repository paths from normal setup while preserving the SDK's
backend-neutral and no-model-weights boundaries.

### Keep manual paths

This preserves the current architecture but leaves the product developer-only,
so it cannot satisfy R6 setup and installation exits.

### Bundle a provider and model

This offers the shortest first-run path but couples the public SDK to one model
distribution, increases package size and license exposure, and violates the
roadmap's no-model-weights requirement.

## Architecture

The React setup screen calls narrow Tauri commands. Native setup code reuses
gateway-owned deployment types to validate loopback endpoints, discover models,
benchmark a selected model with the existing Ollama-compatible adapter,
atomically write an owner-only TOML file, and resolve bundled binary paths. The
existing public TypeScript SDK remains the only conversation protocol client.

Provider supervision belongs to the gateway so desktop and Node consumers share
one lifecycle contract. Configuration schema v2 adds explicit external or
gateway-owned provider hosts. A provider specification contains an absolute
executable, bounded argument vector, loopback readiness endpoint, and startup
timeout. The gateway either observes an already-ready external provider without
claiming ownership or spawns the exact executable directly, waits for readiness,
monitors later exit, and reaps only the child it owns. It never executes a shell
string and never silently substitutes a remote service. Schema v1 remains an
explicit legacy external-provider configuration.

The desktop may use the same gateway supervisor temporarily during guided
discovery, then stop that child before launching the gateway, which becomes the
sole long-lived owner. Configuration rendering and validation are defined once
in the gateway crate rather than duplicated as Tauri string templates.

The packaging script builds release gateway and voice-sidecar binaries, stages
target-triple-suffixed Tauri external binaries, builds the `.app` and DMG, and
verifies that the bundle contains the expected nested executables but no TOML
deployment configuration, model weights, or known private model directories.
The application and Swift package share a macOS 14 minimum. Hardened-runtime
entitlements permit microphone capture without broadening filesystem or network
access beyond the app's declared local behavior.

## Guided Setup Flow

1. Resolve bundled gateway and voice-sidecar paths and the private config path.
2. Probe the default loopback endpoint without starting any process.
3. If unavailable, show an explicit option to select and start a local provider
   executable; never start it merely by opening setup.
4. List model identifiers returned by the selected loopback endpoint.
5. Run a fixed-prompt compatibility and latency check with bounded timeouts.
6. Show first-delta and total latency; do not label a single run as model
   suitability, conversational quality, or voice quality.
7. Write the private local-only config atomically with owner-only permissions.
8. Connect through the existing bundled gateway and verify runtime-reported
   local-only status.

Advanced setup retains manual absolute gateway and config paths for SDK
developers and nonstandard deployments.

## Data and Privacy

- Model identifiers and private paths remain on device and out of telemetry.
- The benchmark uses a fixed public prompt, not conversation content.
- Generated benchmark text is discarded and not added to app history or memory.
- Configuration is written only under the native app-data directory unless the
  user chooses advanced setup.
- Every network target is validated as loopback HTTP before access.
- No provider is started without an explicit user action.
- Gateway shutdown reaps owned provider children after runtime work stops;
  external providers are never terminated.

## Error Handling

Setup errors use bounded categories: provider unavailable, invalid endpoint,
model discovery failed, benchmark failed, configuration failed, bundled runtime
missing, provider startup timed out, and runtime connection failed. Messages do
not echo response bodies, prompts, generated content, or private paths.

Partially written configuration is never accepted. Failed provider startup
reaps the owned child. Benchmark cancellation drops the request and returns the
setup screen to a retryable state.

## Packaging and Release Boundary

The repository produces and verifies an unsigned current-target macOS `.app`
and DMG for local testing. A release script supports optional Developer ID
signing, hardened runtime, notarization, and stapling only when deployment
credentials are explicitly supplied through the environment. Absence of
credentials is reported as an honest skipped release gate, not a passed
signature or notarization. A local install/upgrade smoke mounts the DMG, replaces
an earlier app version, preserves app-data, and checks that no child process is
left behind; it does not bypass Gatekeeper or claim a fresh-Mac release pass.

## Acceptance Criteria

- A clean checkout can build one macOS `.app` and DMG containing the gateway and
  voice sidecar but no model weights or private TOML.
- Guided setup discovers an already-running loopback provider and its models,
  benchmarks a selected model, writes private config, and connects without the
  user entering repository paths.
- Managed-provider mode makes the gateway directly start one explicit absolute
  executable, reports readiness, monitors later exit, and reaps it on failure or
  gateway exit.
- Advanced manual-path setup remains functional.
- Persona get/update and memory list/inspect/approve/delete remain backed by the
  live runtime through the public SDK.
- Control capabilities are advertised explicitly; controls cannot be opened
  while a live voice session would cause the gateway to reject them; failed
  preset replay never remains labeled Active; deletion requires confirmation.
- Full Rust, TypeScript, desktop, Swift, bundle-content, and config-permission
  checks pass.
- Optional signing/notarization commands fail closed when credentials are
  incomplete and verify strict signatures, entitlements, Gatekeeper, and
  stapling when deployment credentials are supplied.
- Human microphone, audible playback, barge-in feel, device routing, and scene
  appearance are recorded separately; they are never inferred from automation.
