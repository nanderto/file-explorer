#!/usr/bin/env bash
# Quality gate for the file-explorer repo. Run by Claude Code hooks (pre-commit /
# pre-push), by CI, and manually. Exits 2 with a reason on failure.
#
# Usage:
#   gate.sh commit  [original-command]   staged-diff checks + full cargo gate
#   gate.sh push    [original-command]   branch-vs-origin/main diff checks + full cargo gate
#   gate.sh ci-diff <base-ref>           diff checks only (no cargo) — used by the CI docs job
set -u
MODE="${1:-commit}"
ARG2="${2:-}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 2

fail() { printf 'GATE FAILED [%s]: %s\n' "$1" "$2" >&2; exit 2; }

# ---------- cargo gates (skipped until the workspace exists, and in ci-diff mode) ----------
if [ "$MODE" != "ci-diff" ]; then
  if [ -f Cargo.toml ]; then
    echo "gate: cargo fmt --check"
    cargo fmt --all --check || fail fmt "formatting is off — run 'cargo fmt --all' and retry"
    echo "gate: cargo build (workspace, all targets)"
    cargo build --workspace --all-targets || fail build "the workspace does not compile"
    echo "gate: cargo clippy (-D warnings)"
    cargo clippy --workspace --all-targets -- -D warnings || fail clippy "clippy lints must be fixed, not silenced"
    echo "gate: cargo test (unit + integration + UI)"
    cargo test --workspace || fail test "tests are failing"
  else
    echo "gate: no Cargo.toml yet — cargo gates skipped"
  fi
fi

# ---------- diff checks: code changes must ship with tests and AS_BUILT.md updates ----------
if ! git rev-parse --git-dir >/dev/null 2>&1; then
  echo "gate: not a git repo yet — diff checks skipped"
  echo "gate: PASS ($MODE)"
  exit 0
fi

case "$ARG2" in
  *"[skip-checks]"*)
    echo "gate: [skip-checks] marker found — docs/tests diff checks skipped"
    echo "gate: PASS ($MODE)"
    exit 0
    ;;
esac

if [ "$MODE" = "commit" ]; then
  CHANGED="$(git diff --cached --name-only)"
  DIFF_CMD="git diff --cached"
elif [ "$MODE" = "ci-diff" ]; then
  BASE="${ARG2:-origin/main}"
  CHANGED="$(git diff --name-only "$BASE"...HEAD)" || fail diff "cannot diff against $BASE (shallow clone? use fetch-depth: 0)"
  DIFF_CMD="git diff $BASE...HEAD"
else
  BASE="$(git merge-base HEAD origin/main 2>/dev/null || echo '')"
  if [ -z "$BASE" ]; then
    echo "gate: no origin/main to compare against — diff checks skipped"
    echo "gate: PASS ($MODE)"
    exit 0
  fi
  CHANGED="$(git diff --name-only "$BASE" HEAD)"
  DIFF_CMD="git diff $BASE HEAD"
fi

CODE_CHANGED="$(printf '%s\n' "$CHANGED" | grep -E '\.rs$|Cargo\.toml$' | grep -v '^docs/' || true)"
if [ -n "$CODE_CHANGED" ]; then
  # 1) Documentation: AS_BUILT.md must be updated alongside code.
  printf '%s\n' "$CHANGED" | grep -qx 'docs/AS_BUILT.md' \
    || fail docs "code changed but docs/AS_BUILT.md was not updated — record what was built/changed there (and update other affected docs)"

  # 2) Tests: source changes must come with test-file changes or new #[test]/#[gpui::test] blocks.
  SRC_CHANGED="$(printf '%s\n' "$CODE_CHANGED" | grep -E '^crates/[^/]+/src/.*\.rs$' || true)"
  TEST_FILES_CHANGED="$(printf '%s\n' "$CHANGED" | grep -E '^crates/[^/]+/tests/.*\.rs$' || true)"
  if [ -n "$SRC_CHANGED" ] && [ -z "$TEST_FILES_CHANGED" ]; then
    $DIFF_CMD -- '*.rs' | grep -qE '^\+.*#\[ *(test|gpui::test|cfg\( *test *\))' \
      || fail tests "source changed but no tests were added or updated — add unit tests (in-module #[test]), integration tests (crates/*/tests/), or UI tests (#[gpui::test]). For a change that genuinely needs no tests, include [skip-checks] in the commit command and justify it in the PR."
  fi
fi

echo "gate: PASS ($MODE)"
exit 0
