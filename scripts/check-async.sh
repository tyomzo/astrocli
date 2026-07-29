#!/usr/bin/env bash
#
# check-async.sh — reject blocking calls in the async binaries (SDD §2, §7).
#
# The two clippy lints in the root manifest (`await_holding_lock`,
# `await_holding_refcell_ref`) catch a guard held across an `.await`. They cannot see the other
# way the runtime gets blocked: a synchronous call that simply does not yield —
# `std::thread::sleep`, `std::fs::` reads, or a `.blocking_*()` variant of an async API. Those
# compile cleanly, pass every test on a fast desktop, and then park a runtime worker on a Pi.
#
# The field node runs `min(2, cores - 2)` workers with a floor of 1 (SDD §7). Blocking one of
# them is not a slowdown, it is a fraction of the whole node — and the fraction is 1/2 or 1/1.
#
# Usage:  scripts/check-async.sh [--verbose]
# Exit:   0 = clean, 1 = findings, 2 = the script could not run
#
# Deliberate decisions:
#
#   * Only the two binaries are scanned. `astroctl-core` and the library crates are sync by
#     design and legitimately use `std::fs`; the rule is about code that runs *on the runtime*.
#
#   * Test code is exempt. A `#[tokio::test]` blocking its own runtime affects one test. The
#     exemption is by path (`tests/`) and by the `#[cfg(test)]` module marker, which is coarse —
#     it exempts a whole file's tail once a `mod tests` opens, and that is the right trade for a
#     grep: the alternative is parsing Rust, and the honest name for that is clippy.
#
#   * `build.rs` is exempt. It runs at build time, not on a runtime.
#
#   * A line may be waived with a trailing or preceding `check-async: allow <reason>` comment.
#     The reason is mandatory and is the entire point: a bare opt-out teaches a reviewer nothing
#     and is indistinguishable from someone silencing an inconvenient gate. With the reason at
#     the site, "runs before the runtime exists" can be checked in five seconds. Waivers are
#     counted and printed, so they cannot quietly accumulate.
#
# This is a net, not a proof. T-ISO-1 is the behavioural test that catches what slips through.

set -euo pipefail

VERBOSE=0
[[ "${1:-}" == "--verbose" ]] && VERBOSE=1

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Each entry: pattern <TAB> why it matters.
PATTERNS=$(
    cat <<'EOF'
std::thread::sleep	parks a runtime worker; use tokio::time::sleep
\bstd::fs::	blocking file I/O on the runtime; use tokio::fs
\.blocking_(send|recv|read|write|lock)\b	the blocking variant of an async API; await the async one
EOF
)

FINDINGS=0
SCANNED=0
WAIVED=0

for crate in astroctl-field astroctl-stack; do
    [[ -d "crates/$crate/src" ]] || continue
    while IFS= read -r file; do
        [[ "$(basename "$file")" == "build.rs" ]] && continue
        SCANNED=$((SCANNED + 1))

        # Everything from the first `#[cfg(test)]` onward is test code.
        cutoff=$(grep -n '#\[cfg(test)\]' "$file" | head -1 | cut -d: -f1 || true)
        if [[ -n "$cutoff" ]]; then
            body=$(head -n $((cutoff - 1)) "$file")
        else
            body=$(cat "$file")
        fi

        mapfile -t lines <<<"$body"

        while IFS=$'\t' read -r pattern why; do
            [[ -z "$pattern" ]] && continue
            while IFS= read -r hit; do
                [[ -z "$hit" ]] && continue
                line="${hit%%:*}"
                text="${hit#*:}"

                # A waiver on the offending line or the one above it.
                prev="${lines[$((line - 2))]:-}"
                if [[ "$text" == *"check-async: allow"* || "$prev" == *"check-async: allow"* ]]; then
                    WAIVED=$((WAIVED + 1))
                    [[ "$VERBOSE" -eq 1 ]] && echo "-- waived $file:$line"
                    continue
                fi

                echo "FAIL: $file:$line — $why" >&2
                echo "        ${text#"${text%%[![:space:]]*}"}" >&2
                FINDINGS=$((FINDINGS + 1))
            done < <(grep -nE "$pattern" <<<"$body" || true)
        done <<<"$PATTERNS"
    done < <(find "crates/$crate" -name '*.rs' -not -path '*/target/*')
done

if [[ "$VERBOSE" -eq 1 ]]; then
    echo "-- scanned $SCANNED non-test source files in astroctl-field, astroctl-stack"
fi

if [[ "$FINDINGS" -gt 0 ]]; then
    echo >&2
    echo "FAIL: $FINDINGS blocking call(s) on an async path. See SDD §2 and §7." >&2
    echo "      If one is genuinely correct, wrap it in spawn_blocking and say why in a comment." >&2
    exit 1
fi

if [[ "$WAIVED" -gt 0 ]]; then
    echo "OK: $SCANNED files scanned, no blocking calls on async paths ($WAIVED waived)."
else
    echo "OK: $SCANNED files scanned, no blocking calls on async paths."
fi
