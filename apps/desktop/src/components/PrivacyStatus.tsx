import type { ConversationSessionState } from "../runtime/conversation-session.js";

export type ComponentExecutionStatus =
  | { status: "ready"; location: "local" | "remote" }
  | { status: "unavailable" | "unknown"; location: null };

export interface ComponentStatusSnapshot {
  stt: ComponentExecutionStatus;
  llm: ComponentExecutionStatus;
  tts: ComponentExecutionStatus;
}

export interface PrivacyStatusProps {
  components: ComponentStatusSnapshot;
  className?: string;
}

export function PrivacyStatus({
  components,
  className = "",
}: PrivacyStatusProps) {
  return (
    <div className={`privacy-status ${className}`.trim()} aria-label="Component locality">
      <span>STT {componentLabel(components.stt)}</span>
      <span aria-hidden="true">·</span>
      <span>LLM {componentLabel(components.llm)}</span>
      <span aria-hidden="true">·</span>
      <span>TTS {componentLabel(components.tts)}</span>
    </div>
  );
}

export function textOnlyComponentStatus(
  status: ConversationSessionState["status"],
): ComponentStatusSnapshot {
  return {
    stt: { status: "unavailable", location: null },
    llm: { status: "ready", location: status.languageLocation },
    tts: { status: "unavailable", location: null },
  };
}

export function voiceComponentStatus(
  status: ConversationSessionState["status"],
): ComponentStatusSnapshot {
  const location = (kind: "speech_recognition" | "language_model" | "speech_synthesis") => {
    const component = status.components.find((candidate) => candidate.kind === kind);
    return component
      ? { status: "ready" as const, location: component.executionLocation }
      : { status: "unavailable" as const, location: null };
  };
  return {
    stt: location("speech_recognition"),
    llm: location("language_model"),
    tts: location("speech_synthesis"),
  };
}

export const disconnectedComponentStatus: ComponentStatusSnapshot = {
  stt: { status: "unavailable", location: null },
  llm: { status: "unavailable", location: null },
  tts: { status: "unavailable", location: null },
};

function componentLabel(component: ComponentExecutionStatus): string {
  return component.status === "ready" ? component.location : component.status;
}
