#!/usr/bin/env bash

set +e

ci_log_dir=${NANOCODEX_CI_LOG_DIR:-/tmp}
ci_log_capture=${NANOCODEX_CI_LOG_CAPTURE:-/usr/local/bin/nanocodex-ci-log-capture}
stdout_file="$ci_log_dir/ci-step.out"
stderr_file="$ci_log_dir/ci-step.err"
stdout_pipe="$ci_log_dir/ci-step.out.pipe"
stderr_pipe="$ci_log_dir/ci-step.err.pipe"
command_pid_file="$ci_log_dir/ci-step.command.pid"

rm -f "$stdout_file" "$stderr_file" "$stdout_pipe" "$stderr_pipe" "$command_pid_file"
mkfifo "$stdout_pipe" "$stderr_pipe"
"$ci_log_capture" "$stdout_file" < "$stdout_pipe" &
stdout_capture=$!
"$ci_log_capture" "$stderr_file" < "$stderr_pipe" &
stderr_capture=$!

finish_captures() {
  wait "$stdout_capture"
  stdout_status=$?
  wait "$stderr_capture"
  stderr_status=$?
  rm -f "$stdout_pipe" "$stderr_pipe"
  if [[ "$stdout_status" -ne 0 || "$stderr_status" -ne 0 ]]; then
    return 125
  fi
  return 0
}

terminate() {
  trap - TERM INT
  if [[ -n ${command_pid:-} ]]; then
    if [[ ${command_owns_group:-0} -eq 1 ]]; then
      kill -TERM -- "-$command_pid" 2>/dev/null || true
    else
      pkill -TERM -P "$command_pid" 2>/dev/null || true
      kill -TERM "$command_pid" 2>/dev/null || true
    fi
    sleep 2
    if [[ ${command_owns_group:-0} -eq 1 ]]; then
      kill -KILL -- "-$command_pid" 2>/dev/null || true
    else
      pkill -KILL -P "$command_pid" 2>/dev/null || true
      kill -KILL "$command_pid" 2>/dev/null || true
    fi
    wait "$command_pid" 2>/dev/null || true
  fi
  finish_captures
  exit 124
}
trap terminate TERM INT

if command -v setsid >/dev/null 2>&1; then
  setsid bash -c "$1" > "$stdout_pipe" 2> "$stderr_pipe" &
  command_owns_group=1
else
  bash -c "$1" > "$stdout_pipe" 2> "$stderr_pipe" &
  command_owns_group=0
fi
command_pid=$!
printf '%s\n' "$command_pid" > "$command_pid_file"
wait "$command_pid"
command_status=$?
trap - TERM INT
finish_captures
capture_status=$?
if [[ "$capture_status" -ne 0 ]]; then
  exit "$capture_status"
fi
exit "$command_status"
