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
[ ! -L "$metrics_path" ] || fail "metrics output must not be a symbolic link"

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
parser_state="$temporary_root/parser.state"
descendant_history="$temporary_root/descendants.tsv"
unique_descendants="$temporary_root/unique-descendants.tsv"
process_snapshot="$temporary_root/process-snapshot.txt"
process_inspection_failed="$temporary_root/process-inspection-failed"
duration_marker="$temporary_root/duration-reached"
interrupt_marker="$temporary_root/user-interrupted"

voice_pid=
timer_pid=
parser_pid=
monitor_pid=
force_shutdown_pid=

cleanup() {
    for pid in "$timer_pid" "$force_shutdown_pid" "$monitor_pid" "$parser_pid"; do
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    if [ -n "$voice_pid" ] && kill -0 "$voice_pid" 2>/dev/null; then
        kill -INT "$voice_pid" 2>/dev/null || true
        wait "$voice_pid" 2>/dev/null || true
    fi
    rm -rf "$temporary_root"
}
trap cleanup EXIT

config_sha256=$(/usr/bin/shasum -a 256 "$config_path" | awk '{print $1}') ||
    fail "could not digest configuration"
: >"$metrics_path" || fail "could not create metrics output"
printf '{"schema_version":1,"record_type":"session_start","config_sha256":"%s","duration_seconds":%s}\n' \
    "$config_sha256" "$duration_seconds" >>"$metrics_path"

mkfifo "$fifo_path" || fail "could not create private metrics pipe"
: >"$descendant_history"

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
' <"$fifo_path" >>"$metrics_path" &
parser_pid=$!

"$voice_loop_bin" --config "$config_path" >/dev/null 2>"$fifo_path" &
voice_pid=$!

(
    while kill -0 "$voice_pid" 2>/dev/null; do
        if ! /bin/ps -axo pid=,ppid=,lstart= >"$process_snapshot"; then
            : >"$process_inspection_failed"
            break
        fi
        awk -v root="$voice_pid" '
        {
            pid[NR] = $1
            parent[$1] = $2
            started[$1] = $3 " " $4 " " $5 " " $6 " " $7
        }
        END {
            for (row = 1; row <= NR; row++) {
                candidate = pid[row]
                ancestor = candidate
                while (ancestor in parent && parent[ancestor] != 0) {
                    if (parent[ancestor] == root) {
                        printf "%s\t%s\n", candidate, started[candidate]
                        break
                    }
                    ancestor = parent[ancestor]
                }
            }
        }
        ' "$process_snapshot" >>"$descendant_history"
        sleep 0.1
    done
) &
monitor_pid=$!

/usr/bin/perl -e '
    $SIG{HUP} = $SIG{INT} = $SIG{TERM} = sub { exit 0 };
    my ($duration, $pid, $marker) = @ARGV;
    sleep $duration;
    exit 0 unless kill 0, $pid;
    open my $handle, ">", $marker or exit 2;
    close $handle;
    kill "INT", $pid;
    sleep 10;
    kill "TERM", $pid if kill 0, $pid;
    sleep 2;
    kill "KILL", $pid if kill 0, $pid;
' "$duration_seconds" "$voice_pid" "$duration_marker" &
timer_pid=$!

handle_interrupt() {
    : >"$interrupt_marker"
    if [ -n "$voice_pid" ]; then
        kill -INT "$voice_pid" 2>/dev/null || true
        if [ -z "$force_shutdown_pid" ]; then
            /usr/bin/perl -e '
                $SIG{HUP} = $SIG{INT} = $SIG{TERM} = sub { exit 0 };
                my ($pid) = @ARGV;
                sleep 10;
                kill "TERM", $pid if kill 0, $pid;
                sleep 2;
                kill "KILL", $pid if kill 0, $pid;
            ' "$voice_pid" &
            force_shutdown_pid=$!
        fi
    fi
}
trap handle_interrupt HUP INT TERM

while :; do
    wait "$voice_pid"
    voice_status=$?
    if [ -f "$interrupt_marker" ] && kill -0 "$voice_pid" 2>/dev/null; then
        continue
    fi
    break
done
voice_pid=

kill "$timer_pid" 2>/dev/null || true
wait "$timer_pid" 2>/dev/null || true
timer_pid=
if [ -n "$force_shutdown_pid" ]; then
    kill "$force_shutdown_pid" 2>/dev/null || true
    wait "$force_shutdown_pid" 2>/dev/null || true
    force_shutdown_pid=
fi
wait "$monitor_pid" 2>/dev/null || true
monitor_pid=

awk -F '	' '!seen[$1]++' "$descendant_history" >"$unique_descendants"
orphaned_child_count=0
tab=$(printf '\t')
while IFS="$tab" read -r descendant_pid recorded_start; do
    case "$descendant_pid" in
        ''|*[!0-9]*) continue ;;
    esac
    current_start=$(
        /bin/ps -p "$descendant_pid" -o lstart= 2>/dev/null |
            awk '{$1=$1; print}'
    )
    if [ -n "$current_start" ] && [ "$current_start" = "$recorded_start" ]; then
        orphaned_child_count=$((orphaned_child_count + 1))
        kill -TERM "$descendant_pid" 2>/dev/null || true
        sleep 0.1
        kill -KILL "$descendant_pid" 2>/dev/null || true
    fi
done <"$unique_descendants"

/usr/bin/perl -e '
    $SIG{HUP} = $SIG{INT} = $SIG{TERM} = sub { exit 0 };
    my ($pid) = @ARGV;
    sleep 2;
    kill "TERM", $pid if kill 0, $pid;
' "$parser_pid" &
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

printf '{"schema_version":1,"record_type":"session_summary","session_result":"%s","exit_code":%s,"session_reset_count":%s,"stale_generation_reject_count":%s,"stale_generation_observed":%s,"queue_underrun_count":%s,"queue_underrun_observed":%s,"interruption_count":%s,"orphaned_child_count":%s,"process_inspection_failure_count":%s,"dropped_metric_record_count":%s}\n' \
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
    "$dropped_metric_record_count" >>"$metrics_path"

case "$session_result" in
    duration-complete) exit 0 ;;
    interrupted) exit 130 ;;
    *) exit 1 ;;
esac
