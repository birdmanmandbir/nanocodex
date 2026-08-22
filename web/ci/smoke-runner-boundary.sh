#!/usr/bin/env bash

set -Eeuo pipefail

readonly control_header=X-Nanocodex-Sandbox-Control
readonly control_url=http://127.0.0.1:3000
readonly smoke_program=/usr/local/libexec/nanocodex-ci-boundary-smoke

report_failure() {
  local status=$?
  printf 'CI boundary smoke failed at %s:%s: %s\n' \
    "${BASH_SOURCE[1]:-${BASH_SOURCE[0]}}" "${BASH_LINENO[0]:-?}" "$BASH_COMMAND" >&2
  if [[ -f /tmp/control-server.log ]]; then
    /bin/cat /tmp/control-server.log >&2
  fi
  for log in /tmp/ci-step.out /tmp/ci-step.err; do
    if [[ -f $log ]]; then
      printf '%s:\n' "$log" >&2
      /bin/cat "$log" >&2
    fi
  done
  exit "$status"
}
trap report_failure ERR

payload_probe() {
  [[ $(id -u):$(id -g):$(id -G) == 10001:10001:10001 ]]
  [[ $HOME:$USER:$LOGNAME == /home/nanocodex-ci:nanocodex-ci:nanocodex-ci ]]
  [[ $PATH == /usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin ]]
  [[ -z ${NANOCODEX_CI_BOUNDARY_PROBE+x} ]]
  [[ -z ${NANOCODEX_SANDBOX_CONTROL_TOKEN+x} ]]
  grep -Eq '^Uid:[[:space:]]+10001[[:space:]]+10001[[:space:]]+10001[[:space:]]+10001$' /proc/self/status
  grep -Eq '^Gid:[[:space:]]+10001[[:space:]]+10001[[:space:]]+10001[[:space:]]+10001$' /proc/self/status
  grep -Eq '^Groups:[[:space:]]*$' /proc/self/status
  grep -Eq '^NoNewPrivs:[[:space:]]+1$' /proc/self/status
  [[ $(grep -Ec '^Cap(Inh|Prm|Eff|Bnd|Amb):[[:space:]]+0+$' /proc/self/status) -eq 5 ]]
  [[ ! -r /proc/1/environ ]]
  [[ ! -r /proc/$PPID/environ ]]

  denied http://127.0.0.1:3000/api/process/list GET 401
  denied http://127.0.0.1:3000/api/process/start POST 401
  denied "http://$(hostname -i):3000/api/process/list" GET 401
  denied 'http://[::1]:3000/api/process/list' GET '000|401'
  denied http://127.0.0.1:3000/ws WEBSOCKET 401

  ! /usr/bin/setpriv --reuid 0 --regid 0 --clear-groups /usr/bin/id >/dev/null 2>&1
  ! /usr/bin/unshare --user --map-root-user /bin/true >/dev/null 2>&1
  /usr/local/bin/python3 - <<'PY'
import socket

try:
    socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(3))
except PermissionError:
    pass
else:
    raise SystemExit("CI payload unexpectedly opened a raw packet socket")
PY
}

denied() {
  local url=$1
  local method=$2
  local expected=$3
  local status
  local curl_args=(-sS -o /dev/null -w '%{http_code}')
  case "$method" in
    GET) ;;
    POST)
      curl_args+=(-X POST -H 'content-type: application/json' --data '{"command":"id"}')
      ;;
    WEBSOCKET)
      curl_args+=(
        -H 'Connection: Upgrade'
        -H 'Upgrade: websocket'
        -H 'Sec-WebSocket-Key: bmFub2NvZGV4LWNp'
        -H 'Sec-WebSocket-Version: 13'
      )
      ;;
    *) return 1 ;;
  esac
  status=$(curl "${curl_args[@]}" "$url" 2>/dev/null || true)
  [[ $status =~ ^($expected)$ ]]
}

escaped_descendant() {
  /usr/local/bin/python3 - <<'PY'
import os
import time

pid = os.fork()
if pid:
    with open("/workspace/escaped.pid", "w", encoding="utf-8") as handle:
        handle.write(str(pid))
    os._exit(0)
os.setsid()
device = os.open("/dev/null", os.O_RDWR)
for descriptor in (0, 1, 2):
    os.dup2(device, descriptor)
time.sleep(60)
PY
}

root_smoke() {
  local control_server
  local control_token
  local ready=0
  local status
  printf -v control_token '%129s' ''
  control_token=${control_token// /a}

  NANOCODEX_SANDBOX_CONTROL_TOKEN=$control_token \
    /usr/local/bin/bun /container-server/dist/index.js \
    >/tmp/control-server.log 2>&1 &
  control_server=$!
  trap 'kill "$control_server" 2>/dev/null || true; wait "$control_server" 2>/dev/null || true' EXIT

  for _ in {1..400}; do
    if ! kill -0 "$control_server" 2>/dev/null; then
      /bin/cat /tmp/control-server.log >&2
      return 1
    fi
    if curl -fsS "$control_url/api/ping" >/dev/null 2>&1; then
      ready=1
      break
    fi
    sleep 0.05
  done
  if [[ $ready -ne 1 ]]; then
    /bin/cat /tmp/control-server.log >&2
    return 1
  fi

  denied "$control_url/api/process/list" GET 401
  denied "$control_url/api/process/start" POST 401
  denied "$control_url/ws" WEBSOCKET 401
  [[ $(curl -sS -o /dev/null -w '%{http_code}' \
    -H "$control_header: $control_token" \
    "$control_url/api/process/list") == 200 ]]

  NANOCODEX_SANDBOX_CONTROL_TOKEN=$control_token \
    NANOCODEX_CI_BOUNDARY_PROBE=must-not-cross \
    /usr/local/bin/nanocodex-ci-run "$smoke_program payload"

  /bin/rm -f /workspace/escaped.pid
  if /usr/local/bin/nanocodex-ci-run "$smoke_program escape"; then
    status=0
  else
    status=$?
  fi
  [[ $status -eq 125 ]]
  [[ -s /workspace/escaped.pid ]]
  local escaped_pid
  escaped_pid=$(< /workspace/escaped.pid)
  [[ $escaped_pid =~ ^[1-9][0-9]*$ ]]
  if kill -0 "$escaped_pid" 2>/dev/null; then
    local state
    state=$(/usr/bin/awk '{ print $3 }' "/proc/$escaped_pid/stat" 2>/dev/null || true)
    [[ -z $state || $state == Z ]]
  fi
  /usr/bin/ps -e -o ruid= -o euid= -o stat= | \
    /usr/bin/awk '$1 == 10001 || $2 == 10001 { if ($3 !~ /^Z/) exit 1 }'

  /bin/rm -f /workspace/escaped.pid /tmp/ci-step.*
  kill "$control_server"
  wait "$control_server" || true
  trap - EXIT
}

case ${1:-root} in
  root) root_smoke ;;
  payload) payload_probe ;;
  escape) escaped_descendant ;;
  *) exit 2 ;;
esac
