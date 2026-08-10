# R6 Desktop Voice Session Native Check

**Status:** Not yet run for the current branch.

This checklist records macOS behavior that deterministic tests and compiled
fixtures cannot prove. It is separate from R3 acoustic acceptance: a successful
desktop check does not establish first-audible latency, audible-stop p95,
subjective voice quality, or ten-minute continuity.

## Privacy Rules

- Use a private gateway configuration outside the repository.
- Keep microphone/speaker names, ASR paths, exact model/voice selections, and
  transcripts in a local untracked record only.
- Commit only a content-free summary such as pass, fail, skipped, elapsed range,
  and failure stage.
- Confirm the session reports `LocalOnly` and every STT, LLM, TTS, audio, tool,
  memory, and telemetry component is local or disabled before selecting
  `Start voice`.
- Do not continue if the app or gateway silently selects a remote provider.

## Local Run Record

Copy this section to an untracked local file before testing:

```text
Date/time:
macOS version:
Mac model:
Input device:
Output device:
Gateway commit:
Private config path:
ASR selection:
LLM selection:
TTS/voice selection:
Observed notes:
```

Do not commit the completed local record.

## Preconditions

- [ ] `cargo build --locked -p conversation-runtime-gateway` passes.
- [ ] The release managed sidecar is built and executable.
- [ ] The private gateway configuration uses existing absolute local paths and
      loopback-only provider endpoints.
- [ ] The selected language and speech services are already running locally.
- [ ] `npm run desktop:dev` opens the native window.
- [ ] macOS microphone permission can be observed or changed for this build.

## Interaction Checklist

- [ ] Entering Voice Focus does not access the microphone.
- [ ] Start voice requests permission and reaches Listening.
- [ ] One spoken turn appears in the same transcript as a typed turn.
- [ ] Speech during playback audibly interrupts the old generation.
- [ ] Stop voice and exit waits until microphone/playback stop.
- [ ] Keep voice running leaves a visible microphone indicator.
- [ ] Cancel remains in Voice Focus.
- [ ] Composer focus pauses capture before typed send.
- [ ] Typed terminal resumes capture when no draft remains.
- [ ] App close leaves no gateway or sidecar child.

## Additional Observations

- [ ] The displayed component-locality status remains correct throughout the
      session and changes are never hidden.
- [ ] Denied microphone permission produces a visible recoverable failure and
      does not break typed chat.
- [ ] Disconnecting or changing the input device produces a visible stage-specific
      failure rather than a false Listening state.
- [ ] Voice Focus Stop failure remains visible and retryable.
- [ ] Background voice can be stopped or returned to Focus from Conversation.
- [ ] Partial transcript text disappears after finalization and is absent from
      reopened History.
- [ ] Soft Aurora, Silk, Threads, Prism, Orb, Still Gradient, and None render;
      reduced-motion uses the intended fallback.

## Result Summary

Record only content-free results in the repository evaluation:

```text
Native window observed: pass | fail | skipped
Microphone permission observed: pass | fail | skipped
Shared typed/spoken transcript: pass | fail | skipped
Audible playback observed: pass | fail | skipped
Audible barge-in observed: pass | fail | skipped
Exit choices observed: pass | fail | skipped
Composer pause/resume observed: pass | fail | skipped
Child cleanup observed: pass | fail | skipped
Failure stage, if any: permission | capture | recognition | generation | synthesis | playback | lifecycle | unknown
```

If the Mac is locked, audio cannot be heard, the configured device is
unavailable, or a human did not observe the behavior directly, mark the item
`skipped`; do not infer a pass from logs, tests, process startup, or visual UI
state alone.
