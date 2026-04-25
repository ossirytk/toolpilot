# toolpilot

High‑performance persistent MCP server for GitHub Copilot CLI and VS Code Copilot.

## Available tools

| Tool | Description |
|------|-------------|
| `fs_glob` | Deterministic glob expansion with capped output |
| `fs_tree` | Depth-limited directory tree as structured JSON |
| `text_search` | Literal/regex search with line and byte offsets |
| `json_select` | Explicit field selection and typed filters for JSON files |
| `yaml_select` | Field extraction from YAML/TOML files using dot-notation paths |
| `git_log` | Git commit history with optional path filter |
| `server_stats` | Request and cache counters |

The server runs as a single persistent stdio MCP process, returns structured
JSON only, and uses in-process caches for parsed JSON, memory-mapped text
files, and compiled regexes.

## Installation

### Prerequisites

A Rust toolchain is required. Install it via [rustup](https://rustup.rs/):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Option 1 — `cargo install` from GitHub (recommended)

```sh
cargo install --git https://github.com/ossirytk/toolpilot
```

The binary is placed in `~/.cargo/bin/toolpilot` (Linux/macOS) or
`%USERPROFILE%\.cargo\bin\toolpilot.exe` (Windows). Ensure `~/.cargo/bin` is
on your `PATH` (rustup adds it automatically).

### Option 2 — `cargo binstall`

If you have [cargo-binstall](https://github.com/cargo-bins/cargo-binstall)
installed:

```sh
cargo binstall --git https://github.com/ossirytk/toolpilot toolpilot
```

### Option 3 — Build from source

```sh
git clone https://github.com/ossirytk/toolpilot
cd toolpilot
cargo build --release
```

The binary is written to `target/release/toolpilot`.

## Configuration

After installation, register toolpilot as an MCP server in your editor or CLI.

> **Note:** Do not use `cargo run` as the command — the compilation delay causes
> the client to time out before the server is ready. Always point to the
> compiled binary.

### GitHub Copilot CLI

Copy `.mcp.example.json` to `.mcp.json` in your project root (or home
directory) and set `command` to the binary path:

```json
{
  "mcpServers": {
    "toolpilot": {
      "type": "stdio",
      "command": "toolpilot"
    }
  }
}
```

If the binary is not on your `PATH`, use the absolute path instead:

```json
{
  "mcpServers": {
    "toolpilot": {
      "type": "stdio",
      "command": "/absolute/path/to/toolpilot"
    }
  }
}
```

### VS Code Copilot

#### Workspace-level (`.vscode/mcp.json`)

Add a `.vscode/mcp.json` file to your workspace:

```json
{
  "servers": {
    "toolpilot": {
      "type": "stdio",
      "command": "toolpilot"
    }
  }
}
```

#### User-level (`settings.json`)

Open **Settings → Open User Settings (JSON)** and add:

```json
{
  "mcp": {
    "servers": {
      "toolpilot": {
        "type": "stdio",
        "command": "toolpilot"
      }
    }
  }
}
```

The `settings.json` file is located at:

- **Linux:** `~/.config/Code/User/settings.json`
- **macOS:** `~/Library/Application Support/Code/User/settings.json`
- **Windows:** `%APPDATA%\Code\User\settings.json`
