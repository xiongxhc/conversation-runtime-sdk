import { useEffect, useRef, useState } from "react";
import { CommandRejectedError } from "@conversation/runtime/browser";

import type { DesktopSession } from "../App.js";
import type {
  ConversationHistory,
  ConversationHistoryWrite,
  ConversationHistoryStore,
  ConversationSummary,
  HistoryRevision,
  PreparedContinuation,
} from "../history/conversation-history.js";
import { savePreferences, type Preferences, type StorageLike } from "../preferences/preferences.js";
import type {
  CarriedConversationContext,
  ConversationSessionState,
} from "../runtime/conversation-session.js";
import {
  disconnectedComponentStatus,
  PrivacyStatus,
  textOnlyComponentStatus,
  voiceComponentStatus,
} from "./PrivacyStatus.js";
import { ConversationVoiceStatus } from "./ConversationVoiceStatus.js";
import { MemoryPane } from "./MemoryPane.js";
import { SettingsPane } from "./SettingsPane.js";
import { VoiceExitDialog, type VoiceExitChoice } from "./VoiceExitDialog.js";
import { VoiceFocus } from "./VoiceFocus.js";
import {
  ConversationInstrument,
  type ConversationInstrumentTurn,
  type ConversationOperationFailure,
  type ConversationSendState,
} from "./workspace/ConversationInstrument.js";
import { RuntimeSignalPanel } from "./workspace/RuntimeSignalPanel.js";
import {
  WorkspaceNavigation,
  type DestinationAvailability,
  type WorkspaceDestination,
} from "./workspace/WorkspaceNavigation.js";

export interface WorkspaceProps {
  session: DesktopSession;
  historyStore: ConversationHistoryStore;
  initialPreferences: Preferences;
  storage: StorageLike;
  onClosed(setupError?: string): void;
}

type FocusMode = "live" | "preview";
type WorkspaceView = "conversation" | "history" | "memory" | "settings";
type VoiceControlFailure = { message: string; retry: () => Promise<void> };
type ActiveConversation = ConversationHistoryWrite & { revision?: HistoryRevision };
type HistoryReconciliationCheckpoint = {
  historyStore: ConversationHistoryStore;
  operationId: string | null;
};
type ContinuationPreview = {
  bytes: number;
  exchanges: { user: string; assistant: string }[];
};
type ContinuationSelection =
  | { preview: ContinuationPreview; error?: never }
  | { preview?: never; error: string };

const memoryExtractedNoticeTimeoutMs = 6_000;
const continuationExchangeLimit = 16;
const continuationByteLimit = 32_768;
const continuationMessageByteLimit = 16_384;

export function Workspace({
  session,
  historyStore,
  initialPreferences,
  storage,
  onClosed,
}: WorkspaceProps) {
  const [sessionState, setSessionState] = useState<ConversationSessionState>(session.state);
  const [message, setMessage] = useState("");
  const [preferences, setPreferences] = useState(initialPreferences);
  const preferencesRef = useRef(initialPreferences);
  const personaApplicationGeneration = useRef(0);
  const personaReplaySession = useRef<DesktopSession | undefined>(undefined);
  const [focusMode, setFocusMode] = useState<FocusMode>();
  const [workspaceView, setWorkspaceView] = useState<WorkspaceView>("conversation");
  const [history, setHistory] = useState<ConversationSummary[]>([]);
  const [selectedHistory, setSelectedHistory] = useState<ConversationHistory>();
  const [historyPath, setHistoryPath] = useState<string>();
  const [historyError, setHistoryError] = useState<string>();
  const [historyReconciliationCheckpoint, setHistoryReconciliationCheckpoint] =
    useState<HistoryReconciliationCheckpoint>();
  const [continuationError, setContinuationError] = useState<string>();
  const [continuationPreview, setContinuationPreview] = useState<ContinuationPreview>();
  const [continuationPending, setContinuationPending] = useState(false);
  const [continuationRecoveryError, setContinuationRecoveryError] = useState<string>();
  const [recoveredCarriedContext, setRecoveredCarriedContext] =
    useState<CarriedConversationContext>();
  const [operationError, setOperationError] = useState<string>();
  const [personaReplayNotice, setPersonaReplayNotice] = useState<string>();
  const [memoryExtractedNotice, setMemoryExtractedNotice] = useState<string>();
  const [newMemoryReviewCount, setNewMemoryReviewCount] = useState(0);
  const [memoryRefreshSignal, setMemoryRefreshSignal] = useState(0);
  const memoryExtractedNoticeTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const [voiceControlFailure, setVoiceControlFailure] = useState<VoiceControlFailure>();
  const [exitVoiceSessionId, setExitVoiceSessionId] = useState<bigint | null>();
  const [exitDialogError, setExitDialogError] = useState<string>();
  const [exitBusy, setExitBusy] = useState(false);
  const currentConversation = useRef<ActiveConversation | undefined>(undefined);
  const historyTurnOffset = useRef(0);
  const lastPersistedState = useRef("");
  const latestSessionState = useRef(session.state);
  const historyWrite = useRef(Promise.resolve());
  const focusReturn = useRef<HTMLButtonElement>(null);
  const restoreFocusOnWorkspace = useRef(false);
  const voiceControlOperation = useRef(0);
  const typedResume = useRef<{ sessionId: bigint; turnCount: number } | undefined>(undefined);
  const pausedForComposerSession = useRef<bigint | undefined>(undefined);
  const composerFocused = useRef(false);
  const focusComposerAfterContinuation = useRef(false);
  const runtimeHealthy = sessionState.phase === "ready" || sessionState.phase === "streaming";
  const personaAvailable = sessionState.status.capabilities.includes("persona_control");
  const memoryAvailable =
    sessionState.status.memoryEnabled &&
    sessionState.status.memoryLocation === "local" &&
    sessionState.status.capabilities.includes("memory_inspection");
  const voiceConfigured = sessionState.voice.availability === "configured";
  const voiceRunning = sessionState.voice.session !== "idle" && (
    sessionState.voice.session !== "error" || sessionState.voice.sessionId !== undefined
  );
  const runtimeFailed = sessionState.phase === "failed" || sessionState.phase === "closed";
  const continuationAvailable = sessionState.status.capabilities.includes(
    "conversation_context_seed",
  );
  const lastSeedOperationId = sessionState.status.lastContextSeedOperationId ?? null;
  const historyReconciliationPending =
    historyReconciliationCheckpoint?.historyStore !== historyStore
    || historyReconciliationCheckpoint?.operationId !== lastSeedOperationId;
  const historyReconciliationPendingRef = useRef(historyReconciliationPending);
  historyReconciliationPendingRef.current = historyReconciliationPending;
  const currentContinuationUnavailableReason = historyReconciliationPending
    ? "Wait for Session recovery to finish before continuing a Session."
    : continuationReason(
      sessionState,
      continuationAvailable,
      operationError,
      false,
    );
  const runtimeNeedsAttention = runtimeFailed || operationError !== undefined;
  const runtimeControlsUnavailable =
    sessionState.phase !== "ready"
    || voiceRunning
    || operationError !== undefined
    || continuationPending
    || historyReconciliationPending
    || sessionState.continuation.inProgress;
  const memoryNavigationGuidance = historyReconciliationPending
    ? "Wait for Session recovery to finish before opening Memory review."
    : operationError
    ? "Reconnect local runtime before opening Memory review."
    : voiceRunning
      ? "Stop voice before opening Memory review."
      : sessionState.phase === "streaming"
        ? "Finish or stop the active response before opening Memory review."
        : sessionState.phase === "failed" || sessionState.phase === "closed"
          ? "Reconnect local runtime before opening Memory review."
          : undefined;
  const settingsNavigationGuidance = historyReconciliationPending
    ? "Wait for Session recovery to finish before changing how it responds."
    : operationError
    ? "Reconnect local runtime before opening How it responds."
    : voiceRunning
      ? "Stop voice before opening How it responds."
      : sessionState.phase === "streaming"
        ? "Finish or stop the active response before opening How it responds."
        : sessionState.phase === "failed" || sessionState.phase === "closed"
          ? "Reconnect local runtime before opening How it responds."
          : undefined;
  const exitChoiceVisible = exitVoiceSessionId !== undefined;
  const canRenderLiveFocus =
    focusMode === "live"
    && runtimeHealthy
    && voiceConfigured
    && operationError === undefined
    && !historyReconciliationPending;
  const components = voiceConfigured
    ? voiceComponentStatus(sessionState.status)
    : textOnlyComponentStatus(sessionState.status);

  useEffect(() => session.subscribe((state) => {
    latestSessionState.current = state;
    setSessionState(state);
    if (state.continuation.carriedContext) {
      setRecoveredCarriedContext(state.continuation.carriedContext);
    }
    persistConversation(state);
  }), [session, historyStore]);
  useEffect(() => {
    const unsubscribe = session.onMemoryExtracted((summary) => {
      const message = `${summary.created} memories saved${
        summary.pendingApproval > 0 ? ` · ${summary.pendingApproval} awaiting approval` : ""
      }`;
      setMemoryExtractedNotice(message);
      setNewMemoryReviewCount((value) => value + summary.pendingApproval);
      if (memoryExtractedNoticeTimer.current !== undefined) {
        clearTimeout(memoryExtractedNoticeTimer.current);
      }
      memoryExtractedNoticeTimer.current = setTimeout(
        () => setMemoryExtractedNotice(undefined),
        memoryExtractedNoticeTimeoutMs,
      );
      setMemoryRefreshSignal((value) => value + 1);
    });
    return () => {
      unsubscribe();
      if (memoryExtractedNoticeTimer.current !== undefined) {
        clearTimeout(memoryExtractedNoticeTimer.current);
      }
    };
  }, [session]);
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const [path, conversations] = await Promise.all([
          historyStore.storagePath(),
          historyStore.list(),
        ]);
        if (cancelled) return;
        setHistoryPath(path);
        const reconciled = await reconcilePreparingHistory(
          historyStore,
          conversations,
          lastSeedOperationId,
        );
        if (!cancelled) {
          setHistory((current) => mergeHistory(reconciled.conversations, current));
          if (reconciled.activeBranch) {
            activateContinuationBranch(reconciled.activeBranch, {
              confirmationPending: reconciled.confirmationPending,
              focusComposer: false,
            });
          }
        }
      } catch {
        if (!cancelled) setHistoryError("Local history is unavailable in this app session.");
      } finally {
        if (!cancelled) {
          setHistoryReconciliationCheckpoint({
            historyStore,
            operationId: lastSeedOperationId,
          });
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [historyStore, lastSeedOperationId]);
  useEffect(() => {
    if (continuationPreview && currentContinuationUnavailableReason) {
      setContinuationPreview(undefined);
      setContinuationError(undefined);
    }
  }, [continuationPreview, currentContinuationUnavailableReason]);
  useEffect(() => {
    if (workspaceView === "conversation" && focusComposerAfterContinuation.current) {
      focusComposerAfterContinuation.current = false;
      document.getElementById("conversation-message")?.focus();
    }
  }, [workspaceView]);
  useEffect(() => {
    if (!focusMode && restoreFocusOnWorkspace.current) {
      restoreFocusOnWorkspace.current = false;
      focusReturn.current?.focus();
    }
  }, [focusMode]);
  useEffect(() => {
    if (
      (workspaceView === "memory" && !memoryAvailable)
      || (workspaceView === "settings" && !personaAvailable)
      || ((workspaceView === "memory" || workspaceView === "settings") && runtimeControlsUnavailable)
    ) {
      setWorkspaceView("conversation");
    }
  }, [memoryAvailable, personaAvailable, runtimeControlsUnavailable, workspaceView]);
  useEffect(() => {
    if (historyReconciliationPending || personaReplaySession.current === session) return;
    personaReplaySession.current = session;
    // Replay fires once per connected session, after history ownership is settled.
    const replayPreferences = preferencesRef.current;
    const replayPersonaApplicationGeneration = personaApplicationGeneration.current;
    const activePreset = replayPreferences.personaPresets.find(
      (preset) => preset.name === replayPreferences.activePresetName,
    );
    if (!activePreset || !personaAvailable) return;
    void session.updatePersona(activePreset.persona).catch(() => {
      const currentPreferences = preferencesRef.current;
      if (
        personaApplicationGeneration.current === replayPersonaApplicationGeneration
        && currentPreferences.activePresetName === activePreset.name
      ) {
        updatePreferences({ ...currentPreferences, activePresetName: null });
        setPersonaReplayNotice(
          `The "${activePreset.name}" persona preset could not be applied. Open How it responds to reapply it.`,
        );
      }
    });
  }, [historyReconciliationPending, session]);
  useEffect(() => {
    const pending = typedResume.current;
    if (sessionState.voice.session !== "active") {
      typedResume.current = undefined;
    }
    if (pending && sessionState.voice.sessionId !== pending.sessionId) {
      typedResume.current = undefined;
      return;
    }
    if (
      pending &&
      sessionState.phase === "ready" &&
      sessionState.turns.length > pending.turnCount &&
      sessionState.voice.session === "active" &&
      sessionState.voice.sessionId === pending.sessionId &&
      sessionState.voice.capture === "paused" &&
      pausedForComposerSession.current === pending.sessionId
    ) {
      typedResume.current = undefined;
      pausedForComposerSession.current = undefined;
      runVoiceControl(
        () => session.resumeVoiceCapture(),
        "Microphone resume failed. Retry or stop voice before continuing.",
      );
    }
  }, [sessionState.phase, sessionState.turns.length, sessionState.voice.capture, sessionState.voice.session, sessionState.voice.sessionId]);
  useEffect(() => {
    if (
      !composerFocused.current &&
      message.trim() === "" &&
      sessionState.phase === "ready" &&
      sessionState.voice.session === "active" &&
      sessionState.voice.capture === "paused" &&
      sessionState.voice.sessionId !== undefined &&
      pausedForComposerSession.current === sessionState.voice.sessionId &&
      typedResume.current === undefined
    ) {
      pausedForComposerSession.current = undefined;
      runVoiceControl(
        () => session.resumeVoiceCapture(),
        "Microphone resume failed. Retry or stop voice before continuing.",
      );
    }
  }, [message, sessionState.phase, sessionState.voice.capture, sessionState.voice.session, sessionState.voice.sessionId]);
  useEffect(() => {
    const ownedSessionId = pausedForComposerSession.current;
    if (ownedSessionId !== undefined && sessionState.voice.sessionId !== ownedSessionId) {
      pausedForComposerSession.current = undefined;
    }
    if (
      composerFocused.current &&
      sessionState.voice.session === "active" &&
      sessionState.voice.capture === "listening" &&
      sessionState.voice.sessionId !== undefined &&
      pausedForComposerSession.current !== sessionState.voice.sessionId
    ) {
      requestComposerPause();
    }
  }, [sessionState.voice.capture, sessionState.voice.session, sessionState.voice.sessionId]);
  useEffect(() => {
    if (exitVoiceSessionId === undefined) return;
    const currentSessionId = sessionState.voice.sessionId ?? null;
    if (!voiceRunning || currentSessionId !== exitVoiceSessionId) {
      setExitVoiceSessionId(undefined);
      setExitDialogError(undefined);
    }
  }, [exitVoiceSessionId, sessionState.voice.sessionId, voiceRunning]);

  const updatePreferences = (nextPreferences: Preferences) => {
    preferencesRef.current = nextPreferences;
    setPreferences(nextPreferences);
    savePreferences(storage, nextPreferences);
  };

  const persistConversation = (state: ConversationSessionState) => {
    const turns = state.turns.slice(historyTurnOffset.current);
    const lastTurn = turns.at(-1);
    if (!lastTurn) return;
    const persistenceState = `${historyTurnOffset.current}:${state.turns.length}:${lastTurn.state}:${state.phase}`;
    if (persistenceState === lastPersistedState.current) return;
    lastPersistedState.current = persistenceState;

    const now = Date.now();
    const existing = currentConversation.current;
    const carriedTurns = existing?.turns.filter((turn) => turn.origin === "continued_context") ?? [];
    const conversation: ActiveConversation = {
      id: existing?.id ?? createConversationId(now),
      title: existing?.title ?? conversationTitle(turns[0].transcript),
      createdAtMs: existing?.createdAtMs ?? now,
      updatedAtMs: now,
      revision: existing?.revision,
      continuedFromId: existing?.continuedFromId ?? null,
      continuationOperationId: existing?.continuationOperationId ?? null,
      continuationState: existing?.continuationState ?? null,
      turns: [
        ...carriedTurns,
        ...turns.map((turn) => ({
          turnId: turn.turnId.toString(),
          transcript: turn.transcript,
          response: turn.response,
          state: turn.state,
          failureMessage: turn.failure?.message ?? null,
          origin: "live" as const,
        })),
      ],
    };
    currentConversation.current = conversation;
    historyWrite.current = historyWrite.current.then(async () => {
      const active = currentConversation.current;
      const expectedRevision = active?.id === conversation.id
        ? active.revision
        : conversation.revision;
      const { revision: _revision, ...write } = conversation;
      const result = await historyStore.save(write, expectedRevision);
      const saved: ConversationHistory = { ...write, revision: result.revision };
      if (currentConversation.current?.id === conversation.id) {
        currentConversation.current = {
          ...currentConversation.current,
          revision: result.revision,
        };
      }
      setHistory((current) => [
        summaryOf(saved),
        ...current.filter((item) => item.id !== conversation.id),
      ]);
    }).catch(() => {
      setHistoryError("This conversation could not be saved to local history.");
    });
  };

  const showHistory = async () => {
    setWorkspaceView("history");
    setSelectedHistory(undefined);
    setHistoryError(undefined);
    try {
      await historyWrite.current;
      setHistory(await historyStore.list());
    } catch {
      setHistoryError("Local history is unavailable in this app session.");
    }
  };

  const showMemory = () => {
    setNewMemoryReviewCount(0);
    setWorkspaceView("memory");
  };

  const openHistory = async (id: string) => {
    setHistoryError(undefined);
    setContinuationError(undefined);
    setContinuationPreview(undefined);
    try {
      const conversation = await historyStore.get(id);
      if (!conversation) {
        setHistoryError("That saved conversation no longer exists.");
        setHistory((current) => current.filter((item) => item.id !== id));
        return;
      }
      setSelectedHistory(conversation);
    } catch {
      setHistoryError("That saved conversation could not be opened.");
    }
  };

  const deleteHistory = async (id: string, revision: HistoryRevision) => {
    setHistoryError(undefined);
    try {
      await historyWrite.current;
      const currentRevision = currentConversation.current?.id === id
        ? currentConversation.current.revision ?? revision
        : revision;
      await historyStore.delete(id, currentRevision);
      if (currentConversation.current?.id === id) {
        historyTurnOffset.current = latestSessionState.current.turns.length;
        currentConversation.current = undefined;
        lastPersistedState.current = "";
        setRecoveredCarriedContext(undefined);
        setContinuationRecoveryError(undefined);
      }
      setHistory((current) => current.filter((item) => item.id !== id));
      setSelectedHistory((current) => current?.id === id ? undefined : current);
    } catch (error) {
      setHistoryError(deleteHistoryError(error));
      throw error;
    }
  };

  const requestContinuation = () => {
    if (!selectedHistory || historyReconciliationPendingRef.current) return;
    setContinuationError(undefined);
    const selection = selectContinuationPreview(selectedHistory);
    if (selection.error) {
      setContinuationPreview(undefined);
      setContinuationError(selection.error);
      return;
    }
    setContinuationPreview(selection.preview);
  };

  const activateContinuationBranch = (
    branch: ConversationHistory,
    options: { confirmationPending: boolean; focusComposer: boolean },
  ) => {
    currentConversation.current = branch;
    historyTurnOffset.current = latestSessionState.current.turns.length;
    lastPersistedState.current = "";
    setRecoveredCarriedContext(carriedContextOf(branch));
    setContinuationRecoveryError(options.confirmationPending
      ? "The runtime accepted the carried context, but local Session confirmation is still pending. The new branch is active and will be reconciled on reconnect."
      : undefined);
    setHistory((current) => [
      summaryOf(branch),
      ...current.filter((item) => item.id !== branch.id),
    ]);
    setSelectedHistory(undefined);
    setContinuationPreview(undefined);
    if (options.focusComposer) {
      focusComposerAfterContinuation.current = true;
    }
    setWorkspaceView("conversation");
  };

  const confirmContinuation = async () => {
    const source = selectedHistory;
    const preview = continuationPreview;
    if (
      !source
      || !preview
      || continuationPending
      || historyReconciliationPendingRef.current
    ) return;
    setContinuationPending(true);
    setContinuationError(undefined);
    setContinuationRecoveryError(undefined);
    let prepared: PreparedContinuation;
    try {
      await historyWrite.current;
      if (historyReconciliationPendingRef.current) return;
      const latestState = latestSessionState.current;
      const preflightReason = continuationReason(
        latestState,
        latestState.status.capabilities.includes("conversation_context_seed"),
        operationError,
        false,
      );
      if (preflightReason) {
        setContinuationPreview(undefined);
        setContinuationError(undefined);
        return;
      }
      try {
        prepared = await historyStore.prepareContinuation(source.id, source.revision);
      } catch (error) {
        setContinuationError(preparationError(error));
        return;
      }
      const canonicalBytes = contextBytes(prepared.seed);
      try {
        await session.continueWithSeed({
          sourceId: source.id,
          sourceTitle: source.title,
          operationId: prepared.operationId,
          exchanges: prepared.seed,
          bytes: canonicalBytes,
        });
      } catch (error) {
        if (isDefiniteContinuationRejection(error)) {
          try {
            await historyStore.delete(prepared.branch.id, prepared.branch.revision);
            setHistory((current) => current.filter((item) => item.id !== prepared.branch.id));
          } catch {
            // Startup reconciliation will preserve and classify any branch whose cleanup was uncertain.
          }
          setContinuationError(
            "The new conversation could not be started. Your current conversation and saved Session were not changed.",
          );
          return;
        }
        let branch = prepared.branch;
        try {
          const result = await historyStore.setContinuationState(
            branch.id,
            branch.revision,
            "unconfirmed",
          );
          branch = { ...branch, continuationState: "unconfirmed", revision: result.revision };
        } catch {
          // Retain the preparing copy; a later startup can reconcile it from the operation ID.
        }
        setHistory((current) => [
          summaryOf(branch),
          ...current.filter((item) => item.id !== branch.id),
        ]);
        setContinuationError(
          "The runtime connection ended before continuation could be confirmed. A local continuation copy was saved; open it after reconnect to try again.",
        );
        return;
      }

      activateContinuationBranch(prepared.branch, {
        confirmationPending: true,
        focusComposer: true,
      });
      const confirmation = await confirmContinuationState(historyStore, prepared.branch);
      activateContinuationBranch(confirmation.branch, {
        confirmationPending: !confirmation.verified,
        focusComposer: false,
      });
    } finally {
      setContinuationPending(false);
    }
  };

  const exitFocus = () => {
    restoreFocusOnWorkspace.current = true;
    setExitVoiceSessionId(undefined);
    setExitDialogError(undefined);
    setFocusMode(undefined);
  };

  const runVoiceControl = (
    control: () => Promise<void>,
    errorMessage: string,
  ) => {
    if (historyReconciliationPendingRef.current) return;
    const operation = ++voiceControlOperation.current;
    setVoiceControlFailure(undefined);
    void control().then(
      () => {
        if (voiceControlOperation.current === operation) {
          setVoiceControlFailure(undefined);
        }
      },
      () => {
        if (voiceControlOperation.current === operation) {
          setVoiceControlFailure({ message: errorMessage, retry: control });
        }
      },
    );
  };

  const startVoice = () => runVoiceControl(
    () => session.startVoice(),
    "Voice could not start. Retry or review the local microphone configuration.",
  );

  const stopVoice = () => {
    typedResume.current = undefined;
    pausedForComposerSession.current = undefined;
    runVoiceControl(
      () => session.stopVoice(),
      "Voice could not stop cleanly. Retry before closing the runtime.",
    );
  };

  const requestComposerPause = () => {
    const sessionId = sessionState.voice.sessionId;
    if (sessionId === undefined) return;
    pausedForComposerSession.current = sessionId;
    runVoiceControl(
      () => session.pauseVoiceCapture(),
      "Microphone pause failed. Retry or stop voice before typing.",
    );
  };

  const requestExitFocus = () => {
    if (voiceRunning) {
      setExitVoiceSessionId(sessionState.voice.sessionId ?? null);
      setExitDialogError(undefined);
    } else {
      exitFocus();
    }
  };

  const chooseVoiceExit = async (choice: VoiceExitChoice) => {
    if (choice === "cancel") {
      setExitVoiceSessionId(undefined);
      setExitDialogError(undefined);
      return;
    }
    if (choice === "keep") {
      exitFocus();
      return;
    }
    if (historyReconciliationPendingRef.current) return;
    const currentSessionId = sessionState.voice.sessionId ?? null;
    if (!voiceRunning || currentSessionId !== exitVoiceSessionId) {
      setExitVoiceSessionId(undefined);
      setExitDialogError(undefined);
      return;
    }
    typedResume.current = undefined;
    pausedForComposerSession.current = undefined;
    setExitBusy(true);
    try {
      await session.stopVoice();
      exitFocus();
    } catch {
      setExitDialogError("Voice could not stop cleanly. Retry or keep voice active.");
    } finally {
      setExitBusy(false);
    }
  };

  useEffect(() => {
    if (
      focusMode
      && (
        historyReconciliationPending
        ||
        !runtimeHealthy
        || (focusMode === "live" && (!voiceConfigured || operationError !== undefined))
      )
    ) {
      exitFocus();
    }
  }, [focusMode, historyReconciliationPending, operationError, runtimeHealthy, voiceConfigured]);

  const submit = () => {
    const transcript = message.trim();
    const voiceNeedsPause = voiceRunning && sessionState.voice.capture !== "paused";
    if (
      !transcript
      || sessionState.phase !== "ready"
      || voiceNeedsPause
      || operationError !== undefined
      || continuationPending
      || historyReconciliationPendingRef.current
      || sessionState.continuation.inProgress
    ) return;
    setOperationError(undefined);
    if (sessionState.voice.sessionId !== undefined && sessionState.voice.capture === "paused") {
      typedResume.current = {
        sessionId: sessionState.voice.sessionId,
        turnCount: sessionState.turns.length,
      };
    }
    void Promise.resolve(session.send(transcript)).catch(() => undefined);
    setMessage("");
  };

  const focusComposer = () => {
    composerFocused.current = true;
    if (
      !historyReconciliationPendingRef.current
      && sessionState.voice.capture === "listening"
    ) {
      requestComposerPause();
    }
  };

  const blurComposer = () => {
    composerFocused.current = false;
    if (
      message.trim() === "" &&
      sessionState.phase === "ready" &&
      sessionState.voice.session === "active" &&
      sessionState.voice.capture === "paused" &&
      pausedForComposerSession.current === sessionState.voice.sessionId &&
      typedResume.current === undefined
    ) {
      pausedForComposerSession.current = undefined;
      runVoiceControl(
        () => session.resumeVoiceCapture(),
        "Microphone resume failed. Retry or stop voice before continuing.",
      );
    }
  };

  const close = async () => {
    if (historyReconciliationPendingRef.current) return;
    setOperationError(undefined);
    try {
      await session.close();
      onClosed();
    } catch {
      setWorkspaceView("conversation");
      setOperationError(
        "Could not close the runtime cleanly. Return to setup and verify the previous process stopped before reconnecting.",
      );
    }
  };

  const stop = async () => {
    setOperationError(undefined);
    try {
      await session.interrupt();
    } catch {
      setOperationError(
        "Could not stop the response. The runtime may be disconnected. Reconnect before sending another message.",
      );
    }
  };

  const reconnect = async () => {
    if (historyReconciliationPendingRef.current) return;
    let closeFailed = false;
    try {
      await session.close();
    } catch {
      closeFailed = true;
    }
    onClosed(
      closeFailed
        ? "The previous runtime could not be closed cleanly. Verify that process stopped, then reconnect."
        : "Review the preserved setup paths, then reconnect the local runtime.",
    );
  };
  const returnToSetup = () => {
    if (historyReconciliationPendingRef.current) return;
    onClosed(operationError ?? "Runtime disconnected");
  };
  const memoryExtractionAnnouncement = memoryExtractedNotice ? (
    <p
      className="memory-extraction-announcement"
      data-memory-extraction-announcement=""
      role="status"
    >
      {memoryExtractedNotice}
    </p>
  ) : null;

  if (canRenderLiveFocus) {
    return (
      <>
        {memoryExtractionAnnouncement}
        <VoiceFocus
          capture={sessionState.voice.capture}
          components={components}
          devices={sessionState.voice.devices}
          lastHeardTranscript={sessionState.voice.lastHeardTranscript}
          controlError={voiceControlFailure?.message}
          mode="live"
          onExit={requestExitFocus}
          onPreferencesChange={updatePreferences}
          onRetryControl={voiceControlFailure
            ? () => runVoiceControl(
              voiceControlFailure.retry,
              voiceControlFailure.message,
            )
            : undefined}
          onStart={startVoice}
          onStop={stopVoice}
          preferences={preferences}
          reducedMotion={prefersReducedMotion()}
          runtimeError={sessionState.voice.error?.message}
          session={sessionState.voice.session}
          state={sessionState.voice.visual}
          suspended={exitChoiceVisible}
          transcript={sessionState.voice.partialTranscript || sessionState.activeTurn?.transcript || ""}
        />
        {exitChoiceVisible ? (
          <VoiceExitDialog
            busy={exitBusy}
            error={exitDialogError}
            onChoose={(choice) => void chooseVoiceExit(choice)}
          />
        ) : null}
      </>
    );
  }

  if (focusMode === "preview" && runtimeHealthy && !historyReconciliationPending) {
    return (
      <>
        {memoryExtractionAnnouncement}
        <VoiceFocus
          capture="stopped"
          components={textOnlyComponentStatus(sessionState.status)}
          mode="preview"
          onExit={exitFocus}
          onPreferencesChange={updatePreferences}
          onStart={() => undefined}
          onStop={() => undefined}
          preferences={preferences}
          reducedMotion={prefersReducedMotion()}
          session="idle"
          state="idle"
          suspended={false}
          transcript=""
        />
      </>
    );
  }

  const activeDestination: WorkspaceDestination = workspaceView === "history"
    ? "sessions"
    : workspaceView === "settings"
      ? "response"
      : workspaceView;
  const memoryAvailabilityReason = memoryAvailable
    ? memoryNavigationGuidance
    : !sessionState.status.memoryEnabled
      ? "Memory review is unavailable because memory is off."
      : !sessionState.status.capabilities.includes("memory_inspection")
        ? "Memory review is unavailable because local memory inspection is not supported by this runtime."
        : "Memory review is unavailable because local memory was not verified.";
  const responseAvailabilityReason = personaAvailable
    ? settingsNavigationGuidance
    : "How it responds is unavailable because response controls are not supported by this runtime.";
  const destinationAvailability: Record<WorkspaceDestination, DestinationAvailability> = {
    conversation: { enabled: true },
    sessions: { enabled: true },
    memory: {
      badge: newMemoryReviewCount > 0 ? `${newMemoryReviewCount} new` : undefined,
      enabled: memoryAvailable && !runtimeControlsUnavailable,
      reason: memoryAvailabilityReason,
    },
    response: {
      enabled: personaAvailable && !runtimeControlsUnavailable,
      reason: responseAvailabilityReason,
    },
  };
  const conversationTurns: ConversationInstrumentTurn[] = sessionState.turns.map((turn) => {
    if (turn.state === "failed") {
      return {
        failure: {
          message: turn.failure?.message ?? "This response could not be completed.",
          retry: sessionState.phase === "ready"
            ? { enabled: true, onInvoke: () => setMessage(turn.transcript) }
            : { enabled: false, reason: "Reconnect local runtime before trying this turn again." },
        },
        id: turn.turnId.toString(),
        response: turn.response,
        state: "failed",
        transcript: turn.transcript,
      };
    }
    return {
      id: turn.turnId.toString(),
      response: turn.response,
      state: turn.state,
      transcript: turn.transcript,
    };
  });
  const sendState: ConversationSendState = historyReconciliationPending
    ? {
      enabled: false,
      reason: "Wait for Session recovery to finish before sending another message.",
    }
    : operationError
    ? {
      enabled: false,
      reason: "Reconnect local runtime before sending another message.",
    }
    : continuationPending || sessionState.continuation.inProgress
      ? {
        enabled: false,
        reason: "Wait for Session continuation to finish before sending another message.",
      }
      : sessionState.phase !== "ready"
      ? {
        enabled: false,
        reason: sessionState.phase === "streaming"
          ? "Finish or stop the active response before sending another message."
          : "Reconnect local runtime before sending another message.",
      }
      : voiceRunning && sessionState.voice.capture !== "paused"
        ? {
          enabled: false,
          reason: sessionState.voice.capture === "pausing"
            || pausedForComposerSession.current === sessionState.voice.sessionId
            ? "Voice is pausing before you type."
            : "Focus the message field to pause voice before sending.",
        }
        : { enabled: true };
  const operationFailureMessage = operationError ?? "Runtime disconnected";
  const failedConversationOperation: ConversationOperationFailure = {
    message: operationFailureMessage,
    recovery: historyReconciliationPending
      ? {
        reconnect: {
          enabled: false,
          reason: "Wait for Session recovery to finish before reconnecting the runtime.",
        },
        returnToSetup: {
          enabled: false,
          reason: "Wait for Session recovery to finish before returning to setup.",
        },
      }
      : {
        reconnect: { enabled: true, onInvoke: () => void reconnect() },
        returnToSetup: { enabled: true, onInvoke: returnToSetup },
      },
  };
  const operationFailure = runtimeNeedsAttention ? failedConversationOperation : undefined;
  const memorySignalState = runtimeFailed
    ? "error" as const
    : memoryAvailable
      ? "verified" as const
      : "unavailable" as const;
  const voiceSignalState = runtimeFailed || sessionState.voice.session === "error"
    ? "error" as const
    : voiceConfigured
      ? "verified" as const
      : "unavailable" as const;

  const selectDestination = (destination: WorkspaceDestination) => {
    switch (destination) {
      case "sessions":
        void showHistory();
        break;
      case "memory":
        showMemory();
        break;
      case "response":
        setWorkspaceView("settings");
        break;
      default:
        setWorkspaceView("conversation");
    }
  };

  return (
    <>
      {memoryExtractionAnnouncement}
      <main className="workspace-shell">
      <WorkspaceNavigation
        activeDestination={activeDestination}
        availability={destinationAvailability}
        onSelect={selectDestination}
      />

      {workspaceView === "history" ? (
        <HistoryPane
          continuationAvailable={continuationAvailable}
          continuationError={continuationError}
          continuationPending={continuationPending || sessionState.continuation.inProgress}
          continuationPreview={continuationPreview}
          continuationUnavailableReason={historyReconciliationPending
            ? "Wait for Session recovery to finish before continuing a Session."
            : continuationReason(
              sessionState,
              continuationAvailable,
              operationError,
              continuationPending,
            )}
          conversations={history}
          error={historyError}
          onBack={() => {
            setSelectedHistory(undefined);
            setContinuationError(undefined);
            setContinuationPreview(undefined);
          }}
          onCancelContinuation={() => {
            setContinuationError(undefined);
            setContinuationPreview(undefined);
          }}
          onConfirmContinuation={() => void confirmContinuation()}
          onDelete={deleteHistory}
          onOpen={(id) => void openHistory(id)}
          onRequestContinuation={requestContinuation}
          selected={selectedHistory}
          storagePath={historyPath}
        />
      ) : workspaceView === "memory" ? (
        <MemoryPane
          onBack={() => setWorkspaceView("conversation")}
          refreshSignal={memoryRefreshSignal}
          session={session}
          status={sessionState.status}
        />
      ) : workspaceView === "settings" && !historyReconciliationPending ? (
        <SettingsPane
          onBack={() => setWorkspaceView("conversation")}
          onPersonaApplied={() => {
            personaApplicationGeneration.current += 1;
          }}
          onPreferencesChange={updatePreferences}
          preferences={preferences}
          session={session}
        />
      ) : (
        <div className="conversation-stack">
        {continuationRecoveryError ? (
          <div className="workspace-error continuation-recovery-error" role="alert">
            <p>{continuationRecoveryError}</p>
          </div>
        ) : null}
        {recoveredCarriedContext ?? sessionState.continuation.carriedContext ? (
          <CarriedContextSection
            context={recoveredCarriedContext ?? sessionState.continuation.carriedContext!}
          />
        ) : null}
        {runtimeFailed ? (
          <ConversationInstrument
            composer={{
              value: message,
              voicePausePending: voiceRunning && sessionState.voice.capture === "pausing",
              voicePaused: voiceRunning
                && sessionState.voice.capture === "paused"
                && pausedForComposerSession.current === sessionState.voice.sessionId,
            }}
            onComposerBlur={blurComposer}
            onComposerChange={setMessage}
            onComposerFocus={focusComposer}
            onSend={submit}
            onStop={() => void stop()}
            operationFailure={failedConversationOperation}
            send={sendState}
            state={sessionState.phase === "failed" ? "failed" : "closed"}
            turns={conversationTurns}
          />
        ) : (
          <ConversationInstrument
            composer={{
              value: message,
              voicePausePending: voiceRunning && sessionState.voice.capture === "pausing",
              voicePaused: voiceRunning
                && sessionState.voice.capture === "paused"
                && pausedForComposerSession.current === sessionState.voice.sessionId,
            }}
            onComposerBlur={blurComposer}
            onComposerChange={setMessage}
            onComposerFocus={focusComposer}
            onSend={submit}
            onStop={() => void stop()}
            operationFailure={operationFailure}
            send={sendState}
            state={sessionState.phase === "streaming" ? "streaming" : "ready"}
            turns={conversationTurns}
          />
        )
        }
        </div>
      )}

      <aside className="workspace-signal-column" aria-label="Runtime status">
        {personaReplayNotice ? (
          <p className="workspace-notice" role="status">{personaReplayNotice}</p>
        ) : null}
        <RuntimeSignalPanel
          actions={{
            connection: historyReconciliationPending
              ? {
                enabled: false,
                label: runtimeNeedsAttention
                  ? "Reconnect local runtime"
                  : "Disconnect local runtime",
                reason: "Wait for Session recovery to finish before changing the runtime connection.",
              }
              : continuationPending || sessionState.continuation.inProgress
              ? {
                enabled: false,
                label: "Disconnect local runtime",
                reason: "Wait for Session continuation to finish before disconnecting.",
              }
              : workspaceView === "conversation" && operationFailure
              ? undefined
              : runtimeNeedsAttention
                ? {
                  enabled: true,
                  label: "Reconnect local runtime",
                  onInvoke: () => void reconnect(),
                }
                : {
                  enabled: true,
                  label: "Disconnect local runtime",
                  onInvoke: () => void close(),
                },
            voice: historyReconciliationPending
              ? {
                buttonRef: focusReturn,
                enabled: false,
                label: voiceConfigured ? "Voice Focus" : "Preview Voice Focus",
                reason: "Wait for Session recovery to finish before starting Voice.",
              }
              : continuationPending || sessionState.continuation.inProgress
              ? {
                buttonRef: focusReturn,
                enabled: false,
                label: voiceConfigured ? "Voice Focus" : "Preview Voice Focus",
                reason: "Wait for Session continuation to finish before starting Voice.",
              }
              : runtimeHealthy && (!voiceConfigured || operationError === undefined)
              ? {
                buttonRef: focusReturn,
                enabled: true,
                label: voiceConfigured ? "Voice Focus" : "Preview Voice Focus",
                onInvoke: () => setFocusMode(voiceConfigured ? "live" : "preview"),
              }
              : {
                buttonRef: focusReturn,
                enabled: false,
                label: voiceConfigured ? "Voice Focus" : "Preview Voice Focus",
                reason: "Reconnect local runtime before opening Voice Focus.",
              },
          }}
          connectionLabel={runtimeNeedsAttention ? "Needs attention" : "Connected to this Mac"}
          locality={{
            memory: { state: memorySignalState },
            model: { state: runtimeFailed ? "error" : "verified" },
            runtime: { state: runtimeNeedsAttention ? "error" : "verified" },
            voice: {
              detail: sessionState.voice.error?.message,
              state: voiceSignalState,
            },
          }}
          memory={{
            label: "Memory",
            state: memorySignalState,
            value: memoryAvailable
              ? "Available locally"
              : sessionState.status.memoryEnabled
                ? "Unavailable"
                : "Memory off",
          }}
          model={{
            label: "Model",
            state: runtimeFailed ? "error" : "verified",
            value: sessionState.status.modelId,
          }}
          voice={{
            label: "Voice",
            state: voiceSignalState,
            value: runtimeFailed ? "Needs attention" : voiceSignalLabel(sessionState.voice),
          }}
        />
        {!voiceConfigured && runtimeHealthy ? (
          <p className="voice-preview-disclosure">
            Microphone and speech playback are not connected in this desktop build. Preview Voice Focus is intentionally text-only.
          </p>
        ) : null}
        {voiceConfigured && runtimeHealthy && !voiceRunning ? (
          <p className="voice-preview-disclosure">
            Open Voice Focus, then start the local microphone when you are ready.
          </p>
        ) : null}
        {voiceRunning && focusMode !== "live" ? (
          <ConversationVoiceStatus
            error={voiceControlFailure?.message ?? sessionState.voice.error?.message}
            onRetry={voiceControlFailure
              ? () => runVoiceControl(
                voiceControlFailure.retry,
                voiceControlFailure.message,
              )
              : undefined}
            onReturn={() => setFocusMode("live")}
            onStop={stopVoice}
            retryLabel={voiceControlFailure ? "Retry voice control" : "Retry voice"}
            voice={sessionState.voice}
          />
        ) : null}
        <details className="workspace-diagnostics">
          <summary>Diagnostics</summary>
          <PrivacyStatus
            components={
              runtimeFailed
                ? disconnectedComponentStatus
                : components
            }
          />
        </details>
      </aside>
      </main>
    </>
  );
}

interface HistoryPaneProps {
  continuationAvailable: boolean;
  continuationError: string | undefined;
  continuationPending: boolean;
  continuationPreview: ContinuationPreview | undefined;
  continuationUnavailableReason: string | undefined;
  conversations: ConversationSummary[];
  error: string | undefined;
  onBack(): void;
  onCancelContinuation(): void;
  onConfirmContinuation(): void;
  onDelete(id: string, revision: HistoryRevision): Promise<void>;
  onOpen(id: string): void;
  onRequestContinuation(): void;
  selected: ConversationHistory | undefined;
  storagePath: string | undefined;
}

function HistoryPane({
  continuationAvailable,
  continuationError,
  continuationPending,
  continuationPreview,
  continuationUnavailableReason,
  conversations,
  error,
  onBack,
  onCancelContinuation,
  onConfirmContinuation,
  onDelete,
  onOpen,
  onRequestContinuation,
  selected,
  storagePath,
}: HistoryPaneProps) {
  const [confirmDeleteId, setConfirmDeleteId] = useState<string>();
  const [deletePendingId, setDeletePendingId] = useState<string>();
  const [focusTarget, setFocusTarget] = useState<string>();
  const [restoreContinuationFocus, setRestoreContinuationFocus] = useState(false);
  const deleteButtons = useRef(new Map<string, HTMLButtonElement>());
  const openButtons = useRef(new Map<string, HTMLButtonElement>());
  const historyHeading = useRef<HTMLHeadingElement>(null);
  const destructiveConfirmation = useRef<HTMLButtonElement>(null);
  const continueButton = useRef<HTMLButtonElement>(null);
  const continueConfirmation = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    setConfirmDeleteId(undefined);
  }, [selected?.id]);
  useEffect(() => {
    if (confirmDeleteId) destructiveConfirmation.current?.focus();
  }, [confirmDeleteId]);
  useEffect(() => {
    if (continuationPreview) continueConfirmation.current?.focus();
  }, [continuationPreview]);
  useEffect(() => {
    if (!continuationPreview && restoreContinuationFocus) {
      setRestoreContinuationFocus(false);
      continueButton.current?.focus();
    }
  }, [continuationPreview, restoreContinuationFocus]);
  useEffect(() => {
    if (!focusTarget) return;
    if (focusTarget === "heading") {
      historyHeading.current?.focus();
    } else if (focusTarget.startsWith("delete:")) {
      deleteButtons.current.get(focusTarget.slice("delete:".length))?.focus();
    } else {
      openButtons.current.get(focusTarget)?.focus();
    }
    setFocusTarget(undefined);
  }, [conversations, focusTarget, selected]);

  const cancelDelete = () => {
    const id = confirmDeleteId;
    setConfirmDeleteId(undefined);
    if (id) setFocusTarget(`delete:${id}`);
  };

  const confirmDelete = async (conversation: ConversationSummary) => {
    if (deletePendingId) return;
    const index = conversations.findIndex((item) => item.id === conversation.id);
    const nextFocus = conversations[index + 1]?.id ?? conversations[index - 1]?.id ?? "heading";
    setDeletePendingId(conversation.id);
    try {
      await onDelete(conversation.id, conversation.revision);
      setConfirmDeleteId(undefined);
      setFocusTarget(nextFocus);
    } catch {
      setConfirmDeleteId(undefined);
      setFocusTarget(`delete:${conversation.id}`);
    } finally {
      setDeletePendingId(undefined);
    }
  };

  const deletionConfirmation = (conversation: ConversationSummary) => (
    <div className="history-delete-confirmation" role="group" aria-labelledby="delete-session-title">
      <h2 id="delete-session-title">Delete {conversation.title}?</h2>
      <p>This permanently removes this saved Session and its locally stored turns.</p>
      <div className="delete-history-controls">
        <button
          className="delete-history-action"
          disabled={deletePendingId === conversation.id}
          onClick={() => void confirmDelete(conversation)}
          ref={destructiveConfirmation}
          type="button"
        >
          {deletePendingId === conversation.id ? "Deleting…" : "Delete permanently"}
        </button>
        <button
          className="quiet-action"
          disabled={deletePendingId === conversation.id}
          onClick={cancelDelete}
          type="button"
        >
          Cancel
        </button>
      </div>
    </div>
  );

  if (selected) {
    const summary = summaryOf(selected);
    const contextTurns = selected.turns.filter((turn) => turn.origin === "continued_context");
    const liveTurns = selected.turns.filter((turn) => turn.origin === "live");
    const continueReasonId = continuationUnavailableReason
      ? "continue-session-unavailable"
      : undefined;
    return (
      <section className="history-pane" aria-labelledby="history-title">
        <header className="history-header">
          <div>
            <p className="utility-label">Local transcript</p>
            <h1 id="history-title">{selected.title}</h1>
          </div>
          <button
            className="quiet-action"
            disabled={continuationPending || deletePendingId !== undefined}
            onClick={onBack}
            type="button"
          >
            All Sessions
          </button>
        </header>
        <p className="history-disclosure">
          Sessions are read-only conversations saved locally by this app. Opening one does not restore model context or change the active conversation.
        </p>
        {selected.continuationState === "unconfirmed" ? (
          <p className="history-continuation-state" role="status">Continuation unconfirmed</p>
        ) : selected.continuationState === "preparing" ? (
          <p className="history-continuation-state" role="status">Continuation preparing</p>
        ) : null}
        {contextTurns.length > 0 ? (
          <StoredCarriedContextSection conversation={selected} turns={contextTurns} />
        ) : null}
        <div className="transcript history-transcript" aria-label="Saved conversation transcript">
          {liveTurns.map((turn, index) => (
            <article className="turn" key={`${turn.turnId}-${index}`}>
              <p className="turn-user">{turn.transcript}</p>
              <div className="turn-assistant">{turn.response || "No saved response"}</div>
              {turn.failureMessage ? <p className="turn-error">{turn.failureMessage}</p> : null}
            </article>
          ))}
        </div>
        {error ? <p className="turn-error" role="alert">{error}</p> : null}
        {continuationError ? <p className="turn-error" role="alert">{continuationError}</p> : null}
        {continuationPreview ? (
          <div className="continue-confirmation" role="group" aria-labelledby="continue-session-title">
            <h2 id="continue-session-title">Continue {selected.title} as a new conversation?</h2>
            <p className="continuation-preview">
              {continuationPreview.exchanges.length} completed exchanges · {continuationPreview.bytes} UTF-8 bytes
            </p>
            <p>
              Up to the latest 16 completed exchanges and 32 KiB will be carried over. The new conversation uses the current model, current response persona, and memories currently active and eligible under the runtime&apos;s retrieval policy.
            </p>
            <p>
              The saved source remains unchanged. This does not restore the exact historical model state.
            </p>
            <div className="continuation-actions">
              <button
                className="primary-action"
                disabled={continuationPending}
                onClick={onConfirmContinuation}
                ref={continueConfirmation}
                type="button"
              >
                {continuationPending ? "Starting…" : "Start new conversation"}
              </button>
              <button
                className="quiet-action"
                disabled={continuationPending}
                onClick={() => {
                  setRestoreContinuationFocus(true);
                  onCancelContinuation();
                }}
                type="button"
              >
                Cancel
              </button>
            </div>
          </div>
        ) : (
          <div className="history-detail-actions">
            <button
              aria-describedby={continueReasonId}
              className="continue-history-action"
              disabled={!continuationAvailable || continuationUnavailableReason !== undefined}
              onClick={onRequestContinuation}
              ref={continueButton}
              type="button"
            >
              Continue as new conversation
            </button>
            {continuationUnavailableReason ? (
              <p id={continueReasonId}>{continuationUnavailableReason}</p>
            ) : null}
            {confirmDeleteId === selected.id ? deletionConfirmation(summary) : (
              <button
                className="delete-history-action"
                disabled={continuationPending}
                onClick={() => setConfirmDeleteId(selected.id)}
                ref={(button) => {
                  if (button) deleteButtons.current.set(selected.id, button);
                }}
                type="button"
            >
              Delete session
            </button>
            )}
          </div>
        )}
      </section>
    );
  }

  return (
    <section className="history-pane" aria-labelledby="history-title">
      <header className="history-header">
        <div>
          <p className="utility-label">Stored on this Mac</p>
          <h1 id="history-title" ref={historyHeading} tabIndex={-1}>Sessions</h1>
        </div>
      </header>
      <p className="history-disclosure">
        Sessions are read-only conversations saved locally by this app. They are separate from runtime memory, and opening one never restores model context.
      </p>
      {error ? <p className="turn-error" role="alert">{error}</p> : null}
      <div className="history-list" aria-label="Saved conversations">
        {conversations.length === 0 ? (
          <p className="empty-history">No saved conversations yet.</p>
        ) : conversations.map((conversation) => {
          const titleId = `history-session-${conversation.id}`;
          const pending = deletePendingId === conversation.id;
          return (
            <div
              aria-labelledby={titleId}
              className="history-row"
              key={conversation.id}
              role="group"
            >
              <div className="history-row-main">
                <button
                  aria-describedby={titleId}
                  aria-label={`Open ${conversation.title}`}
                  className="history-item"
                  disabled={pending}
                  onClick={() => onOpen(conversation.id)}
                  ref={(button) => {
                    if (button) openButtons.current.set(conversation.id, button);
                  }}
                  type="button"
                >
                  <span id={titleId}>{conversation.title}</span>
                  <time dateTime={new Date(conversation.updatedAtMs).toISOString()}>
                    {formatHistoryDate(conversation.updatedAtMs)}
                  </time>
                  {conversation.continuationState === "unconfirmed" ? (
                    <small>Continuation unconfirmed</small>
                  ) : conversation.continuationState === "preparing" ? (
                    <small>Local confirmation pending</small>
                  ) : null}
                </button>
                <button
                  aria-describedby={titleId}
                  aria-label={`Delete session ${conversation.title}`}
                  className="history-row-delete"
                  disabled={pending}
                  onClick={() => setConfirmDeleteId(conversation.id)}
                  ref={(button) => {
                    if (button) deleteButtons.current.set(conversation.id, button);
                  }}
                  type="button"
                >
                  Delete
                </button>
              </div>
              {confirmDeleteId === conversation.id ? deletionConfirmation(conversation) : null}
            </div>
          );
        })}
      </div>
      <details className="history-location">
        <summary>Storage details</summary>
        <code>{storagePath ?? "Resolving local storage path…"}</code>
      </details>
    </section>
  );
}

function CarriedContextSection({
  context,
}: {
  context: NonNullable<ConversationSessionState["continuation"]["carriedContext"]>;
}) {
  return (
    <details
      aria-label={`Context carried over from ${context.sourceTitle}`}
      className="carried-context"
      open
      role="region"
    >
      <summary>
        <strong>Context carried over from {context.sourceTitle}</strong>
        <span>{context.exchanges.length} completed exchanges · {context.bytes} UTF-8 bytes</span>
      </summary>
      <p className="carried-context-disclosure">
        Read-only context for this branch. Collapsing this section does not remove it from the runtime.
      </p>
      <div className="carried-context-turns">
        {context.exchanges.map((exchange, index) => (
          <article className="carried-context-turn" key={index}>
            <p>{exchange.user}</p>
            <div>{exchange.assistant}</div>
          </article>
        ))}
      </div>
    </details>
  );
}

function StoredCarriedContextSection({
  conversation,
  turns,
}: {
  conversation: ConversationHistory;
  turns: ConversationHistory["turns"];
}) {
  const sourceTitle = conversation.title.startsWith("Continued: ")
    ? conversation.title.slice("Continued: ".length)
    : conversation.title;
  return (
    <details
      aria-label={`Context carried over from ${sourceTitle}`}
      className="carried-context history-carried-context"
      open
      role="region"
    >
      <summary>
        <strong>Context carried over from {sourceTitle}</strong>
        <span>{turns.length} completed exchanges · saved copy</span>
      </summary>
      <p className="carried-context-disclosure">
        This copied context remains with the branch even if its source Session is deleted.
      </p>
      <div className="carried-context-turns">
        {turns.map((turn, index) => (
          <article className="carried-context-turn" key={`${turn.turnId}-${index}`}>
            <p>{turn.transcript}</p>
            <div>{turn.response}</div>
          </article>
        ))}
      </div>
    </details>
  );
}

function continuationReason(
  state: ConversationSessionState,
  capabilityAvailable: boolean,
  operationError: string | undefined,
  continuationPending: boolean,
): string | undefined {
  if (!capabilityAvailable) return "The connected runtime cannot continue saved context.";
  if (continuationPending || state.continuation.inProgress) {
    return "A Session continuation is already in progress.";
  }
  if (state.voice.session !== "idle" || state.voice.capture !== "stopped") {
    return "End or resolve Voice before continuing a Session.";
  }
  if (state.phase === "streaming") {
    return "Wait for the current response before continuing a Session.";
  }
  if (operationError || state.phase === "failed" || state.phase === "closed") {
    return "Reconnect the local runtime before continuing a Session.";
  }
  return undefined;
}

function selectContinuationPreview(conversation: ConversationHistory): ContinuationSelection {
  const candidates = conversation.turns.filter((turn) =>
    turn.state === "completed"
    && turn.transcript.trim().length > 0
    && turn.response.trim().length > 0);
  if (candidates.length === 0) {
    return { error: "This Session has no completed exchanges to continue." };
  }

  const exchanges: { user: string; assistant: string }[] = [];
  let bytes = 0;
  for (let index = candidates.length - 1; index >= 0; index -= 1) {
    const candidate = candidates[index]!;
    const userBytes = utf8Bytes(candidate.transcript);
    const assistantBytes = utf8Bytes(candidate.response);
    const exchangeBytes = userBytes + assistantBytes;
    const tooLarge = userBytes > continuationMessageByteLimit
      || assistantBytes > continuationMessageByteLimit
      || exchangeBytes > continuationByteLimit;
    if (exchanges.length === 0 && tooLarge) {
      return {
        error: "The latest exchange is too large to continue without shortening or compression.",
      };
    }
    if (
      tooLarge
      || exchanges.length === continuationExchangeLimit
      || bytes + exchangeBytes > continuationByteLimit
    ) {
      break;
    }
    exchanges.push({ user: candidate.transcript, assistant: candidate.response });
    bytes += exchangeBytes;
  }
  exchanges.reverse();
  return { preview: { bytes, exchanges } };
}

function contextBytes(exchanges: readonly { user: string; assistant: string }[]): number {
  return exchanges.reduce(
    (total, exchange) => total + utf8Bytes(exchange.user) + utf8Bytes(exchange.assistant),
    0,
  );
}

function utf8Bytes(value: string): number {
  return new TextEncoder().encode(value).byteLength;
}

function preparationError(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  if (/not found/i.test(message)) return "That saved conversation no longer exists.";
  if (/revision conflict/i.test(message)) {
    return "That saved conversation changed. Open it again to continue.";
  }
  if (/latest exchange is too large/i.test(message)) {
    return "The latest exchange is too large to continue without shortening or compression.";
  }
  if (/no completed exchanges/i.test(message)) {
    return "This Session has no completed exchanges to continue.";
  }
  return "The saved conversation could not be prepared for continuation. Try again.";
}

function deleteHistoryError(error: unknown): string {
  const message = error instanceof Error ? error.message : String(error);
  if (/not found/i.test(message)) return "That saved conversation no longer exists.";
  if (/revision conflict/i.test(message)) {
    return "That saved conversation changed. Open it again before deleting.";
  }
  return "That saved conversation could not be deleted. Try again.";
}

function isDefiniteContinuationRejection(error: unknown): boolean {
  if (error instanceof CommandRejectedError) return true;
  const message = error instanceof Error ? error.message : String(error);
  return /conversation session is closed|continuation is in progress|conversation turn is already active|voice session is not idle/i
    .test(message);
}

function carriedContextOf(
  branch: ConversationHistory,
): CarriedConversationContext | undefined {
  if (!branch.continuedFromId || !branch.continuationOperationId) return undefined;
  const exchanges = branch.turns
    .filter((turn) => turn.origin === "continued_context")
    .map((turn) => ({ user: turn.transcript, assistant: turn.response }));
  const sourceTitle = branch.title.startsWith("Continued: ")
    ? branch.title.slice("Continued: ".length)
    : branch.title;
  return {
    sourceId: branch.continuedFromId,
    sourceTitle,
    operationId: branch.continuationOperationId,
    exchanges,
    bytes: contextBytes(exchanges),
  };
}

async function confirmContinuationState(
  store: ConversationHistoryStore,
  branch: ConversationHistory,
): Promise<{ branch: ConversationHistory; verified: boolean }> {
  for (let attempt = 0; attempt < 2; attempt += 1) {
    try {
      const result = await store.setContinuationState(
        branch.id,
        branch.revision,
        "confirmed",
      );
      return {
        branch: { ...branch, continuationState: "confirmed", revision: result.revision },
        verified: true,
      };
    } catch {
      // Retry once with the same revision: the native transition is idempotent after a lost reply.
    }
  }
  try {
    const stored = await store.get(branch.id);
    if (stored) {
      return { branch: stored, verified: stored.continuationState === "confirmed" };
    }
  } catch {
    // Preserve the prepared snapshot when local verification itself is unavailable.
  }
  return { branch, verified: false };
}

async function reconcilePreparingHistory(
  store: ConversationHistoryStore,
  conversations: ConversationSummary[],
  lastOperationId: string | null,
): Promise<{
  conversations: ConversationSummary[];
  activeBranch?: ConversationHistory;
  confirmationPending: boolean;
}> {
  const reconciled: ConversationSummary[] = [];
  let activeBranch: ConversationHistory | undefined;
  let confirmationPending = false;
  for (const conversation of conversations) {
    const exactMatch = conversation.continuationOperationId !== null
      && conversation.continuationOperationId === lastOperationId;
    if (exactMatch && conversation.continuationState !== null) {
      try {
        const stored = await store.get(conversation.id);
        if (stored) {
          const confirmation = stored.continuationState === "confirmed"
            ? { branch: stored, verified: true }
            : await confirmContinuationState(store, stored);
          activeBranch ??= confirmation.branch;
          confirmationPending ||= !confirmation.verified;
          reconciled.push(summaryOf(confirmation.branch));
          continue;
        }
      } catch {
        // Leave the summary available when the full matched branch cannot be loaded.
      }
      reconciled.push(conversation);
      continue;
    }
    if (conversation.continuationState !== "preparing") {
      reconciled.push(conversation);
      continue;
    }
    try {
      const result = await store.setContinuationState(
        conversation.id,
        conversation.revision,
        "unconfirmed",
      );
      reconciled.push({
        ...conversation,
        continuationState: "unconfirmed",
        revision: result.revision,
      });
    } catch {
      reconciled.push(conversation);
    }
  }
  return {
    conversations: reconciled,
    ...(activeBranch ? { activeBranch } : {}),
    confirmationPending,
  };
}

function createConversationId(timestamp: number): string {
  const randomId = globalThis.crypto?.randomUUID?.();
  return randomId ?? `conversation-${timestamp}-${Math.random().toString(16).slice(2)}`;
}

function conversationTitle(transcript: string): string {
  const normalized = transcript.replace(/\s+/g, " ").replace(/\p{Cc}/gu, "").trim();
  if (normalized.length <= 72) return normalized;
  const clipped = normalized.slice(0, 69);
  const wholeCodePoints = /[\uD800-\uDBFF]$/.test(clipped) ? clipped.slice(0, -1) : clipped;
  return `${wholeCodePoints.trimEnd()}…`;
}

function summaryOf(conversation: ConversationHistory): ConversationSummary {
  const { turns: _turns, ...summary } = conversation;
  return summary;
}

function mergeHistory(
  stored: ConversationSummary[],
  current: ConversationSummary[],
): ConversationSummary[] {
  const merged = new Map(stored.map((conversation) => [conversation.id, conversation]));
  for (const conversation of current) {
    const storedConversation = merged.get(conversation.id);
    if (!storedConversation || conversation.updatedAtMs > storedConversation.updatedAtMs) {
      merged.set(conversation.id, conversation);
    }
  }
  return [...merged.values()].sort((left, right) =>
    right.updatedAtMs - left.updatedAtMs || left.id.localeCompare(right.id));
}

function formatHistoryDate(timestamp: number): string {
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(timestamp);
}

function prefersReducedMotion(): boolean {
  return window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ?? false;
}

function voiceSignalLabel(voice: ConversationSessionState["voice"]): string {
  if (voice.error || voice.session === "error") return "Needs attention";
  switch (voice.capture) {
    case "starting":
    case "listening":
      return "Voice listening";
    case "pausing":
      return "Voice pausing";
    case "paused":
      return "Voice paused";
    case "resuming":
      return "Voice resuming";
    default:
      return voice.availability === "configured" ? "Ready" : "Text only";
  }
}
