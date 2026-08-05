import { useEffect, useRef, useState, type Ref } from "react";

import {
  CommandRejectedError,
  type MemoryCursor,
  type MemoryInspection,
  type MemoryPage,
  type MemoryRetention,
  type MemorySummary,
  type RuntimeStatus,
} from "@conversation/runtime/browser";

import type { DesktopSession } from "../App.js";

export interface MemoryPaneProps {
  session: DesktopSession;
  status: RuntimeStatus;
  onBack(): void;
}

type MemoryError =
  | {
    message: string;
    scope: "list";
    cursor: MemoryCursor | null;
    replace: boolean;
    noticeOnSuccess?: string;
  }
  | {
    message: string;
    scope: "detail";
  };

export function MemoryPane({ session, status, onBack }: MemoryPaneProps) {
  const [records, setRecords] = useState<MemorySummary[]>([]);
  const [nextCursor, setNextCursor] = useState<MemoryCursor | null>();
  const [selectedId, setSelectedId] = useState<bigint>();
  const [inspection, setInspection] = useState<MemoryInspection>();
  const [loadingList, setLoadingList] = useState(true);
  const [loadingDetail, setLoadingDetail] = useState(false);
  const [error, setError] = useState<MemoryError>();
  const [notice, setNotice] = useState<string>();
  const listGeneration = useRef(0);
  const selectionGeneration = useRef(0);
  const originMemoryId = useRef<bigint | undefined>(undefined);
  const restoreListFocus = useRef(false);
  const listBackRef = useRef<HTMLButtonElement>(null);
  const detailBackRef = useRef<HTMLButtonElement>(null);
  const rowRefs = useRef(new Map<bigint, HTMLButtonElement>());

  const loadPage = async (
    cursor: MemoryCursor | null,
    replace: boolean,
    noticeOnSuccess?: string,
  ) => {
    const generation = replace ? ++listGeneration.current : listGeneration.current;
    setLoadingList(true);
    setError(undefined);
    if (replace) {
      setRecords([]);
      setNextCursor(undefined);
    }
    try {
      const page = await session.listMemories(cursor);
      if (generation !== listGeneration.current) return;
      setRecords((current) => mergeRecords(replace ? [] : current, page));
      setNextCursor(page.nextCursor);
      if (noticeOnSuccess) setNotice(noticeOnSuccess);
    } catch (loadError) {
      if (generation !== listGeneration.current) return;
      setError({
        message: memoryErrorMessage(loadError),
        scope: "list",
        cursor,
        replace,
        noticeOnSuccess,
      });
    } finally {
      if (generation === listGeneration.current) setLoadingList(false);
    }
  };

  useEffect(() => {
    void loadPage(null, true);
    return () => {
      listGeneration.current += 1;
      selectionGeneration.current += 1;
    };
  }, [session]);
  useEffect(() => {
    if (selectedId !== undefined) {
      detailBackRef.current?.focus();
      return;
    }
    if (!restoreListFocus.current || loadingList) return;
    restoreListFocus.current = false;
    const origin = originMemoryId.current;
    const target = origin === undefined ? undefined : rowRefs.current.get(origin);
    (target ?? listBackRef.current)?.focus();
  }, [loadingList, records, selectedId]);

  const openMemory = async (memoryId: bigint) => {
    const generation = ++selectionGeneration.current;
    originMemoryId.current = memoryId;
    setSelectedId(memoryId);
    setInspection(undefined);
    setLoadingDetail(true);
    setError(undefined);
    setNotice(undefined);
    try {
      const detail = await session.inspectMemory(memoryId);
      if (generation === selectionGeneration.current) setInspection(detail);
    } catch (inspectionError) {
      if (generation !== selectionGeneration.current) return;
      if (
        inspectionError instanceof CommandRejectedError
        && inspectionError.code === "memory_not_found"
      ) {
        restoreListFocus.current = true;
        setSelectedId(undefined);
        setInspection(undefined);
        setNotice("That memory no longer exists.");
        await loadPage(
          null,
          true,
          "That memory no longer exists. The list has been refreshed.",
        );
      } else {
        setError({
          message: memoryErrorMessage(inspectionError),
          scope: "detail",
        });
      }
    } finally {
      if (generation === selectionGeneration.current) setLoadingDetail(false);
    }
  };

  const showList = () => {
    selectionGeneration.current += 1;
    restoreListFocus.current = true;
    setSelectedId(undefined);
    setInspection(undefined);
    setLoadingDetail(false);
    setError(undefined);
  };

  const retry = () => {
    if (error?.scope === "detail" && selectedId !== undefined) {
      void openMemory(selectedId);
    } else if (error?.scope === "list") {
      void loadPage(error.cursor, error.replace, error.noticeOnSuccess);
    } else {
      void loadPage(null, true);
    }
  };

  if (selectedId !== undefined) {
    return (
      <section
        aria-busy={loadingDetail}
        aria-label="Memory detail"
        className="memory-pane"
      >
        <MemoryHeader
          actionLabel="All memories"
          actionRef={detailBackRef}
          eyebrow="Read-only inspection"
          onAction={showList}
          title="Memory detail"
        />
        {error ? <MemoryErrorState error={error.message} onRetry={retry} /> : null}
        {loadingDetail ? <p className="memory-loading">Loading memory…</p> : null}
        {inspection ? <MemoryDetail inspection={inspection} /> : null}
      </section>
    );
  }

  return (
    <section aria-busy={loadingList} aria-label="Runtime memory" className="memory-pane">
      <MemoryHeader
        actionLabel="Conversation"
        actionRef={listBackRef}
        eyebrow="Stored on this Mac"
        onAction={onBack}
        title="Runtime memory"
      />
      <p className="memory-disclosure">
        Memory for {status.modelId} stays local. Stored memory is fallible context, not an
        instruction or fixed behavior. Only eligible active memory may be used. Candidate and
        expired records remain visible for inspection.
      </p>
      {notice ? <p className="memory-notice" role="status">{notice}</p> : null}
      {error ? <MemoryErrorState error={error.message} onRetry={retry} /> : null}
      {loadingList && records.length === 0 ? (
        <p className="memory-loading">Loading memories…</p>
      ) : null}
      {!loadingList && !error && records.length === 0 ? (
        <p className="empty-memory">No memories to inspect.</p>
      ) : null}
      {records.length > 0 ? (
        <div className="memory-list" aria-label="Memories">
          {records.map((record) => (
            <button
              className="memory-item"
              key={record.id.toString()}
              onClick={() => void openMemory(record.id)}
              ref={(element) => {
                if (element) rowRefs.current.set(record.id, element);
                else rowRefs.current.delete(record.id);
              }}
              type="button"
            >
              <span className="memory-item-copy">{record.contentPreview}</span>
              <span className="memory-item-meta">
                <span>{label(record.kind)} · {formatTimestamp(record.updatedAtMs)}</span>
                <span className={`memory-state-badge memory-state-${record.state}`}>
                  {label(record.state)}
                </span>
                {record.pinned ? <span className="memory-pin-badge">Pinned</span> : null}
              </span>
            </button>
          ))}
        </div>
      ) : null}
      {nextCursor ? (
        <button
          className="memory-load-more"
          disabled={loadingList}
          onClick={() => void loadPage(nextCursor, false)}
          type="button"
        >
          {loadingList ? "Loading…" : "Load more"}
        </button>
      ) : null}
    </section>
  );
}

function MemoryHeader({
  actionLabel,
  actionRef,
  eyebrow,
  onAction,
  title,
}: {
  actionLabel: string;
  actionRef?: Ref<HTMLButtonElement>;
  eyebrow: string;
  onAction(): void;
  title: string;
}) {
  return (
    <header className="memory-header">
      <div>
        <p className="utility-label">{eyebrow}</p>
        <h1>{title}</h1>
      </div>
      <button
        className="quiet-action"
        onClick={onAction}
        ref={actionRef}
        type="button"
      >
        {actionLabel}
      </button>
    </header>
  );
}

function MemoryErrorState({ error, onRetry }: { error: string; onRetry(): void }) {
  return (
    <div className="memory-error" role="alert">
      <p>{error}</p>
      <button className="quiet-action" onClick={onRetry} type="button">Retry</button>
    </div>
  );
}

function MemoryDetail({ inspection }: { inspection: MemoryInspection }) {
  const { record } = inspection;
  const confidence = Number(record.confidence) / 10;
  return (
    <div className="memory-detail">
      <p className="memory-content">{record.content}</p>
      <dl className="memory-metadata">
        <Metadata label="Memory ID" value={record.id.toString()} code />
        <Metadata label="Kind" value={label(record.kind)} />
        <Metadata label="State" value={label(record.state)} />
        <div>
          <dt>Confidence</dt>
          <dd
            aria-label={`Confidence ${formatPercentage(confidence)}, exact value ${record.confidence} out of 1000`}
          >
            {formatPercentage(confidence)}
          </dd>
        </div>
        <Metadata label="Created" value={formatTimestamp(record.createdAtMs)} />
        <Metadata label="Updated" value={formatTimestamp(record.updatedAtMs)} />
        <Metadata
          label="Last used"
          value={record.lastUsedAtMs === null ? "Never" : formatTimestamp(record.lastUsedAtMs)}
        />
        <Metadata label="Retention" value={formatRetention(record.retention)} />
        <Metadata
          label="Last retrieval"
          value={record.lastRetrievalReason === null ? "Not retrieved" : label(record.lastRetrievalReason)}
        />
        <Metadata label="Pin status" value={record.pinned ? "Pinned" : "Not pinned"} />
        <Metadata label="Revision" value={record.revision.toString()} />
      </dl>

      <HistorySection title="Provenance">
        {inspection.sources.map((source, index) => (
          <article className="memory-history-entry" key={`${source.sourceId}-${index}`}>
            <p>{label(source.kind)}</p>
            <dl>
              <Metadata label="Source" value={source.sourceId} code />
              <Metadata label="Actor" value={source.actor} code />
              <Metadata label="Recorded" value={formatTimestamp(source.sourceTimestampMs)} />
            </dl>
          </article>
        ))}
        {inspection.sourcesTruncated ? (
          <p className="memory-history-note">Older provenance entries are not shown</p>
        ) : null}
      </HistorySection>

      <HistorySection title="Approvals">
        {inspection.approvals.length === 0 ? (
          <p className="empty-memory-history">No approval history.</p>
        ) : inspection.approvals.map((approval, index) => (
          <article className="memory-history-entry" key={`${approval.confirmationId}-${index}`}>
            <p>{approval.confirmationId}</p>
            <dl>
              <Metadata label="Actor" value={approval.actor} code />
              <Metadata label="Confirmed" value={formatTimestamp(approval.confirmedAtMs)} />
              <Metadata label="Approved revision" value={approval.approvedRevision.toString()} />
            </dl>
          </article>
        ))}
        {inspection.approvalsTruncated ? (
          <p className="memory-history-note">Older approval entries are not shown</p>
        ) : null}
      </HistorySection>
    </div>
  );
}

function Metadata({ label: name, value, code = false }: {
  label: string;
  value: string;
  code?: boolean;
}) {
  return (
    <div>
      <dt>{name}</dt>
      <dd>{code ? <code>{value}</code> : value}</dd>
    </div>
  );
}

function HistorySection({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="memory-history" aria-labelledby={`memory-${title.toLowerCase()}`}>
      <h2 id={`memory-${title.toLowerCase()}`}>{title}</h2>
      {children}
    </section>
  );
}

function mergeRecords(current: MemorySummary[], page: MemoryPage): MemorySummary[] {
  const known = new Set(current.map((record) => record.id));
  return [
    ...current,
    ...page.records.filter((record) => {
      if (known.has(record.id)) return false;
      known.add(record.id);
      return true;
    }),
  ];
}

function memoryErrorMessage(error: unknown): string {
  if (error instanceof CommandRejectedError && error.code === "memory_unavailable") {
    return "Memory inspection is temporarily unavailable.";
  }
  return "Memory inspection could not be loaded.";
}

function formatRetention(retention: MemoryRetention): string {
  switch (retention.kind) {
    case "working":
      return `Working memory · Expires ${formatTimestamp(retention.expiresAtMs)}`;
    case "session":
      return `Session memory · Session ID ${retention.sessionId}`;
    case "until":
      return `Time-limited · Expires ${formatTimestamp(retention.expiresAtMs)}`;
    default:
      return "Until deleted";
  }
}

function formatTimestamp(timestamp: bigint): string {
  if (timestamp < 0n || timestamp > 8_640_000_000_000_000n) {
    return "Timestamp out of range";
  }
  const date = new Date(Number(timestamp));
  if (Number.isNaN(date.getTime())) return "Timestamp out of range";
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(date);
}

function formatPercentage(value: number): string {
  return `${Number.isInteger(value) ? value : value.toFixed(1)}%`;
}

function label(value: string): string {
  const words = value.replaceAll("_", " ");
  return `${words[0].toUpperCase()}${words.slice(1)}`;
}
