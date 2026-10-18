#!/usr/bin/env bash
# E2E smoke: hear-myself monitor lifecycle on a real PipeWire session
# (F-0.4.2 "Hear Myself": wet default, retarget, default-follow, unplug
# fallback, channel pairing).
#
# Boots a private `vm-daemon --live` (own config dir, next free control
# port) and drives the monitor through its whole life:
#
#   1. fresh config        -> wet default: mix 1.0, disabled, device ""
#   2. enable + volume/blend edits -> wire state + disk (schema v5), and
#      NO link churn from non-device edits
#   3. retarget to a temporary null-sink -> the monitor links move to
#      it and are paired FL<->FL / FR<->FR (0.4.2 channel-tag fix)
#   4. back to ""          -> links return to the system default sink
#   5. unplug (unload the sink) -> DeviceRemoved falls back to the
#      default, `device` persists as "", the monitor stays enabled
#   6. disable             -> state + disk; SIGINT -> clean shutdown
#
# The monitorChanged broadcast is asserted from the raw WebSocket feed
# (python3 + the `websockets` module; without it the state asserts
# still run and a NOTE is printed).
#
# A concurrently running system daemon is fine: link assertions use
# deltas around the temporary sink, which is unique to this run. Note
# that the smoke daemon shares `vm-mic` with it for the few seconds of
# the run (both pipelines feed the virtual mic while the smoke lives).
#
# Skips cleanly (exit 0) without: built binaries, pactl/pw-link,
# python3, or a reachable default sink.
#
# Env: DAEMON=path VM=path (defaults to target/release/{vm-daemon,vm}).
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
DAEMON=${DAEMON:-$ROOT/target/release/vm-daemon}
VMC=${VM:-$ROOT/target/release/vm}
cfg=$(mktemp -d "${TMPDIR:-/tmp}/vm-monitor-smoke.XXXXXX")
SMOKE_SINK="monitor-smoke-out-$$"
MOD=""
pids=()
fail=0
cleanup() {
  for p in ${pids[@]+"${pids[@]}"}; do kill "$p" 2>/dev/null; done
  [ -n "$MOD" ] && pactl unload-module "$MOD" 2>/dev/null
  rm -rf "$cfg"
}
trap cleanup EXIT

skip()  { echo "SKIP: $*"; exit 0; }
fatal() { echo "FAIL: $*"; exit 1; }
ok()    { echo "OK: $*"; }
bad()   { echo "FAIL: $*"; fail=1; }
note()  { echo "NOTE: $*"; }

[ -x "$DAEMON" ] || skip "$DAEMON not built"
[ -x "$VMC" ] || skip "$VMC not built"
command -v pactl   >/dev/null 2>&1 || skip "pactl not found"
command -v pw-link >/dev/null 2>&1 || skip "pw-link not found"
command -v python3 >/dev/null 2>&1 || skip "python3 not found"
DEFAULT=$(pactl get-default-sink 2>/dev/null || true)
[ -n "$DEFAULT" ] || skip "no default sink (PipeWire/pulse not reachable)"

# -- helpers -----------------------------------------------------------

REPLY=""
call() { REPLY=$("$VMC" call "$1" --ports "$PORT" 2>/dev/null); }

# One field of the last reply's `data`, as canonical JSON (strings
# quoted, booleans true/false).
pick() {
  printf '%s' "$REPLY" | python3 -c "import json,sys; print(json.dumps(json.load(sys.stdin)['data']['$1']))"
}
# Same for floats: the wire carries f32 values (0.8 -> 0.800000011920929
# when widened), so compare through %g (0.8, -6, 1).
pickf() {
  printf '%s' "$REPLY" | python3 -c "import json,sys; print('%g' % json.load(sys.stdin)['data']['$1'])"
}
reply_ok() {
  printf '%s' "$REPLY" | python3 -c "import json,sys; print(json.dumps(json.load(sys.stdin)['ok']))"
}
# One field of monitor{} in the daemon's config.json on disk.
cfgmon() {
  python3 -c "import json; print(json.dumps(json.load(open('$cfg/config.json'))['monitor']['$1']))"
}
cfgmonf() {
  python3 -c "import json; print('%g' % json.load(open('$cfg/config.json'))['monitor']['$1'])"
}
cfgver() {
  python3 -c "import json; print(json.load(open('$cfg/config.json'))['version'])"
}
# uid of the temporary null-sink as the daemon enumerated it.
smoke_uid() {
  printf '%s' "$REPLY" | python3 -c "
import json,sys
for d in json.load(sys.stdin)['data']['devices']:
    if d['uid'].startswith('$SMOKE_SINK#'):
        print(d['uid']); break
"
}
# vm-monitor -> $1 playback links currently on the graph (every
# vm-monitor node: deltas stay correct with a second daemon running).
mon_links_to() {
  pw-link -l | awk -v sink="$1" '
    /^vm-monitor:output_F[LR]$/ { mon=1; next }
    /^[^ ]/ { mon=0 }
    mon && /^  \|-> / && index($0, sink ":playback_") > 0 { c++ }
    END { print c+0 }'
}
# Target of the FL/FR monitor port while pinned to $2 — the temp sink
# is unique to this run, so the match cannot hit another daemon's links.
mon_target_on() {
  pw-link -l | awk -v ch="$1" -v sink="$2" '
    $0 == "vm-monitor:output_" ch { mon=1; next }
    /^[^ ]/ { mon=0 }
    mon && /^  \|-> / && index($0, sink ":") > 0 { print $2; exit }'
}
wait_config_device() { # $1 = expected json, $2 = max polls (x0.25 s)
  for _ in $(seq 1 "$2"); do
    [ "$(cfgmon device)" = "$1" ] && return 0
    sleep 0.25
  done
  return 1
}

# -- boot --------------------------------------------------------------

VM_CONFIG_DIR="$cfg" "$DAEMON" --live >"$cfg/daemon.log" 2>&1 &
dpid=$!; pids+=("$dpid")
others=$( { pgrep -x vm-daemon 2>/dev/null || true; } | grep -vc "^$dpid$" || true)
[ "${others:-0}" -gt 0 ] && \
  note "another vm-daemon runs alongside: link counts are delta-based; vm-mic is shared for this run"

PORT=""
for _ in $(seq 1 100); do
  PORT=$(sed -n 's/.*control api listening (effective port \([0-9][0-9]*\)).*/\1/p' "$cfg/daemon.log" | head -1)
  [ -n "$PORT" ] && break
  kill -0 "$dpid" 2>/dev/null || break
  sleep 0.2
done
[ -n "$PORT" ] || fatal "the daemon never opened its control port:
$(sed -n '1,15p' "$cfg/daemon.log")"
echo "daemon up on control port $PORT"

streamed=0
for _ in $(seq 1 120); do
  grep -q "streaming\. Ctrl-C to stop\." "$cfg/daemon.log" && { streamed=1; break; }
  kill -0 "$dpid" 2>/dev/null || break
  sleep 0.25
done
[ "$streamed" = 1 ] || fatal "streams never came up:
$(tail -15 "$cfg/daemon.log")"

# Raw-frame listener (best effort): every reply + broadcast as JSONL.
events="$cfg/events.jsonl"; : > "$events"
have_ws=0; lpid=""
cat > "$cfg/listen.py" <<'PY'
#!/usr/bin/env python3
"""Append every Control API frame (replies + broadcasts) as JSONL."""
import asyncio, sys
from pathlib import Path

import websockets

async def main() -> int:
    port, out, secs = int(sys.argv[1]), Path(sys.argv[2]), float(sys.argv[3])
    with out.open("a", buffering=1) as f:
        async with websockets.connect(f"ws://127.0.0.1:{port}/") as ws:
            async def pump():
                async for msg in ws:
                    f.write(str(msg) + "\n")
            try:
                await asyncio.wait_for(pump(), timeout=secs)
            except Exception:
                pass
    return 0

raise SystemExit(asyncio.run(main()))
PY
if python3 -c 'import websockets' 2>/dev/null; then
  have_ws=1
  python3 "$cfg/listen.py" "$PORT" "$events" 40 &
  lpid=$!; pids+=("$lpid")
else
  note "python3 'websockets' missing - raw frame asserts skipped"
fi

# -- phases ------------------------------------------------------------

echo "== 1. fresh config: wet default, disabled"
call '{"action":"getMonitor"}'
[ "$(pick enabled)" = "false" ] || bad "fresh monitor shows enabled=$(pick enabled)"
[ "$(pickf mix)" = "1" ]        || bad "fresh monitor mix=$(pickf mix), want 1.0 (wet default)"
[ "$(pick device)" = '""' ]     || bad "fresh monitor device=$(pick device), want default"
n0=$(mon_links_to "$DEFAULT")
echo "   baseline: $n0 vm-monitor link(s) into the default sink"

echo "== 2. enable + volume/blend: wire, disk, no link churn"
sleep 0.8   # let the raw-frame listener connect
call '{"action":"setMonitor","enabled":true,"volume_db":-6,"mix":0.8}'
[ "$(reply_ok)" = "true" ] || bad "setMonitor failed: $REPLY"
sleep 0.5
call '{"action":"getMonitor"}'
[ "$(pick enabled)" = "true" ]   || bad "getMonitor enabled=$(pick enabled)"
[ "$(pickf mix)" = "0.8" ]       || bad "getMonitor mix=$(pickf mix)"
[ "$(pickf volume_db)" = "-6" ]  || bad "getMonitor volume_db=$(pickf volume_db)"
[ "$(cfgmon enabled)" = "true" ] || bad "config enabled=$(cfgmon enabled)"
[ "$(cfgmonf mix)" = "0.8" ]     || bad "config mix=$(cfgmonf mix)"
[ "$(cfgver)" = "5" ]            || bad "config version=$(cfgver), want 5"
n1=$(mon_links_to "$DEFAULT")
[ "$n1" = "$n0" ] \
  && echo "OK: volume/blend edits did not rebuild the monitor link" \
  || bad "link count churned: $n0 -> $n1"

echo "== 3. retarget to the temporary null-sink ($SMOKE_SINK)"
MOD=$(pactl load-module module-null-sink "sink_name=$SMOKE_SINK" 2>/dev/null) || MOD=""
[ -n "$MOD" ] || fatal "could not load a null-sink module"
sleep 0.8
call '{"action":"getAudioDevices"}'
uid=$(smoke_uid)
[ -n "$uid" ] || fatal "the temp sink is not visible to the daemon"
call "{\"action\":\"setMonitor\",\"device\":\"$uid\"}"
[ "$(reply_ok)" = "true" ] || bad "retarget rejected: $REPLY"
sleep 0.5
tcount=$(mon_links_to "$SMOKE_SINK")
[ "$tcount" = "2" ] || bad "temp sink has $tcount monitor link(s), want 2"
fl=$(mon_target_on FL "$SMOKE_SINK")
fr=$(mon_target_on FR "$SMOKE_SINK")
[ "$fl" = "$SMOKE_SINK:playback_FL" ] || bad "FL port paired to '$fl'"
[ "$fr" = "$SMOKE_SINK:playback_FR" ] || bad "FR port paired to '$fr'"
if [ "$fl" = "$SMOKE_SINK:playback_FL" ] && [ "$fr" = "$SMOKE_SINK:playback_FR" ]; then
  echo "OK: channel pairing FL<->FL / FR<->FR"
fi
dcount=$(mon_links_to "$DEFAULT")
[ "$dcount" = "$((n0 - 2))" ] || bad "default sink has $dcount monitor link(s), want $((n0 - 2))"
[ "$(cfgmon device)" = "\"$uid\"" ] || bad "config device=$(cfgmon device)"
call '{"action":"getMonitor"}'
[ "$(pick device)" = "\"$uid\"" ] || bad "getMonitor device=$(pick device)"

echo "== 4. back to \"\": the system default"
call '{"action":"setMonitor","device":""}'
[ "$(reply_ok)" = "true" ] || bad "retarget to default rejected: $REPLY"
sleep 0.5
dcount=$(mon_links_to "$DEFAULT")
[ "$dcount" = "$n0" ] || bad "default sink has $dcount monitor link(s), want $n0"
[ "$(mon_links_to "$SMOKE_SINK")" = "0" ] || bad "temp sink still has monitor links"
[ "$(cfgmon device)" = '""' ] || bad "config device=$(cfgmon device)"

echo "== 5. unplug: DeviceRemoved falls back to the default"
call "{\"action\":\"setMonitor\",\"device\":\"$uid\"}"
sleep 0.5
[ "$(mon_links_to "$SMOKE_SINK")" = "2" ] || bad "re-pin before the unplug failed"
pactl unload-module "$MOD" && MOD=""
if wait_config_device '""' 40; then
  echo "OK: device persisted as empty after the unplug"
else
  bad "device still $(cfgmon device) 10 s after the unplug"
fi
dcount=0
for _ in $(seq 1 16); do
  dcount=$(mon_links_to "$DEFAULT")
  [ "$dcount" = "$n0" ] && break
  sleep 0.25
done
[ "$dcount" = "$n0" ] || bad "no fallback link to the default ($dcount)"
grep -q "output device removed — falling back to system default" "$cfg/daemon.log" \
  && echo "OK: fallback logged" || bad "fallback line missing from the daemon log"
call '{"action":"getMonitor"}'
[ "$(pick enabled)" = "true" ] || bad "monitor switched itself off on unplug"
[ "$(pick device)" = '""' ] || bad "getMonitor device=$(pick device) after the fallback"

echo "== 6. disable + clean shutdown"
call '{"action":"setMonitor","enabled":false}'
[ "$(reply_ok)" = "true" ] || bad "disable failed: $REPLY"
sleep 0.5
call '{"action":"getMonitor"}'
[ "$(pick enabled)" = "false" ] || bad "getMonitor enabled=$(pick enabled)"
[ "$(cfgmon enabled)" = "false" ] || bad "config enabled=$(cfgmon enabled)"
if [ "$have_ws" = 1 ]; then
  sleep 0.4
  kill "$lpid" 2>/dev/null
  n=$(grep -c '"event":"monitorChanged"' "$events" || true)
  [ "${n:-0}" -ge 4 ] \
    && echo "OK: ${n} monitorChanged frame(s) on the wire" \
    || bad "only ${n:-0} monitorChanged frame(s)"
  grep -q '"event":"monitorChanged","enabled":true' "$events" || bad "no enabled=true frame"
  grep -q '"event":"monitorChanged","enabled":false' "$events" || bad "no enabled=false frame"
  grep -q "\"device\":\"$SMOKE_SINK#" "$events" || bad "no frame carried the temp-sink device"
fi
kill -INT "$dpid" 2>/dev/null
for _ in $(seq 1 40); do kill -0 "$dpid" 2>/dev/null || break; sleep 0.25; done
if kill -0 "$dpid" 2>/dev/null; then
  bad "daemon did not exit on SIGINT"
else
  grep -q "^stopped\." "$cfg/daemon.log" \
    && echo "OK: clean shutdown" || bad "'stopped.' missing from the log"
fi

echo
if [ "$fail" = 0 ]; then echo "SMOKE_OK"; else echo "SMOKE_FAIL"; fi
exit "$fail"
