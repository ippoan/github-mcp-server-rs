#!/bin/bash
# Example SessionStart hook for a *consumer* repo that wants to use the
# github-mcp-server-rs MCP server inside Claude Code on the web.
#
# Drop this file into your consumer repo at:
#   .claude/hooks/session-start.sh
#
# Then register it in your consumer repo's .claude/settings.json:
#   {
#     "hooks": {
#       "SessionStart": [
#         {
#           "hooks": [
#             { "type": "command",
#               "command": "$CLAUDE_PROJECT_DIR/.claude/hooks/session-start.sh" }
#           ]
#         }
#       ]
#     }
#   }
#
# auth-worker INTERNAL_SHARED_SECRET は v0.0.5+ release binary に build-time
# embed されているので、Claude Code Web secret 登録は不要 (#25)。
#
# Optional overrides (export before the curl pipe if you need to):
#   GITHUB_MCP_ENV          staging|prod   (default: staging)
#   GITHUB_MCP_PIN_TAG      v0.0.6         (pin to a specific release; v0.0.6+ 必須 — relay subcommand)
#
# Advanced override (embed を上書きしたい時のみ):
#   GITHUB_MCP_INTERNAL_SHARED_SECRET — 自分の auth-worker fork に当てる dev 用途等
set -euo pipefail

# Only run remotely (Claude Code on the web). Skip on local dev.
if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  exit 0
fi

# Pull and execute the reusable installer from this repo's main branch.
# For reproducibility, replace `main` with a commit SHA or tag.
curl -sSfL \
  https://raw.githubusercontent.com/ippoan/github-mcp-server-rs/main/.claude/hooks/install-mcp.sh \
  | bash
