# github-mcp-server-rs

GitHub Admin tools (43+ tools) を **Streamable HTTP MCP server** として
`mcp(-staging).ippoan.org` 配下に公開する Rust binary。auth-worker と WS relay で
pair する設計 (issue #27 + #42)。

詳細は `README.md` / `docs/`。ここは Claude session 向けの最小 runbook のみ。

## 認証 (1-click pair flow — CCoW default)

新規 CCoW container で MCP を使えるようにする標準手順。先に **`GITHUB_LOGIN` を
container env に登録** (CCoW Settings → Environment variables) しておくと無人で進む。

```bash
# CCoW 上、または手動で hook を直接叩く時
export CLAUDE_CODE_REMOTE=true                # hook の guard
export GITHUB_LOGIN=<your-github-username>    # 必須 (no-token path)
export CLAUDE_PROJECT_DIR=/path/to/repo       # state dir のベース
export GITHUB_MCP_PIN_TAG=v0.0.16             # regex fix 入り。省略時は latest release
bash .claude/hooks/install-mcp.sh
```

出力に出る `https://auth-staging.ippoan.org/mcp/pair/<40-char-code>` を **ブラウザで開いて 1 click**。
binary 側に `✓ pair: WS upgrade accepted` が出れば成功。

`mcp-state/url` ＝ `mcp-state/mcp-url` に書かれる **per-user URL**
(`https://mcp-staging.ippoan.org/u/<login>/mcp`) を Claude Code Web の MCP entry に登録。

## Gotchas (smoke test 2026-05-18 で実機検証)

- **`tools.listChanged: false`** が auth-worker 側で有効 (`src/handlers/mcp-tools.ts:384`)
  なので、binary が後から WS attach しても Claude Code Web に tool list 更新 push が
  飛ばない。pair 完了後は MCP entry を **1 回「切断 → 再接続」** して tools/list を
  再取得すること。`5 tools (cc-relay stub) のまま増えない` 症状は大抵これ。
- **per-user URL `/u/<login>/mcp` が正**。bare `/mcp` も同じ DO に届くが (ADR-003
  user-less variant)、install-mcp.sh が書き出す per-user URL をそのまま使えば良い。
  URL 変更で tools が増えるわけではない (両方 binary attach 状態に依存)。
- **binary 未起動 = stub 5 tools 固定**。consumer repo の
  `.claude/hooks/session-start.sh` から install-mcp.sh を呼ぶ glue が missing だと、
  Claude Code Web から見て「ずっと 5 tools」になる。
- **`v0.0.15` には install-mcp.sh の regex bug** がある (POST 用 `…/mcp/pair/new`
  URL を pair_url と誤検知)。`v0.0.16` 以降を pin すること。
- **`SESSION_COOKIE_SECRET`** は auth-worker staging/prod 両方に登録が必要
  (browser claim 経路の cookie 署名鍵)。未登録だと `/mcp/pair/<code>` が 503。
  rotation policy: ephemeral、`openssl rand -hex 32 | wrangler secret put …` で再発行可。

## Release / smoke test workflow

| 操作 | コマンド / 参照 |
|---|---|
| 正式 release tag を切る | `/tag-release patch` (= manual workflow_dispatch、`v{major}.{minor}.{patch}`) |
| 開発 release | **自動** — main への src 系 push で `dev-release.yml` が `dev-{N}` を採番して push (counter) |
| Smoke test 実行 | `docs/smoke-tests/2026-05-18-pair-flow.md` の Re-test plan section |
| 結果記録 | `docs/smoke-tests/YYYY-MM-DD-<topic>.md` に `## Re-run YYYY-MM-DD — PASS/FAIL` を追記 |

### 開発 release (dev channel) の使い方

main に binary 関連変更 (`src/**`, `Cargo.{toml,lock}`, `build.rs`, release workflow) が
merge される度に `.github/workflows/dev-release.yml` が走り、`dev-N` タグを 1 個採番して
push する。`release.yml` がそれを拾って GitHub Release を **prerelease** として作成
(タグに `-` を含むので auto prerelease)。`releases/latest` API には出ないので
stable consumer (= default の `GITHUB_MCP_CHANNEL=stable`) は影響を受けない。

dev release を試したい consumer は session 起動前に env を立てる:

```bash
# CCoW Settings → Environment variables, または手動 hook 実行時
export GITHUB_MCP_CHANNEL=dev
bash .claude/hooks/install-mcp.sh
```

install-mcp.sh が `/releases?per_page=100` から `dev-N` の最大 N を解決して
download する。`GITHUB_MCP_PIN_TAG=dev-5` で個別 pin も可能。

## Branch convention

CCoW session 起動時に渡される `claude/<topic>-<token>` ブランチで作業する。
本 repo の最近の例: `claude/add-pair-flow-smoke-test-PBCMu`、
`claude/implement-pair-subcommand-lSOf7`。`Refs #N` で issue 紐付け
(auto-close 抑制、release 時に手動 close)。
