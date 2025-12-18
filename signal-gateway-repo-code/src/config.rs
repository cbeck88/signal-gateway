//! Configuration types for repository code browsing.

use serde::Deserialize;
use std::{path::PathBuf, str::FromStr};

/// A GitHub repository identifier (owner/repo).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
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

/// Source of repository code - either a GitHub repo or a local tarball file.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub enum Source {
    /// GitHub repository source. Downloads tarballs from GitHub API.
    #[serde(rename = "github")]
    GitHub {
        /// GitHub repository in "owner/repo" format.
        repo: GitHubRepo,
        /// Path to file containing the GitHub personal access token.
        /// Optional for public repositories (unauthenticated access has lower rate limits).
        token_file: Option<PathBuf>,
    },
    /// Local tarball file source. Reads a .tar.gz file from disk.
    #[serde(rename = "file")]
    File {
        /// Path to the tarball file (.tar.gz).
        path: PathBuf,
    },
}

/// Configuration for an application's source code access.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RepoCodeConfig {
    /// Name of the application (used to identify it in tool calls).
    pub name: String,
    /// Source of the repository code.
    #[serde(flatten)]
    pub source: Source,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_github_source() {
        let json = r#"{
            "name": "my-app",
            "github": {
                "repo": "owner/repo-name",
                "token_file": "/path/to/token"
            }
        }"#;

        let config: RepoCodeConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.name, "my-app");
        assert_eq!(
            config.source,
            Source::GitHub {
                repo: GitHubRepo {
                    owner: "owner".to_string(),
                    repo: "repo-name".to_string(),
                },
                token_file: Some(PathBuf::from("/path/to/token")),
            }
        );
        assert!(config.glob.is_empty());
        assert!(!config.include_non_utf8);
    }

    #[test]
    fn test_parse_github_source_no_token() {
        let json = r#"{
            "name": "public-app",
            "github": {
                "repo": "org/public-repo"
            }
        }"#;

        let config: RepoCodeConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.name, "public-app");
        assert_eq!(
            config.source,
            Source::GitHub {
                repo: GitHubRepo {
                    owner: "org".to_string(),
                    repo: "public-repo".to_string(),
                },
                token_file: None,
            }
        );
    }

    #[test]
    fn test_parse_file_source() {
        let json = r#"{
            "name": "local-app",
            "file": {
                "path": "/tmp/source.tar.gz"
            }
        }"#;

        let config: RepoCodeConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.name, "local-app");
        assert_eq!(
            config.source,
            Source::File {
                path: PathBuf::from("/tmp/source.tar.gz"),
            }
        );
    }

    #[test]
    fn test_parse_with_glob_and_options() {
        let json = r#"{
            "name": "filtered-app",
            "github": {
                "repo": "owner/repo"
            },
            "glob": ["**/*.rs", "Cargo.toml"],
            "include_non_utf8": true
        }"#;

        let config: RepoCodeConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.name, "filtered-app");
        assert_eq!(config.glob, vec!["**/*.rs", "Cargo.toml"]);
        assert!(config.include_non_utf8);
    }

    #[test]
    fn test_parse_github_repo_string() {
        let repo: GitHubRepo = "owner/repo".parse().unwrap();
        assert_eq!(repo.owner, "owner");
        assert_eq!(repo.repo, "repo");
    }

    #[test]
    fn test_parse_github_repo_invalid() {
        assert!("invalid".parse::<GitHubRepo>().is_err());
        assert!("".parse::<GitHubRepo>().is_err());
        assert!("/repo".parse::<GitHubRepo>().is_err());
        assert!("owner/".parse::<GitHubRepo>().is_err());
        assert!("a/b/c".parse::<GitHubRepo>().is_err());
    }
}
