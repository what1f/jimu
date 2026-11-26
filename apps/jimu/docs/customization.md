# Customizing and Integrating Jimu

## 1. Project and Personal Instructions (`AGENTS.md`)

Jimu automatically loads instruction files so you do not have to repeat project rules in every conversation:

- **Personal instructions**: `~/.jimu/AGENTS.md` and `~/.agents/AGENTS.md`
- **Project instructions**: `AGENTS.md` (or `AGENTS.override.md` to override a directory's `AGENTS.md`), discovered from the project root down to your current directory

Useful CLI flags:
- `--instruction-file <path>` — Include an extra instruction file (repeatable)
- `--walk-boundary <project-root|filesystem-root|cwd-only>` — Control how far upward to search
- `--instruction-max-bytes <bytes|none>` — Set size limit (default `32768`; `none` for unlimited)
- `--no-user-instructions` — Ignore personal `~/.jimu` / `~/.agents` instructions
- `--no-instructions` — Disable instruction discovery completely
- `--system-prompt "<text>"` — Replace the default system instructions entirely

## 2. Skills (`SKILL.md`)

Skills are reusable instruction packages for specialized workflows. Jimu automatically discovers skills in:

- Project: `.agents/skills/` and `.jimu/skills/`
- Personal: `~/.agents/skills/` and `~/.jimu/skills/`
- Extra directories passed with `--skill-dir <dir>` (repeatable)

Each skill lives in its own folder with a `SKILL.md` file:

```markdown
---
name: release-check
description: Verify formatting, tests, and changelog before tagging a release
---

# Release Check

1. Run the project test suite.
2. Check `git status` for untracked files.
3. Verify the version bump and changelog entries.
```

- Jimu watches your skill folders and picks up added, removed, or edited skills automatically—no restart needed.
- Mention `$skill-name` in a prompt to explicitly invoke a skill, or run `/skills` to see all discovered skills.

## 3. Built-In Tools

Jimu includes 11 built-in tools:

`read`, `write`, `edit`, `apply_patch`, `find`, `grep`, `exec_command`, `write_stdin`, `monitor`, `sleep`, `web_fetch`.

Customize which tools are active for a run:

```bash
# Read-only session
jimu --tools read,find,grep

# Disable shell execution and web access
jimu --disable-tools exec_command,write_stdin,monitor,web_fetch

# Chat only (no tools)
jimu --no-tools

# Enable scripted multi-tool execution (`exec`)
jimu --enable-codemode
```

## 4. MCP Servers (Shared Across TUI, `-p`, RPC, and ACP)

Configure persistent MCP servers (`stdio` or Streamable `http`) in `~/.jimu/settings.json` under `mcp_servers`:

```json
{
  "mcp_servers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "."]
    },
    "docs": {
      "url": "https://example.com/mcp",
      "headers": {
        "Authorization": "Bearer ..."
      },
      "mode": "deferred"
    }
  }
}
```

- Optional fields per server: `args`, `env`, `cwd` (stdio only), `headers` (HTTP only), `disabled: true`, and `mode` (`"auto"`, `"deferred"`, or `"resident"`).
- Tools are exposed as `mcp_<server>_<tool>` and work identically across interactive TUI (view via `/mcp`), one-shot `-p`, `jimu rpc`, and `jimu acp` (where editor-supplied `mcpServers` merge with and override matching global entries).
- For one-off or scripted runs, pass `--mcp-config <path>` (JSON file containing `{"mcp_servers": {...}}`) or `--mcp-server <name>=<url|command|json>` (both repeatable):

```bash
jimu --mcp-config ./mcp.json
jimu -p "Search docs" --mcp-server docs=https://example.com/mcp
jimu --mcp-server fs='{"command":"npx","args":["-y","@modelcontextprotocol/server-filesystem","."]}'
```

## 5. Custom Status Line & Header (`settings.json`)

Customize the TUI chrome in `~/.jimu/settings.json`:

```json
{
  "show_header": false,
  "status_line": {
    "command": "~/.jimu/status-line.sh",
    "refresh_ms": 5000,
    "timeout_ms": 1500
  }
}
```

- `show_header`: Set `true` to show the built-in 1-row top header bar in fullscreen mode (default `false`).
- `status_line`: Runs `command` via `sh -c` in the background whenever session state changes (plus optional periodic `refresh_ms`), piping a JSON snapshot to `stdin` and rendering ANSI-colored `stdout` (16-color, 256-color, TrueColor, bold/dim/italic/underline) as the bottom status bar. Exiting with empty/whitespace-only `stdout` clears the cached custom bar and falls back to the built-in status bar.

### `status_line` Stdin JSON Schema

```json
{
  "version": "0.1.0",
  "session_id": "...",
  "transcript_path": "/path/to/session.jsonl",
  "model": { "id": "gpt-5.4", "display_name": "GPT-5.4" },
  "workspace": { "current_dir": "/path/to/cwd", "project_dir": "/path/to/project" },
  "git_branch": "main",
  "context_window": {
    "context_window_size": 200000,
    "used_percentage": 12.3,
    "current_usage": {
      "input_tokens": 0,
      "output_tokens": 0,
      "cache_read_input_tokens": 0,
      "cache_creation_input_tokens": 0
    }
  },
  "cost": { "total_cost_usd": 0.0, "total_duration_ms": 0 },
  "effort": "high",
  "fast_mode": false,
  "permission_mode": "default",
  "running": false
}
```

Example `~/.jimu/status-line.sh` (using `jq`):

```bash
#!/usr/bin/env bash
json=$(cat)
model=$(jq -r '.model.display_name' <<<"$json")
branch=$(jq -r '.git_branch' <<<"$json")
pct=$(jq -r '.context_window.used_percentage' <<<"$json")
printf '\033[1;36m%s\033[0m  \033[33m%s\033[0m  \033[90mctx %s%%\033[0m\n' "$model" "$branch" "$pct"
```

## 6. Custom JSON Themes (`~/.jimu/themes/*.json`)

Create personal themes in `~/.jimu/themes/<name>.json` or project-specific themes in `<project>/.jimu/themes/<name>.json` (project themes override personal themes with the same name). Select them with `--theme <name>` or `/theme`:

```json
{
  "base": "dark",
  "colors": {
    "bg": "#111318",
    "bg_surface": "#1a1d24",
    "fg": "#e6edf3",
    "muted": "#8b949e",
    "accent": "#58a6ff",
    "border": "#30363d",
    "border_focus": "#58a6ff",
    "error": "#f85149",
    "success": "#3fb950",
    "warning": "#d29922"
  }
}
```

- `base` (optional): Any built-in theme (`dark`, `light`, `tokyonight`, `rosepine`, `oscura`, `terminal`, `auto`).
- Color values accept `#rrggbb`, `#rgb`, `rgb(r,g,b)`, `ansi(n)`, `reset`, or standard ANSI color names.

## 7. JavaScript Extensions & UI Mods

Jimu automatically discovers `.js` extensions across all modes (TUI, `-p`, RPC, ACP) from:

- Personal: `~/.jimu/extensions/*.js`
- Project: `<project>/.jimu/extensions/*.js` (overrides a same-named personal extension)
- Explicit entries in `~/.jimu/settings.json` (`"extensions": ["./my-ext.js"]`; relative paths resolve against `~/.jimu/`)
- CLI flags: `--extension <path>` (file or directory, repeatable; relative paths resolve against the current working directory)

```bash
jimu --extension ./extensions/my-tools.js
```

An extension exports a default function that receives the `jimu` API:

```javascript
export default function (jimu) {
  // 1. Register custom tools
  jimu.registerTool({
    name: "hello",
    description: "Return a friendly greeting",
    parameters: {
      type: "object",
      properties: {
        name: { type: "string" },
      },
      required: ["name"],
    },
    execute(_id, args) {
      return `Hello, ${args.name}!`;
    },
  });

  // 2. Intercept agent lifecycle hooks
  jimu.on("before_tool_call", (ctx) => {
    return undefined;
  });

  // 3. Optional TUI theme overrides & declarative UI mods (ignored in headless modes)
  jimu.ui.theme({ accent: "#7aa2f7" });

  jimu.on("ui.render", (e, next) => {
    if (e.component === "StatusLine") {
      return jimu.ui.row({ gap: 1 }, [
        next(e),
        jimu.ui.text("[team]", { color: "accent_success", bold: true }),
      ]);
    }
    if (e.component === "AbovePrompt" && e.props.running) {
      return jimu.ui.text("Working...", { color: "warning", dim: true });
    }
    return next(e);
  });
}
```

- **Agent hook events** (`jimu.on(event, handler)`): `prepare_request`, `transform_context`, `before_tool_call`, `after_tool_call`, `finish_turn`.
- **UI Mod slots** (`jimu.on("ui.render", (e, next) => ...)`): `StatusLine`, `AbovePrompt`, `Header`, `Footer`, `UserMessage`, `ToolRow`, `Spinner`.
  - Call `next(e)` inside `jimu.ui.box(...)` / `jimu.ui.row(...)` to wrap native rendering without losing syntax highlighting, image chips, or diffs; omit `next(e)` to replace a slot completely.
  - Pure-data node builders: `jimu.ui.text(str, style)`, `jimu.ui.box(props, children)`, `jimu.ui.row(props, children)`, `jimu.ui.divider(style)`, `jimu.ui.native()`, `jimu.ui.theme(overrides)`, and `jimu.ui.invalidate()`.
  - Run `/reload` in the TUI to hot-reload edited extensions, UI mods, and custom themes without restarting.

## 8. Editor Integration (`jimu acp`)

Connect Jimu to any editor that supports the Agent Client Protocol (ACP)—such as **Zed**, **JetBrains IDEs**, or **Neovim**.

Example **Zed** (`settings.json`):

```json
{
  "agent_servers": {
    "Jimu": {
      "command": "jimu",
      "args": ["acp"]
    }
  }
}
```

MCP servers configured by your editor are attached automatically in ACP mode. You can pass any shared flags (such as `--provider`, `-m`, `--thinking`, `--tools`, or `--permission-mode`) after `acp`.

## 9. Cross-Process Automation (`jimu rpc`)

For external apps or scripts that want persistent, streaming control over a session via newline-delimited JSON on stdin/stdout:

```bash
jimu rpc --cwd /path/to/project --session /path/to/session.jsonl
```

(`jimu --mode rpc` is an equivalent form.) Send JSON commands such as `prompt`, `steer`, `follow_up`, `abort`, `set_model`, `compact`, and `get_state` on stdin and read responses and streaming events from stdout.
