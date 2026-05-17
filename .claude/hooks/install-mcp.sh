#!/bin/bash
# Reusable Claude Code SessionStart hook published by
# https://github.com/ippoan/github-mcp-server-rs
#
# Purpose:
#   Make the `github-mcp-server-rs` MCP server available to a Claude Code on the
#   web session from any consumer repo. Outbound WebSocket relay against
#   auth-worker `mcp(-staging).ippoan.org` (issue #27, paired with
#   ippoan/auth-worker#117) — no cloudflared, no inbound port.
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
#   GITHUB_MCP_PIN_TAG      pin release tag (e.g. v0.0.6)         (default: latest)
#   GITHUB_MCP_FORCE_REINSTALL=1  force re-download even when tag matches
#
# Override (advanced; 通常は不要):
#   GITHUB_MCP_INTERNAL_SHARED_SECRET — embed されている値を上書きしたい時のみ
#                                       (例: 自分の auth-worker fork を叩く dev)
#
# On success:
#   - binary installed at  $HOME/.local/bin/github-mcp-server-rs
#   - relay running (outbound WS to mcp(-staging).ippoan.org)
#   -固定 MCP URL written to:
#       $CLAUDE_PROJECT_DIR/.claude/mcp-state/mcp-url
#     and exported as $GITHUB_MCP_URL via $CLAUDE_ENV_FILE.
#
# Re-running is safe: existing binary / token cache / running relay are reused.

set -euo pipefail

# ─── 0. only run in Claude Code on the web ────────────────────────────────────
if [ "${CLAUDE_CODE_REMOTE:-}" != "true" ]; then
  echo "[install-mcp] skipped: not a remote Claude Code session (CLAUDE_CODE_REMOTE != true)" >&2
  exit 0
fi

REPO="ippoan/github-mcp-server-rs"
ENV_NAME="${GITHUB_MCP_ENV:-staging}"

PROJECT_DIR="${CLAUDE_PROJECT_DIR:-$(pwd)}"
INSTALL_DIR="$HOME/.local/bin"
STATE_DIR="$PROJECT_DIR/.claude/mcp-state"
mkdir -p "$INSTALL_DIR" "$STATE_DIR"

# Cleanup state files from old cloudflared-based versions (issue #27 hard-cut).
rm -f "$STATE_DIR/serve.pid" "$STATE_DIR/serve.log" \
      "$STATE_DIR/cloudflared.pid" "$STATE_DIR/cloudflared.log" \
      "$STATE_DIR/url" 2>/dev/null || true

# Make $HOME/.local/bin reachable for the rest of the session.
case ":$PATH:" in
  *":$INSTALL_DIR:"*) : ;;
  *) export PATH="$INSTALL_DIR:$PATH" ;;
esac
if [ -n "${CLAUDE_ENV_FILE:-}" ]; then
  echo "export PATH=\"$INSTALL_DIR:\$PATH\"" >> "$CLAUDE_ENV_FILE"
fi

# ─── 1. resolve target release tag & (re)download if stale ───────────────────
# Issue: previously the script skipped download whenever `$BIN` existed,
# so consumers stayed pinned to whatever tag was first installed (e.g.
# v0.0.10 staying live while v0.0.11 was already cut). The relay then
# advertised a stale tools/list (missing tools added in newer tags).
#
# Fix: always resolve the desired TAG, compare against `$BIN.tag` (the tag
# we recorded at last successful install), and re-download on mismatch.
# Honors `GITHUB_MCP_FORCE_REINSTALL=1` for ad-hoc forced refresh.
BIN="$INSTALL_DIR/github-mcp-server-rs"
TAG_FILE="$BIN.tag"

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

INSTALLED_TAG=""
[ -s "$TAG_FILE" ] && INSTALLED_TAG="$(cat "$TAG_FILE" 2>/dev/null || true)"

# Read the release tag the binary was built from. Release builds embed it
# via build.rs (`BUILD_RELEASE_TAG` from `GITHUB_REF_NAME` on tag push),
# and clap prints it in parentheses, e.g.:
#   github-mcp-server-rs 0.1.0 (v0.0.11)
# Dev/local builds emit no parens, so $EMBEDDED_TAG stays empty.
EMBEDDED_TAG=""
if [ -x "$BIN" ]; then
  EMBEDDED_TAG="$("$BIN" --version 2>/dev/null \
    | grep -oE '\(v[0-9][^)]*\)' \
    | head -1 \
    | tr -d '()' || true)"
fi

need_install=0
if [ ! -x "$BIN" ]; then
  need_install=1
elif [ "$INSTALLED_TAG" != "$TAG" ]; then
  echo "[install-mcp] upgrading binary: $INSTALLED_TAG -> $TAG" >&2
  need_install=1
elif [ -n "$EMBEDDED_TAG" ] && [ "$EMBEDDED_TAG" != "$TAG" ]; then
  # Extra guard added on top of #39's TAG_FILE check: the file can lie
  # (manual touch, partial install, copy from another host), so cross-check
  # against the tag the binary itself was built from. Empty EMBEDDED_TAG
  # means a pre-guard release or a local dev build — skip the check in
  # that case to avoid clobbering legitimate dev binaries.
  echo "[install-mcp] binary embeds $EMBEDDED_TAG but expected $TAG -- re-downloading" >&2
  need_install=1
elif [ "${GITHUB_MCP_FORCE_REINSTALL:-}" = "1" ]; then
  echo "[install-mcp] GITHUB_MCP_FORCE_REINSTALL=1 set, re-downloading $TAG" >&2
  need_install=1
fi

if [ "$need_install" = "1" ]; then
  # Note: the existing relay process (if any) is killed in step 4 below
  # before being restarted, so it picks up the new binary at $BIN.
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
  printf '%s\n' "$TAG" > "$TAG_FILE"
  rm -rf "$TMP"
fi
echo "[install-mcp] binary: $($BIN --version 2>/dev/null || echo "$BIN") (tag=$TAG)" >&2

# ─── 2. binary は relay subcommand を持つか? (古い tag の pin 対策) ──────────
if ! "$BIN" relay --help >/dev/null 2>&1; then
  echo "[install-mcp] ERROR: installed binary does not support 'relay' subcommand." >&2
  echo "[install-mcp]        Required: v0.0.6 or later (issue #27)." >&2
  echo "[install-mcp]        If GITHUB_MCP_PIN_TAG is set, bump it to v0.0.6+." >&2
  exit 1
fi

# ─── 3. device-flow auth if no token cache yet ────────────────────────────────
# CCoW (Claude Code on the web) containers are ephemeral: $HOME is wiped on
# reclaim, so the local token cache file disappears too — every new container
# would otherwise re-prompt for device-flow auth. To make a fresh container
# bootstrap silently, the user can pre-stage the cached token JSON via the
# env var $GITHUB_MCP_TOKEN_JSON (registered as a CCoW Setup-script secret).
# The auth-worker refresh token in that JSON is long-lived (~30 days), so the
# user just rotates the secret once a month, not once per session.
TOKEN_FILE="$HOME/.config/github-mcp-server-rs/token-${ENV_NAME}.json"
if [ ! -f "$TOKEN_FILE" ] && [ -n "${GITHUB_MCP_TOKEN_JSON:-}" ]; then
  echo "[install-mcp] hydrating $TOKEN_FILE from \$GITHUB_MCP_TOKEN_JSON" >&2
  mkdir -p "$(dirname "$TOKEN_FILE")"
  printf '%s' "$GITHUB_MCP_TOKEN_JSON" > "$TOKEN_FILE"
  chmod 600 "$TOKEN_FILE"
fi
if [ ! -f "$TOKEN_FILE" ]; then
  echo "" >&2
  echo "[install-mcp] ───── device authorization required (env=$ENV_NAME) ─────" >&2
  echo "[install-mcp] OPEN the verification_uri_complete URL printed below in a" >&2
  echo "[install-mcp] browser, sign in with GitHub, and Approve.  The hook will" >&2
  echo "[install-mcp] block until polling completes." >&2
  echo "[install-mcp]" >&2
  echo "[install-mcp] Tip: to skip this prompt on future fresh containers, copy" >&2
  echo "[install-mcp]   $TOKEN_FILE" >&2
  echo "[install-mcp] into a CCoW Setup-script secret named GITHUB_MCP_TOKEN_JSON." >&2
  echo "" >&2
  "$BIN" auth --env "$ENV_NAME" >&2
fi

# ─── 4. (re)start relay in the background ─────────────────────────────────────
if [ -f "$STATE_DIR/relay.pid" ]; then
  old_pid="$(cat "$STATE_DIR/relay.pid" 2>/dev/null || true)"
  if [ -n "$old_pid" ] && kill -0 "$old_pid" 2>/dev/null; then
    kill "$old_pid" 2>/dev/null || true
    sleep 1
  fi
fi

: > "$STATE_DIR/relay.log"
nohup "$BIN" relay --env "$ENV_NAME" --state-dir "$STATE_DIR" \
  > "$STATE_DIR/relay.log" 2>&1 &
echo $! > "$STATE_DIR/relay.pid"

# ─── 5. wait for the relay to write the public URL state file ────────────────
# binary は `--state-dir` の `<dir>/url` に固定 URL を書いてから WS connect を始める。
# install-mcp.sh はその file 出現を待つ。30s で諦める。
ready=0
for _ in $(seq 1 30); do
  if [ -s "$STATE_DIR/url" ]; then
    ready=1; break
  fi
  if ! kill -0 "$(cat "$STATE_DIR/relay.pid")" 2>/dev/null; then
    echo "[install-mcp] ERROR: relay process died during startup. Log:" >&2
    tail -n 50 "$STATE_DIR/relay.log" >&2 || true
    exit 1
  fi
  sleep 1
done
if [ "$ready" != "1" ]; then
  echo "[install-mcp] ERROR: relay did not produce $STATE_DIR/url within 30s." >&2
  tail -n 50 "$STATE_DIR/relay.log" >&2 || true
  exit 1
fi

MCP_URL="$(cat "$STATE_DIR/url")"
echo "$MCP_URL" > "$STATE_DIR/mcp-url"
if [ -n "${CLAUDE_ENV_FILE:-}" ]; then
  echo "export GITHUB_MCP_URL=\"$MCP_URL\"" >> "$CLAUDE_ENV_FILE"
fi

cat >&2 <<EOF

[install-mcp] ✓ github-mcp-server-rs is ready (relay mode).
[install-mcp]   MCP URL (Streamable HTTP via auth-worker WS relay): $MCP_URL
[install-mcp]   This URL is **stable** — register it once in Claude Code Web's MCP
[install-mcp]   settings, no need to update per-session.
[install-mcp]   Also exported as \$GITHUB_MCP_URL and written to:
[install-mcp]     $STATE_DIR/mcp-url
[install-mcp]   Relay log: $STATE_DIR/relay.log
EOF
