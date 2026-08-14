// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  CommandRejectedError,
  type MemoryCursor,
  type MemoryInspection,
  type MemoryPage,
  type MemoryRetention,
  type RuntimeStatus,
} from "@conversation/runtime/browser";

import type { DesktopSession } from "../src/App.js";
import { MemoryPane } from "../src/components/MemoryPane.js";
import type { ConversationSessionState } from "../src/runtime/conversation-session.js";

afterEach(cleanup);

const localMemoryStatus: RuntimeStatus = {
  transport: "stdio",
  privacyMode: "local_only",
  languageLocation: "local",
  modelId: "local-model",
  memoryEnabled: true,
  memoryLocation: "local",
  telemetryEnabled: false,
  capabilities: ["text", "memory_inspection"],
  components: [
    { kind: "language_model", executionLocation: "local", providerLabel: "Local language" },
    { kind: "memory", executionLocation: "local", providerLabel: "Local memory" },
  ],
};

describe("MemoryPane", () => {
  it("shows loading, then a quiet empty state without pagination", async () => {
    const pendingPage = deferred<MemoryPage>();
    const session = new MemorySession();
    session.listMemories.mockReturnValueOnce(pendingPage.promise);

    renderPane(session);

    expect(screen.getByText("Loading memories…")).toBeTruthy();
    expect(screen.getByLabelText("Runtime memory").getAttribute("aria-busy")).toBe("true");

    pendingPage.resolve({ records: [], nextCursor: null });

    expect(await screen.findByText("No memories to inspect.")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
  });

  it("renders one page without a Load more action", async () => {
    const session = new MemorySession();
    session.listMemories.mockResolvedValueOnce({
      records: [summary(7n, "Prefers concise explanations")],
      nextCursor: null,
    });

    renderPane(session);

    expect(await screen.findByRole("button", { name: /Prefers concise explanations/ })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
  });

  it("describes stored memory as eligible fallible context, never an instruction", async () => {
    const session = new MemorySession();
    session.listMemories.mockResolvedValueOnce({
      records: [{ ...summary(7n, "Candidate memory"), state: "candidate" }],
      nextCursor: null,
    });

    renderPane(session);

    expect(await screen.findByText(
      "Memory for local-model stays local. Stored memory is fallible context, not an instruction or fixed behavior. Only eligible active memory may be used. Candidate and expired records remain visible for inspection.",
    )).toBeTruthy();
    expect(screen.queryByText(/can use as local context/i)).toBeNull();
  });

  it("shows state and pinned status directly in memory rows", async () => {
    const session = new MemorySession();
    session.listMemories.mockResolvedValueOnce({
      records: [
        summary(8n, "Active unpinned memory"),
        { ...summary(7n, "Pinned candidate memory"), state: "candidate", pinned: true },
      ],
      nextCursor: null,
    });

    renderPane(session);

    const active = await screen.findByRole("button", { name: /Active unpinned memory/ });
    const candidate = screen.getByRole("button", { name: /Pinned candidate memory/ });
    expect(within(active).getByText("Active")).toBeTruthy();
    expect(within(active).queryByText("Pinned")).toBeNull();
    expect(within(candidate).getByText("Candidate")).toBeTruthy();
    expect(within(candidate).getByText("Pinned")).toBeTruthy();
  });

  it("accumulates two pages without duplicate memory rows", async () => {
    const session = new MemorySession();
    session.listMemories
      .mockResolvedValueOnce({
        records: [summary(7n, "First memory"), summary(6n, "Overlapping memory")],
        nextCursor: { beforeId: 6n },
      })
      .mockResolvedValueOnce({
        records: [summary(6n, "Overlapping memory"), summary(5n, "Older memory")],
        nextCursor: null,
      });

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: "Load more" }));

    await screen.findByRole("button", { name: /Older memory/ });
    expect(screen.getAllByRole("button", { name: /memory/i })).toHaveLength(3);
    expect(session.listMemories).toHaveBeenNthCalledWith(2, { beforeId: 6n });
    expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
  });

  it("preserves accumulated rows and retries a failed page from its cursor", async () => {
    const session = new MemorySession();
    session.listMemories
      .mockResolvedValueOnce({
        records: [summary(7n, "First page memory")],
        nextCursor: { beforeId: 7n },
      })
      .mockRejectedValueOnce(commandError("memory_unavailable"))
      .mockResolvedValueOnce({
        records: [summary(6n, "Recovered second page memory")],
        nextCursor: null,
      });

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: "Load more" }));

    expect((await screen.findByRole("alert")).textContent).toContain(
      "Memory inspection is temporarily unavailable.",
    );
    expect(screen.getByRole("button", { name: /First page memory/ })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));

    expect(await screen.findByRole("button", { name: /Recovered second page memory/ })).toBeTruthy();
    expect(screen.getByRole("button", { name: /First page memory/ })).toBeTruthy();
    expect(session.listMemories).toHaveBeenNthCalledWith(3, { beforeId: 7n });
  });

  it("shows complete detail metadata and preserves received history order", async () => {
    const session = new MemorySession();
    session.listMemories.mockResolvedValueOnce({
      records: [summary(7n, "Prefers concise explanations")],
      nextCursor: null,
    });
    session.inspectMemory.mockResolvedValueOnce(inspection());

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));

    const detail = await screen.findByLabelText("Memory detail");
    expect(within(detail).getByText("Prefers concise explanations in technical discussions.")).toBeTruthy();
    expect(within(detail).getByText("Semantic")).toBeTruthy();
    expect(within(detail).getByText("Active")).toBeTruthy();
    expect(within(detail).getByText("84%").getAttribute("aria-label")).toBe(
      "Confidence 84%, exact value 840 out of 1000",
    );
    expect(within(detail).getByText("Until deleted")).toBeTruthy();
    expect(within(detail).getByText("Exact phrase")).toBeTruthy();
    expect(within(detail).getByText("Not pinned")).toBeTruthy();
    expect(within(detail).queryByText(/Session ID/i)).toBeNull();

    const detailText = detail.textContent ?? "";
    expect(detailText.indexOf("conversation-older")).toBeLessThan(
      detailText.indexOf("conversation-newer"),
    );
    expect(detailText.indexOf("confirmation-older")).toBeLessThan(
      detailText.indexOf("confirmation-newer"),
    );
  });

  it.each([
    [
      { kind: "working", expiresAtMs: 1_760_000_000_000n } satisfies MemoryRetention,
      /^Working memory · Expires /,
      /Session ID/,
    ],
    [
      { kind: "session", sessionId: 42n } satisfies MemoryRetention,
      "Session memory · Session ID 42",
      /Expires/,
    ],
    [
      { kind: "until", expiresAtMs: 1_760_000_000_000n } satisfies MemoryRetention,
      /^Time-limited · Expires /,
      /Session ID/,
    ],
  ])("renders retention kind %# with only its applicable value", async (
    retention,
    expected,
    inapplicable,
  ) => {
    const base = inspection();
    const session = sessionForInspection({
      ...base,
      record: { ...base.record, retention },
    });

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));

    expect(await screen.findByText(expected)).toBeTruthy();
    expect(screen.queryByText(inapplicable)).toBeNull();
  });

  it("moves focus into detail and restores it to the originating row", async () => {
    const pendingInspection = deferred<MemoryInspection>();
    const session = new MemorySession();
    session.listMemories.mockResolvedValueOnce({
      records: [summary(7n, "Focus origin memory")],
      nextCursor: null,
    });
    session.inspectMemory.mockReturnValueOnce(pendingInspection.promise);

    renderPane(session);
    const origin = await screen.findByRole("button", { name: /Focus origin memory/ });
    origin.focus();
    fireEvent.click(origin);

    const back = await screen.findByRole("button", { name: "All memories" });
    await waitFor(() => expect(document.activeElement).toBe(back));
    pendingInspection.resolve(inspection());
    await screen.findByText("Prefers concise explanations in technical discussions.");
    fireEvent.click(back);

    await waitFor(() => expect(document.activeElement).toBe(
      screen.getByRole("button", { name: /Focus origin memory/ }),
    ));
  });

  it("announces truncated older provenance and approval entries", async () => {
    const session = sessionForInspection({
      ...inspection(),
      sourcesTruncated: true,
      approvalsTruncated: true,
    });

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));

    expect(await screen.findByText("Older provenance entries are not shown")).toBeTruthy();
    expect(screen.getByText("Older approval entries are not shown")).toBeTruthy();
  });

  it("shows timestamps outside the JavaScript Date range safely", async () => {
    const base = inspection();
    const session = sessionForInspection({
      ...base,
      record: { ...base.record, updatedAtMs: 9_000_000_000_000_000n },
    });

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));

    expect(await screen.findByText("Timestamp out of range")).toBeTruthy();
  });

  it("returns to a refreshed list when the selected memory no longer exists", async () => {
    const session = new MemorySession();
    session.listMemories
      .mockResolvedValueOnce({ records: [summary(7n, "Vanishing memory")], nextCursor: null })
      .mockResolvedValueOnce({ records: [], nextCursor: null });
    session.inspectMemory.mockRejectedValueOnce(commandError("memory_not_found"));

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Vanishing memory/ }));

    expect((await screen.findByRole("status")).textContent).toContain(
      "That memory no longer exists. The list has been refreshed.",
    );
    expect(screen.getByText("No memories to inspect.")).toBeTruthy();
    expect(session.listMemories).toHaveBeenCalledTimes(2);
    await waitFor(() => expect(document.activeElement).toBe(
      screen.getByRole("button", { name: "Conversation" }),
    ));
  });

  it("does not announce a not-found refresh until the refresh succeeds", async () => {
    const session = new MemorySession();
    session.listMemories
      .mockResolvedValueOnce({ records: [summary(7n, "Vanishing memory")], nextCursor: null })
      .mockRejectedValueOnce(commandError("memory_unavailable"))
      .mockResolvedValueOnce({ records: [], nextCursor: null });
    session.inspectMemory.mockRejectedValueOnce(commandError("memory_not_found"));

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Vanishing memory/ }));

    expect((await screen.findByRole("status")).textContent).toBe(
      "That memory no longer exists.",
    );
    expect((await screen.findByRole("alert")).textContent).toContain(
      "Memory inspection is temporarily unavailable.",
    );
    expect(screen.queryByText(/list has been refreshed/i)).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByText("No memories to inspect.")).toBeTruthy();
    expect(screen.getByRole("status").textContent).toContain("The list has been refreshed.");
  });

  it("offers a retry when memory inspection is temporarily unavailable", async () => {
    const session = new MemorySession();
    session.listMemories.mockResolvedValueOnce({
      records: [summary(7n, "Recovered memory")],
      nextCursor: null,
    });
    session.inspectMemory
      .mockRejectedValueOnce(commandError("memory_unavailable"))
      .mockResolvedValueOnce(inspection());

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Recovered memory/ }));

    expect((await screen.findByRole("alert")).textContent).toContain(
      "Memory inspection is temporarily unavailable.",
    );
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByText(
      "Prefers concise explanations in technical discussions.",
    )).toBeTruthy();
    expect(session.inspectMemory).toHaveBeenCalledTimes(2);
  });

  it("exposes delete but no other mutation or retrieval controls for an active memory", async () => {
    const session = sessionForInspection(inspection());

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));
    await screen.findByLabelText("Memory detail");

    for (const name of ["Create", "Edit", "Approve", "Pin", "Retrieve"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
    expect(screen.getByRole("button", { name: "Delete" })).toBeTruthy();
  });

  it("shows Approve only for a candidate memory, not for an active one", async () => {
    const activeSession = sessionForInspection(inspection());
    renderPane(activeSession);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));
    await screen.findByLabelText("Memory detail");
    expect(screen.queryByRole("button", { name: "Approve" })).toBeNull();
    cleanup();

    const candidateBase = inspection();
    const candidateSession = sessionForInspection({
      ...candidateBase,
      record: { ...candidateBase.record, state: "candidate" },
    });
    renderPane(candidateSession);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));
    await screen.findByLabelText("Memory detail");
    expect(screen.getByRole("button", { name: "Approve" })).toBeTruthy();
  });

  it("approves a candidate memory, replacing the inspection and showing a notice", async () => {
    const base = inspection();
    const session = sessionForInspection({
      ...base,
      record: { ...base.record, state: "candidate" },
    });
    session.approveMemory.mockResolvedValueOnce({
      ...base,
      record: { ...base.record, state: "active", revision: 4n },
    });

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Approve" }));

    expect(await screen.findByRole("status")).toHaveProperty("textContent", "Memory approved");
    const detail = screen.getByLabelText("Memory detail");
    expect(within(detail).getByText("Active")).toBeTruthy();
    expect(within(detail).getByText("4")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Approve" })).toBeNull();
    expect(session.approveMemory).toHaveBeenCalledWith(7n, 3n);
  });

  it("deletes a memory, returning to a refreshed list with a notice", async () => {
    const session = sessionForInspection(inspection());
    session.deleteMemory.mockResolvedValueOnce(4n);
    session.listMemories.mockResolvedValueOnce({ records: [], nextCursor: null });

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Delete" }));

    expect(await screen.findByRole("heading", { name: "Runtime memory" })).toBeTruthy();
    expect((await screen.findByRole("status")).textContent).toBe("Memory deleted");
    expect(screen.getByText("No memories to inspect.")).toBeTruthy();
    expect(session.deleteMemory).toHaveBeenCalledWith(7n, 3n);
    expect(session.listMemories).toHaveBeenCalledTimes(2);
  });

  it("shows a retry-copy notice and re-inspects on a memory_conflict rejection from approve", async () => {
    const base = inspection();
    const session = sessionForInspection({
      ...base,
      record: { ...base.record, state: "candidate" },
    });
    session.approveMemory.mockRejectedValueOnce(commandError("memory_conflict"));
    session.inspectMemory.mockResolvedValueOnce({
      ...base,
      record: { ...base.record, state: "candidate", revision: 5n },
    });

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Approve" }));

    expect(await screen.findByRole("status")).toHaveProperty(
      "textContent",
      "This memory changed elsewhere — refreshed; try again.",
    );
    await waitFor(() => expect(session.inspectMemory).toHaveBeenCalledTimes(2));
    const detail = screen.getByLabelText("Memory detail");
    expect(within(detail).getByText("5")).toBeTruthy();
  });

  it("shows a retry-copy notice and re-inspects on a memory_conflict rejection from delete", async () => {
    const session = sessionForInspection(inspection());
    session.deleteMemory.mockRejectedValueOnce(commandError("memory_conflict"));
    session.inspectMemory.mockResolvedValueOnce({
      ...inspection(),
      record: { ...inspection().record, revision: 9n },
    });

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Delete" }));

    expect(await screen.findByRole("status")).toHaveProperty(
      "textContent",
      "This memory changed elsewhere — refreshed; try again.",
    );
    await waitFor(() => expect(session.inspectMemory).toHaveBeenCalledTimes(2));
    expect(screen.getByLabelText("Memory detail")).toBeTruthy();
    const detail = screen.getByLabelText("Memory detail");
    expect(within(detail).getByText("9")).toBeTruthy();
  });

  it("shows action-specific copy, not inspection copy, on a memory_unavailable rejection from approve", async () => {
    const base = inspection();
    const session = sessionForInspection({
      ...base,
      record: { ...base.record, state: "candidate" },
    });
    session.approveMemory.mockRejectedValueOnce(commandError("memory_unavailable"));

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Approve" }));

    expect((await screen.findByRole("alert")).textContent).toContain(
      "This memory could not be approved because memory is temporarily unavailable.",
    );
    expect(screen.queryByText("Memory inspection could not be loaded.")).toBeNull();
  });

  it("shows action-specific copy, not inspection copy, on a generic rejection from delete", async () => {
    const session = sessionForInspection(inspection());
    session.deleteMemory.mockRejectedValueOnce(new Error("boom"));

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Delete" }));

    expect((await screen.findByRole("alert")).textContent).toContain(
      "This memory could not be deleted.",
    );
    expect(screen.queryByText("Memory inspection could not be loaded.")).toBeNull();
    expect(screen.getByLabelText("Memory detail")).toBeTruthy();
  });

  it("returns to a refreshed list with a not-found notice on a memory_not_found rejection from approve", async () => {
    const base = inspection();
    const session = sessionForInspection({
      ...base,
      record: { ...base.record, state: "candidate" },
    });
    session.approveMemory.mockRejectedValueOnce(commandError("memory_not_found"));
    session.listMemories.mockResolvedValueOnce({ records: [], nextCursor: null });

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Approve" }));

    expect(await screen.findByRole("heading", { name: "Runtime memory" })).toBeTruthy();
    expect((await screen.findByRole("status")).textContent).toContain(
      "That memory no longer exists. The list has been refreshed.",
    );
    expect(screen.getByText("No memories to inspect.")).toBeTruthy();
    expect(session.listMemories).toHaveBeenCalledTimes(2);
  });

  it("returns to a refreshed list with a not-found notice on a memory_not_found rejection from delete", async () => {
    const session = sessionForInspection(inspection());
    session.deleteMemory.mockRejectedValueOnce(commandError("memory_not_found"));
    session.listMemories.mockResolvedValueOnce({ records: [], nextCursor: null });

    renderPane(session);
    fireEvent.click(await screen.findByRole("button", { name: /Prefers concise explanations/ }));
    fireEvent.click(await screen.findByRole("button", { name: "Delete" }));

    expect(await screen.findByRole("heading", { name: "Runtime memory" })).toBeTruthy();
    expect((await screen.findByRole("status")).textContent).toContain(
      "That memory no longer exists. The list has been refreshed.",
    );
    expect(screen.getByText("No memories to inspect.")).toBeTruthy();
    expect(session.listMemories).toHaveBeenCalledTimes(2);
  });

  it("refreshes the list when refreshSignal changes, but not on initial mount", async () => {
    const session = new MemorySession();
    session.listMemories
      .mockResolvedValueOnce({ records: [summary(7n, "Original memory")], nextCursor: null })
      .mockResolvedValueOnce({ records: [summary(9n, "Newly extracted memory")], nextCursor: null });

    const rendered = render(
      <MemoryPane onBack={vi.fn()} refreshSignal={0} session={session} status={localMemoryStatus} />,
    );
    await screen.findByRole("button", { name: /Original memory/ });
    expect(session.listMemories).toHaveBeenCalledTimes(1);

    rendered.rerender(
      <MemoryPane onBack={vi.fn()} refreshSignal={1} session={session} status={localMemoryStatus} />,
    );

    expect(await screen.findByRole("button", { name: /Newly extracted memory/ })).toBeTruthy();
    expect(session.listMemories).toHaveBeenCalledTimes(2);
  });
});

function renderPane(session: DesktopSession) {
  return render(
    <MemoryPane
      onBack={vi.fn()}
      session={session}
      status={localMemoryStatus}
    />,
  );
}

class MemorySession implements DesktopSession {
  state: ConversationSessionState = {
    phase: "ready",
    status: localMemoryStatus,
    turns: [],
    activeTurn: undefined,
    voice: {
      availability: "unavailable",
      session: "idle",
      capture: "stopped",
      visual: "idle",
      partialTranscript: "",
    },
    error: undefined,
  };
  readonly close = vi.fn(async () => undefined);
  readonly inspectMemory = vi.fn<(memoryId: bigint) => Promise<MemoryInspection>>();
  readonly approveMemory = vi.fn<DesktopSession["approveMemory"]>();
  readonly deleteMemory = vi.fn<DesktopSession["deleteMemory"]>();
  readonly onMemoryExtracted = vi.fn<DesktopSession["onMemoryExtracted"]>(() => () => undefined);
  readonly interrupt = vi.fn(async () => undefined);
  readonly listMemories = vi.fn<(cursor?: MemoryCursor | null) => Promise<MemoryPage>>();
  readonly getPersona = vi.fn<DesktopSession["getPersona"]>();
  readonly updatePersona = vi.fn<DesktopSession["updatePersona"]>();
  readonly pauseVoiceCapture = vi.fn(async () => undefined);
  readonly resumeVoiceCapture = vi.fn(async () => undefined);
  readonly send = vi.fn(async () => 1n);
  readonly startVoice = vi.fn(async () => undefined);
  readonly stopVoice = vi.fn(async () => undefined);

  subscribe(listener: (state: ConversationSessionState) => void) {
    listener(this.state);
    return () => undefined;
  }
}

function sessionForInspection(value: MemoryInspection): MemorySession {
  const session = new MemorySession();
  session.listMemories.mockResolvedValueOnce({
    records: [summary(7n, "Prefers concise explanations")],
    nextCursor: null,
  });
  session.inspectMemory.mockResolvedValueOnce(value);
  return session;
}

function summary(id: bigint, contentPreview: string): MemoryPage["records"][number] {
  return {
    id,
    contentPreview,
    kind: "semantic",
    state: "active",
    pinned: false,
    updatedAtMs: 1_750_000_000_000n,
  };
}

function inspection(): MemoryInspection {
  return {
    record: {
      id: 7n,
      kind: "semantic",
      content: "Prefers concise explanations in technical discussions.",
      state: "active",
      confidence: 840n,
      createdAtMs: 1_740_000_000_000n,
      updatedAtMs: 1_750_000_000_000n,
      pinned: false,
      revision: 3n,
      retention: { kind: "until_deleted" },
      lastUsedAtMs: 1_749_000_000_000n,
      lastRetrievalReason: "exact_phrase",
    },
    sources: [
      {
        kind: "user_provided",
        sourceId: "conversation-older",
        sourceTimestampMs: 1_740_000_000_000n,
        actor: "local-user",
      },
      {
        kind: "user_edited",
        sourceId: "conversation-newer",
        sourceTimestampMs: 1_745_000_000_000n,
        actor: "local-user",
      },
    ],
    approvals: [
      {
        confirmationId: "confirmation-older",
        actor: "local-user",
        confirmedAtMs: 1_746_000_000_000n,
        approvedRevision: 1n,
      },
      {
        confirmationId: "confirmation-newer",
        actor: "local-user",
        confirmedAtMs: 1_747_000_000_000n,
        approvedRevision: 2n,
      },
    ],
    sourcesTruncated: false,
    approvalsTruncated: false,
  };
}

function commandError(code: "memory_not_found" | "memory_unavailable" | "memory_conflict") {
  const messages = {
    memory_not_found: "memory record was not found",
    memory_unavailable: "memory inspection is unavailable",
    memory_conflict: "memory record changed since it was last inspected",
  } as const;
  return new CommandRejectedError({
    code,
    kind: "invalid_state",
    stage: "memory",
    message: messages[code],
  });
}

function deferred<T>() {
  let resolvePromise!: (value: T) => void;
  const promise = new Promise<T>((resolve) => {
    resolvePromise = resolve;
  });
  return { promise, resolve: resolvePromise };
}
