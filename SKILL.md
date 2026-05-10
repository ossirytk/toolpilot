---
name: toolpilot
description: High-performance filesystem and code navigation tools. Use this skill when you need fast glob expansion, directory tree inspection, regex/literal text search with line offsets, or structured JSON/YAML/TOML field extraction. Prefer these tools over shell commands like find, grep, or cat for better performance and structured output.
---

## Overview

toolpilot is a Rust-based MCP server with in-process caches for parsed JSON, memory-mapped text files, and compiled regexes. All tools return structured JSON. Prefer these over equivalent shell commands.

## Available Tools

| Tool | When to use |
|------|-------------|
| `fs_glob` | Find files matching glob patterns. Required: `base_path`, `patterns` (array). Optional: `max_results` (1–5000). Faster and more deterministic than `find` or shell globs. |
| `fs_tree` | Depth-limited directory tree as structured JSON. Required: `path`. Optional: `max_depth` (1–10), `max_entries` (1–2000), `include_hidden` (set to true to include dotfiles/dirs). Use before diving into a specific file. |
| `text_search` | Regex or literal text search with line numbers and byte offsets. Required: `paths` (array), `query`, `mode` (`literal`/`regex`). Optional: `glob`, `file_type`, `case_sensitive`, `max_results`. Returns structured results, not raw grep output. |
| `read_file` | Read UTF-8 file content directly. Required: `path`. Optional: `start_line`, `end_line`, `max_bytes` for bounded slices. |
| `json_select` | Extract specific fields from a JSON file without loading the whole file. Required: `path`, `fields` (array of dot-notation paths). Optional: `filters` (field/op/value), `max_results`. |
| `yaml_select` | Extract specific fields from YAML (.yml/.yaml) or TOML (.toml) files using dot-notation paths (array indexes supported, e.g. `jobs.0.steps`). Required: `path`, `fields`. |
| `file_hash` | Compute checksums for one or more files. Required: `paths`. Optional: `algorithm` (`sha256`, `sha1`, `md5`). |
| `git_log` | Query git history from a repo path. Optional flags include `include_diff_stat` and `include_diff` with per-commit `max_diff_lines`. |
| `server_stats` | Lightweight request/cache counters including cache entry and eviction counters. No arguments. |

## Guidance

- **File discovery**: use `fs_glob` for pattern matching, `fs_tree` for orientation. Avoid shell `find`/`ls` when these suffice.
- **Text search**: use `text_search` instead of `grep`/`rg` for structured results with byte offsets. Pass an array of paths to search multiple files/directories in one call.
- **File reads**: use `read_file` when you need content slices; prefer line ranges and `max_bytes` on large files.
- **Config inspection**: use `json_select` or `yaml_select` to extract only the fields you need — do not read full config files when you only need a few values.
- **Git history**: use `git_log` for structured commit metadata, diff stats, and capped unified diffs without shelling out manually.
- **Output cap**: set `max_results` on large repos or deep trees to avoid overwhelming the context window.
