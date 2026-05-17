//! ci-dashboard MCP ツール群を category 別に Rust 移植したもの。
//!
//! 各 module は `#[tool_router(router = X_router, vis = "pub(crate)")]` で
//! `GithubMcp` の inherent fn を生やし、`mcp_server.rs::GithubMcp::new` で
//! `+` operator で合成される (rmcp::ToolRouter は `Add` を実装)。

pub mod actions;
pub mod branches;
pub mod commits;
pub mod issues;
pub mod logs;
pub mod projects;
pub mod pulls;
pub mod releases;
pub mod repository;
