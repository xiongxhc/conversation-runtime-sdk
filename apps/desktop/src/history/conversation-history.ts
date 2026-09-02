import { invoke } from "@tauri-apps/api/core";

export type HistoryRevision = string;
export type ConversationOrigin = "continued_context" | "live";
export type ContinuationState = "preparing" | "confirmed" | "unconfirmed";

export type ConversationHistoryTurn = {
  turnId: string;
  transcript: string;
  response: string;
  state: "streaming" | "completed" | "cancelled" | "failed";
  failureMessage: string | null;
  origin: ConversationOrigin;
};

export type ConversationSummary = {
  id: string;
  title: string;
  createdAtMs: number;
  updatedAtMs: number;
  revision: HistoryRevision;
  continuedFromId: string | null;
  continuationOperationId: string | null;
  continuationState: ContinuationState | null;
};

export type ConversationHistory = ConversationSummary & {
  turns: ConversationHistoryTurn[];
};

export type ConversationHistoryWrite = Omit<ConversationHistory, "revision">;

export type HistorySaveResult = {
  revision: HistoryRevision;
};

export type PreparedContinuation = {
  branch: ConversationHistory;
  seed: { user: string; assistant: string }[];
  operationId: string;
};

export interface ConversationHistoryStore {
  storagePath(): Promise<string>;
  list(): Promise<ConversationSummary[]>;
  get(id: string): Promise<ConversationHistory | undefined>;
  save(
    write: ConversationHistoryWrite,
    expectedRevision?: HistoryRevision,
  ): Promise<HistorySaveResult>;
  delete(id: string, expectedRevision: HistoryRevision): Promise<void>;
  prepareContinuation(
    sourceId: string,
    expectedRevision: HistoryRevision,
  ): Promise<PreparedContinuation>;
  setContinuationState(
    branchId: string,
    expectedRevision: HistoryRevision,
    state: ContinuationState,
  ): Promise<HistorySaveResult>;
}

type HistoryStorageInfo = {
  databasePath: string;
};

export type NativeInvoke = <T>(
  command: string,
  arguments_?: Record<string, unknown>,
) => Promise<T>;

type IdFactory = () => string;
type Clock = () => number;

const MAX_HISTORY_REVISION = 9_223_372_036_854_775_807n;

export class TauriConversationHistoryStore implements ConversationHistoryStore {
  constructor(
    private readonly nativeInvoke: NativeInvoke = invoke,
    private readonly createId: IdFactory = createRandomId,
    private readonly now: Clock = () => Date.now(),
  ) {}

  async storagePath(): Promise<string> {
    const info = await this.nativeInvoke<HistoryStorageInfo>("history_storage_info");
    return info.databasePath;
  }

  async list(): Promise<ConversationSummary[]> {
    const summaries = await this.nativeInvoke<ConversationSummary[]>(
      "list_conversation_history",
    );
    for (const summary of summaries) {
      assertHistoryRevision(summary.revision);
    }
    return summaries;
  }

  async get(id: string): Promise<ConversationHistory | undefined> {
    const conversation = await this.nativeInvoke<ConversationHistory | null>(
      "get_conversation_history",
      { id },
    );
    if (conversation === null) {
      return undefined;
    }
    assertHistoryRevision(conversation.revision);
    return conversation;
  }

  async save(
    conversation: ConversationHistoryWrite,
    expectedRevision?: HistoryRevision,
  ): Promise<HistorySaveResult> {
    if (expectedRevision !== undefined) {
      assertHistoryRevision(expectedRevision);
    }
    const arguments_ = expectedRevision === undefined
      ? { conversation }
      : { conversation, expectedRevision };
    const result = await this.nativeInvoke<HistorySaveResult>(
      "save_conversation_history",
      arguments_,
    );
    assertHistoryRevision(result.revision);
    return result;
  }

  async delete(id: string, expectedRevision: HistoryRevision): Promise<void> {
    assertHistoryRevision(expectedRevision);
    return this.nativeInvoke("delete_conversation_history", { id, expectedRevision });
  }

  async prepareContinuation(
    sourceId: string,
    expectedRevision: HistoryRevision,
  ): Promise<PreparedContinuation> {
    assertHistoryRevision(expectedRevision);
    const branchId = this.createId();
    const operationId = this.createId();
    const prepared = await this.nativeInvoke<PreparedContinuation>(
      "prepare_conversation_continuation",
      {
        sourceId,
        expectedRevision,
        branchId,
        operationId,
        nowMs: this.now(),
      },
    );
    assertHistoryRevision(prepared.branch.revision);
    return prepared;
  }

  async setContinuationState(
    branchId: string,
    expectedRevision: HistoryRevision,
    state: ContinuationState,
  ): Promise<HistorySaveResult> {
    assertHistoryRevision(expectedRevision);
    const result = await this.nativeInvoke<HistorySaveResult>(
      "set_conversation_continuation_state",
      { branchId, expectedRevision, state },
    );
    assertHistoryRevision(result.revision);
    return result;
  }
}

function assertHistoryRevision(value: unknown): asserts value is HistoryRevision {
  if (
    typeof value !== "string"
    || !/^[1-9]\d*$/.test(value)
    || BigInt(value) > MAX_HISTORY_REVISION
  ) {
    throw new Error("conversation history revision is invalid");
  }
}

function createRandomId(): string {
  const value = globalThis.crypto?.randomUUID?.();
  if (value === undefined) {
    throw new Error("secure random identifier generation is unavailable");
  }
  return value;
}

export const conversationHistoryStore = new TauriConversationHistoryStore();
