import type { VoiceSessionState } from "../runtime/conversation-session.js";

export interface ConversationVoiceStatusProps {
  error?: string;
  retryBusy?: boolean;
  retryLabel?: string;
  voice: VoiceSessionState;
  onReturn(): void;
  onRetry?(): void;
  onStop(): void;
}

export function ConversationVoiceStatus({
  error,
  retryBusy = false,
  retryLabel = "Retry voice control",
  voice,
  onReturn,
  onRetry,
  onStop,
}: ConversationVoiceStatusProps) {
  return (
    <section className="conversation-voice-status" aria-label="Voice status">
      <div aria-hidden="true" className="microphone-pulse" data-capture={voice.capture} />
      <div>
        <p aria-live="polite">{voiceStatusLabel(voice)}</p>
        <span>Audio stays on this Mac.</span>
        {error ? <span className="voice-control-error" role="alert">{error}</span> : null}
      </div>
      <div className="conversation-voice-actions">
        <button onClick={onReturn} type="button">Return to Voice Focus</button>
        {error && onRetry ? (
          <button disabled={retryBusy} onClick={onRetry} type="button">{retryLabel}</button>
        ) : null}
        <button onClick={onStop} type="button">Stop voice</button>
      </div>
    </section>
  );
}

function voiceStatusLabel(voice: VoiceSessionState): string {
  if (voice.error) return "Voice needs attention locally";
  switch (voice.capture) {
    case "starting":
      return "Microphone permission requested locally";
    case "listening":
      return "Microphone listening locally";
    case "pausing":
      return "Microphone pausing locally";
    case "paused":
      return "Microphone paused locally";
    case "resuming":
      return "Microphone resuming locally";
    default:
      return voice.session === "stopping" ? "Voice is stopping locally" : "Voice needs attention";
  }
}
