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

frontend() {
    # `npm ci`, not `npm install`: the lockfile is the pinned input, and a gate that silently
    # resolves a different tree than CI is not a gate.
    ( cd frontend && npm ci --silent && npm run build --silent && npx tsc --noEmit )
}

echo "AstroCtl quality gate"
echo

gate fmt      cargo fmt --all --check
gate clippy   cargo clippy --workspace --all-targets --quiet -- -D warnings
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
