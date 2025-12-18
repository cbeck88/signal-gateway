//! Repository source code browsing via GitHub tarball downloads.
//!
//! This crate provides tools for browsing repository source code by downloading
//! tarballs from GitHub and caching them in memory.

mod cached;
mod config;

pub use config::{CodeToolConfig, GitHubRepo, Source};

use cached::CachedTarball;

use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::Deserialize;
use signal_gateway_assistant::{Tool, ToolExecutor, ToolResult};
use std::{error::Error, fmt::Write, future::Future, pin::Pin, sync::Arc};
use tokio::sync::{Mutex, MutexGuard};
use tracing::{error, info, warn};

/// Callback type for getting the current deployed git SHA.
///
/// This is an async callback that returns a future resolving to the SHA.
pub type ShaCallback = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<String, Box<dyn Error + Send + Sync>>> + Send>>
        + Send
        + Sync,
>;

/// Internal representation of the resolved source.
enum ResolvedSource {
    GitHub {
        owner: String,
        repo: String,
        token: Option<String>,
    },
    File {
        path: std::path::PathBuf,
    },
}

/// Repository source code browser.
///
/// Downloads and caches GitHub tarballs for browsing repository source code.
pub struct CodeTool {
    config: CodeToolConfig,
    source: ResolvedSource,
    glob_filter: Option<GlobSet>,
    summary: Option<Box<str>>,
    get_sha: ShaCallback,
    client: reqwest::Client,
    cache: Mutex<Option<CachedTarball>>,
}

impl CodeTool {
    /// Create a new CodeTool instance from configuration.
    ///
    /// The `get_sha` callback is called to determine which git SHA to download/load.
    /// For GitHub sources, this is the commit SHA. For file sources, this can be
    /// used to track file modification (e.g., mtime or a version string).
    pub fn new(config: CodeToolConfig, get_sha: ShaCallback) -> Result<Self, std::io::Error> {
        let source = match &config.source {
            Source::GitHub { repo, token_file } => {
                let token = token_file
                    .as_ref()
                    .map(|path| std::fs::read_to_string(path).map(|s| s.trim().to_string()))
                    .transpose()?;
                ResolvedSource::GitHub {
                    owner: repo.owner.clone(),
                    repo: repo.repo.clone(),
                    token,
                }
            }
            Source::File { path } => ResolvedSource::File { path: path.clone() },
        };

        // Compile glob patterns if any are specified
        let glob_filter =
            if config.glob.is_empty() {
                None
            } else {
                let mut builder = GlobSetBuilder::new();
                for pattern in &config.glob {
                    let glob = Glob::new(pattern).map_err(|e| {
                        std::io::Error::other(format!("invalid glob pattern '{}': {}", pattern, e))
                    })?;
                    builder.add(glob);
                }
                Some(builder.build().map_err(|e| {
                    std::io::Error::other(format!("failed to build glob set: {}", e))
                })?)
            };

        // Load summary from file if configured
        let summary = config
            .summary_file
            .as_ref()
            .map(|path| std::fs::read_to_string(path).map(|s| s.into_boxed_str()))
            .transpose()?;

        Ok(Self {
            config,
            source,
            glob_filter,
            summary,
            get_sha,
            client: reqwest::Client::new(),
            cache: Mutex::new(None),
        })
    }

    /// Get the repository name.
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// Check if this code tool has a summary available.
    pub fn has_summary(&self) -> bool {
        self.summary.is_some()
    }

    /// Get the summary of the codebase, if available.
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// Get the current tarball, downloading or reading from file as needed.
    ///
    /// Returns a mutex guard containing the cached tarball. This method never fails;
    /// instead it logs warnings and returns whatever is currently cached:
    ///
    /// - If the SHA callback fails, returns the existing cache (possibly stale or None)
    /// - If the tarball download/read fails, returns the existing cache
    /// - If tarball extraction fails, returns the existing cache
    ///
    /// This design assumes that stale code is better than no code, since most of the
    /// codebase is likely unchanged between versions.
    async fn get_current_tarball(&self) -> MutexGuard<'_, Option<CachedTarball>> {
        let current_sha = match (self.get_sha)().await {
            Ok(sha) => sha,
            Err(e) => {
                warn!("Failed to get current SHA for {}: {e}", self.config.name);
                return self.cache.lock().await;
            }
        };

        let mut cache = self.cache.lock().await;

        // Check if we need to load/reload the tarball.
        // For file sources, we only load once (no refresh after initial load).
        // For GitHub sources, we reload when the SHA changes.
        let needs_refresh = match (&*cache, &self.source) {
            (None, _) => true,
            (Some(_), ResolvedSource::File { .. }) => false,
            (Some(cached), ResolvedSource::GitHub { .. }) => cached.sha != current_sha,
        };

        if needs_refresh {
            info!(
                "Loading tarball for {} at {}",
                self.config.name, current_sha
            );

            let tarball = match self.load_tarball(&current_sha).await {
                Ok(t) => t,
                Err(e) => {
                    error!("Failed to load tarball for {}: {e}", self.config.name);
                    return cache;
                }
            };

            match CachedTarball::extract(
                current_sha,
                &tarball,
                self.glob_filter.as_ref(),
                self.config.include_non_utf8,
            ) {
                Ok(cached_tarball) => {
                    *cache = Some(cached_tarball);
                }
                Err(e) => {
                    error!("Failed to extract tarball for {}: {e}", self.config.name);
                }
            };
        }

        cache
    }

    /// Load a tarball either from GitHub or from a local file.
    async fn load_tarball(&self, sha: &str) -> Result<Vec<u8>, String> {
        match &self.source {
            ResolvedSource::GitHub { owner, repo, token } => {
                self.download_tarball_from_github(owner, repo, token.as_deref(), sha)
                    .await
            }
            ResolvedSource::File { path } => std::fs::read(path)
                .map_err(|e| format!("Failed to read tarball from {}: {e}", path.display())),
        }
    }

    /// Download a tarball from GitHub for the given SHA.
    async fn download_tarball_from_github(
        &self,
        owner: &str,
        repo: &str,
        token: Option<&str>,
        sha: &str,
    ) -> Result<Vec<u8>, String> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/tarball/{}",
            owner, repo, sha
        );

        let mut request = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "signal-gateway")
            .header("X-GitHub-Api-Version", "2022-11-28");

        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {}", token));
        }

        let response = request
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?;

        if !response.status().is_success() {
            return Err(format!(
                "GitHub API error: {} {}",
                response.status(),
                response.text().await.unwrap_or_default()
            ));
        }

        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("Failed to read response body: {e}"))
    }

    /// List files in a directory (like `ls`).
    ///
    /// If `path` is None or empty, lists the root directory.
    pub async fn ls(&self, path: Option<&str>) -> Result<String, String> {
        let cache = self.get_current_tarball().await;
        let cached = cache.as_ref().ok_or("source code not available")?;

        let prefix = path.unwrap_or("").trim_start_matches('/');
        let prefix = if prefix.is_empty() {
            String::new()
        } else if prefix.ends_with('/') {
            prefix.to_string()
        } else {
            format!("{}/", prefix)
        };

        let mut entries = std::collections::BTreeSet::new();

        for file_path in cached.files.keys() {
            if prefix.is_empty() || file_path.starts_with(&prefix) {
                // Get the part after the prefix
                let remainder = if prefix.is_empty() {
                    file_path.as_str()
                } else {
                    &file_path[prefix.len()..]
                };

                // Get just the first component (file or directory name)
                if let Some(first) = remainder.split('/').next()
                    && !first.is_empty()
                {
                    // Check if it's a directory (has more components)
                    let is_dir = remainder.contains('/');
                    let entry = if is_dir {
                        format!("{}/", first)
                    } else {
                        first.to_string()
                    };
                    entries.insert(entry);
                }
            }
        }

        if entries.is_empty() {
            Ok(format!(
                "No files found in '{}'",
                prefix.trim_end_matches('/')
            ))
        } else {
            Ok(entries.into_iter().collect::<Vec<_>>().join("\n"))
        }
    }

    /// Find files matching a glob pattern (like `find`).
    ///
    /// Supports glob patterns using the `globset` crate syntax.
    pub async fn find(&self, pattern: Option<&str>) -> Result<String, String> {
        let cache = self.get_current_tarball().await;
        let cached = cache.as_ref().ok_or("source code not available")?;

        let pattern = pattern.unwrap_or("*");

        let glob = Glob::new(pattern)
            .map_err(|e| format!("Invalid glob pattern: {e}"))?
            .compile_matcher();

        let matches: Vec<&str> = cached
            .files
            .keys()
            .filter(|path| glob.is_match(path))
            .map(|s| s.as_str())
            .collect();

        if matches.is_empty() {
            Ok(format!("No files matching '{}'", pattern))
        } else {
            Ok(matches.join("\n"))
        }
    }

    /// Read a file's contents.
    ///
    /// If `line_range` is provided, only returns those lines (1-indexed, inclusive).
    pub async fn read(
        &self,
        path: &str,
        line_start: Option<usize>,
        line_end: Option<usize>,
    ) -> Result<String, String> {
        let cache = self.get_current_tarball().await;
        let cached = cache.as_ref().ok_or("source code not available")?;

        let path = path.trim_start_matches('/');
        let file = cached
            .files
            .get(path)
            .ok_or_else(|| format!("File not found: {}", path))?;

        let total_lines = file.line_count();
        let start = line_start.unwrap_or(1) as u32;
        let end = line_end.map(|e| e as u32);

        if start > total_lines {
            return Ok(format!(
                "Line {} is past end of file ({} lines)",
                start, total_lines
            ));
        }

        let mut output = String::new();
        for (i, line) in file.line_range(Some(start), end).enumerate() {
            writeln!(&mut output, "{:>6}\t{}", start as usize + i, line)
                .map_err(|e| format!("Format error: {e}"))?;
        }

        Ok(output)
    }

    /// Search for a regex pattern in all files.
    ///
    /// - `pattern`: The regex pattern to search for.
    /// - `context`: Number of context lines to show (like `grep -C`).
    /// - `path_prefix`: Optional path prefix to limit search scope.
    pub async fn search(
        &self,
        pattern: &str,
        context: u32,
        path_prefix: Option<&str>,
    ) -> Result<String, String> {
        let regex = Regex::new(pattern).map_err(|e| format!("Invalid regex: {e}"))?;

        let cache = self.get_current_tarball().await;
        let cached = cache.as_ref().ok_or("source code not available")?;

        let prefix = path_prefix.map(|p| p.trim_start_matches('/'));

        let mut output = String::new();
        let mut match_count = 0;
        let mut file_count = 0;
        const MAX_MATCHES: usize = 100;

        'outer: for (path, file) in &cached.files {
            // Skip if path doesn't match prefix
            if let Some(prefix) = prefix
                && !path.starts_with(prefix)
            {
                continue;
            }

            // Skip binary-looking files
            if file.is_binary() {
                continue;
            }

            // Find all matches and map byte positions to line numbers
            let content = file.as_str();
            let mut file_matches: Vec<u32> = Vec::new();

            for m in regex.find_iter(content) {
                let line_num = file.idx_to_line(m.start());
                // Deduplicate: only add if this line isn't already recorded
                if file_matches.last() != Some(&line_num) {
                    file_matches.push(line_num);
                    match_count += 1;
                    if match_count >= MAX_MATCHES {
                        break 'outer;
                    }
                }
            }

            if !file_matches.is_empty() {
                file_count += 1;
                let total_lines = file.line_count();

                if context == 0 {
                    // No context, just print matches
                    for &line_num in &file_matches {
                        let line = file
                            .line_range(Some(line_num), Some(line_num))
                            .next()
                            .unwrap_or("");
                        writeln!(&mut output, "{}:{}: {}", path, line_num, line)
                            .map_err(|e| format!("Format error: {e}"))?;
                    }
                } else {
                    // Print with context
                    writeln!(&mut output, "=== {} ===", path)
                        .map_err(|e| format!("Format error: {e}"))?;

                    let mut printed = std::collections::BTreeSet::new();

                    for &match_line in &file_matches {
                        let start = match_line.saturating_sub(context).max(1);
                        let end = (match_line + context).min(total_lines);

                        // Add separator if there's a gap
                        if let Some(&last) = printed.iter().next_back()
                            && start > last + 1
                        {
                            writeln!(&mut output, "---")
                                .map_err(|e| format!("Format error: {e}"))?;
                        }

                        for (i, line) in file.line_range(Some(start), Some(end)).enumerate() {
                            let line_num = start + i as u32;
                            if printed.insert(line_num) {
                                let marker = if line_num == match_line { ">" } else { " " };
                                writeln!(&mut output, "{}{:>5}\t{}", marker, line_num, line)
                                    .map_err(|e| format!("Format error: {e}"))?;
                            }
                        }
                    }
                    writeln!(&mut output).map_err(|e| format!("Format error: {e}"))?;
                }
            }
        }

        if match_count == 0 {
            Ok(format!("No matches for '{}'", pattern))
        } else {
            let truncated = if match_count >= MAX_MATCHES {
                format!(" (truncated at {} matches)", MAX_MATCHES)
            } else {
                String::new()
            };
            Ok(format!(
                "{}\n[{} matches in {} files{}]",
                output.trim_end(),
                match_count,
                file_count,
                truncated
            ))
        }
    }
}

/// Tool executor for multiple repository code browsers.
pub struct CodeToolTools {
    repos: Vec<CodeTool>,
}

impl CodeToolTools {
    /// Create a new CodeToolTools instance.
    pub fn new(repos: Vec<CodeTool>) -> Self {
        Self { repos }
    }

    /// Find a repo by name.
    fn find_repo(&self, name: &str) -> Option<&CodeTool> {
        self.repos.iter().find(|repo| repo.name() == name)
    }

    /// Get list of repo names for error messages.
    fn repo_names(&self) -> String {
        self.repos
            .iter()
            .map(|r| r.name())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Get list of repo names that have summaries available.
    fn repos_with_summaries(&self) -> Vec<&str> {
        self.repos
            .iter()
            .filter(|r| r.has_summary())
            .map(|r| r.name())
            .collect()
    }
}

#[derive(Deserialize)]
struct LsInput {
    repo: String,
    path: Option<String>,
}

#[derive(Deserialize)]
struct FindInput {
    repo: String,
    pattern: Option<String>,
}

#[derive(Deserialize)]
struct ReadInput {
    repo: String,
    path: String,
    line_start: Option<usize>,
    line_end: Option<usize>,
}

#[derive(Deserialize)]
struct SearchInput {
    repo: String,
    pattern: String,
    #[serde(default)]
    context: u32,
    path_prefix: Option<String>,
}

#[derive(Deserialize)]
struct SummaryInput {
    repo: String,
}

#[async_trait]
impl ToolExecutor for CodeToolTools {
    fn tools(&self) -> Vec<Tool> {
        let mut tools = vec![
            Tool {
                name: "code_ls",
                description: "List files in a directory of a repository's source code.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "repo": {
                            "type": "string",
                            "description": "Name of the repository"
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory path to list (optional, defaults to root)"
                        }
                    },
                    "required": ["repo"]
                }),
            },
            Tool {
                name: "code_find",
                description: "Find files matching a glob pattern in a repository's source code.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "repo": {
                            "type": "string",
                            "description": "Name of the repository"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern to match (e.g., '*.rs', 'src/*.py')"
                        }
                    },
                    "required": ["repo"]
                }),
            },
            Tool {
                name: "code_read",
                description: "Read a file from a repository's source code.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "repo": {
                            "type": "string",
                            "description": "Name of the repository"
                        },
                        "path": {
                            "type": "string",
                            "description": "Path to the file to read"
                        },
                        "line_start": {
                            "type": "integer",
                            "description": "Starting line number (1-indexed, optional)"
                        },
                        "line_end": {
                            "type": "integer",
                            "description": "Ending line number (inclusive, optional)"
                        }
                    },
                    "required": ["repo", "path"]
                }),
            },
            Tool {
                name: "code_search",
                description: "Search for a regex pattern in a repository's source code (like grep).",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "repo": {
                            "type": "string",
                            "description": "Name of the repository"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for"
                        },
                        "context": {
                            "type": "integer",
                            "description": "Number of context lines to show (like grep -C, default 0)"
                        },
                        "path_prefix": {
                            "type": "string",
                            "description": "Optional path prefix to limit search scope"
                        }
                    },
                    "required": ["repo", "pattern"]
                }),
            },
        ];

        // Only include summary tool if at least one repo has a summary
        let repos_with_summaries = self.repos_with_summaries();
        if !repos_with_summaries.is_empty() {
            tools.push(Tool {
                name: "code_summary",
                description: "Get a summary/overview of a repository's codebase.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "repo": {
                            "type": "string",
                            "description": format!("Name of the repository. Repos with summaries: {}", repos_with_summaries.join(", "))
                        }
                    },
                    "required": ["repo"]
                }),
            });
        }

        tools
    }

    fn has_tool(&self, name: &str) -> bool {
        matches!(
            name,
            "code_ls" | "code_find" | "code_read" | "code_search" | "code_summary"
        )
    }

    async fn execute(&self, name: &str, input: &serde_json::Value) -> Result<ToolResult, String> {
        match name {
            "code_ls" => {
                let input: LsInput = serde_json::from_value(input.clone())
                    .map_err(|e| format!("Invalid input: {e}"))?;
                let repo = self.find_repo(&input.repo).ok_or_else(|| {
                    format!(
                        "Unknown repo '{}'. Available: {}",
                        input.repo,
                        self.repo_names()
                    )
                })?;
                let result = repo.ls(input.path.as_deref()).await?;
                Ok(ToolResult::new(result))
            }
            "code_find" => {
                let input: FindInput = serde_json::from_value(input.clone())
                    .map_err(|e| format!("Invalid input: {e}"))?;
                let repo = self.find_repo(&input.repo).ok_or_else(|| {
                    format!(
                        "Unknown repo '{}'. Available: {}",
                        input.repo,
                        self.repo_names()
                    )
                })?;
                let result = repo.find(input.pattern.as_deref()).await?;
                Ok(ToolResult::new(result))
            }
            "code_read" => {
                let input: ReadInput = serde_json::from_value(input.clone())
                    .map_err(|e| format!("Invalid input: {e}"))?;
                let repo = self.find_repo(&input.repo).ok_or_else(|| {
                    format!(
                        "Unknown repo '{}'. Available: {}",
                        input.repo,
                        self.repo_names()
                    )
                })?;
                let result = repo
                    .read(&input.path, input.line_start, input.line_end)
                    .await?;
                Ok(ToolResult::new(result))
            }
            "code_search" => {
                let input: SearchInput = serde_json::from_value(input.clone())
                    .map_err(|e| format!("Invalid input: {e}"))?;
                let repo = self.find_repo(&input.repo).ok_or_else(|| {
                    format!(
                        "Unknown repo '{}'. Available: {}",
                        input.repo,
                        self.repo_names()
                    )
                })?;
                let result = repo
                    .search(&input.pattern, input.context, input.path_prefix.as_deref())
                    .await?;
                Ok(ToolResult::new(result))
            }
            "code_summary" => {
                let input: SummaryInput = serde_json::from_value(input.clone())
                    .map_err(|e| format!("Invalid input: {e}"))?;
                let repo = self.find_repo(&input.repo).ok_or_else(|| {
                    format!(
                        "Unknown repo '{}'. Available: {}",
                        input.repo,
                        self.repo_names()
                    )
                })?;
                let summary = repo.summary().ok_or_else(|| {
                    format!(
                        "No summary available for '{}'. Repos with summaries: {}",
                        input.repo,
                        self.repos_with_summaries().join(", ")
                    )
                })?;
                Ok(ToolResult::new(summary.to_string()))
            }
            _ => Err(format!("Unknown tool: {name}")),
        }
    }
}
