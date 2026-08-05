import { describe, expect, it } from "vitest";

import {
  defaultSetupPaths,
  loadSetupPaths,
  saveSetupPaths,
  setupStorageKey,
} from "../src/preferences/setup.js";

describe("local setup preferences", () => {
  it("uses empty paths for missing or malformed storage", () => {
    expect(loadSetupPaths(storageWith())).toEqual(defaultSetupPaths);
    expect(loadSetupPaths(storageWith("not json"))).toEqual(defaultSetupPaths);
  });

  it("rejects relative or unsupported stored paths", () => {
    expect(loadSetupPaths(storageWith({
      version: 1,
      gatewayPath: "runtime-gateway",
      configPath: "/runtime.toml",
    }))).toEqual(defaultSetupPaths);
    expect(loadSetupPaths(storageWith({
      version: 2,
      gatewayPath: "/runtime-gateway",
      configPath: "/runtime.toml",
    }))).toEqual(defaultSetupPaths);
  });

  it("round-trips only absolute non-sensitive UI paths", () => {
    const storage = storageWith();
    saveSetupPaths(storage, {
      gatewayPath: "/Applications/Conversation Runtime/runtime-gateway",
      configPath: "/Users/tester/runtime.toml",
    });

    expect(JSON.parse(storage.getItem(setupStorageKey) ?? "")).toEqual({
      version: 1,
      gatewayPath: "/Applications/Conversation Runtime/runtime-gateway",
      configPath: "/Users/tester/runtime.toml",
    });
    expect(loadSetupPaths(storage)).toEqual({
      gatewayPath: "/Applications/Conversation Runtime/runtime-gateway",
      configPath: "/Users/tester/runtime.toml",
    });
  });
});

function storageWith(value?: unknown) {
  const values = new Map<string, string>();
  if (value !== undefined) {
    values.set(setupStorageKey, typeof value === "string" ? value : JSON.stringify(value));
  }
  return {
    getItem(key: string) {
      return values.get(key) ?? null;
    },
    setItem(key: string, storedValue: string) {
      values.set(key, storedValue);
    },
  };
}
