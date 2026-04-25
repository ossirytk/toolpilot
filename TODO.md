# Toolpilot — Improvement Ideas

## Fixes

- **No `read_file` tool** — the most obvious gap. `text_search` finds content but there's no way to simply read a file's contents. Agents fall back to shell `cat`, defeating the purpose of a structured file tool. Add `read_file` with optional `start_line`/`end_line` range params and a max-bytes cap.
- **`git_log` returns no diff content** — the `include_diff_stat` flag returns changed file names and insertion/deletion counts but not actual diff hunks. Add an `include_diff` flag that returns unified diff per commit (capped at N lines per commit to avoid blowup).

## Enhancements

- **`text_search` file-type filter** — add a `glob` or `file_type` param (e.g., `"*.py"`, `"*.ts"`) to narrow search scope without a separate `fs_glob` call. Mirrors ripgrep's `-g`/`-t` flags.
- **File hash/checksum tool** — add `file_hash` returning SHA-256 (and optionally MD5/SHA-1) for one or more paths. Useful for verifying downloads, detecting changes without diffing, and cache invalidation checks.
- **`fs_tree` hidden file toggle** — already has `include_hidden` param but the default excludes hidden files/dirs. Make this more prominent in docs; many project configs live in `.github/`, `.vscode/`, etc.
- **`yaml_select` array indexing** — clarify/implement dot-notation array index access (e.g., `jobs.0.steps`) for YAML/TOML files with list values, which is common in CI configs.
- **`server_stats` cache eviction info** — expose cache entry count and eviction count alongside hit/miss ratios so it's easier to tune cache sizing for large repos.
