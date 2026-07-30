#!/bin/sh
set -eu

REPOSITORY_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
HARNESS="$REPOSITORY_ROOT/tests/voice/acceptance-macos.sh"
FIXTURE_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/conversation-runtime-acceptance-test.XXXXXX")
outside_pid=

cleanup() {
    if [ -n "$outside_pid" ]; then
        kill -KILL "$outside_pid" 2>/dev/null || true
        wait "$outside_pid" 2>/dev/null || true
    fi
    for pid_file in "$FIXTURE_ROOT"/*.pid; do
        [ -f "$pid_file" ] || continue
        pid=$(cat "$pid_file")
        case "$pid" in
            ''|*[!0-9]*) continue ;;
        esac
        kill -KILL "$pid" 2>/dev/null || true
    done
    rm -rf "$FIXTURE_ROOT"
}
trap cleanup EXIT HUP INT TERM

CONFIG="$FIXTURE_ROOT/voice-session.toml"
printf '%s\n' 'schema_version = 2' >"$CONFIG"

assert_fails() {
    if "$@" >"$FIXTURE_ROOT/assert.stdout" 2>"$FIXTURE_ROOT/assert.stderr"; then
        printf '%s\n' "expected command to fail: $*" >&2
        exit 1
    fi
}

assert_fails_bounded() {
    timeout_marker="$FIXTURE_ROOT/assert-timeout"
    rm -f "$timeout_marker"
    "$@" >"$FIXTURE_ROOT/assert.stdout" 2>"$FIXTURE_ROOT/assert.stderr" &
    command_pid=$!
    /usr/bin/perl -e '
        my ($pid, $marker) = @ARGV;
        sleep 2;
        exit 0 unless kill 0, $pid;
        open my $handle, ">", $marker or exit 2;
        close $handle;
        kill "TERM", $pid;
        sleep 1;
        kill "KILL", $pid if kill 0, $pid;
    ' "$command_pid" "$timeout_marker" &
    guard_pid=$!
    if wait "$command_pid"; then
        command_status=0
    else
        command_status=$?
    fi
    kill "$guard_pid" 2>/dev/null || true
    wait "$guard_pid" 2>/dev/null || true
    if [ -f "$timeout_marker" ]; then
        printf '%s\n' "command did not fail promptly: $*" >&2
        exit 1
    fi
    if [ "$command_status" -eq 0 ]; then
        printf '%s\n' "expected command to fail: $*" >&2
        exit 1
    fi
}

assert_process_gone() {
    process_pid=$1
    attempts=0
    while kill -0 "$process_pid" 2>/dev/null; do
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 40 ]; then
            printf '%s\n' "fixture process remains alive: $process_pid" >&2
            return 1
        fi
        sleep 0.05
    done
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

ln -s "$REPOSITORY_ROOT/tests/voice" "$FIXTURE_ROOT/repository-alias"
assert_fails env CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$FIXTURE_ROOT/repository-alias/forbidden-alias-metrics.jsonl"
test ! -e "$REPOSITORY_ROOT/tests/voice/forbidden-alias-metrics.jsonl"

EXISTING_METRICS="$FIXTURE_ROOT/existing.jsonl"
printf '%s\n' 'existing metrics must remain unchanged' >"$EXISTING_METRICS"
chmod 644 "$EXISTING_METRICS"
assert_fails env CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$EXISTING_METRICS"
test "$(cat "$EXISTING_METRICS")" = 'existing metrics must remain unchanged'
test "$(stat -f '%Lp' "$EXISTING_METRICS")" = 644

SYMLINK_SOURCE="$FIXTURE_ROOT/symlink-source"
SYMLINK_METRICS="$FIXTURE_ROOT/symlink.jsonl"
printf '%s\n' 'symlink source must remain unchanged' >"$SYMLINK_SOURCE"
ln -s "$SYMLINK_SOURCE" "$SYMLINK_METRICS"
assert_fails env CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$SYMLINK_METRICS"
test "$(cat "$SYMLINK_SOURCE")" = 'symlink source must remain unchanged'

HARD_LINK_SOURCE="$FIXTURE_ROOT/hard-link-source"
HARD_LINK_METRICS="$FIXTURE_ROOT/hard-link.jsonl"
printf '%s\n' 'hard link source must remain unchanged' >"$HARD_LINK_SOURCE"
ln "$HARD_LINK_SOURCE" "$HARD_LINK_METRICS"
assert_fails env CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$HARD_LINK_METRICS"
test "$(cat "$HARD_LINK_SOURCE")" = 'hard link source must remain unchanged'
test "$(stat -f '%l' "$HARD_LINK_SOURCE")" = 2

FIFO_METRICS="$FIXTURE_ROOT/metrics.fifo"
mkfifo "$FIFO_METRICS"
assert_fails_bounded env CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$FIFO_METRICS"
test -p "$FIFO_METRICS"

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
if ! CONVERSATION_VOICE_LOOP_BIN="$FAKE_LOOP" "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$METRICS"; then
    cat "$METRICS" >&2
    exit 1
fi

test "$(stat -f '%Lp' "$METRICS")" = 600
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

PUBLIC_OUTPUT="$FIXTURE_ROOT/public-output"
mkdir "$PUBLIC_OUTPUT"
chmod 755 "$PUBLIC_OUTPUT"
PUBLIC_METRICS="$PUBLIC_OUTPUT/metrics.jsonl"
CONVERSATION_VOICE_LOOP_BIN="$FAKE_LOOP" "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$PUBLIC_METRICS"
test "$(stat -f '%Lp' "$PUBLIC_OUTPUT")" = 755
test "$(stat -f '%Lp' "$PUBLIC_METRICS")" = 600

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

IMMEDIATE_PID="$FIXTURE_ROOT/immediate.pid"
LATE_PID="$FIXTURE_ROOT/late.pid"
LATE_CHILD_LOOP="$FIXTURE_ROOT/late-child-voice-loop.sh"
cat >"$LATE_CHILD_LOOP" <<EOF
#!/usr/bin/ruby
child = fork do
  File.write("$IMMEDIATE_PID", Process.pid.to_s)
  Signal.trap("TERM") do
    late = Process.spawn("/bin/sleep", "30")
    File.write("$LATE_PID", late.to_s)
    sleep 30
  end
  sleep 30
end
exit 1
EOF
chmod 700 "$LATE_CHILD_LOOP"

/bin/sleep 30 &
outside_pid=$!
LATE_CHILD_METRICS="$FIXTURE_ROOT/late-child.jsonl"
assert_fails env CONVERSATION_VOICE_LOOP_BIN="$LATE_CHILD_LOOP" "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 5 \
    --metrics "$LATE_CHILD_METRICS"
jq -e . "$LATE_CHILD_METRICS" >/dev/null
grep -Eq '"orphaned_child_count":[1-9][0-9]*' "$LATE_CHILD_METRICS"
test -f "$IMMEDIATE_PID"
test -f "$LATE_PID"
assert_process_gone "$(cat "$IMMEDIATE_PID")"
assert_process_gone "$(cat "$LATE_PID")"
kill -0 "$outside_pid"
kill -TERM "$outside_pid"
wait "$outside_pid" 2>/dev/null || true
outside_pid=

printf '%s\n' 'acceptance harness tests passed'
