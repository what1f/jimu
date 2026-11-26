# Using Jimu

## Starting Jimu

```bash
# Interactive terminal session (default)
jimu

# Start interactive mode with an initial message
jimu "Find and fix the failing test in src/"

# Continue the most recent session for the current directory
jimu -c

# Pick a previous session to resume on startup
jimu -r

# Use a specific session file, or run without saving history
jimu --session ./my-session.jsonl
jimu --no-session

# Work in another directory
jimu -C /path/to/project
```

## Non-Interactive (One-Shot) Mode

Use `-p` / `--print` to run a single prompt and exit—ideal for scripts, pipelines, and quick questions:

```bash
# Plain text output
jimu -p "Summarize recent changes in this repo"

# Structured JSONL event stream
jimu -p --mode json "List all TODO comments in src/"

# Continue the last session in one-shot mode
jimu -p -c "Now write a commit message for those changes"
```

## Keyboard Shortcuts

| Shortcut | Action |
| :--- | :--- |
| `Enter` | Send message (or steer the assistant while it is working) |
| `Shift+Enter` / `Ctrl+J` | Insert a newline in the editor |
| `Alt+Enter` | Queue a follow-up message to run after the current turn finishes |
| `Tab` | Autocomplete `/` commands or `@` file paths |
| `Esc` | Close active dialog/menu, or interrupt the current response |
| `Esc` twice (empty input) | Open the Rewind / Branch Tree picker |
| `Ctrl+C` | Interrupt if running, clear non-empty draft, or press twice to exit |
| `Ctrl+D` / `Ctrl+Q` | Exit Jimu (`Ctrl+D` on empty input, `Ctrl+Q` anytime) |
| `Shift+Tab` | Cycle permission mode (`default` → `acceptEdits` → `bypassPermissions`) |
| `Alt+P` | Open the model picker |
| `Alt+T` | Cycle thinking / effort level |
| `Ctrl+O` | Expand or collapse all tool and thinking cards |
| `Ctrl+R` | Search previous prompt history |
| `Ctrl+V` / `Alt+V` | Paste an image from the clipboard (inserted as an `[Image #N]` chip) |

## Pasting Images

- Press `Ctrl+V` (or `Alt+V` if your terminal intercepts `Ctrl+V`) to attach a screenshot or copied image directly from your clipboard.
- You can also paste or drop a local image file path into the prompt editor—Jimu automatically turns it into a numbered `[Image #N]` chip and keeps the `[Image #N]` placeholder in your conversation transcript so you can refer to it across turns.

## Layout & Bottom Status Bar

- By default, Jimu hides the top header bar to keep the transcript uncluttered and renders a compact single-line status bar at the bottom (model, effort, fast mode, directory & Git branch, context usage, permission mode, and active spinner).
- To re-enable the 1-row top header bar, set `"show_header": true` in `~/.jimu/settings.json`.
- To replace the bottom status bar with your own shell script or JS UI mod, see `customization.md`.

## Slash Commands

Type `/` in the prompt editor to open the command menu:

### Session & History
- `/new` (alias `/clear`) — Start a fresh conversation
- `/resume` — Browse and switch to a saved session
- `/rewind` (alias `/tree`) — Jump back to an earlier point or switch branches
- `/export [path]` — Export the conversation to a Markdown file
- `/exit` (alias `/quit`) — Quit Jimu

### Model, Context & Permissions
- `/model` — Switch model
- `/effort` (alias `/thinking`) — Change thinking / reasoning effort level
- `/fast [on|off]` — Toggle fast mode
- `/compact` — Summarize older conversation history to free up context space
- `/context` — Inspect current context window usage
- `/permissions` — Change tool approval mode

### Extensions & Interface
- `/skills` — View discovered skills
- `/mcp` — View connected MCP servers and tools
- `/reload` — Hot-reload JS extensions, UI mods, and custom themes
- `/copy` — Copy the last assistant response to the clipboard
- `/theme` — Switch color theme (built-in or custom themes)
- `/help` (alias `/hotkeys`) — Show keyboard shortcuts

## Rewind and Conversation Branching

Press `Esc` twice on an empty input (or run `/rewind` / `/tree`) to browse earlier turns:

- Turns are listed chronologically from top to bottom and focus automatically on your current leaf (`●`).
- Once you branch from an earlier point, the picker switches to a compact tree view: linear segments stay aligned without staircase drift, `├─` / `└─` mark branch points, active-branch nodes show `•`, and the active branch sorts first.
- Selecting a **user message** rewinds to just before it and puts its text back into your editor so you can edit and resend.
- Selecting an **assistant response** switches directly to that branch state.

## Permission Modes

Control when Jimu asks before making changes or running commands. Set an initial mode with `--permission-mode <mode>`, or switch live with `Shift+Tab` / `/permissions`:

- `default` — Asks for approval before editing files, running shell commands, fetching web pages, or invoking MCP / extension tools.
- `acceptEdits` — Automatically approves file edits; still asks before shell commands, web fetches, or MCP / extension tools.
- `bypassPermissions` — Runs all tools without prompting. Use only in trusted projects or sandboxes.

In non-interactive `-p` and `jimu rpc` modes there is no approval prompt, so passing an explicit `--permission-mode` other than `bypassPermissions` is rejected at startup.

## Background Processes

When a shell command or monitor continues running in the background, a compact indicator appears above the footer. Click it to inspect or stop any running background process.

## Themes

Choose a built-in or custom theme at startup with `--theme <name>` or switch live with `/theme` (your selection is saved to `~/.jimu/settings.json`):

- **Built-in themes**: `auto` (follows your terminal light/dark appearance), `dark`, `light`, `tokyonight`, `rosepine`, `oscura`, `terminal`.
- **Custom themes**: Place `<name>.json` files in `~/.jimu/themes/` (personal) or `.jimu/themes/` (project, overrides personal). See `customization.md` for the JSON format.

You can also set a default theme via the `TUI_THEME` environment variable.
