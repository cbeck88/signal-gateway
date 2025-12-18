//! Configuration types for repository code browsing.

use serde::Deserialize;
use std::{path::PathBuf, str::FromStr};

/// A GitHub repository identifier (owner/repo).
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "String")]
pub struct GitHubRepo {
    /// The repository owner (user or organization).
    pub owner: String,
    /// The repository name.
    pub repo: String,
}

impl FromStr for GitHubRepo {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('/').collect();
        if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
            return Err(format!(
                "invalid GitHub repo '{}': expected 'owner/repo' format",
                s
            ));
        }
        Ok(Self {
            owner: parts[0].to_string(),
            repo: parts[1].to_string(),
        })
    }
}

impl TryFrom<String> for GitHubRepo {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// Configuration for an application's source code access.
#[derive(Clone, Debug, Deserialize)]
pub struct RepoCodeConfig {
    /// Name of the application (used to identify it in tool calls).
    pub name: String,
    /// GitHub repository in "owner/repo" format.
    pub github: GitHubRepo,
    /// Path to file containing the GitHub personal access token.
    /// Optional for public repositories (unauthenticated access has lower rate limits).
    pub token_file: Option<PathBuf>,
    /// Glob patterns to filter which files are included from the tarball.
    /// If non-empty, only files matching at least one pattern are kept.
    /// Uses gitignore-style glob syntax (e.g., "*.rs", "src/**/*.rs").
    #[serde(default)]
    pub glob: Vec<String>,
    /// Include files that aren't valid UTF-8 (using lossy conversion).
    /// By default (false), non-UTF-8 files are skipped entirely.
    #[serde(default)]
    pub include_non_utf8: bool,
}
