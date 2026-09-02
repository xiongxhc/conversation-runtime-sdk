import { describe, expect, it } from "vitest";

import {
  TauriConversationHistoryStore,
  type ConversationHistory,
  type ConversationHistoryWrite,
  type NativeInvoke,
  type PreparedContinuation,
} from "../src/history/conversation-history.js";

describe("TauriConversationHistoryStore", () => {
  it("maps the revisioned history and continuation API to exact native commands", async () => {
    const conversation = storedConversation();
    const { revision: _revision, ...write } = conversation;
    const prepared = preparedContinuation();
    const calls: unknown[][] = [];
    const nativeInvoke: NativeInvoke = async <T>(
      command: string,
      arguments_?: Record<string, unknown>,
    ) => {
      calls.push(arguments_ ? [command, arguments_] : [command]);
      return switchValue(command, conversation, prepared) as T;
    };
    const generatedIds = ["branch-generated", "operation-generated"];
    const store = new TauriConversationHistoryStore(
      nativeInvoke,
      () => generatedIds.shift() ?? "unexpected-id",
      () => 1_725_000_000_000,
    );

    expect(await store.storagePath()).toBe("/private/history.sqlite3");
    expect(await store.list()).toEqual([{ ...conversation, turns: undefined }]);
    expect(await store.get("conversation-1")).toEqual(conversation);
    expect(await store.save(write)).toEqual({ revision: "2" });
    expect(await store.save(write, "1")).toEqual({ revision: "2" });
    await store.delete("conversation-1", "2");
    expect(await store.prepareContinuation("conversation-1", "2")).toEqual(prepared);
    expect(
      await store.setContinuationState("branch-generated", "1", "confirmed"),
    ).toEqual({ revision: "2" });

    expect(calls).toEqual([
      ["history_storage_info"],
      ["list_conversation_history"],
      ["get_conversation_history", { id: "conversation-1" }],
      ["save_conversation_history", { conversation: write }],
      ["save_conversation_history", { conversation: write, expectedRevision: "1" }],
      ["delete_conversation_history", { id: "conversation-1", expectedRevision: "2" }],
      [
        "prepare_conversation_continuation",
        {
          sourceId: "conversation-1",
          expectedRevision: "2",
          branchId: "branch-generated",
          operationId: "operation-generated",
          nowMs: 1_725_000_000_000,
        },
      ],
      [
        "set_conversation_continuation_state",
        {
          branchId: "branch-generated",
          expectedRevision: "1",
          state: "confirmed",
        },
      ],
    ]);
  });

  it("maps a missing native conversation to undefined", async () => {
    const nativeInvoke: NativeInvoke = async <T>() => null as T;
    const store = new TauriConversationHistoryStore(nativeInvoke);

    expect(await store.get("missing")).toBeUndefined();
  });

  it("rejects invalid decimal input revisions before invoking native code", async () => {
    const calls: string[] = [];
    const nativeInvoke: NativeInvoke = async <T>(command: string) => {
      calls.push(command);
      return undefined as T;
    };
    const store = new TauriConversationHistoryStore(nativeInvoke);

    for (const invalid of ["", "0", "01", "-1", "1.0", "9223372036854775808"]) {
      await expect(store.delete("conversation-1", invalid)).rejects.toThrow(
        "conversation history revision is invalid",
      );
    }
    expect(calls).toEqual([]);
  });

  it("rejects invalid decimal revisions returned by native code", async () => {
    const conversation = storedConversation();
    const { revision: _revision, ...write } = conversation;
    const nativeInvoke: NativeInvoke = async <T>(command: string) => {
      if (command === "save_conversation_history") {
        return { revision: "01" } as T;
      }
      return [{ ...conversation, revision: "0", turns: undefined }] as T;
    };
    const store = new TauriConversationHistoryStore(nativeInvoke);

    await expect(store.save(write)).rejects.toThrow(
      "conversation history revision is invalid",
    );
    await expect(store.list()).rejects.toThrow(
      "conversation history revision is invalid",
    );
  });
});

function switchValue(
  command: string,
  conversation: ConversationHistory,
  prepared: PreparedContinuation,
): unknown {
  switch (command) {
    case "history_storage_info":
      return { databasePath: "/private/history.sqlite3" };
    case "list_conversation_history":
      return [{ ...conversation, turns: undefined }];
    case "get_conversation_history":
      return conversation;
    case "save_conversation_history":
    case "set_conversation_continuation_state":
      return { revision: "2" };
    case "prepare_conversation_continuation":
      return prepared;
    default:
      return undefined;
  }
}

function preparedContinuation(): PreparedContinuation {
  return {
    branch: {
      ...storedConversation(),
      id: "branch-generated",
      title: "Continued: Local conversation",
      revision: "1",
      continuedFromId: "conversation-1",
      continuationOperationId: "operation-generated",
      continuationState: "preparing",
      turns: [{
        ...storedConversation().turns[0],
        origin: "continued_context",
      }],
    },
    seed: [{ user: "Hello", assistant: "Hi" }],
    operationId: "operation-generated",
  };
}

function storedConversation(): ConversationHistory {
  return {
    id: "conversation-1",
    title: "Local conversation",
    createdAtMs: 1,
    updatedAtMs: 2,
    revision: "1",
    continuedFromId: null,
    continuationOperationId: null,
    continuationState: null,
    turns: [{
      turnId: "1",
      transcript: "Hello",
      response: "Hi",
      state: "completed",
      failureMessage: null,
      origin: "live",
    }],
  };
}
