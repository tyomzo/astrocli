#!/usr/bin/env bash
#
# check.sh — the quality gate. What CI runs, runnable offline on any dev machine.
#
# This script is the deliverable; `.github/workflows/ci.yml` is a wrapper that installs a
# toolchain and calls it. That direction matters: a workflow that listed the gates itself would
# be a second copy of them, and the copies would drift the first time someone added a check in
# a hurry. Here there is one list, and "green locally" and "green in CI" mean the same thing.
#
# Usage:  scripts/check.sh [--fast] [--list]
#           --fast   skip the frontend build (the slow stage) — for a tight edit loop, NOT for
#                    a pre-push hook and never for CI
#           --list   print the gates and exit
# Exit:   0 = every gate passed, 1 = a gate failed, 2 = the script could not run
#
# Gates run in ascending order of cost, so the fastest failure is the one you see first.
#
# As a pre-push hook:
#     ln -s ../../scripts/check.sh .git/hooks/pre-push

set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

FAST=0
for arg in "$@"; do
    case "$arg" in
        --fast) FAST=1 ;;
        --list)
            printf '%s\n' "fmt" "clippy" "deps" "async" "test" "frontend"
            exit 0
            ;;
        *)
            echo "check.sh: unknown argument \`$arg\`" >&2
            exit 2
            ;;
    esac
done

FAILED=()
PASSED=0

# Run one gate. Output is shown only on failure: a gate that prints on success trains people to
# ignore the output, and then a real failure scrolls past unread.
gate() {
    local name="$1"
    shift
    local start output status
    start=$SECONDS
    printf '  %-10s ' "$name"
    output=$("$@" 2>&1)
    status=$?
    if [[ $status -eq 0 ]]; then
        printf 'ok      %ds\n' "$((SECONDS - start))"
        PASSED=$((PASSED + 1))
    else
        printf 'FAILED  %ds\n' "$((SECONDS - start))"
        FAILED+=("$name")
        printf '%s\n' "$output" | sed 's/^/      | /'
    fi
}

# The end-to-end suite (tests/e2e) is not a workspace member — it needs Docker and two built
# images, and `cargo test --workspace` has to stay hermetic. That exclusion normally costs a crate
# its linting, as it does spikes/, and this one must not pay it: the e2e suite is the permanent
# health signal for every later milestone, so it is held to the same fmt and clippy bar as the
# product. Hence a second manifest on those two gates, and only those two.
E2E_MANIFEST="tests/e2e/Cargo.toml"

fmt() {
    cargo fmt --all --check || return 1
    [[ -f "$E2E_MANIFEST" ]] || return 0
    cargo fmt --manifest-path "$E2E_MANIFEST" --all --check
}

clippy() {
    cargo clippy --workspace --all-targets --quiet -- -D warnings || return 1
    [[ -f "$E2E_MANIFEST" ]] || return 0
    cargo clippy --manifest-path "$E2E_MANIFEST" --all-targets --quiet -- -D warnings
}

frontend() {
    # `npm ci`, not `npm install`: the lockfile is the pinned input, and a gate that silently
    # resolves a different tree than CI is not a gate.
    #
    # `npm test` runs before the build for the same reason the Rust gates are ordered by cost: the
    # unit tests take well under a second and fail on the things most likely to be wrong.
    ( cd frontend \
        && npm ci --silent \
        && npm test --silent \
        && npm run build --silent \
        && npx tsc --noEmit )
}

echo "AstroCtl quality gate"
echo

gate fmt      fmt
gate clippy   clippy
gate deps     ./scripts/check-deps.sh
gate async    ./scripts/check-async.sh
gate test     cargo test --workspace --quiet

if [[ $FAST -eq 1 ]]; then
    echo "  frontend   SKIPPED (--fast)"
else
    gate frontend frontend
fi

echo
if [[ ${#FAILED[@]} -gt 0 ]]; then
    echo "FAIL: ${#FAILED[@]} gate(s) failed — ${FAILED[*]}" >&2
    exit 1
fi

if [[ $FAST -eq 1 ]]; then
    echo "OK: $PASSED gates passed. The frontend gate was skipped — not a green tree."
else
    echo "OK: all $PASSED gates passed."
fi
