import { FormEvent, useState } from "react";

import type { RuntimePaths } from "../runtime/tauri-transport.js";

export interface SetupViewProps {
  connecting: boolean;
  error?: string;
  initialPaths: RuntimePaths;
  onConnect(paths: RuntimePaths): Promise<void>;
}

type PathErrors = Partial<Record<keyof RuntimePaths, string>>;

export function SetupView({ connecting, error, initialPaths, onConnect }: SetupViewProps) {
  const [gatewayPath, setGatewayPath] = useState(initialPaths.gatewayPath);
  const [configPath, setConfigPath] = useState(initialPaths.configPath);
  const [pathErrors, setPathErrors] = useState<PathErrors>({});

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const nextErrors: PathErrors = {};
    if (!gatewayPath.startsWith("/")) {
      nextErrors.gatewayPath = "Enter an absolute path beginning with /.";
    }
    if (!configPath.startsWith("/")) {
      nextErrors.configPath = "Enter an absolute path beginning with /.";
    }
    setPathErrors(nextErrors);
    if (Object.keys(nextErrors).length === 0) {
      void onConnect({ gatewayPath, configPath });
    }
  };

  return (
    <main className="setup-view">
      <section className="setup-panel" aria-labelledby="setup-title">
        <p className="utility-label">Local runtime setup</p>
        <h1 id="setup-title">Conversation Runtime</h1>
        <p className="setup-introduction">
          Connect the desktop app to a gateway and configuration already on this Mac.
        </p>
        <form aria-label="Runtime setup" onSubmit={submit} noValidate>
          <label htmlFor="gateway-path">Gateway executable</label>
          <input
            id="gateway-path"
            aria-describedby={pathErrors.gatewayPath ? "gateway-path-error" : undefined}
            aria-invalid={pathErrors.gatewayPath ? true : undefined}
            autoComplete="off"
            onChange={(event) => {
              const value = event.target.value;
              setGatewayPath(value);
              if (value.startsWith("/")) {
                setPathErrors((current) => ({ ...current, gatewayPath: undefined }));
              }
            }}
            placeholder="/absolute/path/to/runtime-gateway"
            value={gatewayPath}
          />
          {pathErrors.gatewayPath ? (
            <p className="field-error" id="gateway-path-error">
              {pathErrors.gatewayPath}
            </p>
          ) : null}

          <label htmlFor="config-path">Runtime configuration</label>
          <input
            id="config-path"
            aria-describedby={pathErrors.configPath ? "config-path-error" : undefined}
            aria-invalid={pathErrors.configPath ? true : undefined}
            autoComplete="off"
            onChange={(event) => {
              const value = event.target.value;
              setConfigPath(value);
              if (value.startsWith("/")) {
                setPathErrors((current) => ({ ...current, configPath: undefined }));
              }
            }}
            placeholder="/absolute/path/to/runtime.toml"
            value={configPath}
          />
          {pathErrors.configPath ? (
            <p className="field-error" id="config-path-error">
              {pathErrors.configPath}
            </p>
          ) : null}

          {error ? (
            <p className="setup-error" role="alert">
              {error}
            </p>
          ) : null}
          <button className="primary-action" disabled={connecting} type="submit">
            {connecting ? "Connecting…" : "Connect local runtime"}
          </button>
        </form>
      </section>
    </main>
  );
}
