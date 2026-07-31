#!/usr/bin/env bash
#
# e2e.sh — run the M1-T16 end-to-end suite against the M0-T08 container pair.
#
# Usage:  scripts/e2e.sh [--no-build] [--keep] [--repeat <n>] [--test <name>] [-- <cargo args>]
#         scripts/e2e.sh --list
# Exit:   0 = every run passed, 1 = a run failed, 2 = the script could not run
#
#   --no-build   attach to the pair that is already up. The images are rebuilt by default for the
#                reason dev-up.sh gives: the configs and the binaries are *baked in*, so a harness
#                quietly running last week's field node is worse than a slow one.
#   --keep       leave the pair up afterwards, for poking at a failure by hand.
#   --repeat N   run the suite N times and report the tally. This is the flake gate: M1-T16's
#                acceptance criterion is ×20 consecutive runs with zero flakes, and it is spelled
#                as a loop rather than as a retry on purpose — a retry hides the number this is
#                supposed to produce.
#   --test NAME  run one scenario file; --list prints the names rather than repeating them here,
#                where they would go stale the first time one is added.
#   --list       print the scenario files and exit.
#
# ---------------------------------------------------------------------------------------------
# Why a script rather than `cargo test`
# ---------------------------------------------------------------------------------------------
#
# Three things have to be true before the first assertion and none of them are cargo's job: two
# images have to exist and be current, the pair has to be up and answering, and the volumes have
# to be in a state the suite can reason about. This script owns all three, and the Rust half owns
# only what happens between them — which is why a scenario can stop a node mid-session without
# also having to know how to build one.
#
# ---------------------------------------------------------------------------------------------
# --test-threads=1, always
# ---------------------------------------------------------------------------------------------
#
# There is one container pair and the scenarios drive it destructively: one stops the stacking
# server, one restarts the field node, one puts a 1 Mbit ceiling on the link between them. Run two
# at once and the failures are unattributable. The suite also takes a cross-process lock so that a
# `cargo test` typed by hand in another terminal waits instead of interleaving, but the lock is the
# backstop; this flag is the mechanism.

set -uo pipefail

source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib/harness.sh"

BUILD=1
KEEP=0
REPEAT=1
ONLY=""
CARGO_EXTRA=()
MANIFEST="$HARNESS_ROOT/tests/e2e/Cargo.toml"

while [[ $# -gt 0 ]]; do
    case "$1" in
    --no-build)
        BUILD=0
        shift
        ;;
    --keep)
        KEEP=1
        shift
        ;;
    --repeat)
        REPEAT="${2:-}"
        [[ "$REPEAT" =~ ^[0-9]+$ ]] && [[ "$REPEAT" -gt 0 ]] || {
            echo "e2e: --repeat needs a positive number of runs" >&2
            exit 2
        }
        shift 2
        ;;
    --test)
        ONLY="${2:-}"
        [[ -n "$ONLY" ]] || {
            echo "e2e: --test needs a scenario name" >&2
            exit 2
        }
        shift 2
        ;;
    --list)
        find "$HARNESS_ROOT/tests/e2e/tests" -name '*.rs' -printf '%f\n' 2>/dev/null |
            sed 's/\.rs$//' | sort
        exit 0
        ;;
    --)
        shift
        CARGO_EXTRA=("$@")
        break
        ;;
    -h | --help)
        sed -n '3,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "e2e: unknown argument '$1'" >&2
        exit 2
        ;;
    esac
done

command -v cargo >/dev/null 2>&1 || {
    echo "e2e: cargo not found on PATH" >&2
    exit 2
}
harness_init || exit 2
[[ -f "$MANIFEST" ]] || {
    echo "e2e: $MANIFEST does not exist" >&2
    exit 2
}

echo "== astroctl end-to-end suite =="

# ---------------------------------------------------------------------------------------
# The pair.
#
# Volumes are wiped before the first run and *not* between repeats. Both halves are deliberate:
# the acceptance criterion is "green from a fresh clone", so a run must not depend on state a
# previous one left; and the repeats then exercise the opposite case — a session that has been
# running for twenty captures, with a growing frame list and a journal that has seen every state.
# A suite that reset between repeats would test the first case twenty times and the second never.
# ---------------------------------------------------------------------------------------
if [[ "$BUILD" -eq 1 ]]; then
    echo "-- resetting the pair (volumes included)"
    "$HARNESS_ROOT/scripts/dev-down.sh" --volumes >/dev/null 2>&1 || true
    "$HARNESS_ROOT/scripts/dev-up.sh" || {
        echo "e2e: the harness did not come up" >&2
        exit 1
    }
else
    echo "-- attaching to the running pair (--no-build)"
    "$HARNESS_ROOT/scripts/dev-up.sh" --no-build || {
        echo "e2e: the harness is not up and would not start" >&2
        exit 1
    }
fi

# dev-up.sh writes the token to deploy/.env; export it so the Rust half finds it either way and so
# a `--repeat` loop does not re-read the file twenty times.
if [[ -z "${ASTROCTL_TOKEN:-}" && -f "$HARNESS_ROOT/deploy/.env" ]]; then
    ASTROCTL_TOKEN="$(sed -n 's/^ASTROCTL_TOKEN=//p' "$HARNESS_ROOT/deploy/.env" | head -n1)"
    export ASTROCTL_TOKEN
fi

cleanup() {
    # Shaping lives in the containers' network namespaces and dies with them, but a run that failed
    # inside T-HOL-1 leaves the pair *up* and shaped, and the next thing anybody does is wonder why
    # their browser is slow. Removing it is cheap and unconditional.
    "$HARNESS_ROOT/scripts/shape-link.sh" off >/dev/null 2>&1 || true
    if [[ "$KEEP" -eq 0 ]]; then
        "$HARNESS_ROOT/scripts/dev-down.sh" >/dev/null 2>&1 || true
    else
        echo "-- the pair is still up (--keep)"
    fi
}
trap cleanup EXIT

# ---------------------------------------------------------------------------------------
# The runs.
# ---------------------------------------------------------------------------------------
CARGO_ARGS=(test --manifest-path "$MANIFEST")
[[ -n "$ONLY" ]] && CARGO_ARGS+=(--test "$ONLY")
CARGO_ARGS+=("${CARGO_EXTRA[@]+"${CARGO_EXTRA[@]}"}" -- --test-threads=1 --nocapture)

# Compile once before the loop, so the first run's wall-clock is a run and not a build. A failure
# here is a compile failure and says so rather than arriving as "run 1 of 20 failed".
echo "-- compiling the suite"
cargo build --manifest-path "$MANIFEST" --tests --quiet || {
    echo "e2e: the suite does not compile" >&2
    exit 1
}

# ---------------------------------------------------------------------------------------
# The crate-level suite members: T-XFER-1, T-ING-1, T-IPC-1.
#
# All three already exist as crate tests, written by the tasks that built what they test, and
# M1-T16's job is to make "run the suite" mean "run everything M1 stands on" rather than to write
# them again. A second T-ING-1 out here would assert *less* — it cannot bit-flip an upload from
# the far side of an HTTP API — and would drift from the first within a milestone.
#
# What this guards against is not the tests failing; `cargo test --workspace` in the quality gate
# catches that first. It guards against them **disappearing**: a target renamed, emptied or
# quietly `#[ignore]`d still leaves the gate green while the ID it carried stops being verified.
# So each is named explicitly — cargo fails on an unknown `--test` — and each must report a
# non-zero number of passing tests.
#
# In the shell rather than as a fifth scenario file, which is where it was first written. A
# `cargo test` invoked *from inside* a cargo test binary inherits an environment built for that
# binary, and two of those variables break the child in ways that look like compiler bugs:
# `LD_LIBRARY_PATH` points at the suite's own `deps/`, so the child's rustc loads mismatched
# proc-macro dylibs and fails inside `syn`; and the target-directory variables send the workspace
# build somewhere it then cannot link against. Here the environment is the shell's and none of it
# arises.
#
# Run once per invocation, not once per `--repeat`: these are deterministic and hermetic, and the
# flake gate is a question about the container scenarios.
crate_target() { # crate_target <package> <test-target|--bins> <id>
    local package="$1" target="$2" id="$3" selector output passed
    if [[ "$target" == "--bins" ]]; then
        selector=(--bins)
    else
        selector=(--test "$target")
    fi
    printf -- '-- %-9s %s::%s ' "$id" "$package" "$target"
    if ! output="$(cargo test --manifest-path "$HARNESS_ROOT/Cargo.toml" \
        -p "$package" "${selector[@]}" 2>&1)"; then
        printf 'FAILED\n'
        printf '%s\n' "$output" | sed 's/^/      | /' >&2
        return 1
    fi
    passed="$(grep -E '^test result: ok\.' <<<"$output" | awk '{ s += $4 } END { print s + 0 }')"
    if [[ "$passed" -eq 0 ]]; then
        printf 'FAILED\n'
        echo "      | ran zero tests — the target exists but no longer verifies anything" >&2
        return 1
    fi
    printf 'ok  (%s tests)\n' "$passed"
}

echo "-- crate suite members"
CRATE_FAILURES=0
# T-XFER-1's crate half: the journal's own crash behaviour, and the upload contract against a
# mock receiver. The two-node half — kill the stacking server, kill the field node — is the
# `faults` scenario below, because it needs two nodes and these do not.
crate_target astroctl-transfer durability T-XFER-1 || CRATE_FAILURES=$((CRATE_FAILURES + 1))
crate_target astroctl-transfer wire T-XFER-1 || CRATE_FAILURES=$((CRATE_FAILURES + 1))
# T-ING-1 lives in the stacking server's unit tests: it is a binary crate and the ingest cases
# reach into the archive and the journal, neither of which is public.
crate_target astroctl-stack --bins T-ING-1 || CRATE_FAILURES=$((CRATE_FAILURES + 1))
# T-IPC-1's two halves verify different things: `golden_messages` asserts one fixture against both
# the Rust enums and workers/astroctl_ipc.py, which is the only thing keeping the two language
# bindings in agreement; `supervision` covers the version refusal and the kill-9 restart.
crate_target astroctl-ipc golden_messages T-IPC-1 || CRATE_FAILURES=$((CRATE_FAILURES + 1))
crate_target astroctl-ipc supervision T-IPC-1 || CRATE_FAILURES=$((CRATE_FAILURES + 1))
if [[ "$CRATE_FAILURES" -gt 0 ]]; then
    echo "e2e: $CRATE_FAILURES crate suite member(s) failed" >&2
    exit 1
fi

PASSED=0
FAILED=0
FAILED_RUNS=()
for run in $(seq 1 "$REPEAT"); do
    if [[ "$REPEAT" -gt 1 ]]; then
        echo
        echo "-- run $run/$REPEAT"
    fi
    start=$SECONDS
    if cargo "${CARGO_ARGS[@]}"; then
        PASSED=$((PASSED + 1))
        printf -- '-- run %d: PASS (%ds)\n' "$run" "$((SECONDS - start))"
    else
        FAILED=$((FAILED + 1))
        FAILED_RUNS+=("$run")
        printf -- '-- run %d: FAIL (%ds)\n' "$run" "$((SECONDS - start))"
        # Keep going. The number this script exists to produce is "how many of twenty flaked",
        # and stopping at the first failure produces "at least one", which is not the same
        # answer and is not the one the acceptance criterion asks for.
    fi
done

echo
if [[ "$FAILED" -eq 0 ]]; then
    echo "OK: $PASSED/$REPEAT run(s) passed."
    exit 0
fi
echo "FAIL: $FAILED/$REPEAT run(s) failed (runs ${FAILED_RUNS[*]})." >&2
exit 1
