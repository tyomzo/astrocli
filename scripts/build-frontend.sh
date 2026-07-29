#!/usr/bin/env bash
#
# build-frontend.sh — produce frontend/dist/, the bundle astroctl-field embeds (ARC-02, M0-T06).
#
# Usage:  scripts/build-frontend.sh [--no-install] [--check]
# Exit:   0 = dist/ built, 1 = the build failed, 2 = the script could not run
#
#   --no-install  skip `npm ci` and use the existing node_modules. For iterating; a release build
#                 should not use it, because node_modules is the one input to this build that is
#                 not in git.
#   --check       additionally build a second time into a scratch directory and compare checksums,
#                 which is how the determinism claim below is verified rather than asserted.
#
# Deliberate decisions, so nobody has to re-derive them:
#
#   * `npm ci`, never `npm install`. `ci` installs exactly what package-lock.json pins and fails
#     if the lock and the manifest disagree; `install` silently updates the lock, which turns
#     "the bundle differs between two machines" into a thing that can happen without anyone
#     changing a dependency on purpose.
#
#   * `dist/` is removed first. Vite writes content-hashed filenames, so a stale asset from a
#     previous build is not overwritten — it is simply left behind, and `include_dir!` then
#     compiles it into the binary. That is how a binary ends up shipping two versions of the
#     application, one of which nothing references.
#
#   * The output is deterministic: same lockfile plus same sources gives byte-identical files,
#     because nothing in the pipeline stamps a timestamp or a build id into the output. `--check`
#     proves it. If that ever stops holding, the cause is a new plugin, and the fix is that
#     plugin — not a note here explaining why the checksums drift.
#
#   * This is NOT wired into `cargo build`. A Rust-only contributor must never be blocked on npm,
#     so astroctl-field's build.rs detects the absence of dist/ and compiles a placeholder page
#     instead (see crates/astroctl-field/src/pwa.rs).

set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
FRONTEND="$ROOT/frontend"
DIST="$FRONTEND/dist"

INSTALL=1
CHECK=0
for arg in "$@"; do
    case "$arg" in
    --no-install) INSTALL=0 ;;
    --check) CHECK=1 ;;
    *)
        echo "build-frontend: unknown argument '$arg'" >&2
        exit 2
        ;;
    esac
done

command -v node >/dev/null 2>&1 || { echo "build-frontend: node not found on PATH" >&2; exit 2; }
command -v npm >/dev/null 2>&1 || { echo "build-frontend: npm not found on PATH" >&2; exit 2; }

# Vite 8 and @vitejs/plugin-react 6 both require ^20.19 || >=22.12. Checking here turns an
# obscure syntax error deep in a plugin into a sentence naming the actual problem.
NODE_MAJOR="$(node -p 'process.versions.node.split(".")[0]')"
NODE_MINOR="$(node -p 'process.versions.node.split(".")[1]')"
if [[ "$NODE_MAJOR" -lt 20 ]] ||
    { [[ "$NODE_MAJOR" -eq 20 ]] && [[ "$NODE_MINOR" -lt 19 ]]; } ||
    { [[ "$NODE_MAJOR" -eq 21 ]]; }; then
    echo "build-frontend: node $(node -v) is too old; need ^20.19 or >=22.12" >&2
    exit 2
fi

echo "== astroctl frontend build =="
echo "-- node $(node -v), npm $(npm -v)"

if [[ "$INSTALL" -eq 1 ]]; then
    echo "-- npm ci (exactly what package-lock.json pins)"
    (cd "$FRONTEND" && npm ci --no-audit --no-fund)
else
    echo "-- skipping npm ci (--no-install)"
    [[ -d "$FRONTEND/node_modules" ]] || {
        echo "build-frontend: --no-install given but $FRONTEND/node_modules does not exist" >&2
        exit 2
    }
fi

echo "-- rm -rf dist"
rm -rf "$DIST"

echo "-- tsc --noEmit && vite build"
(cd "$FRONTEND" && npm run build)

[[ -f "$DIST/index.html" ]] || {
    echo "build-frontend: the build reported success but $DIST/index.html does not exist" >&2
    exit 1
}

# Every file, its size, and the whole tree's checksum. The last line is what to compare when
# asking "is the binary I am holding built from this bundle".
echo "-- dist/"
(cd "$DIST" && find . -type f | sort | while read -r f; do
    printf '   %8s  %s\n' "$(wc -c <"$f")" "${f#./}"
done)
TREE_SUM="$(cd "$DIST" && find . -type f -exec sha256sum {} \; | sort -k2 | sha256sum | cut -d' ' -f1)"
TOTAL="$(du -sh "$DIST" | cut -f1)"
echo "-- $TOTAL total, tree sha256 $TREE_SUM"

if [[ "$CHECK" -eq 1 ]]; then
    echo "-- determinism check: rebuilding and comparing"
    SCRATCH="$(mktemp -d)"
    trap 'rm -rf "$SCRATCH"' EXIT
    cp -r "$DIST" "$SCRATCH/first"
    rm -rf "$DIST"
    (cd "$FRONTEND" && npm run build >/dev/null)
    SECOND_SUM="$(cd "$DIST" && find . -type f -exec sha256sum {} \; | sort -k2 | sha256sum | cut -d' ' -f1)"
    if [[ "$TREE_SUM" != "$SECOND_SUM" ]]; then
        echo "build-frontend: the build is NOT deterministic ($TREE_SUM vs $SECOND_SUM)" >&2
        diff -r "$SCRATCH/first" "$DIST" >&2 || true
        exit 1
    fi
    echo "-- deterministic: two clean builds agree"
fi

echo "OK: $DIST is ready to embed. Rebuild astroctl-field to pick it up."
