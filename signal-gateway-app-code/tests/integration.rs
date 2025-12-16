//! Integration tests for signal-gateway-app-code.
//!
//! These tests exercise the GitHub tarball download and file browsing functionality
//! against a real public repository at a pinned commit.

use signal_gateway_app_code::{AppCode, AppCodeConfig, GitHubRepo, ShaCallback};
use std::sync::Arc;

/// Test against cbeck88/ver-stub-rs at a known commit.
/// This is a public repo, so no token is needed.
const TEST_OWNER: &str = "cbeck88";
const TEST_REPO: &str = "ver-stub-rs";
const TEST_SHA: &str = "79b98e25f27ae4f5dd73a5a3d8f37dad655a57e8";

fn create_test_app_code() -> AppCode {
    let config = AppCodeConfig {
        name: "test-app".to_string(),
        github: GitHubRepo {
            owner: TEST_OWNER.to_string(),
            repo: TEST_REPO.to_string(),
        },
        token_file: None, // Public repo, no auth needed
    };

    let sha = TEST_SHA.to_string();
    let sha_callback: ShaCallback = Arc::new(move || {
        let sha = sha.clone();
        Box::pin(async move { Ok(sha) })
    });

    AppCode::new(config, sha_callback).expect("Failed to create AppCode")
}

#[tokio::test]
async fn test_ls_root() {
    let app = create_test_app_code();

    let result = app.ls(None).await.expect("ls failed");

    // Verify expected top-level entries exist
    assert!(result.contains("Cargo.toml"), "Should contain Cargo.toml");
    assert!(result.contains("README.md"), "Should contain README.md");
    assert!(result.contains("ver-stub/"), "Should contain ver-stub/ directory");
    assert!(result.contains("ver-stub-build/"), "Should contain ver-stub-build/ directory");
    assert!(result.contains("tests.sh"), "Should contain tests.sh");
}

#[tokio::test]
async fn test_ls_subdirectory() {
    let app = create_test_app_code();

    let result = app.ls(Some("ver-stub")).await.expect("ls failed");

    // Should have src/ directory and Cargo.toml
    assert!(result.contains("src/"), "Should contain src/ directory");
    assert!(result.contains("Cargo.toml"), "Should contain Cargo.toml");
}

#[tokio::test]
async fn test_find_rust_files() {
    let app = create_test_app_code();

    let result = app.find(Some("*.rs")).await.expect("find failed");

    // Should find Rust source files
    assert!(result.contains("ver-stub/src/lib.rs"), "Should find ver-stub/src/lib.rs");
    assert!(result.contains("ver-stub-build/src/lib.rs"), "Should find ver-stub-build/src/lib.rs");
}

#[tokio::test]
async fn test_find_with_path_pattern() {
    let app = create_test_app_code();

    let result = app.find(Some("ver-stub-build/src/*.rs")).await.expect("find failed");

    // Should find files in ver-stub-build/src/
    assert!(result.contains("ver-stub-build/src/lib.rs"), "Should find lib.rs");
}

#[tokio::test]
async fn test_read_cargo_toml() {
    let app = create_test_app_code();

    let result = app.read("Cargo.toml", None, None).await.expect("read failed");

    // Verify content matches what we know is in the file
    assert!(result.contains("[workspace]"), "Should contain [workspace]");
    assert!(result.contains("ver-stub"), "Should contain ver-stub");
    assert!(result.contains("ver-stub-build"), "Should contain ver-stub-build");
}

#[tokio::test]
async fn test_read_with_line_range() {
    let app = create_test_app_code();

    // Read just the first 5 lines
    let result = app.read("Cargo.toml", Some(1), Some(5)).await.expect("read failed");

    // Should only have 5 lines
    let line_count = result.lines().count();
    assert_eq!(line_count, 5, "Should have exactly 5 lines");

    // First line should be [workspace]
    assert!(result.contains("[workspace]"), "First lines should contain [workspace]");
}

#[tokio::test]
async fn test_read_nonexistent_file() {
    let app = create_test_app_code();

    let result = app.read("nonexistent-file.txt", None, None).await;

    assert!(result.is_err(), "Should fail for nonexistent file");
    assert!(
        result.unwrap_err().contains("not found"),
        "Error should mention file not found"
    );
}

#[tokio::test]
async fn test_search_simple() {
    let app = create_test_app_code();

    let result = app.search("workspace", 0, None).await.expect("search failed");

    // Should find "workspace" in Cargo.toml
    assert!(result.contains("Cargo.toml"), "Should find match in Cargo.toml");
}

#[tokio::test]
async fn test_search_with_context() {
    let app = create_test_app_code();

    let result = app.search("resolver", 2, None).await.expect("search failed");

    // Should have context lines around the match
    assert!(result.contains("Cargo.toml"), "Should find match in Cargo.toml");
    // With context, should see surrounding lines
    assert!(result.contains("[workspace]"), "Should show context including [workspace]");
}

#[tokio::test]
async fn test_search_with_path_prefix() {
    let app = create_test_app_code();

    // Search only in ver-stub-build directory
    let result = app
        .search("pub", 0, Some("ver-stub-build/src"))
        .await
        .expect("search failed");

    // Should only find matches in ver-stub-build/src
    for line in result.lines() {
        if line.contains(":") && !line.starts_with('[') {
            // This is a match line (path:line: content)
            assert!(
                line.starts_with("ver-stub-build/src"),
                "All matches should be in ver-stub-build/src, got: {}",
                line
            );
        }
    }
}

#[tokio::test]
async fn test_search_no_matches() {
    let app = create_test_app_code();

    let result = app
        .search("xyzzy_unlikely_string_12345", 0, None)
        .await
        .expect("search failed");

    assert!(result.contains("No matches"), "Should report no matches");
}
