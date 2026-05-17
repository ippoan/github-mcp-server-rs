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

    // Embed the release tag when built by GitHub Actions on a tag push
    // (release.yml on `tags: ["v*"]`). install-mcp.sh reads `$BIN --version`
    // and refuses a stale install when the embedded tag disagrees with the
    // resolved release tag — a stronger check than the TAG_FILE bookkeeping
    // added in #39, which can drift if the file is written without a real
    // download. For local `cargo build`, GITHUB_REF_TYPE is unset and the
    // embedded value stays empty, which install-mcp.sh treats as "skip the
    // mismatch guard" so dev builds are not forcibly re-downloaded.
    println!("cargo:rerun-if-env-changed=GITHUB_REF_TYPE");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");
    let release_tag = match (
        std::env::var("GITHUB_REF_TYPE").ok().as_deref(),
        std::env::var("GITHUB_REF_NAME").ok(),
    ) {
        (Some("tag"), Some(name)) => name,
        _ => String::new(),
    };
    println!("cargo:rustc-env=BUILD_RELEASE_TAG={}", release_tag);
}
