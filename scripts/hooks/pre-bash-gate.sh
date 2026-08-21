#!/usr/bin/env bash
# Claude Code PreToolUse hook (matcher: Bash).
# Runs the quality gate before `git commit`, `git push`, or `gh pr create`.
# Exit 0 = allow the command; exit 2 = block it (stderr is fed back to Claude).
payload="$(cat)"
cmd="$(printf '%s' "$payload" | node -e "let d='';process.stdin.on('data',c=>d+=c).on('end',()=>{try{process.stdout.write(JSON.parse(d).tool_input.command||'')}catch(e){}})" 2>/dev/null)"
ROOT="${CLAUDE_PROJECT_DIR:-$(cd "$(dirname "$0")/../.." && pwd)}"
case "$cmd" in
  *"git commit"*)
    exec bash "$ROOT/scripts/gate.sh" commit "$cmd" 1>&2
    ;;
  *"git push"*|*"gh pr create"*)
    exec bash "$ROOT/scripts/gate.sh" push "$cmd" 1>&2
    ;;
esac
exit 0
