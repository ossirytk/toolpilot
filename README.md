# toolpilot
High‑performance persistent MCP server optimized for Copilot CLI.

## Implemented MCP tools

- `fs_glob`: deterministic glob expansion with capped output.
- `text_search`: literal/regex search with line and byte offsets.
- `json_select`: explicit field selection and typed filters for JSON files.
- `server_stats`: request and cache counters.

The server is Rust + tokio, runs as a single persistent stdio MCP process,
returns structured JSON only, and uses in-process caches for parsed JSON,
memory-mapped text files, and compiled regexes.
