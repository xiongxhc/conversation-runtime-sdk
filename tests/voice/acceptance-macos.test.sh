#!/bin/sh
set -eu

REPOSITORY_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
HARNESS="$REPOSITORY_ROOT/tests/voice/acceptance-macos.sh"
FIXTURE_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/conversation-runtime-acceptance-test.XXXXXX")
trap 'rm -rf "$FIXTURE_ROOT"' EXIT HUP INT TERM

CONFIG="$FIXTURE_ROOT/voice-session.toml"
printf '%s\n' 'schema_version = 2' >"$CONFIG"

assert_fails() {
    if "$@" >"$FIXTURE_ROOT/assert.stdout" 2>"$FIXTURE_ROOT/assert.stderr"; then
        printf '%s\n' "expected command to fail: $*" >&2
        exit 1
    fi
}

assert_fails "$HARNESS" \
    --config voice-session.toml \
    --duration-seconds 1 \
    --metrics "$FIXTURE_ROOT/relative.jsonl"

assert_fails "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$REPOSITORY_ROOT/tests/voice/forbidden-metrics.jsonl"
test ! -e "$REPOSITORY_ROOT/tests/voice/forbidden-metrics.jsonl"

FAKE_LOOP="$FIXTURE_ROOT/fake-voice-loop.sh"
cat >"$FAKE_LOOP" <<'EOF'
#!/usr/bin/ruby
owned_child = Process.spawn("/bin/sleep", "30")
shutdown = proc do
  Process.kill("TERM", owned_child) rescue nil
  Process.wait(owned_child) rescue nil
  warn "status=cancelled"
  exit 130
end
Signal.trap("INT", &shutdown)
Signal.trap("TERM", &shutdown)
warn "privacy=local-only"
warn "turn=4 milestone=speech_end elapsed_ms=12"
warn "turn=4 generation=7 status=interrupted"
warn "stale_generation=rejected"
warn "metric=underrun_count count=2"
warn "metric=queue_depth_frames count=4"
warn "private synthesized response must not persist"
Process.wait(owned_child)
EOF
chmod 700 "$FAKE_LOOP"

METRICS="$FIXTURE_ROOT/metrics.jsonl"
CONVERSATION_VOICE_LOOP_BIN="$FAKE_LOOP" "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$METRICS"

jq -e . "$METRICS" >/dev/null
grep -q '"metric":"speech_end_ms"' "$METRICS"
grep -q '"turn_id":4' "$METRICS"
grep -q '"session_result":"duration-complete"' "$METRICS"
grep -q '"interruption_count":1' "$METRICS"
grep -q '"session_reset_count":0' "$METRICS"
grep -q '"stale_generation_reject_count":1' "$METRICS"
grep -q '"stale_generation_observed":true' "$METRICS"
grep -q '"queue_underrun_count":2' "$METRICS"
grep -q '"queue_underrun_observed":true' "$METRICS"
grep -q '"metric":"queue_depth_frames","count":4' "$METRICS"
grep -q '"orphaned_child_count":0' "$METRICS"
grep -q '"process_inspection_failure_count":0' "$METRICS"
if grep -q 'private synthesized response' "$METRICS"; then
    printf '%s\n' 'sensitive fixture content reached metrics' >&2
    exit 1
fi

FAILING_LOOP="$FIXTURE_ROOT/failing-voice-loop.sh"
cat >"$FAILING_LOOP" <<'EOF'
#!/bin/sh
printf '%s\n' 'status=error stage=speech_synthesizer error=private synthesized response' >&2
exit 1
EOF
chmod 700 "$FAILING_LOOP"

FAILED_METRICS="$FIXTURE_ROOT/failed.jsonl"
assert_fails env CONVERSATION_VOICE_LOOP_BIN="$FAILING_LOOP" "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 5 \
    --metrics "$FAILED_METRICS"
jq -e . "$FAILED_METRICS" >/dev/null
grep -q '"session_result":"failed"' "$FAILED_METRICS"
grep -q '"stage":"speech_synthesizer"' "$FAILED_METRICS"
grep -q '"stale_generation_reject_count":null' "$FAILED_METRICS"
grep -q '"stale_generation_observed":false' "$FAILED_METRICS"
grep -q '"queue_underrun_count":null' "$FAILED_METRICS"
grep -q '"queue_underrun_observed":false' "$FAILED_METRICS"
if grep -q 'private synthesized response' "$FAILED_METRICS"; then
    printf '%s\n' 'failure content reached metrics' >&2
    exit 1
fi

ORPHAN_PID="$FIXTURE_ROOT/orphan.pid"
ORPHANING_LOOP="$FIXTURE_ROOT/orphaning-voice-loop.sh"
cat >"$ORPHANING_LOOP" <<EOF
#!/usr/bin/ruby
child = Process.spawn("/bin/sleep", "30")
File.write("$ORPHAN_PID", child.to_s)
sleep 0.5
exit 1
EOF
chmod 700 "$ORPHANING_LOOP"

ORPHAN_METRICS="$FIXTURE_ROOT/orphan.jsonl"
assert_fails env CONVERSATION_VOICE_LOOP_BIN="$ORPHANING_LOOP" "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 5 \
    --metrics "$ORPHAN_METRICS"
jq -e . "$ORPHAN_METRICS" >/dev/null
grep -q '"orphaned_child_count":1' "$ORPHAN_METRICS"
orphan_pid=$(cat "$ORPHAN_PID")
if kill -0 "$orphan_pid" 2>/dev/null; then
    printf '%s\n' 'harness left a fixture child running' >&2
    kill -KILL "$orphan_pid" 2>/dev/null || true
    exit 1
fi

printf '%s\n' 'acceptance harness tests passed'
