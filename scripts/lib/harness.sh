# harness.sh — what dev-up.sh, dev-down.sh and shape-link.sh all have to agree about.
#
# Sourced, never executed:  source "$(dirname -- "${BASH_SOURCE[0]}")/lib/harness.sh"
#
# There are only about thirty lines here, and they are in one file rather than copied into three
# because of what a disagreement between the copies would look like from the outside: shape-link.sh
# resolving `podman compose` while dev-up.sh resolved `docker compose` does not fail, it reports
# that the containers are not running — and then somebody spends an hour on the shaping code.
#
# Everything here is about *addressing* the harness: which compose, which engine, which container,
# which address. Nothing here decides policy; that belongs in the script the reader opened.

# Where the repository is, derived from this file rather than from the caller, so it holds however
# the caller was invoked.
HARNESS_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
HARNESS_COMPOSE_FILE="$HARNESS_ROOT/deploy/compose.yaml"
# Prefix for this library's own diagnostics; the caller's name is what the reader recognises.
HARNESS_NAME="$(basename -- "$0")"

# Resolve the container engine and compose implementation. Call once, early; returns 2 (the
# repository's "the script could not run" code) if there is nothing to resolve.
#
# Compose v2 is a docker subcommand, and podman's is a subcommand in some distributions and a
# separate binary in others, so all three forms are tried. ASTROCTL_COMPOSE overrides the lot for
# anyone whose setup this does not anticipate:  ASTROCTL_COMPOSE=podman-compose scripts/dev-up.sh
harness_init() {
    if [[ -n "${ASTROCTL_COMPOSE:-}" ]]; then
        read -r -a HARNESS_COMPOSE <<<"$ASTROCTL_COMPOSE"
    elif docker compose version >/dev/null 2>&1; then
        HARNESS_COMPOSE=(docker compose)
    elif podman compose version >/dev/null 2>&1; then
        HARNESS_COMPOSE=(podman compose)
    elif command -v podman-compose >/dev/null 2>&1; then
        HARNESS_COMPOSE=(podman-compose)
    else
        echo "$HARNESS_NAME: no compose implementation found (tried 'docker compose'," >&2
        echo "  'podman compose', 'podman-compose'). Set ASTROCTL_COMPOSE to override." >&2
        return 2
    fi

    # The engine is what `run` and `inspect` go to. podman-compose drives podman.
    case "${HARNESS_COMPOSE[0]}" in
    podman*) HARNESS_ENGINE=podman ;;
    *) HARNESS_ENGINE="${HARNESS_COMPOSE[0]}" ;;
    esac

    [[ -f "$HARNESS_COMPOSE_FILE" ]] || {
        echo "$HARNESS_NAME: $HARNESS_COMPOSE_FILE does not exist" >&2
        return 2
    }
}

# Run compose against this repository's harness.
harness_compose() {
    "${HARNESS_COMPOSE[@]}" -f "$HARNESS_COMPOSE_FILE" "$@"
}

# The container id of a service, empty if it is not running. Resolved through compose rather than
# by assuming a container name: the project name comes from compose.yaml under docker and from the
# directory under some other implementations, so the name is not ours to predict.
harness_container() {
    harness_compose ps -q "$1" 2>/dev/null | head -n1
}

# The address a container holds on the harness network, and the image it was started from. Both
# are for shape-link.sh: `tc` filters name the peer by address, because DNS is the application's
# business and not the shaper's, and the sidecar runs the service's own image so the harness needs
# no second image to pull.
harness_address() {
    "$HARNESS_ENGINE" inspect -f \
        '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$1" 2>/dev/null
}

harness_image() {
    "$HARNESS_ENGINE" inspect -f '{{.Config.Image}}' "$1" 2>/dev/null
}
