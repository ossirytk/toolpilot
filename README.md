# toolpilot
High‑performance persistent MCP server optimized for Copilot CLI.

## Implemented MCP tools

- `fs_glob`: deterministic glob expansion with capped output.
- `fs_tree`: depth-limited directory tree as structured JSON.
- `text_search`: literal/regex search with line and byte offsets.
- `json_select`: explicit field selection and typed filters for JSON files.
- `yaml_select`: field extraction from YAML/TOML files using dot-notation paths.
- `git_log`: git commit history with optional path filter.
- `server_stats`: request and cache counters.

The server is Rust + tokio, runs as a single persistent stdio MCP process,
returns structured JSON only, and uses in-process caches for parsed JSON,
memory-mapped text files, and compiled regexes.

## Setup

### Build

```sh
cargo build --release
```

The release binary is written to `target/release/toolpilot`.

### Configure Copilot CLI

Copy `.mcp.example.json` to `.mcp.json` and set `command` to the absolute path
of the built binary:

```json
{
  "mcpServers": {
    "toolpilot": {
      "type": "stdio",
      "command": "/absolute/path/to/toolpilot/target/release/toolpilot"
    }
  }
}
```

> **Note:** Do not use `cargo run` in the MCP config — the compilation delay on
> cold start causes the CLI to time out before the server is ready.
