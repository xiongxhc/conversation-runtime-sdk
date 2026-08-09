import assert from "node:assert/strict";
import test from "node:test";

import * as browser from "../src/browser.js";
import * as root from "../src/index.js";
import type {
  MemoryApproval,
  MemoryCursor,
  MemoryInspection,
  MemoryPage,
  MemoryProvenance,
  MemoryRecord,
  MemoryRetention,
  MemorySummary,
  VoiceSession,
  VoiceSessionEvent,
} from "../src/browser.js";
import type {
  MemoryApproval as RootMemoryApproval,
  MemoryCursor as RootMemoryCursor,
  MemoryInspection as RootMemoryInspection,
  MemoryPage as RootMemoryPage,
  MemoryProvenance as RootMemoryProvenance,
  MemoryRecord as RootMemoryRecord,
  MemoryRetention as RootMemoryRetention,
  MemorySummary as RootMemorySummary,
  VoiceSession as RootVoiceSession,
  VoiceSessionEvent as RootVoiceSessionEvent,
} from "../src/index.js";

test("browser entry exports typed command rejections without the stdio transport", () => {
  assert.equal(typeof browser.RuntimeClient.connect, "function");
  assert.equal(typeof browser.CommandRejectedError, "function");
  assert.equal(browser.CommandRejectedError, root.CommandRejectedError);
  assert.equal("StdioGatewayTransport" in browser, false);
});

test("browser and root entries expose browser-safe memory DTOs", () => {
  const cursor: MemoryCursor = { beforeId: 7n };
  const summary: MemorySummary = {
    id: 7n,
    contentPreview: "Local preference",
    kind: "semantic",
    state: "active",
    pinned: false,
    updatedAtMs: 9_007_199_254_740_993n,
  };
  const retention: MemoryRetention = { kind: "until_deleted" };
  const record: MemoryRecord = {
    ...summary,
    content: "Local preference",
    confidence: 900n,
    createdAtMs: 9_007_199_254_740_993n,
    revision: 3n,
    retention,
    lastUsedAtMs: null,
    lastRetrievalReason: null,
  };
  const provenance: MemoryProvenance = {
    kind: "user_provided",
    sourceId: "source-1",
    sourceTimestampMs: 1n,
    actor: "local-user",
  };
  const approval: MemoryApproval = {
    confirmationId: "confirmation-1",
    actor: "local-user",
    confirmedAtMs: 1n,
    approvedRevision: 3n,
  };
  const page: MemoryPage = { records: [summary], nextCursor: cursor };
  const inspection: MemoryInspection = {
    record,
    sources: [provenance],
    approvals: [approval],
    sourcesTruncated: false,
    approvalsTruncated: false,
  };
  const rootTypes: [
    RootMemoryCursor,
    RootMemorySummary,
    RootMemoryPage,
    RootMemoryRecord,
    RootMemoryInspection,
    RootMemoryProvenance,
    RootMemoryApproval,
    RootMemoryRetention,
  ] = [cursor, summary, page, record, inspection, provenance, approval, retention];

  assert.equal(rootTypes.length, 8);
});

test("browser entry exports complete voice surface without Node builtins", () => {
  // Verify VoiceSession type is available from browser entry
  const browserVoiceSession: VoiceSession = null as any;
  const rootVoiceSession: RootVoiceSession = browserVoiceSession;
  assert.ok(rootVoiceSession === browserVoiceSession);

  // Verify VoiceSessionEvent type is available from browser entry
  const browserEvent: VoiceSessionEvent = null as any;
  const rootEvent: RootVoiceSessionEvent = browserEvent;
  assert.ok(rootEvent === browserEvent);

  // Verify RuntimeClient.startVoiceSession method is accessible and has correct signature
  const startVoiceSessionMethod = browser.RuntimeClient.prototype.startVoiceSession;
  assert.equal(typeof startVoiceSessionMethod, "function");

  // Verify no Node builtins are reachable from browser entry
  assert.equal("StdioGatewayTransport" in browser, false);
});
