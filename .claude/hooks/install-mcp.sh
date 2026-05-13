#!/bin/bash
# Reusable Claude Code SessionStart hook published by
# https://github.com/ippoan/github-mcp-server-rs
#
# Purpose:
#   Make the `github-mcp-server-rs` MCP server available to a Claude Code on
#   the web session from any consumer repo, exposed via cloudflared so that
#   Claude on the web can reach it over Streamable HTTP.
#
# Consumer usage — drop this into the consumer repo's
# `.claude/hooks/session-start.sh`:
#
#   #!/bin/bash
#   set -euo pipefail
#   [ "${CLAUDE_CODE_REMOTE:-}" != "true" ] && exit 0
#   curl -sSfL \
#     https://raw.githubusercontent.com/ippoan/github-mcp-server-rs/main/.claude/hooks/install-mcp.sh \
#     | bash
#
# auth-worker INTERNAL_SHARED_SECRET は v0.0.5 から release binary に build-time
# embed されている (#25)。consumer 側 secret 登録は不要。
#
# Optional env (with defaults):
#   GITHUB_MCP_ENV          staging|prod                          (default: staging)
#   GITHUB_MCP_BIND_PORT    local serve port                       (default: 18765)
#   GITHUB_MCP_PIN_TAG      pin release tag (e.g. v0.0.5)          (default: latest)
#
# Override (advanced; 通常は不要):
#   GITHUB_MCP_INTERNAL_SHARED_SECRET — embed されている値を上書きしたい時のみ
#                                       (例: 自分の auth-worker fork を叩く dev)
#
# On success:
#   - binary installed at  $HOME/.local/bin/github-mcp-server-rs
#   - cloudflared running, MCP URL written to:
#       $CLAUDE_PROJECT_DIR/.claude/mcp-state/mcp-url
#     and exported as $GITHUB_MCP_URL via $CLAUDE_ENV_FILE.
#
# Re-running is safe: existing binary / token cache / running serve are reused.

set -euo pipefail

# ─── 0. only run in Claude Code on the web ────────────────────────────────────
if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  echo "[install-mcp] skipped: not a remote Claude Code session (CLAUDE_CODE_REMOTE != true)" >&2
  exit 0
fi

REPO="ippoan/github-mcp-server-rs"
ENV_NAME="${GITHUB_MCP_ENV:-staging}"
BIND_PORT="${GITHUB_MCP_BIND_PORT:-18765}"

PROJECT_DIR="${CLAUDE_PROJECT_DIR:-$(pwd)}"
INSTALL_DIR="$HOME/.local/bin"
STATE_DIR="$PROJECT_DIR/.claude/mcp-state"
mkdir -p "$INSTALL_DIR" "$STATE_DIR"

# Make $HOME/.local/bin reachable for the rest of the session.
case ":$PATH:" in
  *":$INSTALL_DIR:"*) : ;;
  *) export PATH="$INSTALL_DIR:$PATH" ;;
esac
if [ -n "${CLAUDE_ENV_FILE:-}" ]; then
  echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$CLAUDE_ENV_FILE"
fi

# ─── 1. download github-mcp-server-rs release binary ──────────────────────────
BIN="$INSTALL_DIR/github-mcp-server-rs"
if [ ! -x "$BIN" ]; then
  if [ -n "${GITHUB_MCP_PIN_TAG:-}" ]; then
    TAG="$GITHUB_MCP_PIN_TAG"
  else
    echo "[install-mcp] resolving latest release tag..." >&2
    TAG="$(curl -sSfL "https://api.github.com/repos/$REPO/releases/latest" \
            | grep -oE '"tag_name"[[:space:]]*:[[:space:]]*"[^"]+"' \
            | head -1 | cut -d'"' -f4)"
  fi
  if [ -z "${TAG:-}" ]; then
    echo "[install-mcp] ERROR: could not resolve a release tag for $REPO" >&2
    exit 1
  fi

  ASSET="github-mcp-server-rs-${TAG}-x86_64-unknown-linux-gnu.tar.gz"
  URL="https://github.com/$REPO/releases/download/$TAG/$ASSET"
  echo "[install-mcp] downloading $ASSET..." >&2
  TMP="$(mktemp -d)"
  curl -sSfL "$URL" -o "$TMP/binary.tar.gz"
  curl -sSfL "$URL.sha256" -o "$TMP/binary.tar.gz.sha256" || true
  if [ -s "$TMP/binary.tar.gz.sha256" ]; then
    EXPECTED="$(awk '{print $1}' "$TMP/binary.tar.gz.sha256")"
    ACTUAL="$(sha256sum "$TMP/binary.tar.gz" | awk '{print $1}')"
    if [ "$EXPECTED" != "$ACTUAL" ]; then
      echo "[install-mcp] ERROR: sha256 mismatch (expected=$EXPECTED actual=$ACTUAL)" >&2
      exit 1
    fi
  fi
  tar -xzf "$TMP/binary.tar.gz" -C "$TMP"
  EXTRACTED="$(find "$TMP" -maxdepth 3 -type f -name 'github-mcp-server-rs' -perm -u+x | head -1)"
  if [ -z "$EXTRACTED" ]; then
    echo "[install-mcp] ERROR: github-mcp-server-rs not found in $ASSET" >&2
    ls -la "$TMP" >&2
    exit 1
  fi
  install -m 0755 "$EXTRACTED" "$BIN"
  rm -rf "$TMP"
fi
echo "[install-mcp] binary: $($BIN --version 2>/dev/null || echo "$BIN")" >&2

# ─── 2. install cloudflared (for HTTPS tunnel) ────────────────────────────────
if ! command -v cloudflared >/dev/null 2>&1; then
  echo "[install-mcp] installing cloudflared..." >&2
  curl -sSfL \
    "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64" \
    -o "$INSTALL_DIR/cloudflared"
  chmod +x "$INSTALL_DIR/cloudflared"
fi

# ─── 3. device-flow auth if no token cache yet ────────────────────────────────
# INTERNAL_SHARED_SECRET は v0.0.5+ release binary に embed 済みなので、ここで
# 検証する必要は無い (#25)。env で override したい advanced ユーザは serve
# 起動時に child process が inherit するので追加の処理不要。

TOKEN_FILE="$HOME/.config/github-mcp-server-rs/token-${ENV_NAME}.json"
if [ ! -f "$TOKEN_FILE" ]; then
  echo "" >&2
  echo "[install-mcp] ───── device authorization required (env=$ENV_NAME) ─────" >&2
  echo "[install-mcp] OPEN the verification_uri_complete URL printed below in a" >&2
  echo "[install-mcp] browser, sign in with GitHub, and Approve.  The hook will" >&2
  echo "[install-mcp] block until polling completes." >&2
  echo "" >&2
  "$BIN" auth --env "$ENV_NAME" >&2
fi

# ─── 4. (re)start serve in the background ─────────────────────────────────────
start_serve() {
  local allowed_hosts="$1"
  if [ -f "$STATE_DIR/serve.pid" ]; then
    local old_pid
    old_pid="$(cat "$STATE_DIR/serve.pid" 2>/dev/null || true)"
    if [ -n "$old_pid" ] && kill -0 "$old_pid" 2>/dev/null; then
      kill "$old_pid" 2>/dev/null || true
      sleep 1
    fi
  fi
  nohup "$BIN" serve --env "$ENV_NAME" \
    --bind "127.0.0.1:$BIND_PORT" \
    --allowed-hosts "$allowed_hosts" \
    > "$STATE_DIR/serve.log" 2>&1 &
  echo $! > "$STATE_DIR/serve.pid"
}

start_serve "localhost,127.0.0.1"

# Wait for serve to bind the port.
ready=0
for _ in 1 2 3 4 5 6 7 8 9 10; do
  if (echo > "/dev/tcp/127.0.0.1/$BIND_PORT") 2>/dev/null; then
    ready=1; break
  fi
  sleep 1
done
if [ "$ready" != "1" ]; then
  echo "[install-mcp] ERROR: serve did not bind 127.0.0.1:$BIND_PORT" >&2
  tail -n 50 "$STATE_DIR/serve.log" >&2 || true
  exit 1
fi

# ─── 5. start cloudflared & extract the trycloudflare URL ────────────────────
: > "$STATE_DIR/cloudflared.log"
nohup cloudflared tunnel --no-autoupdate \
  --url "http://127.0.0.1:$BIND_PORT" \
  > "$STATE_DIR/cloudflared.log" 2>&1 &
echo $! > "$STATE_DIR/cloudflared.pid"

TUNNEL_URL=""
for _ in $(seq 1 60); do
  TUNNEL_URL="$(grep -oE 'https://[a-z0-9-]+\.trycloudflare\.com' \
                  "$STATE_DIR/cloudflared.log" 2>/dev/null | head -1 || true)"
  [ -n "$TUNNEL_URL" ] && break
  sleep 1
done

if [ -z "$TUNNEL_URL" ]; then
  echo "[install-mcp] ERROR: failed to extract cloudflared tunnel URL" >&2
  tail -n 50 "$STATE_DIR/cloudflared.log" >&2 || true
  exit 1
fi

# ─── 6. restart serve with the trycloudflare host in --allowed-hosts ──────────
TUNNEL_HOST="${TUNNEL_URL#https://}"
start_serve "localhost,127.0.0.1,$TUNNEL_HOST"

# ─── 7. publish the URL for the session ───────────────────────────────────────
MCP_URL="$TUNNEL_URL/mcp"
echo "$MCP_URL" > "$STATE_DIR/mcp-url"
if [ -n "${CLAUDE_ENV_FILE:-}" ]; then
  echo "export GITHUB_MCP_URL=\"$MCP_URL\"" >> "$CLAUDE_ENV_FILE"
fi

cat >&2 <<EOF

[install-mcp] ✓ github-mcp-server-rs is ready.
[install-mcp]   MCP URL (Streamable HTTP): $MCP_URL
[install-mcp]   Add it to Claude Code's MCP settings on the web, or it is also
[install-mcp]   exported as \$GITHUB_MCP_URL and written to:
[install-mcp]     $STATE_DIR/mcp-url
EOF
