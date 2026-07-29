#!/usr/bin/env bash
#
# dev-up.sh — bring the M0-T08 two-node harness up and wait until both nodes answer.
#
# Usage:  scripts/dev-up.sh [--no-build] [--timeout <seconds>]
# Exit:   0 = both nodes are healthy, 1 = they did not get there, 2 = the script could not run
#
#   --no-build  start what is already built. The images are rebuilt by default because the usual
#               reason to run this is that the code changed, and a harness quietly running last
#               week's binary is worse than a slow one.
#   --timeout   how long to wait for the two nodes, in seconds (default 90).
#
# Deliberate decisions, so nobody has to re-derive them:
#
#   * **The token is generated if there is none.** Both nodes read the shared token from
#     ASTROCTL_TOKEN (SEC-02, PRD §8.1/§8.2 — one token, not two). With none set, the first run
#     ends in the SEC-01 startup refusal: a node with no token bound to a non-loopback address
#     refuses to start. That is exactly right and a terrible introduction, so this script writes
#     one to deploy/.env (mode 0600, git-ignored), which compose reads by itself. An
#     ASTROCTL_TOKEN already in the environment always wins, because that is compose's own
#     precedence and two rules would be one too many.
#
#   * **Readiness is decided over the published port, not by the container healthcheck.** The
#     healthcheck runs inside the network namespace; what has to work is the operator's path from
#     the workstation, and only a request from out here proves it.
#
#   * **The stacking server is waited for through /stack/\*** on the field node — never at its own
#     address, which is not published at all. That is the topology (ADR-07, STK-19) and it means
#     the proxy is exercised before this script has finished printing.
#
#   * It does not wait for the two in parallel. The field node is the one that must be up for the
#     system to be usable; if it is not, "the stacking server is also not answering" is noise.

set -euo pipefail

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib/harness.sh"

BUILD=1
TIMEOUT=90
while [[ $# -gt 0 ]]; do
    case "$1" in
    --no-build)
        BUILD=0
        shift
        ;;
    --timeout)
        TIMEOUT="${2:-}"
        [[ "$TIMEOUT" =~ ^[0-9]+$ ]] || {
            echo "dev-up: --timeout needs a number of seconds" >&2
            exit 2
        }
        shift 2
        ;;
    -h | --help)
        sed -n '3,11p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "dev-up: unknown argument '$1'" >&2
        exit 2
        ;;
    esac
done

command -v curl >/dev/null 2>&1 || {
    echo "dev-up: curl not found on PATH — it is how readiness is decided" >&2
    exit 2
}
harness_init || exit 2

echo "== astroctl two-node harness =="

# ---------------------------------------------------------------------------------------
# The shared token.
# ---------------------------------------------------------------------------------------
ENV_FILE="$HARNESS_ROOT/deploy/.env"
if [[ -n "${ASTROCTL_TOKEN:-}" ]]; then
    echo "-- token: from the environment"
elif [[ -f "$ENV_FILE" ]] && grep -q '^ASTROCTL_TOKEN=' "$ENV_FILE"; then
    ASTROCTL_TOKEN="$(sed -n 's/^ASTROCTL_TOKEN=//p' "$ENV_FILE" | head -n1)"
    echo "-- token: from deploy/.env"
else
    # 24 bytes of urandom, URL-safe. Not `openssl rand`: one fewer thing that has to be installed
    # for the harness to come up.
    ASTROCTL_TOKEN="$(head -c 24 /dev/urandom | base64 | tr -d '=\n' | tr '+/' '-_')"
    (
        umask 077
        printf '# Written by scripts/dev-up.sh. Git-ignored; not a production credential.\n' >"$ENV_FILE"
        printf 'ASTROCTL_TOKEN=%s\n' "$ASTROCTL_TOKEN" >>"$ENV_FILE"
    )
    echo "-- token: generated, written to deploy/.env (mode 0600)"
fi
export ASTROCTL_TOKEN

# ---------------------------------------------------------------------------------------
# Up.
# ---------------------------------------------------------------------------------------
if [[ "$BUILD" -eq 1 ]]; then
    echo "-- building both images"
    harness_compose build
fi
echo "-- starting field and stack"
harness_compose up -d

# The published port comes from compose rather than from a default repeated here: compose.yaml
# owns it, and a second copy of "18470" is a second thing to change.
PUBLISHED="$(harness_compose port field 8470 2>/dev/null | tail -n1)"
HOST_PORT="${PUBLISHED##*:}"
[[ "$HOST_PORT" =~ ^[0-9]+$ ]] || HOST_PORT="${ASTROCTL_FIELD_HOST_PORT:-18470}"
FIELD_URL="http://localhost:$HOST_PORT"

# ---------------------------------------------------------------------------------------
# Wait. Both nodes report `starting` before `ok` (SDD §8.1), so a 200 is not enough.
# ---------------------------------------------------------------------------------------
wait_for_ok() { # wait_for_ok <label> <url>
    local label="$1" url="$2" deadline=$((SECONDS + TIMEOUT)) body=""
    while [[ "$SECONDS" -lt "$deadline" ]]; do
        body="$(curl -fsS --max-time 5 -H "Authorization: Bearer $ASTROCTL_TOKEN" "$url" 2>/dev/null || true)"
        case "$body" in
        *'"status":"ok"'*)
            echo "-- $label: ok"
            return 0
            ;;
        esac
        sleep 1
    done
    echo "dev-up: $label did not report ok within ${TIMEOUT}s ($url)" >&2
    [[ -n "$body" ]] && echo "  last response: $body" >&2
    echo "  logs:  $(printf '%s ' "${HARNESS_COMPOSE[@]}")-f deploy/compose.yaml logs" >&2
    return 1
}

wait_for_ok "field node" "$FIELD_URL/api/system/health" || exit 1
# Through the proxy, deliberately: this is the operator's only route to the stacking server, so it
# is the one worth waiting on. A stack node that is up but unreachable through /stack/* is not up
# as far as anything in this system is concerned.
wait_for_ok "stacking server (through /stack/*)" "$FIELD_URL/stack/api/system/health" || exit 1

echo
echo "OK: the field node is at $FIELD_URL"
echo "    token:  $ASTROCTL_TOKEN"
echo "    the PWA prompts for it; the stacking server is at $FIELD_URL/stack/*"
