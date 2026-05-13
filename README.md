# github-mcp-server-rs

GitHub MCP server (Model Context Protocol) — `auth-worker` の **Device Authorization Grant (RFC 8628)** クライアント実装。
Claude Code / Claude Desktop などの MCP host から GitHub API を叩く用途で、`ippoan/auth-worker` の MCP OAuth Provider と組で動く。

> **Status**: Phase 6 MVP — auth flow + introspect 検証のみ。実 MCP server (stdio JSON-RPC + tool definitions) は次フェーズで追加。

## 動作の全体像

```
┌───────────────────────────────────────────────────────────────────┐
│ 1. github-mcp-server-rs auth --env staging                        │
│      → POST /mcp/device_authorization                             │
│      ← device_code / user_code (BCDF-GHJK) / verification_uri     │
│                                                                   │
│ 2. ブラウザで verification_uri_complete を開く                      │
│      → /device → user_code 確認 → Approve                          │
│      → GitHub OAuth (read:user) → /mcp/device_callback             │
│      → ACL pass → KV に github_token を AES-256-GCM 暗号化保存      │
│                                                                   │
│ 3. binary が POST /mcp/token を polling                            │
│      ← 200 { access_token (JWT), refresh_token, scope, expires_in }│
│      → ~/.config/github-mcp-server-rs/token-staging.json に保存     │
│                                                                   │
│ 4. github-mcp-server-rs whoami --env staging                      │
│      → POST /mcp/introspect (Bearer = INTERNAL_SHARED_SECRET)      │
│      ← 200 { active: true, github_login, github_token, ... }       │
│      → GitHub /user を github_token で叩いて login 表示             │
└───────────────────────────────────────────────────────────────────┘
```

## Quick start (staging で先行検証)

### 前提

- Rust 1.75+ (`rustup install stable`)
- `ippoan/auth-worker` が staging deploy 済み (`auth-staging.ippoan.org`)
- staging の `INTERNAL_SHARED_SECRET` を持っている (auth-worker の `.dev.vars` or wrangler secret)
- GitHub login が `GITHUB_MCP_USER_ALLOWLIST` に登録されている (staging default = `["yhonda-ohishi"]`)

### Build

```bash
cd ~/rust/github-mcp-server-rs
cargo build --release
ln -sf "$(pwd)/target/release/github-mcp-server-rs" ~/.local/bin/  # optional
```

### Staging — 認証 (一度だけ)

```bash
export GITHUB_MCP_INTERNAL_SHARED_SECRET="<staging INTERNAL_SHARED_SECRET>"

./target/release/github-mcp-server-rs auth --env staging
# → ブラウザで verification_uri_complete を開く
# → GitHub OAuth → "認証完了" 画面
# → binary 側: "✓ Token saved to ~/.config/github-mcp-server-rs/token-staging.json"
```

### Staging — token 確認 (whoami)

```bash
./target/release/github-mcp-server-rs whoami --env staging
# → /mcp/introspect で github_token を取り出し、GitHub /user に投げて login を表示
# 期待出力:
#   ✓ Introspect OK:
#     sub:          github:yhonda-ohishi
#     github_login: yhonda-ohishi
#     scope:        mcp.read mcp.write
#   ✓ GitHub /user OK:
#     login: yhonda-ohishi
#     id:    <numeric>
```

### Prod に切り替えるとき

prod 環境が準備済 ([auth-worker issue #97](https://github.com/ippoan/auth-worker/issues/97)) になったら:

```bash
export GITHUB_MCP_INTERNAL_SHARED_SECRET="<prod INTERNAL_SHARED_SECRET>"
./target/release/github-mcp-server-rs auth --env prod
./target/release/github-mcp-server-rs whoami --env prod
```

staging / prod の token cache は別 file (`token-staging.json` / `token-prod.json`) なので
両方並列で持てる。

## Subcommands

| Subcommand | 役割 |
|---|---|
| `auth` | Device flow を実行して token cache に保存 |
| `whoami` | cache 読み → 期限切れなら refresh → introspect で github_token 取得 → GitHub `/user` 確認 |
| `logout` | token cache を削除 |
| `doctor` | 設定 / cache 状況をダンプ (secret 値は出さない) |
| `serve` | MCP server (Streamable HTTP) を起動。Claude Code Web / Claude Code CLI 等の MCP client から `POST /mcp` に接続 |

## MCP server mode

`auth` でログイン済みの状態で `serve` を起動すると、`POST http://<bind>/mcp` で MCP protocol (Streamable HTTP, 2025-06-18 spec) を喋る endpoint が立ち上がる。起動時に `/mcp/introspect` を 1 回叩いて github_token を回収し、in-memory に保持。

```bash
export GITHUB_MCP_INTERNAL_SHARED_SECRET="<staging INTERNAL_SHARED_SECRET>"
./github-mcp-server-rs serve --env staging --bind 127.0.0.1:18765
# ⇒ MCP server listening on http://127.0.0.1:18765/mcp (env=staging)
```

### Tools (MVP)

| Tool | 引数 | 戻り値 |
|---|---|---|
| `whoami` | (なし) | `{ github_login, scope }` |
| `list_repos` | `visibility?` ("all" / "public" / "private")、`per_page?` (1–100)、`page?` (1+) | `{ page, per_page, count, repos: [{ full_name, private, description, html_url, default_branch, language, stargazers_count, pushed_at }] }` |

### Claude Code Web で使う (HTTPS tunnel 経由)

Claude Code Web (claude.ai/code) の MCP connector は **HTTPS な URL** が必要。ローカル `127.0.0.1` は届かないので、`cloudflared` の Quick Tunnel で 1 コマンド公開:

```bash
# (別ターミナル) cloudflared がインストール済みなら:
cloudflared tunnel --url http://127.0.0.1:18765
# → "https://xxx-yyy-zzz.trycloudflare.com" が表示される
```

その後 Claude Code Web の設定で MCP server を追加:

- URL: `https://xxx-yyy-zzz.trycloudflare.com/mcp`
- Transport: Streamable HTTP (default)

接続後、Claude に `whoami` ツールを呼ばせて自分の github_login が返れば成功。

### Allowed hosts

Cloudflare 経由で公開する場合、`Host` header validation を緩めたい時は明示:

```bash
./github-mcp-server-rs serve --env staging \
  --bind 0.0.0.0:18765 \
  --allowed-hosts "localhost,127.0.0.1,xxx-yyy-zzz.trycloudflare.com"
```

(`--allowed-hosts` を省略すると default = `localhost,127.0.0.1,::1` + `--bind` 値。
cloudflared 経由だと `Host` は cloudflare ドメイン名で来るので追加が必要。)

## Global flags

| Flag / env | 役割 | Default |
|---|---|---|
| `--env staging\|prod` | URL preset の切替 | `staging` |
| `--auth-base <URL>` | base URL 任意上書き (wt-quick の `*.trycloudflare.com` 用) | — |
| `--internal-shared-secret <S>` / `GITHUB_MCP_INTERNAL_SHARED_SECRET` | introspect 認証用 secret | — |
| `--client-id <ID>` / `GITHUB_MCP_CLIENT_ID` | device_authorization の client_id | `github-mcp-server-rs` |
| `--scope <S>` | MCP scope | `mcp.read mcp.write` |

`RUST_LOG=debug` で reqwest の詳細ログが出る。

## トラブルシューティング

| 症状 | 原因 / 対処 |
|---|---|
| `auth`: `device_authorization failed: HTTP 503` | auth-worker の `MCP_OAUTH_KV` 等の env / KV binding が未投入。staging なら確認、prod なら #97 手順 |
| ブラウザで approve 後も polling が `authorization_pending` で止まる | GitHub OAuth App の callback URL が staging/prod と一致していない |
| approve 後に「Access denied」HTML | `GITHUB_MCP_USER_ALLOWLIST` に自分の login が無い (fail-closed) |
| `whoami`: `401 — check INTERNAL_SHARED_SECRET` | `--internal-shared-secret` / env が間違い、または staging/prod を取り違え |
| `whoami`: `active:false` | token が revoke / `github_token:{sub}` が KV から TTL 切れ (30d) — `auth` をやり直す |

## アーキテクチャ

```
src/
├── main.rs         — CLI entry (clap)、Auth/Whoami/Logout/Doctor/Serve subcommand
├── config.rs       — env switch (AuthEnv::{Staging,Prod})、URL 組み立て、cache path
├── auth.rs         — RFC 8628 device flow (start + poll + refresh)
├── introspect.rs   — POST /mcp/introspect → github_token 復元
├── token_cache.rs  — ~/.config/.../token-{env}.json への永続化 (0600 perm)
└── mcp_server.rs   — rmcp ServerHandler 実装 + tool_router (whoami / list_repos)
```

## Claude Code on the web から使う (別 repo から install hook 経由)

このリポジトリは、**他のリポジトリ** が Claude Code on the web セッション開始時に
`github-mcp-server-rs` を自動セットアップできる **再利用可能な SessionStart hook**
(`.claude/hooks/install-mcp.sh`) を公開している。

### 仕組み

```
consumer-repo/.claude/hooks/session-start.sh
  └─ curl https://raw.githubusercontent.com/ippoan/github-mcp-server-rs/main/.claude/hooks/install-mcp.sh | bash
       ├─ GitHub Releases から binary を download (latest or GITHUB_MCP_PIN_TAG)
       ├─ cloudflared を download
       ├─ auth (device flow) を実行 — browser で approve
       ├─ serve を 127.0.0.1:18765 で background 起動
       ├─ cloudflared tunnel で公開 URL を取得
       └─ serve を tunnel host を allowed-hosts に追加して再起動
            ⇒ MCP URL (https://xxx.trycloudflare.com/mcp) を
              $GITHUB_MCP_URL & .claude/mcp-state/mcp-url に書き出す
```

### 使い方 (consumer repo 側)

`examples/consumer-claude-hook/` にコピー用のテンプレを置いている。最短手順:

```bash
# consumer repo で実行
mkdir -p .claude/hooks
curl -sSfL https://raw.githubusercontent.com/ippoan/github-mcp-server-rs/main/examples/consumer-claude-hook/.claude/hooks/session-start.sh \
  -o .claude/hooks/session-start.sh
chmod +x .claude/hooks/session-start.sh
curl -sSfL https://raw.githubusercontent.com/ippoan/github-mcp-server-rs/main/examples/consumer-claude-hook/.claude/settings.json \
  -o .claude/settings.json
```

Claude Code on the web 側で secret を 1 つ登録:

- `GITHUB_MCP_INTERNAL_SHARED_SECRET` — auth-worker の `INTERNAL_SHARED_SECRET`

セッション開始 → hook 内で device flow の URL が stderr に出るので、
browser で開いて approve → 自動的に MCP server が立ち上がり、tunnel URL が
hook の最後にプリントされる。その URL を Claude Code (web) → MCP servers
に **Streamable HTTP** transport で登録すれば `whoami` / `list_repos` 等が使える。

### Optional 環境変数 (consumer hook の curl 前に export)

| Env | Default | 用途 |
|---|---|---|
| `GITHUB_MCP_ENV` | `staging` | `staging` or `prod` |
| `GITHUB_MCP_BIND_PORT` | `18765` | local serve port |
| `GITHUB_MCP_PIN_TAG` | latest release | 再現性のため tag pin (例: `v0.0.4`) |

> **Note**: hook は `CLAUDE_CODE_REMOTE=true` のときだけ動く。local Claude Code
> セッションでは no-op。

## 関連

- auth-worker: <https://github.com/ippoan/auth-worker>
- Epic: <https://github.com/ippoan/auth-worker/issues/91>
- Phase 5 (introspect 実装): <https://github.com/ippoan/auth-worker/issues/96>
- RFC 8628: <https://datatracker.ietf.org/doc/html/rfc8628>
- RFC 7662: <https://datatracker.ietf.org/doc/html/rfc7662>

## License

MIT
