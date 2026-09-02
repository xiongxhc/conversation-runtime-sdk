export type ConversationInstrumentState = "ready" | "streaming" | "failed" | "closed";

type ConversationTurnContent = {
  id: string;
  transcript: string;
  response: string;
};

export type ConversationInstrumentTurn =
  | (ConversationTurnContent & { state: "streaming" | "completed" | "cancelled"; failure?: never })
  | (ConversationTurnContent & { state: "failed"; failure: ConversationTurnFailure });

export interface ConversationComposerState {
  value: string;
  disabled?: boolean;
  voicePausePending?: boolean;
  voicePaused?: boolean;
}

export type ConversationSendState =
  | { enabled: true }
  | { enabled: false; reason?: string };

export type ConversationRecoveryAction =
  | { enabled: true; onInvoke(): void }
  | { enabled: false; reason: string };

export interface ConversationRecoveryActions {
  reconnect: ConversationRecoveryAction;
  returnToSetup: ConversationRecoveryAction;
}

export interface ConversationOperationFailure {
  message: string;
  recovery: ConversationRecoveryActions;
}

export interface ConversationTurnFailure {
  message: string;
  retry: ConversationRecoveryAction;
}

export interface ConversationNotices {
  status?: string;
}

interface ConversationInstrumentBaseProps {
  composer: ConversationComposerState;
  notices?: ConversationNotices;
  send: ConversationSendState;
  turns: readonly ConversationInstrumentTurn[];
  onComposerBlur?(): void;
  onComposerChange(value: string): void;
  onComposerFocus?(): void;
  onSend(): void;
  onStop(): void;
}

export type ConversationInstrumentProps =
  | (ConversationInstrumentBaseProps & {
    operationFailure: ConversationOperationFailure;
    state: "failed" | "closed";
  })
  | (ConversationInstrumentBaseProps & {
    operationFailure?: ConversationOperationFailure;
    state: "ready" | "streaming";
  });

export function ConversationInstrument({
  composer,
  notices,
  operationFailure,
  send,
  state,
  turns,
  onComposerBlur,
  onComposerChange,
  onComposerFocus,
  onSend,
  onStop,
}: ConversationInstrumentProps) {
  const streaming = state === "streaming";
  const composerDisabled = composer.disabled || state === "failed" || state === "closed";
  const canSend = send.enabled && composer.value.trim().length > 0 && !composerDisabled;
  const sendReasonId = !send.enabled && send.reason ? "conversation-send-reason" : undefined;

  return (
    <section aria-labelledby="conversation-instrument-title" className="conversation-instrument">
      <header className="conversation-instrument-header">
        <div>
          <p className="utility-label">Conversation</p>
          <h1 id="conversation-instrument-title">A quiet place to think</h1>
        </div>
        <p aria-live="polite" className="conversation-phase">{phaseLabel(state)}</p>
      </header>

      {notices?.status ? <p className="conversation-notice" role="status">{notices.status}</p> : null}
      {operationFailure ? (
        <>
          <p className="conversation-error" role="alert">{operationFailure.message}</p>
        <div className="conversation-recovery-actions" aria-label="Conversation recovery actions">
          <RecoveryAction action={operationFailure.recovery.reconnect} label="Reconnect local runtime" />
          <RecoveryAction action={operationFailure.recovery.returnToSetup} label="Return to setup" />
        </div>
        </>
      ) : null}

      <div
        aria-atomic="false"
        aria-busy={streaming}
        aria-label="Conversation transcript"
        aria-live="polite"
        aria-relevant="additions"
        className="conversation-transcript"
        role="log"
      >
        {turns.length === 0 ? (
          <p className="conversation-empty">Start with a thought, question, or draft.</p>
        ) : turns.map((turn) => (
          <article className="conversation-turn" key={turn.id}>
            <p className="conversation-turn-user">{turn.transcript}</p>
            <p className="conversation-turn-response">
              {turn.response || (turn.state === "streaming" ? "Thinking…" : "No response")}
            </p>
            {turn.state === "failed" ? (
              <div className="conversation-turn-failure">
                <p className="conversation-turn-error" role="alert">{turn.failure.message}</p>
                <RecoveryAction action={turn.failure.retry} label="Try again" />
              </div>
            ) : null}
          </article>
        ))}
      </div>

      <p className="conversation-local-disclosure">This conversation is saved on this Mac.</p>
      {composer.voicePaused ? (
        <p className="conversation-voice-pause" role="status">
          Voice paused while you type; it will resume after this response.
        </p>
      ) : null}
      {!send.enabled && send.reason ? <p className="conversation-send-reason" id={sendReasonId}>{send.reason}</p> : null}
      <form
        className="conversation-composer"
        onSubmit={(event) => {
          event.preventDefault();
          if (canSend && !streaming) onSend();
        }}
      >
        <label className="visually-hidden" htmlFor="conversation-message">Message</label>
        <textarea
          disabled={composerDisabled}
          id="conversation-message"
          onBlur={onComposerBlur}
          onChange={(event) => onComposerChange(event.target.value)}
          onFocus={onComposerFocus}
          placeholder="Write a message"
          rows={2}
          value={composer.value}
        />
        {streaming ? (
          <button className="conversation-stop" onClick={onStop} type="button">Stop</button>
        ) : (
          <button aria-describedby={sendReasonId} className="conversation-send" disabled={!canSend} type="submit">Send</button>
        )}
      </form>
    </section>
  );
}

function RecoveryAction({ action, label }: { action: ConversationRecoveryAction; label: string }) {
  const reasonId = !action.enabled ? `${label.toLowerCase().replaceAll(" ", "-")}-reason` : undefined;
  return (
    <div>
      <button
        aria-describedby={reasonId}
        disabled={!action.enabled}
        onClick={() => {
          if (action.enabled) action.onInvoke();
        }}
        type="button"
      >
        {label}
      </button>
      {!action.enabled ? <p id={reasonId}>{action.reason}</p> : null}
    </div>
  );
}

function phaseLabel(state: ConversationInstrumentState): string {
  switch (state) {
    case "streaming":
      return "Thinking";
    case "failed":
      return "Needs attention";
    case "closed":
      return "Closed";
    default:
      return "Ready";
  }
}
