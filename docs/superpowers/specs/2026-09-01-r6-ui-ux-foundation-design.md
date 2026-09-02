# R6 UI/UX Foundation Design

**Status:** Approved by the product owner on 2026-09-01.

## User Problem

People need one dependable place to think with text or voice, but the current
desktop app exposes runtime machinery before it establishes a clear
conversation flow. Navigation, local-state reporting, memory review, response
controls, and Voice Focus therefore feel like separate technical surfaces
rather than one understandable product.

## Decision

R6 gains a UI/UX Foundation phase before guided setup. This is a product-flow
redesign, not a visual reskin. It preserves the existing runtime, voice,
history, persona, and memory semantics while reorganizing their presentation
around a conversation-first information architecture.

The visual direction is **Local Signal Instrument**: a calm, native-feeling
interface whose visual emphasis comes from truthful on-device state. The
normal workspace and Voice Focus must feel like two light levels of the same
instrument rather than unrelated products.

## Scope and Ordering

This phase delivers:

- one shared semantic visual system for setup, workspace chrome, utility panes,
  dialogs, and Voice Focus;
- a labelled, responsive navigation system with real icons and explicit
  available, unavailable, disabled, current, and review-needed states;
- a conversation-first main surface with local-history disclosure, legible
  text/voice handoff, and one visible recovery action for every failure state;
- a compact contextual local-signal panel instead of developer-oriented runtime
  status dominating the screen;
- coherent wording for Sessions, Memory review, and How it responds;
- durable notification of newly extracted memories until Memory review is
  opened;
- accessible Voice Focus secondary controls, error treatment, and locality
  trace behavior;
- responsive behavior at the native default and minimum window sizes, plus
  narrow-window resilience.

The immediately following R6 guided-setup task still owns provider discovery,
model selection, benchmarking, private configuration creation, managed-provider
start, and advanced filesystem paths. It reuses this phase's visual foundation
but does not share its state machine.

This phase does not change protocol messages, runtime capabilities, conversation
transport, provider lifecycle, model behavior, history persistence, memory
approval rules, persona values, voice lifecycle, or focus scene registry.

## Information Architecture

The connected app has four stable destinations:

1. **Conversation** — the default home for text, voice state, Stop, retry, and
   Voice Focus entry.
2. **Sessions** — read-only conversations saved locally by the app. Opening a
   session never restores model context.
3. **Memory review** — runtime memory availability, newly extracted review
   count, inspection, approval, and revision-bound deletion.
4. **How it responds** — current response controls and client-owned presets.

Voice Focus is a mode of the current conversation, not a destination or a new
session. Diagnostics are a secondary disclosure inside the local-signal panel;
they do not occupy primary navigation.

Unavailable destinations remain visible and explain why they are unavailable.
Capability gates continue to fail closed: a disabled destination never invokes
an unsupported runtime command. During streaming or active voice, runtime
controls remain disabled with the existing immediate action needed to unlock
them.

## Ordinary Language

Normal UI uses:

- “Conversation,” “Sessions,” “Memory review,” and “How it responds”;
- “Connected to this Mac,” “Ready,” “Thinking,” “Voice listening,” “Voice
  paused,” and “Needs attention”;
- “Disconnect local runtime” and “Reconnect local runtime”;
- “This conversation is saved on this Mac.” before the first send;
- “Voice paused while you type; it will resume after this response.” when the
  existing pause-before-type lifecycle is active;
- “Changing how it responds starts a fresh active conversation. Saved sessions
  and approved memories remain.” at the response-control apply decision.

“Gateway,” “configuration,” “capability,” “revision,” “provenance,” filesystem
paths, and STT/LLM/TTS abbreviations are reserved for setup Advanced mode,
Diagnostics, or forensic memory detail where the technical term is the data.

## Visual System

### Semantic color tokens

| Token | Light | Dark | Meaning |
|---|---:|---:|---|
| `--canvas` | `#F6F3EC` | `#202226` | Application background |
| `--ink` | `#282A2B` | `#F1EEE7` | Primary text and high-emphasis actions |
| `--muted` | `#626560` | `#B7B5AE` | Supporting copy and metadata |
| `--rule` | `#CDCAC2` | `#46494D` | Dividers and inactive boundaries |
| `--signal` | `#315F5C` | `#A8D8D3` | Verified local-ready state and focus |
| `--attention` | `#8A4039` | `#FFB4AB` | Error, destructive action, broken trace |

Application chrome uses these semantic tokens instead of mode-specific raw
colors. Trusted Focus scene implementations may retain their own artistic
palette; semantic controls and status layered above them still use the tokens.

### Typography

- Display: `-apple-system, BlinkMacSystemFont, "SF Pro Display", system-ui,
  sans-serif`, used only for screen and pane titles.
- Body: `-apple-system, BlinkMacSystemFont, "SF Pro Text", system-ui,
  sans-serif`, used for content and controls.
- Utility/data: `"SF Mono", "SFMono-Regular", ui-monospace, Menlo, Monaco,
  monospace`, used for locality, runtime state, device labels, and metadata.

No typeface or visual asset is fetched from the network.

### Geometry and motion

- Spacing uses `4, 8, 12, 16, 24, 32, 48, 64px` tokens.
- Controls use an 8px radius, fields/notices 12px, and status pills or trace
  rings alone may use a fully rounded shape.
- Every designed control has rest, hover, pressed, disabled, current, and
  visible keyboard-focus treatment.
- Reduced motion removes looping trace and presence animation. No meaning is
  communicated only by motion or opacity.

## Signature Element: Locality Trace

The Locality Trace is one thin route that carries only verified runtime state.

- In the connected workspace it links Runtime, Model, Memory, and Voice.
  Verified local segments are solid `--signal`; unavailable segments are
  dashed `--rule`; an error is a static broken `--attention` segment.
- In Voice Focus it becomes a restrained orbital route around the presence.
  Idle is static. Listening, thinking, and speaking may use different slow
  cadences only when reduced motion is not requested. Interrupted becomes still
  immediately; error becomes a static broken route.
- It never implies microphone activity, audio level, or local-only execution
  unless that fact exists in runtime state.

## Responsive Layout

- At 960px and wider, navigation is a 176px labelled rail, Conversation is the
  flexible center, and local signals use a compact contextual panel.
- From 600px through 959px, navigation is a 64px icon rail with accessible
  names, selected state, and pointer/keyboard tooltip. First-letter navigation
  is forbidden.
- Below 600px, navigation becomes a horizontally scrollable top rail with icon
  plus short label. All destinations and their selected/disabled state remain
  reachable.
- The composer, Stop/Send action, and current error recovery never leave the
  visible interaction path at 200% zoom.

## Component and State Boundaries

```text
App
  -> SetupView (unchanged until guided-setup task)
  -> Workspace (behavioral controller)
       -> WorkspaceNavigation (session-blind presentation)
       -> ConversationInstrument (session-blind presentation)
       -> RuntimeSignalPanel (session-blind presentation)
       -> Sessions / MemoryPane / SettingsPane
       -> VoiceFocus

DesktopSession -> Workspace only
Workspace -> derived props and callbacks -> presentation components
```

`Workspace` remains the sole owner of session subscription, persistence,
capability gates, active view, voice pause/resume races, Focus entry/exit, and
recovery operations. The three extracted presentation components must not
import `DesktopSession`, Tauri APIs, persistence adapters, or runtime transport.

## State and Recovery Rules

- The current runtime phase is always expressed in plain language and announced
  politely when it changes.
- A failed text turn remains attached to its transcript. Runtime disconnection
  exposes Reconnect; an operation failure exposes Reconnect and return-to-setup.
- A Memory extraction event produces a transient status announcement and a
  durable “N new” navigation count. The count means newly announced candidate
  memories since Memory review was last opened; it clears on opening that
  destination and does not claim to be the total store-wide candidate count.
- Focusing the composer while capture is active visibly explains the existing
  pause/resume behavior. No new resume path is introduced.
- Voice Focus persistent and secondary controls remain visually available while
  interactive. Focus-mode errors receive `--attention` treatment and a named
  recovery action.
- Applying response controls presents the fresh-active-conversation consequence
  at the decision point; saved Sessions and approved memories remain unaffected.

## Accessibility

- Every destination and control has a programmatic name matching its visible
  language.
- Current destinations use `aria-current`; unavailable/temporarily disabled
  destinations expose a visible reason and an `aria-describedby` relationship.
- Transcript remains a polite `role="log"`; state notices use `role="status"`;
  actionable errors use `role="alert"`.
- Keyboard traversal never lands on a control faded below useful contrast.
- Normal text meets 4.5:1 contrast and controls, focus, current state, and trace
  states meet 3:1 against adjacent surfaces in light and dark modes.

## Acceptance Criteria

- A connected person reaches Conversation without encountering gateway paths or
  protocol terminology.
- Conversation, Sessions, Memory review, and How it responds remain identifiable
  at 1120×760, 900×680, and narrow-window layouts.
- Text send, Stop, reconnect, Voice Focus, session deletion, memory
  approve/delete, response-control apply, and disconnect preserve their existing
  behavior and safety gates.
- Local history is disclosed before the first send; Sessions remain read-only
  and distinct from model context and runtime memory.
- Memory is intelligible as available, off, unsupported, temporarily disabled,
  or newly needing review without relying only on a six-second notice.
- Typing during active voice visibly explains pause/resume and retains the
  existing same-session, terminal-turn resume safeguards.
- Voice Focus controls remain visible and named, Focus errors have readable
  recovery treatment, and reduced motion communicates the same states without
  looping animation.
- Desktop unit/component tests, typechecks, production build, scene chunk check,
  and existing Rust guided-setup tests pass.
- Human review separately checks light/dark appearance, keyboard flow, 200%
  zoom, reduced motion, 1120×760 and 900×680 layouts, Voice Focus cohesion, and
  real-device interaction. Automated checks do not claim subjective visual,
  acoustic, or device acceptance.
