#!/usr/bin/env bash
#
# desk-soak.sh — the M2-T05 soak: capture every 60 s, watch RSS, catch a wedge or a lost frame.
#
# Usage:  scripts/desk-soak.sh [--hours 2 | --minutes N] [--interval 60] [--evidence DIR]
#                              [--base http://127.0.0.1:18470] [--rss-limit 512]
# Exit:   0 = clean soak, 1 = a wedge, a lost frame or an RSS breach, 2 = the script could not run
#
#   --hours H     the M2-T05 figure is 2. Overrides --minutes.
#   --minutes N   for a short proving run (the 15-20 min one in the evidence bundle).
#   --interval S  seconds between capture *starts* (default 60, per the task).
#   --rss-limit M PRF-05's ceiling in MB for the field node (default 512).
#   --base        a field node that is already up. Default is the one desk-e2e.sh leaves running.
#
# This does not start a node. It attaches to the pair `scripts/desk-e2e.sh` leaves behind, which
# is deliberate: a soak that built and started its own node would be soaking a *fresh* process,
# and the interesting failures — a leak, a thread pool that grows, a camera handle that goes stale
# — are the ones that need a process to have been alive and working first.
#
# ---------------------------------------------------------------------------------------------
# What counts as a failure, and why "no wedges" needs a definition
# ---------------------------------------------------------------------------------------------
#
# "No wedges, zero lost frames" is only checkable against a definition of each, so:
#
#   lost frame  a capture was accepted (202 with a frame id) and no `preview_ready` for that id
#               arrived before the next capture was due. The frame may well be on disk — this is
#               a *pipeline* soak, and a frame the operator never sees is lost to the operator.
#
#   wedge       a capture was refused, or the node stopped answering `/api/system/health`, or two
#               consecutive captures produced no preview. One missed preview is a slow decode
#               under a burst; two in a row is the node not coming back.
#
# The RSS line is PRF-05's 512 MB, sampled from `/proc/<pid>/status` every interval. **The
# decode spikes are inside the number, not excluded.** M2-T05 permits excluding them ("with real
# decode spikes excluded per definition") and this does not, because the sample lands at a fixed
# offset after each capture rather than at a random phase — see `sample_rss`. An excluded spike is
# an unmeasured spike, and the spike is the part that would kill a Pi.

set -uo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
NAME="desk-soak"

MINUTES=120
INTERVAL=60
RSS_LIMIT_MB=512
BASE="http://127.0.0.1:${ASTROCTL_FIELD_PORT:-18470}"
EVIDENCE="$ROOT/docs/evidence/m2/desk-soak"
TOKEN="${ASTROCTL_TOKEN:-s3cret}"
# 0 = a timed capture at whatever shutter the body is set to. Non-zero sends a bulb of that many
# seconds, which is what the R10 needs when its mode dial is on **B** — in that position the body
# offers no timed shutter at all and every timed capture is correctly refused, so a soak that did
# not know about the dial would report two hours of refusals as two hours of node failures.
BULB_SECONDS=0

while [[ $# -gt 0 ]]; do
    case "$1" in
    --hours)
        MINUTES=$(( ${2:-2} * 60 )); shift 2 ;;
    --minutes) MINUTES="${2:-}"; shift 2 ;;
    --interval) INTERVAL="${2:-}"; shift 2 ;;
    --rss-limit) RSS_LIMIT_MB="${2:-}"; shift 2 ;;
    --bulb) BULB_SECONDS="${2:-}"; shift 2 ;;
    --base) BASE="${2:-}"; shift 2 ;;
    --evidence) EVIDENCE="${2:-}"; shift 2 ;;
    -h | --help) sed -n '3,16p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "$NAME: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

[[ "$MINUTES" =~ ^[0-9]+$ ]] || { echo "$NAME: --minutes/--hours need a number" >&2; exit 2; }
[[ "$INTERVAL" =~ ^[0-9]+$ ]] && [[ "$INTERVAL" -gt 0 ]] || { echo "$NAME: --interval needs seconds" >&2; exit 2; }

for tool in curl jq python3; do
    command -v "$tool" >/dev/null 2>&1 || { echo "$NAME: $tool is required" >&2; exit 2; }
done

AUTH=(-H "Authorization: Bearer $TOKEN")
curl -fsS "${AUTH[@]}" "$BASE/api/system/health" >/dev/null 2>&1 || {
    echo "$NAME: nothing is answering $BASE/api/system/health." >&2
    echo "  Start a pair first:  scripts/desk-e2e.sh --frames 1" >&2
    exit 2
}

# The field node's pid, found by who owns the port rather than by name — there may be more than
# one astroctl-field on a developer's machine and only one of them is the one under test.
PORT="${BASE##*:}"
PORT="${PORT%%/*}"
FIELD_PID="$(ss -lptnH "sport = :$PORT" 2>/dev/null | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2)"
[[ -n "$FIELD_PID" ]] || { echo "$NAME: cannot find the pid listening on $PORT" >&2; exit 2; }

mkdir -p "$EVIDENCE"
SAMPLES="$EVIDENCE/samples.csv"
EVENTS="$EVIDENCE/events.jsonl"
ROUNDS=$(( MINUTES * 60 / INTERVAL ))
[[ "$ROUNDS" -ge 1 ]] || ROUNDS=1

printf 'round,t_s,frame_id,accepted,preview_s,rss_mb,verdict\n' >"$SAMPLES"

echo
echo "  soak: $MINUTES min, one capture every ${INTERVAL}s = $ROUNDS rounds"
echo "  field node pid $FIELD_PID on port $PORT, PRF-05 line ${RSS_LIMIT_MB} MB"
echo "  evidence: $EVIDENCE"
echo

python3 "$ROOT/scripts/lib/wsobserve.py" --base "$BASE" --token "$TOKEN" --out "$EVENTS" \
    2>"$EVIDENCE/wsobserve.err" &
OBSERVER_PID=$!
trap 'kill "$OBSERVER_PID" 2>/dev/null' EXIT
for _ in $(seq 1 80); do
    grep -q connected "$EVIDENCE/wsobserve.err" 2>/dev/null && break
    sleep 0.25
done

# Resident set from the kernel rather than from anything the process says about itself: PRF-05
# bounds what the OS is holding, which is the number an OOM killer would use.
sample_rss() {
    awk '/^VmRSS:/ {print int($2/1024)}' "/proc/$FIELD_PID/status" 2>/dev/null || echo 0
}

START="$(date +%s)"
LOST=0
WEDGES=0
RSS_BREACH=0
CONSECUTIVE_MISSES=0
PEAK_RSS=0

for round in $(seq 1 "$ROUNDS"); do
    round_start="$(date +%s)"
    elapsed=$(( round_start - START ))

    # A fresh envelope per round (SDD §5.8.1). Reusing a command id would replay the first
    # round's answer out of the ledger for two hours and report a perfect soak of one capture.
    body=""
    [[ "$BULB_SECONDS" -gt 0 ]] && body="{\"bulb_seconds\": $BULB_SECONDS}"
    response="$(curl -fsS -X POST "${AUTH[@]}" -H 'Content-Type: application/json' \
        -H "astroctl-command-id: $(cat /proc/sys/kernel/random/uuid)" \
        -H "astroctl-issued-at: $(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)" \
        ${body:+-d "$body"} \
        "$BASE/api/camera/capture" 2>/dev/null)"
    frame="$(echo "$response" | jq -r '.frame_id // empty' 2>/dev/null)"

    if [[ -z "$frame" ]]; then
        WEDGES=$((WEDGES + 1))
        CONSECUTIVE_MISSES=$((CONSECUTIVE_MISSES + 1))
        printf '%s,%s,,no,,%s,CAPTURE REFUSED\n' "$round" "$elapsed" "$(sample_rss)" >>"$SAMPLES"
        printf '  %3d/%d  t=%-6s  CAPTURE REFUSED — %s\n' \
            "$round" "$ROUNDS" "${elapsed}s" "$(echo "$response" | head -c 120)"
    else
        # Wait for the preview, but never past this round's own slot: a soak that let one slow
        # frame push the schedule would stop being a "capture every 60 s" soak by round three.
        deadline=$(( round_start + INTERVAL - 3 ))
        preview_at=""
        while [[ "$(date +%s)" -lt "$deadline" ]]; do
            if grep "\"$frame\"" "$EVENTS" 2>/dev/null | grep -q preview_ready; then
                preview_at="$(python3 -c '
import json,sys
frame=sys.argv[1]
saved=ready=None
for line in open(sys.argv[2]):
    try: e=json.loads(line)
    except Exception: continue
    d=e.get("data") or {}
    if d.get("frame_id")!=frame or e.get("topic")!="capture.progress": continue
    if d.get("state")=="saved" and saved is None: saved=e["t"]
    if d.get("state")=="preview_ready" and ready is None: ready=e["t"]
print(f"{ready-saved:.3f}" if (saved is not None and ready is not None) else "")
' "$frame" "$EVENTS" 2>/dev/null)"
                break
            fi
            sleep 1
        done

        rss="$(sample_rss)"
        [[ "$rss" -gt "$PEAK_RSS" ]] && PEAK_RSS="$rss"
        [[ "$rss" -gt "$RSS_LIMIT_MB" ]] && RSS_BREACH=$((RSS_BREACH + 1))

        if [[ -n "$preview_at" ]]; then
            CONSECUTIVE_MISSES=0
            printf '%s,%s,%s,yes,%s,%s,ok\n' "$round" "$elapsed" "$frame" "$preview_at" "$rss" >>"$SAMPLES"
            printf '  %3d/%d  t=%-6s  %s  preview %ss  rss %s MB\n' \
                "$round" "$ROUNDS" "${elapsed}s" "$frame" "$preview_at" "$rss"
        else
            LOST=$((LOST + 1))
            CONSECUTIVE_MISSES=$((CONSECUTIVE_MISSES + 1))
            [[ "$CONSECUTIVE_MISSES" -ge 2 ]] && WEDGES=$((WEDGES + 1))
            printf '%s,%s,%s,yes,,%s,LOST\n' "$round" "$elapsed" "$frame" "$rss" >>"$SAMPLES"
            printf '  %3d/%d  t=%-6s  %s  LOST (no preview in slot)  rss %s MB\n' \
                "$round" "$ROUNDS" "${elapsed}s" "$frame" "$rss"
        fi
    fi

    if ! curl -fsS "${AUTH[@]}" "$BASE/api/system/health" >/dev/null 2>&1; then
        WEDGES=$((WEDGES + 1))
        printf '  %3d/%d  the node stopped answering /api/system/health\n' "$round" "$ROUNDS"
    fi

    # Sleep to the *slot*, not for the interval: the whole claim is "every 60 s", and adding the
    # capture's own duration to every sleep would make it every 75 and then every 90.
    next=$(( round_start + INTERVAL ))
    now="$(date +%s)"
    [[ "$now" -lt "$next" ]] && sleep $(( next - now ))
done

kill "$OBSERVER_PID" 2>/dev/null
wait "$OBSERVER_PID" 2>/dev/null

python3 "$ROOT/scripts/lib/soak_report.py" \
    --samples "$SAMPLES" --events "$EVENTS" --out "$EVIDENCE/soak.md" \
    --minutes "$MINUTES" --interval "$INTERVAL" --rss-limit "$RSS_LIMIT_MB"
status=$?

echo
cat "$EVIDENCE/soak.md"
echo
if [[ "$LOST" -eq 0 && "$WEDGES" -eq 0 && "$RSS_BREACH" -eq 0 && "$status" -eq 0 ]]; then
    printf '\033[32mSOAK CLEAN\033[0m — %s rounds, peak RSS %s MB under the %s MB line\n' \
        "$ROUNDS" "$PEAK_RSS" "$RSS_LIMIT_MB"
    exit 0
fi
printf '\033[31mSOAK FAILED\033[0m — %s lost, %s wedge(s), %s RSS breach(es), peak %s MB\n' \
    "$LOST" "$WEDGES" "$RSS_BREACH" "$PEAK_RSS"
exit 1
