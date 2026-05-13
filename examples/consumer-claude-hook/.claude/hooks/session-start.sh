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
# Required Claude Code Web secret:
#   GITHUB_MCP_INTERNAL_SHARED_SECRET
#
# Optional overrides (export before the curl pipe if you need to):
#   GITHUB_MCP_ENV          staging|prod   (default: staging)
#   GITHUB_MCP_BIND_PORT    18765
#   GITHUB_MCP_PIN_TAG      v0.0.4         (pin to a specific release)
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
