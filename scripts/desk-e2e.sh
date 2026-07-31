#!/usr/bin/env bash
#
# desk-e2e.sh — the M1 demo, rerun against a real camera. M2-T05.
#
# Usage:  scripts/desk-e2e.sh [--no-build] [--frames N] [--bulb SECONDS] [--simulator]
#                             [--workdir DIR] [--evidence DIR] [--keep|--stop]
# Exit:   0 = the run passed, 1 = an assertion failed, 2 = the script could not run
#
#   --no-build   use the binaries already built. Building is the default for dev-up.sh's reason.
#   --frames N   timed captures before the bulb frame (default 5, the M2-T05 figure).
#   --bulb S     the bulb exposure's length in seconds (default 10).
#   --simulator  drive the simulator instead of the camera. Not the point of this script — it is
#                here so the script itself can be exercised without a body on the desk, and so a
#                failure can be attributed to the driver rather than to the harness.
#   --workdir    where sessions, logs and configs go (default: a fresh mktemp -d).
#   --evidence   where the bundle is written (default: docs/evidence/m2/desk-e2e).
#   --keep       leave both nodes running when the run finishes. **This is the default**, because
#                M2-T05 asks for a working pair the operator can point the PWA at afterwards.
#   --stop       tear the pair down instead.
#
# ---------------------------------------------------------------------------------------------
# Why this is not scripts/e2e.sh
# ---------------------------------------------------------------------------------------------
#
# `e2e.sh` drives the container pair, and a container is exactly the wrong shape here: the camera
# is a USB device on *this* machine, claimed exclusively by one process, and the whole subject of
# the run is the path from that device to the operator's screen. So this script runs the two
# binaries directly on the host, with configs generated into a scratch directory from the shipped
# examples — patched, not rewritten, so that a field added to the example is a field this run
# exercises rather than one it silently omits.
#
# The mount is the simulator throughout, and that is not a compromise: there is no mount on the
# desk, M2's subject is the camera, and the M1 stack above the HAL cannot tell the difference —
# which is the claim this whole task exists to test.
#
# ---------------------------------------------------------------------------------------------
# What it measures, and why the numbers come off the event stream
# ---------------------------------------------------------------------------------------------
#
# The acceptance criterion is "previews <= 3 s after exposure end". Both ends of that are events,
# not responses: `capture.progress: saved` is published when the frame is durable, and
# `preview_ready` when the preview is cached and pushed. So the run holds a `/ws` subscription
# open for its whole length (`scripts/lib/wsobserve.py`) and the timings are differences between
# arrival times in that one stream — one clock, monotonic, no round-trip of the script's own
# added to the measurement.
#
# `saved` is the honest "exposure end" here and it is *pessimistic*: it lands after the download,
# not when the shutter closed, so a preview measured from it has already spent the download inside
# its budget. Measuring from a shutter-close the node does not publish would flatter the result.

set -uo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
NAME="desk-e2e"

BUILD=1
FRAMES=5
BULB_SECONDS=10
DRIVER="gphoto2"
WORKDIR=""
EVIDENCE="$ROOT/docs/evidence/m2/desk-e2e"
KEEP=1

FIELD_PORT="${ASTROCTL_FIELD_PORT:-18470}"
STACK_PORT="${ASTROCTL_STACK_PORT:-18471}"
TOKEN="${ASTROCTL_TOKEN:-s3cret}"

while [[ $# -gt 0 ]]; do
    case "$1" in
    --no-build) BUILD=0; shift ;;
    --simulator) DRIVER="simulator"; shift ;;
    --keep) KEEP=1; shift ;;
    --stop) KEEP=0; shift ;;
    --frames)
        FRAMES="${2:-}"
        [[ "$FRAMES" =~ ^[0-9]+$ ]] || { echo "$NAME: --frames needs a number" >&2; exit 2; }
        shift 2 ;;
    --bulb)
        BULB_SECONDS="${2:-}"
        [[ "$BULB_SECONDS" =~ ^[0-9]+$ ]] || { echo "$NAME: --bulb needs seconds" >&2; exit 2; }
        shift 2 ;;
    --workdir) WORKDIR="${2:-}"; shift 2 ;;
    --evidence) EVIDENCE="${2:-}"; shift 2 ;;
    -h | --help) sed -n '3,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "$NAME: unknown argument '$1'" >&2; exit 2 ;;
    esac
done

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
note() { printf '   %s\n' "$*"; }
fail() { printf '\033[31mFAIL: %s\033[0m\n' "$*" >&2; FAILURES=$((FAILURES + 1)); }
FAILURES=0

# ---------------------------------------------------------------------------------------------
# 0. Preflight
# ---------------------------------------------------------------------------------------------
say "preflight"

for tool in curl jq python3; do
    command -v "$tool" >/dev/null 2>&1 || { echo "$NAME: $tool is required" >&2; exit 2; }
done

if [[ "$DRIVER" == "gphoto2" ]]; then
    if ! lsusb 2>/dev/null | grep -qi "canon"; then
        echo "$NAME: no Canon camera on USB. Plug the R10 in, or pass --simulator." >&2
        exit 2
    fi
    note "camera: $(lsusb | grep -i canon | head -1)"

    # gvfs auto-mounts a camera on hotplug and holds the USB claim; M2-T01 measured 80 s of a
    # blocked reconnect before anyone worked out why. The driver diagnoses this and names the
    # command — but a run that starts by *not* being blocked is a run whose timings mean what
    # they say, so the mount is released here rather than diagnosed later.
    mount_uri="$(gio mount -l 2>/dev/null | grep -o 'gphoto2://[^ ]*' | head -1)"
    if [[ -n "$mount_uri" ]]; then
        note "releasing the gvfs claim on $mount_uri"
        gio mount -u "$mount_uri" >/dev/null 2>&1 || true
        sleep 1
    fi
fi

WORKDIR="${WORKDIR:-$(mktemp -d -t astroctl-desk-XXXXXX)}"
mkdir -p "$WORKDIR"/{field-sessions,field-logs,field-queue,stack-sessions,stack-logs,evidence}
note "workdir:  $WORKDIR"
note "evidence: $EVIDENCE"

# ---------------------------------------------------------------------------------------------
# 1. Build
# ---------------------------------------------------------------------------------------------
FIELD_FEATURES=()
[[ "$DRIVER" == "gphoto2" ]] && FIELD_FEATURES=(--features libgphoto2)

if [[ "$BUILD" -eq 1 ]]; then
    say "build"
    # Release, and not only for speed: the CR3 decode is the one stage whose *measurement* is an
    # acceptance criterion, and a debug build's bounds-checked pass over 24 million photosites is
    # not the thing that ships. A 3 s budget met by a debug binary would prove nothing.
    if ! cargo build --release -p astroctl-field "${FIELD_FEATURES[@]}"; then
        echo "$NAME: the field node did not build." >&2
        [[ "$DRIVER" == "gphoto2" ]] && cat <<'EOF' >&2

  The gphoto2 driver needs libgphoto2-dev at *build* time — `libgphoto2_sys` runs pkg-config
  and bindgen in its build script, so without the headers the crate fails to compile rather
  than to link. Either:

      sudo apt install libgphoto2-dev

  ...or, with no root, unpack the .deb and point pkg-config at it:

      apt-get download libgphoto2-dev libgphoto2-6t64
      for d in *.deb; do dpkg -x "$d" ./prefix; done
      export PKG_CONFIG_PATH=$PWD/prefix/usr/lib/x86_64-linux-gnu/pkgconfig
      export LD_LIBRARY_PATH=$PWD/prefix/usr/lib/x86_64-linux-gnu
EOF
        exit 2
    fi
    cargo build --release -p astroctl-stack || { echo "$NAME: the stack node did not build." >&2; exit 2; }
fi

FIELD_BIN="$ROOT/target/release/astroctl-field"
STACK_BIN="$ROOT/target/release/astroctl-stack"
[[ -x "$FIELD_BIN" ]] || { echo "$NAME: $FIELD_BIN is missing; drop --no-build" >&2; exit 2; }
[[ -x "$STACK_BIN" ]] || { echo "$NAME: $STACK_BIN is missing; drop --no-build" >&2; exit 2; }

# ---------------------------------------------------------------------------------------------
# 2. Configs, patched from the shipped examples
# ---------------------------------------------------------------------------------------------
say "configs"

FIELD_YAML="$WORKDIR/field-node.yaml"
STACK_YAML="$WORKDIR/stacking-server.yaml"

# `sed` over the examples rather than a heredoc of our own, and the reason is the same one
# `astroctl_field::test_support` gives for doing it this way: a config written out here would stop
# exercising a key the moment somebody added one to the example, and nothing would notice.
sed \
    -e "s|^  driver: skywatcher|  driver: simulator|" \
    -e "s|^  driver: gphoto2|  driver: $DRIVER|" \
    -e "s|^  sessions_dir: /data/astro/sessions|  sessions_dir: $WORKDIR/field-sessions|" \
    -e "s|^  queue_dir: /data/astro/transfer_queue.*|  queue_dir: $WORKDIR/field-queue|" \
    -e "s|^  log_dir: /data/astro/logs|  log_dir: $WORKDIR/field-logs|" \
    -e "s|^  host: 192.168.1.100.*|  host: 127.0.0.1|" \
    -e "s|^  port: 8471|  port: $STACK_PORT|" \
    -e "s|^  port: 8470|  port: $FIELD_PORT|" \
    -e "s|^  host: 0.0.0.0.*|  host: 127.0.0.1|" \
    -e "s|^  enabled: true$|  enabled: false|" \
    "$ROOT/config/field-node.example.yaml" >"$FIELD_YAML"

# The `enabled: true` rewrite above hits both `stacking_server` and `llm`, and only the second is
# wanted off — there is no API key on the desk and a node that fails to start over one would fail
# the run for a reason M2-T05 is not about. Put the transfer queue back on.
python3 - "$FIELD_YAML" <<'PY'
import re, sys
path = sys.argv[1]
text = open(path).read()
text = re.sub(r"(stacking_server:\n  enabled: )false", r"\1true", text)
open(path, "w").write(text)
PY

sed \
    -e "s|^  sessions_dir: /data/astro/sessions|  sessions_dir: $WORKDIR/stack-sessions|" \
    -e "s|^  log_dir: /data/astro/logs|  log_dir: $WORKDIR/stack-logs|" \
    -e "s|^  host: 0.0.0.0.*|  host: 127.0.0.1|" \
    -e "s|^  port: 8471|  port: $STACK_PORT|" \
    -e "s|^  python_interpreter: .*|  python_interpreter: $(command -v python3)|" \
    "$ROOT/config/stacking-server.example.yaml" >"$STACK_YAML"

note "field: $FIELD_YAML"
note "stack: $STACK_YAML"

# ---------------------------------------------------------------------------------------------
# 3. Start the pair
# ---------------------------------------------------------------------------------------------
say "start"

# Kill by port owner rather than by a pid file. A pid file left by a crashed run points at
# whatever the OS has since reused the number for, and the question actually being asked is "is
# something already answering on this port" — which `ss` answers directly.
free_port() {
    local port="$1" pid
    pid="$(ss -lptnH "sport = :$port" 2>/dev/null | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2)"
    if [[ -n "$pid" ]]; then
        note "port $port is held by pid $pid; stopping it"
        kill "$pid" 2>/dev/null || true
        for _ in $(seq 1 40); do
            kill -0 "$pid" 2>/dev/null || break
            sleep 0.25
        done
        kill -9 "$pid" 2>/dev/null || true
    fi
}
free_port "$STACK_PORT"
free_port "$FIELD_PORT"

export ASTROCTL_TOKEN="$TOKEN"
# The one deployment knob this run had to discover. `rawler` decodes CRX on a thread pool and
# glibc hands every thread its own malloc arena, so on a many-core host peak RSS for repeated
# 24 MP decodes settles around 253 MB — measured — against PRF-05's 512 MB for the whole node.
# Capping the arenas took the same measurement to 102 MB with no change in decode time. It is set
# here rather than in deploy/ because deploy/ is outside this task's blast radius; see the
# evidence bundle, which asks for it to be made permanent.
export MALLOC_ARENA_MAX="${MALLOC_ARENA_MAX:-2}"

"$STACK_BIN" --config "$STACK_YAML" >"$WORKDIR/stack.out" 2>&1 &
STACK_PID=$!
"$FIELD_BIN" --config "$FIELD_YAML" >"$WORKDIR/field.out" 2>&1 &
FIELD_PID=$!

FIELD="http://127.0.0.1:$FIELD_PORT"
STACK="http://127.0.0.1:$STACK_PORT"
AUTH=(-H "Authorization: Bearer $TOKEN")

wait_for_health() {
    local base="$1" label="$2"
    for _ in $(seq 1 120); do
        if curl -fsS "${AUTH[@]}" "$base/api/system/health" >/dev/null 2>&1; then
            note "$label is up"
            return 0
        fi
        sleep 0.25
    done
    echo "$NAME: $label never answered /api/system/health" >&2
    tail -40 "$WORKDIR/${label}.out" >&2 2>/dev/null || true
    return 1
}
wait_for_health "$STACK" stack || exit 1
wait_for_health "$FIELD" field || exit 1

cleanup() {
    [[ -n "${OBSERVER_PID:-}" ]] && kill "$OBSERVER_PID" 2>/dev/null
    if [[ "$KEEP" -eq 0 ]]; then
        kill "$FIELD_PID" "$STACK_PID" 2>/dev/null
    fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------------------------------
# 4. The event stream, open for the whole run
# ---------------------------------------------------------------------------------------------
EVENTS="$WORKDIR/events.jsonl"
python3 "$ROOT/scripts/lib/wsobserve.py" --base "$FIELD" --token "$TOKEN" --out "$EVENTS" \
    2>"$WORKDIR/wsobserve.err" &
OBSERVER_PID=$!
# Block on the observer's own "connected" line rather than on a sleep: an event published before
# the socket is up is an event this run cannot measure, and a fixed sleep is a guess about how
# long that takes.
for _ in $(seq 1 80); do
    grep -q "connected" "$WORKDIR/wsobserve.err" 2>/dev/null && break
    sleep 0.25
done
grep -q "connected" "$WORKDIR/wsobserve.err" || { echo "$NAME: the event observer never connected" >&2; cat "$WORKDIR/wsobserve.err" >&2; exit 1; }
note "watching $FIELD/ws"

# Every mutating route wants SDD §5.8.1's envelope: a client-generated `command_id` that makes the
# request idempotent under a retry, and an `issued_at` the node refuses if it is older than
# `max_command_age_ms` (2 s by default). Both are headers, and both must be *fresh per request* —
# reusing a command id is how you get the previous answer replayed out of the ledger instead of a
# new capture, which in a five-frame run would look like four frames vanishing.
api() {
    local method="$1" path="$2" body="${3:-}"
    local -a envelope_headers=(
        -H "astroctl-command-id: $(cat /proc/sys/kernel/random/uuid)"
        -H "astroctl-issued-at: $(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)"
    )
    if [[ -n "$body" ]]; then
        curl -fsS -X "$method" "${AUTH[@]}" "${envelope_headers[@]}" \
            -H 'Content-Type: application/json' -d "$body" "$FIELD$path"
    else
        curl -fsS -X "$method" "${AUTH[@]}" "${envelope_headers[@]}" "$FIELD$path"
    fi
}

# ---------------------------------------------------------------------------------------------
# 5. Connect, settings, live view
# ---------------------------------------------------------------------------------------------
say "connect"
CONNECT="$(api POST /api/camera/connect)" || { fail "connect refused"; exit 1; }
note "camera.status: $CONNECT"
echo "$CONNECT" | jq -e '.connected == true' >/dev/null || fail "the camera did not report connected"

say "settings"
# §5.8.1 flattens the current settings into the top level of the body, so these are `.iso` and
# not `.current.iso` — the shape the PWA reads.
SETTINGS="$(api GET /api/camera/settings)" || fail "settings unreadable"
echo "$SETTINGS" | jq -c '{iso, shutter, aperture, format, offered: (.available | map_values(length))}' \
    2>/dev/null || echo "$SETTINGS"
ISO="$(echo "$SETTINGS" | jq -r '.iso // "?"')"
FORMAT="$(echo "$SETTINGS" | jq -r '.format // "?"')"
note "the body reports iso=$ISO shutter=$(echo "$SETTINGS" | jq -r '.shutter // "?"') format=$FORMAT"

# The settings *round trip*, which is the half of "connect → settings" worth proving: the reply is
# read back from the camera rather than echoed, so a body that coerced the value says so.
#
# It is also what makes the timed captures take a minute rather than three. `default_shutter` is
# `"30"`, and five 30-second frames plus their downloads is most of the battery this run has. A
# short exposure exercises exactly the same path.
TARGET_SHUTTER="${ASTROCTL_DESK_SHUTTER:-}"
if [[ -z "$TARGET_SHUTTER" ]]; then
    # Ask the body which of these it offers rather than assuming: the tokens are the camera's,
    # and M2-T02 found the R10's list changes with the mode dial.
    for candidate in "1/60" "1/30" "1" "2"; do
        if echo "$SETTINGS" | jq -e --arg s "$candidate" '.available.shutters | index($s)' >/dev/null 2>&1; then
            TARGET_SHUTTER="$candidate"
            break
        fi
    done
fi
if [[ -n "$TARGET_SHUTTER" ]]; then
    UPDATED="$(api PUT /api/camera/settings "{\"shutter\": \"$TARGET_SHUTTER\"}")" || true
    READ_BACK="$(echo "$UPDATED" | jq -r '.shutter // "?"')"
    if [[ "$READ_BACK" == "$TARGET_SHUTTER" ]]; then
        note "shutter set to $TARGET_SHUTTER and read back from the camera as $READ_BACK"
    else
        note "asked for shutter $TARGET_SHUTTER, the body reports $READ_BACK"
    fi
    SETTINGS="$UPDATED"
fi
SHUTTER="$(echo "$SETTINGS" | jq -r '.shutter // "?"')"

say "live view"
# A second observer, on the second socket. `/ws` carries events and `/ws/liveview` carries the
# frames themselves (SDD §8.3(5)), so counting frames on `/ws` would always report zero — which
# is exactly what the first run of this script did.
LIVEVIEW_EVENTS="$WORKDIR/liveview.jsonl"
python3 "$ROOT/scripts/lib/wsobserve.py" --base "$FIELD" --token "$TOKEN" \
    --path /ws/liveview --out "$LIVEVIEW_EVENTS" 2>"$WORKDIR/wsliveview.err" &
LIVEVIEW_PID=$!
for _ in $(seq 1 40); do
    grep -q connected "$WORKDIR/wsliveview.err" 2>/dev/null && break
    sleep 0.25
done

if api POST /api/camera/liveview/start >/dev/null 2>&1; then
    note "streaming for 8 s at the configured rate"
    sleep 8
    api POST /api/camera/liveview/stop >/dev/null 2>&1 || true
    LIVEVIEW_FRAMES="$(wc -l <"$LIVEVIEW_EVENTS" 2>/dev/null || echo 0)"
    note "stopped after $LIVEVIEW_FRAMES frames on /ws/liveview"
    if [[ "$DRIVER" == "gphoto2" ]] && [[ "$LIVEVIEW_FRAMES" -lt 20 ]]; then
        fail "live view produced $LIVEVIEW_FRAMES frames in 8 s; PRF-02 wants >= 5 fps"
    fi
else
    fail "live view would not start"
fi
kill "$LIVEVIEW_PID" 2>/dev/null
wait "$LIVEVIEW_PID" 2>/dev/null

# ---------------------------------------------------------------------------------------------
# 6. The captures
# ---------------------------------------------------------------------------------------------
capture() {
    local body="$1" label="$2" response frame
    response="$(api POST /api/camera/capture "$body")" || { fail "$label: capture refused"; return 1; }
    frame="$(echo "$response" | jq -r '.frame_id')"
    note "$label -> $frame (exposure $(echo "$response" | jq -r '.exposure_s')s)"
    # Wait on the preview event rather than polling the frame listing: the preview is the last
    # step of the pipeline under test, so a run that waits for it is a run whose next capture
    # cannot overlap the previous one's decode and confuse the timings.
    for _ in $(seq 1 400); do
        if grep -q "\"$frame\"" "$EVENTS" 2>/dev/null &&
            grep "\"$frame\"" "$EVENTS" | grep -q "preview_ready"; then
            return 0
        fi
        sleep 0.25
    done
    fail "$label: no preview_ready for $frame within 100 s"
    return 1
}

say "$FRAMES timed captures"
# ---------------------------------------------------------------------------------------------
# The physical mode dial decides which half of this run is possible, and no script can move it.
#
# On the R10 the dial is not a setting — it is a constraint on the settings. With it on **B** the
# body offers `bulb` as its only shutter speed, so a timed capture has no duration to fire for and
# the driver correctly refuses one (M2-T02 established this and asserts the refusal). With it on
# **M** the body enumerates 30"..1/4000 and offers no `bulb` at all, so the bulb frame is the one
# that cannot run.
#
# So a single run can have five timed frames or one bulb frame, never both, and which one depends
# on where a human last left a knob. Rather than fail whichever half the dial forbids — which
# would report a working camera as a broken one — the run does the half it can, says which, and
# prints the command for the other.
# ---------------------------------------------------------------------------------------------
TIMED_DONE=0
if [[ "$SHUTTER" == "bulb" && "$DRIVER" == "gphoto2" ]]; then
    cat <<EOF

  SKIPPED — the body offers only 'bulb', so the mode dial is on **B**.

  A timed capture has no duration to fire for and the driver refuses it. For this half:

      turn the dial to M, then:  scripts/desk-e2e.sh --no-build --frames $FRAMES --bulb 0

EOF
    TIMED_SKIPPED=1
else
    for i in $(seq 1 "$FRAMES"); do
        capture "" "timed $i/$FRAMES" && TIMED_DONE=$((TIMED_DONE + 1))
    done
fi

BULB_DONE=0
if [[ "$BULB_SECONDS" -eq 0 ]]; then
    say "bulb capture skipped (--bulb 0)"
    BULB_SKIPPED=1
else
    say "one bulb capture (${BULB_SECONDS}s)"
    if [[ "$SHUTTER" == "bulb" || "$DRIVER" == "simulator" ]]; then
        capture "{\"bulb_seconds\": $BULB_SECONDS}" "bulb" && BULB_DONE=1
    else
        cat <<EOF

  SKIPPED — the body's shutter reads '$SHUTTER', not 'bulb', so the dial is on **M**.

      turn the dial to B, then:  scripts/desk-e2e.sh --no-build --frames 0 --bulb $BULB_SECONDS

EOF
        BULB_SKIPPED=1
    fi
fi

# ---------------------------------------------------------------------------------------------
# 7. The stack node's side
# ---------------------------------------------------------------------------------------------
say "transfer and stack"
TRANSFER=""
for _ in $(seq 1 120); do
    TRANSFER="$(api GET /api/transfer/status 2>/dev/null)" || true
    if [[ -n "$TRANSFER" ]] && [[ "$(echo "$TRANSFER" | jq -r '.pending // 0')" == "0" ]]; then
        break
    fi
    sleep 0.5
done
note "transfer: $TRANSFER"

STACK_HEALTH="$(curl -fsS "${AUTH[@]}" "$STACK/api/system/health" 2>/dev/null)" || true
note "stack health: $(echo "$STACK_HEALTH" | jq -c '.' 2>/dev/null || echo "$STACK_HEALTH")"

# ---------------------------------------------------------------------------------------------
# 8. The evidence
# ---------------------------------------------------------------------------------------------
say "evidence"
sleep 2 # let any trailing preview_ready land before the stream is analysed
kill "$OBSERVER_PID" 2>/dev/null
wait "$OBSERVER_PID" 2>/dev/null
OBSERVER_PID=""

mkdir -p "$EVIDENCE"
cp "$EVENTS" "$EVIDENCE/events.jsonl" 2>/dev/null || true
# The live-view frames are recorded in their own file (their own socket), so they are folded into
# the one the analyser reads. Only their sizes and arrival times are in there — see wsobserve.py.
cat "$LIVEVIEW_EVENTS" >>"$EVENTS" 2>/dev/null || true
cp "$LIVEVIEW_EVENTS" "$EVIDENCE/liveview.jsonl" 2>/dev/null || true
cp "$WORKDIR/field.out" "$EVIDENCE/field-node.log" 2>/dev/null || true
cp "$WORKDIR/stack.out" "$EVIDENCE/stack-node.log" 2>/dev/null || true
cp "$FIELD_YAML" "$EVIDENCE/field-node.yaml" 2>/dev/null || true

python3 "$ROOT/scripts/lib/desk_timings.py" \
    --events "$EVENTS" \
    --out "$EVIDENCE/timings.md" \
    --json "$EVIDENCE/timings.json" \
    --driver "$DRIVER" \
    --budget 3.0 || fail "the timings did not meet the budget"

echo
cat "$EVIDENCE/timings.md"

# ---------------------------------------------------------------------------------------------
say "result"
if [[ "$FAILURES" -eq 0 ]]; then
    printf '\033[32mPASS\033[0m — %s timed + %s bulb, evidence in %s\n' \
        "$TIMED_DONE" "$BULB_DONE" "$EVIDENCE"
    [[ "${TIMED_SKIPPED:-0}" -eq 1 ]] && printf '  (the timed half needs the dial on M — see above)\n'
    [[ "${BULB_SKIPPED:-0}" -eq 1 ]] && printf '  (the bulb half needs the dial on B — see above)\n'
else
    printf '\033[31m%s check(s) failed\033[0m — evidence in %s\n' "$FAILURES" "$EVIDENCE"
fi

if [[ "$KEEP" -eq 1 ]]; then
    cat <<EOF

  The pair is still up, which is the point — open the PWA against the real camera:

      $FIELD          (token: $TOKEN)

  Stop it with:   scripts/desk-e2e.sh --stop   (or kill $FIELD_PID $STACK_PID)
  Logs:           $WORKDIR/field.out  $WORKDIR/stack.out
EOF
fi

exit $((FAILURES > 0))
