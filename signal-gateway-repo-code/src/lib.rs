//! Application source code browsing via GitHub tarball downloads.
//!
//! This crate provides tools for browsing application source code by downloading
//! tarballs from GitHub and caching them in memory.

mod config;

pub use config::{GitHubRepo, RepoCodeConfig};

use async_trait::async_trait;
use flate2::read::GzDecoder;
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::Deserialize;
use signal_gateway_assistant::{Tool, ToolExecutor, ToolResult};
use std::{collections::HashMap, error::Error, fmt::Write, future::Future, io::Read, pin::Pin, sync::Arc};
use tar::Archive;
use tokio::sync::{Mutex, MutexGuard};
use tracing::{error, info, warn};

/// A file stored in memory from the tarball.
#[derive(Debug, Clone)]
struct CachedFile {
    /// The file contents as a string (lossy UTF-8 conversion).
    content: String,
}

/// Cached tarball contents.
struct CachedTarball {
    /// The git SHA this tarball corresponds to.
    sha: String,
    /// Map from file path to file contents.
    files: HashMap<String, CachedFile>,
}

/// Callback type for getting the current deployed git SHA.
///
/// This is an async callback that returns a future resolving to the SHA.
pub type ShaCallback = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<String, Box<dyn Error + Send + Sync>>> + Send>>
        + Send
        + Sync,
>;

/// Application source code browser.
///
/// Downloads and caches GitHub tarballs for browsing application source code.
pub struct RepoCode {
    config: RepoCodeConfig,
    token: Option<String>,
    glob_filter: Option<GlobSet>,
    get_sha: ShaCallback,
    client: reqwest::Client,
    cache: Mutex<Option<CachedTarball>>,
}

impl RepoCode {
    /// Create a new RepoCode instance from configuration.
    ///
    /// The `get_sha` callback is called to determine which git SHA to download.
    /// It should return `None` if the SHA is not yet known.
    pub fn new(config: RepoCodeConfig, get_sha: ShaCallback) -> Result<Self, std::io::Error> {
        let token = config
            .token_file
            .as_ref()
            .map(|path| std::fs::read_to_string(path).map(|s| s.trim().to_string()))
            .transpose()?;

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

        Ok(Self {
            config,
            token,
            glob_filter,
            get_sha,
            client: reqwest::Client::new(),
            cache: Mutex::new(None),
        })
    }

    /// Get the application name.
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// Get the current tarball, downloading if necessary.
    ///
    /// Returns a mutex guard containing the cached tarball. This method never fails;
    /// instead it logs warnings and returns whatever is currently cached:
    ///
    /// - If the SHA callback fails, returns the existing cache (possibly stale or None)
    /// - If the tarball download fails, returns the existing cache
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

        // Check if we already have this SHA cached
        let needs_download = match &*cache {
            Some(cached) => cached.sha != current_sha,
            None => true,
        };

        if needs_download {
            info!(
                "Downloading tarball for {} at {}",
                self.config.name, current_sha
            );

            let tarball = match self.download_tarball(&current_sha).await {
                Ok(t) => t,
                Err(e) => {
                    error!("Failed to download tarball for {}: {e}", self.config.name);
                    return cache;
                }
            };

            let files = match self.extract_tarball(&tarball) {
                Ok(f) => f,
                Err(e) => {
                    error!("Failed to extract tarball for {}: {e}", self.config.name);
                    return cache;
                }
            };

            *cache = Some(CachedTarball {
                sha: current_sha,
                files,
            });
        }

        cache
    }

    /// Download a tarball from GitHub for the given SHA.
    async fn download_tarball(&self, sha: &str) -> Result<Vec<u8>, String> {
        let url = format!(
            "https://api.github.com/repos/{}/{}/tarball/{}",
            self.config.github.owner, self.config.github.repo, sha
        );

        let mut request = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "signal-gateway")
            .header("X-GitHub-Api-Version", "2022-11-28");

        if let Some(token) = &self.token {
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

    /// Extract a tarball into a map of file paths to contents.
    fn extract_tarball(&self, tarball: &[u8]) -> Result<HashMap<String, CachedFile>, String> {
        let decoder = GzDecoder::new(tarball);
        let mut archive = Archive::new(decoder);

        let mut files = HashMap::new();

        for entry in archive
            .entries()
            .map_err(|e| format!("Failed to read tarball: {e}"))?
        {
            let mut entry = entry.map_err(|e| format!("Failed to read entry: {e}"))?;

            // Skip directories
            if entry.header().entry_type().is_dir() {
                continue;
            }

            let path = entry
                .path()
                .map_err(|e| format!("Failed to get path: {e}"))?
                .to_string_lossy()
                .to_string();

            // GitHub tarballs have a prefix like "owner-repo-sha/"
            // Strip the first component
            let path = path.split('/').skip(1).collect::<Vec<_>>().join("/");

            if path.is_empty() {
                continue;
            }

            // Apply glob filter if configured
            if let Some(ref glob_filter) = self.glob_filter {
                if !glob_filter.is_match(&path) {
                    continue;
                }
            }

            // Read file contents
            let mut contents = Vec::new();
            if entry.read_to_end(&mut contents).is_err() {
                continue; // Skip files we can't read
            }

            // Convert to string, handling non-UTF-8 based on config
            let content = match String::from_utf8(contents) {
                Ok(s) => s,
                Err(e) => {
                    if self.config.include_non_utf8 {
                        // Use lossy conversion if configured to include non-UTF-8
                        String::from_utf8_lossy(e.as_bytes()).into_owned()
                    } else {
                        // Skip non-UTF-8 files by default
                        continue;
                    }
                }
            };

            files.insert(path, CachedFile { content });
        }

        info!("Extracted {} files from tarball", files.len());
        Ok(files)
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
    /// Supports simple glob patterns with `*` wildcards.
    pub async fn find(&self, pattern: Option<&str>) -> Result<String, String> {
        let cache = self.get_current_tarball().await;
        let cached = cache.as_ref().ok_or("source code not available")?;

        let pattern = pattern.unwrap_or("*");

        // Convert glob pattern to regex
        let regex_pattern = glob_to_regex(pattern);
        let regex = Regex::new(&regex_pattern).map_err(|e| format!("Invalid pattern: {e}"))?;

        let mut matches: Vec<&str> = cached
            .files
            .keys()
            .filter(|path| regex.is_match(path))
            .map(|s| s.as_str())
            .collect();

        matches.sort();

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

        let lines: Vec<&str> = file.content.lines().collect();

        // Handle line range (1-indexed)
        let start = line_start.unwrap_or(1).saturating_sub(1);
        let end = line_end.unwrap_or(lines.len()).min(lines.len());

        if start >= lines.len() {
            return Ok(format!(
                "Line {} is past end of file ({} lines)",
                start + 1,
                lines.len()
            ));
        }

        let mut output = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            writeln!(&mut output, "{:>6}\t{}", start + i + 1, line)
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

        let mut sorted_files: Vec<_> = cached.files.iter().collect();
        sorted_files.sort_by_key(|(path, _)| *path);

        'outer: for (path, file) in sorted_files {
            // Skip if path doesn't match prefix
            if let Some(prefix) = prefix
                && !path.starts_with(prefix)
            {
                continue;
            }

            // Skip binary-looking files
            if looks_binary(&file.content) {
                continue;
            }

            let lines: Vec<&str> = file.content.lines().collect();
            let mut file_matches = Vec::new();

            for (line_num, line) in lines.iter().enumerate() {
                if regex.is_match(line) {
                    file_matches.push(line_num);
                    match_count += 1;
                    if match_count >= MAX_MATCHES {
                        break 'outer;
                    }
                }
            }

            if !file_matches.is_empty() {
                file_count += 1;

                if context == 0 {
                    // No context, just print matches
                    for &line_num in &file_matches {
                        writeln!(
                            &mut output,
                            "{}:{}: {}",
                            path,
                            line_num + 1,
                            lines[line_num]
                        )
                        .map_err(|e| format!("Format error: {e}"))?;
                    }
                } else {
                    // Print with context
                    writeln!(&mut output, "=== {} ===", path)
                        .map_err(|e| format!("Format error: {e}"))?;

                    let context = context as usize;
                    let mut printed = std::collections::BTreeSet::new();

                    for &match_line in &file_matches {
                        let start = match_line.saturating_sub(context);
                        let end = (match_line + context + 1).min(lines.len());

                        // Add separator if there's a gap
                        if let Some(&last) = printed.iter().next_back()
                            && start > last + 1
                        {
                            writeln!(&mut output, "---")
                                .map_err(|e| format!("Format error: {e}"))?;
                        }

                        for (i, line) in lines[start..end].iter().enumerate() {
                            let line_idx = start + i;
                            if printed.insert(line_idx) {
                                let marker = if line_idx == match_line { ">" } else { " " };
                                writeln!(&mut output, "{}{:>5}\t{}", marker, line_idx + 1, line)
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

/// Convert a simple glob pattern to a regex.
fn glob_to_regex(pattern: &str) -> String {
    let mut regex = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                regex.push('\\');
                regex.push(c);
            }
            _ => regex.push(c),
        }
    }
    regex.push('$');
    regex
}

/// Check if content looks like binary data.
fn looks_binary(content: &str) -> bool {
    // Check first 1000 chars for null bytes or high ratio of non-printable chars
    let sample: String = content.chars().take(1000).collect();
    let non_printable = sample
        .chars()
        .filter(|c| !c.is_ascii_graphic() && !c.is_ascii_whitespace())
        .count();
    non_printable > sample.len() / 10
}

/// Tool executor for multiple application source code browsers.
pub struct RepoCodeTools {
    apps: Vec<RepoCode>,
}

impl RepoCodeTools {
    /// Create a new RepoCodeTools instance.
    pub fn new(apps: Vec<RepoCode>) -> Self {
        Self { apps }
    }

    /// Find an app by name.
    fn find_app(&self, name: &str) -> Option<&RepoCode> {
        self.apps.iter().find(|app| app.name() == name)
    }

    /// Get list of app names for error messages.
    fn app_names(&self) -> String {
        self.apps
            .iter()
            .map(|a| a.name())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Deserialize)]
struct LsInput {
    app: String,
    path: Option<String>,
}

#[derive(Deserialize)]
struct FindInput {
    app: String,
    pattern: Option<String>,
}

#[derive(Deserialize)]
struct ReadInput {
    app: String,
    path: String,
    line_start: Option<usize>,
    line_end: Option<usize>,
}

#[derive(Deserialize)]
struct SearchInput {
    app: String,
    pattern: String,
    #[serde(default)]
    context: u32,
    path_prefix: Option<String>,
}

#[async_trait]
impl ToolExecutor for RepoCodeTools {
    fn tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "code_ls",
                description: "List files in a directory of an application's source code.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "app": {
                            "type": "string",
                            "description": "Name of the application"
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory path to list (optional, defaults to root)"
                        }
                    },
                    "required": ["app"]
                }),
            },
            Tool {
                name: "code_find",
                description: "Find files matching a glob pattern in an application's source code.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "app": {
                            "type": "string",
                            "description": "Name of the application"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern to match (e.g., '*.rs', 'src/*.py')"
                        }
                    },
                    "required": ["app"]
                }),
            },
            Tool {
                name: "code_read",
                description: "Read a file from an application's source code.",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "app": {
                            "type": "string",
                            "description": "Name of the application"
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
                    "required": ["app", "path"]
                }),
            },
            Tool {
                name: "code_search",
                description: "Search for a regex pattern in an application's source code (like grep).",
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "app": {
                            "type": "string",
                            "description": "Name of the application"
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
                    "required": ["app", "pattern"]
                }),
            },
        ]
    }

    fn has_tool(&self, name: &str) -> bool {
        matches!(name, "code_ls" | "code_find" | "code_read" | "code_search")
    }

    async fn execute(&self, name: &str, input: &serde_json::Value) -> Result<ToolResult, String> {
        match name {
            "code_ls" => {
                let input: LsInput = serde_json::from_value(input.clone())
                    .map_err(|e| format!("Invalid input: {e}"))?;
                let app = self.find_app(&input.app).ok_or_else(|| {
                    format!(
                        "Unknown app '{}'. Available: {}",
                        input.app,
                        self.app_names()
                    )
                })?;
                let result = app.ls(input.path.as_deref()).await?;
                Ok(ToolResult::new(result))
            }
            "code_find" => {
                let input: FindInput = serde_json::from_value(input.clone())
                    .map_err(|e| format!("Invalid input: {e}"))?;
                let app = self.find_app(&input.app).ok_or_else(|| {
                    format!(
                        "Unknown app '{}'. Available: {}",
                        input.app,
                        self.app_names()
                    )
                })?;
                let result = app.find(input.pattern.as_deref()).await?;
                Ok(ToolResult::new(result))
            }
            "code_read" => {
                let input: ReadInput = serde_json::from_value(input.clone())
                    .map_err(|e| format!("Invalid input: {e}"))?;
                let app = self.find_app(&input.app).ok_or_else(|| {
                    format!(
                        "Unknown app '{}'. Available: {}",
                        input.app,
                        self.app_names()
                    )
                })?;
                let result = app
                    .read(&input.path, input.line_start, input.line_end)
                    .await?;
                Ok(ToolResult::new(result))
            }
            "code_search" => {
                let input: SearchInput = serde_json::from_value(input.clone())
                    .map_err(|e| format!("Invalid input: {e}"))?;
                let app = self.find_app(&input.app).ok_or_else(|| {
                    format!(
                        "Unknown app '{}'. Available: {}",
                        input.app,
                        self.app_names()
                    )
                })?;
                let result = app
                    .search(&input.pattern, input.context, input.path_prefix.as_deref())
                    .await?;
                Ok(ToolResult::new(result))
            }
            _ => Err(format!("Unknown tool: {name}")),
        }
    }
}
