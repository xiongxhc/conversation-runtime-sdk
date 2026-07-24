#!/usr/bin/env bash
set -euo pipefail
umask 077

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "Usage: tests/ollama/benchmark-local.sh <model> [output-name]" >&2
  exit 2
fi

for required_command in rustup curl jq shasum awk git sysctl sw_vers ollama perl; do
  if ! command -v "$required_command" >/dev/null 2>&1; then
    echo "Required command not found: $required_command" >&2
    exit 1
  fi
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

normalize_timeout_milliseconds() {
  local name="$1"
  local raw_value="$2"
  local normalized_value

  case "$raw_value" in
    "" | *[!0-9]*)
      echo "$name must be positive integer milliseconds." >&2
      return 2
      ;;
  esac
  if [[ "${#raw_value}" -gt 6 ]]; then
    echo "$name must be between 1 and 600000 milliseconds." >&2
    return 2
  fi

  normalized_value=$((10#$raw_value))
  if [[ "$normalized_value" -eq 0 || "$normalized_value" -gt 600000 ]]; then
    echo "$name must be between 1 and 600000 milliseconds." >&2
    return 2
  fi

  printf '%s\n' "$normalized_value"
}

if ! git diff --quiet || ! git diff --cached --quiet ||
  [[ -n "$(git ls-files --others --exclude-standard)" ]]; then
  echo "Benchmark requires a clean Git working tree so HEAD identifies the exact source." >&2
  exit 1
fi

model="$1"
safe_model="$(printf '%s' "$model" | tr '/: ' '___')"
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
output_name="${2:-${timestamp}-${safe_model}-$$}"
prompt="Answer in two short spoken sentences: What makes a conversation feel natural?"
endpoint="${OLLAMA_ENDPOINT:-http://127.0.0.1:11434}"
first_delta_timeout_ms="$(
  normalize_timeout_milliseconds \
    OLLAMA_FIRST_DELTA_TIMEOUT_MS \
    "${OLLAMA_FIRST_DELTA_TIMEOUT_MS:-60000}"
)"
idle_timeout_ms="$(
  normalize_timeout_milliseconds \
    OLLAMA_IDLE_TIMEOUT_MS \
    "${OLLAMA_IDLE_TIMEOUT_MS:-30000}"
)"
total_timeout_ms="$(
  normalize_timeout_milliseconds \
    OLLAMA_TOTAL_TIMEOUT_MS \
    "${OLLAMA_TOTAL_TIMEOUT_MS:-120000}"
)"
wrapper_timeout_seconds=$(((total_timeout_ms + 999) / 1000 + 15))

if [[ -z "$output_name" || "$output_name" == "." || "$output_name" == ".." ||
  "$output_name" == *"/"* ]]; then
  echo "Output name must be one new direct child name without '/'." >&2
  exit 2
fi

case "$endpoint" in
  http://127.0.0.1 | http://127.0.0.1:* | http://localhost | http://localhost:*) ;;
  *)
    echo "Local benchmark endpoint must use loopback HTTP without credentials." >&2
    exit 2
    ;;
esac

if [[ "$endpoint" == *"@"* || "$endpoint" == *"?"* || "$endpoint" == *"#"* ]]; then
  echo "Local benchmark endpoint cannot contain credentials, a query, or a fragment." >&2
  exit 2
fi

artifact_parent="$repo_root/artifacts"
artifact_root="$artifact_parent/ollama"
if [[ -L "$artifact_parent" || -L "$artifact_root" ]]; then
  echo "Artifact directories cannot be symbolic links." >&2
  exit 2
fi
if [[ ! -e "$artifact_parent" ]]; then
  mkdir "$artifact_parent"
fi
if [[ ! -d "$artifact_parent" ]]; then
  echo "Artifact parent is not a directory: artifacts" >&2
  exit 2
fi
if [[ ! -e "$artifact_root" ]]; then
  mkdir "$artifact_root"
fi
if [[ ! -d "$artifact_root" ]]; then
  echo "Artifact root is not a directory: artifacts/ollama" >&2
  exit 2
fi

output_directory="$artifact_root/$output_name"
relative_output_directory="artifacts/ollama/$output_name"
if [[ -e "$output_directory" || -L "$output_directory" ]]; then
  echo "Output directory already exists: $relative_output_directory" >&2
  exit 2
fi
mkdir "$output_directory"

run_with_timeout() {
  local timeout_seconds="$1"
  shift
  perl -e 'alarm shift @ARGV; exec @ARGV or exit 127' "$timeout_seconds" "$@"
}

curl_json() {
  curl --fail --silent --show-error \
    --connect-timeout 5 \
    --max-time 10 \
    "$@"
}

api_base="${endpoint%/}"
curl_processes() {
  curl --fail --silent --show-error \
    --connect-timeout 1 \
    --max-time 1 \
    "$api_base/api/ps"
}

validate_processes_json() {
  jq -e '
    (.models | type == "array") and
    all(
      .models[];
      type == "object" and
      ((has("name") | not) or (.name | type == "string")) and
      ((has("model") | not) or (.model | type == "string")) and
      (has("name") or has("model"))
    )
  ' >/dev/null
}

unload_model() {
  local unload_request
  local processes_json
  local loaded_state
  unload_request="$(jq -cn --arg model "$model" '{model:$model,keep_alive:0}')"
  curl_json \
    -H "Content-Type: application/json" \
    -d "$unload_request" \
    "$api_base/api/generate" >/dev/null

  local attempt
  for attempt in $(seq 1 20); do
    if ! processes_json="$(curl_processes)"; then
      echo "Could not read Ollama process state while unloading: $model" >&2
      return 1
    fi
    if ! printf '%s' "$processes_json" | validate_processes_json; then
      echo "Ollama process response did not match the expected model schema." >&2
      return 1
    fi
    if ! loaded_state="$(
      printf '%s' "$processes_json" |
        jq -r --arg model "$model" \
          'any(.models[]; (.name == $model or .model == $model))'
    )"; then
      echo "Could not evaluate Ollama process state while unloading: $model" >&2
      return 1
    fi
    case "$loaded_state" in
      false) return 0 ;;
      true) ;;
      *)
        echo "Ollama process-state query returned an unexpected value." >&2
        return 1
        ;;
    esac
    sleep 0.5
  done

  echo "Model did not unload within the bounded polling window: $model" >&2
  return 1
}

capture_loaded_state() {
  local processes_json
  local match_count
  if ! processes_json="$(curl_processes)"; then
    echo "Could not read loaded Ollama state for benchmark evidence." >&2
    return 1
  fi
  if ! printf '%s' "$processes_json" | validate_processes_json; then
    echo "Loaded Ollama state did not match the expected model schema." >&2
    return 1
  fi
  match_count="$(
    printf '%s' "$processes_json" |
      jq -r --arg model "$model" \
        '[.models[] | select(.name == $model or .model == $model)] | length'
  )"
  if [[ "$match_count" -ne 1 ]]; then
    echo "Expected one loaded model entry after warm-up, found $match_count." >&2
    return 1
  fi
  printf '%s' "$processes_json" |
    jq --arg model "$model" \
      '.models[] | select(.name == $model or .model == $model)' \
      >"$output_directory/loaded-state.json"
}

cleanup() {
  local original_exit_code=$?
  local cleanup_exit_code=0
  trap - EXIT
  set +e
  if unload_model; then
    if ! printf '%s\n' "success" >"$output_directory/cleanup-status.txt"; then
      cleanup_exit_code=1
      echo "Could not record successful benchmark cleanup." >&2
    fi
  else
    cleanup_exit_code=1
    if ! printf '%s\n' "failed" >"$output_directory/cleanup-status.txt"; then
      echo "Could not record failed benchmark cleanup." >&2
    fi
    echo "Benchmark cleanup failed; model state may still be loaded." >&2
  fi
  if [[ "$original_exit_code" -eq 0 && "$cleanup_exit_code" -ne 0 ]]; then
    exit "$cleanup_exit_code"
  fi
  exit "$original_exit_code"
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP

cargo_binary="${CARGO:-$(rustup which cargo)}"
rustc_binary="$(rustup which rustc)"
export PATH="$(dirname "$rustc_binary"):$PATH"

run_with_timeout 300 "$cargo_binary" build --locked -p conversation-ollama-probe
probe_binary="$repo_root/target/debug/conversation-ollama-probe"
probe_sha256="$(shasum -a 256 "$probe_binary" | awk '{print $1}')"
tags_json="$(curl_json "$api_base/api/tags")"
model_digests="$(
  printf '%s' "$tags_json" |
    jq -r --arg model "$model" \
      '.models[]? | select(.name == $model or .model == $model) | (.digest // empty)'
)"
model_digest_count="$(
  printf '%s\n' "$model_digests" |
    awk 'NF { count += 1 } END { print count + 0 }'
)"

if [[ "$model_digest_count" -ne 1 ]]; then
  echo "Expected exactly one installed digest for model, found $model_digest_count: $model" >&2
  exit 1
fi
model_digest="$(printf '%s\n' "$model_digests" | awk 'NF { print; exit }')"

cat >"$output_directory/manifest.txt" <<EOF
timestamp_utc=$timestamp
git_commit=$(git rev-parse HEAD)
git_tree_state=clean
build_command=cargo build --locked -p conversation-ollama-probe
cargo_version=$("$cargo_binary" --version)
rustc_version=$("$rustc_binary" --version)
cargo_lock_sha256=$(shasum -a 256 Cargo.lock | awk '{print $1}')
probe_sha256=$probe_sha256
endpoint=$endpoint
model=$model
model_digest=$model_digest
prompt=$prompt
think=false
temperature=0
seed=42
num_predict=128
num_ctx=8192
first_delta_timeout_ms=$first_delta_timeout_ms
idle_timeout_ms=$idle_timeout_ms
total_timeout_ms=$total_timeout_ms
machine_model=$(sysctl -n hw.model)
chip=$(sysctl -n machdep.cpu.brand_string)
logical_cpu_count=$(sysctl -n hw.logicalcpu)
memory_bytes=$(sysctl -n hw.memsize)
macos_version=$(sw_vers -productVersion)
ollama_version=$(ollama --version 2>&1 | tail -n 1)
EOF

export OLLAMA_ENDPOINT="$endpoint"
export OLLAMA_FIRST_DELTA_TIMEOUT_MS="$first_delta_timeout_ms"
export OLLAMA_IDLE_TIMEOUT_MS="$idle_timeout_ms"
export OLLAMA_TOTAL_TIMEOUT_MS="$total_timeout_ms"

unload_model

run_probe() {
  local label="$1"
  set +e
  printf '%s\n' "$prompt" |
    run_with_timeout "$wrapper_timeout_seconds" "$probe_binary" "$model" \
      >"$output_directory/${label}.response.txt" \
      2>"$output_directory/${label}.metrics.txt"
  local exit_code=$?
  set -e
  if ! printf '%s\n' "$exit_code" >"$output_directory/${label}.exit-code.txt"; then
    echo "Could not record probe exit code for $label." >&2
    return 125
  fi
  if ! cat "$output_directory/${label}.metrics.txt"; then
    echo "Could not read probe metrics for $label." >&2
    return 125
  fi
  return "$exit_code"
}

if ! run_probe warmup; then
  echo "Warm-up failed; evidence retained in $relative_output_directory" >&2
  exit 1
fi
capture_loaded_state

for run in 1 2 3; do
  if ! run_probe "run-${run}"; then
    echo "Measured run ${run} failed; evidence retained in $relative_output_directory" >&2
    exit 1
  fi
done

unload_model
printf '%s\n' "success" >"$output_directory/cleanup-status.txt"
trap - EXIT
echo "Benchmark evidence: $relative_output_directory"
