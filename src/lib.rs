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
}
