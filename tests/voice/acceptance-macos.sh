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
metrics_parent=$(
    CDPATH= cd -- "$metrics_parent" 2>/dev/null && pwd -P
) || fail "metrics parent directory does not exist"
metrics_path="$metrics_parent/$metrics_name"
case "$metrics_path" in
    "$REPOSITORY_ROOT"|"$REPOSITORY_ROOT"/*)
        fail "metrics output must be outside the repository"
        ;;
esac

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
fifo_path="$temporary_root/stderr.fifo"
metrics_fifo="$temporary_root/metrics.fifo"
metrics_ready="$temporary_root/metrics-ready"
metrics_failed="$temporary_root/metrics-failed"
parser_state="$temporary_root/parser.state"
process_group_snapshot="$temporary_root/process-groups.txt"
process_inspection_failed="$temporary_root/process-inspection-failed"
process_cleanup_failed="$temporary_root/process-cleanup-failed"
duration_marker="$temporary_root/duration-reached"
interrupt_marker="$temporary_root/user-interrupted"

voice_pid=
voice_pgid=
timer_pid=
parser_pid=
force_shutdown_pid=
metrics_writer_pid=
metrics_fd_open=0

group_exists() {
    [ -n "$voice_pgid" ] || return 1
    /usr/bin/perl -e 'exit(kill(0, -$ARGV[0]) ? 0 : 1)' "$voice_pgid"
}

signal_voice_group() {
    signal_name=$1
    [ -n "$voice_pgid" ] || return 0
    /usr/bin/perl -e '
        my ($signal_name, $process_group) = @ARGV;
        kill $signal_name, -$process_group;
        exit 0;
    ' "$signal_name" "$voice_pgid"
}

stop_voice_group() {
    [ -n "$voice_pgid" ] || return 0
    if group_exists; then
        signal_voice_group TERM
        sleep 0.5
    fi
    if group_exists; then
        signal_voice_group KILL
    fi

    attempts=0
    while group_exists; do
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 40 ]; then
            : >"$process_cleanup_failed"
            return 1
        fi
        sleep 0.05
    done
    return 0
}

cleanup() {
    for pid in "$timer_pid" "$force_shutdown_pid" "$parser_pid"; do
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    stop_voice_group || true
    if [ -n "$voice_pid" ]; then
        wait "$voice_pid" 2>/dev/null || true
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
/usr/bin/perl -e '
    use Fcntl qw(:DEFAULT :mode O_NOFOLLOW);
    my ($path, $fifo, $ready, $failed) = @ARGV;

    sub mark_failed {
        my ($marker) = @_;
        sysopen(my $failure_handle, $marker, O_WRONLY | O_CREAT | O_EXCL, 0600);
        close $failure_handle if $failure_handle;
        exit 1;
    }

    sysopen(my $output, $path, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, 0600)
        or mark_failed($failed);
    my @opened = stat($output);
    mark_failed($failed)
        unless @opened &&
            S_ISREG($opened[2]) &&
            $opened[3] == 1 &&
            ($opened[2] & 0777) == 0600;

    sysopen(my $ready_handle, $ready, O_WRONLY | O_CREAT | O_EXCL, 0600)
        or mark_failed($failed);
    close $ready_handle or mark_failed($failed);
    open my $input, "<", $fifo or mark_failed($failed);

    my $buffer;
    while (1) {
        my $read = sysread($input, $buffer, 8192);
        mark_failed($failed) unless defined $read;
        last if $read == 0;
        my $offset = 0;
        while ($offset < $read) {
            my $written = syswrite($output, $buffer, $read - $offset, $offset);
            mark_failed($failed) unless defined $written && $written > 0;
            $offset += $written;
        }
    }

    my @path_state = lstat($path);
    mark_failed($failed)
        unless @path_state &&
            $path_state[0] == $opened[0] &&
            $path_state[1] == $opened[1] &&
            S_ISREG($path_state[2]) &&
            $path_state[3] == 1 &&
            ($path_state[2] & 0777) == 0600;
    close $input or mark_failed($failed);
    close $output or mark_failed($failed);
' "$metrics_path" "$metrics_fifo" "$metrics_ready" "$metrics_failed" &
metrics_writer_pid=$!

attempts=0
while [ ! -f "$metrics_ready" ]; do
    if [ -f "$metrics_failed" ] || ! kill -0 "$metrics_writer_pid" 2>/dev/null; then
        wait "$metrics_writer_pid" 2>/dev/null || true
        metrics_writer_pid=
        fail "metrics output must be a previously absent regular file"
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

/usr/bin/perl -MPOSIX=setsid -e '
    setsid() >= 0 or die "could not create measured process session\n";
    exec @ARGV or die "could not execute measured command\n";
' "$voice_loop_bin" --config "$config_path" >/dev/null 2>"$fifo_path" 3>&- &
voice_pid=$!
voice_pgid=$voice_pid

/usr/bin/perl -e '
    $SIG{HUP} = $SIG{INT} = $SIG{TERM} = sub { exit 0 };
    my ($duration, $process_group, $marker) = @ARGV;
    sleep $duration;
    exit 0 unless kill 0, -$process_group;
    open my $handle, ">", $marker or exit 2;
    close $handle;
    kill "INT", -$process_group;
    sleep 10;
    kill "TERM", -$process_group if kill 0, -$process_group;
    sleep 2;
    kill "KILL", -$process_group if kill 0, -$process_group;
' "$duration_seconds" "$voice_pgid" "$duration_marker" 3>&- &
timer_pid=$!

handle_interrupt() {
    : >"$interrupt_marker"
    if [ -n "$voice_pgid" ]; then
        signal_voice_group INT
        if [ -z "$force_shutdown_pid" ]; then
            /usr/bin/perl -e '
                $SIG{HUP} = $SIG{INT} = $SIG{TERM} = sub { exit 0 };
                my ($process_group) = @ARGV;
                sleep 10;
                kill "TERM", -$process_group if kill 0, -$process_group;
                sleep 2;
                kill "KILL", -$process_group if kill 0, -$process_group;
            ' "$voice_pgid" 3>&- &
            force_shutdown_pid=$!
        fi
    fi
}
trap handle_interrupt HUP INT TERM

while :; do
    if wait "$voice_pid"; then
        voice_status=0
    else
        voice_status=$?
    fi
    if [ -f "$interrupt_marker" ] && kill -0 "$voice_pid" 2>/dev/null; then
        continue
    fi
    break
done

kill "$timer_pid" 2>/dev/null || true
wait "$timer_pid" 2>/dev/null || true
timer_pid=
if [ -n "$force_shutdown_pid" ]; then
    kill "$force_shutdown_pid" 2>/dev/null || true
    wait "$force_shutdown_pid" 2>/dev/null || true
    force_shutdown_pid=
fi

orphaned_child_count=0
if group_exists; then
    if /bin/ps -axo pgid= >"$process_group_snapshot"; then
        orphaned_child_count=$(
            awk -v process_group="$voice_pgid" \
                '$1 == process_group { count++ } END { print count + 0 }' \
                "$process_group_snapshot"
        )
    else
        : >"$process_inspection_failed"
    fi
fi
stop_voice_group || true
voice_pid=

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
