use pikra_agent::{AgentTool, AgentToolResult};
use pikra_ai::CancelToken;
use pikra_schema::CompiledSchema;
use pikra_tools::{SleepTool, WakeGate};
use serde_json::{Value, json};
use std::sync::Arc;

async fn call(tool: &dyn AgentTool, args: Value) -> Result<AgentToolResult, String> {
    let schema = CompiledSchema::compile(tool.definition().parameters).unwrap();
    let args = schema
        .coerce_and_validate(&args)
        .map_err(|err| err.to_string())?;
    tool.execute("test".into(), args, CancelToken::new(), Arc::new(|_| {}))
        .await
}

#[tokio::test]
async fn sleep_wakes_on_signal_and_observes_cancellation() {
    let gate = WakeGate::new();
    let tool = SleepTool::new(gate.clone());
    gate.notify("old event");
    let result = tool
        .execute(
            "wait".into(),
            json!({"duration_ms": 43_200_000, "reason": "waiting for a signal"}),
            CancelToken::new(),
            Arc::new(move |_| gate.notify("new input")),
        )
        .await
        .unwrap();
    assert_eq!(result.details["woken_early"], true);
    assert_eq!(result.details["wake_reason"], "new input");

    let cancel = CancelToken::new();
    cancel.cancel();
    let error = tool
        .execute(
            "cancelled".into(),
            json!({"duration_ms": 1}),
            cancel,
            Arc::new(|_| {}),
        )
        .await
        .unwrap_err();
    assert_eq!(error, "Operation aborted");
    let done = call(&tool, json!({"duration_ms": 1})).await.unwrap();
    assert_eq!(done.details["woken_early"], false);
}

#[cfg(all(unix, not(target_os = "ios")))]
mod subprocesses {
    use super::*;
    use pikra_ai::ContentPart;
    use pikra_tools::{
        BackgroundEventCallback, BackgroundToolEvent, ExecCommandTool, MonitorConfig,
        MonitorManager, MonitorStopReason, MonitorTool, ProcessSessionManager,
        WorkspaceToolsConfig, WriteStdinTool, format_background_events_xml,
    };
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use tempfile::TempDir;
    use tokio::io::AsyncReadExt;
    use tokio::sync::{mpsc, oneshot};

    fn text(result: &AgentToolResult) -> &str {
        let ContentPart::Text(text) = &result.content[0] else {
            panic!("expected text");
        };
        &text.text
    }

    fn events() -> (
        BackgroundEventCallback,
        mpsc::UnboundedReceiver<BackgroundToolEvent>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(move |event| tx.send(event).unwrap()), rx)
    }

    fn fifo(path: &Path) {
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a valid, NUL-terminated path in the test's private directory.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    }

    async fn await_output(stdin: &WriteStdinTool, id: &Value, mut output: String, expected: &str) {
        while !output.contains(expected) {
            let next = call(stdin, json!({"session_id": id})).await.unwrap();
            assert!(
                !text(&next).is_empty(),
                "no output while waiting for {expected:?}: {output:?}"
            );
            output.push_str(text(&next));
        }
    }

    #[tokio::test]
    async fn foreground_and_naturally_completed_background_commands_return_output() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let workspace = Arc::new(WorkspaceToolsConfig::new(dir.path()).with_env_provider(
            Arc::new(|| Box::pin(async { vec![("PIKRA_TEST_VAR".into(), "provided".into())] })),
        ));
        let (callback, mut events) = events();
        let manager = ProcessSessionManager::new(workspace, WakeGate::new(), Some(callback));
        let exec = ExecCommandTool::new(manager.clone());
        let stdin = WriteStdinTool::new(manager);
        let (progress, mut chunks) = mpsc::unbounded_channel();
        let result = exec
            .execute(
                "foreground".into(),
                json!({
                    "cmd": "printf '%s %s' \"$AI_AGENT\" \"$PIKRA_TEST_VAR\"; exit 7",
                    "cwd": "sub",
                }),
                CancelToken::new(),
                Arc::new(move |chunk| progress.send(chunk).unwrap()),
            )
            .await
            .unwrap();
        assert_eq!(result.details["status"], "exited");
        assert_eq!(result.details["exit_code"], 7);
        assert_eq!(result.details["cwd"], json!(dir.path().join("sub")));
        assert_eq!(text(&result), "pikra provided");
        assert_eq!(text(&chunks.recv().await.unwrap()), "pikra provided");
        assert!(events.try_recv().is_err());

        let result = call(&exec, json!({"command": "printf done", "background": true}))
            .await
            .unwrap();
        let id = result.details["session_id"].as_u64().unwrap() as u32;
        assert_eq!(result.details["status"], "running");
        assert_eq!(
            events.recv().await.unwrap(),
            BackgroundToolEvent::ShellExited {
                session_id: id,
                command: "printf done".into(),
                exit_code: Some(0),
            },
        );
        let end = call(&stdin, json!({"session_id": id})).await.unwrap();
        assert_eq!(end.details["exit_code"], 0);
        assert_eq!(format!("{}{}", text(&result), text(&end)), "done");
    }

    #[tokio::test]
    async fn terminal_yields_accepts_input_and_kills() {
        let dir = TempDir::new().unwrap();
        let (callback, mut events) = events();
        let manager = ProcessSessionManager::with_max_sessions(
            Arc::new(WorkspaceToolsConfig::new(dir.path())),
            WakeGate::new(),
            Some(callback),
            1,
        );
        let exec = ExecCommandTool::new(manager.clone());
        let stdin = WriteStdinTool::new(manager.clone());

        let start = call(&exec, json!({
            "command": "test -t 0 && test -t 1 && test -t 2 || exit 8; stty -echo; printf 'prompt\\n'; read -r line; printf 'got:%s\\n' \"$line\"; read -r line",
            "tty": true,
            "yield_time_ms": 0,
        })).await.unwrap();
        let id = &start.details["session_id"];
        assert!(id.is_u64());
        await_output(&stdin, id, text(&start).into(), "prompt").await;
        assert!(
            call(&exec, json!({"command": "true", "background": true}))
                .await
                .is_err()
        );
        let short = call(&exec, json!({"command": "printf available"}))
            .await
            .unwrap();
        assert_eq!(text(&short), "available");
        let reply = call(&stdin, json!({"session_id": id, "chars": "hello\n"}))
            .await
            .unwrap();
        await_output(&stdin, id, text(&reply).into(), "got:hello").await;
        let stop = call(&stdin, json!({"session_id": id, "kill": true}))
            .await
            .unwrap();
        assert_eq!(stop.details["status"], "exited");
        let timeout = call(&exec, json!({"command": "read -r unused", "timeout_ms": 1}))
            .await
            .unwrap();
        assert_eq!(timeout.details["status"], "exited");
        assert_eq!(timeout.details["timed_out"], true);
        assert!(events.try_recv().is_err());
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_and_drop_close_a_descendants_pipe() {
        let dir = TempDir::new().unwrap();
        fifo(&dir.path().join("lifetime"));
        fifo(&dir.path().join("hold"));
        let workspace = Arc::new(WorkspaceToolsConfig::new(dir.path()));
        let gate = WakeGate::new();
        let command = "(exec 3>lifetime; printf x >&3; exec cat hold) & wait";

        let connect = || async {
            let mut pipe = tokio::fs::File::open(dir.path().join("lifetime"))
                .await
                .unwrap();
            assert_eq!(pipe.read_u8().await.unwrap(), b'x');
            pipe
        };

        let exec = ExecCommandTool::new(ProcessSessionManager::new(
            workspace.clone(),
            gate.clone(),
            None,
        ));
        call(&exec, json!({"command": command, "background": true}))
            .await
            .unwrap();
        let mut pipe = connect().await;
        drop(exec);
        assert_eq!(pipe.read(&mut [0]).await.unwrap(), 0);

        for shutdown in [true, false] {
            let (callback, mut events) = events();
            let manager = MonitorManager::new(workspace.clone(), gate.clone(), Some(callback));
            let monitor = MonitorTool::new(manager.clone());
            call(
                &monitor,
                json!({"action": "start", "name": "cleanup", "command": command}),
            )
            .await
            .unwrap();
            let mut pipe = connect().await;
            if shutdown {
                manager.shutdown().await;
            }
            drop(monitor);
            drop(manager);
            assert_eq!(pipe.read(&mut [0]).await.unwrap(), 0);
            assert!(events.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn monitor_batches_stdout_wakes_sleep_and_stops_silently() {
        let dir = TempDir::new().unwrap();
        fifo(&dir.path().join("hold"));
        let gate = WakeGate::new();
        let (callback, mut events) = events();
        let manager = MonitorManager::with_config(
            Arc::new(WorkspaceToolsConfig::new(dir.path())),
            gate.clone(),
            Some(callback),
            MonitorConfig {
                max_active: 1,
                monitor_log_dir: Some("logs".into()),
                ..MonitorConfig::default()
            },
        );
        let tool = MonitorTool::new(manager.clone());
        let [start, stop, list] = MonitorTool::split(manager.clone());
        let sleep = SleepTool::new(gate);
        let (ready, waiting) = oneshot::channel();
        let ready = std::sync::Mutex::new(Some(ready));
        let wait = sleep.execute(
            "sleep".into(),
            json!({"duration_ms": 43_200_000, "reason": "monitor"}),
            CancelToken::new(),
            Arc::new(move |_| ready.lock().unwrap().take().unwrap().send(()).unwrap()),
        );
        let start_monitor = async {
            waiting.await.unwrap();
            call(
                &start,
                json!({
                    "name": "line & monitor",
                    "command": "printf 'one\\ntwo\\n'; printf 'hidden\\n' >&2; exec cat hold",
                }),
            )
            .await
            .unwrap()
        };
        let (wake, monitor) = tokio::join!(wait, start_monitor);
        assert_eq!(
            wake.unwrap().details["wake_reason"],
            "New output from line & monitor."
        );
        let id = &monitor.details["id"];
        let event = events.recv().await.unwrap();
        let BackgroundToolEvent::MonitorLines {
            lines, log_path, ..
        } = &event
        else {
            panic!("expected monitor lines");
        };
        assert_eq!(lines, "one\ntwo");
        assert!(
            format_background_events_xml(std::slice::from_ref(&event))
                .contains("name=\"line &amp; monitor\"")
        );
        assert_eq!(
            call(&list, json!({})).await.unwrap().details["monitors"][0]["id"],
            *id
        );
        assert!(
            call(
                &tool,
                json!({"action": "start", "name": "extra", "command": "true"})
            )
            .await
            .is_err()
        );
        call(&stop, json!({"id": id})).await.unwrap();
        assert!(events.try_recv().is_err());
        assert_eq!(
            call(&tool, json!({"action": "list"}))
                .await
                .unwrap()
                .details["monitors"],
            json!([])
        );
        assert!(log_path.starts_with(dir.path().join("logs")));
        assert!(fs::read_to_string(log_path).unwrap().contains("hidden"));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn monitor_reports_exit_flood_and_timeout() {
        let dir = TempDir::new().unwrap();
        fifo(&dir.path().join("hold"));
        let (callback, mut events) = events();
        let tool = MonitorTool::new(MonitorManager::new(
            Arc::new(WorkspaceToolsConfig::new(dir.path())),
            WakeGate::new(),
            Some(callback),
        ));
        let output =
            "i=1; while [ \"$i\" -le 201 ]; do printf 'line%s\\n' \"$i\"; i=$((i+1)); done";
        for (notify, command, expected) in [
            (
                "exit",
                format!("{output}; printf 'stderr\\n' >&2; exit 7"),
                MonitorStopReason::Exited,
            ),
            (
                "lines",
                format!("{output}; exec cat hold"),
                MonitorStopReason::Flood,
            ),
            ("exit", "exec cat hold".into(), MonitorStopReason::Timeout),
        ] {
            let mut args =
                json!({"action": "start", "name": notify, "command": command, "notify": notify});
            if expected == MonitorStopReason::Timeout {
                args["timeout_ms"] = json!(1);
            }
            let started = call(&tool, args).await.unwrap();
            let event = loop {
                match events.recv().await.unwrap() {
                    BackgroundToolEvent::MonitorLines { .. }
                        if expected == MonitorStopReason::Flood => {}
                    event => break event,
                }
            };
            let BackgroundToolEvent::MonitorExited {
                id,
                exit_code,
                reason,
                tail_lines,
                log_path,
                ..
            } = &event
            else {
                panic!("expected monitor exit");
            };
            assert_eq!(*reason, expected);
            assert_eq!(started.details["id"], *id);
            assert_eq!(
                call(&tool, json!({"action": "list"}))
                    .await
                    .unwrap()
                    .details["monitors"],
                json!([])
            );
            if expected == MonitorStopReason::Exited {
                assert_eq!(*exit_code, Some(7));
                assert_eq!(tail_lines.lines().count(), 50);
                assert!(tail_lines.contains("line201"));
                let log = fs::read_to_string(log_path).unwrap();
                assert!(log.contains("line1\n") && log.contains("stderr\n"));
            } else {
                assert_eq!(*exit_code, None);
            }
            if expected == MonitorStopReason::Flood {
                assert!(format_background_events_xml(&[event]).contains("grep --line-buffered"));
            }
        }
    }
}
