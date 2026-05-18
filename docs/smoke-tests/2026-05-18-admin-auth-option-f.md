# Admin auth path (Option F) — staging smoke test

Companion record to [`ippoan/auth-worker docs/smoke-tests/2026-05-18-admin-auth-option-f.md`](https://github.com/ippoan/auth-worker/blob/main/docs/smoke-tests/2026-05-18-admin-auth-option-f.md) (the proxy under test lives there); this copy is kept here so the binary repo has a local pointer the next time `set/get/delete_branch_protection` is touched.

| Field        | Value                                                                  |
|--------------|------------------------------------------------------------------------|
| Date         | 2026-05-18                                                             |
| Environment  | staging (`auth-staging.ippoan.org`, `mcp-staging.ippoan.org`)          |
| PRs covered  | ippoan/github-mcp-server-rs#48, ippoan/auth-worker#149                 |
| Binary       | v0.0.14 (first release containing PR #48 — proxy via `/mcp/admin/exec`) |
| Result       | **PASS** (7/8 checks; 403 `not_elevated` path deferred)                |

## Binary-side contract verified

PR #48 moved `set/get/delete_branch_protection` from direct `api.github.com` calls to a proxy through `auth-worker POST /mcp/admin/exec`. The smoke test confirms:

1. **Always-exposed admin tools.** `GithubMcp::new` no longer gates `branches_router` behind `mcp.admin` scope. The token cache from check #3 (`scope=mcp.read mcp.write`) was sufficient to reach `/mcp/admin/exec`; the proxy enforces elevation server-side rather than relying on JWT scope.
2. **`GithubContext` plumbing.** `run_relay()` populates `jwt = token.access_token` and `auth_worker_origin = cfg.auth_base` correctly — the `Bearer` header that reached the proxy decoded to the expected JWT (`sub=github:yhonda-ohishi`, `aud=github-mcp-server-rs`).
3. **`src/admin_exec.rs` error contract.** Exercised the 200 success path (HTTP 200 → tool result returned verbatim as JSON). 401/403/400/502 surfaces remain covered only by the existing `cargo test` suite; the wire-level 403 `not_elevated` from a real expired flag was not run this session.

## What was found that the binary contract should be aware of

- The OAuth token recovered from the proxy's KV (`github_token:{sub}`) carries `read:user, repo` scopes — confirmed by GitHub's `x-oauth-scopes` header on `/user`. No `delete_repo`, no `admin:org`, no `admin:repo_hook`. Branch protection write paths exercised in this binary's admin tool set fit inside that surface; if a future admin tool needs anything wider (e.g. webhook management, repo deletion), the pair-flow OAuth scope mapping in `auth-worker/src/lib/mcp-scope.ts::mcpToGithubScope` would need to widen first, AND the security model would need re-evaluation since wider `repo` derivatives carry significantly more blast radius.
- Empirically verified that this token **cannot delete repos** (HTTP 403 "Must have admin rights to Repository." with empty `x-accepted-oauth-scopes` — GitHub's documented signal for missing `delete_repo`). This is the structural narrowing PR #149 advertises.

## Reproduction (binary perspective)

```bash
# fresh container — binary is NOT installed by cc-relay session-start hook (see Issue)
curl -sSfL "https://github.com/ippoan/github-mcp-server-rs/releases/download/v0.0.14/github-mcp-server-rs-v0.0.14-x86_64-unknown-linux-gnu.tar.gz" \
  | tar -xz -C /tmp && install -m 0755 /tmp/github-mcp-server-rs ~/.local/bin/

~/.local/bin/github-mcp-server-rs --env staging auth     # device flow, prints URL
# browser: approve
~/.local/bin/github-mcp-server-rs --env staging whoami   # verifies introspect + GitHub /user round-trip
# browser: open https://auth-staging.ippoan.org/mcp/elevate, click Authorize

# admin tool via proxy
JWT=$(python3 -c "import json; print(json.load(open('$HOME/.config/github-mcp-server-rs/token-staging.json'))['access_token'])")
curl -sS -X POST "https://auth-staging.ippoan.org/mcp/admin/exec" \
  -H "Authorization: Bearer $JWT" \
  -H "Content-Type: application/json" \
  --data '{"tool":"get_branch_protection","args":{"owner":"ippoan","repo":"cc-relay","branch":"main"}}'
```

## Known gap surfaced by this smoke test (binary repo follow-up)

The `cc-relay` session-start hook does NOT currently invoke `github-mcp-server-rs/.claude/hooks/install-mcp.sh`, so a fresh CCoW container does not have the binary even though everything in this binary repo is wired up to install it. The fix belongs in `cc-relay/.claude/hooks/session-start.sh` (or its `claude-hooks` delegate), not here, but is worth tracking: until that's wired the binary must be installed by hand or via a `claude-md`-level hook update. See discussion in this session's transcript.
