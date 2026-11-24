use std::path::Path;

/// Build the default base system prompt for headless Pikra hosts (`rpc` / `acp` / one-shot).
///
/// Intentionally domain-neutral and tool-name agnostic: it establishes runtime contracts
/// (`<system-reminder>`, permission denials), general safety/conciseness principles, and
/// concrete runtime environment facts (`cwd`, OS/arch).
pub fn build_default_system_prompt(cwd: &Path, has_tools: bool) -> String {
    let os = match std::env::consts::OS {
        "macos" => "macOS",
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    };
    let arch = std::env::consts::ARCH;
    let cwd_display = cwd.to_string_lossy();

    if has_tools {
        format!(
            "\
You are a helpful assistant with access to tools.

- Verify facts before acting; do not guess file contents or command outputs.
- Call independent tools in parallel when appropriate.
- Proceed freely with safe, reversible local actions; ask before destructive, irreversible, or externally visible actions.
- If a tool call is rejected or fails due to permissions, do not blindly retry—adjust your approach or ask the user.
- Messages wrapped in <system-reminder> tags are runtime context; follow them without echoing internal tags.
- Be concise and direct.

<environment>
Working directory: {cwd_display}
OS: {os} ({arch})
</environment>"
        )
    } else {
        format!(
            "\
You are a helpful assistant.

- Verify facts before acting; do not fabricate details you do not know.
- Messages wrapped in <system-reminder> tags are runtime context; follow them without echoing internal tags.
- Be concise and direct.

<environment>
Working directory: {cwd_display}
OS: {os} ({arch})
</environment>"
        )
    }
}
