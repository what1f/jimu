#![cfg(feature = "tui")]

use jimu::{
    host::{
        HostConfig, discover_extension_paths, load_js_extensions, should_enable_deferred_gateway,
    },
    settings::{StatusLineConfig, UserSettings, discover_custom_themes},
    status_line::{StatusLineRunner, StatusLineSnapshot, parse_ansi_lines, run_command_once},
    tools::ToolSelection,
    ui_mod::{MAX_RENDERED_UI_LINES, UiModBridge, render_node_to_decoration, render_node_to_lines},
};
use jimu_agent::AgentTool;
use jimu_ai::CancelToken;
use jimu_js::{UiComponent, UiDirection, UiNode, UiTextStyle};
use jimu_tui::{
    engine::theme::{Theme, ThemeKind},
    harness::{
        ChromeState, ContextMeterSnapshot, ToolRowView, UsageStats, render_default_status_line,
    },
};
use ratatui::style::{Color, Modifier};
use serde_json::json;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tempfile::tempdir;
use tokio::sync::Notify;

fn sample_snapshot(cwd: &Path) -> StatusLineSnapshot {
    StatusLineSnapshot {
        version: "0.1.0".into(),
        session_id: "sess-123".into(),
        transcript_path: cwd.join("sess-123.jsonl").to_string_lossy().into_owned(),
        model_id: "gpt-5.4".into(),
        model_display_name: "GPT-5.4".into(),
        current_dir: cwd.to_string_lossy().into_owned(),
        project_dir: cwd.to_string_lossy().into_owned(),
        git_branch: "main".into(),
        context_window_size: 200_000,
        used_percentage: 12.34,
        input_tokens: 1_200,
        output_tokens: 340,
        cache_read_input_tokens: 800,
        cache_creation_input_tokens: 100,
        total_cost_usd: 0.0125,
        total_duration_ms: 4_200,
        effort: "high".into(),
        fast_mode: true,
        permission_mode: "acceptEdits".into(),
        running: false,
    }
}

#[tokio::test]
async fn status_line_stdin_json_ansi_parsing_and_timeout_pgid_cleanup() {
    let dir = tempdir().expect("tempdir");
    let snap = sample_snapshot(dir.path());
    let payload = snap.to_json();
    assert_eq!(payload["model"]["id"], "gpt-5.4");
    assert_eq!(payload["git_branch"], "main");
    assert_eq!(payload["context_window"]["used_percentage"], 12.3);
    assert_eq!(payload["fast_mode"], true);

    // 1. Direct ANSI + stdin JSON command execution (verifies TrueColor, 256-color, bold, OSC strip).
    let cmd = r#"input=$(cat); case "$input" in *'"display_name":"GPT-5.4"'*'"git_branch":"main"'*) printf '\033]8;;https://example.com\007\033[1;38;2;12;34;56mGPT-5.4\033[0m \033[38;5;208mmain\033[0m\033]8;;\007\n' ;; *) echo "BAD_JSON"; exit 1 ;; esac"#;
    let lines = run_command_once(cmd, dir.path(), &payload, 2_000)
        .await
        .expect("status_line command should succeed");
    assert_eq!(lines.len(), 1);
    let rendered_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(rendered_text, "GPT-5.4 main");
    assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(12, 34, 56)));
    assert!(
        lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD)
    );
    let branch_span = lines[0]
        .spans
        .iter()
        .find(|s| s.content.as_ref() == "main")
        .expect("branch span");
    assert_eq!(branch_span.style.fg, Some(Color::Indexed(208)));

    // 2. Async StatusLineRunner updates cache, invokes redraw callback, and clears cache when the
    //    command succeeds with empty / whitespace-only output.
    let output_file = dir.path().join("status.txt");
    fs::write(&output_file, "\x1b[32mREADY\x1b[0m\n").unwrap();
    let updated = Arc::new(Notify::new());
    let updated_cb = Arc::clone(&updated);
    let runner = StatusLineRunner::spawn(
        StatusLineConfig {
            command: format!("cat '{}'", output_file.display()),
            refresh_ms: None,
            timeout_ms: Some(2_000),
        },
        dir.path().to_path_buf(),
        Arc::new(move || updated_cb.notify_one()),
    );
    runner.update_snapshot(snap.clone());
    tokio::time::timeout(Duration::from_secs(3), updated.notified())
        .await
        .expect("runner should notify on first render");
    let cached = runner.current_lines().expect("cached status lines");
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].spans[0].content.as_ref(), "READY");
    assert_eq!(cached[0].spans[0].style.fg, Some(Color::Green));

    // Overwrite with whitespace-only output and push a new snapshot -> cache must clear to None.
    fs::write(&output_file, "   \t\n\n").unwrap();
    let mut snap2 = snap;
    snap2.git_branch = "feature".into();
    runner.update_snapshot(snap2);
    tokio::time::timeout(Duration::from_secs(3), updated.notified())
        .await
        .expect("runner should notify when clearing cache on empty output");
    assert!(
        runner.current_lines().is_none(),
        "blank status_line output should clear stale cache"
    );
    drop(runner);

    // 3. Process-group cleanup on BOTH timeout AND normal exit (background grandchild).
    #[cfg(unix)]
    {
        use std::io::Read as _;

        // Helper: create a temporary FIFO and spawn a blocking reader that waits for the
        // grandchild's 1-byte ready marker and then blocks until EOF (kernel closes the fd the
        // instant SIGKILL terminates the grandchild — zero sleep polling).
        let spawn_fifo_watcher = |fifo: &Path| {
            let fifo_c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
            // SAFETY: valid NUL-terminated path for creating a temporary FIFO in tempdir.
            assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
            let fifo_path = fifo.to_path_buf();
            tokio::task::spawn_blocking(move || {
                let mut f = fs::File::open(&fifo_path)?;
                let mut buf = Vec::new();
                f.read_to_end(&mut buf)?;
                Ok::<Vec<u8>, std::io::Error>(buf)
            })
        };

        // 3a. Timeout path: grandchild writes 'a' to FIFO and sleeps 30s; timeout kills -pgid,
        //     closing the grandchild's FIFO write end immediately.
        let timeout_fifo = dir.path().join("timeout.fifo");
        let timeout_watcher = spawn_fifo_watcher(&timeout_fifo);
        let slow_cmd = format!(
            "(printf 'a' >&3; exec sleep 30) </dev/null >/dev/null 2>&1 3>'{}' & wait",
            timeout_fifo.display(),
        );
        let err = run_command_once(&slow_cmd, dir.path(), &payload, 400)
            .await
            .expect_err("slow status_line command should time out");
        assert!(err.contains("timed out"), "unexpected error: {err}");
        let timeout_bytes = tokio::time::timeout(Duration::from_secs(2), timeout_watcher)
            .await
            .expect("grandchild must be killed on timeout (FIFO EOF)")
            .expect("FIFO watcher join")
            .expect("FIFO watcher read");
        assert_eq!(
            timeout_bytes, b"a",
            "grandchild should have started before timeout"
        );

        // 3b. Normal-exit path: script waits via a ready FIFO until the detached background
        //     grandchild is live and holding `normal_fifo` open, then prints OK and exits 0.
        let normal_fifo = dir.path().join("normal.fifo");
        let ready_fifo = dir.path().join("normal-ready.fifo");
        let ready_c = std::ffi::CString::new(ready_fifo.to_string_lossy().as_bytes()).unwrap();
        // SAFETY: valid NUL-terminated path for creating a temporary FIFO in tempdir.
        assert_eq!(unsafe { libc::mkfifo(ready_c.as_ptr(), 0o600) }, 0);
        let normal_watcher = spawn_fifo_watcher(&normal_fifo);
        let normal_cmd = format!(
            "(printf 'a' >&3; printf 'r\\n' > '{}'; exec sleep 30) </dev/null >/dev/null 2>&1 3>'{}' & read _ < '{}'; printf 'OK\\n'",
            ready_fifo.display(),
            normal_fifo.display(),
            ready_fifo.display(),
        );
        let ok_lines = run_command_once(&normal_cmd, dir.path(), &payload, 2_000)
            .await
            .expect("normal status_line command should succeed");
        assert_eq!(ok_lines.len(), 1);
        assert_eq!(ok_lines[0].spans[0].content.as_ref(), "OK");
        let normal_bytes = tokio::time::timeout(Duration::from_secs(2), normal_watcher)
            .await
            .expect("background grandchild must be killed on normal exit (FIFO EOF)")
            .expect("FIFO watcher join")
            .expect("FIFO watcher read");
        assert_eq!(
            normal_bytes, b"a",
            "background grandchild should have started before normal exit killed -pgid"
        );
    }

    // 4. Verify vte-based ANSI parser strips non-SGR CSI cleanly and NEVER panics on malformed
    //    or incomplete ESC sequences followed immediately by multi-byte UTF-8 text.
    let parsed = parse_ansi_lines("\x1b[2K\x1b[4;91merr\x1b[0m\r\n");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].spans[0].content.as_ref(), "err");
    assert_eq!(parsed[0].spans[0].style.fg, Some(Color::LightRed));
    assert!(
        parsed[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::UNDERLINED)
    );

    for malformed in ["\x1b(中文", "\x1b[31中文", "\x1b中文"] {
        let _ = parse_ansi_lines(malformed);
        let surrounded = format!("前缀{malformed}\x1b[0m正常中文");
        let lines = parse_ansi_lines(&surrounded);
        let combined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            combined.contains("前缀") && combined.contains("正常中文"),
            "normal text around malformed ESC {malformed:?} must be preserved, got {combined:?}"
        );
    }
    let cjk_styled = parse_ansi_lines("\x1b(B\x1b[32m状态正常\x1b[0m");
    assert_eq!(cjk_styled.len(), 1);
    assert_eq!(cjk_styled[0].spans[0].content.as_ref(), "状态正常");
    assert_eq!(cjk_styled[0].spans[0].style.fg, Some(Color::Green));
}

#[test]
fn custom_theme_discovery_project_overrides_user_and_skips_invalid_json() {
    let home = tempdir().expect("home tempdir");
    let cwd = tempdir().expect("cwd tempdir");

    let user_themes = home.path().join("themes");
    let project_themes = cwd.path().join(".jimu").join("themes");
    fs::create_dir_all(&user_themes).unwrap();
    fs::create_dir_all(&project_themes).unwrap();

    fs::write(
        user_themes.join("ocean.json"),
        r##"{"base":"dark","colors":{"accent":"#112233","bg":"#010203"}}"##,
    )
    .unwrap();
    fs::write(
        user_themes.join("user-only.json"),
        r##"{"base":"light","colors":{"fg":"#222222"}}"##,
    )
    .unwrap();
    fs::write(user_themes.join("broken.json"), r#"{ not valid json"#).unwrap();

    // Project theme overrides user theme with matching name (case-insensitive).
    fs::write(
        project_themes.join("Ocean.json"),
        r##"{"base":"dark","colors":{"accent":"#abcdef"}}"##,
    )
    .unwrap();

    let (themes, warnings) = discover_custom_themes(home.path(), cwd.path());
    assert_eq!(
        warnings.len(),
        1,
        "expected 1 warning for broken.json: {warnings:?}"
    );
    assert!(warnings[0].contains("broken.json"));
    assert!(themes.contains_key("user-only"));
    assert!(
        !themes.contains_key("ocean"),
        "case-insensitive collision should be replaced"
    );
    let ocean = themes.get("Ocean").expect("project Ocean theme");
    assert_eq!(
        ocean.colors.get("accent").map(String::as_str),
        Some("#abcdef")
    );

    // UserSettings accepts custom theme names, status_line, show_header, and extensions.
    fs::write(
        UserSettings::path(home.path()),
        r#"{
            "theme": "Ocean",
            "show_header": true,
            "status_line": { "command": "echo hi", "refresh_ms": 2000, "timeout_ms": 1000 },
            "extensions": ["./ext/custom.js"]
        }"#,
    )
    .unwrap();
    let (settings, settings_warnings) = UserSettings::load_with_warnings(home.path());
    assert!(
        settings_warnings.is_empty(),
        "unexpected warnings: {settings_warnings:?}"
    );
    assert_eq!(settings.theme.as_deref(), Some("Ocean"));
    assert_eq!(settings.show_header, Some(true));
    assert_eq!(
        settings.status_line,
        Some(StatusLineConfig {
            command: "echo hi".into(),
            refresh_ms: Some(2000),
            timeout_ms: Some(1000),
        })
    );
    assert_eq!(settings.extensions.len(), 1);
}

#[tokio::test]
async fn ui_mod_bridge_translates_onion_wrappers_and_full_replacements() {
    let dir = tempdir().expect("tempdir");
    let ext_path = dir.path().join("ui-mod.js");
    fs::write(
        &ext_path,
        r##"
export default function (jimu) {
  jimu.ui.theme({ accent: "#00ff88" });

  jimu.on("ui.render", (e, next) => {
    if (e.component === "StatusLine") {
      return jimu.ui.row({ gap: 1 }, [
        next(e),
        jimu.ui.text("[mod]", { color: "accent_success", bold: true }),
      ]);
    }
    if (e.component === "UserMessage") {
      return jimu.ui.box(
        { border: "left", border_color: "#ff5500" },
        [
          jimu.ui.text("USER_PREFIX", { color: "muted" }),
          next(e),
          jimu.ui.text("USER_SUFFIX", { dim: true }),
        ]
      );
    }
    if (e.component === "ToolRow") {
      return jimu.ui.text("TOOL:" + e.props.name, { color: "#11aaee" });
    }
    return next(e);
  });
}
"##,
    )
    .unwrap();

    let loaded = load_js_extensions(&[ext_path], &HashSet::new())
        .expect("load ui-mod.js")
        .expect("loaded extensions");
    assert!(loaded.runtime.has_ui_render());
    assert_eq!(
        loaded
            .runtime
            .ui_theme_overrides()
            .get("accent")
            .map(String::as_str),
        Some("#00ff88")
    );

    let chrome_updated = Arc::new(Notify::new());
    let history_updated = Arc::new(Notify::new());
    let chrome_count = Arc::new(AtomicUsize::new(0));
    let history_count = Arc::new(AtomicUsize::new(0));
    let chrome_cb = {
        let n = Arc::clone(&chrome_updated);
        let c = Arc::clone(&chrome_count);
        Arc::new(move || {
            c.fetch_add(1, Ordering::Relaxed);
            n.notify_one();
        })
    };
    let history_cb = {
        let n = Arc::clone(&history_updated);
        let c = Arc::clone(&history_count);
        Arc::new(move || {
            c.fetch_add(1, Ordering::Relaxed);
            n.notify_one();
        })
    };
    let bridge =
        UiModBridge::with_callbacks(Some(Arc::clone(&loaded.runtime)), chrome_cb, history_cb);

    let theme = Theme::for_kind(ThemeKind::Dark);
    let usage = UsageStats::default();
    let ctx = ContextMeterSnapshot {
        tokens: Some(1_000),
        estimated_tokens: 1_000,
        context_window: 100_000,
        percent: Some(1.0),
        compaction_threshold: None,
    };
    let chrome_state = ChromeState {
        running: false,
        usage: &usage,
        context: Some(&ctx),
        model_name: Some("GPT-5.4"),
        queued_count: 0,
        theme: &theme,
        width: 80,
        cwd: dir.path(),
        git_branch: Some("main"),
        effort: Some("high"),
        permission_mode: Some("default"),
        fast_mode: false,
        session_id: Some("sess-1"),
    };

    // 1. Trigger StatusLine async render via O(1) cache miss -> fires lightweight chrome callback
    //    (NOT history layout invalidation) and preserves default status line via `next(e)`.
    assert!(
        bridge
            .slot_node(UiComponent::StatusLine, &chrome_state)
            .is_none()
    );
    tokio::time::timeout(Duration::from_secs(3), chrome_updated.notified())
        .await
        .expect("UiModBridge should finish StatusLine render");
    assert_eq!(history_count.load(Ordering::Relaxed), 0);
    assert!(chrome_count.load(Ordering::Relaxed) >= 1);

    let native_default = vec![render_default_status_line(&chrome_state)];
    let rendered = bridge
        .render_chrome_slot(UiComponent::StatusLine, &chrome_state, &native_default)
        .expect("rendered StatusLine wrapping native default bar");
    assert_eq!(rendered.len(), 1);
    let text: String = rendered[0]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("GPT-5.4") && text.contains("[mod]"),
        "wrapped StatusLine must keep default model name and custom badge, got: {text:?}"
    );
    let badge = rendered[0]
        .spans
        .iter()
        .find(|s| s.content.as_ref() == "[mod]")
        .expect("badge span");
    assert_eq!(badge.style.fg, Some(theme.accent_success));
    assert!(badge.style.add_modifier.contains(Modifier::BOLD));

    // 2. Trigger UserMessage onion-wrapper render -> fires history invalidation callback.
    assert!(
        bridge
            .user_message_decoration("hello world", 1, 80, &theme)
            .is_none()
    );
    tokio::time::timeout(Duration::from_secs(3), history_updated.notified())
        .await
        .expect("UiModBridge should finish UserMessage render");
    assert!(history_count.load(Ordering::Relaxed) >= 1);
    let user_dec = bridge
        .user_message_decoration("hello world", 1, 80, &theme)
        .expect("cached UserMessage decoration");
    assert!(
        user_dec.replace.is_none(),
        "onion wrapper must preserve native message"
    );
    assert_eq!(user_dec.left_border, Some(Color::Rgb(0xff, 0x55, 0x00)));
    assert_eq!(user_dec.prefix_lines.len(), 1);
    assert_eq!(
        user_dec.prefix_lines[0].spans[0].content.as_ref(),
        "USER_PREFIX"
    );
    assert_eq!(user_dec.suffix_lines.len(), 1);
    assert_eq!(
        user_dec.suffix_lines[0].spans[0].content.as_ref(),
        "USER_SUFFIX"
    );

    // 3. Trigger ToolRow full replacement -> SurfaceDecoration with replace: Some(...).
    let tool_view = ToolRowView {
        name: "grep",
        running: false,
        is_error: false,
        width: 80,
    };
    assert!(bridge.tool_row_decoration(&tool_view, &theme).is_none());
    tokio::time::timeout(Duration::from_secs(3), history_updated.notified())
        .await
        .expect("UiModBridge should finish ToolRow render");
    let tool_dec = bridge
        .tool_row_decoration(&tool_view, &theme)
        .expect("cached ToolRow decoration");
    let replaced = tool_dec
        .replace
        .expect("full replacement should populate replace");
    assert_eq!(replaced.len(), 1);
    assert_eq!(replaced[0].spans[0].content.as_ref(), "TOOL:grep");
    assert_eq!(
        replaced[0].spans[0].style.fg,
        Some(Color::Rgb(0x11, 0xaa, 0xee))
    );

    // 4. Hard line cap bounds huge expansions cleanly.
    let huge_column = UiNode::Box {
        direction: UiDirection::Column,
        gap: 0,
        border: jimu_js::UiBorder::None,
        border_color: None,
        bg: None,
        padding_left: 0,
        padding_right: 0,
        children: (0..200)
            .map(|i| UiNode::Text {
                text: format!("line-{i}"),
                style: UiTextStyle::default(),
            })
            .collect(),
    };
    let capped = render_node_to_lines(&huge_column, &[], &theme, 80);
    assert_eq!(capped.len(), MAX_RENDERED_UI_LINES);

    // Pure Native node produces None decoration.
    assert!(render_node_to_decoration(&jimu_js::UiNode::Native, &theme, 80).is_none());
}

#[tokio::test]
async fn headless_extension_discovery_and_ui_mod_zero_side_effects() {
    let home = tempdir().expect("home tempdir");
    let cwd = tempdir().expect("cwd tempdir");

    let user_ext_dir = home.path().join("extensions");
    let proj_ext_dir = cwd.path().join(".jimu").join("extensions");
    fs::create_dir_all(&user_ext_dir).unwrap();
    fs::create_dir_all(&proj_ext_dir).unwrap();

    // Same-named file in user & project: project version must win.
    fs::write(
        user_ext_dir.join("shared.js"),
        r#"
export default function (jimu) {
  jimu.registerTool({
    name: "ping_ext",
    description: "user version",
    execute() { return "from-user"; }
  });
}
"#,
    )
    .unwrap();
    fs::write(
        proj_ext_dir.join("shared.js"),
        r##"
export default function (jimu) {
  jimu.ui.theme({ accent: "#ff00ff" });
  jimu.on("ui.render", (e, next) => jimu.ui.box([next(e), jimu.ui.text("ui")]));
  jimu.on("before_tool_call", () => undefined);
  jimu.registerTool({
    name: "ping_ext",
    description: "project version",
    parameters: {
      type: "object",
      properties: { msg: { type: "string" } },
      required: ["msg"]
    },
    execute(_id, args) { return "from-project:" + (args.msg || ""); }
  });
}
"##,
    )
    .unwrap();

    let discovered = discover_extension_paths(home.path(), cwd.path(), &[], &[]);
    assert_eq!(discovered.len(), 1);
    assert!(discovered[0].starts_with(&proj_ext_dir));

    // Load in headless mode: tool works, UI render is never invoked, zero errors.
    let loaded = load_js_extensions(&discovered, &HashSet::new())
        .expect("headless load should succeed")
        .expect("loaded extension");
    assert_eq!(loaded.runtime.ui_render_invocations(), 0);
    assert!(loaded.runtime.last_error().is_none());

    let tool: Arc<dyn AgentTool> = loaded
        .tools
        .iter()
        .find(|t| t.definition().name == "ping_ext")
        .cloned()
        .expect("ping_ext tool");
    let result = tool
        .execute(
            "call-1".to_owned(),
            json!({ "msg": "ok" }),
            CancelToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .expect("ping_ext execution");
    let text = result
        .content
        .iter()
        .filter_map(|p| match p {
            jimu_ai::ContentPart::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(text, "from-project:ok");
    assert_eq!(loaded.runtime.ui_render_invocations(), 0);

    // Settings-relative paths resolve against `jimu_home` (NOT `cwd`), and explicit directories
    // overlapping with auto-discovered directories are expanded and deduplicated by canonical path.
    let home_custom_dir = home.path().join("custom-exts");
    fs::create_dir_all(&home_custom_dir).unwrap();
    let ui_only_path = home_custom_dir.join("ui-only.js");
    fs::write(
        &ui_only_path,
        r##"
export default function (jimu) {
  jimu.ui.theme({ accent: "#123456" });
  jimu.on("ui.render", (e, next) => next(e));
}
"##,
    )
    .unwrap();

    let deduped = discover_extension_paths(
        home.path(),
        cwd.path(),
        &[PathBuf::from("./custom-exts/ui-only.js")],
        &[proj_ext_dir.clone(), home_custom_dir.clone()],
    );
    assert_eq!(
        deduped.len(),
        2,
        "overlapping directory entries must deduplicate canonically: {deduped:?}"
    );
    assert!(deduped.iter().any(|p| p.ends_with("shared.js")));
    assert!(deduped.iter().any(|p| p.ends_with("ui-only.js")));

    // Pure UI extension registers zero tools -> must NOT trigger deferred gateway under `--no-tools`.
    let ui_only_loaded = load_js_extensions(&[ui_only_path], &HashSet::new())
        .expect("load ui-only.js")
        .expect("loaded ui-only");
    assert!(ui_only_loaded.tools.is_empty());
    assert!(!should_enable_deferred_gateway(
        &ToolSelection::none(),
        false,
        !ui_only_loaded.tools.is_empty(),
        false,
    ));

    // HostConfig::resolve_extension_paths isolates project extensions per workspace cwd (ACP).
    let other_cwd = tempdir().expect("other cwd");
    let other_proj_dir = other_cwd.path().join(".jimu").join("extensions");
    fs::create_dir_all(&other_proj_dir).unwrap();
    fs::write(
        other_proj_dir.join("other-only.js"),
        "export default function () {}",
    )
    .unwrap();
    let host_cfg = HostConfig {
        cwd: cwd.path().to_path_buf(),
        jimu_home: home.path().to_path_buf(),
        ..HostConfig::default()
    };
    let resolved_other = host_cfg.resolve_extension_paths(other_cwd.path());
    assert!(resolved_other.iter().any(|p| p.ends_with("other-only.js")));
    assert!(
        !resolved_other.iter().any(|p| p.starts_with(&proj_ext_dir)),
        "session in other_cwd must not inherit startup cwd's project extensions: {resolved_other:?}"
    );
}
