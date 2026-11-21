#![cfg(all(feature = "patch", feature = "search"))]

use pikra_agent::{AgentTool, AgentToolResult};
use pikra_ai::{CancelToken, ContentPart};
use pikra_tools::{
    ApplyPatchTool, BuiltinToolKind, BuiltinToolSet, DescriptionOverride, EditTool, FindTool,
    GrepTool, ToolDefinitionOverride, WorkspaceTools, WorkspaceToolsConfig, WriteTool,
    wrap_tool_with_override,
};
use serde_json::{Value, json};
use std::{fs, sync::Arc};
use tempfile::TempDir;

async fn run(tool: &dyn AgentTool, args: Value) -> Result<AgentToolResult, String> {
    tool.execute("call".into(), args, CancelToken::new(), Arc::new(|_| {}))
        .await
}

fn text(result: &AgentToolResult) -> &str {
    assert!(!result.is_error, "{result:?}");
    let ContentPart::Text(content) = &result.content[0] else {
        panic!("Expected text output")
    };
    &content.text
}

#[tokio::test]
async fn patch_preflights_all_files_then_applies_ordered_chunks_and_moves() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    fs::write(
        root.join("story.txt"),
        "Title\r\n  “Hello”—world \r\nkeep \r\nlast\r\n",
    )
    .unwrap();
    fs::write(root.join("old.txt"), "old").unwrap();
    fs::write(root.join("obsolete.txt"), [0, 1, 2]).unwrap();

    let config = WorkspaceToolsConfig::new(root);
    assert!(!config.enabled_tools.contains(BuiltinToolKind::ApplyPatch));
    assert!(BuiltinToolSet::all().contains(BuiltinToolKind::ApplyPatch));
    let patch = ApplyPatchTool::new(Arc::new(config));
    let bad = "*** Begin Patch\n*** Add File: invalid/new.txt\n+new\n*** Update File: story.txt\n@@\n-absent\n+changed\n*** End Patch";
    assert!(run(&patch, json!({"patch": bad})).await.is_err());
    assert!(!root.join("invalid").exists());

    #[cfg(unix)]
    let story_path = {
        use std::os::unix::fs::{PermissionsExt, symlink};
        fs::set_permissions(root.join("story.txt"), fs::Permissions::from_mode(0o640)).unwrap();
        symlink("story.txt", root.join("linked.txt")).unwrap();
        "linked.txt"
    };
    #[cfg(not(unix))]
    let story_path = "story.txt";

    let valid = format!(
        "*** Begin Patch\n*** Add File: new/added.txt\n+new\n*** Update File: {story_path}\n@@ Title\n-\"Hello\"-world\n+updated\n keep\n@@\n-last\n+done\n*** End of File\n*** Update File: old.txt\n*** Move to: new/moved.txt\n@@\n-old\n+moved\n*** Delete File: obsolete.txt\n*** End Patch"
    );
    let result = run(&patch, json!({"patch": valid})).await.unwrap();
    assert!(text(&result).contains("Moved old.txt to new/moved.txt"));
    assert_eq!(
        result.details["added"],
        json!(["new/added.txt", "new/moved.txt"])
    );
    assert_eq!(result.details["modified"], json!([story_path]));
    assert_eq!(
        result.details["deleted"],
        json!(["old.txt", "obsolete.txt"])
    );
    assert_eq!(
        fs::read_to_string(root.join("new/added.txt")).unwrap(),
        "new\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("new/moved.txt")).unwrap(),
        "moved\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("story.txt")).unwrap(),
        "Title\r\nupdated\r\nkeep \r\ndone\r\n"
    );
    assert!(!root.join("old.txt").exists());
    assert!(!root.join("obsolete.txt").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert!(root.join(story_path).is_symlink());
        assert_eq!(
            root.join(story_path)
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }
}

#[tokio::test]
async fn edit_fuzzes_whole_lines_and_rejects_ambiguous_batches() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("text.txt");
    let original = "header\r\n  “quoted”—one\u{202f}two \r\n  repeat \r\n  repeat \r\n";
    fs::write(&file, original).unwrap();

    let edit = EditTool::new(Arc::new(WorkspaceToolsConfig::new(dir.path())));
    let change = json!({"oldText": "\"quoted\"-one two", "newText": "changed"});
    let failed = run(
        &edit,
        json!({"path": "text.txt", "edits": [change, {
            "oldText": "repeat\t", "newText": "unique"
        }]}),
    )
    .await
    .unwrap_err();
    assert!(failed.contains("multiple locations"), "{failed}");
    assert_eq!(fs::read_to_string(&file).unwrap(), original);
    run(&edit, json!({"path": "text.txt", "edits": [change]}))
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(file).unwrap(),
        "header\r\nchanged\r\n  repeat \r\n  repeat \r\n"
    );
}

#[tokio::test]
async fn path_and_content_search_respect_filters_modes_limits_and_cancellation() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    for name in [".git", "a", "target", "custom"] {
        fs::create_dir(root.join(name)).unwrap();
    }
    for name in [
        "ignored.rs",
        "filtered.rs",
        "target/hit.rs",
        "custom/hit.rs",
        ".git/hit.rs",
        ".hidden.rs",
        "a/hit.rs",
        "z.rs",
    ] {
        fs::write(root.join(name), "before\r\nNeed.le\r\nafter\r\nneed.le\r\n").unwrap();
    }
    fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
    fs::write(root.join(".ignore"), "filtered.rs\n").unwrap();
    fs::write(root.join("a/binary.rs"), b"Need.le\n\0").unwrap();
    fs::write(
        root.join("a/long.txt"),
        format!("Need.le {}\n", "中文".repeat(500)),
    )
    .unwrap();
    let config = Arc::new(WorkspaceToolsConfig::new(root).with_ignore_dirs(["target", "custom"]));
    let find = FindTool::new(config.clone());
    let grep = GrepTool::new(config);

    let result = run(
        &find,
        json!({"pattern": "*.rs", "max_depth": 1, "hidden": true}),
    )
    .await
    .unwrap();
    assert_eq!(text(&result), ".hidden.rs\nz.rs");
    let result = run(&find, json!({"pattern": "hit"})).await.unwrap();
    assert_eq!(text(&result), "a/hit.rs");
    let result = run(&find, json!({"pattern": "a"})).await.unwrap();
    assert!(text(&result).lines().any(|line| line == "a"));

    let result = run(&grep, json!({"pattern": "need.le", "fixed_strings": true, "case_insensitive": true, "path": "a", "glob": "*.rs", "context_lines": 1, "limit": 1})).await.unwrap();
    assert_eq!(
        text(&result),
        "a/hit.rs-1-before\na/hit.rs:2:Need.le\na/hit.rs-3-after"
    );
    assert_eq!(result.details["hit_limit"], true);
    let result = run(
        &grep,
        json!({"pattern": r"(?i)^need\.le$", "glob": "!a/**", "output_mode": "count"}),
    )
    .await
    .unwrap();
    assert_eq!(text(&result), "z.rs:2");
    let result = run(
        &grep,
        json!({"pattern": "Need", "glob": "*.rs", "output_mode": "files_with_matches"}),
    )
    .await
    .unwrap();
    assert_eq!(
        text(&result),
        ".hidden.rs\na/hit.rs\nfiltered.rs\nignored.rs\nz.rs"
    );
    let result = run(&grep, json!({"pattern": "Need", "path": "a/long.txt"}))
        .await
        .unwrap();
    assert!(text(&result).ends_with('…'));
    assert!(!text(&result).contains('\u{FFFD}'));

    let capped = GrepTool::new(Arc::new(
        WorkspaceToolsConfig::new(root).with_max_output_bytes(120),
    ));
    let result = run(
        &capped,
        json!({"pattern": "Need", "path": root.join("a/long.txt")}),
    )
    .await
    .unwrap();
    assert!(text(&result).len() <= 120);
    assert_eq!(result.details["truncated"], true);
    assert!(result.details["omitted_bytes"].as_u64().unwrap() > 0);

    let cancel = CancelToken::new();
    cancel.cancel();
    for tool in [&find as &dyn AgentTool, &grep] {
        assert!(
            tool.execute(
                "call".into(),
                json!({"pattern": "Need"}),
                cancel.clone(),
                Arc::new(|_| {})
            )
            .await
            .unwrap_err()
            .contains("aborted")
        );
    }
}

#[tokio::test]
async fn host_schema_overrides_hide_and_pin_arguments() {
    let dir = TempDir::new().unwrap();
    let config = Arc::new(WorkspaceToolsConfig::new(dir.path()));
    let write: Arc<dyn AgentTool> = Arc::new(WriteTool::new(config.clone()));
    let mut spec = ToolDefinitionOverride {
        rename: Some("save_note".into()),
        description: Some(DescriptionOverride::Replace("Save the note.".into())),
        hidden_parameters: vec!["path".into()],
        parameter_descriptions: [("content".into(), "Note text.".into())].into(),
        ..Default::default()
    };
    assert!(wrap_tool_with_override(write.clone(), spec.clone()).is_err());
    spec.default_arguments
        .insert("path".into(), json!("note.txt"));
    spec.default_arguments
        .insert("content".into(), json!("default"));

    let tool = wrap_tool_with_override(write.clone(), spec.clone()).unwrap();
    assert_eq!(tool.execution_mode(), write.execution_mode());
    run(tool.as_ref(), json!({"path": "other.txt"}))
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("note.txt")).unwrap(),
        "default"
    );
    assert!(!dir.path().join("other.txt").exists());

    let mut host = (*config)
        .clone()
        .with_enabled_tools([BuiltinToolKind::Write])
        .with_tool_override(BuiltinToolKind::Write, spec);
    let tools = WorkspaceTools::try_build(host.clone()).unwrap();
    let definition = tools[0].definition();
    assert_eq!(definition.name, "save_note");
    assert_eq!(definition.description, "Save the note.");
    assert!(definition.parameters.get("required").is_none());
    assert_eq!(
        definition.parameters["properties"],
        json!({"content": {"type": "string", "description": "Note text."}})
    );
    run(tools[0].as_ref(), json!({"content": "explicit"}))
        .await
        .unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("note.txt")).unwrap(),
        "explicit"
    );

    let spec = host
        .tool_overrides
        .get_mut(&BuiltinToolKind::Write)
        .unwrap();
    spec.description = Some(DescriptionOverride::Append("Keep it short.".into()));
    let definitions = WorkspaceTools::new(host).definitions();
    assert!(definitions[0].description.ends_with("\nKeep it short."));
}

#[tokio::test]
async fn patch_trailing_newline_grep_fifo_unreadable_and_bash_bounds_work() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let config = Arc::new(WorkspaceToolsConfig::new(root));

    // 1. apply_patch preserves missing trailing newline on untouched final line,
    // and inserts newline before newly appended trailing lines.
    fs::write(root.join("no_nl.txt"), "alpha\nbeta").unwrap();
    let patch_tool = ApplyPatchTool::new(config.clone());
    run(
        &patch_tool,
        json!({"patch": "*** Begin Patch\n*** Update File: no_nl.txt\n@@\n-alpha\n+ALPHA\n*** End Patch"}),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(root.join("no_nl.txt")).unwrap(),
        "ALPHA\nbeta"
    );

    run(
        &patch_tool,
        json!({"patch": "*** Begin Patch\n*** Update File: no_nl.txt\n@@\n beta\n+gamma\n*** End Patch"}),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(root.join("no_nl.txt")).unwrap(),
        "ALPHA\nbeta\ngamma\n"
    );

    // 2. grep skips FIFOs without hanging and skips unreadable child files.
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;

        fs::write(root.join("good.txt"), "NEEDLE_HERE\n").unwrap();
        fs::write(root.join("unreadable.txt"), "NEEDLE_HIDDEN\n").unwrap();
        fs::set_permissions(
            root.join("unreadable.txt"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        let fifo = root.join("trap.fifo");
        let c_fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);

        let grep = GrepTool::new(config.clone());
        let found = run(&grep, json!({"pattern": "NEEDLE_HERE"})).await.unwrap();
        assert!(
            text(&found).contains("good.txt:1:NEEDLE_HERE"),
            "{}",
            text(&found)
        );

        let _ = fs::set_permissions(
            root.join("unreadable.txt"),
            fs::Permissions::from_mode(0o600),
        );
    }

    // 3. ExecCommandTool::one_shot rejects timeout_ms > 86_400_000, omits AI_AGENT=pikra,
    //    handles invalid UTF-8 lossily, and ReadTool caps minified lines > 2,000 chars.
    #[cfg(not(target_os = "ios"))]
    {
        let exec = pikra_tools::ExecCommandTool::one_shot(config.clone());
        let err = run(
            &exec,
            json!({"command": "echo hi", "timeout_ms": 86_400_001}),
        )
        .await
        .unwrap_err();
        assert!(err.contains("86400000"), "{err}");

        let env_out = run(
            &exec,
            json!({"command": "printf '%s' \"${AI_AGENT:-unset}\""}),
        )
        .await
        .unwrap();
        assert_eq!(text(&env_out), "unset");

        let invalid_utf8 = run(&exec, json!({"command": "printf 'ok\\377tail'"}))
            .await
            .unwrap();
        let structured = invalid_utf8.structured_content.as_ref().unwrap();
        assert_eq!(structured["output_truncated"], json!(false));
        assert!(
            structured["output"].as_str().unwrap().contains("ok")
                && structured["output"].as_str().unwrap().contains("tail"),
            "{structured}"
        );
    }

    let minified = root.join("minified.js");
    fs::write(&minified, format!("{}\nshort\n", "x".repeat(5_000))).unwrap();
    let read_tool = pikra_tools::ReadTool::new(config.clone());
    let read_min = run(&read_tool, json!({"path": "minified.js"}))
        .await
        .unwrap();
    let read_min_text = text(&read_min);
    assert!(
        read_min_text.contains("[line truncated, 5000 chars]"),
        "{read_min_text}"
    );
    assert!(read_min_text.contains("2: short"), "{read_min_text}");
    assert_eq!(read_min.details["truncated"], json!(true));
}
