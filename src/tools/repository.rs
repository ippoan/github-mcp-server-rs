//! Repository (file tree / content / code search / symbol search) —
//! ci-dashboard `src/mcp/tools/repository.ts` 移植。

use base64::Engine;
use reqwest::Method;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::github_api::{github_api_json, parse_and_validate_repo};
use crate::mcp_server::GithubMcp;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFileTreeArgs {
    /// Repository (e.g. 'rust-alc-api' or 'ippoan/rust-alc-api').
    pub repo: String,
    /// Branch, tag, or commit SHA (default: "main").
    #[serde(default, rename = "ref")]
    pub ref_: Option<String>,
    /// Filter to paths starting with this prefix (e.g. 'src/routes').
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetFileContentArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// File path (e.g. 'src/main.rs').
    pub path: String,
    /// Branch, tag, or commit SHA.
    #[serde(default, rename = "ref")]
    pub ref_: Option<String>,
    /// Start line (1-based).
    #[serde(default)]
    pub start_line: Option<u32>,
    /// End line (inclusive).
    #[serde(default)]
    pub end_line: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchCodeArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Search query (e.g. 'handleWebhook', 'TODO').
    pub query: String,
    /// Path filter (e.g. 'src/routes').
    #[serde(default)]
    pub path: Option<String>,
    /// File extension filter (e.g. 'ts', 'rs').
    #[serde(default)]
    pub extension: Option<String>,
    /// Results per page (1–100, default 20).
    #[serde(default)]
    pub per_page: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchSymbolsArgs {
    /// Repository (e.g. 'rust-alc-api').
    pub repo: String,
    /// Symbol name to search for.
    pub symbol: String,
    /// Symbol kind: "function" | "class" | "struct" | "interface" | "type" | "enum" | "trait" | "mod".
    /// Builds language-aware query (e.g. 'fn' for Rust function).
    #[serde(default)]
    pub kind: Option<String>,
    /// Language filter (e.g. 'rust', 'typescript').
    #[serde(default)]
    pub language: Option<String>,
    /// Results per page (1–50, default 10).
    #[serde(default)]
    pub per_page: Option<u32>,
}

fn symbol_keyword(kind: &str, language: Option<&str>) -> String {
    let lang = language.map(|s| s.to_lowercase());
    let lang = lang.as_deref();
    match kind {
        "function" => match lang {
            Some("rust") => "fn",
            Some("python") => "def",
            Some("go") => "func",
            _ => "function",
        },
        "class" => "class",
        "struct" => "struct",
        "interface" => match lang {
            Some("go") => "type",
            _ => "interface",
        },
        "type" => "type",
        "enum" => "enum",
        "trait" => "trait",
        "mod" => match lang {
            Some("rust") => "mod",
            Some("python") => "import",
            _ => "module",
        },
        other => return other.to_string(),
    }
    .to_string()
}

fn format_search_results(data: &serde_json::Value, header: &str) -> String {
    let items: Vec<String> = data
        .get("items")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|item| {
                    let path = item.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    let fragments: Vec<String> = item
                        .get("text_matches")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|m| m.get("fragment").and_then(|v| v.as_str()))
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default();
                    let body = if fragments.is_empty() {
                        "(no text preview)".to_string()
                    } else {
                        fragments.join("\n---\n")
                    };
                    format!("## {path}\n{body}")
                })
                .collect()
        })
        .unwrap_or_default();
    format!("{header}\n\n{}", items.join("\n\n"))
}

#[tool_router(router = repository_router, vis = "pub(crate)")]
impl GithubMcp {
    /// Get the file tree of a repository.
    #[tool(
        description = "Get the file tree of a repository. Use path filter to scope to a subdirectory."
    )]
    async fn get_file_tree(
        &self,
        Parameters(args): Parameters<GetFileTreeArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let ref_ = args.ref_.unwrap_or_else(|| "main".to_string());
        let data: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &format!("/repos/{}/{}/git/trees/{}", r.owner, r.repo, ref_),
            &[("recursive", "1".to_string())],
            None,
            &[],
        )
        .await?;
        let truncated = data
            .get("truncated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut items: Vec<(String, String, Option<i64>)> = data
            .get("tree")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|t| {
                        let kind = if t.get("type").and_then(|v| v.as_str()) == Some("blob") {
                            "file"
                        } else {
                            "dir"
                        };
                        let path = t
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let size = t.get("size").and_then(|v| v.as_i64());
                        (kind.to_string(), path, size)
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(prefix) = args.path.as_deref() {
            let with_slash = if prefix.ends_with('/') {
                prefix.to_string()
            } else {
                format!("{prefix}/")
            };
            items.retain(|(_, p, _)| p.starts_with(&with_slash) || p == prefix);
        }
        let mut header = format!("{} entries", items.len());
        if truncated {
            header.push_str(" (truncated — repo too large for full tree)");
        }
        let body: Vec<String> = items
            .iter()
            .map(|(kind, path, size)| {
                let prefix = if kind == "dir" { "d" } else { "f" };
                match size {
                    Some(s) => format!("{prefix} {path} ({s}B)"),
                    None => format!("{prefix} {path}"),
                }
            })
            .collect();
        Ok(CallToolResult::success(vec![Content::text(format!(
            "{header}\n\n{}",
            body.join("\n")
        ))]))
    }

    /// Get file content with optional line range. For directories, returns the entry listing.
    #[tool(
        description = "Get file content with optional line range. For directories, returns the entry listing."
    )]
    async fn get_file_content(
        &self,
        Parameters(args): Parameters<GetFileContentArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(ref_) = args.ref_ {
            params.push(("ref", ref_));
        }
        let data: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            &format!("/repos/{}/{}/contents/{}", r.owner, r.repo, args.path),
            &params,
            None,
            &[],
        )
        .await?;

        // Directory listing comes back as an array.
        if let Some(arr) = data.as_array() {
            let entries: Vec<String> = arr
                .iter()
                .map(|e| {
                    let kind = if e.get("type").and_then(|v| v.as_str()) == Some("dir") {
                        "d"
                    } else {
                        "f"
                    };
                    let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let size = e.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
                    if size > 0 {
                        format!("{kind} {name} ({size}B)")
                    } else {
                        format!("{kind} {name}")
                    }
                })
                .collect();
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "Directory: {}\n{} entries\n\n{}",
                args.path,
                entries.len(),
                entries.join("\n")
            ))]));
        }

        let type_ = data.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if type_ == "symlink" || type_ == "submodule" {
            let target = data
                .get("target")
                .or_else(|| data.get("submodule_git_url"))
                .or_else(|| data.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "{type_}: {target}"
            ))]));
        }

        let Some(content_b64) = data.get("content").and_then(|v| v.as_str()) else {
            let size = data.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "File too large ({size}B). Use git blob API for files > 1MB."
            ))]));
        };
        let stripped: String = content_b64.chars().filter(|c| *c != '\n').collect();
        let decoded_bytes = base64::engine::general_purpose::STANDARD
            .decode(stripped.as_bytes())
            .map_err(|e| rmcp::ErrorData::internal_error(format!("base64 decode: {e}"), None))?;
        let decoded = String::from_utf8_lossy(&decoded_bytes).into_owned();
        let lines: Vec<&str> = decoded.split('\n').collect();
        let total = lines.len();

        let (selected, header, start_num) = if let Some(start) = args.start_line {
            let start = start.max(1) as usize;
            let end = args
                .end_line
                .map(|e| (e as usize).min(total))
                .unwrap_or(total);
            let slice = if start <= total {
                lines[start - 1..end].to_vec()
            } else {
                Vec::new()
            };
            let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("");
            (
                slice,
                format!("{name} — Lines {}-{} of {}", start, end.min(total), total),
                start,
            )
        } else {
            let name = data.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let size = data.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
            (
                lines.clone(),
                format!("{name} — {total} lines ({size}B)"),
                1,
            )
        };
        let numbered: Vec<String> = selected
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}: {}", start_num + i, line))
            .collect();
        Ok(CallToolResult::success(vec![Content::text(format!(
            "{header}\n\n{}",
            numbered.join("\n")
        ))]))
    }

    /// Search code in a repository (grep-like). Returns matching files with text fragments.
    #[tool(
        description = "Search code in a repository (grep-like). Returns matching files with text fragments."
    )]
    async fn search_code(
        &self,
        Parameters(args): Parameters<SearchCodeArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let per_page = args.per_page.unwrap_or(20).clamp(1, 100);
        let mut q = format!("{} repo:{}/{}", args.query, r.owner, r.repo);
        if let Some(p) = args.path {
            q.push_str(&format!(" path:{p}"));
        }
        if let Some(ext) = args.extension {
            q.push_str(&format!(" extension:{ext}"));
        }
        let data: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            "/search/code",
            &[("q", q), ("per_page", per_page.to_string())],
            None,
            &[("Accept", "application/vnd.github.text-match+json")],
        )
        .await?;
        let total = data
            .get("total_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let shown = data
            .get("items")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let header = format!("{total} total matches, showing {shown}");
        Ok(CallToolResult::success(vec![Content::text(
            format_search_results(&data, &header),
        )]))
    }

    /// Search for symbol definitions (functions, classes, structs, types). LSP-like definition finder.
    #[tool(
        description = "Search for symbol definitions (functions, classes, structs, types) in a repository. LSP-like definition finder."
    )]
    async fn search_symbols(
        &self,
        Parameters(args): Parameters<SearchSymbolsArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let r = parse_and_validate_repo(&args.repo)?;
        let per_page = args.per_page.unwrap_or(10).clamp(1, 50);
        let keyword = args
            .kind
            .as_deref()
            .map(|k| symbol_keyword(k, args.language.as_deref()))
            .unwrap_or_default();
        let quoted = if keyword.is_empty() {
            format!("\"{}\"", args.symbol)
        } else {
            format!("\"{keyword} {}\"", args.symbol)
        };
        let mut q = format!("{quoted} repo:{}/{}", r.owner, r.repo);
        if let Some(lang) = args.language.as_deref() {
            q.push_str(&format!(" language:{lang}"));
        }
        let data: serde_json::Value = github_api_json(
            &self.ctx().client,
            &self.ctx().github_token,
            Method::GET,
            "/search/code",
            &[("q", q), ("per_page", per_page.to_string())],
            None,
            &[("Accept", "application/vnd.github.text-match+json")],
        )
        .await?;
        let total = data
            .get("total_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let header = format!(
            "{total} matches for {} \"{}\"",
            args.kind.as_deref().unwrap_or("symbol"),
            args.symbol
        );
        Ok(CallToolResult::success(vec![Content::text(
            format_search_results(&data, &header),
        )]))
    }
}
