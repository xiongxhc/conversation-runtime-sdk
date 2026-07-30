#!/bin/sh
set -u

REPOSITORY_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd -P) || exit 1
DEFAULT_VOICE_LOOP_BIN="$REPOSITORY_ROOT/target/release/conversation-voice-loop"
MAX_METRIC_RECORDS=10000

usage() {
    cat <<'EOF'
Usage: acceptance-macos.sh --config ABSOLUTE_PATH --metrics ABSOLUTE_PATH [--duration-seconds SECONDS]

Runs the real voice loop for 600 seconds by default. The JSONL output contains
only content-free timing metrics, identifiers, stages, counts, and a session
result. Set CONVERSATION_VOICE_LOOP_BIN to an absolute executable for a
separately built binary.
EOF
}

fail() {
    printf '%s\n' "acceptance harness: $1" >&2
    exit 1
}

config_path=
metrics_path=
duration_seconds=600

while [ "$#" -gt 0 ]; do
    case "$1" in
        --config)
            [ "$#" -ge 2 ] || fail "--config requires an absolute path"
            [ -z "$config_path" ] || fail "--config may be specified only once"
            config_path=$2
            shift 2
            ;;
        --metrics)
            [ "$#" -ge 2 ] || fail "--metrics requires an absolute path"
            [ -z "$metrics_path" ] || fail "--metrics may be specified only once"
            metrics_path=$2
            shift 2
            ;;
        --duration-seconds)
            [ "$#" -ge 2 ] || fail "--duration-seconds requires a positive integer"
            duration_seconds=$2
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            fail "unknown argument"
            ;;
    esac
done

case "$config_path" in
    /*) ;;
    *) fail "--config requires an absolute path" ;;
esac
case "$metrics_path" in
    /*) ;;
    *) fail "--metrics requires an absolute path" ;;
esac
case "$duration_seconds" in
    ''|*[!0-9]*) fail "--duration-seconds requires a positive integer" ;;
esac
[ "$duration_seconds" -gt 0 ] || fail "--duration-seconds requires a positive integer"
[ "$duration_seconds" -le 86400 ] || fail "--duration-seconds exceeds the 86400-second safety limit"
[ -f "$config_path" ] && [ -r "$config_path" ] || fail "configuration file is not readable"

metrics_parent=$(dirname -- "$metrics_path")
metrics_name=$(basename -- "$metrics_path")
[ "$metrics_name" != "." ] && [ "$metrics_name" != ".." ] || fail "invalid metrics path"

voice_loop_bin=${CONVERSATION_VOICE_LOOP_BIN:-$DEFAULT_VOICE_LOOP_BIN}
case "$voice_loop_bin" in
    /*) ;;
    *) fail "conversation voice loop executable must use an absolute path" ;;
esac
[ -x "$voice_loop_bin" ] || fail "conversation voice loop executable is not available"
[ -x /usr/bin/perl ] || fail "macOS Perl runtime is required for bounded watchdogs"

umask 077
temporary_root=$(mktemp -d "${TMPDIR:-/tmp}/conversation-runtime-acceptance.XXXXXX") ||
    fail "could not create private temporary directory"
helper_bin=${CONVERSATION_ACCEPTANCE_HELPER_BIN:-}
if [ -z "$helper_bin" ]; then
    helper_bin="$temporary_root/acceptance-helper"
    /usr/bin/xcrun clang \
        -std=c11 \
        -O2 \
        -Wall \
        -Wextra \
        -Werror \
        "$REPOSITORY_ROOT/tests/voice/acceptance-helper.c" \
        -o "$helper_bin" ||
        fail "could not build acceptance safety helper"
fi
case "$helper_bin" in
    /*) ;;
    *) fail "acceptance safety helper must use an absolute path" ;;
esac
[ -x "$helper_bin" ] || fail "acceptance safety helper is not available"

fifo_path="$temporary_root/stderr.fifo"
metrics_fifo="$temporary_root/metrics.fifo"
metrics_ready="$temporary_root/metrics-ready"
metrics_failed="$temporary_root/metrics-failed"
parser_state="$temporary_root/parser.state"
process_inspection_failed="$temporary_root/process-inspection-failed"
process_cleanup_failed="$temporary_root/process-cleanup-failed"
duration_marker="$temporary_root/duration-reached"
interrupt_marker="$temporary_root/user-interrupted"
launch_handshake="$temporary_root/launch-handshake"
launch_release="$temporary_root/launch-release"
launch_status="$temporary_root/launch-status"
launch_finish="$temporary_root/launch-finish"

voice_pid=
voice_pgid=
voice_sid=
voice_identity_verified=0
timer_pid=
parser_pid=
force_shutdown_pid=
metrics_writer_pid=
metrics_fd_open=0

session_identity_valid() {
    [ "$voice_identity_verified" -eq 1 ] || return 1
    [ -n "$voice_pid" ] && [ -n "$voice_pgid" ] && [ -n "$voice_sid" ] ||
        return 1
    "$helper_bin" verify-session "$voice_pid" "$voice_pgid" "$voice_sid"
}

group_member_count() {
    [ -n "$voice_pgid" ] || return 1
    "$helper_bin" group-count "$voice_pgid"
}

signal_voice_group() {
    signal_name=$1
    session_identity_valid || return 1
    "$helper_bin" signal-group \
        "$voice_pid" \
        "$voice_pgid" \
        "$voice_sid" \
        "$signal_name"
}

verify_group_empty() {
    [ -n "$voice_pgid" ] || return 0
    attempts=0
    while :; do
        member_count=$(group_member_count) || {
            : >"$process_inspection_failed"
            return 1
        }
        [ "$member_count" -eq 0 ] && return 0
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 40 ]; then
            : >"$process_cleanup_failed"
            return 1
        fi
        sleep 0.05
    done
}

stop_unverified_voice() {
    [ -n "$voice_pid" ] || return 0
    kill -TERM "$voice_pid" 2>/dev/null || true
    attempts=0
    while kill -0 "$voice_pid" 2>/dev/null; do
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 20 ]; then
            kill -KILL "$voice_pid" 2>/dev/null || true
            break
        fi
        sleep 0.01
    done
    wait "$voice_pid" 2>/dev/null || true
    voice_pid=
}

stop_voice_group() {
    [ "$voice_identity_verified" -eq 1 ] || return 0
    if session_identity_valid; then
        signal_voice_group TERM || : >"$process_cleanup_failed"
        sleep 0.5
    fi

    if session_identity_valid; then
        member_count=$(group_member_count) || {
            : >"$process_inspection_failed"
            member_count=2
        }
        if [ "$member_count" -gt 1 ]; then
            signal_voice_group KILL || : >"$process_cleanup_failed"
        else
            : >"$launch_finish"
        fi
    fi

    if [ -n "$voice_pid" ]; then
        wait "$voice_pid" 2>/dev/null || true
        voice_pid=
    fi
    verify_group_empty
}

cleanup() {
    for pid in "$timer_pid" "$force_shutdown_pid" "$parser_pid"; do
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    if [ "$voice_identity_verified" -eq 1 ]; then
        stop_voice_group || true
    else
        stop_unverified_voice
    fi
    if [ "$metrics_fd_open" -eq 1 ]; then
        exec 3>&-
        metrics_fd_open=0
    fi
    if [ -n "$metrics_writer_pid" ]; then
        kill "$metrics_writer_pid" 2>/dev/null || true
        wait "$metrics_writer_pid" 2>/dev/null || true
    fi
    rm -rf "$temporary_root"
}
trap cleanup EXIT

config_sha256=$(/usr/bin/shasum -a 256 "$config_path" | awk '{print $1}') ||
    fail "could not digest configuration"
mkfifo "$metrics_fifo" || fail "could not create private metrics pipe"
"$helper_bin" metrics \
    "$metrics_parent" \
    "$metrics_name" \
    "$metrics_fifo" \
    "$metrics_ready" \
    "$metrics_failed" \
    "$REPOSITORY_ROOT" &
metrics_writer_pid=$!

attempts=0
while [ ! -f "$metrics_ready" ]; do
    if [ -f "$metrics_failed" ] || ! kill -0 "$metrics_writer_pid" 2>/dev/null; then
        wait "$metrics_writer_pid" 2>/dev/null || true
        metrics_writer_pid=
        fail "metrics output requires a safe, unchanged directory and absent target"
    fi
    attempts=$((attempts + 1))
    [ "$attempts" -lt 200 ] || fail "timed out creating metrics output"
    sleep 0.01
done
exec 3>"$metrics_fifo" || fail "could not open private metrics pipe"
metrics_fd_open=1
printf '{"schema_version":1,"record_type":"session_start","config_sha256":"%s","duration_seconds":%s}\n' \
    "$config_sha256" "$duration_seconds" >&3 ||
    fail "could not write metrics output"

mkfifo "$fifo_path" || fail "could not create private metrics pipe"

awk -v state_file="$parser_state" -v max_records="$MAX_METRIC_RECORDS" '
function metric_name(milestone) {
    if (milestone == "speech_end") return "speech_end_ms"
    if (milestone == "transcript_final") return "transcript_final_ms"
    if (milestone == "first_text_delta") return "first_text_delta_ms"
    if (milestone == "first_synthesis_request") return "first_synthesis_request_ms"
    if (milestone == "first_playable_audio") return "first_playable_audio_ms"
    if (milestone == "first_sidecar_accept") return "first_sidecar_accept_ms"
    if (milestone == "playback_render_acknowledged") return "playback_render_ack_ms"
    if (milestone == "barge_in_onset") return "barge_in_onset_ms"
    if (milestone == "barge_in_threshold") return "barge_in_threshold_ms"
    if (milestone == "playback_flush_acknowledged") return "playback_flush_ack_ms"
    if (milestone == "cleanup") return "cleanup_ms"
    return ""
}
function allowed_stage(stage) {
    return stage == "runtime" ||
        stage == "privacy_policy" ||
        stage == "audio_capture" ||
        stage == "speech_recognizer" ||
        stage == "language_model" ||
        stage == "speech_synthesizer" ||
        stage == "audio_output" ||
        stage == "voice_sidecar" ||
        stage == "continuous_audio_output"
}
function emit(line) {
    if (records < max_records) {
        print line
        records++
    } else {
        dropped++
    }
}
{
    delete field
    for (field_index = 1; field_index <= NF; field_index++) {
        separator = index($field_index, "=")
        if (separator > 1) {
            key = substr($field_index, 1, separator - 1)
            value = substr($field_index, separator + 1)
            field[key] = value
        }
    }

    metric = metric_name(field["milestone"])
    if (metric != "" && field["elapsed_ms"] ~ /^[0-9]+$/) {
        if (field["turn"] ~ /^[0-9]+$/) {
            emit(sprintf("{\"schema_version\":1,\"record_type\":\"metric\",\"metric\":\"%s\",\"turn_id\":%s,\"value_ms\":%s}", metric, field["turn"], field["elapsed_ms"]))
        }
    }

    if (field["status"] == "interrupted") interruptions++
    if (field["status"] == "session-reset") resets++
    if (field["stale_generation"] == "rejected") {
        stale_rejects++
        stale_observed = 1
    }
    if (field["metric"] == "underrun_count" && field["count"] ~ /^[0-9]+$/) {
        underruns += field["count"]
        underrun_observed = 1
        emit(sprintf("{\"schema_version\":1,\"record_type\":\"metric\",\"metric\":\"underrun_count\",\"count\":%s}", field["count"]))
    }
    if (field["metric"] == "queue_depth_frames" && field["count"] ~ /^[0-9]+$/) {
        emit(sprintf("{\"schema_version\":1,\"record_type\":\"metric\",\"metric\":\"queue_depth_frames\",\"count\":%s}", field["count"]))
    }
    if ((field["status"] == "error" || field["status"] == "recoverable") && allowed_stage(field["stage"])) {
        emit(sprintf("{\"schema_version\":1,\"record_type\":\"stage_count\",\"stage\":\"%s\",\"count\":1}", field["stage"]))
    }
}
END {
    printf "interruption_count=%d\n", interruptions > state_file
    printf "session_reset_count=%d\n", resets >> state_file
    printf "stale_generation_reject_count=%d\n", stale_rejects >> state_file
    printf "stale_generation_observed=%d\n", stale_observed >> state_file
    printf "queue_underrun_count=%d\n", underruns >> state_file
    printf "queue_underrun_observed=%d\n", underrun_observed >> state_file
    printf "dropped_metric_record_count=%d\n", dropped >> state_file
}
' <"$fifo_path" >&3 &
parser_pid=$!

"$helper_bin" launch \
    "$launch_handshake" \
    "$launch_release" \
    "$launch_status" \
    "$launch_finish" \
    "$voice_loop_bin" \
    --config \
    "$config_path" \
    >/dev/null 2>"$fifo_path" 3>&- &
voice_pid=$!

"$helper_bin" wait-handshake "$voice_pid" "$launch_handshake" 1000 ||
    fail "timed out verifying measured process identity"

handshake_pid=
handshake_pgid=
handshake_sid=
handshake_extra=
IFS=' ' read -r handshake_pid handshake_pgid handshake_sid handshake_extra \
    <"$launch_handshake" ||
    fail "invalid measured process identity handshake"
for identity in "$handshake_pid" "$handshake_pgid" "$handshake_sid"; do
    case "$identity" in
        ''|*[!0-9]*) fail "invalid measured process identity handshake" ;;
    esac
done
[ -z "$handshake_extra" ] ||
    fail "invalid measured process identity handshake"
[ "$handshake_pid" = "$voice_pid" ] &&
    [ "$handshake_pid" = "$handshake_pgid" ] &&
    [ "$handshake_pid" = "$handshake_sid" ] ||
    fail "measured process identity handshake did not match launcher"
"$helper_bin" verify-session \
    "$handshake_pid" \
    "$handshake_pgid" \
    "$handshake_sid" ||
    fail "measured process identity could not be verified"
voice_pgid=$handshake_pgid
voice_sid=$handshake_sid
voice_identity_verified=1
: >"$launch_release" ||
    fail "could not release verified measured process"

"$helper_bin" timeout-watchdog \
    "$voice_pid" \
    "$voice_pgid" \
    "$voice_sid" \
    "$duration_seconds" \
    "$duration_marker" \
    "$process_cleanup_failed" \
    3>&- &
timer_pid=$!

handle_interrupt() {
    : >"$interrupt_marker"
    if [ "$voice_identity_verified" -eq 1 ]; then
        signal_voice_group INT || : >"$process_cleanup_failed"
        if [ -z "$force_shutdown_pid" ]; then
            "$helper_bin" shutdown-watchdog \
                "$voice_pid" \
                "$voice_pgid" \
                "$voice_sid" \
                "$process_cleanup_failed" \
                3>&- &
            force_shutdown_pid=$!
        fi
    fi
}
trap handle_interrupt HUP INT TERM

voice_status=
while [ ! -f "$launch_status" ]; do
    if ! kill -0 "$voice_pid" 2>/dev/null; then
        if wait "$voice_pid"; then
            launcher_status=0
        else
            launcher_status=$?
        fi
        voice_pid=
        voice_status=$launcher_status
        verify_group_empty || true
        break
    fi
    sleep 0.01
done
if [ -f "$launch_status" ]; then
    IFS= read -r voice_status <"$launch_status" ||
        voice_status=
    case "$voice_status" in
        ''|*[!0-9]*)
            voice_status=125
            : >"$process_cleanup_failed"
            ;;
    esac
fi

kill "$timer_pid" 2>/dev/null || true
wait "$timer_pid" 2>/dev/null || true
timer_pid=
if [ -n "$force_shutdown_pid" ]; then
    kill "$force_shutdown_pid" 2>/dev/null || true
    wait "$force_shutdown_pid" 2>/dev/null || true
    force_shutdown_pid=
fi

orphaned_child_count=0
if [ -n "$voice_pid" ]; then
    if member_count=$(group_member_count); then
        if [ "$member_count" -gt 0 ]; then
            orphaned_child_count=$((member_count - 1))
        fi
    else
        : >"$process_inspection_failed"
    fi
fi
stop_voice_group || true

/usr/bin/perl -e '
    $SIG{HUP} = $SIG{INT} = $SIG{TERM} = sub { exit 0 };
    my ($pid) = @ARGV;
    sleep 2;
    kill "TERM", $pid if kill 0, $pid;
' "$parser_pid" 3>&- &
parser_guard_pid=$!
wait "$parser_pid"
parser_status=$?
parser_pid=
kill "$parser_guard_pid" 2>/dev/null || true
wait "$parser_guard_pid" 2>/dev/null || true

interruption_count=0
session_reset_count=0
stale_generation_reject_count=0
stale_generation_observed=0
queue_underrun_count=0
queue_underrun_observed=0
dropped_metric_record_count=0
if [ -f "$parser_state" ]; then
    . "$parser_state"
fi

if [ -f "$interrupt_marker" ]; then
    session_result=interrupted
elif [ -f "$duration_marker" ] && { [ "$voice_status" -eq 0 ] || [ "$voice_status" -eq 130 ]; }; then
    session_result=duration-complete
elif [ "$voice_status" -eq 0 ]; then
    session_result=completed-early
else
    session_result=failed
fi
if [ "$parser_status" -ne 0 ] || [ "$orphaned_child_count" -ne 0 ]; then
    session_result=failed
fi
process_inspection_failure_count=0
if [ -f "$process_inspection_failed" ]; then
    process_inspection_failure_count=1
    session_result=failed
fi
process_cleanup_failure_count=0
if [ -f "$process_cleanup_failed" ]; then
    process_cleanup_failure_count=1
    session_result=failed
fi

stale_generation_reject_json=null
stale_generation_observed_json=false
if [ "$stale_generation_observed" -eq 1 ]; then
    stale_generation_reject_json=$stale_generation_reject_count
    stale_generation_observed_json=true
fi
queue_underrun_json=null
queue_underrun_observed_json=false
if [ "$queue_underrun_observed" -eq 1 ]; then
    queue_underrun_json=$queue_underrun_count
    queue_underrun_observed_json=true
fi

printf '{"schema_version":1,"record_type":"session_summary","session_result":"%s","exit_code":%s,"session_reset_count":%s,"stale_generation_reject_count":%s,"stale_generation_observed":%s,"queue_underrun_count":%s,"queue_underrun_observed":%s,"interruption_count":%s,"orphaned_child_count":%s,"process_inspection_failure_count":%s,"process_cleanup_failure_count":%s,"dropped_metric_record_count":%s}\n' \
    "$session_result" \
    "$voice_status" \
    "$session_reset_count" \
    "$stale_generation_reject_json" \
    "$stale_generation_observed_json" \
    "$queue_underrun_json" \
    "$queue_underrun_observed_json" \
    "$interruption_count" \
    "$orphaned_child_count" \
    "$process_inspection_failure_count" \
    "$process_cleanup_failure_count" \
    "$dropped_metric_record_count" >&3 ||
    fail "could not write metrics summary"

exec 3>&-
metrics_fd_open=0
if wait "$metrics_writer_pid"; then
    metrics_writer_status=0
else
    metrics_writer_status=$?
fi
metrics_writer_pid=
[ "$metrics_writer_status" -eq 0 ] || exit 1

case "$session_result" in
    duration-complete) exit 0 ;;
    interrupted) exit 130 ;;
    *) exit 1 ;;
esac
