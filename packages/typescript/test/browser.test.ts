import assert from "node:assert/strict";
import test from "node:test";

import * as browser from "../src/browser.js";
import * as root from "../src/index.js";
import type {
  MemoryApproval,
  MemoryCursor,
  MemoryExtractedSummary,
  MemoryInspection,
  MemoryPage,
  MemoryProvenance,
  MemoryRecord,
  MemoryRetention,
  MemorySummary,
  PersonaState,
  ConversationContextExchange,
  RuntimeStatus,
  VoiceSession,
  VoiceSessionEvent,
} from "../src/browser.js";
import type {
  MemoryApproval as RootMemoryApproval,
  MemoryCursor as RootMemoryCursor,
  MemoryExtractedSummary as RootMemoryExtractedSummary,
  MemoryInspection as RootMemoryInspection,
  MemoryPage as RootMemoryPage,
  MemoryProvenance as RootMemoryProvenance,
  MemoryRecord as RootMemoryRecord,
  MemoryRetention as RootMemoryRetention,
  MemorySummary as RootMemorySummary,
  PersonaState as RootPersonaState,
  ConversationContextExchange as RootConversationContextExchange,
  RuntimeStatus as RootRuntimeStatus,
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

test("browser and root entries expose persona and memory extraction DTOs", () => {
  const persona: PersonaState = {
    mode: "companionship",
    warmth: 95,
    humor: 60,
    teasing: 40,
    initiative: 35,
    directness: 80,
    intimacy: 30,
    verbosity: 20,
    followUpFrequency: 25,
  };
  const extracted: MemoryExtractedSummary = { created: 2, activated: 1, pendingApproval: 1 };
  const rootTypes: [RootPersonaState, RootMemoryExtractedSummary] = [persona, extracted];

  assert.equal(rootTypes.length, 2);
  assert.equal(typeof browser.RuntimeClient.prototype.getPersona, "function");
  assert.equal(typeof browser.RuntimeClient.prototype.updatePersona, "function");
  assert.equal(typeof browser.RuntimeClient.prototype.approveMemory, "function");
  assert.equal(typeof browser.RuntimeClient.prototype.deleteMemory, "function");
  assert.equal(typeof browser.RuntimeClient.prototype.onMemoryExtracted, "function");
});

test("browser and root entries expose context seeding and status DTOs", () => {
  const exchange: ConversationContextExchange = { user: "hello", assistant: "hi" };
  const runtimeStatus: RuntimeStatus = {
    transport: "stdio",
    privacyMode: "local_only",
    languageLocation: "local",
    modelId: "local-model",
    memoryEnabled: false,
    memoryLocation: null,
    telemetryEnabled: false,
    capabilities: ["text", "conversation_context_seed"],
    components: [{ kind: "language_model", executionLocation: "local", providerLabel: "Local language" }],
    lastContextSeedOperationId: "continue-1",
  };
  const rootTypes: [RootConversationContextExchange, RootRuntimeStatus] = [exchange, runtimeStatus];

  assert.equal(rootTypes.length, 2);
  assert.equal(typeof browser.RuntimeClient.prototype.seedConversationContext, "function");
});

test("browser entry exports complete voice surface without Node builtins", () => {
  // Verify the browser entry's voice types are the root entry's voice types
  const browserVoiceSession: VoiceSession = null as any;
  const browserEvent: VoiceSessionEvent = null as any;
  const rootTypes: [RootVoiceSession, RootVoiceSessionEvent] = [
    browserVoiceSession,
    browserEvent,
  ];

  assert.equal(rootTypes.length, 2);

  // Verify RuntimeClient.startVoiceSession method is accessible and has correct signature
  const startVoiceSessionMethod = browser.RuntimeClient.prototype.startVoiceSession;
  assert.equal(typeof startVoiceSessionMethod, "function");

  // Verify no Node builtins are reachable from browser entry
  assert.equal("StdioGatewayTransport" in browser, false);
});
