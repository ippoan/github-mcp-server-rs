#!/bin/bash
# Migrated to ippoan/mcp-relay-rs#9 Phase 4 (2026-05-20). This file is a shim
# that forwards to the new monorepo hook so existing consumer setups
# (`curl raw.githubusercontent.com/ippoan/github-mcp-server-rs/main/.claude/hooks/install-mcp.sh | bash`)
# keep working through one extra hop. Update your hook to point directly at
# https://raw.githubusercontent.com/ippoan/mcp-relay-rs/main/.claude/hooks/install-mcp.sh
# when convenient.
exec curl -sSfL https://raw.githubusercontent.com/ippoan/mcp-relay-rs/main/.claude/hooks/install-mcp.sh | bash
