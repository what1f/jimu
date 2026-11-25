# Authentication, Providers, and Models

Jimu works with subscriptions, API keys, and custom OpenAI/Anthropic/Google-compatible endpoints.
All modes (`jimu` TUI, `jimu -p` / `--mode text|json`, `jimu rpc`, and `jimu acp`) read the same global configuration files under `~/.jimu/`.

## 1. Sign In with ChatGPT / Codex

```bash
jimu login
```

This opens a browser sign-in flow and saves credentials to `~/.jimu/auth.json`. If you are already signed in with the Codex CLI (`~/.codex/auth.json`), Jimu can use those credentials automatically.

To remove stored credentials:

```bash
jimu logout
```

## 2. Built-in Provider Presets

Built-in providers supply connection presets (wire protocol, default base URL, and environment variable names) without hardcoding a legacy model list:

| Provider (`--provider`) | API Protocol | Default Base URL | Environment Variable |
| :--- | :--- | :--- | :--- |
| `anthropic` | `anthropic-messages` | `https://api.anthropic.com` | `ANTHROPIC_API_KEY` |
| `openai` | `openai-responses` | `https://api.openai.com/v1` | `OPENAI_API_KEY` |
| `openai-codex` | `openai-codex-responses` | ChatGPT Codex endpoint | *(use `jimu login`)* |
| `google` | `google-generative-ai` | `https://generativelanguage.googleapis.com/v1beta` | `GEMINI_API_KEY` or `GOOGLE_API_KEY` |
| `deepseek` | `openai-responses` | `https://api.deepseek.com/v1` | `DEEPSEEK_API_KEY` |
| `openrouter` | `openai-completions` | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` |

Export a key and pass `-m <model>` (or configure `models` in `~/.jimu/providers.json`):

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
jimu --provider anthropic -m claude-sonnet-4-6
```

If your key lives in a custom environment variable, pass `--api-key-env` (or set `api_key_env` in `providers.json`).

## 3. Selecting Model, Thinking, and Fast Mode

At startup:

```bash
jimu --provider anthropic -m claude-sonnet-4-6 --thinking high
jimu -m anthropic/claude-sonnet-4-6 --fast
```

Inside an interactive TUI session:
- Switch model & effort together: `/model` or `Alt+P` (`↑`/`↓` model, `←`/`→` effort slider, `Enter` apply), or type `/model <query>` / `/effort <query>` for inline autocomplete
- Switch thinking level: `/effort` or `Alt+T`
- Toggle fast mode: `/fast` (or `/fast on` / `/fast off`)

Switching model, effort, permission mode, theme, or fast mode in TUI automatically saves your defaults to `~/.jimu/settings.json`, which are reused on the next launch (overridden by explicit CLI flags or resumed session state).

Supported `--thinking` levels: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`.

## 4. Global Provider & Model Config (`~/.jimu/providers.json`)

Configure custom providers, local model servers (such as Magpie, Ollama, or LM Studio), model lists, or preset overrides in `~/.jimu/providers.json`:

```json
{
  "openai": {
    "disabled": true
  },
  "anthropic": {
    "models": [
      "claude-sonnet-4-6",
      "claude-opus-4-6"
    ]
  },
  "magpie": {
    "api": "openai-responses",
    "base_url": "http://127.0.0.1:3425/v1",
    "api_key_env": "MAGPIE_API_KEY",
    "models": [
      {
        "id": "my-model",
        "name": "My Model",
        "context_window": 200000,
        "max_output_tokens": 64000,
        "supports_images": true,
        "thinking_levels": ["low", "high", "max"]
      }
    ]
  }
}
```

### Provider Fields

- `api` — Optional for built-in presets (defaults to preset protocol, or `openai-responses` for custom providers). One of:
  - `openai-responses`
  - `openai-completions`
  - `anthropic-messages`
  - `google-generative-ai`
- `base_url` — Optional for built-in presets; required for active custom providers.
- `api_key_env` — Optional environment variable name for this provider's API key. If omitted, custom provider `<name>` automatically looks up `<NAME>_API_KEY`.
- `models` — List of model IDs (strings) or model objects with optional `name`, `context_window`, `max_output_tokens`, `supports_images`, and `thinking_levels`. Only models listed here (plus any `-m` CLI model) appear in model pickers.
- `disabled` — Optional boolean (`true` hides the provider; no `api` or `base_url` required).

Entries in `providers.json` are validated individually: a malformed entry emits a warning and is skipped without discarding other valid providers.

## 5. One-Off Endpoint Overrides

CLI flags always take highest precedence (`CLI flags > ~/.jimu/providers.json > built-in presets`):

```bash
jimu \
  --provider my-proxy \
  -m my-model \
  --api openai-completions \
  --base-url https://proxy.example.com/v1 \
  --api-key-env MY_PROXY_KEY \
  --context-window 128000
```
