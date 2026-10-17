#!/usr/bin/env bash
# E2E smoke: vm-ui tray / close lifecycle on a real desktop session.
#
# The VM_UI_SMOKE_* env hooks drive the exact code paths a human click would:
#   VM_UI_SMOKE_CLOSE_SECS -> close the window (same flow as the WM "X")
#   VM_UI_SMOKE_SHOW_SECS  -> re-open one after N s in the tray wait (same
#                             flow as the tray "Show" item / left click)
#   VM_UI_SMOKE_QUIT_SECS  -> set quit + close (same as the tray Quit item)
#
# Cases:
#   1. Window close while visible: window goes away, process keeps running
#      in the tray ("running in the tray" logged). Quit with no window
#      (tray wait loop) then exits the process.
#   2. --minimized boot: no window is ever created; Show opens one; the
#      close hook closes it again; Quit while hidden exits. Covers
#      boot -> show -> close -> quit (including a fresh window after a
#      runtime close, i.e. run_native re-entry).
#   3. Wayland only (WAYLAND_DEBUG=1 protocol capture):
#      3a. --minimized: ZERO wl_surface buffer attaches and ZERO
#          xdg_toplevels — nothing can flash on screen at boot.
#      3b. --minimized + Show: attaches appear (the window is real).
#
# Needs a StatusNotifier host (KDE/GNOME session). Skips cleanly without a
# display. Only processes this script started are ever killed.
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
UI=${UI:-$ROOT/target/release/vm-ui}
cfg=$(mktemp -d /tmp/vm-ui-smoke.XXXXXX)
export VM_CONFIG_DIR="$cfg"
fail=0
pids=()

[ -x "$UI" ] || { echo "SKIP: $UI not built"; exit 0; }
if [ -z "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ]; then
  echo "SKIP: no desktop session"
  exit 0
fi
trap 'for p in "${pids[@]:-}"; do kill "$p" 2>/dev/null; done; rm -rf "$cfg"' EXIT

# wait_for_exit <pid> <max_secs>: 0 = gone, 1 = still alive
wait_for_exit() {
  local pid=$1 max=$2 slept=0
  local quarter=$((max * 4))
  while [ "$slept" -lt "$quarter" ]; do
    kill -0 "$pid" 2>/dev/null || return 0
    sleep 0.25; slept=$((slept + 1))
  done
  return 1
}

echo "== case 1: window close -> tray; Quit with no window exits"
VM_UI_SMOKE_CLOSE_SECS=2 VM_UI_SMOKE_QUIT_SECS=8 "$UI" >"$cfg/c1.log" 2>&1 &
pid=$!; pids+=("$pid")
sleep 5
if ! kill -0 "$pid" 2>/dev/null; then
  echo "FAIL: process exited when the window closed (tray should keep it)"; fail=1
elif ! grep -q 'running in the tray' "$cfg/c1.log"; then
  echo "FAIL: alive but no 'running in the tray' log"; fail=1
else
  echo "OK: window closed, process running in the tray"
  if wait_for_exit "$pid" 12; then
    echo "OK: Quit with no window exited cleanly"
  else
    echo "FAIL: never quit (tray wait loop broken?)"; fail=1
  fi
fi

echo "== case 2: --minimized boot -> Show -> close -> Quit hidden"
VM_UI_SMOKE_SHOW_SECS=2 VM_UI_SMOKE_CLOSE_SECS=4 VM_UI_SMOKE_QUIT_SECS=8 \
  "$UI" --minimized >"$cfg/c2.log" 2>&1 &
pid=$!; pids+=("$pid")
sleep 6
if ! kill -0 "$pid" 2>/dev/null; then
  echo "FAIL: exited too early (before the quit hook)"; fail=1
elif ! grep -q 'running in the tray' "$cfg/c2.log"; then
  echo "FAIL: no 'running in the tray' after Show -> close"; fail=1
else
  echo "OK: booted hidden, Show opened a window, it closed again"
fi
if kill -0 "$pid" 2>/dev/null; then
  if wait_for_exit "$pid" 12; then
    echo "OK: Quit while hidden exited cleanly"
  else
    echo "FAIL: never quit after the show/close cycle"; fail=1
  fi
else
  fail=1
fi

if [ -n "${WAYLAND_DISPLAY:-}" ]; then
  echo "== case 3a: --minimized creates no wayland surface"
  WAYLAND_DEBUG=1 VM_UI_SMOKE_QUIT_SECS=3 "$UI" --minimized >"$cfg/c3a.log" 2>&1 &
  pid=$!; pids+=("$pid")
  if wait_for_exit "$pid" 12; then
    n=$(grep -c '\.attach(' "$cfg/c3a.log")
    m=$(grep -c 'xdg_toplevel' "$cfg/c3a.log")
    if [ "$n" -eq 0 ] && [ "$m" -eq 0 ]; then
      echo "OK: windowless boot (no buffer attaches, no xdg_toplevel)"
    else
      echo "FAIL: --minimized created a window (attaches=$n toplevels=$m)"; fail=1
    fi
  else
    echo "FAIL: --minimized never exited"; fail=1
  fi

  echo "== case 3b: Show produces a real window"
  WAYLAND_DEBUG=1 VM_UI_SMOKE_SHOW_SECS=2 VM_UI_SMOKE_QUIT_SECS=6 \
    "$UI" --minimized >"$cfg/c3b.log" 2>&1 &
  pid=$!; pids+=("$pid")
  if wait_for_exit "$pid" 14; then
    n=$(grep -c '\.attach(' "$cfg/c3b.log")
    if [ "$n" -ge 1 ]; then
      echo "OK: Show created a window (buffer attaches=$n)"
    else
      echo "FAIL: Show created no window (no buffer attaches)"; fail=1
    fi
  else
    echo "FAIL: show case never exited"; fail=1
  fi
fi

echo "== logs =="
echo "--- case 1 (tail) ---"; tail -3 "$cfg/c1.log"
echo "--- case 2 (tail) ---"; tail -3 "$cfg/c2.log"
if [ -n "${WAYLAND_DISPLAY:-}" ]; then
  echo "--- case 3a: attaches=$(grep -c '\.attach(' "$cfg/c3a.log") toplevels=$(grep -c 'xdg_toplevel' "$cfg/c3a.log")"
  echo "--- case 3b: attaches=$(grep -c '\.attach(' "$cfg/c3b.log")"
fi
[ "$fail" -eq 0 ] && echo "SMOKE_OK" || echo "SMOKE_FAIL"
exit "$fail"
