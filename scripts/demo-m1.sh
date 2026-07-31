#!/usr/bin/env bash
#
# demo-m1.sh — bring up the M1 walking skeleton and put it on a phone.
#
# Usage:  scripts/demo-m1.sh [--no-build] [--port <n>] [--address <ip>]
# Exit:   0 = the pair is up and the walkthrough is printed, 1 = it did not come up,
#         2 = the script could not run
#
#   --no-build   start what is already built (see dev-up.sh for why building is the default).
#   --port       the published port, if it is not the harness default.
#   --address    the address to put in the URL, when the guess is wrong — a machine with several
#                interfaces, or a VPN whose address is not the default route's.
#
# This is the M1 exit demo of IMP §2, executable: two nodes, a phone, and the operator story of
# connect → goto → capture → watch the stack preview arrive. The walkthrough it prints is the
# whole of `docs/plan/tasks/M1-walking-skeleton/DEMO.md`'s happy path, so the demo cannot drift
# from its own script without somebody noticing on the next run.
#
# ---------------------------------------------------------------------------------------------
# Why the address is not localhost, and why the script works it out
# ---------------------------------------------------------------------------------------------
#
# The point of the demo is a *phone*. `http://localhost:18470` is correct from the workstation and
# useless from anything else, and typing the wrong one into a phone at night produces a connection
# error that looks exactly like a broken deployment. So the URL is built from the address the
# workstation reaches the network on. It is a guess — a machine with a VPN, a container bridge and
# a wired interface has several plausible answers — which is why `--address` exists and why the
# script prints what it chose rather than only the QR.
#
# ---------------------------------------------------------------------------------------------
# Two QR codes, and why the token gets one
# ---------------------------------------------------------------------------------------------
#
# The PWA takes its token by hand: `frontend/src/store/token.ts` reads it from local storage and
# the connect screen asks for it, and there is deliberately no `?token=` handoff — a token in a
# URL is a token in every access log and browser history between here and the phone. The cost is
# that somebody has to get 32 base64 characters into a text field outdoors in the dark, which
# token.ts itself calls out as not a thing an operator can do. A second QR is the way out: any
# scanner will read it as text, and the phone can paste it.
#
# Both are also printed as plain text. The QR encoder is `scripts/lib/qr.py` — self-tested, but the
# one thing a self-test cannot prove is that a camera reads it, so the demo never depends on it.

set -uo pipefail

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib/harness.sh"

BUILD_FLAG=""
PORT=""
ADDRESS=""

while [[ $# -gt 0 ]]; do
    case "$1" in
    --no-build)
        BUILD_FLAG="--no-build"
        shift
        ;;
    --port)
        PORT="${2:-}"
        [[ "$PORT" =~ ^[0-9]+$ ]] || {
            echo "demo-m1: --port needs a number" >&2
            exit 2
        }
        shift 2
        ;;
    --address)
        ADDRESS="${2:-}"
        [[ -n "$ADDRESS" ]] || {
            echo "demo-m1: --address needs a value" >&2
            exit 2
        }
        shift 2
        ;;
    -h | --help)
        sed -n '3,13p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "demo-m1: unknown argument '$1'" >&2
        exit 2
        ;;
    esac
done

harness_init || exit 2

# ---------------------------------------------------------------------------------------
# The pair.
# ---------------------------------------------------------------------------------------
# shellcheck disable=SC2086  # BUILD_FLAG is deliberately unquoted: empty means "no flag"
"$HARNESS_ROOT/scripts/dev-up.sh" $BUILD_FLAG || {
    echo "demo-m1: the pair did not come up" >&2
    exit 1
}

TOKEN="${ASTROCTL_TOKEN:-}"
if [[ -z "$TOKEN" && -f "$HARNESS_ROOT/deploy/.env" ]]; then
    TOKEN="$(sed -n 's/^ASTROCTL_TOKEN=//p' "$HARNESS_ROOT/deploy/.env" | head -n1)"
fi
[[ -n "$TOKEN" ]] || {
    echo "demo-m1: no token — dev-up.sh should have written one to deploy/.env" >&2
    exit 1
}

if [[ -z "$PORT" ]]; then
    PUBLISHED="$(harness_compose port field 8470 2>/dev/null | tail -n1)"
    PORT="${PUBLISHED##*:}"
    [[ "$PORT" =~ ^[0-9]+$ ]] || PORT="${ASTROCTL_FIELD_HOST_PORT:-18470}"
fi

if [[ -z "$ADDRESS" ]]; then
    # The source address of the default route: what this machine looks like to the rest of the
    # network. `ip route get` asks the kernel rather than parsing `ifconfig`, and the destination
    # is never contacted — only routed.
    ADDRESS="$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{ for (i = 1; i < NF; i++) if ($i == "src") { print $(i + 1); exit } }')"
    [[ -n "$ADDRESS" ]] || ADDRESS="$(hostname -I 2>/dev/null | awk '{print $1}')"
    [[ -n "$ADDRESS" ]] || ADDRESS="localhost"
fi

URL="http://$ADDRESS:$PORT/"

# ---------------------------------------------------------------------------------------
# The codes.
# ---------------------------------------------------------------------------------------
qr() { # qr <text>
    python3 "$HARNESS_ROOT/scripts/lib/qr.py" "$1" 2>/dev/null
}

QR_OK=0
if command -v python3 >/dev/null 2>&1 &&
    python3 "$HARNESS_ROOT/scripts/lib/qr.py" --self-test >/dev/null 2>&1; then
    QR_OK=1
fi

echo
echo "============================================================"
echo "  AstroCtl — M1 walking skeleton"
echo "============================================================"
echo
echo "  Open this on the phone:"
echo
echo "      $URL"
echo
if [[ "$QR_OK" -eq 1 ]]; then
    qr "$URL"
    echo
fi
echo "  Then paste this token when the app asks:"
echo
echo "      $TOKEN"
echo
if [[ "$QR_OK" -eq 1 ]]; then
    qr "$TOKEN"
    echo
else
    echo "  (scripts/lib/qr.py did not pass its self-test on this machine, so no QR codes;"
    echo "   the URL and token above are all the demo needs.)"
    echo
fi

# ---------------------------------------------------------------------------------------
# The walkthrough — IMP §2/M1's exit narrative, in the order it is performed.
# ---------------------------------------------------------------------------------------
cat <<'WALKTHROUGH'
------------------------------------------------------------
  The demo, in order

  1. CONNECT     Tap "Connect" for the mount, then the camera. The pointing
                 readout starts moving at 1 Hz and the camera reports its
                 battery and card. Nothing connected at startup, deliberately:
                 switching a field node on must not produce motion.

  2. GOTO        Enter a target and slew. Watch the status go idle → slewing →
                 idle, and the readout track the tube the whole way. Pick a
                 declination above +45° if the site is northern — the node
                 refuses a target below 15° altitude, which is the safety limit
                 working and looks like a bug if it is a surprise.

  3. CAPTURE     Take a frame. The strip shows exposing → downloading → saved.
                 The frame is on the field node's disk at "saved", before
                 anything is sent anywhere.

  4. STACK       Switch the image surface to STACK. The frame uploads to the
                 stacking server, is acknowledged, and the preview comes back
                 through the field node's proxy — one image surface, two
                 sources. Ten seconds end to end is the budget; about five is
                 what it takes.

  5. PULL THE PLUG   docker compose -f deploy/compose.yaml stop stack
                 Keep capturing. The queue depth climbs, the stack panel says
                 offline, and exactly one alert appears — not one per retry.
                 Then start it again and watch the queue drain and every frame
                 get acknowledged.

  The suite that asserts all of the above, every time:  scripts/e2e.sh
  The full script, with what to say and what to watch:
      docs/plan/tasks/M1-walking-skeleton/DEMO.md
------------------------------------------------------------
WALKTHROUGH

echo
echo "  Stop the demo with:  scripts/dev-down.sh"
echo
