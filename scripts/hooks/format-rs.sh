#!/usr/bin/env bash
# Claude Code PostToolUse hook (matcher: Write|Edit).
# Auto-formats an edited Rust file with rustfmt. Never blocks (always exits 0).
payload="$(cat)"
f="$(printf '%s' "$payload" | node -e "let d='';process.stdin.on('data',c=>d+=c).on('end',()=>{try{const j=JSON.parse(d);process.stdout.write((j.tool_response&&j.tool_response.filePath)||(j.tool_input&&j.tool_input.file_path)||'')}catch(e){}})" 2>/dev/null)"
case "$f" in
  *.rs)
    if [ -n "$f" ] && [ -f "$f" ] && command -v rustfmt >/dev/null 2>&1; then
      rustfmt --edition 2024 "$f" >/dev/null 2>&1
    fi
    ;;
esac
exit 0
