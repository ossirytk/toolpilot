use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use glob::glob;
use memmap2::Mmap;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_PATHS: usize = 128;
pub const MAX_PATTERN_LENGTH: usize = 512;
pub const MAX_FIELDS: usize = 64;
pub const MAX_RESULTS_DEFAULT: usize = 200;
pub const MAX_RESULTS_HARD_CAP: usize = 5000;

pub const MAX_TREE_DEPTH_HARD_CAP: usize = 10;
pub const MAX_TREE_ENTRIES_DEFAULT: usize = 200;
pub const MAX_TREE_ENTRIES_HARD_CAP: usize = 2000;

pub const MAX_GIT_COMMITS_DEFAULT: usize = 20;
pub const MAX_GIT_COMMITS_HARD_CAP: usize = 500;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolError {
    pub code: String,
    pub message: String,
}

type ToolResult<T> = Result<T, ToolError>;

fn resolve_max_results(max_results: Option<usize>) -> ToolResult<usize> {
    let value = max_results.unwrap_or(MAX_RESULTS_DEFAULT).max(1);
    if value > MAX_RESULTS_HARD_CAP {
        return Err(ToolError {
            code: "InvalidInput".to_string(),
            message: format!("max_results cannot exceed {}", MAX_RESULTS_HARD_CAP),
        });
    }
    Ok(value)
}

#[derive(Clone)]
struct CachedJson {
    modified: Option<SystemTime>,
    len: u64,
    value: Arc<Value>,
}

struct CachedText {
    modified: Option<SystemTime>,
    len: u64,
    mmap: Arc<Mmap>,
}

#[derive(Default)]
pub struct ServerState {
    json_cache: HashMap<PathBuf, CachedJson>,
    text_cache: HashMap<PathBuf, CachedText>,
    regex_cache: HashMap<(String, bool), Regex>,
    requests_per_tool: BTreeMap<String, u64>,
    cache_hits: u64,
    cache_misses: u64,
    yaml_cache: HashMap<PathBuf, CachedJson>,
}

impl ServerState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_request(&mut self, tool: &str) {
        *self.requests_per_tool.entry(tool.to_string()).or_insert(0) += 1;
    }

    pub fn metrics_json(&self) -> Value {
        json!({
            "requests_per_tool": self.requests_per_tool,
            "cache": {
                "hits": self.cache_hits,
                "misses": self.cache_misses
            }
        })
    }

    fn metadata_signature(path: &Path) -> ToolResult<(Option<SystemTime>, u64)> {
        let metadata = std::fs::metadata(path).map_err(|_| ToolError {
            code: "PathNotFound".to_string(),
            message: format!("Path does not exist: {}", path.display()),
        })?;
        Ok((metadata.modified().ok(), metadata.len()))
    }

    fn load_json(&mut self, path: &Path) -> ToolResult<Arc<Value>> {
        let (modified, len) = Self::metadata_signature(path)?;
        if let Some(cached) = self.json_cache.get(path)
            && cached.modified == modified
            && cached.len == len
        {
            self.cache_hits += 1;
            return Ok(cached.value.clone());
        }

        self.cache_misses += 1;
        let raw = std::fs::read_to_string(path).map_err(|_| ToolError {
            code: "ReadFailed".to_string(),
            message: format!("Failed to read file: {}", path.display()),
        })?;
        let parsed = serde_json::from_str::<Value>(&raw).map_err(|_| ToolError {
            code: "InvalidJson".to_string(),
            message: format!("Invalid JSON in {}", path.display()),
        })?;
        let parsed = Arc::new(parsed);
        self.json_cache.insert(
            path.to_path_buf(),
            CachedJson {
                modified,
                len,
                value: parsed.clone(),
            },
        );
        Ok(parsed)
    }

    fn load_text(&mut self, path: &Path) -> ToolResult<Arc<Mmap>> {
        let (modified, len) = Self::metadata_signature(path)?;
        if let Some(cached) = self.text_cache.get(path)
            && cached.modified == modified
            && cached.len == len
        {
            self.cache_hits += 1;
            return Ok(cached.mmap.clone());
        }

        self.cache_misses += 1;
        let file = File::open(path).map_err(|_| ToolError {
            code: "ReadFailed".to_string(),
            message: format!("Failed to read file: {}", path.display()),
        })?;
        // SAFETY: File is opened read-only and mmap is immutable.
        let mmap = unsafe { Mmap::map(&file) }.map_err(|_| ToolError {
            code: "ReadFailed".to_string(),
            message: format!("Failed to memory-map file: {}", path.display()),
        })?;
        let mmap = Arc::new(mmap);
        self.text_cache.insert(
            path.to_path_buf(),
            CachedText {
                modified,
                len,
                mmap: mmap.clone(),
            },
        );
        Ok(mmap)
    }

    fn load_yaml(&mut self, path: &Path) -> ToolResult<Arc<Value>> {
        let (modified, len) = Self::metadata_signature(path)?;
        if let Some(cached) = self.yaml_cache.get(path)
            && cached.modified == modified
            && cached.len == len
        {
            self.cache_hits += 1;
            return Ok(cached.value.clone());
        }

        self.cache_misses += 1;
        let raw = std::fs::read_to_string(path).map_err(|_| ToolError {
            code: "ReadFailed".to_string(),
            message: format!("Failed to read file: {}", path.display()),
        })?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let parsed: Value = match ext {
            "toml" => toml::from_str::<Value>(&raw).map_err(|e| ToolError {
                code: "InvalidToml".to_string(),
                message: format!("Invalid TOML in {}: {e}", path.display()),
            })?,
            "yml" | "yaml" => serde_yaml::from_str::<Value>(&raw).map_err(|e| ToolError {
                code: "InvalidYaml".to_string(),
                message: format!("Invalid YAML in {}: {e}", path.display()),
            })?,
            _ => {
                return Err(ToolError {
                    code: "UnsupportedFormat".to_string(),
                    message: "Only .yml, .yaml, and .toml files are supported".to_string(),
                });
            }
        };
        let parsed = Arc::new(parsed);
        self.yaml_cache.insert(
            path.to_path_buf(),
            CachedJson {
                modified,
                len,
                value: parsed.clone(),
            },
        );
        Ok(parsed)
    }

    fn cached_regex(&mut self, pattern: &str, case_sensitive: bool) -> ToolResult<Regex> {
        let key = (pattern.to_string(), case_sensitive);
        if let Some(cached) = self.regex_cache.get(&key) {
            self.cache_hits += 1;
            return Ok(cached.clone());
        }

        self.cache_misses += 1;
        let regex = RegexBuilder::new(pattern)
            .case_insensitive(!case_sensitive)
            .build()
            .map_err(|_| ToolError {
                code: "InvalidPattern".to_string(),
                message: "Pattern failed to compile".to_string(),
            })?;
        self.regex_cache.insert(key, regex.clone());
        Ok(regex)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsGlobInput {
    pub base_path: String,
    pub patterns: Vec<String>,
    pub max_results: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct FsGlobOutput {
    pub paths: Vec<String>,
    pub count: usize,
    pub truncated: bool,
}

pub fn execute_fs_glob(input: FsGlobInput) -> ToolResult<FsGlobOutput> {
    if input.patterns.is_empty() || input.patterns.len() > MAX_PATHS {
        return Err(ToolError {
            code: "InvalidInput".to_string(),
            message: "patterns must contain between 1 and 128 items".to_string(),
        });
    }
    let max_results = resolve_max_results(input.max_results)?;
    let base_path = PathBuf::from(input.base_path);
    let mut all = Vec::new();
    for pattern in &input.patterns {
        if pattern.len() > MAX_PATTERN_LENGTH {
            return Err(ToolError {
                code: "InvalidPattern".to_string(),
                message: "Pattern exceeds max length".to_string(),
            });
        }
        let full = base_path.join(pattern).to_string_lossy().to_string();
        let entries = glob(&full).map_err(|_| ToolError {
            code: "InvalidPattern".to_string(),
            message: "Invalid glob pattern".to_string(),
        })?;
        for path in entries.flatten() {
            all.push(path.to_string_lossy().to_string());
        }
    }
    all.sort();
    all.dedup();
    let count = all.len();
    let truncated = count > max_results;
    all.truncate(max_results);
    Ok(FsGlobOutput {
        paths: all,
        count,
        truncated,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Literal,
    Regex,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextSearchInput {
    pub paths: Vec<String>,
    pub query: String,
    pub mode: SearchMode,
    pub case_sensitive: Option<bool>,
    pub max_results: Option<usize>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TextMatch {
    pub file: String,
    pub line: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct TextSearchOutput {
    pub matches: Vec<TextMatch>,
    pub count: usize,
    pub truncated: bool,
}

pub fn execute_text_search(
    state: &mut ServerState,
    input: TextSearchInput,
) -> ToolResult<TextSearchOutput> {
    if input.paths.is_empty() || input.paths.len() > MAX_PATHS {
        return Err(ToolError {
            code: "InvalidInput".to_string(),
            message: "paths must contain between 1 and 128 items".to_string(),
        });
    }
    if input.query.is_empty() || input.query.len() > MAX_PATTERN_LENGTH {
        return Err(ToolError {
            code: "InvalidPattern".to_string(),
            message: "query length is invalid".to_string(),
        });
    }

    let case_sensitive = input.case_sensitive.unwrap_or(true);
    let max_results = resolve_max_results(input.max_results)?;
    let compiled_pattern = match input.mode {
        SearchMode::Literal => regex::escape(&input.query),
        SearchMode::Regex => input.query,
    };
    let regex = state.cached_regex(&compiled_pattern, case_sensitive)?;

    let mut paths = input.paths;
    paths.sort();
    paths.dedup();

    let mut matches = Vec::new();
    let mut truncated = false;
    'paths: for path in paths {
        let mmap = state.load_text(Path::new(&path))?;
        let content = std::str::from_utf8(&mmap).map_err(|_| ToolError {
            code: "UnsupportedEncoding".to_string(),
            message: format!("File is not valid UTF-8: {path}"),
        })?;
        let mut line_no = 1usize;
        let mut line_start = 0usize;
        for line in content.split_inclusive('\n') {
            for capture in regex.find_iter(line) {
                if matches.len() == max_results {
                    truncated = true;
                    break 'paths;
                }
                matches.push(TextMatch {
                    file: path.clone(),
                    line: line_no,
                    byte_start: line_start + capture.start(),
                    byte_end: line_start + capture.end(),
                    text: line.trim_end_matches('\n').to_string(),
                });
            }
            line_start += line.len();
            line_no += 1;
        }
    }

    let count = matches.len();
    Ok(TextSearchOutput {
        matches,
        count,
        truncated,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JsonFilterOp {
    Eq,
    Contains,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonFilter {
    pub field: String,
    pub op: JsonFilterOp,
    pub value: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonSelectInput {
    pub path: String,
    pub fields: Vec<String>,
    pub filters: Option<Vec<JsonFilter>>,
    pub max_results: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct JsonSelectOutput {
    pub rows: Vec<Value>,
    pub count: usize,
    pub truncated: bool,
}

pub fn execute_json_select(
    state: &mut ServerState,
    input: JsonSelectInput,
) -> ToolResult<JsonSelectOutput> {
    if input.fields.is_empty() || input.fields.len() > MAX_FIELDS {
        return Err(ToolError {
            code: "InvalidInput".to_string(),
            message: "fields must contain between 1 and 64 items".to_string(),
        });
    }
    let max_results = resolve_max_results(input.max_results)?;
    let value = state.load_json(Path::new(&input.path))?;
    let rows: Vec<&Value> = match value.as_array() {
        Some(arr) => arr.iter().collect(),
        None => vec![value.as_ref()],
    };
    let filters = input.filters.unwrap_or_default();
    let mut selected_rows = Vec::new();
    let mut truncated = false;
    for row in rows {
        let obj = row.as_object().ok_or_else(|| ToolError {
            code: "InvalidJsonShape".to_string(),
            message: "Expected root object or array of objects".to_string(),
        })?;
        let mut include = true;
        for filter in &filters {
            let candidate = obj.get(&filter.field).unwrap_or(&Value::Null);
            let matched = match filter.op {
                JsonFilterOp::Eq => candidate == &filter.value,
                JsonFilterOp::Contains => match candidate {
                    Value::String(s) => filter
                        .value
                        .as_str()
                        .is_some_and(|needle| s.contains(needle)),
                    Value::Array(items) => items.contains(&filter.value),
                    _ => false,
                },
            };
            if !matched {
                include = false;
                break;
            }
        }
        if include {
            if selected_rows.len() == max_results {
                truncated = true;
                break;
            }
            let mut out = serde_json::Map::new();
            for field in &input.fields {
                out.insert(
                    field.clone(),
                    obj.get(field).cloned().unwrap_or(Value::Null),
                );
            }
            selected_rows.push(Value::Object(out));
        }
    }
    let count = selected_rows.len();
    Ok(JsonSelectOutput {
        rows: selected_rows,
        count,
        truncated,
    })
}

// ── fs_tree ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsTreeInput {
    pub path: String,
    pub max_depth: Option<usize>,
    pub include_hidden: Option<bool>,
    pub max_entries: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct FsTreeEntry {
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<FsTreeEntry>>,
}

#[derive(Debug, Serialize)]
pub struct FsTreeOutput {
    pub tree: FsTreeEntry,
    pub total_entries: usize,
    pub truncated: bool,
}

fn build_tree_entry(
    path: &Path,
    depth: usize,
    max_depth: usize,
    include_hidden: bool,
    max_entries: usize,
    total: &mut usize,
    truncated: &mut bool,
) -> ToolResult<FsTreeEntry> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());

    let metadata = std::fs::metadata(path).map_err(|_| ToolError {
        code: "PathNotFound".to_string(),
        message: format!("Cannot read metadata: {}", path.display()),
    })?;

    if metadata.is_file() {
        *total += 1;
        return Ok(FsTreeEntry {
            name,
            kind: "file".to_string(),
            size_bytes: Some(metadata.len()),
            children: None,
        });
    }

    *total += 1;

    if depth >= max_depth {
        return Ok(FsTreeEntry {
            name,
            kind: "dir".to_string(),
            size_bytes: None,
            children: None,
        });
    }

    let mut entries: Vec<_> = std::fs::read_dir(path)
        .map_err(|_| ToolError {
            code: "ReadFailed".to_string(),
            message: format!("Cannot read directory: {}", path.display()),
        })?
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut children = Vec::new();
    for entry in entries {
        if *truncated || *total >= max_entries {
            *truncated = true;
            break;
        }
        let entry_name = entry.file_name();
        if !include_hidden && entry_name.to_string_lossy().starts_with('.') {
            continue;
        }
        let child = build_tree_entry(
            &entry.path(),
            depth + 1,
            max_depth,
            include_hidden,
            max_entries,
            total,
            truncated,
        )?;
        children.push(child);
    }

    Ok(FsTreeEntry {
        name,
        kind: "dir".to_string(),
        size_bytes: None,
        children: Some(children),
    })
}

pub fn execute_fs_tree(input: FsTreeInput) -> ToolResult<FsTreeOutput> {
    let max_depth = input.max_depth.unwrap_or(4).min(MAX_TREE_DEPTH_HARD_CAP);
    let max_entries = input.max_entries.unwrap_or(MAX_TREE_ENTRIES_DEFAULT);
    if max_entries > MAX_TREE_ENTRIES_HARD_CAP {
        return Err(ToolError {
            code: "InvalidInput".to_string(),
            message: format!("max_entries cannot exceed {MAX_TREE_ENTRIES_HARD_CAP}"),
        });
    }
    let include_hidden = input.include_hidden.unwrap_or(false);
    let root = std::path::PathBuf::from(&input.path);
    if !root.exists() {
        return Err(ToolError {
            code: "PathNotFound".to_string(),
            message: format!("Path does not exist: {}", root.display()),
        });
    }
    let mut total = 0usize;
    let mut truncated = false;
    let tree = build_tree_entry(
        &root,
        0,
        max_depth,
        include_hidden,
        max_entries,
        &mut total,
        &mut truncated,
    )?;
    Ok(FsTreeOutput {
        tree,
        total_entries: total,
        truncated,
    })
}

// ── yaml_select ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct YamlSelectInput {
    pub path: String,
    pub fields: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct YamlSelectOutput {
    pub data: Value,
}

fn get_nested<'a>(value: &'a Value, path: &str) -> &'a Value {
    let mut current = value;
    for key in path.split('.') {
        match current.get(key) {
            Some(v) => current = v,
            None => return &Value::Null,
        }
    }
    current
}

pub fn execute_yaml_select(
    state: &mut ServerState,
    input: YamlSelectInput,
) -> ToolResult<YamlSelectOutput> {
    if input.fields.is_empty() || input.fields.len() > MAX_FIELDS {
        return Err(ToolError {
            code: "InvalidInput".to_string(),
            message: "fields must contain between 1 and 64 items".to_string(),
        });
    }
    let value = state.load_yaml(Path::new(&input.path))?;
    let mut out = serde_json::Map::new();
    for field in &input.fields {
        out.insert(field.clone(), get_nested(&value, field).clone());
    }
    Ok(YamlSelectOutput {
        data: Value::Object(out),
    })
}

// ── git_log ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitLogInput {
    pub repo_path: String,
    pub path_filter: Option<String>,
    pub max_results: Option<usize>,
    pub include_diff_stat: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct CommitEntry {
    pub sha: String,
    pub author: String,
    pub date: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub struct GitLogOutput {
    pub commits: Vec<CommitEntry>,
    pub count: usize,
    pub truncated: bool,
}

fn unix_to_iso8601(seconds: i64) -> String {
    if seconds < 0 {
        return "1970-01-01T00:00:00Z".to_string();
    }
    let s = seconds as u64;
    let days = s / 86400;
    let rem = s % 86400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let sec = rem % 60;
    // Civil date from days since Unix epoch — Hinnant's algorithm
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{sec:02}Z")
}

fn commit_touches_path(repo: &git2::Repository, commit: &git2::Commit, path_filter: &str) -> bool {
    let commit_tree = match commit.tree() {
        Ok(t) => t,
        Err(_) => return false,
    };
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    let mut opts = git2::DiffOptions::new();
    opts.pathspec(path_filter);
    let diff =
        match repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&commit_tree), Some(&mut opts)) {
            Ok(d) => d,
            Err(_) => return false,
        };
    diff.stats().map(|s| s.files_changed() > 0).unwrap_or(false)
}

fn commit_files(repo: &git2::Repository, commit: &git2::Commit) -> Vec<String> {
    let commit_tree = match commit.tree() {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
    let diff = match repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&commit_tree), None) {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    let mut files = Vec::new();
    for delta in diff.deltas() {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(|p| p.to_string_lossy().to_string());
        if let Some(p) = path {
            files.push(p);
        }
    }
    files
}

pub fn execute_git_log(input: GitLogInput) -> ToolResult<GitLogOutput> {
    let max_results = input.max_results.unwrap_or(MAX_GIT_COMMITS_DEFAULT);
    if max_results > MAX_GIT_COMMITS_HARD_CAP {
        return Err(ToolError {
            code: "InvalidInput".to_string(),
            message: format!("max_results cannot exceed {MAX_GIT_COMMITS_HARD_CAP}"),
        });
    }
    let include_diff_stat = input.include_diff_stat.unwrap_or(false);

    let repo = git2::Repository::discover(&input.repo_path).map_err(|e| ToolError {
        code: "GitError".to_string(),
        message: format!("Cannot open repository: {}", e.message()),
    })?;

    let mut revwalk = repo.revwalk().map_err(|e| ToolError {
        code: "GitError".to_string(),
        message: format!("Cannot walk repository: {}", e.message()),
    })?;
    revwalk.push_head().map_err(|e| ToolError {
        code: "GitError".to_string(),
        message: format!("Cannot read HEAD: {}", e.message()),
    })?;

    let mut commits = Vec::new();
    let mut truncated = false;

    for oid in revwalk {
        let oid = oid.map_err(|e| ToolError {
            code: "GitError".to_string(),
            message: format!("Walk error: {}", e.message()),
        })?;
        let commit = repo.find_commit(oid).map_err(|e| ToolError {
            code: "GitError".to_string(),
            message: format!("Cannot read commit: {}", e.message()),
        })?;

        if input
            .path_filter
            .as_deref()
            .is_some_and(|filter| !commit_touches_path(&repo, &commit, filter))
        {
            continue;
        }

        if commits.len() == max_results {
            truncated = true;
            break;
        }

        let files_changed = if include_diff_stat {
            Some(commit_files(&repo, &commit))
        } else {
            None
        };

        commits.push(CommitEntry {
            sha: oid.to_string(),
            author: commit.author().name().unwrap_or("Unknown").to_string(),
            date: unix_to_iso8601(commit.time().seconds()),
            message: commit.message().unwrap_or("").trim().to_string(),
            files_changed,
        });
    }

    let count = commits.len();
    Ok(GitLogOutput {
        commits,
        count,
        truncated,
    })
}

pub fn tool_definitions() -> Value {
    json!({
        "tools": [
            {
                "name": "fs_glob",
                "description": "Deterministic filesystem globbing with output caps",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["base_path", "patterns"],
                    "properties": {
                        "base_path": {"type": "string"},
                        "patterns": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_PATHS},
                        "max_results": {"type": "integer", "minimum": 1, "maximum": 5000}
                    }
                }
            },
            {
                "name": "text_search",
                "description": "Regex/literal text search with line + byte offsets",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["paths", "query", "mode"],
                    "properties": {
                        "paths": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_PATHS},
                        "query": {"type": "string", "minLength": 1, "maxLength": MAX_PATTERN_LENGTH},
                        "mode": {"type": "string", "enum": ["literal", "regex"]},
                        "case_sensitive": {"type": "boolean"},
                        "max_results": {"type": "integer", "minimum": 1, "maximum": 5000}
                    }
                }
            },
            {
                "name": "json_select",
                "description": "Subset JSON selection with explicit fields and filters",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path", "fields"],
                    "properties": {
                        "path": {"type": "string"},
                        "fields": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_FIELDS},
                        "filters": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "required": ["field", "op", "value"],
                                "properties": {
                                    "field": {"type": "string"},
                                    "op": {"type": "string", "enum": ["eq", "contains"]},
                                    "value": {}
                                }
                            }
                        },
                        "max_results": {"type": "integer", "minimum": 1, "maximum": 5000}
                    }
                }
            },
            {
                "name": "server_stats",
                "description": "Lightweight counters for requests and cache behavior",
                "inputSchema": {"type": "object", "additionalProperties": false}
            },
            {
                "name": "fs_tree",
                "description": "Depth-limited directory tree as structured JSON. Use for project orientation before diving into specifics.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path"],
                    "properties": {
                        "path": {"type": "string"},
                        "max_depth": {"type": "integer", "minimum": 1, "maximum": 10},
                        "include_hidden": {"type": "boolean"},
                        "max_entries": {"type": "integer", "minimum": 1, "maximum": 2000}
                    }
                }
            },
            {
                "name": "yaml_select",
                "description": "Extract specific fields from YAML (.yml/.yaml) or TOML (.toml) files using dot-notation paths. Returns only the requested fields, saving context.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path", "fields"],
                    "properties": {
                        "path": {"type": "string"},
                        "fields": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_FIELDS}
                    }
                }
            },
            {
                "name": "git_log",
                "description": "Query git commit history with optional path filter. Returns structured commit data without shell access.",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["repo_path"],
                    "properties": {
                        "repo_path": {"type": "string"},
                        "path_filter": {"type": "string"},
                        "max_results": {"type": "integer", "minimum": 1, "maximum": 500},
                        "include_diff_stat": {"type": "boolean"}
                    }
                }
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn fs_glob_is_sorted_and_capped() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("b.txt"), "x").unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        let out = execute_fs_glob(FsGlobInput {
            base_path: dir.path().to_string_lossy().to_string(),
            patterns: vec!["*.txt".to_string()],
            max_results: Some(1),
        })
        .unwrap();
        assert_eq!(out.count, 2);
        assert!(out.truncated);
        assert_eq!(out.paths.len(), 1);
        assert!(out.paths[0].ends_with("a.txt"));
    }

    #[test]
    fn text_search_is_deterministic_and_reports_offsets() {
        let dir = tempdir().unwrap();
        let first = dir.path().join("b.log");
        let second = dir.path().join("a.log");
        fs::write(&first, "zzz\nneedle\n").unwrap();
        fs::write(&second, "needle\n").unwrap();
        let mut state = ServerState::new();
        let out = execute_text_search(
            &mut state,
            TextSearchInput {
                paths: vec![
                    first.to_string_lossy().to_string(),
                    second.to_string_lossy().to_string(),
                ],
                query: "needle".to_string(),
                mode: SearchMode::Literal,
                case_sensitive: Some(true),
                max_results: Some(5),
            },
        )
        .unwrap();
        assert_eq!(out.count, 2);
        assert_eq!(out.matches[0].line, 1);
        assert!(out.matches[0].file.ends_with("a.log"));
        assert_eq!(out.matches[0].byte_start, 0);
    }

    #[test]
    fn text_search_handles_last_line_without_newline() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("single.log");
        fs::write(&path, "needle").unwrap();
        let mut state = ServerState::new();
        let out = execute_text_search(
            &mut state,
            TextSearchInput {
                paths: vec![path.to_string_lossy().to_string()],
                query: "needle".to_string(),
                mode: SearchMode::Literal,
                case_sensitive: Some(true),
                max_results: Some(10),
            },
        )
        .unwrap();
        assert_eq!(out.count, 1);
        assert_eq!(out.matches[0].line, 1);
        assert_eq!(out.matches[0].byte_end, 6);
    }

    #[test]
    fn json_select_filters_and_uses_cache() {
        let dir = tempdir().unwrap();
        let json_path = dir.path().join("data.json");
        fs::write(
            &json_path,
            r#"[{"name":"alpha","tags":["x"]},{"name":"beta","tags":["y"]}]"#,
        )
        .unwrap();

        let mut state = ServerState::new();
        let input = JsonSelectInput {
            path: json_path.to_string_lossy().to_string(),
            fields: vec!["name".to_string()],
            filters: Some(vec![JsonFilter {
                field: "name".to_string(),
                op: JsonFilterOp::Contains,
                value: Value::String("alp".to_string()),
            }]),
            max_results: Some(10),
        };
        let out = execute_json_select(&mut state, input).unwrap();
        assert_eq!(out.count, 1);
        assert_eq!(out.rows[0]["name"], Value::String("alpha".to_string()));

        let input2 = JsonSelectInput {
            path: json_path.to_string_lossy().to_string(),
            fields: vec!["name".to_string()],
            filters: None,
            max_results: Some(10),
        };
        let _ = execute_json_select(&mut state, input2).unwrap();
        assert!(state.cache_hits >= 1);
    }

    #[test]
    fn rejects_max_results_over_hard_cap() {
        let dir = tempdir().unwrap();
        let txt_path = dir.path().join("a.txt");
        fs::write(&txt_path, "x").unwrap();
        let glob_err = execute_fs_glob(FsGlobInput {
            base_path: dir.path().to_string_lossy().to_string(),
            patterns: vec!["*.txt".to_string()],
            max_results: Some(MAX_RESULTS_HARD_CAP + 1),
        })
        .unwrap_err();
        assert_eq!(glob_err.code, "InvalidInput");

        let mut state = ServerState::new();
        let search_err = execute_text_search(
            &mut state,
            TextSearchInput {
                paths: vec![txt_path.to_string_lossy().to_string()],
                query: "x".to_string(),
                mode: SearchMode::Literal,
                case_sensitive: Some(true),
                max_results: Some(MAX_RESULTS_HARD_CAP + 1),
            },
        )
        .unwrap_err();
        assert_eq!(search_err.code, "InvalidInput");

        let json_path = dir.path().join("data.json");
        fs::write(&json_path, r#"[{"name":"x"}]"#).unwrap();
        let json_err = execute_json_select(
            &mut state,
            JsonSelectInput {
                path: json_path.to_string_lossy().to_string(),
                fields: vec!["name".to_string()],
                filters: None,
                max_results: Some(MAX_RESULTS_HARD_CAP + 1),
            },
        )
        .unwrap_err();
        assert_eq!(json_err.code, "InvalidInput");
    }

    #[test]
    fn truncated_is_false_when_exactly_at_limit_without_extra_matches() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("x.log");
        fs::write(&file_path, "needle\n").unwrap();
        let mut state = ServerState::new();
        let text = execute_text_search(
            &mut state,
            TextSearchInput {
                paths: vec![file_path.to_string_lossy().to_string()],
                query: "needle".to_string(),
                mode: SearchMode::Literal,
                case_sensitive: Some(true),
                max_results: Some(1),
            },
        )
        .unwrap();
        assert_eq!(text.count, 1);
        assert!(!text.truncated);

        let json_path = dir.path().join("single.json");
        fs::write(&json_path, r#"[{"name":"one"}]"#).unwrap();
        let json = execute_json_select(
            &mut state,
            JsonSelectInput {
                path: json_path.to_string_lossy().to_string(),
                fields: vec!["name".to_string()],
                filters: None,
                max_results: Some(1),
            },
        )
        .unwrap();
        assert_eq!(json.count, 1);
        assert!(!json.truncated);
    }

    #[test]
    fn fs_tree_returns_sorted_structure_and_excludes_hidden() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("b.rs"), "x").unwrap();
        fs::write(dir.path().join("a.rs"), "x").unwrap();
        fs::write(dir.path().join(".hidden"), "x").unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();
        fs::write(dir.path().join("subdir").join("c.rs"), "x").unwrap();

        let out = execute_fs_tree(FsTreeInput {
            path: dir.path().to_string_lossy().to_string(),
            max_depth: Some(2),
            include_hidden: Some(false),
            max_entries: None,
        })
        .unwrap();

        assert_eq!(out.tree.kind, "dir");
        let children = out.tree.children.unwrap();
        // hidden file excluded
        assert!(!children.iter().any(|c| c.name == ".hidden"));
        // sorted: a.rs < b.rs < subdir
        assert_eq!(children[0].name, "a.rs");
        assert_eq!(children[1].name, "b.rs");
        assert_eq!(children[2].name, "subdir");
        assert_eq!(children[2].kind, "dir");
        // subdir children visible at depth 2
        let sub_children = children[2].children.as_ref().unwrap();
        assert_eq!(sub_children[0].name, "c.rs");
        assert!(!out.truncated);
    }

    #[test]
    fn fs_tree_caps_entries_and_sets_truncated() {
        let dir = tempdir().unwrap();
        for i in 0..5 {
            fs::write(dir.path().join(format!("{i}.txt")), "x").unwrap();
        }
        let out = execute_fs_tree(FsTreeInput {
            path: dir.path().to_string_lossy().to_string(),
            max_depth: None,
            include_hidden: None,
            max_entries: Some(3),
        })
        .unwrap();
        assert!(out.truncated);
        assert!(out.total_entries <= 3);
    }

    #[test]
    fn fs_tree_rejects_max_entries_over_hard_cap() {
        let dir = tempdir().unwrap();
        let err = execute_fs_tree(FsTreeInput {
            path: dir.path().to_string_lossy().to_string(),
            max_depth: None,
            include_hidden: None,
            max_entries: Some(MAX_TREE_ENTRIES_HARD_CAP + 1),
        })
        .unwrap_err();
        assert_eq!(err.code, "InvalidInput");
    }

    #[test]
    fn yaml_select_extracts_nested_fields_from_toml() {
        let dir = tempdir().unwrap();
        let toml_path = dir.path().join("Cargo.toml");
        fs::write(
            &toml_path,
            "[package]\nname = \"toolpilot\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let mut state = ServerState::new();
        let out = execute_yaml_select(
            &mut state,
            YamlSelectInput {
                path: toml_path.to_string_lossy().to_string(),
                fields: vec!["package.name".to_string(), "package.version".to_string()],
            },
        )
        .unwrap();
        assert_eq!(
            out.data["package.name"],
            Value::String("toolpilot".to_string())
        );
        assert_eq!(
            out.data["package.version"],
            Value::String("0.1.0".to_string())
        );
    }

    #[test]
    fn yaml_select_extracts_fields_from_yaml() {
        let dir = tempdir().unwrap();
        let yaml_path = dir.path().join("config.yml");
        fs::write(&yaml_path, "server:\n  port: 8080\nenv: production\n").unwrap();
        let mut state = ServerState::new();
        let out = execute_yaml_select(
            &mut state,
            YamlSelectInput {
                path: yaml_path.to_string_lossy().to_string(),
                fields: vec!["server.port".to_string(), "env".to_string()],
            },
        )
        .unwrap();
        assert_eq!(out.data["server.port"], Value::Number(8080.into()));
        assert_eq!(out.data["env"], Value::String("production".to_string()));
    }

    #[test]
    fn yaml_select_returns_null_for_missing_field() {
        let dir = tempdir().unwrap();
        let yaml_path = dir.path().join("data.yaml");
        fs::write(&yaml_path, "key: value\n").unwrap();
        let mut state = ServerState::new();
        let out = execute_yaml_select(
            &mut state,
            YamlSelectInput {
                path: yaml_path.to_string_lossy().to_string(),
                fields: vec!["missing.field".to_string()],
            },
        )
        .unwrap();
        assert_eq!(out.data["missing.field"], Value::Null);
    }

    #[test]
    fn yaml_select_uses_cache() {
        let dir = tempdir().unwrap();
        let toml_path = dir.path().join("test.toml");
        fs::write(&toml_path, "[section]\nkey = \"val\"\n").unwrap();
        let mut state = ServerState::new();
        let input1 = YamlSelectInput {
            path: toml_path.to_string_lossy().to_string(),
            fields: vec!["section.key".to_string()],
        };
        let _ = execute_yaml_select(&mut state, input1).unwrap();
        let input2 = YamlSelectInput {
            path: toml_path.to_string_lossy().to_string(),
            fields: vec!["section.key".to_string()],
        };
        let _ = execute_yaml_select(&mut state, input2).unwrap();
        assert!(state.cache_hits >= 1);
    }

    #[test]
    fn yaml_select_rejects_unsupported_extension() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("data.xml");
        fs::write(&path, "<root/>").unwrap();
        let mut state = ServerState::new();
        let err = execute_yaml_select(
            &mut state,
            YamlSelectInput {
                path: path.to_string_lossy().to_string(),
                fields: vec!["key".to_string()],
            },
        )
        .unwrap_err();
        assert_eq!(err.code, "UnsupportedFormat");
    }

    #[test]
    fn git_log_returns_commits_for_current_repo() {
        // Use the toolpilot repo itself (must have at least one commit)
        let out = execute_git_log(GitLogInput {
            repo_path: ".".to_string(),
            path_filter: None,
            max_results: Some(5),
            include_diff_stat: Some(false),
        })
        .unwrap();
        assert!(!out.commits.is_empty());
        // SHA should be a valid 40-char hex string
        assert_eq!(out.commits[0].sha.len(), 40);
    }

    #[test]
    fn git_log_rejects_over_hard_cap() {
        let err = execute_git_log(GitLogInput {
            repo_path: ".".to_string(),
            path_filter: None,
            max_results: Some(MAX_GIT_COMMITS_HARD_CAP + 1),
            include_diff_stat: None,
        })
        .unwrap_err();
        assert_eq!(err.code, "InvalidInput");
    }

    #[test]
    fn unix_to_iso8601_formats_epoch_correctly() {
        assert_eq!(unix_to_iso8601(0), "1970-01-01T00:00:00Z");
    }
}
