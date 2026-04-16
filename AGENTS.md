# AGENTS.md — Project Rules for AI Assistants (Rust + Python)

This repository is currently developed primarily from a **Windows dev drive** using **PowerShell** and **VS Code**.  
**WSL:Arc Linux**, **fish**, and **NvChad Neovim** remain supported alternative workflows.  
All tooling runs via terminal commands. The core server is written in **Rust** (managed with `cargo`).  
Contributors maintain CLI-first workflows with minimal, deterministic diffs that comply with repository standards.

GitHub Copilot agents and other LLM-based assistants use this file to align with project-specific practices.  
VS Code's agentic AI features can apply multi-file coordinated changes, so the rules below constrain that behavior.

---

## 0. Development Environment

- **Primary OS/workspace:** Windows dev drive
- **Supported alternative OS/workspace:** WSL 2 with Arc Linux
- **Editors:** VS Code (primary), LazyVim Neovim (supported alternative)
- **Shells:** PowerShell (current default), `fish` in WSL (supported alternative)
- **Rust toolchain:** `rustup` with stable channel; `cargo` for builds, tests, and dependency management

All terminal commands should be reproducible from the supported shell/editor combinations.

---

## 0.1 Available CLI Tools

The following tools are installed locally and available for use in terminal workflows and agent tasks:

| Tool | Purpose |
|------|---------|
| `diffutils` | File comparison (`diff`, `cmp`, `diff3`, `sdiff`) |
| `fd` | Fast, user-friendly alternative to `find` for file search |
| `fzf` | General-purpose fuzzy finder for interactive filtering |
| `ripgrep` (`rg`) | Fast regex search across files; prefer over `grep`/`Select-String` |
| `zip` | Archive creation and extraction |
| `tokei` | Count lines of code by language |
| `ast-grep` (`sg`) | Structural code search and rewriting using AST patterns |
| `jq` | JSON query and transformation CLI |
| `yq` | YAML/JSON/TOML query and transformation CLI |
| `hyperfine` | Command-line benchmarking with statistical output |
| `pre-commit` | Run and manage repository pre-commit hooks |
| `http` / `https` (HTTPie) | Human-friendly HTTP API client |
| `just` | Project task runner via `justfile` recipes |
| `difft` (difftastic) | Syntax-aware structural diffing |

Prefer these tools over PowerShell built-ins where applicable (e.g., use `rg` instead of `Select-String`, use `fd` instead of `Get-ChildItem` for file discovery).

### Preferred command order

- Content search: `rg` first, then `ast-grep` for structural/language-aware matching
- File discovery: `fd` first, then `rg --files` as a fallback
- JSON config inspection: `jq`
- YAML/TOML inspection: `yq`
- HTTP/API smoke checks: `http` / `https` (HTTPie)
- Task orchestration: `just` recipes when a `justfile` exists
- Diff/review: `difft` for syntax-aware diffs, `diff` for plain text diffs
- Performance comparisons: `hyperfine` for repeatable timing

### Avoid in autonomous runs

- Avoid interactive-only flows (for example `fzf` prompts) unless the user explicitly asks for interactive selection
- Avoid destructive git/file operations unless the user explicitly approves them
- Avoid long-running watch commands by default; use one-shot checks first, then switch to watch mode only when requested
- Avoid invoking `pre-commit run --all-files` on very large repos when a targeted path or hook is enough for the task

---

## 1. Authoritative Tools & Source of Truth

### Rust
- `cargo` is the ONLY build system and dependency manager.
- `cargo clippy` is the ONLY linter; `cargo fmt` is the ONLY formatter.
- Do NOT use third-party Rust formatters or linters outside the standard toolchain.
- `Cargo.toml` is the authoritative source for Rust dependencies and features.

### Cross-Editor Compatibility
- Contributors primarily use VS Code and still support Neovim workflows.
- All changes must be reproducible via terminal commands.

---

## 2. Terminal Workflows

### Rust — Building & Testing

```powershell
# Build (debug)
cargo build

# Build (release)
cargo build --release

# Run tests
cargo test

# Run with verbose-logging feature
cargo run --features verbose-logging

# Lint
cargo clippy -- -D warnings

# Format
cargo fmt

# Check format without modifying
cargo fmt --check
```

## 4. Editor Configuration

### VS Code Settings
- Use the Rust Analyzer extension for Rust language support
- Use the Ruff extension for Python real-time linting
- Terminal integration: Use the integrated PowerShell terminal by default; WSL + `fish` remains a supported alternative

### LazyVim/Neovim
- Configure rust-analyzer LSP for Rust
- Configure LSP to use linting results from Ruff for Python
- Do not rely on editor auto-formatting; use `cargo fmt` / `uv run ruff format` before committing

---

## 5. Git Workflow Discipline

- Run `cargo clippy -- -D warnings` and `cargo fmt --check` before committing Rust changes
- Verify all checks pass cleanly before opening a PR
- Keep diffs minimal and focused on the change
- Do not include unrelated reformatting in commits
