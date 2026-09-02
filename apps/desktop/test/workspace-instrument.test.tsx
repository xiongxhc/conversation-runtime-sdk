// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import {
  WorkspaceNavigation,
  type DestinationAvailability,
} from "../src/components/workspace/WorkspaceNavigation.js";
import {
  ConversationInstrument,
  type ConversationInstrumentProps,
} from "../src/components/workspace/ConversationInstrument.js";
import { RuntimeSignalPanel } from "../src/components/workspace/RuntimeSignalPanel.js";

afterEach(cleanup);

// @ts-expect-error Failed presentation states require linked recovery actions.
const failedWithoutRecovery: ConversationInstrumentProps = {
  composer: { value: "" },
  onComposerChange: () => undefined,
  onSend: () => undefined,
  onStop: () => undefined,
  send: { enabled: false },
  state: "failed",
  turns: [],
};
void failedWithoutRecovery;

// @ts-expect-error Closed presentation states require linked recovery actions.
const closedWithoutRecovery: ConversationInstrumentProps = {
  composer: { value: "" },
  onComposerChange: () => undefined,
  onSend: () => undefined,
  onStop: () => undefined,
  send: { enabled: false },
  state: "closed",
  turns: [],
};
void closedWithoutRecovery;

const availability: Record<"conversation" | "sessions" | "memory" | "response", DestinationAvailability> = {
  conversation: { enabled: true },
  sessions: { enabled: true },
  memory: { badge: "3 new", enabled: false, reason: "Memory review is unavailable while the response is active." },
  response: { enabled: true },
};

describe("WorkspaceNavigation", () => {
  it("keeps each named destination identifiable while disabling unavailable Memory review", () => {
    const onSelect = vi.fn();
    const { container } = render(
      <WorkspaceNavigation
        activeDestination="conversation"
        availability={availability}
        onSelect={onSelect}
      />,
    );

    expect(screen.getByRole("button", { name: "Conversation" }).getAttribute("aria-current")).toBe("page");
    expect((screen.getByRole("button", { name: "Sessions" }) as HTMLButtonElement).disabled).toBe(false);
    const memoryReview = screen.getByRole("button", {
      name: "Memory review; 3 newly announced candidate memories since Memory review was last opened",
    });
    expect(memoryReview.getAttribute("aria-disabled")).toBe("true");
    expect(memoryReview.getAttribute("aria-describedby")).toBe(
      "memory-destination-tooltip memory-destination-explanation",
    );
    expect(screen.getByRole("button", { name: "Conversation" }).getAttribute("aria-describedby"))
      .toBe("conversation-destination-tooltip");
    expect(screen.getAllByRole("tooltip").map((tooltip) => tooltip.textContent)).toEqual([
      "Conversation",
      "Sessions",
      "Memory review",
      "How it responds",
    ]);
    expect(screen.getByRole("tooltip", { name: "Memory review" }).id)
      .toBe("memory-destination-tooltip");
    expect(screen.getByText("Memory review is unavailable while the response is active.")).toBeTruthy();
    expect(screen.getByText("3 new")).toBeTruthy();
    expect((screen.getByRole("button", { name: "How it responds" }) as HTMLButtonElement).disabled).toBe(false);
    expect(container.querySelectorAll("svg[data-icon]")).toHaveLength(4);
    expect([...container.querySelectorAll("svg[data-icon]")].map((icon) => icon.getAttribute("data-icon"))).toEqual([
      "conversation",
      "sessions",
      "memory",
      "response",
    ]);

    fireEvent.click(screen.getByRole("button", { name: "Sessions" }));
    expect(onSelect).toHaveBeenCalledWith("sessions");
    memoryReview.focus();
    expect(document.activeElement).toBe(memoryReview);
    fireEvent.click(memoryReview);
    expect(onSelect).not.toHaveBeenCalledWith("memory");
  });
});

describe("ConversationInstrument", () => {
  it("keeps the streaming transcript busy and exposes Stop instead of a second Send", () => {
    const onStop = vi.fn();
    render(
      <ConversationInstrument
        composer={{ value: "A follow-up" }}
        onComposerChange={vi.fn()}
        onSend={vi.fn()}
        onStop={onStop}
        send={{ enabled: true }}
        state="streaming"
        turns={[{ id: "turn-1", response: "A partial answer", state: "streaming", transcript: "A question" }]}
      />,
    );

    expect(screen.getByRole("log", { name: "Conversation transcript" }).getAttribute("aria-busy")).toBe("true");
    expect(screen.getByText("This conversation is saved on this Mac.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Stop" })).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Send" })).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    expect(onStop).toHaveBeenCalledOnce();
  });

  it("keeps the textarea editable while voice pause is pending, then enables Send after pause acknowledgement", () => {
    const onSend = vi.fn();
    const { rerender } = render(
      <ConversationInstrument
        composer={{ value: "Typed while voice is active", voicePausePending: true }}
        onComposerChange={vi.fn()}
        onSend={onSend}
        onStop={vi.fn()}
        send={{ enabled: false, reason: "Voice is pausing before you type." }}
        state="ready"
        turns={[]}
      />,
    );

    expect((screen.getByLabelText("Message") as HTMLTextAreaElement).disabled).toBe(false);
    expect((screen.getByRole("button", { name: "Send" }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText("Voice is pausing before you type.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(onSend).not.toHaveBeenCalled();

    rerender(
      <ConversationInstrument
        composer={{ value: "Typed while voice is active", voicePaused: true }}
        onComposerChange={vi.fn()}
        onSend={onSend}
        onStop={vi.fn()}
        send={{ enabled: true }}
        state="ready"
        turns={[]}
      />,
    );

    expect(screen.getByText("Voice paused while you type; it will resume after this response.")).toBeTruthy();
    const send = screen.getByRole("button", { name: "Send" }) as HTMLButtonElement;
    expect(send.disabled).toBe(false);
    expect(send.getAttribute("aria-describedby")).toBeNull();

    fireEvent.click(send);
    expect(onSend).toHaveBeenCalledOnce();
  });

  it("renders linked recovery actions whenever an operation failure needs attention", () => {
    const onReconnect = vi.fn();
    const onReturnToSetup = vi.fn();
    render(
      <ConversationInstrument
        composer={{ value: "" }}
        onComposerChange={vi.fn()}
        onSend={vi.fn()}
        onStop={vi.fn()}
        operationFailure={{
          message: "The runtime disconnected.",
          recovery: {
            reconnect: { enabled: true, onInvoke: onReconnect },
            returnToSetup: { enabled: true, onInvoke: onReturnToSetup },
          },
        }}
        send={{ enabled: false, reason: "Reconnect before sending another message." }}
        state="failed"
        turns={[]}
      />,
    );

    expect(screen.getByRole("alert").textContent).toContain("The runtime disconnected.");
    const reconnect = screen.getByRole("button", { name: "Reconnect local runtime" });
    expect(reconnect.getAttribute("aria-describedby")).toBeNull();
    fireEvent.click(reconnect);
    fireEvent.click(screen.getByRole("button", { name: "Return to setup" }));
    expect(onReconnect).toHaveBeenCalledOnce();
    expect(onReturnToSetup).toHaveBeenCalledOnce();
  });

  it("renders a failed turn's Try again action without auto-sending", () => {
    const onRetry = vi.fn();
    const onSend = vi.fn();
    render(
      <ConversationInstrument
        composer={{ value: "" }}
        onComposerChange={vi.fn()}
        onSend={onSend}
        onStop={vi.fn()}
        send={{ enabled: false, reason: "Write a message before sending." }}
        state="ready"
        turns={[{
          failure: { message: "The response failed.", retry: { enabled: true, onInvoke: onRetry } },
          id: "turn-1",
          response: "",
          state: "failed",
          transcript: "A question",
        }]}
      />,
    );

    expect(screen.getByRole("alert").textContent).toContain("The response failed.");
    const retry = screen.getByRole("button", { name: "Try again" });
    expect(retry.getAttribute("aria-describedby")).toBeNull();
    fireEvent.click(retry);
    expect(onRetry).toHaveBeenCalledOnce();
    expect(onSend).not.toHaveBeenCalled();
  });
});

describe("RuntimeSignalPanel", () => {
  it("renders Locality Trace states in order and keeps recovery actions named", () => {
    const onVoiceFocus = vi.fn();
    const onReconnect = vi.fn();
    const onDisconnect = vi.fn();
    render(
      <RuntimeSignalPanel
        actions={{
          connection: { enabled: true, label: "Reconnect local runtime", onInvoke: onReconnect },
          voice: { enabled: true, label: "Preview Voice Focus", onInvoke: onVoiceFocus },
        }}
        locality={{
          memory: { state: "unavailable" },
          model: { state: "verified" },
          runtime: { state: "verified" },
          voice: { detail: "Voice device disconnected", state: "error" },
        }}
        memory={{ label: "Memory", state: "unavailable", value: "Memory off" }}
        model={{ label: "Model", state: "verified", value: "Local model" }}
        voice={{ label: "Voice", state: "error", value: "Needs attention" }}
      />,
    );

    const trace = screen.getByRole("list", { name: "Locality Trace" });
    expect([...trace.querySelectorAll("li")].map((segment) => segment.textContent)).toEqual([
      "RuntimeVerified locally",
      "ModelVerified locally",
      "MemoryUnavailable",
      "VoiceNeeds attention: Voice device disconnected",
    ]);
    expect(trace.querySelectorAll('[data-state="verified"]')).toHaveLength(2);
    expect(trace.querySelectorAll('[data-state="unavailable"]')).toHaveLength(1);
    expect(trace.querySelectorAll('[data-state="error"]')).toHaveLength(1);

    const voiceFocus = screen.getByRole("button", { name: "Preview Voice Focus" });
    expect(voiceFocus.getAttribute("aria-describedby")).toBeNull();
    fireEvent.click(voiceFocus);
    fireEvent.click(screen.getByRole("button", { name: "Reconnect local runtime" }));
    expect(onVoiceFocus).toHaveBeenCalledOnce();
    expect(onReconnect).toHaveBeenCalledOnce();
    expect(onDisconnect).not.toHaveBeenCalled();
  });

  it("fails closed when Voice Focus and the single connection action are unavailable", () => {
    const onVoiceFocus = vi.fn();
    const onDisconnect = vi.fn();
    const unavailableVoice = {
      enabled: false as const,
      label: "Voice Focus" as const,
      onInvoke: onVoiceFocus,
      reason: "Voice Focus is unavailable until audio is connected.",
    };
    const unavailableConnection = {
      enabled: false as const,
      label: "Disconnect local runtime" as const,
      onInvoke: onDisconnect,
      reason: "Reconnect the runtime before it can be disconnected.",
    };
    render(
      <RuntimeSignalPanel
        actions={{
          connection: unavailableConnection,
          voice: unavailableVoice,
        }}
        locality={{
          memory: { state: "unavailable" },
          model: { state: "verified" },
          runtime: { state: "verified" },
          voice: { state: "unavailable" },
        }}
        memory={{ label: "Memory", state: "unavailable", value: "Memory off" }}
        model={{ label: "Model", state: "verified", value: "Local model" }}
        voice={{ label: "Voice", state: "unavailable", value: "Unavailable" }}
      />,
    );

    expect(screen.getAllByRole("button")).toHaveLength(2);
    expect((screen.getByRole("button", { name: "Voice Focus" }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: "Disconnect local runtime" }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText("Voice Focus is unavailable until audio is connected.")).toBeTruthy();
    expect(screen.getByText("Reconnect the runtime before it can be disconnected.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Voice Focus" }));
    fireEvent.click(screen.getByRole("button", { name: "Disconnect local runtime" }));
    expect(onVoiceFocus).not.toHaveBeenCalled();
    expect(onDisconnect).not.toHaveBeenCalled();
  });
});
