# Consumer-repo example: use `github-mcp-server-rs` in Claude Code on the web

This directory shows the two files you copy into **your** repo so a Claude
Code on the web session brings up the `github-mcp-server-rs` MCP server
automatically.

```
your-repo/
├── .claude/
│   ├── settings.json            ← from this example
│   └── hooks/
│       └── session-start.sh     ← from this example
└── ...
```

## 1. Set the secret

In Claude Code (web) → Settings → Secrets, add:

- `GITHUB_MCP_INTERNAL_SHARED_SECRET` — auth-worker `INTERNAL_SHARED_SECRET`
  for the env you want (`staging` by default).

## 2. Copy the two files into your repo

```bash
mkdir -p .claude/hooks
curl -sSfL https://raw.githubusercontent.com/ippoan/github-mcp-server-rs/main/examples/consumer-claude-hook/.claude/hooks/session-start.sh \
  -o .claude/hooks/session-start.sh
chmod +x .claude/hooks/session-start.sh
curl -sSfL https://raw.githubusercontent.com/ippoan/github-mcp-server-rs/main/examples/consumer-claude-hook/.claude/settings.json \
  -o .claude/settings.json
git add .claude/
git commit -m "claude code: bring up github-mcp-server-rs on session start"
git push
```

## 3. Start a Claude Code on the web session

When the session starts, the hook will:

1. download the latest `github-mcp-server-rs` release binary,
2. install `cloudflared`,
3. run the **device authorization flow** — open the printed
   `verification_uri_complete` in a browser and approve,
4. start the MCP server on `127.0.0.1:18765` and a `cloudflared` quick tunnel,
5. print the public MCP URL (a `https://*.trycloudflare.com/mcp`) and export
   it as `$GITHUB_MCP_URL`.

Add that URL to Claude Code (web) → MCP servers, transport
**Streamable HTTP**. Confirm with `whoami`.

## Optional overrides

Set these in the consumer hook before the curl pipe:

| Env | Default | Meaning |
|---|---|---|
| `GITHUB_MCP_ENV` | `staging` | `staging` or `prod` |
| `GITHUB_MCP_BIND_PORT` | `18765` | local serve port |
| `GITHUB_MCP_PIN_TAG` | latest release | pin to a specific tag, e.g. `v0.0.4` |
