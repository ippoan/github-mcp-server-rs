//! Build script: embed MCP_INTERNAL_SECRET into the release binary so consumers
//! can `curl | bash` the install hook with no secret registration step.
//!
//! The secret is intentionally quasi-public — the true authorization boundary
//! is the JWT signature check inside auth-worker `/mcp/introspect`. See #25.
//!
//! 解決順 (src/main.rs `resolve_internal_secret`):
//!   1. `--internal-shared-secret <S>` (CLI)
//!   2. env `GITHUB_MCP_INTERNAL_SHARED_SECRET`
//!   3. build-time embed `MCP_INTERNAL_SECRET` (← この build.rs が焼き込む)
//!   4. dev fallback `"dev-secret-do-not-use"`

fn main() {
    println!("cargo:rerun-if-env-changed=MCP_INTERNAL_SECRET");
    let value = std::env::var("MCP_INTERNAL_SECRET").unwrap_or_default();
    println!("cargo:rustc-env=MCP_INTERNAL_SECRET={}", value);
}
