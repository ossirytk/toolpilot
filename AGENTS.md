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

## 3. Editor Configuration

### VS Code Settings
- Use the Rust Analyzer extension for Rust language support
- Terminal integration: Use the integrated PowerShell terminal by default; WSL + `fish` remains a supported alternative

### LazyVim/Neovim
- Configure rust-analyzer LSP for Rust
- Do not rely on editor auto-formatting; use `cargo fmt` before committing

---

## 4. Git Workflow Discipline

- Run `cargo clippy -- -D warnings` and `cargo fmt --check` before committing Rust changes
- Verify all checks pass cleanly before opening a PR
- Keep diffs minimal and focused on the change
- Do not include unrelated reformatting in commits
