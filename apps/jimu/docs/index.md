# Jimu User Guide

Jimu is a coding assistant that runs in your terminal, scripts, and code editors. It can read and edit files, search your project, run shell commands, and fetch web pages.

## Quick Start

1. **Authenticate** with a ChatGPT/Codex subscription or an API key:
   ```bash
   # Sign in with ChatGPT / Codex
   jimu login

   # Or export an API key for your preferred provider
   export ANTHROPIC_API_KEY="sk-ant-..."
   ```
2. **Open a project and start Jimu**:
   ```bash
   cd /path/to/your-project
   jimu
   ```
3. **Give Jimu a task**, or pass an initial prompt right on the command line:
   ```bash
   jimu "Explain how this repository is organized and how to run its checks"
   ```

## Documentation Map

Read the topic you need:

- **`usage.md`** — Interactive terminal guide, CLI flags, one-shot mode (`-p`), sessions (`-c` / `-r`), slash commands, keyboard shortcuts, rewind & branching, permission modes, and themes.
- **`models.md`** — Signing in (`jimu login`), environment variable API keys, built-in provider presets, thinking levels, fast mode, and global provider/model setup (`~/.jimu/providers.json`).
- **`customization.md`** — Project instructions (`AGENTS.md`), reusable skills (`SKILL.md`), choosing which tools are enabled, JavaScript extensions (`--extension`), editor integration (`jimu acp`), and automation (`jimu rpc`).

## Where Jimu Stores Files

By default, Jimu stores user data in `~/.jimu` (override with `--jimu-home <dir>` or `JIMU_HOME`):

- `~/.jimu/auth.json` — Stored login credentials
- `~/.jimu/providers.json` — Global providers and models (shared across TUI, `-p`, RPC, and ACP)
- `~/.jimu/settings.json` — Saved default model, effort, permission mode, theme, fast mode, and global `mcp_servers`
- `~/.jimu/sessions/` — Unified conversation histories (shared across TUI, RPC, and ACP)
- `~/.jimu/skills/` — Personal skills
- `~/.jimu/AGENTS.md` — Personal instructions applied across projects
- `~/.jimu/docs/` — This built-in user guide
