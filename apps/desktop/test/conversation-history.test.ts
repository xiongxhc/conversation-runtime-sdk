import { describe, expect, it } from "vitest";

import {
  TauriConversationHistoryStore,
  type ConversationHistory,
  type NativeInvoke,
} from "../src/history/conversation-history.js";

describe("TauriConversationHistoryStore", () => {
  it("maps the typed history API to the native command boundary", async () => {
    const conversation = storedConversation();
    const calls: unknown[][] = [];
    const nativeInvoke: NativeInvoke = async <T>(
      command: string,
      arguments_?: Record<string, unknown>,
    ) => {
      calls.push(arguments_ ? [command, arguments_] : [command]);
      const value = switchValue(command, conversation);
      return value as T;
    };
    const store = new TauriConversationHistoryStore(nativeInvoke);

    expect(await store.storagePath()).toBe("/private/history.sqlite3");
    expect(await store.list()).toHaveLength(1);
    expect(await store.get("conversation-1")).toEqual(conversation);
    await store.save(conversation);
    await store.delete("conversation-1");

    expect(calls).toEqual([
      ["history_storage_info"],
      ["list_conversation_history"],
      ["get_conversation_history", { id: "conversation-1" }],
      ["save_conversation_history", { conversation }],
      ["delete_conversation_history", { id: "conversation-1" }],
    ]);
  });

  it("maps a missing native conversation to undefined", async () => {
    const nativeInvoke: NativeInvoke = async <T>() => null as T;
    const store = new TauriConversationHistoryStore(nativeInvoke);

    expect(await store.get("missing")).toBeUndefined();
  });
});

function switchValue(command: string, conversation: ConversationHistory): unknown {
  switch (command) {
        case "history_storage_info":
          return { databasePath: "/private/history.sqlite3" };
        case "list_conversation_history":
          return [{ ...conversation, turns: undefined }];
        case "get_conversation_history":
          return conversation;
        default:
          return undefined;
  }
}

function storedConversation(): ConversationHistory {
  return {
    id: "conversation-1",
    title: "Local conversation",
    createdAtMs: 1,
    updatedAtMs: 2,
    turns: [{
      turnId: "1",
      transcript: "Hello",
      response: "Hi",
      state: "completed",
      failureMessage: undefined,
    }],
  };
}
