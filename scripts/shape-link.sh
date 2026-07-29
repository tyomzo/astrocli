#!/usr/bin/env bash
#
# shape-link.sh — constrain the field↔stack link of the M0-T08 harness with `tc`.
#
# Usage:  scripts/shape-link.sh [--latency <time>] [--loss <pct>] [--burst <size>]
#                               [--queue <time>] <bandwidth>
#         scripts/shape-link.sh off
#         scripts/shape-link.sh status
#         scripts/shape-link.sh measure [MiB]
# Exit:   0 = done, 1 = the shaping or the measurement failed, 2 = the script could not run
#
#   scripts/shape-link.sh 1mbit                     the T-HOL-1 link (SDD §9, T-HOL-1)
#   scripts/shape-link.sh --latency 80ms --loss 1% 2mbit    a bad night on a phone tether
#   scripts/shape-link.sh --latency 300ms 5mbit     a satellite-ish link, no rate ceiling worth
#                                                   worrying about but a punishing RTT
#
# This is why the two nodes are in containers. The requirements that are hardest to believe on a
# workstation are the ones about a *slow, shared* link — T-HOL-1 ("saturate /ws/liveview over a
# shaped 1 Mbit link; assert /ws position events and e-stop latency are unaffected"), the transfer
# pacing of PRD §8.1, the degraded-mode behaviour of ADD §5.4.4. Shaping loopback is awkward and
# unconvincing; shaping a veth pair between two network namespaces is neither.
#
# ---------------------------------------------------------------------------------------------
# Where the capability lives, and why it is not on the services
# ---------------------------------------------------------------------------------------------
#
# `tc` needs CAP_NET_ADMIN in the namespace it is configuring. Three ways to arrange that:
#
#   1. Shape the host-side veth from the host. Needs root on the workstation. A harness every
#      developer is expected to run must not require sudo, so this is out — and it is the option
#      M0-T08 asks to prefer, so it is worth saying plainly that it was rejected for that reason
#      and not overlooked.
#   2. Give the field and stack containers NET_ADMIN. Simple, and wrong: they would then run with
#      a capability their systemd units will not have, so the harness would stop resembling the
#      deployment in the direction that matters — a service that can rewrite its own routing table
#      is a service whose failures do not mean what they mean in production.
#   3. Run `tc` in a **throwaway sidecar** that joins the target container's network namespace and
#      holds NET_ADMIN for the second and a half it lives. The services stay unprivileged, the
#      host stays untouched, and nothing persists.
#
# This script does (3). The sidecar runs the service's own image, so the harness needs no second
# image and no network to pull one; `iproute2` and `netcat-openbsd` are in those images for
# exactly this (see Dockerfile.field).
#
# ---------------------------------------------------------------------------------------------
# What gets shaped
# ---------------------------------------------------------------------------------------------
#
# Egress on each container's eth0, filtered to the *other node's address*:
#
#     prio 1:                      root, three bands, ordinary traffic through band 1 untouched
#      └─ 1:3  netem  (delay/loss) the shaped band, reached only via the filter below
#          └─  tbf    (rate)
#     filter: ip dst <peer>  →  1:3
#
# The filter is the point. A root qdisc with no filter would also throttle the operator's own
# traffic — the PWA is served over the same eth0 through the published port — and a "1 Mbit link"
# that also makes the UI crawl would prove the opposite of what T-HOL-1 asks. The two nodes are
# shaped symmetrically, so both the frame upload (field→stack) and the preview push (stack→field)
# meet the same link.
#
# `tc` has no ingress shaping worth the name without an ifb device, which is why this is done as
# egress on both sides rather than ingress on one.
#
# The qdiscs live in the containers' network namespaces and die with them: `dev-down.sh` needs no
# knowledge of this script, and a crashed harness leaves nothing behind on the workstation.

set -euo pipefail

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib/harness.sh"

# The interface inside the container. eth0 under both docker and podman bridge networking;
# overridable rather than assumed, because "it is always eth0" is the kind of thing that is true
# until somebody adds a second network.
IFACE="${ASTROCTL_LINK_IFACE:-eth0}"
# Port for the throughput probe. Inside the containers' namespaces, never published.
PROBE_PORT=19999

LATENCY=""
LOSS=""
# tbf needs a burst of at least rate/HZ or it silently fails to reach the configured rate. 32 kbit
# (4 kB) covers everything up to ~30 Mbit with HZ=1000; raise it with --burst above that.
BURST="32kbit"
# How much the shaped band may queue, expressed as time. This is the knob T-HOL-1 actually cares
# about: head-of-line blocking is a queueing phenomenon, and a link with no queue cannot exhibit it.
QUEUE="400ms"
BANDWIDTH=""
COMMAND="apply"
MEASURE_MIB=1

usage() { sed -n '3,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
    case "$1" in
    --latency)
        LATENCY="${2:-}"
        shift 2
        ;;
    --loss)
        LOSS="${2:-}"
        shift 2
        ;;
    --burst)
        BURST="${2:-}"
        shift 2
        ;;
    --queue)
        QUEUE="${2:-}"
        shift 2
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    off | clear | remove)
        COMMAND="off"
        shift
        ;;
    status)
        COMMAND="status"
        shift
        ;;
    measure)
        COMMAND="measure"
        shift
        if [[ $# -gt 0 ]]; then
            MEASURE_MIB="$1"
            shift
        fi
        ;;
    -*)
        echo "shape-link: unknown option '$1'" >&2
        usage >&2
        exit 2
        ;;
    *)
        BANDWIDTH="$1"
        shift
        ;;
    esac
done

if [[ "$COMMAND" == "apply" && -z "$BANDWIDTH" && -z "$LATENCY" && -z "$LOSS" ]]; then
    echo "shape-link: nothing to apply — give a bandwidth, --latency or --loss" >&2
    usage >&2
    exit 2
fi
[[ "$MEASURE_MIB" =~ ^[0-9]+$ ]] || {
    echo "shape-link: measure takes a whole number of MiB" >&2
    exit 2
}

harness_init || exit 2

# ---------------------------------------------------------------------------------------
# Both containers, their addresses and their images.
# ---------------------------------------------------------------------------------------
FIELD_ID="$(harness_container field)"
STACK_ID="$(harness_container stack)"
for pair in "field:$FIELD_ID" "stack:$STACK_ID"; do
    [[ -n "${pair#*:}" ]] || {
        echo "shape-link: the ${pair%%:*} container is not running — scripts/dev-up.sh first" >&2
        exit 2
    }
done
FIELD_IP="$(harness_address "$FIELD_ID")"
STACK_IP="$(harness_address "$STACK_ID")"
FIELD_IMAGE="$(harness_image "$FIELD_ID")"
STACK_IMAGE="$(harness_image "$STACK_ID")"
[[ -n "$FIELD_IP" && -n "$STACK_IP" ]] || {
    echo "shape-link: cannot read the containers' addresses from $HARNESS_ENGINE inspect" >&2
    exit 2
}

# Run a shell in a container's network namespace. The `--cap-add` is on this throwaway container
# and never on the service; see the header.
in_netns() { # in_netns <container-id> <image> <privileged:0|1> <script>
    local args=(run --rm --network "container:$1" --user 0 --entrypoint /bin/sh)
    [[ "$3" == "1" ]] && args+=(--cap-add NET_ADMIN)
    "$HARNESS_ENGINE" "${args[@]}" "$2" -c "$4"
}

# The qdisc chain for one direction, as a script for the sidecar's shell.
tc_apply_script() { # tc_apply_script <peer-address>
    local peer="$1" parent="1:3" handle="30:"
    local script="set -e
tc qdisc del dev $IFACE root 2>/dev/null || true
tc qdisc add dev $IFACE root handle 1: prio
"
    if [[ -n "$LATENCY" || -n "$LOSS" ]]; then
        script+="tc qdisc add dev $IFACE parent $parent handle $handle netem"
        [[ -n "$LATENCY" ]] && script+=" delay $LATENCY"
        [[ -n "$LOSS" ]] && script+=" loss $LOSS"
        script+="
"
        # tbf hangs off netem's single child class.
        parent="30:1"
        handle="31:"
    fi
    if [[ -n "$BANDWIDTH" ]]; then
        script+="tc qdisc add dev $IFACE parent $parent handle $handle tbf rate $BANDWIDTH burst $BURST latency $QUEUE
"
    fi
    # Everything not addressed to the peer keeps the default band and is untouched — including the
    # operator's traffic to the published port.
    script+="tc filter add dev $IFACE protocol ip parent 1:0 prio 1 u32 match ip dst $peer/32 flowid 1:3
"
    printf '%s' "$script"
}

case "$COMMAND" in
# ---------------------------------------------------------------------------------------
apply)
    echo "== shaping the field↔stack link =="
    echo "-- bandwidth ${BANDWIDTH:-uncapped}, latency ${LATENCY:-none}, loss ${LOSS:-none}"
    in_netns "$FIELD_ID" "$FIELD_IMAGE" 1 "$(tc_apply_script "$STACK_IP")"
    echo "-- field  → stack ($STACK_IP) shaped"
    in_netns "$STACK_ID" "$STACK_IMAGE" 1 "$(tc_apply_script "$FIELD_IP")"
    echo "-- stack  → field ($FIELD_IP) shaped"
    echo
    echo "OK: both directions shaped. 'scripts/shape-link.sh measure' to see it,"
    echo "    'scripts/shape-link.sh off' to remove it."
    ;;

# ---------------------------------------------------------------------------------------
off)
    echo "== removing the shaping =="
    # Deleting the root qdisc takes the whole tree, filters included. Absence is not an error:
    # `off` has to be safe to run when nothing was ever applied.
    for pair in "field:$FIELD_ID:$FIELD_IMAGE" "stack:$STACK_ID:$STACK_IMAGE"; do
        IFS=: read -r name id image <<<"$pair"
        in_netns "$id" "$image" 1 "tc qdisc del dev $IFACE root 2>/dev/null || true"
        echo "-- $name: unshaped"
    done
    echo "OK: the link is back to whatever the bridge does on its own."
    ;;

# ---------------------------------------------------------------------------------------
status)
    for pair in "field:$FIELD_ID:$FIELD_IMAGE" "stack:$STACK_ID:$STACK_IMAGE"; do
        IFS=: read -r name id image <<<"$pair"
        echo "== $name ($IFACE) =="
        in_netns "$id" "$image" 0 "tc -s qdisc show dev $IFACE; echo; tc filter show dev $IFACE"
    done
    ;;

# ---------------------------------------------------------------------------------------
# The measurement. Bytes flow field→stack, which is the direction M1's transfer agent uploads in
# and therefore the one T-HOL-1 is about.
#
# The timing is curl's, on the receiving side, and that is deliberate: timing the *sender* would
# measure how fast bytes reach the kernel's socket buffer, which on a shaped link is nothing like
# how fast they reach the other node — the sender finishes with megabytes still queued in tbf. The
# server is `nc` reading a canned HTTP response from a pipe, so the only thing between the two
# measurements is the link.
measure)
    BYTES=$((MEASURE_MIB * 1024 * 1024))
    echo "== field → stack, $MEASURE_MIB MiB =="

    # `nc -N` closes the socket when the pipe is exhausted, so curl sees a clean end of body.
    listener="$(
        "$HARNESS_ENGINE" run -d --network "container:$FIELD_ID" --user 0 \
            --entrypoint /bin/sh "$FIELD_IMAGE" -c \
            "{ printf 'HTTP/1.0 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: $BYTES\r\n\r\n'; head -c $BYTES /dev/zero; } | nc -N -l $PROBE_PORT >/dev/null"
    )"
    # shellcheck disable=SC2064  # the id is wanted now, not when the trap fires
    trap "$HARNESS_ENGINE rm -f '$listener' >/dev/null 2>&1 || true" EXIT

    # --retry-connrefused covers the race between the sidecar starting and nc binding; no bytes
    # have moved at that point, so a retry costs nothing but the round trip.
    result="$(
        in_netns "$STACK_ID" "$STACK_IMAGE" 0 \
            "curl -sS -o /dev/null --retry 10 --retry-connrefused --retry-delay 1 --max-time 600 -w '%{size_download} %{time_total} %{speed_download}' http://$FIELD_IP:$PROBE_PORT/"
    )" || {
        echo "shape-link: the probe failed — is the link shaped so hard it timed out?" >&2
        exit 1
    }

    read -r size seconds speed <<<"$result"
    [[ "${size:-0}" == "$BYTES" ]] || {
        echo "shape-link: the probe transferred $size bytes, expected $BYTES" >&2
        exit 1
    }
    mbit="$(awk -v s="$speed" 'BEGIN { printf "%.2f", s * 8 / 1000000 }')"
    printf -- "-- %s bytes in %s s\n" "$size" "$seconds"
    printf -- "-- %s Mbit/s\n" "$mbit"
    ;;
esac
