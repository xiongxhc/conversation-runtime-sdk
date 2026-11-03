import { FormEvent, useState } from "react";

import {
  defaultLocalModelEndpoint,
  RuntimeSetupError,
  type LocalModelLatency,
  type RuntimeSetupClient,
} from "../runtime/runtime-setup.js";
import type { RuntimePaths } from "../runtime/tauri-transport.js";

export interface SetupViewProps {
  connecting: boolean;
  error?: string;
  initialPaths: RuntimePaths;
  onConnect(paths: RuntimePaths): Promise<void>;
  setupClient: RuntimeSetupClient;
}

type PathErrors = Partial<Record<keyof RuntimePaths, string>>;

type GuidedStep = "idle" | "discovering" | "checking" | "preparing";

type LatencyOutcome = { kind: "report"; report: LocalModelLatency } | { kind: "cancelled" };

export function SetupView({ connecting, error, initialPaths, onConnect, setupClient }: SetupViewProps) {
  const [gatewayPath, setGatewayPath] = useState(initialPaths.gatewayPath);
  const [configPath, setConfigPath] = useState(initialPaths.configPath);
  const [pathErrors, setPathErrors] = useState<PathErrors>({});
  const [endpoint, setEndpoint] = useState(defaultLocalModelEndpoint);
  const [models, setModels] = useState<string[]>();
  const [model, setModel] = useState<string>();
  const [step, setStep] = useState<GuidedStep>("idle");
  const [latency, setLatency] = useState<LatencyOutcome>();
  const [guidedError, setGuidedError] = useState<string>();
  const [lastConnection, setLastConnection] = useState<"guided" | "manual">("manual");

  const busy = step !== "idle" || connecting;

  const findModels = async (event: FormEvent) => {
    event.preventDefault();
    setStep("discovering");
    setGuidedError(undefined);
    setLatency(undefined);
    try {
      const discovered = await setupClient.discoverModels(endpoint);
      setModels(discovered);
      setModel(discovered[0]);
    } catch (caught) {
      setModels(undefined);
      setGuidedError(guidedSetupGuidance(caught));
    } finally {
      setStep("idle");
    }
  };

  const checkLatency = async () => {
    if (!model) return;
    setStep("checking");
    setGuidedError(undefined);
    setLatency(undefined);
    try {
      setLatency({ kind: "report", report: await setupClient.checkLatency(endpoint, model) });
    } catch (caught) {
      if (caught instanceof RuntimeSetupError && caught.category === "benchmark_cancelled") {
        setLatency({ kind: "cancelled" });
      } else {
        setGuidedError(guidedSetupGuidance(caught));
      }
    } finally {
      setStep("idle");
    }
  };

  const connectGuided = async () => {
    if (!model) return;
    setStep("preparing");
    setGuidedError(undefined);
    setLastConnection("guided");
    try {
      const paths = await setupClient.prepareConnection(endpoint, model);
      setStep("idle");
      await onConnect(paths);
    } catch (caught) {
      setGuidedError(guidedSetupGuidance(caught));
      setStep("idle");
    }
  };

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
      setLastConnection("manual");
      void onConnect({ gatewayPath, configPath });
    }
  };

  const connectionError = (
    <p className="setup-error" role="alert">
      {error}
    </p>
  );

  return (
    <main className="setup-view">
      <section className="setup-panel" aria-labelledby="setup-title">
        <p className="utility-label">Local runtime setup</p>
        <h1 id="setup-title">Conversation Runtime</h1>
        <p className="setup-introduction">
          Pick a model from a local model host on this Mac, check how fast it replies, then connect.
        </p>
        <form aria-label="Local model setup" onSubmit={(event) => void findModels(event)} noValidate>
          <label htmlFor="setup-endpoint">Local model address</label>
          <input
            id="setup-endpoint"
            autoComplete="off"
            onChange={(event) => {
              setEndpoint(event.target.value);
              setModels(undefined);
              setModel(undefined);
              setLatency(undefined);
              setGuidedError(undefined);
            }}
            spellCheck={false}
            value={endpoint}
          />
          <button className="secondary-action" disabled={busy} type="submit">
            {step === "discovering" ? "Finding models…" : "Find local models"}
          </button>
        </form>

        {models ? (
          models.length === 0 ? (
            <p className="setup-note">This model host has no models installed yet.</p>
          ) : (
            <div className="setup-models">
              <fieldset disabled={busy}>
                <legend>Model</legend>
                {models.map((name) => (
                  <label className="setup-model-option" key={name}>
                    <input
                      checked={model === name}
                      name="setup-model"
                      onChange={() => {
                        setModel(name);
                        setLatency(undefined);
                      }}
                      type="radio"
                      value={name}
                    />
                    <span>{name}</span>
                  </label>
                ))}
              </fieldset>
              <div className="setup-latency">
                {step === "checking" ? (
                  <button className="quiet-action" onClick={() => void setupClient.cancelLatencyCheck()} type="button">
                    Cancel check
                  </button>
                ) : (
                  <button className="quiet-action" disabled={busy} onClick={() => void checkLatency()} type="button">
                    Check response time
                  </button>
                )}
                <div role="status">
                  {step === "checking" ? <p className="setup-note">Checking response time…</p> : null}
                  {latency?.kind === "report" ? (
                    <>
                      <p className="setup-latency-result">
                        First words after {formatDuration(latency.report.firstDeltaMs)}. Full reply after{" "}
                        {formatDuration(latency.report.totalMs)}.
                      </p>
                      <p className="setup-note">
                        One run of a short fixed prompt. It measures speed only, not answer or voice quality.
                      </p>
                    </>
                  ) : null}
                  {latency?.kind === "cancelled" ? (
                    <p className="setup-note">Response time check cancelled.</p>
                  ) : null}
                </div>
              </div>
              <button className="primary-action" disabled={busy} onClick={() => void connectGuided()} type="button">
                {step === "preparing" || (connecting && lastConnection === "guided")
                  ? "Connecting…"
                  : "Use this model and connect"}
              </button>
            </div>
          )
        ) : null}
        {guidedError ? (
          <p className="setup-error" role="alert">
            {guidedError}
          </p>
        ) : null}
        {error && lastConnection === "guided" ? connectionError : null}

        <h2 className="setup-advanced-title">Advanced setup</h2>
        <p className="setup-note">Connect a gateway and configuration file you manage yourself.</p>
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

          {error && lastConnection === "manual" ? connectionError : null}
          <button className="primary-action" disabled={busy} type="submit">
            {connecting && lastConnection === "manual" ? "Connecting…" : "Connect local runtime"}
          </button>
        </form>
      </section>
    </main>
  );
}

function formatDuration(milliseconds: number): string {
  if (milliseconds < 1_000) return `${milliseconds} ms`;
  return `${(milliseconds / 1_000).toFixed(1)} s`;
}

function guidedSetupGuidance(error: unknown): string {
  const category = error instanceof RuntimeSetupError ? error.category : "unknown";
  switch (category) {
    case "invalid_endpoint":
      return "Use a local address on this Mac, such as http://127.0.0.1:11434.";
    case "provider_unavailable":
    case "provider_spawn_failed":
    case "provider_exited_before_ready":
    case "provider_startup_cancelled":
      return "No local model host answered at this address. Start it, then try again.";
    case "provider_startup_timeout":
      return "The local model host did not become ready in time. Try again once it has started.";
    case "model_discovery_failed":
      return "No model list came back from this address. Check that a local model host is running there, then try again.";
    case "benchmark_failed":
      return "The response time check did not finish. Check that the model host is still running, then try again.";
    case "configuration_failed":
      return "The private runtime configuration could not be saved. Check that app storage is available, then try again.";
    case "bundled_runtime_missing":
      return "This app is missing its bundled runtime. Reinstall the app, or use advanced setup.";
    default:
      return "Local setup did not finish. Try again, or use advanced setup.";
  }
}
