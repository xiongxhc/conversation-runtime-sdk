#!/bin/sh
set -eu

REPOSITORY_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P)
REAL_HARNESS="$REPOSITORY_ROOT/tests/voice/acceptance-macos.sh"
FIXTURE_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/conversation-runtime-acceptance-test.XXXXXX")
FIXTURE_ROOT=$(CDPATH= cd -- "$FIXTURE_ROOT" && pwd -P)
HARNESS="$FIXTURE_ROOT/acceptance-macos-zero-thresholds.sh"
HELPER="$FIXTURE_ROOT/acceptance-helper"
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
printf '%s\n' 'schema_version = 1' >"$CONFIG"

/usr/bin/xcrun clang \
    -std=c11 \
    -Wall \
    -Wextra \
    -Werror \
    -DACCEPTANCE_HELPER_TESTING \
    "$REPOSITORY_ROOT/tests/voice/acceptance-helper.c" \
    -o "$HELPER"
export CONVERSATION_ACCEPTANCE_HELPER_BIN="$HELPER"

cat >"$HARNESS" <<EOF
#!/bin/sh
exec "$REAL_HARNESS" \
    --minimum-completed-turns 1 \
    --minimum-interruptions 0 \
    "\$@"
EOF
chmod 700 "$HARNESS"

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
        sleep 8;
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

wait_for_path() {
    awaited_path=$1
    awaited_pid=$2
    attempts=0
    while [ ! -e "$awaited_path" ]; do
        if ! kill -0 "$awaited_pid" 2>/dev/null; then
            wait "$awaited_pid" 2>/dev/null || true
            printf '%s\n' "fixture command exited before creating: $awaited_path" >&2
            return 1
        fi
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 400 ]; then
            printf '%s\n' "timed out waiting for fixture path: $awaited_path" >&2
            return 1
        fi
        sleep 0.01
    done
}

assert_background_fails() {
    background_pid=$1
    if wait "$background_pid"; then
        printf '%s\n' "expected background command to fail: $background_pid" >&2
        return 1
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

atomic_swap_paths() {
    /usr/bin/perl -e '
        use constant AT_FDCWD => -2;
        use constant RENAME_SWAP => 2;
        use constant SYS_RENAMEATX_NP => 488;
        syscall(
            SYS_RENAMEATX_NP,
            AT_FDCWD,
            $ARGV[0],
            AT_FDCWD,
            $ARGV[1],
            RENAME_SWAP
        ) == 0 or die "renameatx_np RENAME_SWAP failed\n";
    ' "$1" "$2"
}

HELP_OUTPUT=$("$HARNESS" --help)
printf '%s\n' "$HELP_OUTPUT" | grep -q 'trusted local operator account'
printf '%s\n' "$HELP_OUTPUT" | grep -q 'malicious same-EUID process'
printf '%s\n' "$HELP_OUTPUT" | grep -q 'setpgid/setsid'

assert_fails env CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true "$REAL_HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$FIXTURE_ROOT/missing-thresholds.jsonl"
assert_fails env CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true "$REAL_HARNESS" \
    --config "$CONFIG" \
    --minimum-completed-turns -1 \
    --minimum-interruptions 0 \
    --duration-seconds 1 \
    --metrics "$FIXTURE_ROOT/negative-threshold.jsonl"
assert_fails env CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true "$REAL_HARNESS" \
    --config "$CONFIG" \
    --minimum-completed-turns 10001 \
    --minimum-interruptions 0 \
    --duration-seconds 1 \
    --metrics "$FIXTURE_ROOT/excessive-threshold.jsonl"
assert_fails env CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true "$REAL_HARNESS" \
    --config "$CONFIG" \
    --minimum-completed-turns 01 \
    --minimum-interruptions 0 \
    --duration-seconds 1 \
    --metrics "$FIXTURE_ROOT/noncanonical-threshold.jsonl"
assert_fails env CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true "$REAL_HARNESS" \
    --config "$CONFIG" \
    --minimum-completed-turns 1 \
    --minimum-interruptions 0 \
    --duration-seconds 01 \
    --metrics "$FIXTURE_ROOT/noncanonical-duration.jsonl"

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

UNSAFE_OUTPUT="$FIXTURE_ROOT/unsafe-output"
mkdir "$UNSAFE_OUTPUT"
chmod 777 "$UNSAFE_OUTPUT"
assert_fails env CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$UNSAFE_OUTPUT/metrics.jsonl"
test ! -e "$UNSAFE_OUTPUT/metrics.jsonl"

PARENT_SWAP_ROOT="$FIXTURE_ROOT/parent-swap"
PARENT_SWAP_OUTPUT="$PARENT_SWAP_ROOT/output"
PARENT_SWAP_ORIGINAL="$PARENT_SWAP_ROOT/output-original"
PARENT_SWAP_MARKER="$FIXTURE_ROOT/parent-swap-ready"
mkdir -p "$PARENT_SWAP_OUTPUT"
chmod 700 "$PARENT_SWAP_ROOT" "$PARENT_SWAP_OUTPUT"
env \
    CONVERSATION_ACCEPTANCE_TEST_METRICS_READY_MARKER="$PARENT_SWAP_MARKER" \
    CONVERSATION_ACCEPTANCE_TEST_METRICS_READY_DELAY_MS=500 \
    CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true \
    "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$PARENT_SWAP_OUTPUT/metrics.jsonl" \
    >"$FIXTURE_ROOT/parent-swap.stdout" \
    2>"$FIXTURE_ROOT/parent-swap.stderr" &
parent_swap_pid=$!
wait_for_path "$PARENT_SWAP_MARKER" "$parent_swap_pid"
mv "$PARENT_SWAP_OUTPUT" "$PARENT_SWAP_ORIGINAL"
mkdir "$PARENT_SWAP_OUTPUT"
chmod 700 "$PARENT_SWAP_OUTPUT"
assert_background_fails "$parent_swap_pid"
test ! -e "$PARENT_SWAP_OUTPUT/metrics.jsonl"
test ! -e "$PARENT_SWAP_ORIGINAL/metrics.jsonl"

REPOSITORY_REDIRECT_ROOT="$FIXTURE_ROOT/repository-redirect"
REPOSITORY_REDIRECT_OUTPUT="$REPOSITORY_REDIRECT_ROOT/output"
REPOSITORY_REDIRECT_ORIGINAL="$REPOSITORY_REDIRECT_ROOT/output-original"
REPOSITORY_REDIRECT_MARKER="$FIXTURE_ROOT/repository-redirect-ready"
REPOSITORY_REDIRECT_METRICS="$REPOSITORY_ROOT/tests/voice/forbidden-race-metrics.jsonl"
mkdir -p "$REPOSITORY_REDIRECT_OUTPUT"
chmod 700 "$REPOSITORY_REDIRECT_ROOT" "$REPOSITORY_REDIRECT_OUTPUT"
env \
    CONVERSATION_ACCEPTANCE_TEST_METRICS_READY_MARKER="$REPOSITORY_REDIRECT_MARKER" \
    CONVERSATION_ACCEPTANCE_TEST_METRICS_READY_DELAY_MS=500 \
    CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true \
    "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$REPOSITORY_REDIRECT_OUTPUT/forbidden-race-metrics.jsonl" \
    >"$FIXTURE_ROOT/repository-redirect.stdout" \
    2>"$FIXTURE_ROOT/repository-redirect.stderr" &
repository_redirect_pid=$!
wait_for_path "$REPOSITORY_REDIRECT_MARKER" "$repository_redirect_pid"
mv "$REPOSITORY_REDIRECT_OUTPUT" "$REPOSITORY_REDIRECT_ORIGINAL"
ln -s "$REPOSITORY_ROOT/tests/voice" "$REPOSITORY_REDIRECT_OUTPUT"
assert_background_fails "$repository_redirect_pid"
test ! -e "$REPOSITORY_REDIRECT_METRICS"
test ! -e "$REPOSITORY_REDIRECT_ORIGINAL/forbidden-race-metrics.jsonl"

PARENT_LINK_ROOT="$FIXTURE_ROOT/parent-link"
PARENT_LINK_OUTPUT="$PARENT_LINK_ROOT/output"
PARENT_LINK_MARKER="$FIXTURE_ROOT/parent-link-ready"
mkdir -p "$PARENT_LINK_OUTPUT"
chmod 700 "$PARENT_LINK_ROOT" "$PARENT_LINK_OUTPUT"
env \
    CONVERSATION_ACCEPTANCE_TEST_METRICS_READY_MARKER="$PARENT_LINK_MARKER" \
    CONVERSATION_ACCEPTANCE_TEST_METRICS_READY_DELAY_MS=500 \
    CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true \
    "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$PARENT_LINK_OUTPUT/metrics.jsonl" \
    >"$FIXTURE_ROOT/parent-link.stdout" \
    2>"$FIXTURE_ROOT/parent-link.stderr" &
parent_link_pid=$!
wait_for_path "$PARENT_LINK_MARKER" "$parent_link_pid"
mkdir "$PARENT_LINK_OUTPUT/injected-directory"
rmdir "$PARENT_LINK_OUTPUT/injected-directory"
assert_background_fails "$parent_link_pid"
test ! -e "$PARENT_LINK_OUTPUT/metrics.jsonl"

STAGE_EVENT_ROOT="$FIXTURE_ROOT/stage-event"
STAGE_EVENT_OUTPUT="$STAGE_EVENT_ROOT/output"
STAGE_EVENT_MARKER="$FIXTURE_ROOT/stage-event-ready"
mkdir -p "$STAGE_EVENT_OUTPUT"
chmod 700 "$STAGE_EVENT_ROOT" "$STAGE_EVENT_OUTPUT"
env \
    CONVERSATION_ACCEPTANCE_TEST_METRICS_READY_MARKER="$STAGE_EVENT_MARKER" \
    CONVERSATION_ACCEPTANCE_TEST_METRICS_READY_DELAY_MS=500 \
    CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true \
    "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$STAGE_EVENT_OUTPUT/metrics.jsonl" \
    >"$FIXTURE_ROOT/stage-event.stdout" \
    2>"$FIXTURE_ROOT/stage-event.stderr" &
stage_event_pid=$!
wait_for_path "$STAGE_EVENT_MARKER" "$stage_event_pid"
set -- "$STAGE_EVENT_OUTPUT"/.conversation-runtime-metrics-*
[ "$#" -eq 1 ] && [ -d "$1" ]
mv "$1" "$STAGE_EVENT_OUTPUT/injected-stage-name"
assert_background_fails "$stage_event_pid"
test ! -e "$STAGE_EVENT_OUTPUT/metrics.jsonl"

CONCURRENT_TARGET_METRICS="$FIXTURE_ROOT/concurrent-target.jsonl"
CONCURRENT_TARGET_MARKER="$FIXTURE_ROOT/concurrent-target-ready"
env \
    CONVERSATION_ACCEPTANCE_TEST_METRICS_READY_MARKER="$CONCURRENT_TARGET_MARKER" \
    CONVERSATION_ACCEPTANCE_TEST_METRICS_READY_DELAY_MS=500 \
    CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true \
    "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$CONCURRENT_TARGET_METRICS" \
    >"$FIXTURE_ROOT/concurrent-target.stdout" \
    2>"$FIXTURE_ROOT/concurrent-target.stderr" &
concurrent_target_pid=$!
wait_for_path "$CONCURRENT_TARGET_MARKER" "$concurrent_target_pid"
printf '%s\n' 'concurrent target must remain unchanged' \
    >"$CONCURRENT_TARGET_METRICS"
chmod 644 "$CONCURRENT_TARGET_METRICS"
assert_background_fails "$concurrent_target_pid"
test "$(cat "$CONCURRENT_TARGET_METRICS")" = \
    'concurrent target must remain unchanged'
test "$(stat -f '%Lp' "$CONCURRENT_TARGET_METRICS")" = 644

TRANSIENT_LINK_METRICS="$FIXTURE_ROOT/transient-link.jsonl"
TRANSIENT_LINK_ALIAS="$FIXTURE_ROOT/transient-link-alias.jsonl"
TRANSIENT_LINK_MARKER="$FIXTURE_ROOT/transient-link-published"
env \
    CONVERSATION_ACCEPTANCE_TEST_METRICS_PUBLISHED_MARKER="$TRANSIENT_LINK_MARKER" \
    CONVERSATION_ACCEPTANCE_TEST_METRICS_MONITOR_MS=5000 \
    CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true \
    "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$TRANSIENT_LINK_METRICS" \
    >"$FIXTURE_ROOT/transient-link.stdout" \
    2>"$FIXTURE_ROOT/transient-link.stderr" &
transient_link_pid=$!
wait_for_path "$TRANSIENT_LINK_MARKER" "$transient_link_pid"
ln "$TRANSIENT_LINK_METRICS" "$TRANSIENT_LINK_ALIAS"
rm "$TRANSIENT_LINK_ALIAS"
assert_background_fails "$transient_link_pid"
test ! -e "$TRANSIENT_LINK_METRICS"

REPLACEMENT_METRICS="$FIXTURE_ROOT/replacement.jsonl"
REPLACEMENT_OTHER="$FIXTURE_ROOT/replacement-other.jsonl"
REPLACEMENT_MARKER="$FIXTURE_ROOT/replacement-published"
printf '%s\n' 'unrelated replacement must survive cleanup' >"$REPLACEMENT_OTHER"
chmod 644 "$REPLACEMENT_OTHER"
env \
    CONVERSATION_ACCEPTANCE_TEST_METRICS_PUBLISHED_MARKER="$REPLACEMENT_MARKER" \
    CONVERSATION_ACCEPTANCE_TEST_METRICS_MONITOR_MS=5000 \
    CONVERSATION_VOICE_LOOP_BIN=/usr/bin/true \
    "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$REPLACEMENT_METRICS" \
    >"$FIXTURE_ROOT/replacement.stdout" \
    2>"$FIXTURE_ROOT/replacement.stderr" &
replacement_pid=$!
wait_for_path "$REPLACEMENT_MARKER" "$replacement_pid"
atomic_swap_paths "$REPLACEMENT_METRICS" "$REPLACEMENT_OTHER"
assert_background_fails "$replacement_pid"
test "$(cat "$REPLACEMENT_METRICS")" = \
    'unrelated replacement must survive cleanup'
test "$(stat -f '%Lp' "$REPLACEMENT_METRICS")" = 644
grep -q 'metrics cleanup could not safely remove' \
    "$FIXTURE_ROOT/replacement.stderr"

NEVER_RUN_MARKER="$FIXTURE_ROOT/measured-command-ran"
NEVER_RUN_LOOP="$FIXTURE_ROOT/never-run-voice-loop.sh"
cat >"$NEVER_RUN_LOOP" <<EOF
#!/bin/sh
: >"$NEVER_RUN_MARKER"
exit 0
EOF
chmod 700 "$NEVER_RUN_LOOP"

for launch_mode in setsid_failure delay mismatch; do
    rm -f "$NEVER_RUN_MARKER"
    assert_fails_bounded env \
        CONVERSATION_ACCEPTANCE_TEST_LAUNCH_MODE="$launch_mode" \
        CONVERSATION_VOICE_LOOP_BIN="$NEVER_RUN_LOOP" \
        "$HARNESS" \
        --config "$CONFIG" \
        --duration-seconds 1 \
        --metrics "$FIXTURE_ROOT/launch-$launch_mode.jsonl"
    test ! -e "$NEVER_RUN_MARKER"
done

COLLISION_READY="$FIXTURE_ROOT/collision-ready"
"$HELPER" test-session "$COLLISION_READY" &
outside_pid=$!
wait_for_path "$COLLISION_READY" "$outside_pid"
collision_identity=$(cat "$COLLISION_READY")
test "$collision_identity" = "$outside_pid"
rm -f "$NEVER_RUN_MARKER"
assert_fails_bounded env \
    CONVERSATION_ACCEPTANCE_TEST_LAUNCH_MODE=collision \
    CONVERSATION_ACCEPTANCE_TEST_COLLISION_ID="$collision_identity" \
    CONVERSATION_VOICE_LOOP_BIN="$NEVER_RUN_LOOP" \
    "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 1 \
    --metrics "$FIXTURE_ROOT/launch-collision.jsonl"
test ! -e "$NEVER_RUN_MARKER"
kill -0 "$outside_pid"
kill -TERM "$outside_pid"
wait "$outside_pid" 2>/dev/null || true
outside_pid=

STATUS_CHILD_PID="$FIXTURE_ROOT/status-child.pid"
STATUS_FAILURE_LOOP="$FIXTURE_ROOT/status-failure-voice-loop.sh"
cat >"$STATUS_FAILURE_LOOP" <<EOF
#!/usr/bin/ruby
child = Process.spawn("/bin/sleep", "30")
File.write("$STATUS_CHILD_PID", child.to_s)
exit 1
EOF
chmod 700 "$STATUS_FAILURE_LOOP"

STATUS_FAILURE_METRICS="$FIXTURE_ROOT/status-failure.jsonl"
assert_fails_bounded env \
    CONVERSATION_ACCEPTANCE_TEST_LAUNCH_MODE=status_write_failure \
    CONVERSATION_VOICE_LOOP_BIN="$STATUS_FAILURE_LOOP" \
    "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 5 \
    --metrics "$STATUS_FAILURE_METRICS"
jq -e . "$STATUS_FAILURE_METRICS" >/dev/null
grep -q '"exit_code":125' "$STATUS_FAILURE_METRICS"
grep -q '"process_cleanup_failure_count":1' "$STATUS_FAILURE_METRICS"
test -f "$STATUS_CHILD_PID"
assert_process_gone "$(cat "$STATUS_CHILD_PID")"

CONTROLLED_PARENT_IDENTITY="$FIXTURE_ROOT/controlled-parent.identity"
CONTROLLED_CHILD_IDENTITY="$FIXTURE_ROOT/controlled-child.identity"
CONTROLLED_CHILD_PID="$FIXTURE_ROOT/controlled-child.pid"
CONTROLLED_LOOP="$FIXTURE_ROOT/controlled-group-voice-loop.sh"
cat >"$CONTROLLED_LOOP" <<EOF
#!/usr/bin/ruby
child = fork do
  File.write(
    "$CONTROLLED_CHILD_IDENTITY",
    [Process.pid, Process.getpgrp, Process.getsid].join(" ")
  )
  sleep 30
end
File.write("$CONTROLLED_CHILD_PID", child.to_s)
File.write(
  "$CONTROLLED_PARENT_IDENTITY",
  [Process.pid, Process.getpgrp, Process.getsid].join(" ")
)
sleep 0.2
exit 1
EOF
chmod 700 "$CONTROLLED_LOOP"

assert_fails env CONVERSATION_VOICE_LOOP_BIN="$CONTROLLED_LOOP" "$HARNESS" \
    --config "$CONFIG" \
    --duration-seconds 5 \
    --metrics "$FIXTURE_ROOT/controlled-group.jsonl"
set -- $(cat "$CONTROLLED_PARENT_IDENTITY")
controlled_parent_pgid=$2
controlled_parent_sid=$3
set -- $(cat "$CONTROLLED_CHILD_IDENTITY")
controlled_child_pgid=$2
controlled_child_sid=$3
test "$controlled_parent_pgid" = "$controlled_parent_sid"
test "$controlled_child_pgid" = "$controlled_parent_pgid"
test "$controlled_child_sid" = "$controlled_parent_sid"
assert_process_gone "$(cat "$CONTROLLED_CHILD_PID")"

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
warn "turn=3 status=completed"
warn "turn=3 status=completed"
warn "turn=4 status=cancelled"
warn "turn=4 status=cancelled"
warn "turn=5 status=failed stage=speech_recognizer error=content-free-fixture"
warn "turn=5 status=failed stage=speech_recognizer error=content-free-fixture"
warn "turn=4 milestone=speech_end elapsed_ms=12"
warn "turn=4 generation=7 status=interrupted"
warn "turn=4 generation=7 status=interrupted"
warn "stale_generation=rejected"
warn "metric=underrun_count count=2"
warn "metric=queue_depth_frames count=4"
warn "private synthesized response must not persist"
Process.wait(owned_child)
EOF
chmod 700 "$FAKE_LOOP"

SILENT_LOOP="$FIXTURE_ROOT/silent-voice-loop.sh"
cat >"$SILENT_LOOP" <<'EOF'
#!/bin/sh
trap 'exit 130' INT TERM
while :; do
    sleep 1
done
EOF
chmod 700 "$SILENT_LOOP"

ZERO_ACTIVITY_METRICS="$FIXTURE_ROOT/zero-activity.jsonl"
assert_fails env CONVERSATION_VOICE_LOOP_BIN="$SILENT_LOOP" "$REAL_HARNESS" \
    --config "$CONFIG" \
    --minimum-completed-turns 0 \
    --minimum-interruptions 0 \
    --duration-seconds 1 \
    --metrics "$ZERO_ACTIVITY_METRICS"

SILENT_METRICS="$FIXTURE_ROOT/silent.jsonl"
assert_fails env CONVERSATION_VOICE_LOOP_BIN="$SILENT_LOOP" "$REAL_HARNESS" \
    --config "$CONFIG" \
    --minimum-completed-turns 1 \
    --minimum-interruptions 1 \
    --duration-seconds 1 \
    --metrics "$SILENT_METRICS"
jq -e . "$SILENT_METRICS" >/dev/null
grep -q '"session_result":"failed"' "$SILENT_METRICS"
grep -q '"minimum_completed_turns":1' "$SILENT_METRICS"
grep -q '"minimum_interruptions":1' "$SILENT_METRICS"
grep -q '"completed_turn_count":0' "$SILENT_METRICS"
grep -q '"interruption_count":0' "$SILENT_METRICS"

RESET_LOOP="$FIXTURE_ROOT/reset-voice-loop.sh"
cat >"$RESET_LOOP" <<'EOF'
#!/usr/bin/ruby
Signal.trap("INT") { exit 130 }
Signal.trap("TERM") { exit 130 }
warn "turn=1 status=completed"
warn "status=session-reset"
sleep 30
EOF
chmod 700 "$RESET_LOOP"

RESET_METRICS="$FIXTURE_ROOT/reset.jsonl"
assert_fails env CONVERSATION_VOICE_LOOP_BIN="$RESET_LOOP" "$REAL_HARNESS" \
    --config "$CONFIG" \
    --minimum-completed-turns 1 \
    --minimum-interruptions 0 \
    --duration-seconds 1 \
    --metrics "$RESET_METRICS"
grep -q '"session_reset_count":1' "$RESET_METRICS"
grep -q '"session_result":"failed"' "$RESET_METRICS"

UNMET_COMPLETED_METRICS="$FIXTURE_ROOT/unmet-completed.jsonl"
assert_fails env CONVERSATION_VOICE_LOOP_BIN="$FAKE_LOOP" "$REAL_HARNESS" \
    --config "$CONFIG" \
    --minimum-completed-turns 2 \
    --minimum-interruptions 1 \
    --duration-seconds 1 \
    --metrics "$UNMET_COMPLETED_METRICS"
grep -q '"completed_turn_count":1' "$UNMET_COMPLETED_METRICS"
grep -q '"interruption_count":1' "$UNMET_COMPLETED_METRICS"
grep -q '"session_result":"failed"' "$UNMET_COMPLETED_METRICS"

UNMET_INTERRUPTION_METRICS="$FIXTURE_ROOT/unmet-interruption.jsonl"
assert_fails env CONVERSATION_VOICE_LOOP_BIN="$FAKE_LOOP" "$REAL_HARNESS" \
    --config "$CONFIG" \
    --minimum-completed-turns 1 \
    --minimum-interruptions 2 \
    --duration-seconds 1 \
    --metrics "$UNMET_INTERRUPTION_METRICS"
grep -q '"completed_turn_count":1' "$UNMET_INTERRUPTION_METRICS"
grep -q '"interruption_count":1' "$UNMET_INTERRUPTION_METRICS"
grep -q '"session_result":"failed"' "$UNMET_INTERRUPTION_METRICS"

METRICS="$FIXTURE_ROOT/metrics.jsonl"
if ! CONVERSATION_VOICE_LOOP_BIN="$FAKE_LOOP" "$REAL_HARNESS" \
    --config "$CONFIG" \
    --minimum-completed-turns 1 \
    --minimum-interruptions 1 \
    --duration-seconds 1 \
    --metrics "$METRICS"; then
    cat "$METRICS" >&2
    exit 1
fi

test "$(stat -f '%Lp' "$METRICS")" = 600
jq -e . "$METRICS" >/dev/null
grep -q '"metric":"speech_end_ms"' "$METRICS"
grep -q '"turn_id":4' "$METRICS"
grep -q '"minimum_completed_turns":1' "$METRICS"
grep -q '"minimum_interruptions":1' "$METRICS"
grep -q '"session_result":"duration-complete"' "$METRICS"
grep -q '"completed_turn_count":1' "$METRICS"
grep -q '"cancelled_turn_count":1' "$METRICS"
grep -q '"failed_turn_count":1' "$METRICS"
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
