import { invoke } from "@tauri-apps/api/core";

export type ConversationHistoryTurn = {
  turnId: string;
  transcript: string;
  response: string;
  state: "streaming" | "completed" | "cancelled" | "failed";
  failureMessage: string | undefined;
};

export type ConversationSummary = {
  id: string;
  title: string;
  createdAtMs: number;
  updatedAtMs: number;
};

export type ConversationHistory = ConversationSummary & {
  turns: ConversationHistoryTurn[];
};

export interface ConversationHistoryStore {
  storagePath(): Promise<string>;
  list(): Promise<ConversationSummary[]>;
  get(id: string): Promise<ConversationHistory | undefined>;
  save(conversation: ConversationHistory): Promise<void>;
  delete(id: string): Promise<void>;
}

type HistoryStorageInfo = {
  databasePath: string;
};

export type NativeInvoke = <T>(
  command: string,
  arguments_?: Record<string, unknown>,
) => Promise<T>;

export class TauriConversationHistoryStore implements ConversationHistoryStore {
  constructor(private readonly nativeInvoke: NativeInvoke = invoke) {}

  async storagePath(): Promise<string> {
    const info = await this.nativeInvoke<HistoryStorageInfo>("history_storage_info");
    return info.databasePath;
  }

  list(): Promise<ConversationSummary[]> {
    return this.nativeInvoke("list_conversation_history");
  }

  async get(id: string): Promise<ConversationHistory | undefined> {
    const conversation = await this.nativeInvoke<ConversationHistory | null>(
      "get_conversation_history",
      { id },
    );
    return conversation ?? undefined;
  }

  save(conversation: ConversationHistory): Promise<void> {
    return this.nativeInvoke("save_conversation_history", { conversation });
  }

  delete(id: string): Promise<void> {
    return this.nativeInvoke("delete_conversation_history", { id });
  }
}

export const conversationHistoryStore = new TauriConversationHistoryStore();
