#!/usr/bin/env bash
#
# check-deps.sh — enforce the crate dependency matrix of ADD §5.6.
#
# The rules in ADD §5.6 are what keeps the HAL an actual abstraction boundary rather than a
# suggestion. They are unenforceable by review alone: a forbidden edge is one line in a
# Cargo.toml and it compiles fine. This script reads the real graph out of `cargo metadata`
# and fails the build on any edge the architecture forbids.
#
# Usage:  scripts/check-deps.sh [--verbose]
# Exit:   0 = clean, 1 = violations found, 2 = the script could not run
#
# Deliberate design decisions, so nobody has to re-derive them:
#
#   * It reads `cargo metadata --no-deps`, i.e. *declared* dependencies of workspace members.
#     The architecture rules are about which crates a crate is allowed to name, so declared
#     edges are exactly the right unit — and --no-deps keeps third-party noise out.
#
#   * dev- and build-dependencies count. A dev-dependency does not ship, but it still means
#     the crate's tests reach across a boundary the architecture says is closed, and it is
#     the obvious way to smuggle one in. If a future task has a genuine need for such an
#     edge, that is a documented deviation to argue for — not something this script should
#     wave through silently.
#
#   * Only direct edges are checked. The rule set covers every crate, so composition does
#     the transitive work: drivers may reach only hal+core, hal may reach only core, core
#     may reach nothing. There is no path for a forbidden crate to arrive indirectly.
#
# Do not add exceptions here to make a build pass (tasks/README.md rule 3). If a rule is
# wrong, fix the ADD and this script together.

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
VERBOSE=0
[[ "${1:-}" == "--verbose" || "${1:-}" == "-v" ]] && VERBOSE=1

command -v cargo >/dev/null 2>&1 || { echo "check-deps: cargo not found on PATH" >&2; exit 2; }
command -v jq >/dev/null 2>&1 || {
    echo "check-deps: jq not found on PATH (apt install jq)" >&2
    exit 2
}

# ---------------------------------------------------------------------------------------
# The expected workspace membership. ADD §5.6 lists exactly these 14 crates. Checking the
# set — not just the edges — catches a crate silently dropping out of `members`, and catches
# spikes/ leaking back in after someone removes the `exclude`.
# ---------------------------------------------------------------------------------------
EXPECTED_MEMBERS=(
    astroctl-core
    astroctl-hal
    astroctl-drivers
    astroctl-safety
    astroctl-session
    astroctl-pipeline
    astroctl-solver
    astroctl-planning
    astroctl-guiding
    astroctl-transfer
    astroctl-llm
    astroctl-ipc
    astroctl-field
    astroctl-stack
)

# The two deployables. "Above the HAL" and "domain crates never depend on the binaries" both
# key off this list.
BINARIES=(astroctl-field astroctl-stack)

# ADD §5.6 rule 6 — the field binary must build with no GPU or worker-related code path.
# Crate-level approximation: none of these may appear among its direct dependencies. It is an
# approximation on purpose; the authoritative check is that workers/ is packaged with the
# stacking service only.
FIELD_FORBIDDEN_EXTERNAL=(cust cudarc rustacuda ocl opencl3 tch ort onnxruntime)

# ---------------------------------------------------------------------------------------
# Collect the graph.
# ---------------------------------------------------------------------------------------
if ! META="$(cargo metadata --format-version 1 --no-deps --manifest-path "$ROOT/Cargo.toml" 2>&1)"; then
    echo "check-deps: cargo metadata failed — the workspace does not load:" >&2
    echo "$META" >&2
    exit 2
fi

MEMBERS="$(jq -r '.packages[].name' <<<"$META" | sort)"

# "<from>\t<to>\t<kind>" for every declared dependency on another workspace crate.
INTERNAL_EDGES="$(
    jq -r '
      .packages[] as $p
      | $p.dependencies[]
      | select(.name | startswith("astroctl-"))
      | "\($p.name)\t\(.name)\t\(.kind // "normal")"
    ' <<<"$META" | sort -u
)"

VIOLATIONS=0
violation() {
    VIOLATIONS=$((VIOLATIONS + 1))
    printf '  [%s] %s\n' "$1" "$2" >&2
}

contains() { # contains <needle> <haystack...>
    local needle="$1"
    shift
    local x
    for x in "$@"; do [[ "$x" == "$needle" ]] && return 0; done
    return 1
}

edges_from() { # every "to<TAB>kind" declared by <crate>
    [[ -z "$INTERNAL_EDGES" ]] && return 0
    awk -F'\t' -v c="$1" '$1 == c { print $2 "\t" $3 }' <<<"$INTERNAL_EDGES"
}

edges_to() { # every "from<TAB>kind" that names <crate>
    [[ -z "$INTERNAL_EDGES" ]] && return 0
    awk -F'\t' -v c="$1" '$2 == c { print $1 "\t" $3 }' <<<"$INTERNAL_EDGES"
}

kindsuffix() { [[ "$1" == "normal" ]] && echo "" || echo " ($1-dependency)"; }

# ---------------------------------------------------------------------------------------
# Rule primitives.
# ---------------------------------------------------------------------------------------

# allow_only <rule-id> <crate> [permitted...] — the crate's internal out-edges must all be
# in the permitted set.
allow_only() {
    local id="$1" crate="$2"
    shift 2
    local to kind
    while IFS=$'\t' read -r to kind; do
        [[ -z "$to" ]] && continue
        contains "$to" "$@" || violation "$id" \
            "$crate depends on $to$(kindsuffix "$kind") — permitted: ${*:-<nothing internal>}"
    done < <(edges_from "$crate")
}

# forbid <rule-id> <crate> [forbidden...] — the crate must not name any of these.
forbid() {
    local id="$1" crate="$2"
    shift 2
    local to kind
    while IFS=$'\t' read -r to kind; do
        [[ -z "$to" ]] && continue
        if contains "$to" "$@"; then
            violation "$id" "$crate must not depend on $to$(kindsuffix "$kind")"
        fi
    done < <(edges_from "$crate")
}

# depended_on_only_by <rule-id> <crate> [permitted dependents...]
depended_on_only_by() {
    local id="$1" crate="$2"
    shift 2
    local from kind who="no crate at all may"
    [[ "$#" -gt 0 ]] && who="only $* may"
    while IFS=$'\t' read -r from kind; do
        [[ -z "$from" ]] && continue
        contains "$from" "$@" || violation "$id" \
            "$from depends on $crate$(kindsuffix "$kind") — $who"
    done < <(edges_to "$crate")
}

# ---------------------------------------------------------------------------------------
# Checks.
# ---------------------------------------------------------------------------------------
echo "== astroctl dependency lint (ADD §5.6) =="

# --- membership -------------------------------------------------------------------------
echo "-- workspace membership"
expected_sorted="$(printf '%s\n' "${EXPECTED_MEMBERS[@]}" | sort)"
if [[ "$MEMBERS" != "$expected_sorted" ]]; then
    while read -r m; do
        [[ -z "$m" ]] && continue
        contains "$m" "${EXPECTED_MEMBERS[@]}" ||
            violation "MEMBERS" "unexpected workspace member '$m' (ADD §5.6 lists 14 crates)"
    done <<<"$MEMBERS"
    for m in "${EXPECTED_MEMBERS[@]}"; do
        grep -qxF "$m" <<<"$MEMBERS" ||
            violation "MEMBERS" "missing workspace member '$m' (ADD §5.6)"
    done
fi

# spikes/ is throwaway hardware-spike code with its own [workspace]; it must never be built
# by `cargo build --workspace`.
if [[ -d "$ROOT/spikes" ]]; then
    if ! grep -qE '^\s*exclude\s*=.*"spikes"' "$ROOT/Cargo.toml"; then
        violation "MEMBERS" "spikes/ exists but the root manifest does not exclude it"
    fi
fi

# --- ADD §5.6 dependency rules ------------------------------------------------------------
echo "-- dependency rules"

# Layering implied by the §5.6 layout: core is the foundation everything shares, and the HAL
# sits directly on it. Cargo rejects cycles, but it happily allows core -> anything, which
# would invert the architecture without any build error.
allow_only "LAYER-core" astroctl-core
allow_only "LAYER-hal" astroctl-hal astroctl-core

# Rule 1 — "astroctl-drivers depends only on astroctl-hal + astroctl-core. Nothing above the
# HAL depends on a concrete driver except the registry."
#
# Reading of the second sentence: the `DriverRegistry` *type* lives in astroctl-hal (SDD
# §5.1), and hal cannot depend on drivers without a cycle, so the crate that names concrete
# drivers is whichever one populates the registry — i.e. a deployable. Every domain crate
# reaches devices through `dyn MountDevice`/`dyn Camera`; SafeMount holding
# `Arc<dyn MountDevice>` (SDD §5.4) is the pattern.
allow_only "ADD-5.6-R1a" astroctl-drivers astroctl-core astroctl-hal
depended_on_only_by "ADD-5.6-R1b" astroctl-drivers "${BINARIES[@]}"

# Rule 2 — "domain crates never depend on the binaries — they emit events instead."
for bin in "${BINARIES[@]}"; do
    depended_on_only_by "ADD-5.6-R2" "$bin"
done

# Rule 3 — "astroctl-llm reaches the system only through HTTP calls to the local API; it must
# not depend on astroctl-session or astroctl-hal."
forbid "ADD-5.6-R3" astroctl-llm astroctl-session astroctl-hal

# Rule 5 — "astroctl-field and astroctl-stack never depend on each other."
forbid "ADD-5.6-R5" astroctl-field astroctl-stack
forbid "ADD-5.6-R5" astroctl-stack astroctl-field

# Rule 6 — the field binary carries no GPU/worker code paths. Direct dependencies only.
while IFS=$'\t' read -r dep kind; do
    [[ -z "$dep" ]] && continue
    if contains "$dep" "${FIELD_FORBIDDEN_EXTERNAL[@]}"; then
        violation "ADD-5.6-R6" \
            "astroctl-field depends on GPU/ML runtime crate '$dep'$(kindsuffix "$kind") — those belong to workers/ on the stacking server"
    fi
done < <(jq -r '
      .packages[] | select(.name == "astroctl-field")
      | .dependencies[] | "\(.name)\t\(.kind // "normal")"
    ' <<<"$META")

# --- metadata hygiene ---------------------------------------------------------------------
# Not a dependency rule, but the same class of invariant: cheap to assert, silently wrong
# otherwise. Every crate inherits license from [workspace.package].
echo "-- metadata hygiene"
while IFS=$'\t' read -r name license; do
    [[ "$license" == "MIT" ]] ||
        violation "LICENSE" "$name reports license '$license', expected MIT (inherit with license.workspace = true)"
done < <(jq -r '.packages[] | "\(.name)\t\(.license // "<none>")"' <<<"$META")

# ---------------------------------------------------------------------------------------
if [[ "$VERBOSE" -eq 1 ]]; then
    echo "-- internal edges considered"
    if [[ -z "$INTERNAL_EDGES" ]]; then
        echo "  (none)"
    else
        awk -F'\t' '{ printf "  %s -> %s (%s)\n", $1, $2, $3 }' <<<"$INTERNAL_EDGES"
    fi
fi

if [[ "$VIOLATIONS" -gt 0 ]]; then
    echo
    echo "FAIL: $VIOLATIONS dependency-rule violation(s). See ADD §5.6." >&2
    exit 1
fi

edge_count=0
[[ -n "$INTERNAL_EDGES" ]] && edge_count="$(wc -l <<<"$INTERNAL_EDGES" | tr -d ' ')"
echo "OK: $(wc -l <<<"$MEMBERS" | tr -d ' ') crates, $edge_count internal edges, 0 violations."
