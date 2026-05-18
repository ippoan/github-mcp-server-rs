# github-mcp-server-rs

GitHub MCP server (Model Context Protocol) — `auth-worker` の **Device Authorization Grant (RFC 8628)** クライアント実装。
Claude Code / Claude Desktop などの MCP host から GitHub API を叩く用途で、`ippoan/auth-worker` の MCP OAuth Provider と組で動く。

> **Status**: Phase 7 — `relay` subcommand (issue #27) で outbound WebSocket relay 経由で
> Streamable HTTP MCP server を `mcp(-staging).ippoan.org` 配下に公開。auth-worker #117 と pair。

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
- GitHub login が `GITHUB_MCP_USER_ALLOWLIST` に登録されている (staging default = `["yhonda-ohishi"]`)

> `INTERNAL_SHARED_SECRET` は release binary に build-time embed されているので、
> Releases から download した binary を使う場合は何も設定不要。手元で
> `cargo build` する場合の解決順は [Secret resolution order](#secret-resolution-order) 参照。

### Build

```bash
cd ~/rust/github-mcp-server-rs
cargo build --release
ln -sf "$(pwd)/target/release/github-mcp-server-rs" ~/.local/bin/  # optional
```

### Staging — 認証 (一度だけ)

```bash
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
./target/release/github-mcp-server-rs auth --env prod
./target/release/github-mcp-server-rs whoami --env prod
```

staging / prod の token cache は別 file (`token-staging.json` / `token-prod.json`) なので
両方並列で持てる。

## Subcommands

| Subcommand | 役割 |
|---|---|
| `pair`  | **1-click pair flow** (issue #42, default for CCoW)。`POST /mcp/pair/new` → pair_url を stdout に印字 → browser 1 click → `Authorization: Bearer <pair_code>` で WS upgrade → frame bridge loop。token cache を必要としない (CCoW の reclaim でも device-code prompt が出ない) |
| `relay` | MCP server を outbound WebSocket relay 経由で公開 (issue #27)。device-flow で取得した token cache を JWT として WS upgrade に使う。reconnect / refresh あり (long-running session 向け) |
| `auth`  | RFC 8628 Device flow を実行して token cache に保存 (advanced; CLI / local dev / offline) |
| `whoami`| cache 読み → 期限切れなら refresh → introspect で github_token 取得 → GitHub `/user` 確認 |
| `logout`| token cache を削除 |
| `doctor`| 設定 / cache 状況をダンプ (secret 値は出さない) |

## 1-click pair flow (CCoW default, issue #42)

Claude Code on the Web (CCoW) のコンテナは reclaim ごとに `$HOME` が消えるため、
device-flow の token cache (`~/.config/.../token-staging.json`) が毎回消失する。
従来 `install-mcp.sh` は新コンテナで RFC 8628 device-code プロンプトを出していたが、
v0.0.15 から **1-click pair** が default 経路:

```
┌──────────────────────────────────────────────────────────────────────┐
│ 1. session-start hook が `pair` subcommand を nohup で起動              │
│      → POST https://mcp(-staging).ippoan.org/mcp/pair/new              │
│        body = {claim_login: $GITHUB_LOGIN, binary_version: ...}        │
│      ← 200 {pair_code, pair_url, expires_in: 300}                      │
│                                                                      │
│ 2. binary が pair_url を stdout に 1 行印字 (install-mcp.sh が grep)     │
│      → ユーザーは表示された URL をブラウザで開く                          │
│                                                                      │
│ 3. ブラウザ: auth-worker `/mcp/pair/<code>` を踏む (sticky cookie session)│
│      → 未認証なら GitHub OAuth → callback で session を sign + redirect   │
│      → claim_login と session.github_login が一致したら binding_jwt を mint│
│      → KV `mcp/pair/<code>` を status="approved" + binding_jwt に更新   │
│                                                                      │
│ 4. binary は `Authorization: Bearer <pair_code>` で WS upgrade を polling │
│      ← 401 + `Pair-Status: pending` → 2s sleep → retry (最大 5min)      │
│      ← 101 → auth-worker が内部で pair_code を binding_jwt に置換して DO に│
│              forward → frame bridge loop に合流                         │
└──────────────────────────────────────────────────────────────────────┘
```

CCoW の設定 (1 度だけ):

1. **Environment variables** に `GITHUB_LOGIN=<your-github-username>` を追加
   (pair flow の `claim_login` として送信される — secret ではないので KV/Secret-mgr
   には入れずに env 経由で OK)
2. Claude Code Web の **MCP servers** に `https://mcp(-staging).ippoan.org/u/<login>/mcp`
   を 1 度だけ登録 (URL は github_login で固定)

opt-in で旧 device-flow に戻す:

```bash
GITHUB_MCP_AUTO_DEVICE_FLOW=1  # session-start hook が `pair` の代わりに `auth` を呼ぶ
```

`$GITHUB_LOGIN` 未設定で `$GITHUB_MCP_AUTO_DEVICE_FLOW` も `1` でない場合、
install-mcp.sh は明確な error を出して停止する (Settings → Environment variables への
案内付き)。

### Pair mode の制約

`pair` flow は **WS 接続専用**: binding_jwt は auth-worker の内部にのみ存在し、
binary 側には `pair_code` しか渡らない。したがって、

- `tools/list` は 40 tools を返す (router は context state 非依存)
- `whoami` / GitHub API を直接叩く tool は `github_token` 不在で 401
- admin tool (`set/get/delete_branch_protection`) は binary 側 JWT 不在で auth-worker
  `/mcp/admin/exec` が 401

完全な tool 動作のためには:

- `auth` subcommand (RFC 8628 device flow) で token を取得するか
- pre-staged `$GITHUB_MCP_TOKEN_JSON` を CCoW Setup secret に登録するか

のいずれかが必要 (本 issue の out of scope。30-day auto-pair は将来の cycle)。

## MCP server mode (relay)

`v0.0.6+` から、MCP server は **outbound WebSocket relay** で公開する (issue #27、auth-worker #117 と pair)。
旧 `serve` (cloudflared 用 axum bind) は撤廃。

`auth` でログイン済みの状態で `relay` を起動すると:

1. `/mcp/introspect` で github_token + github_login を 1 回回収して in-memory に保持
2. `wss://mcp(-staging).ippoan.org/u/<github_login>/connect` に `Authorization: Bearer <mcp-jwt>` で接続
3. auth-worker `McpSession` Durable Object と長寿命 WS を張る
4. Claude Code Web からの `POST https://mcp(-staging).ippoan.org/u/<github_login>/mcp` は
   auth-worker → DO → WS frame として binary に届き、`StreamableHttpService` (rmcp) で処理されて
   逆経路で返却

```bash
./github-mcp-server-rs relay --env staging
# → MCP relay starting (env=staging, user=yhonda-ohishi)
# → MCP relay: connecting to wss://mcp-staging.ippoan.org/u/yhonda-ohishi/connect as yhonda-ohishi
```

### 公開 URL

- Staging: `https://mcp-staging.ippoan.org/u/<github_login>/mcp`
- Prod: `https://mcp.ippoan.org/u/<github_login>/mcp`

URL は **github_login で固定**。Claude Code Web の MCP 設定には **1 度だけ**登録すれば
セッションをまたいで使える。

### Tools

すべての repo 引数は `"owner/name"` または `"name"` (`name` 単独なら `ippoan` を補完)。
`owner` は allowlist (`ippoan` / `ohishi-exp` / `yhonda-ohishi`) 配下のみ許可、それ以外は
403 `Org not allowed` で拒否される。

#### Core

| Tool | 引数 | 戻り値 |
|---|---|---|
| `whoami` | (なし) | `{ github_login, scope }` |
| `list_repos` | `visibility?` ("all" / "public" / "private")、`per_page?` (1–100)、`page?` (1+) | `{ page, per_page, count, repos: [...] }` |

#### Actions (Workflow runs / jobs)

| Tool | 引数 | 戻り値 |
|---|---|---|
| `list_workflow_runs` | `repo`, `status?` ("queued"/"in_progress"/"completed"), `per_page?` (1–100, default 10) | `[{ id, name, status, conclusion, branch, actor, created_at, updated_at, url }]` |
| `get_workflow_run` | `repo`, `run_id` | 上記 + `run_attempt` |
| `list_workflow_run_jobs` | `repo`, `run_id` | `[{ id, name, status, conclusion, started_at, completed_at, url }]` |
| `rerun_workflow_run` (write) | `repo`, `run_id` | `Rerun triggered for run N` |
| `rerun_failed_jobs` (write) | `repo`, `run_id` | `Rerun of failed jobs triggered for run N` |
| `cancel_workflow_run` (write) | `repo`, `run_id` | `Cancelled run N` |

#### Commits

| Tool | 引数 | 戻り値 |
|---|---|---|
| `list_commits` | `repo`, `sha?` (branch/tag/sha, default "main"), `path?`, `per_page?` (1–100, default 20) | `[{ sha (short), message (1行目), author, date }]` |
| `get_commit` | `repo`, `sha` | `{ sha, message, author, date, stats, files: [{ filename, status, additions, deletions, patch (500行で truncate) }] }` |

#### Issues

| Tool | 引数 | 戻り値 |
|---|---|---|
| `list_issues` | `repo`, `state?` ("open"/"closed"/"all", default "open"), `labels?` (comma-sep), `per_page?` (1–100, default 20) | `[{ number, title, state, author, labels, created_at, updated_at, comments, url }]` (PR 除外) |
| `get_issue` | `repo`, `issue_number` | 上記 + `body` + `comments: [{ author, created_at, body }]` |
| `list_org_issues` | `orgs[]`, `state?`, `labels?[]`, `assignee?` (`@me` 可), `query?` (raw GitHub search), `per_page?` (1–100, default 30) | `{ total_count, incomplete, items: [{ repo, number, title, state, author, labels, assignees, comments, created_at, updated_at, url }] }` |
| `create_issue` (write) | `repo`, `title`, `body?`, `labels?[]`, `assignees?[]` | `{ number, title, state, url }` |
| `update_issue` (write) | `repo`, `issue_number`, `title?`/`body?`/`labels?[]`/`assignees?[]`/`milestone?` (number\|null) — 最低 1 つ必須 | `{ number, title, state, labels, url }` |
| `add_issue_comment` (write) | `repo`, `issue_number`, `body` | `{ id, url, created_at }` |
| `add_labels` (write) | `repo`, `issue_number`, `labels[]` (非空) | `[label_name]` (最新のラベル一覧) |
| `remove_label` (write) | `repo`, `issue_number`, `label` (UTF-8 path-encode 対応) | `[label_name]` (残りのラベル一覧) |
| `close_issue` (write) | `repo`, `issue_number`, `state_reason?` ("completed"/"not_planned"、default "completed") | `{ number, state, state_reason, url }` |
| `reopen_issue` (write) | `repo`, `issue_number` | `{ number, state, url }` |

#### Logs (Workflow job logs)

| Tool | 引数 | 戻り値 |
|---|---|---|
| `get_job_logs` | `repo`, `job_id`, `tail_lines?` (1–1000, default 200), `start_line?`/`end_line?` (range 指定時 tail_lines 無視) | テキスト (`Lines X-Y of N\n\n1: ...\n2: ...`) |
| `grep_job_logs` | `repo`, `job_id`, `pattern` (regex, case-insensitive), `context_lines?` (0–20, default 3) | `N matches for /p/i in M lines\n\n> マッチ行 / 周辺` (最大 50 match) |

#### Pull requests

| Tool | 引数 | 戻り値 |
|---|---|---|
| `list_pull_requests` | `repo`, `state?` (default "open"), `per_page?` (1–100, default 10) | `[{ number, title, state, author, branch, base, created_at, updated_at, url, draft, mergeable_state }]` |
| `get_pull_request` | `repo`, `pull_number` | 上記 + `mergeable, additions, deletions, changed_files, checks: [{ name, status, conclusion, url }]` |
| `merge_pull_request` (write) | `repo`, `pull_number`, `commit_title?` | `PR #N merged (squash)` |

#### Releases / Tags

| Tool | 引数 | 戻り値 |
|---|---|---|
| `list_tags` | `repo`, `per_page?` (1–100, default 10) | `[{ name, sha (short) }]` |
| `get_latest_release` | `repo` | `{ tag, name, published_at, author, url, body (500文字 snippet) }` |
| `create_tag_release` (write) | `repo` (`tag-release.yml` 必須) | `tag-release dispatched for owner/name` |

#### Projects v2 (GraphQL)

GitHub Projects v2 は REST surface が無く、すべて GraphQL。`repositoryOwner(login:)`
+ `Organization` / `User` inline fragment で user account login (`yhonda-ohishi` 等) も
動くようにしてある。書込み系は内部で `project number → projectId` / `issue number → contentId` /
`field name → fieldId` を resolve するので、ユーザは node ID を直接扱わなくて良い。

| Tool | 引数 | 戻り値 |
|---|---|---|
| `list_org_projects` | `orgs[]` (allowlist), `first?` (1–100, default 50), `include_closed?` (default false) | `[{ org, projects: [{ number, title, url, closed, shortDescription }] }]` |
| `get_project` | `org`, `number` | `{ id, number, title, url, closed, shortDescription, fields: [{ id, name, dataType, options?, iterations? }] }` |
| `list_project_items` | `org`, `number`, `first?` (1–100, default 50) | `[{ item_id, item_type, content: { type, repo, number, title, state, url }, fields: { 名前: 値 } }]` |
| `add_issue_to_project` (write) | `org`, `project_number`, `repo`, `issue_number` | `{ item_id, project_id, content_id, repo, issue_number }` |
| `remove_project_item` (write) | `org`, `project_number`, `item_id` | `{ deleted_item_id }` |
| `set_project_item_field` (write) | `org`, `project_number`, `item_id`, `field_name`, `value` (string/number/null) | `{ item_id, field, dataType, value }`、null clear 時は `{ ..., cleared: true }` |
| `create_project_field` (write) | `org`, `project_number`, `name`, `data_type` ("text"/"number"/"date"/"single_select"), `single_select_options?[]` | `{ field: { __typename, id, name, dataType, options? } }` |
| `create_project` (write) | `org`, `title`, `short_description?` (2 段階 mutation、後段失敗で `warning` 同梱) | `{ id, number, title, url, shortDescription, warning? }` |

#### Repository (file tree / content / code search)

| Tool | 引数 | 戻り値 |
|---|---|---|
| `get_file_tree` | `repo`, `ref?` (default "main"), `path?` (prefix filter) | `N entries\n\nf src/...\nd dir/...` |
| `get_file_content` | `repo`, `path`, `ref?`, `start_line?`/`end_line?` | ファイル → 行番号付き本文 / ディレクトリ → entry リスト |
| `search_code` | `repo`, `query`, `path?`, `extension?`, `per_page?` (1–100, default 20) | `N matches\n\n## path\n<text-match fragment>` |
| `search_symbols` | `repo`, `symbol`, `kind?` ("function"/"class"/"struct"/"interface"/"type"/"enum"/"trait"/"mod"), `language?`, `per_page?` (1–50, default 10) | `search_code` と同形式 |

### `--state-dir` (install hook 連携用)

```bash
./github-mcp-server-rs relay --env staging --state-dir /tmp/mcp
# → /tmp/mcp/url に "https://mcp-staging.ippoan.org/u/<login>/mcp" を即時書き出し
```

`install-mcp.sh` (`.claude/hooks/install-mcp.sh`) はこの file を待って `$GITHUB_MCP_URL`
に export する。

### Architecture (relay flow)

```
[Claude Code Web]
       │  HTTPS POST  https://mcp(-staging).ippoan.org/u/<login>/mcp
       ▼
[auth-worker (Workers)]
       │  routes to DO by <login>
       ▼
[Durable Object: McpSession]
       │  WebSocket frame (JSON: Frame::Req / Resp)
       ▼
[github-mcp-server-rs binary (outbound WS only)]
   │  bridge.rs → StreamableHttpService<GithubMcp>
   │  (rmcp tool_router: whoami, list_repos)
   ▼
[GitHub API]
```

### Reconnect / JWT refresh

- WS が closed (network / auth-worker 再起動) → exponential backoff (1s → 2s → … 最大 30s) で再接続
- handshake が `401` を返したら `/mcp/token` (refresh_token grant) で JWT を更新して再接続
- refresh 自体が失敗 (= refresh_token も失効) したら fatal exit。`auth` を再実行する

## Global flags

| Flag / env | 役割 | Default |
|---|---|---|
| `--env staging\|prod` | URL preset の切替 | `staging` |
| `--auth-base <URL>` | base URL 任意上書き (wt-quick の `*.trycloudflare.com` 用) | — |
| `--internal-shared-secret <S>` / `GITHUB_MCP_INTERNAL_SHARED_SECRET` | introspect 認証用 secret の **override** (通常は build-time embed で足りる、[Secret resolution order](#secret-resolution-order) 参照) | build-time embed |
| `--client-id <ID>` / `GITHUB_MCP_CLIENT_ID` | device_authorization の client_id | `github-mcp-server-rs` |
| `--scope <S>` | MCP scope | `mcp.read mcp.write` |

`RUST_LOG=debug` で reqwest の詳細ログが出る。

## Secret resolution order

`internal_shared_secret` (`/mcp/introspect` の `Authorization` header に乗る値) は
以下の順で解決される:

1. `--internal-shared-secret <S>` (CLI flag) — 明示 override
2. env `GITHUB_MCP_INTERNAL_SHARED_SECRET` — env override (staging で別値を使いたい等)
3. **build-time embed** `MCP_INTERNAL_SECRET` — release binary に焼き込み済み (`build.rs` 経由、 `option_env!()` で読み込み)
4. dev fallback `"dev-secret-do-not-use"` — どれも空のときの最終手段 (本物 auth-worker は 401 を返す)

### なぜ build embed なのか (quasi-public secret)

この secret は **intentionally quasi-public**。release binary を public にすると
`strings` で読めるが、それで何もできないように設計してある。本当の認可境界は
auth-worker `/mcp/introspect` 内の **JWT 署名検証** であり、shared secret 単体では
github_token は引き出せない (RFC 7662 §2.1 の resource-server ↔ authz-server 境界
のうち、resource-server 側 client auth)。詳細は
[#25](https://github.com/ippoan/github-mcp-server-rs/issues/25) 参照。

これにより consumer (Claude Code on Web ユーザ) が自前で secret を登録する手間が
消える (= `curl | bash` で完結する)。

## Local development

`cargo run` / `cargo build` を `MCP_INTERNAL_SECRET` env 無しで実行すると、
build embed が空文字 → 上記順 4 の dev fallback (`"dev-secret-do-not-use"`) が
使われる。本物の auth-worker (staging / prod) はこれを 401 で拒否するので、
ローカルで本物に当てたいときは:

```bash
# A) 本物の staging に当てる: env で override
GITHUB_MCP_INTERNAL_SHARED_SECRET="<staging value>" \
  cargo run --release -- whoami --env staging

# B) ローカル auth-worker (wt-quick / Incus 等) に当てる
#    auth-worker 側の INTERNAL_SHARED_SECRET も "dev-secret-do-not-use" に
#    揃えると zero-config で通る
cargo run --release -- whoami --env staging \
  --auth-base https://xxx.trycloudflare.com

# C) release binary 相当の build を手元で再現する
MCP_INTERNAL_SECRET="<staging value>" cargo build --release
./target/release/github-mcp-server-rs doctor
# → internal_secret: (set, N chars)
```

## トラブルシューティング

| 症状 | 原因 / 対処 |
|---|---|
| `auth`: `device_authorization failed: HTTP 503` | auth-worker の `MCP_OAUTH_KV` 等の env / KV binding が未投入。staging なら確認、prod なら #97 手順 |
| ブラウザで approve 後も polling が `authorization_pending` で止まる | GitHub OAuth App の callback URL が staging/prod と一致していない |
| approve 後に「Access denied」HTML | `GITHUB_MCP_USER_ALLOWLIST` に自分の login が無い (fail-closed) |
| `whoami`: `401 — check INTERNAL_SHARED_SECRET` | (a) `doctor` の `internal_secret: (set, N chars)` を見て **N が 21 なら dev fallback** = embed が無い古い binary か `cargo run` で env 未指定。`v0.0.5+` の release binary を取り直す、または `MCP_INTERNAL_SECRET` 付きで `cargo build` (`Local development` 参照)、(b) staging/prod を取り違えていないか |
| `whoami`: `active:false` | token が revoke / `github_token:{sub}` が KV から TTL 切れ (30d) — `auth` をやり直す |
| `relay`: handshake が 401 で繰り返し reject (`auth rejected (401), refreshing JWT` ループ) | `MCP_JWT_SECRET` が auth-worker と binary 解釈の env で揃っていない、もしくは refresh_token も失効。`logout` → `auth` で初期化 |
| `relay`: `network: ws connect: ...` が backoff し続ける | `mcp.ippoan.org` / `mcp-staging.ippoan.org` の DNS / TLS 問題。`curl -v https://mcp(-staging).ippoan.org/u/<login>/mcp` で疎通確認 |
| `install-mcp.sh`: `relay did not produce $STATE_DIR/url within 30s.` | binary が起動失敗。`tail -n 50 $STATE_DIR/relay.log` を確認 (token 不足 / introspect 失敗 が大半) |
| `install-mcp.sh`: `installed binary does not support 'relay' subcommand` | `GITHUB_MCP_PIN_TAG` が v0.0.5 以下 (cloudflared 時代の binary)。pin を `v0.0.6+` に bump する |

## アーキテクチャ

```
src/
├── main.rs         — CLI entry (clap)、Auth/Whoami/Logout/Doctor/Relay subcommand
├── config.rs       — env switch (AuthEnv::{Staging,Prod})、URL 組み立て、cache path、relay_base
├── auth.rs         — RFC 8628 device flow (start + poll + refresh)
├── introspect.rs   — POST /mcp/introspect → github_token 復元
├── token_cache.rs  — ~/.config/.../token-{env}.json への永続化 (0600 perm)
├── github_api.rs   — GitHub REST/Search/GraphQL 共通ヘルパー (parse_repo / validate_org / github_api_json / github_api_raw / github_graphql)
├── mcp_server.rs   — rmcp ServerHandler 実装 + core tool_router (whoami / list_repos) + 各 category router 合成
├── tools/          — ci-dashboard 由来の category 別ツール群 (issue #35)
│   ├── actions.rs    — workflow runs / jobs (list/get + rerun/rerun_failed_jobs/cancel)
│   ├── commits.rs    — commit list / detail
│   ├── issues.rs     — list / get / list_org_issues (search-backed, PR 除外) / create / update / comment / labels / close / reopen
│   ├── logs.rs       — get_job_logs (tail/range) / grep_job_logs (regex + context)
│   ├── projects.rs   — Projects v2 (GraphQL): list_org_projects / get_project / list_project_items / add_issue_to_project / remove_project_item / set_project_item_field / create_project_field / create_project
│   ├── pulls.rs      — list / get (check-runs 込み) / merge_pull_request
│   ├── releases.rs   — list_tags / get_latest_release / create_tag_release
│   └── repository.rs — get_file_tree / get_file_content / search_code / search_symbols
└── relay/
    ├── mod.rs      — outbound WS client + reconnect + JWT refresh (issue #27)
    ├── frame.rs    — WS frame schema (Req / Resp / Hello, JSON over Text frame)
    └── bridge.rs   — Frame ↔ axum::Request/Response ↔ tower::Service dispatch
```

各 tool module は `#[tool_router(router = X_router, vis = "pub(crate)")] impl GithubMcp { ... }`
で `Self::X_router()` 形式の inherent fn を生やし、`mcp_server.rs::GithubMcp::new` で
`+` operator (`rmcp::ToolRouter: Add`) で合成される。新 category を増やす場合:

1. `src/tools/<name>.rs` を作って `#[tool_router(router = <name>_router, vis = "pub(crate)")]` を貼る
2. `src/tools/mod.rs` に `pub mod <name>;` を追加
3. `src/mcp_server.rs::GithubMcp::new` の `+` chain に `Self::<name>_router()` を足す
4. `tests/relay_smoke.rs` の `#[path]` mod は触らなくて OK (`tools::*` は parent mod 経由で見える)

## Claude Code on the web から使う (別 repo から install hook 経由)

このリポジトリは、**他のリポジトリ** が Claude Code on the web セッション開始時に
`github-mcp-server-rs` を自動セットアップできる **再利用可能な SessionStart hook**
(`.claude/hooks/install-mcp.sh`) を公開している。

### 仕組み (v0.0.6+)

```
consumer-repo/.claude/hooks/session-start.sh
  └─ curl https://raw.githubusercontent.com/ippoan/github-mcp-server-rs/main/.claude/hooks/install-mcp.sh | bash
       ├─ GitHub Releases から binary を download (latest or GITHUB_MCP_PIN_TAG)
       │   ※ v0.0.5+ binary は INTERNAL_SHARED_SECRET を build-time embed 済 (#25)
       │   ※ v0.0.6+ binary は relay subcommand を持つ (#27)
       ├─ auth (device flow) を初回だけ実行 — browser で approve
       ├─ relay を background 起動 (outbound WS to mcp(-staging).ippoan.org)
       └─ relay が <state-dir>/url に固定 URL を書き出すのを待つ
            ⇒ MCP URL (https://mcp(-staging).ippoan.org/u/<login>/mcp) を
              $GITHUB_MCP_URL & .claude/mcp-state/mcp-url に書き出す
```

cloudflared は v0.0.6+ で **撤廃** (issue #27)。URL が固定になったので Claude Code Web 側
登録は **1 度だけ**。

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

**Claude Code Web 側で secret 登録は不要** (`v0.0.5+` から
`INTERNAL_SHARED_SECRET` は release binary に build-time embed 済 — [#25](https://github.com/ippoan/github-mcp-server-rs/issues/25))。

セッション開始 → hook 内で device flow の URL が stderr に出るので、
browser で開いて approve → 自動的に MCP server が立ち上がり、tunnel URL が
hook の最後にプリントされる。その URL を Claude Code (web) → MCP servers
に **Streamable HTTP** transport で登録すれば `whoami` / `list_repos` 等が使える。

### Optional 環境変数 (consumer hook の curl 前に export)

| Env | Default | 用途 |
|---|---|---|
| `GITHUB_MCP_ENV` | `staging` | `staging` or `prod` |
| `GITHUB_MCP_PIN_TAG` | latest release | 再現性のため tag pin。**`v0.0.6` 以上**を指定すること (それ以前は relay subcommand が無いので install-mcp.sh が fail する) |
| `GITHUB_MCP_INTERNAL_SHARED_SECRET` | (embed) | advanced: embed されている secret を上書きしたい時のみ (例: 自分の auth-worker fork に当てる dev 用途) |

> **Note**: hook は `CLAUDE_CODE_REMOTE=true` のときだけ動く。local Claude Code
> セッションでは no-op。
> 旧 `GITHUB_MCP_BIND_PORT` env (cloudflared 用 local port) は v0.0.6+ で **撤廃**。

## 関連

- auth-worker: <https://github.com/ippoan/auth-worker>
- Epic: <https://github.com/ippoan/auth-worker/issues/91>
- Phase 5 (introspect 実装): <https://github.com/ippoan/auth-worker/issues/96>
- RFC 8628: <https://datatracker.ietf.org/doc/html/rfc8628>
- RFC 7662: <https://datatracker.ietf.org/doc/html/rfc7662>

## License

MIT
