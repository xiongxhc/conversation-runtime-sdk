import { useRef, useState } from "react";

import type {
  MemoryCursor,
  MemoryInspection,
  MemoryPage,
} from "@conversation/runtime/browser";

import { SetupView } from "./components/SetupView.js";
import { Workspace, type VoiceCapabilitySnapshot } from "./components/Workspace.js";
import {
  conversationHistoryStore,
  type ConversationHistoryStore,
} from "./history/conversation-history.js";
import { loadPreferences, type StorageLike } from "./preferences/preferences.js";
import {
  loadSetupPaths,
  saveSetupPaths,
  SetupPathsError,
} from "./preferences/setup.js";
import {
  ConversationSession,
  type ConversationSessionState,
} from "./runtime/conversation-session.js";
import {
  RuntimeOpenError,
  TauriGatewayTransport,
  type RuntimePaths,
} from "./runtime/tauri-transport.js";

export interface DesktopSession {
  readonly state: ConversationSessionState;
  subscribe(listener: (state: ConversationSessionState) => void): () => void;
  send(transcript: string): bigint | Promise<bigint>;
  listMemories(cursor?: MemoryCursor | null): Promise<MemoryPage>;
  inspectMemory(memoryId: bigint): Promise<MemoryInspection>;
  interrupt(): Promise<void>;
  close(): Promise<void>;
}

export interface AppProps {
  connectSession?: (paths: RuntimePaths) => Promise<DesktopSession>;
  historyStore?: ConversationHistoryStore;
  storage?: StorageLike;
  voiceCapability?: VoiceCapabilitySnapshot;
}

const defaultConnectSession = async (paths: RuntimePaths): Promise<DesktopSession> => {
  const transport = await TauriGatewayTransport.start(paths);
  return ConversationSession.connect(transport);
};

export function App({
  connectSession = defaultConnectSession,
  historyStore = conversationHistoryStore,
  storage = window.localStorage,
  voiceCapability,
}: AppProps) {
  const [session, setSession] = useState<DesktopSession>();
  const [setupError, setSetupError] = useState<string>();
  const [connecting, setConnecting] = useState(false);
  const [preferences] = useState(() => loadPreferences(storage));
  const [setupPaths, setSetupPaths] = useState(() => loadSetupPaths(storage));
  const latestConnectionRequest = useRef(0);

  const connect = async (paths: RuntimePaths) => {
    const requestId = ++latestConnectionRequest.current;
    setConnecting(true);
    setSetupError(undefined);
    try {
      saveSetupPaths(storage, paths);
      if (requestId === latestConnectionRequest.current) {
        setSetupPaths(paths);
      }
      const connectedSession = await connectSession(paths);
      if (requestId !== latestConnectionRequest.current) {
        await connectedSession.close().catch(() => undefined);
        return;
      }
      if (!isVerifiedLocalOnly(connectedSession.state)) {
        await connectedSession.close().catch(() => undefined);
        throw new Error("The runtime did not verify local-only execution.");
      }
      setSession(connectedSession);
    } catch (error) {
      if (requestId !== latestConnectionRequest.current) return;
      setSetupError(setupGuidance(error));
    } finally {
      if (requestId === latestConnectionRequest.current) {
        setConnecting(false);
      }
    }
  };

  if (!session) {
    return (
      <SetupView
        connecting={connecting}
        error={setupError}
        initialPaths={setupPaths}
        onConnect={connect}
      />
    );
  }

  return (
    <Workspace
      historyStore={historyStore}
      initialPreferences={preferences}
      onClosed={(error) => {
        setSetupError(error);
        setSession(undefined);
      }}
      session={session}
      storage={storage}
      voiceCapability={voiceCapability}
    />
  );
}

function setupGuidance(error: unknown): string {
  if (error instanceof SetupPathsError) {
    switch (error.category) {
      case "path_too_long":
        return "A setup path is too long. Choose absolute paths no longer than 4096 characters, then try again.";
      case "storage":
        return "Setup paths could not be saved locally. Check that local app storage is available, then try connecting again.";
      default:
        return "Both setup paths must be absolute paths beginning with /. Review them, then try again.";
    }
  }
  if (error instanceof RuntimeOpenError) return error.message;
  const message = error instanceof Error ? error.message : "";
  if (/stdout ended|process exited|before ready/i.test(message)) {
    return "The gateway exited before it was ready. Check the runtime configuration and confirm the local model host is running, then reconnect.";
  }
  if (message === "The runtime did not verify local-only execution.") {
    return `${message} Select a local-only configuration before reconnecting.`;
  }
  return "The local runtime could not connect. Verify both absolute paths, executable permission, configuration, and local model host, then try again.";
}

function isVerifiedLocalOnly(state: ConversationSessionState): boolean {
  const status = state.status as unknown as {
    privacyMode?: unknown;
    languageLocation?: unknown;
    telemetryEnabled?: unknown;
  };
  return (
    status.privacyMode === "local_only" &&
    status.languageLocation === "local" &&
    status.telemetryEnabled === false
  );
}
