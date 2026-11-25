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

## 5. JavaScript Extensions

Load custom tools or lifecycle hooks with `--extension <path>` (file or directory, repeatable):

```bash
jimu --extension ./extensions/my-tools.js
```

An extension exports a default function that receives the `jimu` API:

```javascript
export default function (jimu) {
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
    execute(args) {
      return `Hello, ${args.name}!`;
    },
  });

  jimu.on("before_tool_call", (ctx) => {
    // Inspect or block tool calls if needed
    return undefined;
  });
}
```

Available hook events for `jimu.on(event, handler)`:
`prepare_request`, `transform_context`, `before_tool_call`, `after_tool_call`, `finish_turn`.

## 6. Editor Integration (`jimu acp`)

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

## 7. Cross-Process Automation (`jimu rpc`)

For external apps or scripts that want persistent, streaming control over a session via newline-delimited JSON on stdin/stdout:

```bash
jimu rpc --cwd /path/to/project --session /path/to/session.jsonl
```

(`jimu --mode rpc` is an equivalent form.) Send JSON commands such as `prompt`, `steer`, `follow_up`, `abort`, `set_model`, `compact`, and `get_state` on stdin and read responses and streaming events from stdout.
