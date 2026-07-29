#!/usr/bin/env bash
#
# dev-down.sh — stop the M0-T08 two-node harness.
#
# Usage:  scripts/dev-down.sh [--volumes]
# Exit:   0 = down, 2 = the script could not run
#
#   --volumes  also delete /data/astro on both nodes.
#
# The volumes survive by default, and that is the whole decision in this file. They are what makes
# restart-recovery testable: bring the pair down, bring it back up, and the sessions, the transfer
# queue and the event log are still there — which is the property REL-06 and REL-08 are about. A
# `down` that wiped them would make every restart a first run, and nothing about durability would
# ever be exercised on a developer machine.
#
# Any shaping applied by scripts/shape-link.sh goes with the containers: `tc` qdiscs live in the
# container's network namespace, which is destroyed here. There is nothing to clean up on the host.

set -euo pipefail

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib/harness.sh"

VOLUMES=0
case "${1:-}" in
"") ;;
--volumes)
    VOLUMES=1
    ;;
-h | --help)
    sed -n '3,17p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
*)
    echo "dev-down: unknown argument '$1'" >&2
    exit 2
    ;;
esac

harness_init || exit 2

if [[ "$VOLUMES" -eq 1 ]]; then
    echo "-- stopping the harness and deleting both /data/astro volumes"
    harness_compose down --volumes
else
    echo "-- stopping the harness (volumes kept; --volumes deletes them)"
    harness_compose down
fi

echo "OK: the harness is down."
