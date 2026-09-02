import type { Ref } from "react";

export type LocalityState = "verified" | "unavailable" | "error";

export interface RuntimeSignalReading {
  label: string;
  value: string;
  state: LocalityState;
}

export interface LocalityTraceReading {
  state: LocalityState;
  detail?: string;
}

export type LocalityTrace = Record<"runtime" | "model" | "memory" | "voice", LocalityTraceReading>;

export type RuntimeSignalAction = (
  | { enabled: true; onInvoke(): void }
  | { enabled: false; reason: string }
) & { buttonRef?: Ref<HTMLButtonElement> };

export interface RuntimeSignalActions {
  voice: RuntimeSignalAction & { label: "Voice Focus" | "Preview Voice Focus" };
  connection?: RuntimeSignalAction & {
    label: "Reconnect local runtime" | "Disconnect local runtime";
  };
}

export interface RuntimeSignalPanelProps {
  actions: RuntimeSignalActions;
  connectionLabel?: "Connected to this Mac" | "Needs attention";
  locality: LocalityTrace;
  memory: RuntimeSignalReading;
  model: RuntimeSignalReading;
  voice: RuntimeSignalReading;
}

const localityStages = [
  { key: "runtime", label: "Runtime" },
  { key: "model", label: "Model" },
  { key: "memory", label: "Memory" },
  { key: "voice", label: "Voice" },
] as const;

export function RuntimeSignalPanel({
  actions,
  connectionLabel = "Connected to this Mac",
  locality,
  memory,
  model,
  voice,
}: RuntimeSignalPanelProps) {
  return (
    <aside aria-label="Local signals" className="runtime-signal-panel">
      <header>
        <p className="utility-label">{connectionLabel}</p>
        <h2>Local signals</h2>
      </header>
      <dl className="runtime-signal-readings">
        {[model, memory, voice].map((reading) => (
          <div data-state={reading.state} key={reading.label}>
            <dt>{reading.label}</dt>
            <dd>{reading.value}</dd>
          </div>
        ))}
      </dl>
      <ol aria-label="Locality Trace" className="locality-trace">
        {localityStages.map(({ key, label }) => {
          const segment = locality[key];
          return (
            <li data-state={segment.state} key={key}>
              <span className="locality-trace-label">{label}</span>
              <span className="locality-trace-state">
                {localityLabel(segment.state)}{segment.detail ? `: ${segment.detail}` : ""}
              </span>
            </li>
          );
        })}
      </ol>
      <div className="runtime-signal-actions">
        <SignalAction action={actions.voice} label={actions.voice.label} />
        {actions.connection ? (
          <SignalAction action={actions.connection} label={actions.connection.label} />
        ) : null}
      </div>
    </aside>
  );
}

function SignalAction({ action, label }: { action: RuntimeSignalAction; label: string }) {
  const reasonId = !action.enabled ? `${label.toLowerCase().replaceAll(" ", "-")}-reason` : undefined;
  return (
    <div>
      <button
        aria-describedby={reasonId}
        disabled={!action.enabled}
        onClick={() => {
          if (action.enabled) action.onInvoke();
        }}
        ref={action.buttonRef}
        type="button"
      >
        {label}
      </button>
      {!action.enabled ? <p className="runtime-action-reason" id={reasonId}>{action.reason}</p> : null}
    </div>
  );
}

function localityLabel(state: LocalityState): string {
  switch (state) {
    case "verified":
      return "Verified locally";
    case "unavailable":
      return "Unavailable";
    case "error":
      return "Needs attention";
  }
}
