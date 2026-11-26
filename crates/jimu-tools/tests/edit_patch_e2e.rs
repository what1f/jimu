#![cfg(feature = "patch")]

use jimu_agent::{AgentTool, AgentToolResult};
use jimu_ai::{CancelToken, ContentPart};
use jimu_tools::{ApplyPatchTool, EditTool, WorkspaceToolsConfig};
use serde_json::{Value, json};
use std::{fs, sync::Arc};
use tempfile::tempdir;

async fn run(tool: &dyn AgentTool, args: Value) -> Result<AgentToolResult, String> {
    tool.execute("call".into(), args, CancelToken::new(), Arc::new(|_| {}))
        .await
}

#[tokio::test]
async fn edit_matches_original_and_preserves_bom_and_local_endings() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("text");
    fs::write(&file, "\u{feff}head\r\n猫\r\ntail\nend").unwrap();
    let edit = EditTool::new(Arc::new(
        WorkspaceToolsConfig::new(dir.path()).with_max_output_bytes(128),
    ));
    let long_line = "界".repeat(80);
    let result = run(
        &edit,
        json!({"path":"text","edits":[
            {"oldText":"head\n猫","newText":"head\ntail\nextra"},
            {"oldText":"tail","newText":format!("local\n{long_line}")}
        ]}),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read_to_string(file).unwrap(),
        format!("\u{feff}head\r\ntail\r\nextra\r\nlocal\n{long_line}\nend")
    );
    assert_eq!(result.details["first_changed_line"], 2);
    assert_eq!(result.details["edits_applied"], 2);
    let diff = result.details["diff"].as_str().unwrap();
    let ContentPart::Text(body) = &result.content[0] else {
        panic!("expected edit summary")
    };
    assert!(body.text.contains(diff) && body.text.len() <= 128 && diff.contains("truncated"));
    assert!(diff.contains("-猫") && diff.contains("+tail"), "{diff}");
}

#[tokio::test]
async fn edit_rejects_overlap_and_no_change_before_writing() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("text");
    fs::write(&file, "abc").unwrap();
    let edit = EditTool::new(Arc::new(WorkspaceToolsConfig::new(dir.path())));
    for (edits, message) in [
        (
            json!([{"oldText":"abc","newText":"X"},{"oldText":"b","newText":"Y"}]),
            "overlap",
        ),
        (
            json!([{"oldText":"a","newText":"A"},{"oldText":"c","newText":"c"}]),
            "No change",
        ),
        (
            json!([{"oldText":"a","newText":"ab"},{"oldText":"bc","newText":"c"}]),
            "No change",
        ),
    ] {
        let error = run(&edit, json!({"path":"text", "edits":edits}))
            .await
            .unwrap_err();
        assert!(error.contains(message), "{error}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "abc");
    }
}

#[tokio::test]
async fn patch_inserts_after_anchor_matches_eof_and_rejects_delete_body() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("text");
    fs::write(&file, "\u{feff}start\r\nrepeat\nkeep\r\nrepeat \r\n").unwrap();
    let patch = ApplyPatchTool::new(Arc::new(WorkspaceToolsConfig::new(dir.path())));
    let args = |body| json!({"patch":format!("*** Begin Patch\n{body}\n*** End Patch")});
    run(
        &patch,
        args("*** Update File: text\n@@ start\n+inserted\n@@\n-repeat\n+last\n*** End of File"),
    )
    .await
    .unwrap();
    let expected = "\u{feff}start\r\ninserted\r\nrepeat\nkeep\r\nlast\r\n";
    assert_eq!(fs::read_to_string(&file).unwrap(), expected);
    assert!(
        run(
            &patch,
            args("*** Update File: text\n@@\n-repeat\n+wrong\n*** End of File")
        )
        .await
        .is_err()
    );
    assert!(
        run(&patch, args("*** Delete File: text\n+unexpected"))
            .await
            .unwrap_err()
            .contains("patch line 3")
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), expected);
}
