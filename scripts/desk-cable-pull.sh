#!/usr/bin/env bash
#
# desk-cable-pull.sh — watch a field node through a real cable pull, and time the recovery.
#
# Usage:  scripts/desk-cable-pull.sh [--base http://127.0.0.1:18470] [--seconds 180]
#                                    [--evidence DIR] [--no-liveview]
# Exit:   0 = the node recovered to a working capture, 1 = it did not, 2 = could not run
#
# This is the **observation** half of T-CAM-1. The other half is a human pulling a USB cable out
# of a camera, which is not something a script should do and — on this body — not something it
# *can* do safely: M2-T04 established that the software `USBDEVFS_RESET` stand-in is destructive
# here, taking the R10 off the bus until it was physically power-cycled. So the script watches and
# the operator pulls.
#
# ---------------------------------------------------------------------------------------------
# The procedure
# ---------------------------------------------------------------------------------------------
#
#   1. Have a pair up:            scripts/desk-e2e.sh --frames 1
#   2. Start this watcher:        scripts/desk-cable-pull.sh
#   3. Wait for it to say ARMED. It starts live view first, because REL-03's acceptance criterion
#      is a wedge induced *mid-liveview* — a camera sitting idle when the cable goes is an easier
#      problem than the one the criterion asks about.
#   4. **Pull the USB cable from the camera end.** The camera end, not the host end: it is the
#      connector the operator will actually knock in the dark, and on this body the two are not
#      equivalent — the host end can leave the hub's port powered and re-enumerate differently.
#   5. Watch the terminal. It prints each transition as it arrives.
#   6. **Plug it back in** when the watcher says so (about 10 s later — long enough that the
#      driver has certainly noticed, short enough to stay inside REL-03's 30 s).
#   7. The watcher takes a capture at the end. That is the criterion: not "the badge went green"
#      but "the camera works again".
#
# ---------------------------------------------------------------------------------------------
# What it reports, and against what
# ---------------------------------------------------------------------------------------------
#
# REL-03 and T-CAM-1 ask for automatic recovery to a working capture within 30 s of the fault. The
# numbers printed are all offsets from **the first evidence the node had that anything was wrong**
# — the first `camera.status: connected=false` or the first `CAMERA_RECONNECTING` alert, whichever
# lands first. Timing from the moment the operator's hand moved would be kinder and meaningless;
# nothing measures that.
#
# On this body there is a known third state to watch for. M2-T04 found that a desktop gvfs will
# auto-mount the camera the moment it re-enumerates and hold the USB claim, which blocks recovery
# for as long as it holds it — measured at 80 s in the spike. The driver detects this and names
# the releasing command in its alert; if that alert appears, run what it prints.

set -uo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
NAME="desk-cable-pull"

BASE="http://127.0.0.1:${ASTROCTL_FIELD_PORT:-18470}"
SECONDS_TO_WATCH=180
EVIDENCE="$ROOT/docs/evidence/m2/cable-pull"
TOKEN="${ASTROCTL_TOKEN:-s3cret}"
LIVEVIEW=1

while [[ $# -gt 0 ]]; do
    case "$1" in
    --base) BASE="${2:-}"; shift 2 ;;
    --seconds) SECONDS_TO_WATCH="${2:-}"; shift 2 ;;
    --evidence) EVIDENCE="${2:-}"; shift 2 ;;
    --no-liveview) LIVEVIEW=0; shift ;;
    -h | --help) sed -n '3,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "$NAME: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

for tool in curl jq python3; do
    command -v "$tool" >/dev/null 2>&1 || { echo "$NAME: $tool is required" >&2; exit 2; }
done

AUTH=(-H "Authorization: Bearer $TOKEN")
envelope=(-H "astroctl-command-id: $(cat /proc/sys/kernel/random/uuid)"
          -H "astroctl-issued-at: $(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)")

curl -fsS "${AUTH[@]}" "$BASE/api/system/health" >/dev/null 2>&1 || {
    echo "$NAME: nothing answering $BASE. Start a pair: scripts/desk-e2e.sh --frames 1" >&2
    exit 2
}

mkdir -p "$EVIDENCE"
EVENTS="$EVIDENCE/events.jsonl"

python3 "$ROOT/scripts/lib/wsobserve.py" --base "$BASE" --token "$TOKEN" --out "$EVENTS" \
    --seconds "$SECONDS_TO_WATCH" --echo 2>"$EVIDENCE/wsobserve.err" &
OBSERVER=$!
trap 'kill "$OBSERVER" 2>/dev/null' EXIT
for _ in $(seq 1 80); do
    grep -q connected "$EVIDENCE/wsobserve.err" 2>/dev/null && break
    sleep 0.25
done

if [[ "$LIVEVIEW" -eq 1 ]]; then
    curl -fsS -X POST "${AUTH[@]}" "${envelope[@]}" "$BASE/api/camera/liveview/start" >/dev/null 2>&1 \
        && echo "  live view started (T-CAM-1 induces the wedge mid-stream)" \
        || echo "  live view would not start; continuing without it"
fi

cat <<EOF

  ================================================================
   ARMED — pull the USB cable from the CAMERA end now.
   Plug it back in about 10 seconds later.
  ================================================================

  Transitions as they arrive (t = seconds since this watcher started):

EOF

wait "$OBSERVER" 2>/dev/null

echo
echo "  the watch window closed; taking a capture to see whether the camera works"
CAPTURE="$(curl -fsS -X POST "${AUTH[@]}" \
    -H "astroctl-command-id: $(cat /proc/sys/kernel/random/uuid)" \
    -H "astroctl-issued-at: $(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)" \
    -H 'Content-Type: application/json' "$BASE/api/camera/capture" 2>&1)" || true
echo "  capture: $CAPTURE"
echo "$CAPTURE" | jq -e '.frame_id' >/dev/null 2>&1 && WORKING=1 || WORKING=0

python3 "$ROOT/scripts/lib/pull_report.py" \
    --events "$EVENTS" --out "$EVIDENCE/cable-pull.md" --working "$WORKING"
status=$?
echo
cat "$EVIDENCE/cable-pull.md"
exit $status
