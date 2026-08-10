import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const infoPlistPath = fileURLToPath(new URL("../src-tauri/Info.plist", import.meta.url));
const microphonePurpose =
  "Conversation Runtime uses the microphone to capture speech during voice sessions you start.";

describe("macOS privacy metadata", () => {
  it("declares why the desktop app needs microphone access", () => {
    const infoPlist = readFileSync(infoPlistPath, "utf8");

    expect(infoPlist).toContain("<key>NSMicrophoneUsageDescription</key>");
    expect(infoPlist).toContain(
      `<string>${microphonePurpose}</string>`,
    );
  });
});
