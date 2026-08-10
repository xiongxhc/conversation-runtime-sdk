import { useEffect, useRef } from "react";

export type VoiceExitChoice = "stop" | "keep" | "cancel";

export interface VoiceExitDialogProps {
  busy: boolean;
  error?: string;
  onChoose(choice: VoiceExitChoice): void;
}

export function VoiceExitDialog({ busy, error, onChoose }: VoiceExitDialogProps) {
  const busyRef = useRef(busy);
  const cancelButton = useRef<HTMLButtonElement>(null);
  const dialog = useRef<HTMLElement>(null);
  const onChooseRef = useRef(onChoose);

  useEffect(() => {
    onChooseRef.current = onChoose;
  }, [onChoose]);
  useEffect(() => {
    busyRef.current = busy;
    if (busy) dialog.current?.focus();
  }, [busy]);

  useEffect(() => {
    cancelButton.current?.focus();
    const handleKeyDown = (event: KeyboardEvent) => {
      if (busyRef.current) {
        if (event.key === "Escape" || event.key === "Tab") event.preventDefault();
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        onChooseRef.current("cancel");
        return;
      }
      if (event.key !== "Tab") return;
      const buttons = [...(dialog.current?.querySelectorAll<HTMLButtonElement>("button:not(:disabled)") ?? [])];
      if (buttons.length === 0) return;
      const first = buttons[0]!;
      const last = buttons.at(-1)!;
      if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      } else if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      }
    };
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("keydown", handleKeyDown);
      document.getElementById("voice-focus-exit")?.focus();
    };
  }, []);

  return (
    <section
      aria-label="Leave Voice Focus?"
      aria-modal="true"
      className="voice-exit-dialog"
      ref={dialog}
      role="dialog"
      tabIndex={-1}
    >
      <div className="voice-exit-card">
        <p className="utility-label">Voice is still active</p>
        <h2>Leave Voice Focus?</h2>
        <p>Choose whether the local microphone should stop or continue in the conversation view.</p>
        {error ? <p className="voice-exit-error" role="alert">{error}</p> : null}
        <div className="voice-exit-actions">
          <button disabled={busy} onClick={() => onChoose("stop")} type="button">
            Stop voice and exit
          </button>
          <button disabled={busy} onClick={() => onChoose("keep")} type="button">
            Keep voice active
          </button>
          <button disabled={busy} onClick={() => onChoose("cancel")} ref={cancelButton} type="button">
            Cancel
          </button>
        </div>
      </div>
    </section>
  );
}
