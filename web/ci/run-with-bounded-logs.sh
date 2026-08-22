#!/usr/bin/env bash

set +e

readonly executor_uid=10001
readonly executor_gid=10001
readonly executor_name=nanocodex-ci
readonly executor_home=/home/nanocodex-ci
readonly executor_path=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

if [[ $(/usr/bin/uname -s) == Linux ]]; then
  readonly linux_boundary=1
  readonly ci_log_dir=/tmp
  readonly ci_log_capture=/usr/local/bin/nanocodex-ci-log-capture
else
  # The production image is Linux. Keep the dependency-free Darwin path for
  # the repository's local timeout/log regression without adding a Linux
  # privilege bypass or an environment-controlled executor identity.
  readonly linux_boundary=0
  ci_log_dir=${NANOCODEX_CI_LOG_DIR:-/tmp}
  ci_log_capture=${NANOCODEX_CI_LOG_CAPTURE:-/usr/local/bin/nanocodex-ci-log-capture}
fi

stdout_file="$ci_log_dir/ci-step.out"
stderr_file="$ci_log_dir/ci-step.err"
stdout_pipe="$ci_log_dir/ci-step.out.pipe"
stderr_pipe="$ci_log_dir/ci-step.err.pipe"
command_pid_file="$ci_log_dir/ci-step.command.pid"
process_snapshot="$ci_log_dir/ci-step.processes"
executor_pid_file="$ci_log_dir/ci-step.executor-pids"
workspace_scan_file="$ci_log_dir/ci-step.workspace-scan"
cleanup_complete=0
cleanup_observed=0
captures_started=0

linux_collect_executor_pids() {
  executor_pids=()
  /usr/bin/ps -e -o pid= -o ruid= -o euid= -o stat= > "$process_snapshot" || return 1
  /usr/bin/awk -v uid="$executor_uid" \
    '($2 == uid || $3 == uid) && $4 !~ /^Z/ { print $1 }' \
    "$process_snapshot" > "$executor_pid_file" || return 1
  mapfile -t executor_pids < "$executor_pid_file" || return 1
  local pid
  for pid in "${executor_pids[@]}"; do
    [[ $pid =~ ^[1-9][0-9]*$ ]] || return 1
  done
}

linux_reap_executor() {
  local clean_passes=0
  local attempt pid
  cleanup_observed=0
  for attempt in $(/usr/bin/seq 1 100); do
    linux_collect_executor_pids || return 1
    if [[ ${#executor_pids[@]} -eq 0 ]]; then
      clean_passes=$((clean_passes + 1))
      if [[ $clean_passes -ge 2 ]]; then
        cleanup_complete=1
        return 0
      fi
      /bin/sleep 0.02
      continue
    fi

    cleanup_observed=1
    clean_passes=0
    # These PIDs all carried the dedicated executor's real or effective UID in
    # the immediately preceding snapshot. Stopping first closes the fork race;
    # a child that escaped the command's PGID remains owned by the same UID.
    for pid in "${executor_pids[@]}"; do
      builtin kill -STOP "$pid" 2>/dev/null || true
    done
    for pid in "${executor_pids[@]}"; do
      builtin kill -KILL "$pid" 2>/dev/null || true
    done
    /bin/sleep 0.05
  done
  return 1
}

prepare_linux_workspace() {
  [[ $EUID -eq 0 ]] || return 1
  [[ $(/usr/bin/id -u "$executor_name" 2>/dev/null) == "$executor_uid" ]] || return 1
  [[ $(/usr/bin/id -g "$executor_name" 2>/dev/null) == "$executor_gid" ]] || return 1
  [[ $(/usr/bin/id -G "$executor_name" 2>/dev/null) == "$executor_gid" ]] || return 1
  [[ -d /workspace && ! -L /workspace ]] || return 1

  # Never change ownership through a symlink. Reject non-owned hard links so a
  # hostile source archive cannot make this root preparation mutate an inode
  # outside the workspace. Existing executor-owned Cargo hard links are fine.
  /usr/bin/find -P /workspace -xdev \
    ! -type d ! -type l \( ! -uid "$executor_uid" -o ! -gid "$executor_gid" \) \
    -links +1 -print -quit > "$workspace_scan_file" || return 1
  [[ ! -s $workspace_scan_file ]] || return 1
  /bin/chown -h "$executor_uid:$executor_gid" /workspace || return 1
  /usr/bin/find -P /workspace -xdev \
    \( ! -uid "$executor_uid" -o ! -gid "$executor_gid" \) \
    -exec /bin/chown -h "$executor_uid:$executor_gid" -- {} + || return 1
  /usr/bin/find -P /workspace -xdev \
    \( ! -uid "$executor_uid" -o ! -gid "$executor_gid" \) \
    -print -quit > "$workspace_scan_file" || return 1
  [[ ! -s $workspace_scan_file ]]
}

linux_executor_environment() {
  executor_environment=(
    "HOME=$executor_home"
    "USER=$executor_name"
    "LOGNAME=$executor_name"
    "SHELL=/bin/bash"
    "PATH=$executor_path"
    "TMPDIR=/tmp"
    "LANG=C.UTF-8"
    "LC_ALL=C.UTF-8"
    "RUSTUP_HOME=/usr/local/rustup"
  )
  local name
  for name in \
    CI \
    CARGO_HOME CARGO_TARGET_DIR CARGO_BUILD_JOBS CARGO_INCREMENTAL CARGO_TERM_COLOR \
    RUST_TEST_THREADS CARGO_PROFILE_DEV_DEBUG CARGO_PROFILE_TEST_DEBUG; do
    if declare -p "$name" >/dev/null 2>&1; then
      executor_environment+=("$name=${!name}")
    fi
  done
}

stop_captures() {
  if [[ $captures_started -eq 1 ]]; then
    builtin kill -TERM "$stdout_capture" "$stderr_capture" 2>/dev/null || true
    wait "$stdout_capture" 2>/dev/null || true
    wait "$stderr_capture" 2>/dev/null || true
  fi
  rm -f "$stdout_pipe" "$stderr_pipe"
}

exit_cleanup() {
  local status=$?
  trap - EXIT
  if [[ $linux_boundary -eq 1 && $cleanup_complete -ne 1 ]]; then
    linux_reap_executor
    if [[ $? -ne 0 && $status -eq 0 ]]; then
      status=125
    fi
  fi
  rm -f "$process_snapshot" "$executor_pid_file" "$workspace_scan_file"
  exit "$status"
}
trap exit_cleanup EXIT

rm -f \
  "$stdout_file" "$stderr_file" "$stdout_pipe" "$stderr_pipe" \
  "$command_pid_file" "$process_snapshot" "$executor_pid_file" "$workspace_scan_file"
umask 077
mkfifo "$stdout_pipe" "$stderr_pipe"
"$ci_log_capture" "$stdout_file" < "$stdout_pipe" &
stdout_capture=$!
"$ci_log_capture" "$stderr_file" < "$stderr_pipe" &
stderr_capture=$!
captures_started=1

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
  if [[ $linux_boundary -eq 1 ]]; then
    linux_reap_executor
    if [[ $? -ne 0 ]]; then
      stop_captures
      exit 124
    fi
  fi
  finish_captures
  exit 124
}
trap terminate TERM INT

if [[ $linux_boundary -eq 1 ]]; then
  linux_reap_executor
  initial_cleanup_status=$?
  stale_executor=$cleanup_observed
  if [[ $initial_cleanup_status -ne 0 || $stale_executor -ne 0 ]] || \
    ! prepare_linux_workspace; then
    stop_captures
    exit 125
  fi
  cleanup_complete=0
  linux_executor_environment
  /usr/bin/setsid /usr/bin/setpriv \
    --reuid "$executor_uid" \
    --regid "$executor_gid" \
    --clear-groups \
    --inh-caps=-all \
    --ambient-caps=-all \
    --bounding-set=-all \
    --no-new-privs \
    /usr/bin/env -i "${executor_environment[@]}" \
    /bin/bash --noprofile --norc -c "$1" > "$stdout_pipe" 2> "$stderr_pipe" &
  command_owns_group=1
elif command -v setsid >/dev/null 2>&1; then
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
if [[ $linux_boundary -eq 1 ]]; then
  linux_reap_executor
  cleanup_status=$?
  observed_descendant=$cleanup_observed
  if [[ $cleanup_status -ne 0 ]]; then
    stop_captures
    exit 125
  fi
  if [[ $command_status -eq 0 && $observed_descendant -ne 0 ]]; then
    finish_captures
    exit 125
  fi
fi
finish_captures
capture_status=$?
if [[ "$capture_status" -ne 0 ]]; then
  exit "$capture_status"
fi
exit "$command_status"
