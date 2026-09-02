export type WorkspaceDestination = "conversation" | "sessions" | "memory" | "response";

export type DestinationAvailability = {
  enabled: boolean;
  reason?: string;
  badge?: string;
};

export interface WorkspaceNavigationProps {
  activeDestination: WorkspaceDestination;
  availability: Record<WorkspaceDestination, DestinationAvailability>;
  onSelect(destination: WorkspaceDestination): void;
}

const destinations: ReadonlyArray<{ id: WorkspaceDestination; label: string }> = [
  { id: "conversation", label: "Conversation" },
  { id: "sessions", label: "Sessions" },
  { id: "memory", label: "Memory review" },
  { id: "response", label: "How it responds" },
];

export function WorkspaceNavigation({
  activeDestination,
  availability,
  onSelect,
}: WorkspaceNavigationProps) {
  return (
    <nav aria-label="Workspace" className="workspace-navigation">
      {destinations.map(({ id, label }) => {
        const destination = availability[id];
        const tooltipId = `${id}-destination-tooltip`;
        const explanationId = destination.reason ? `${id}-destination-explanation` : undefined;
        const describedBy = [tooltipId, explanationId].filter(Boolean).join(" ");
        const accessibleLabel = id === "memory" && destination.badge
          ? `${label}; ${destination.badge.replace(" new", "")} newly announced candidate memories since Memory review was last opened`
          : label;
        return (
          <div className="workspace-navigation-item" key={id}>
            <button
              aria-label={accessibleLabel}
              aria-current={activeDestination === id ? "page" : undefined}
              aria-disabled={!destination.enabled || undefined}
              aria-describedby={describedBy}
              className="workspace-destination"
              data-current={activeDestination === id || undefined}
              data-disabled={!destination.enabled || undefined}
              onClick={() => {
                if (destination.enabled) onSelect(id);
              }}
              type="button"
            >
              <DestinationIcon destination={id} />
              <span className="workspace-destination-label">{label}</span>
              {destination.badge ? <span className="workspace-destination-badge">{destination.badge}</span> : null}
            </button>
            <span className="workspace-destination-tooltip" id={tooltipId} role="tooltip">{label}</span>
            {destination.reason ? (
              <p className="workspace-destination-reason" id={explanationId}>{destination.reason}</p>
            ) : null}
          </div>
        );
      })}
    </nav>
  );
}

function DestinationIcon({ destination }: { destination: WorkspaceDestination }) {
  const paths: Record<WorkspaceDestination, string> = {
    conversation: "M4 5.5h16v10H8l-4 4v-14Z",
    sessions: "M5 4h14v16H5zM8 8h8M8 12h8",
    memory: "M5 5h14v14H5zM8 9h8M8 13h5",
    response: "M5 5h14v14H5zM8 9h8M8 13h6",
  };

  return (
    <svg aria-hidden="true" data-icon={destination} fill="none" focusable="false" stroke="currentColor" viewBox="0 0 24 24">
      <path d={paths[destination]} strokeLinecap="round" strokeLinejoin="round" strokeWidth="1.8" />
    </svg>
  );
}
