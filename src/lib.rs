use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::SystemTime;

use glob::{Pattern, glob};
use md5::Md5;
use memmap2::Mmap;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha256};

pub const MAX_PATHS: usize = 128;
pub const MAX_PATTERN_LENGTH: usize = 512;
pub const MAX_FIELDS: usize = 64;
pub const MAX_RESULTS_DEFAULT: usize = 200;
pub const MAX_RESULTS_HARD_CAP: usize = 5000;

pub const MAX_TREE_DEPTH_HARD_CAP: usize = 10;
pub const MAX_TREE_ENTRIES_DEFAULT: usize = 200;
pub const MAX_TREE_ENTRIES_HARD_CAP: usize = 2000;

pub const MAX_READ_BYTES_DEFAULT: usize = 16 * 1024;
pub const MAX_READ_BYTES_HARD_CAP: usize = 1024 * 1024;
pub const MAX_DIFF_LINES_DEFAULT: usize = 200;
pub const MAX_DIFF_LINES_HARD_CAP: usize = 2000;
pub const MAX_GIT_LOG_RESULTS_DEFAULT: usize = 20;
pub const MAX_GIT_LOG_RESULTS_HARD_CAP: usize = 500;

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

fn resolve_max_read_bytes(max_bytes: Option<usize>) -> ToolResult<usize> {
    let value = max_bytes.unwrap_or(MAX_READ_BYTES_DEFAULT).max(1);
    if value > MAX_READ_BYTES_HARD_CAP {
        return Err(ToolError {
            code: "InvalidInput".to_string(),
            message: format!("max_bytes cannot exceed {}", MAX_READ_BYTES_HARD_CAP),
        });
    }
    Ok(value)
}

fn resolve_max_diff_lines(max_lines: Option<usize>) -> ToolResult<usize> {
    let value = max_lines.unwrap_or(MAX_DIFF_LINES_DEFAULT).max(1);
    if value > MAX_DIFF_LINES_HARD_CAP {
        return Err(ToolError {
            code: "InvalidInput".to_string(),
            message: format!("max_diff_lines cannot exceed {}", MAX_DIFF_LINES_HARD_CAP),
        });
    }
    Ok(value)
}

fn resolve_max_git_log_results(max_results: Option<usize>) -> ToolResult<usize> {
    let value = max_results.unwrap_or(MAX_GIT_LOG_RESULTS_DEFAULT).max(1);
    if value > MAX_GIT_LOG_RESULTS_HARD_CAP {
        return Err(ToolError {
            code: "InvalidInput".to_string(),
            message: format!("max_results cannot exceed {}", MAX_GIT_LOG_RESULTS_HARD_CAP),
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
    json_cache_evictions: u64,
    text_cache_evictions: u64,
    regex_cache_evictions: u64,
    yaml_cache_evictions: u64,
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
        let cache_entries = json!({
            "json": self.json_cache.len(),
            "text": self.text_cache.len(),
            "regex": self.regex_cache.len(),
            "yaml": self.yaml_cache.len(),
            "total": self.json_cache.len() + self.text_cache.len() + self.regex_cache.len() + self.yaml_cache.len()
        });
        let cache_evictions = json!({
            "json": self.json_cache_evictions,
            "text": self.text_cache_evictions,
            "regex": self.regex_cache_evictions,
            "yaml": self.yaml_cache_evictions,
            "total": self.json_cache_evictions + self.text_cache_evictions + self.regex_cache_evictions + self.yaml_cache_evictions
        });
        json!({
            "requests_per_tool": self.requests_per_tool,
            "cache": {
                "hits": self.cache_hits,
                "misses": self.cache_misses,
                "entries": cache_entries,
                "evictions": cache_evictions
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
    pub glob: Option<String>,
    pub file_type: Option<String>,
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
    let glob_pattern = match input.glob {
        Some(pattern) => {
            if pattern.len() > MAX_PATTERN_LENGTH {
                return Err(ToolError {
                    code: "InvalidPattern".to_string(),
                    message: "glob length is invalid".to_string(),
                });
            }
            Some(Pattern::new(&pattern).map_err(|_| ToolError {
                code: "InvalidPattern".to_string(),
                message: "glob failed to compile".to_string(),
            })?)
        }
        None => None,
    };
    let file_type = input
        .file_type
        .map(|ft| ft.trim_start_matches('.').to_string());
    if let Some(ft) = &file_type
        && (ft.is_empty() || ft.len() > MAX_PATTERN_LENGTH)
    {
        return Err(ToolError {
            code: "InvalidPattern".to_string(),
            message: "file_type length is invalid".to_string(),
        });
    }
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
        let path_obj = Path::new(&path);
        if let Some(pattern) = &glob_pattern
            && !pattern.matches_path(path_obj)
        {
            continue;
        }
        if let Some(ft) = &file_type {
            let ext_matches = path_obj
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext == ft);
            if !ext_matches {
                continue;
            }
        }
        let mmap = state.load_text(Path::new(&path))?;
        let content = std::str::from_utf8(&mmap).map_err(|_| ToolError {
            code: "UnsupportedEncoding".to_string(),
            message: format!("File is not valid UTF-8: {path}"),
        })?;
        let mut line_start = 0usize;
        for (line_no, line) in (1usize..).zip(content.split_inclusive('\n')) {
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
#[serde(deny_unknown_fields)]
pub struct ReadFileInput {
    pub path: String,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct ReadFileOutput {
    pub content: String,
    pub total_lines: usize,
    pub returned_lines: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub bytes: usize,
    pub truncated: bool,
}

fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> (&str, bool) {
    if s.len() <= max_bytes {
        return (s, false);
    }
    let mut idx = max_bytes;
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    (&s[..idx], true)
}

pub fn execute_read_file(
    state: &mut ServerState,
    input: ReadFileInput,
) -> ToolResult<ReadFileOutput> {
    let max_bytes = resolve_max_read_bytes(input.max_bytes)?;
    let start_line = input.start_line.unwrap_or(1).max(1);
    let end_line = input.end_line.unwrap_or(usize::MAX);
    if end_line < start_line {
        return Err(ToolError {
            code: "InvalidInput".to_string(),
            message: "end_line must be greater than or equal to start_line".to_string(),
        });
    }

    let mmap = state.load_text(Path::new(&input.path))?;
    let content = std::str::from_utf8(&mmap).map_err(|_| ToolError {
        code: "UnsupportedEncoding".to_string(),
        message: format!("File is not valid UTF-8: {}", input.path),
    })?;
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();
    if total_lines == 0 {
        return Ok(ReadFileOutput {
            content: String::new(),
            total_lines: 0,
            returned_lines: 0,
            start_line,
            end_line: start_line.saturating_sub(1),
            bytes: 0,
            truncated: false,
        });
    }

    let start_idx = start_line.saturating_sub(1).min(total_lines);
    let end_idx_exclusive = end_line.min(total_lines);
    if start_idx >= end_idx_exclusive {
        return Ok(ReadFileOutput {
            content: String::new(),
            total_lines,
            returned_lines: 0,
            start_line,
            end_line: end_idx_exclusive,
            bytes: 0,
            truncated: false,
        });
    }

    let joined = lines[start_idx..end_idx_exclusive].join("\n");
    let (trimmed, hit_byte_cap) = truncate_to_char_boundary(&joined, max_bytes);
    let returned_lines = if trimmed.is_empty() {
        0
    } else {
        trimmed.lines().count()
    };
    let range_truncated = end_idx_exclusive < total_lines;
    Ok(ReadFileOutput {
        content: trimmed.to_string(),
        total_lines,
        returned_lines,
        start_line,
        end_line: if returned_lines == 0 {
            start_line.saturating_sub(1)
        } else {
            start_line + returned_lines - 1
        },
        bytes: trimmed.len(),
        truncated: hit_byte_cap || range_truncated,
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

    let metadata = std::fs::symlink_metadata(path).map_err(|_| ToolError {
        code: "PathNotFound".to_string(),
        message: format!("Cannot read metadata: {}", path.display()),
    })?;
    let file_type = metadata.file_type();

    if file_type.is_symlink() {
        *total += 1;
        return Ok(FsTreeEntry {
            name,
            kind: "symlink".to_string(),
            size_bytes: None,
            children: None,
        });
    }

    if file_type.is_file() {
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
    let max_depth = input
        .max_depth
        .unwrap_or(4)
        .clamp(1, MAX_TREE_DEPTH_HARD_CAP);
    let max_entries = input.max_entries.unwrap_or(MAX_TREE_ENTRIES_DEFAULT).max(1);
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
        if let Ok(index) = key.parse::<usize>() {
            match current.as_array().and_then(|arr| arr.get(index)) {
                Some(v) => current = v,
                None => return &Value::Null,
            }
        } else {
            match current.get(key) {
                Some(v) => current = v,
                None => return &Value::Null,
            }
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HashAlgorithm {
    Sha256,
    Sha1,
    Md5,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileHashInput {
    pub paths: Vec<String>,
    pub algorithm: Option<HashAlgorithm>,
}

#[derive(Debug, Serialize)]
pub struct FileHashEntry {
    pub path: String,
    pub algorithm: String,
    pub hash: String,
}

#[derive(Debug, Serialize)]
pub struct FileHashOutput {
    pub hashes: Vec<FileHashEntry>,
    pub count: usize,
}

fn digest_file_with_hasher<D: Digest + Default>(path: &Path) -> ToolResult<String> {
    let mut file = File::open(path).map_err(|_| ToolError {
        code: "ReadFailed".to_string(),
        message: format!("Failed to read file: {}", path.display()),
    })?;
    let mut hasher = D::default();
    let mut buffer = [0u8; 8192];
    loop {
        let bytes_read = file.read(&mut buffer).map_err(|_| ToolError {
            code: "ReadFailed".to_string(),
            message: format!("Failed to read file: {}", path.display()),
        })?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut hex, "{byte:02x}");
    }
    Ok(hex)
}

pub fn execute_file_hash(input: FileHashInput) -> ToolResult<FileHashOutput> {
    if input.paths.is_empty() || input.paths.len() > MAX_PATHS {
        return Err(ToolError {
            code: "InvalidInput".to_string(),
            message: "paths must contain between 1 and 128 items".to_string(),
        });
    }
    let algorithm = input.algorithm.unwrap_or(HashAlgorithm::Sha256);
    let algorithm_name = match algorithm {
        HashAlgorithm::Sha256 => "sha256",
        HashAlgorithm::Sha1 => "sha1",
        HashAlgorithm::Md5 => "md5",
    };

    let mut paths = input.paths;
    paths.sort();
    paths.dedup();

    let mut hashes = Vec::with_capacity(paths.len());
    for path in paths {
        let path_ref = Path::new(&path);
        let digest = match algorithm {
            HashAlgorithm::Sha256 => digest_file_with_hasher::<Sha256>(path_ref)?,
            HashAlgorithm::Sha1 => digest_file_with_hasher::<Sha1>(path_ref)?,
            HashAlgorithm::Md5 => digest_file_with_hasher::<Md5>(path_ref)?,
        };
        hashes.push(FileHashEntry {
            path,
            algorithm: algorithm_name.to_string(),
            hash: digest,
        });
    }

    Ok(FileHashOutput {
        count: hashes.len(),
        hashes,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitLogInput {
    pub repo_path: String,
    pub max_results: Option<usize>,
    pub path_filter: Option<String>,
    pub include_diff_stat: Option<bool>,
    pub include_diff: Option<bool>,
    pub max_diff_lines: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct GitDiffStatEntry {
    pub path: String,
    pub insertions: Option<usize>,
    pub deletions: Option<usize>,
    pub binary: bool,
}

#[derive(Debug, Serialize)]
pub struct GitCommit {
    pub sha: String,
    pub author_name: String,
    pub author_email: String,
    pub date: String,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_stat: Option<Vec<GitDiffStatEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_truncated: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct GitLogOutput {
    pub commits: Vec<GitCommit>,
    pub count: usize,
}

fn execute_git(repo_path: &Path, args: &[String]) -> ToolResult<String> {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(args)
        .output()
        .map_err(|e| ToolError {
            code: "ExecutionFailed".to_string(),
            message: format!("Failed to execute git: {e}"),
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(ToolError {
            code: "GitCommandFailed".to_string(),
            message: if stderr.is_empty() {
                format!("Git command failed: git {}", args.join(" "))
            } else {
                stderr
            },
        });
    }
    String::from_utf8(output.stdout).map_err(|_| ToolError {
        code: "UnsupportedEncoding".to_string(),
        message: "Git output is not valid UTF-8".to_string(),
    })
}

fn parse_numstat(output: &str) -> Vec<GitDiffStatEntry> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let insertions = parts.next()?;
            let deletions = parts.next()?;
            let path = parts.next()?.to_string();
            let binary = insertions == "-" || deletions == "-";
            Some(GitDiffStatEntry {
                path,
                insertions: insertions.parse::<usize>().ok(),
                deletions: deletions.parse::<usize>().ok(),
                binary,
            })
        })
        .collect()
}

fn truncate_diff_lines(diff: &str, max_lines: usize) -> (String, bool) {
    let mut out = String::new();
    let mut truncated = false;
    for (count, line) in diff.lines().enumerate() {
        if count == max_lines {
            truncated = true;
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    (out, truncated)
}

pub fn execute_git_log(input: GitLogInput) -> ToolResult<GitLogOutput> {
    let repo_path = PathBuf::from(&input.repo_path);
    let max_results = resolve_max_git_log_results(input.max_results)?;
    let max_diff_lines = resolve_max_diff_lines(input.max_diff_lines)?;
    let include_diff_stat = input.include_diff_stat.unwrap_or(false);
    let include_diff = input.include_diff.unwrap_or(false);

    let mut log_args = vec![
        "--no-pager".to_string(),
        "log".to_string(),
        format!("-n{max_results}"),
        "--date=iso-strict".to_string(),
        "--pretty=format:%H%x1f%an%x1f%ae%x1f%ad%x1f%s%x1e".to_string(),
    ];
    if let Some(path_filter) = &input.path_filter {
        log_args.push("--".to_string());
        log_args.push(path_filter.clone());
    }
    let log_output = execute_git(&repo_path, &log_args)?;

    let mut commits = Vec::new();
    for entry in log_output.split('\x1e') {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut fields = trimmed.split('\x1f');
        let Some(sha) = fields.next() else { continue };
        let Some(author_name) = fields.next() else {
            continue;
        };
        let Some(author_email) = fields.next() else {
            continue;
        };
        let Some(date) = fields.next() else { continue };
        let Some(subject) = fields.next() else {
            continue;
        };

        let mut commit = GitCommit {
            sha: sha.to_string(),
            author_name: author_name.to_string(),
            author_email: author_email.to_string(),
            date: date.to_string(),
            subject: subject.to_string(),
            diff_stat: None,
            diff: None,
            diff_truncated: None,
        };

        if include_diff_stat {
            let mut stat_args = vec![
                "--no-pager".to_string(),
                "show".to_string(),
                "--format=".to_string(),
                "--numstat".to_string(),
                "--no-renames".to_string(),
                commit.sha.clone(),
            ];
            if let Some(path_filter) = &input.path_filter {
                stat_args.push("--".to_string());
                stat_args.push(path_filter.clone());
            }
            let stat_output = execute_git(&repo_path, &stat_args)?;
            commit.diff_stat = Some(parse_numstat(&stat_output));
        }

        if include_diff {
            let mut diff_args = vec![
                "--no-pager".to_string(),
                "show".to_string(),
                "--format=".to_string(),
                "--unified=3".to_string(),
                commit.sha.clone(),
            ];
            if let Some(path_filter) = &input.path_filter {
                diff_args.push("--".to_string());
                diff_args.push(path_filter.clone());
            }
            let diff_output = execute_git(&repo_path, &diff_args)?;
            let (diff, diff_truncated) = truncate_diff_lines(&diff_output, max_diff_lines);
            commit.diff = Some(diff);
            commit.diff_truncated = Some(diff_truncated);
        }

        commits.push(commit);
    }

    Ok(GitLogOutput {
        count: commits.len(),
        commits,
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
                "description": "Regex/literal text search with line + byte offsets and optional path filtering",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["paths", "query", "mode"],
                    "properties": {
                        "paths": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_PATHS},
                        "query": {"type": "string", "minLength": 1, "maxLength": MAX_PATTERN_LENGTH},
                        "mode": {"type": "string", "enum": ["literal", "regex"]},
                        "glob": {"type": "string", "maxLength": MAX_PATTERN_LENGTH},
                        "file_type": {"type": "string", "maxLength": MAX_PATTERN_LENGTH},
                        "case_sensitive": {"type": "boolean"},
                        "max_results": {"type": "integer", "minimum": 1, "maximum": 5000}
                    }
                }
            },
            {
                "name": "read_file",
                "description": "Read a UTF-8 text file with optional line range and byte cap",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path"],
                    "properties": {
                        "path": {"type": "string"},
                        "start_line": {"type": "integer", "minimum": 1},
                        "end_line": {"type": "integer", "minimum": 1},
                        "max_bytes": {"type": "integer", "minimum": 1, "maximum": MAX_READ_BYTES_HARD_CAP}
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
                "description": "Lightweight counters for requests and cache behavior, including cache entries and evictions",
                "inputSchema": {"type": "object", "additionalProperties": false}
            },
            {
                "name": "fs_tree",
                "description": "Depth-limited directory tree as structured JSON. Set include_hidden=true to include hidden files and directories.",
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
                "description": "Extract specific fields from YAML (.yml/.yaml) or TOML (.toml) files using dot-notation paths (including array indexes like jobs.0.steps).",
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
                "name": "file_hash",
                "description": "Compute file checksums (sha256 by default, plus sha1 and md5)",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["paths"],
                    "properties": {
                        "paths": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_PATHS},
                        "algorithm": {"type": "string", "enum": ["sha256", "sha1", "md5"]}
                    }
                }
            },
            {
                "name": "git_log",
                "description": "Query git commit history with optional file stats and unified diff output",
                "inputSchema": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["repo_path"],
                    "properties": {
                        "repo_path": {"type": "string"},
                        "max_results": {"type": "integer", "minimum": 1, "maximum": MAX_GIT_LOG_RESULTS_HARD_CAP},
                        "path_filter": {"type": "string"},
                        "include_diff_stat": {"type": "boolean"},
                        "include_diff": {"type": "boolean"},
                        "max_diff_lines": {"type": "integer", "minimum": 1, "maximum": MAX_DIFF_LINES_HARD_CAP}
                    }
                }
            },
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
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
                glob: None,
                file_type: None,
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
                glob: None,
                file_type: None,
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
    fn text_search_supports_glob_and_file_type_filters() {
        let dir = tempdir().unwrap();
        let py = dir.path().join("a.py");
        let txt = dir.path().join("b.txt");
        fs::write(&py, "needle\n").unwrap();
        fs::write(&txt, "needle\n").unwrap();

        let mut state = ServerState::new();
        let out = execute_text_search(
            &mut state,
            TextSearchInput {
                paths: vec![
                    py.to_string_lossy().to_string(),
                    txt.to_string_lossy().to_string(),
                ],
                query: "needle".to_string(),
                mode: SearchMode::Literal,
                glob: Some("*.py".to_string()),
                file_type: Some("py".to_string()),
                case_sensitive: Some(true),
                max_results: Some(10),
            },
        )
        .unwrap();
        assert_eq!(out.count, 1);
        assert!(out.matches[0].file.ends_with("a.py"));
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
                glob: None,
                file_type: None,
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
                glob: None,
                file_type: None,
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
    fn read_file_supports_line_ranges_and_byte_cap() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("sample.txt");
        fs::write(&file, "one\ntwo\nthree\nfour\n").unwrap();

        let mut state = ServerState::new();
        let out = execute_read_file(
            &mut state,
            ReadFileInput {
                path: file.to_string_lossy().to_string(),
                start_line: Some(2),
                end_line: Some(4),
                max_bytes: Some(7),
            },
        )
        .unwrap();

        assert_eq!(out.content, "two\nthr");
        assert_eq!(out.start_line, 2);
        assert_eq!(out.end_line, 3);
        assert!(out.truncated);
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
    fn fs_tree_normalizes_zero_limits_to_schema_minimum() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        fs::write(dir.path().join("b.txt"), "x").unwrap();
        let out = execute_fs_tree(FsTreeInput {
            path: dir.path().to_string_lossy().to_string(),
            max_depth: Some(0),
            include_hidden: None,
            max_entries: Some(0),
        })
        .unwrap();

        assert_eq!(out.total_entries, 1);
        assert!(out.truncated);
        let root_has_no_children = out
            .tree
            .children
            .as_ref()
            .is_some_and(|children| children.is_empty());
        assert!(root_has_no_children);
    }

    #[test]
    fn fs_tree_represents_symlink_without_recursing() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("inside.txt"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, root.join("link")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target, root.join("link")).unwrap();

        let out = execute_fs_tree(FsTreeInput {
            path: root.to_string_lossy().to_string(),
            max_depth: Some(4),
            include_hidden: Some(true),
            max_entries: Some(10),
        })
        .unwrap();

        let children = out.tree.children.unwrap();
        let link = children.iter().find(|entry| entry.name == "link").unwrap();
        assert_eq!(link.kind, "symlink");
        assert!(link.children.is_none());
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
    fn yaml_select_supports_array_index_paths() {
        let dir = tempdir().unwrap();
        let yaml_path = dir.path().join("pipeline.yml");
        fs::write(
            &yaml_path,
            "jobs:\n  - steps:\n      - run: test\n      - run: lint\n",
        )
        .unwrap();
        let mut state = ServerState::new();
        let out = execute_yaml_select(
            &mut state,
            YamlSelectInput {
                path: yaml_path.to_string_lossy().to_string(),
                fields: vec!["jobs.0.steps.1.run".to_string()],
            },
        )
        .unwrap();
        assert_eq!(
            out.data["jobs.0.steps.1.run"],
            Value::String("lint".to_string())
        );
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
    fn file_hash_computes_sha256() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("x.txt");
        fs::write(&path, "abc").unwrap();
        let out = execute_file_hash(FileHashInput {
            paths: vec![path.to_string_lossy().to_string()],
            algorithm: None,
        })
        .unwrap();
        assert_eq!(
            out.hashes[0].hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn git_log_can_include_diff_content() {
        let dir = tempdir().unwrap();
        let repo = dir.path();

        let status = Command::new("git")
            .current_dir(repo)
            .args(["init"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .current_dir(repo)
            .args(["config", "user.name", "Test User"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .current_dir(repo)
            .args(["config", "user.email", "test@example.com"])
            .status()
            .unwrap();
        assert!(status.success());

        let file = repo.join("a.txt");
        fs::write(&file, "one\n").unwrap();
        let status = Command::new("git")
            .current_dir(repo)
            .args(["add", "a.txt"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .current_dir(repo)
            .args(["commit", "-m", "first"])
            .status()
            .unwrap();
        assert!(status.success());

        fs::write(&file, "one\ntwo\n").unwrap();
        let status = Command::new("git")
            .current_dir(repo)
            .args(["add", "a.txt"])
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("git")
            .current_dir(repo)
            .args(["commit", "-m", "second"])
            .status()
            .unwrap();
        assert!(status.success());

        let out = execute_git_log(GitLogInput {
            repo_path: repo.to_string_lossy().to_string(),
            max_results: Some(1),
            path_filter: None,
            include_diff_stat: Some(true),
            include_diff: Some(true),
            max_diff_lines: Some(50),
        })
        .unwrap();
        assert_eq!(out.count, 1);
        assert!(out.commits[0].diff.is_some());
        assert!(
            out.commits[0]
                .diff_stat
                .as_ref()
                .is_some_and(|d| !d.is_empty())
        );
    }

    #[test]
    fn server_stats_reports_entries_and_evictions() {
        let mut state = ServerState::new();
        state.record_request("text_search");
        let metrics = state.metrics_json();
        assert_eq!(
            metrics["cache"]["evictions"]["total"],
            Value::Number(0.into())
        );
        assert_eq!(
            metrics["cache"]["entries"]["total"],
            Value::Number(0.into())
        );
        assert_eq!(
            metrics["requests_per_tool"]["text_search"],
            Value::Number(1.into())
        );
    }
}
