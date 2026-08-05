import type { StorageLike } from "./preferences.js";
import type { RuntimePaths } from "../runtime/tauri-transport.js";

export const setupStorageKey = "conversation-desktop.setup";

export const defaultSetupPaths: RuntimePaths = {
  gatewayPath: "",
  configPath: "",
};

const maximumPathLength = 4_096;

export type SetupPathsErrorCategory = "invalid" | "path_too_long" | "storage";

export class SetupPathsError extends Error {
  readonly name = "SetupPathsError";

  constructor(readonly category: SetupPathsErrorCategory) {
    super(category);
  }
}

export function loadSetupPaths(storage: StorageLike): RuntimePaths {
  try {
    const storedValue = storage.getItem(setupStorageKey);
    if (storedValue === null) return { ...defaultSetupPaths };
    const parsed = JSON.parse(storedValue) as unknown;
    if (!isRecord(parsed) || parsed.version !== 1) return { ...defaultSetupPaths };
    if (!isAbsolutePath(parsed.gatewayPath) || !isAbsolutePath(parsed.configPath)) {
      return { ...defaultSetupPaths };
    }
    return {
      gatewayPath: parsed.gatewayPath,
      configPath: parsed.configPath,
    };
  } catch {
    return { ...defaultSetupPaths };
  }
}

export function saveSetupPaths(storage: StorageLike, paths: RuntimePaths): void {
  if (!hasAbsolutePrefix(paths.gatewayPath) || !hasAbsolutePrefix(paths.configPath)) {
    throw new SetupPathsError("invalid");
  }
  if (paths.gatewayPath.length > maximumPathLength || paths.configPath.length > maximumPathLength) {
    throw new SetupPathsError("path_too_long");
  }
  try {
    storage.setItem(setupStorageKey, JSON.stringify({ version: 1, ...paths }));
  } catch {
    throw new SetupPathsError("storage");
  }
}

function isAbsolutePath(value: unknown): value is string {
  return hasAbsolutePrefix(value) && value.length <= maximumPathLength;
}

function hasAbsolutePrefix(value: unknown): value is string {
  return typeof value === "string" && value.startsWith("/");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
