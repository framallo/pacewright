#!/usr/bin/env bash
# Start (or stop) N Chromes for a pacewright pool, one per port, each with its own profile.
#
# pacewright never launches a browser — it attaches — so something has to start them. For the
# ONE-Chrome case that something is launchd, because that Chrome is the operator's real, visible,
# logged-in browser (see com.paperclip.pacewright-chrome.plist). This script is for the OTHER case:
# many Chromes for throughput, where nobody is watching and nobody signs in.
#
#   ./packaging/chrome-pool.sh up 4        # ports 9222..9225, prints the config.toml to paste
#   ./packaging/chrome-pool.sh down        # stops every Chrome this script started
#   ./packaging/chrome-pool.sh status
#
# Headed by default because a headless Chrome is a much louder bot signal than a headed one, and a
# portal that blocks you is slower than a browser that costs a little RAM. `HEADLESS=1` overrides
# when the target does not care and the box has no display.
#
# One profile per instance is not optional: two Chromes sharing a --user-data-dir fight over the
# profile lock and the second one exits.
set -euo pipefail

PORT0="${PORT0:-9222}"
ROOT="${ROOT:-$HOME/.pacewright/chrome-pool}"
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
[ -x "$CHROME" ] || CHROME="$(command -v google-chrome || command -v chromium || true)"

usage() { sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'; exit 1; }

up() {
  local n="${1:-}"
  [[ "$n" =~ ^[0-9]+$ ]] && [ "$n" -ge 1 ] || usage
  [ -n "$CHROME" ] && [ -x "$CHROME" ] || { echo "no Chrome binary; set CHROME=" >&2; exit 1; }
  mkdir -p "$ROOT"
  local endpoints=()
  for i in $(seq 0 $((n - 1))); do
    local port=$((PORT0 + i)) dir="$ROOT/$i"
    endpoints+=("\"http://127.0.0.1:$port\"")
    if curl -sf -m2 "http://127.0.0.1:$port/json/version" >/dev/null; then
      echo "  :$port already listening"
      continue
    fi
    mkdir -p "$dir"
    local args=(
      "--remote-debugging-port=$port"
      "--user-data-dir=$dir"
      --no-first-run --no-default-browser-check
      # Chrome throttles background tabs; a pool instance is always in the background from the
      # window server's point of view, so without these a recipe's timers crawl.
      --disable-background-timer-throttling
      --disable-backgrounding-occluded-windows
      --disable-renderer-backgrounding
      # `tab "follow"` can only capture a popup Chrome actually opened.
      --disable-popup-blocking
      about:blank
    )
    [ "${HEADLESS:-0}" = "1" ] && args=(--headless=new "${args[@]}")
    nohup "$CHROME" "${args[@]}" >"$ROOT/$i.log" 2>&1 &
    echo "  :$port started (pid $!, profile $dir)"
  done
  # Readiness, not "we spawned it": a pool entry nothing listens on fails every task routed to it.
  for i in $(seq 0 $((n - 1))); do
    local port=$((PORT0 + i))
    for _ in $(seq 1 40); do
      curl -sf -m1 "http://127.0.0.1:$port/json/version" >/dev/null && break
      sleep 0.25
    done
    curl -sf -m1 "http://127.0.0.1:$port/json/version" >/dev/null \
      || { echo "  :$port never accepted CDP — see $ROOT/$i.log" >&2; exit 1; }
  done
  local joined
  joined="$(IFS=, ; echo "${endpoints[*]}")"
  cat <<TOML

$n Chrome(s) ready. Put this in ~/.pacewright/config.toml:

[browser]
connect = [${joined//,/, }]

That list length is also the parallelism: one run per Chrome at a time, because a named page is a
real tab and two runs in one browser would drive the same one.
TOML
}

down() {
  # Only ours: matched by the pool profile path, so the operator's own Chrome and the launchd
  # always-on instance are never touched.
  local pids
  pids="$(pgrep -f -- "--user-data-dir=$ROOT/" || true)"
  [ -n "$pids" ] || { echo "nothing to stop"; return 0; }
  echo "$pids" | xargs kill
  echo "stopped: $(echo "$pids" | tr '\n' ' ')"
}

status() {
  local found=0
  for i in $(seq 0 15); do
    local port=$((PORT0 + i))
    if v="$(curl -sf -m1 "http://127.0.0.1:$port/json/version" 2>/dev/null)"; then
      echo ":$port  $(echo "$v" | sed -n 's/.*"Browser": *"\([^"]*\)".*/\1/p')"
      found=$((found + 1))
    fi
  done
  echo "$found listening from :$PORT0"
}

case "${1:-}" in
  up) shift; up "$@" ;;
  down) down ;;
  status) status ;;
  *) usage ;;
esac
