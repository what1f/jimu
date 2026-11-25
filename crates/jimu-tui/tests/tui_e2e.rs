#![cfg(not(target_os = "ios"))]

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use futures_channel::mpsc;
use futures_executor::LocalPool;
use futures_util::task::LocalSpawnExt;
use jimu_tui::{
    app::{ScreenMode, TuiApp, TuiAppConfig, TuiInput},
    engine::theme::ThemeKind,
    harness::{
        BoxEventStream, BoxFuture, ChromeConfig, ContextMeterCap, ContextMeterSnapshot,
        HarnessCapabilities, InteractionRequest, ModelOption, ModelPickerCap, PlanDecision,
        QuestionOption, QuestionPrompt, SubagentStatus, SubagentUpdate, ToolOutput, TuiEvent,
        TuiHarness, UsageStats, UserInput,
    },
};
use ratatui::{
    backend::{Backend, ClearType, TestBackend, WindowSize},
    buffer::{Buffer, Cell},
    layout::{Position, Size},
};
use serde_json::json;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::{
    io::{self, Write},
    ops::Range,
};

fn buffer_text(buf: &Buffer) -> String {
    buf.content()
        .chunks(usize::from(buf.area.width.max(1)))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct StandaloneHarness {
    tx: Mutex<Option<mpsc::UnboundedSender<TuiEvent>>>,
    submitted: Mutex<Vec<String>>,
    selected_model: Mutex<String>,
}

impl StandaloneHarness {
    fn new() -> Self {
        Self {
            tx: Mutex::new(None),
            submitted: Mutex::new(Vec::new()),
            selected_model: Mutex::new("custom-1".into()),
        }
    }

    fn emit(&self, ev: TuiEvent) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.unbounded_send(ev);
        }
    }
}

impl ContextMeterCap for StandaloneHarness {
    fn context_usage(&self) -> Option<ContextMeterSnapshot> {
        Some(ContextMeterSnapshot {
            tokens: Some(32_000),
            estimated_tokens: 32_000,
            context_window: 100_000,
            percent: Some(32.0),
            compaction_threshold: Some(80_000),
        })
    }
}

impl ModelPickerCap for StandaloneHarness {
    fn current_model(&self) -> ModelOption {
        let id = self.selected_model.lock().unwrap().clone();
        ModelOption {
            id: id.clone(),
            name: id,
            provider: "custom".into(),
            supports_thinking: true,
        }
    }

    fn available_models(&self) -> Vec<ModelOption> {
        vec![
            ModelOption {
                id: "custom-1".into(),
                name: "custom-1".into(),
                provider: "custom".into(),
                supports_thinking: true,
            },
            ModelOption {
                id: "custom-2".into(),
                name: "custom-2".into(),
                provider: "custom".into(),
                supports_thinking: false,
            },
        ]
    }

    fn set_model(&self, model_id: &str) -> BoxFuture<'_, Result<(), String>> {
        let model_id = model_id.to_owned();
        Box::pin(async move {
            *self.selected_model.lock().unwrap() = model_id;
            Ok(())
        })
    }
}

impl TuiHarness for StandaloneHarness {
    fn submit(&self, input: UserInput) -> BoxFuture<'_, Result<(), String>> {
        let text = input.text;
        Box::pin(async move {
            self.submitted.lock().unwrap().push(text.clone());
            self.emit(TuiEvent::TurnStarted);
            self.emit(TuiEvent::UserMessage {
                id: None,
                text,
                steered: false,
            });
            self.emit(TuiEvent::ThinkingDelta {
                block_index: 0,
                delta: "planning table and edit".into(),
            });
            self.emit(TuiEvent::TextDelta {
                block_index: 1,
                delta: "| Col | Val |\n| --- | --- |\n| ok | 42 |\n\n```rust\nlet x = 1;\n```\n"
                    .into(),
            });
            self.emit(TuiEvent::AssistantMessageEnd { error: None });
            self.emit(TuiEvent::ToolStart {
                call_id: "c1".into(),
                name: "edit".into(),
                effective_name: None,
                args: json!({"path": "demo.rs", "oldText": "a = 1", "newText": "a = 2"}),
            });
            self.emit(TuiEvent::ToolEnd {
                call_id: "c1".into(),
                name: "edit".into(),
                effective_name: None,
                output: ToolOutput::text("edited"),
            });
            self.emit(TuiEvent::ToolStart {
                call_id: "c2".into(),
                name: "exec_command".into(),
                effective_name: None,
                args: json!({"command": "echo hi"}),
            });
            self.emit(TuiEvent::ToolEnd {
                call_id: "c2".into(),
                name: "exec_command".into(),
                effective_name: None,
                output: ToolOutput {
                    text: "\x1b[32mPASS\x1b[0m\n".into(),
                    structured: Some(json!({"output": "\x1b[32mPASS\x1b[0m\n", "exit_code": 0})),
                    ..ToolOutput::default()
                },
            });
            self.emit(TuiEvent::SubagentEvent(SubagentUpdate {
                id: "sub-1".into(),
                label: "reviewer".into(),
                task: "audit diff".into(),
                status: SubagentStatus::Completed {
                    summary: Some("clean".into()),
                },
                detail: None,
            }));
            self.emit(TuiEvent::UsageUpdate(UsageStats {
                input_tokens: 100,
                output_tokens: 40,
                total_tokens: 140,
                ..UsageStats::default()
            }));
            self.emit(TuiEvent::ContextUsageUpdate(ContextMeterSnapshot {
                tokens: Some(32_000),
                estimated_tokens: 32_000,
                context_window: 100_000,
                percent: Some(32.0),
                compaction_threshold: Some(80_000),
            }));
            Ok(())
        })
    }

    fn subscribe(&self) -> BoxEventStream {
        let (tx, rx) = mpsc::unbounded();
        *self.tx.lock().unwrap() = Some(tx);
        Box::pin(rx)
    }

    fn abort(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn capabilities(&self) -> HarnessCapabilities<'_> {
        HarnessCapabilities {
            context_meter: Some(self),
            model_picker: Some(self),
            ..HarnessCapabilities::default()
        }
    }
}

static UI_TEST: Mutex<()> = Mutex::new(());

struct Frames {
    terminal: TestBackend,
    paint: Rc<RefCell<Buffer>>,
}

impl Backend for Frames {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.terminal.draw(content)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.terminal.append_lines(n)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.terminal.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.terminal.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.terminal.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, p: P) -> io::Result<()> {
        self.terminal.set_cursor_position(p)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.terminal.clear()
    }
    fn clear_region(&mut self, kind: ClearType) -> io::Result<()> {
        self.terminal.clear_region(kind)
    }
    fn size(&self) -> io::Result<Size> {
        self.terminal.size()
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.terminal.window_size()
    }
    fn scroll_region_up(&mut self, region: Range<u16>, n: u16) -> io::Result<()> {
        self.terminal.scroll_region_up(region, n)
    }
    fn scroll_region_down(&mut self, region: Range<u16>, n: u16) -> io::Result<()> {
        self.terminal.scroll_region_down(region, n)
    }

    fn flush(&mut self) -> io::Result<()> {
        *self.paint.borrow_mut() = self.terminal.buffer().clone();
        Ok(())
    }
}

#[derive(Clone, Default)]
struct StatusBytes(Rc<RefCell<Vec<u8>>>);

impl Write for StatusBytes {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct TestUi {
    pool: LocalPool,
    tx: mpsc::UnboundedSender<TuiInput>,
    frame: Rc<RefCell<Buffer>>,
    wire: StatusBytes,
}

impl TestUi {
    fn start(app: TuiApp) -> Self {
        let (tx, rx) = mpsc::unbounded();
        let wire = StatusBytes::default();
        let terminal = TestBackend::new(108, 41);
        let frame = Rc::new(RefCell::new(terminal.buffer().clone()));
        let mut ui = Self {
            pool: LocalPool::new(),
            tx,
            frame,
            wire,
        };
        let backend = Frames {
            terminal,
            paint: ui.frame.clone(),
        };
        let writer = ui.wire.clone();
        ui.pool
            .spawner()
            .spawn_local(async move {
                app.run_on_with_status(backend, rx, writer).await.unwrap();
            })
            .unwrap();
        ui.settle();
        ui
    }

    fn settle(&mut self) {
        self.pool.run_until_stalled();
    }

    fn send(&mut self, event: Event) {
        self.tx.unbounded_send(event.into()).unwrap();
        self.settle();
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.send(Event::Key(KeyEvent::new(code, modifiers)));
    }

    fn enter(&mut self, text: &str) {
        self.send(Event::Paste(text.into()));
        self.key(KeyCode::Enter, KeyModifiers::NONE);
    }

    fn screen(&self) -> String {
        buffer_text(&self.frame.borrow())
    }

    fn find(&self, text: &str) -> Position {
        use unicode_width::UnicodeWidthStr;
        let screen = self.screen();
        for (row, line) in screen.lines().enumerate() {
            if let Some(index) = line.find(text) {
                return Position::new(line[..index].width() as u16, row as u16);
            }
        }
        panic!(
            "{text:?} missing from frame:
{screen}"
        );
    }

    fn mouse(&mut self, pos: Position, kind: MouseEventKind) {
        self.send(Event::Mouse(MouseEvent {
            kind,
            column: pos.x,
            row: pos.y,
            modifiers: KeyModifiers::NONE,
        }));
    }

    fn click(&mut self, pos: Position) {
        self.mouse(pos, MouseEventKind::Moved);
        self.mouse(pos, MouseEventKind::Down(MouseButton::Left));
        self.mouse(pos, MouseEventKind::Up(MouseButton::Left));
    }

    fn status(&self) -> Vec<String> {
        let bytes = self.wire.0.borrow();
        let wire = String::from_utf8_lossy(&bytes).replace("\x1b\x1b", "\x1b");
        wire.split("\x1b]7501;")
            .skip(1)
            .filter_map(|sequence| {
                sequence
                    .split_once("\x1b\\")
                    .map(|(status, _)| status.to_owned())
            })
            .collect()
    }

    fn stop(&mut self) {
        self.tx.close_channel();
        self.pool.run();
    }
}

#[test]
fn standalone_harness_fullscreen_mouse_dual_screen_and_program_status_e2e() {
    let _terminal = UI_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let harness = Arc::new(StandaloneHarness::new());
    let copied = Arc::new(Mutex::new(Vec::new()));
    let clipboard = copied.clone();
    let cfg = TuiAppConfig {
        theme_kind: ThemeKind::Dark,
        directory: None,
        chrome: ChromeConfig {
            app_name: Some("Test Terminal".into()),
            app_version: Some("v-test".into()),
            footer_hint: Some("Test welcome footer".into()),
            ..Default::default()
        },
        clipboard: Some(Arc::new(move |text| {
            clipboard.lock().unwrap().push(text.to_owned());
            Ok(())
        })),
        ..Default::default()
    };
    assert_eq!(cfg.screen_mode, ScreenMode::Fullscreen);
    let app = TuiApp::new(harness.clone(), cfg);
    let controller = app.controller();
    let mut ui = TestUi::start(app);
    assert!(ui.screen().contains("Test Terminal"));
    assert!(ui.screen().contains("v-test"));
    assert!(ui.screen().contains("Test welcome footer"));
    assert!(ui.screen().contains("╭"));
    let theme = ui.find("Switch theme");
    let canvas = || Position::new(0, 0);
    let dark = ui.frame.borrow()[canvas()].bg;
    ui.mouse(theme, MouseEventKind::Moved);
    ui.click(theme);
    assert_eq!(
        jimu_tui::engine::theme::Theme::current_kind(),
        ThemeKind::Light
    );
    if dark != ratatui::style::Color::Reset {
        assert_ne!(ui.frame.borrow()[canvas()].bg, dark);
    }
    ui.enter("/theme dark");
    assert_eq!(
        jimu_tui::engine::theme::Theme::current_kind(),
        ThemeKind::Dark
    );
    assert_eq!(ui.frame.borrow()[canvas()].bg, dark);

    ui.send(Event::Paste("render dmo".into()));
    let mut pos = ui.find("dmo");
    pos.x += 1;
    ui.click(pos);
    ui.key(KeyCode::Char('e'), KeyModifiers::NONE);
    ui.key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(
        harness.submitted.lock().unwrap().as_slice(),
        ["render demo"]
    );
    let response = ui.screen();
    assert!(
        response.contains("demo.rs") && response.contains("32K / 100K"),
        "{response}"
    );
    let tool = ui.find("Run echo hi");
    ui.mouse(tool, MouseEventKind::Moved);
    ui.click(ui.find("↗"));
    assert!(ui.screen().contains("PASS"), "{}", ui.screen());
    ui.key(KeyCode::Esc, KeyModifiers::NONE);

    let start = ui.find("render demo");
    let end = Position::new(start.x + 11, start.y);
    ui.mouse(start, MouseEventKind::Down(MouseButton::Left));
    ui.mouse(end, MouseEventKind::Drag(MouseButton::Left));
    ui.mouse(end, MouseEventKind::Up(MouseButton::Left));
    let selections = copied.lock().unwrap();
    assert_eq!(selections.as_slice(), ["render demo"]);
    drop(selections);
    assert!(ui.screen().contains("Copied"));

    let first_cell = ui.find("Col");
    let mut last_cell = ui.find("42");
    last_cell.x += 1;
    ui.mouse(first_cell, MouseEventKind::Down(MouseButton::Left));
    ui.mouse(last_cell, MouseEventKind::Drag(MouseButton::Left));
    ui.mouse(last_cell, MouseEventKind::Up(MouseButton::Left));
    let table = copied.lock().unwrap().last().cloned().unwrap();
    assert!(
        ["Col", "Val", "ok", "42"]
            .into_iter()
            .all(|cell| table.contains(cell)),
        "{table:?}"
    );

    let (reply, mut result) = futures_channel::oneshot::channel();
    controller.send_interaction(InteractionRequest::Select {
        title: "Select destination".into(),
        options: vec!["First target".into(), "Second target".into()],
        reply,
    });
    ui.settle();
    assert!(
        ui.status()
            .iter()
            .any(|status| status.starts_with("state=blocked:kind=question:app=TestTerminal"))
    );
    ui.click(ui.find("Second target"));
    assert_eq!(result.try_recv().unwrap(), Some(Some(1)));

    let (reply, mut result) = futures_channel::oneshot::channel();
    controller.send_interaction(InteractionRequest::AskQuestion {
        prompt: QuestionPrompt {
            title: "Pick target".into(),
            question: "Which mode?".into(),
            options: vec![QuestionOption {
                label: "Local".into(),
                description: None,
            }],
            multi_select: false,
            allow_freeform: false,
        },
        reply,
    });
    ui.settle();
    ui.click(ui.find("Local"));
    ui.click(ui.find("Submit"));
    assert_eq!(
        result
            .try_recv()
            .unwrap()
            .flatten()
            .unwrap()
            .selected_indices,
        [0]
    );

    let (reply, mut result) = futures_channel::oneshot::channel();
    controller.send_interaction(InteractionRequest::ApprovePlan {
        title: "Migration Plan".into(),
        markdown: "# Steps\n- Migrate engine".into(),
        reply,
    });
    ui.settle();
    ui.click(ui.find("Approve plan"));
    assert_eq!(result.try_recv().unwrap(), Some(PlanDecision::Approve));

    harness.emit(TuiEvent::TurnEnded);
    ui.settle();
    ui.enter("/copy");
    assert!(
        copied
            .lock()
            .unwrap()
            .last()
            .is_some_and(|text| text.contains("Col") && text.contains("42")),
        "expected /copy to copy the last assistant response"
    );
    ui.send(Event::Paste("draft to clear".into()));
    assert!(ui.screen().contains("draft to clear"));
    ui.key(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(!ui.screen().contains("draft to clear"));
    ui.key(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(ui.screen().contains("Press Ctrl+C again to exit"));
    harness.emit(TuiEvent::TurnStarted);
    harness.emit(TuiEvent::AssistantMessageEnd {
        error: Some("Test turn failed".into()),
    });
    harness.emit(TuiEvent::TurnEnded);
    ui.settle();
    ui.stop();

    let statuses = ui.status();
    let states = statuses
        .iter()
        .map(|status| status.split(':').next().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(states.first(), Some(&"state=idle"));
    assert!(
        states
            .windows(3)
            .any(|states| states == ["state=working", "state=blocked", "state=working"]),
        "{statuses:?}"
    );
    assert_eq!(
        &states[states.len() - 4..],
        ["state=done", "state=working", "state=error", "state=clear"]
    );
    assert!(
        statuses
            .iter()
            .any(|status| status == "state=error:app=TestTerminal:msg=VGVzdCB0dXJuIGZhaWxlZA==")
    );
    assert!(
        statuses
            .iter()
            .any(|status| status.contains("state=working:app=TestTerminal:msg=ZXhlY19jb21tYW5k")),
        "{statuses:?}"
    );

    let mut silent = TestUi::start(TuiApp::new(
        Arc::new(StandaloneHarness::new()),
        TuiAppConfig {
            chrome: ChromeConfig {
                osc7501_enabled: false,
                ..Default::default()
            },
            labels: jimu_tui::app::Labels([("Switch theme".into(), "Cambiar tema".into())].into()),
            ..Default::default()
        },
    ));
    assert!(silent.screen().contains("Cambiar tema"));
    silent.stop();
    assert!(silent.status().is_empty());
}

#[cfg(feature = "jimu")]
#[test]
fn jimu_native_harness_approval_context_queue_and_wakeup_e2e() {
    let _terminal = UI_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use jimu_agent::{AgentToolResult, ClosureTool};
    use jimu_ai::{
        ApiKind, AssistantBlock, AssistantMessage, AssistantMessageEvent as Ev, Model, StopReason,
        TextContent, ToolCall, ToolDefinition, Usage,
    };
    use jimu_kit::{BackgroundNotification, KitSession, KitSessionConfig, KitToolSpec};
    use jimu_testkit::FauxProvider;
    use jimu_tui::jimu::{JimuApprovalMiddleware, JimuHarness, JimuInteractionSink};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let executions = Arc::new(AtomicUsize::new(0));
    let exec_counter = executions.clone();
    let write_tool = Arc::new(ClosureTool::new(
        ToolDefinition {
            name: "write".into(),
            description: "write file".into(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        },
        move |_id, _args, _cancel, _update| {
            let counter = exec_counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(AgentToolResult::text("written ok"))
            })
        },
    ));

    let mk_done = |blocks: Vec<AssistantBlock>, stop: StopReason| Ev::Done {
        reason: stop,
        message: AssistantMessage {
            content: blocks,
            api: ApiKind::OpenaiResponses,
            provider: "mock".into(),
            model: "mock-model".into(),
            response_id: None,
            thinking_level: None,
            usage: Usage {
                input: 400,
                output: 50,
                cache_read: 100,
                cache_write: 0,
                reasoning: None,
                total_tokens: 550,
                cost_total: 0.0,
                ..Usage::default()
            },
            stop_reason: stop,
            error_message: None,
            diagnostics: Vec::new(),
            duration_ms: None,
            timestamp: 1,
        },
    };

    let mut args = serde_json::Map::new();
    args.insert("path".into(), json!("secret.txt"));
    let call = ToolCall {
        id: "call-1".into(),
        name: "write".into(),
        arguments: args.clone(),
        item_id: None,
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    };
    let call2 = ToolCall {
        id: "call-2".into(),
        ..call.clone()
    };

    let provider = Arc::new(FauxProvider::new());
    for turn in [
        // Turn 1: requests write (will be denied), then acknowledges denial.
        vec![mk_done(
            vec![AssistantBlock::ToolCall(call)],
            StopReason::ToolUse,
        )],
        vec![
            Ev::TextDelta {
                content_index: 0,
                delta: "denied noted".into(),
            },
            mk_done(
                vec![AssistantBlock::Text(TextContent::plain("denied noted"))],
                StopReason::Stop,
            ),
        ],
        // Turn 2: requests write (will be allowed), then confirms.
        vec![mk_done(
            vec![AssistantBlock::ToolCall(call2)],
            StopReason::ToolUse,
        )],
        vec![
            Ev::TextDelta {
                content_index: 0,
                delta: "saved!".into(),
            },
            mk_done(
                vec![AssistantBlock::Text(TextContent::plain("saved!"))],
                StopReason::Stop,
            ),
        ],
        // Turn 3 (background wakeup): responds to monitor event.
        vec![
            Ev::TextDelta {
                content_index: 0,
                delta: "background handled".into(),
            },
            mk_done(
                vec![AssistantBlock::Text(TextContent::plain(
                    "background handled",
                ))],
                StopReason::Stop,
            ),
        ],
        // Turn 4 (branched after double-Esc rewind): responds on alternate branch.
        vec![
            Ev::TextDelta {
                content_index: 0,
                delta: "alt branch reply".into(),
            },
            mk_done(
                vec![AssistantBlock::Text(TextContent::plain("alt branch reply"))],
                StopReason::Stop,
            ),
        ],
    ] {
        provider.enqueue_events(turn);
    }

    let mut model = Model::new("mock-model", "mock", ApiKind::OpenaiResponses, "");
    model.context_window = 10_000;

    let sink = JimuInteractionSink::new();
    let approval = JimuApprovalMiddleware::new(sink.clone(), |ctx| ctx.tool_call.name == "write");

    let mut kit_cfg = KitSessionConfig::new(model);
    kit_cfg.skill_discovery.include_user_skills = false;
    kit_cfg.tools.push(KitToolSpec::resident(write_tool));
    kit_cfg.middlewares.push(Arc::new(approval));

    let session = Arc::new(KitSession::new(provider, kit_cfg).unwrap());
    // Append a hidden meta message to prove it never leaks into the TUI transcript.
    session.append_meta("TOP_SECRET_SYSTEM_REMINDER");
    let harness = Arc::new(JimuHarness::with_interaction_sink(session.clone(), sink));
    let app = TuiApp::new(
        harness,
        TuiAppConfig {
            theme_kind: ThemeKind::Dark,
            directory: None,
            ..TuiAppConfig::default()
        },
    );
    let mut ui = TestUi::start(app);

    ui.enter("write once");
    assert!(ui.screen().contains("Tool approval"));
    assert!(ui.screen().contains("secret.txt"));
    assert!(
        ui.status()
            .iter()
            .any(|status| status.starts_with("state=blocked:kind=permission"))
    );
    ui.click(ui.find("Deny"));
    assert_eq!(executions.load(Ordering::SeqCst), 0);

    ui.enter("write again");
    ui.click(ui.find("Allow"));
    assert_eq!(executions.load(Ordering::SeqCst), 1);

    session.background_sender().send(BackgroundNotification {
        source_id: "mon-1".into(),
        kind: "monitor".into(),
        summary: "build green".into(),
        xml_payload: "<monitor_event>build green</monitor_event>".into(),
        timestamp_ms: 2,
    });
    ui.settle();
    let screen = ui.screen();
    assert!(!screen.contains("TOP_SECRET_SYSTEM_REMINDER"));
    assert!(screen.contains("denied noted"), "{screen}");
    assert!(screen.contains("saved!"), "{screen}");
    assert!(screen.contains("background handled"), "{screen}");

    // Double-Esc opens the Rewind panel; selecting "write again" rewinds before it,
    // refills the composer with "write again", and submitting creates a second branch.
    ui.key(KeyCode::Esc, KeyModifiers::NONE);
    ui.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(
        ui.screen().contains("Rewind conversation"),
        "{}",
        ui.screen()
    );
    ui.click(ui.find("[you] write again"));
    assert!(ui.screen().contains("write again"), "{}", ui.screen());
    assert!(!ui.screen().contains("saved!"), "{}", ui.screen());
    ui.send(Event::Paste(" v2".into()));
    ui.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(ui.screen().contains("alt branch reply"), "{}", ui.screen());

    // Now the session has two branches: /tree renders the indented tree view.
    ui.enter("/tree");
    let tree_screen = ui.screen();
    assert!(
        tree_screen.contains("Session tree")
            && (tree_screen.contains("├─") || tree_screen.contains("└─")),
        "{tree_screen}"
    );
    ui.key(KeyCode::Esc, KeyModifiers::NONE);

    let context = session.context_usage().expect("native context usage");
    let used = context.tokens.unwrap_or(context.estimated_tokens);
    let format = jimu_tui::ui::views::context_bar::fmt_tokens;
    assert!(
        ui.screen().contains(&format!(
            "{} / {}",
            format(used),
            format(u64::from(context.context_window))
        )),
        "{}",
        ui.screen()
    );
    ui.stop();
    assert_eq!(ui.status().last().map(String::as_str), Some("state=clear"));
}

#[cfg(feature = "jimu")]
#[test]
fn slash_paths_skills_and_highlight_e2e() {
    let _terminal = UI_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    struct TempDirGuard(std::path::PathBuf);
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let ws_path = std::env::temp_dir().join(format!("jimu-tui-slash-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ws_path);
    let _ws_guard = TempDirGuard(ws_path.clone());
    let skill_dir = ws_path.join(".agents/skills/review");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: review\ndescription: Review a target file\n---\nReview target: $ARGUMENTS\n",
    )
    .unwrap();

    let submitted = Arc::new(Mutex::new(Vec::<String>::new()));
    let submitted_clone = Arc::clone(&submitted);

    struct CaptureProvider(Arc<Mutex<Vec<String>>>);
    impl jimu_ai::StreamProvider for CaptureProvider {
        fn stream(
            &self,
            model: &jimu_ai::Model,
            context: &jimu_ai::TranscriptContext,
            _options: jimu_ai::SimpleStreamOptions,
        ) -> jimu_ai::AssistantEventStream {
            let last_user = context
                .messages
                .iter()
                .rev()
                .find_map(|m| match m {
                    jimu_ai::Message::User(u) => Some(
                        u.content
                            .iter()
                            .filter_map(|p| match p {
                                jimu_ai::ContentPart::Text(t) => Some(t.text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    _ => None,
                })
                .unwrap_or_default();
            self.0.lock().unwrap().push(last_user);
            let msg = jimu_ai::AssistantMessage {
                content: vec![jimu_ai::AssistantBlock::Text(jimu_ai::TextContent::plain(
                    "ack",
                ))],
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_id: None,
                thinking_level: None,
                usage: jimu_ai::Usage::default(),
                stop_reason: jimu_ai::StopReason::Stop,
                error_message: None,
                diagnostics: Vec::new(),
                timestamp: 1,
                duration_ms: None,
            };
            Box::pin(futures_util::stream::iter([
                jimu_ai::AssistantMessageEvent::Start {
                    partial: msg.clone(),
                },
                jimu_ai::AssistantMessageEvent::Done {
                    reason: jimu_ai::StopReason::Stop,
                    message: msg,
                },
            ]))
        }
    }

    let model_a = jimu_ai::Model::new(
        "alpha-pro",
        "magpie",
        jimu_ai::ApiKind::OpenaiCompletions,
        "http://127.0.0.1:1/v1",
    );
    let model_b = jimu_ai::Model::new(
        "beta-coder",
        "magpie",
        jimu_ai::ApiKind::OpenaiCompletions,
        "http://127.0.0.1:1/v1",
    );
    let mut kit = jimu_kit::KitSessionConfig::new(model_a.clone());
    kit.cwd = Some(ws_path.to_string_lossy().into_owned());
    kit.skill_discovery = jimu_kit::SkillDiscoveryConfig {
        cwd: Some(ws_path.clone()),
        include_user_skills: false,
        ..Default::default()
    };
    let session = Arc::new(
        jimu_kit::KitSession::new(Arc::new(CaptureProvider(submitted_clone)), kit).unwrap(),
    );
    let saved_model = Arc::new(Mutex::new(None::<String>));
    let saved_effort = Arc::new(Mutex::new(None::<jimu_ai::ThinkingLevel>));
    let saved_model_cb = Arc::clone(&saved_model);
    let saved_effort_cb = Arc::clone(&saved_effort);
    let harness = Arc::new(
        jimu_tui::jimu::JimuHarness::new(Arc::clone(&session))
            .with_models(vec![model_a, model_b])
            .with_on_model_change(Arc::new(move |m| {
                *saved_model_cb.lock().unwrap() = Some(format!("{}/{}", m.provider, m.id));
            }))
            .with_on_thinking_change(Arc::new(move |lvl| {
                *saved_effort_cb.lock().unwrap() = Some(lvl);
            })),
    );
    let app = TuiApp::new(
        harness,
        TuiAppConfig {
            theme_kind: ThemeKind::Dark,
            directory: Some(ws_path.clone()),
            ..TuiAppConfig::default()
        },
    );
    let mut ui = TestUi::start(app);

    // 1. Leading space + pasted absolute path passes through (never Unknown command).
    ui.enter(" /Users/example/demo.png");
    // 2. Column-0 pasted absolute path and existing `/tmp` pass through too.
    ui.enter("/Users/example/demo.png");
    ui.enter("/tmp");
    assert!(
        !ui.screen().contains("Unknown command"),
        "paths should never trigger Unknown command: {}",
        ui.screen()
    );

    // 3. Genuine typo triggers Unknown command with edit-distance suggestion.
    ui.enter("/compcat");
    let screen = ui.screen();
    assert!(
        screen.contains("Unknown command") && screen.contains("Did you mean /compact?"),
        "expected typo suggestion: {screen}"
    );

    // 4. Typing `/review` highlights the slash token in the composer with `accent_skill`.
    for ch in "/review".chars() {
        ui.key(KeyCode::Char(ch), KeyModifiers::NONE);
    }
    let pos = ui.find("/review");
    let accent = jimu_tui::engine::theme::Theme::dark().accent_skill;
    assert_eq!(
        ui.frame.borrow()[pos].fg,
        accent,
        "expected /review token to be highlighted with accent_skill"
    );

    // 5. Submitting `/review src/main.rs` expands SKILL.md with `$ARGUMENTS` for the model while
    // keeping the transcript bubble concise.
    ui.send(Event::Paste(" src/main.rs".into()));
    ui.key(KeyCode::Enter, KeyModifiers::NONE);

    let sent = submitted.lock().unwrap();
    assert_eq!(sent.len(), 4, "sent={sent:?}");
    assert_eq!(sent[0], " /Users/example/demo.png");
    assert_eq!(sent[1], "/Users/example/demo.png");
    assert_eq!(sent[2], "/tmp");
    assert!(
        sent[3].contains("<skill name=\"review\"")
            && sent[3].contains("Review target: src/main.rs"),
        "expected expanded skill body: {}",
        sent[3]
    );
    assert!(
        !ui.screen().contains("Review target: src/main.rs"),
        "expanded SKILL.md body should not flood the user bubble"
    );

    // 6. Inline argument autocomplete works for `/model ` and `/effort `.
    for ch in "/model beta".chars() {
        ui.key(KeyCode::Char(ch), KeyModifiers::NONE);
    }
    assert!(
        ui.screen().contains("magpie/beta-coder"),
        "expected inline model completion: {}",
        ui.screen()
    );
    ui.key(KeyCode::Char('c'), KeyModifiers::CONTROL);
    for ch in "/effort hi".chars() {
        ui.key(KeyCode::Char(ch), KeyModifiers::NONE);
    }
    assert!(
        ui.screen().contains("high"),
        "expected inline effort completion: {}",
        ui.screen()
    );
    ui.key(KeyCode::Char('c'), KeyModifiers::CONTROL);

    // 7. Combined Model + Effort modal: Down selects beta-coder, Right steps effort from off -> medium,
    // Enter applies both and fires persistence callbacks.
    ui.enter("/model");
    let modal_screen = ui.screen();
    assert!(
        modal_screen.contains("alpha-pro")
            && modal_screen.contains("beta-coder")
            && modal_screen.contains("Effort")
            && modal_screen.contains("medium"),
        "expected combined model + effort slider modal: {modal_screen}"
    );
    ui.key(KeyCode::Down, KeyModifiers::NONE);
    ui.key(KeyCode::Right, KeyModifiers::NONE);
    ui.key(KeyCode::Right, KeyModifiers::NONE);
    ui.key(KeyCode::Enter, KeyModifiers::NONE);

    assert_eq!(session.model().id, "beta-coder");
    assert_eq!(session.thinking_level(), jimu_ai::ThinkingLevel::Medium);
    assert_eq!(
        saved_model.lock().unwrap().as_deref(),
        Some("magpie/beta-coder")
    );
    assert_eq!(
        *saved_effort.lock().unwrap(),
        Some(jimu_ai::ThinkingLevel::Medium)
    );
    ui.stop();
}

#[cfg(feature = "jimu")]
#[test]
fn branched_session_tree_stays_compact_and_focuses_active_leaf_e2e() {
    use jimu_ai::{
        ApiKind, AssistantBlock, AssistantMessage, AssistantMessageEvent as Ev, Model, StopReason,
        TextContent, Usage,
    };
    use jimu_kit::{KitSession, KitSessionConfig};
    use jimu_testkit::FauxProvider;
    use jimu_tui::{
        app::{ScreenMode, TuiApp, TuiAppConfig},
        engine::theme::ThemeKind,
        harness::{RewindCap, TuiHarness},
        jimu::JimuHarness,
    };
    use std::sync::Arc;

    let provider = Arc::new(FauxProvider::new());
    let mk_reply = |text: &str| {
        let msg = AssistantMessage {
            content: vec![AssistantBlock::Text(TextContent::plain(text))],
            api: ApiKind::OpenaiResponses,
            provider: "mock".into(),
            model: "mock-model".into(),
            response_id: None,
            thinking_level: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            diagnostics: Vec::new(),
            duration_ms: None,
            timestamp: 1,
        };
        vec![
            Ev::TextDelta {
                content_index: 0,
                delta: text.into(),
            },
            Ev::Done {
                reason: StopReason::Stop,
                message: msg,
            },
        ]
    };
    for reply in [
        "reply-1",
        "reply-2",
        "branch-old-reply",
        "branch-new-1",
        "branch-new-2",
    ] {
        provider.enqueue_events(mk_reply(reply));
    }

    let model = Model::new("mock-model", "mock", ApiKind::OpenaiResponses, "");
    let mut cfg = KitSessionConfig::new(model);
    cfg.skill_discovery.include_user_skills = false;
    let session = Arc::new(KitSession::new(provider, cfg).expect("session"));
    let harness = Arc::new(JimuHarness::new(Arc::clone(&session)));

    let app = TuiApp::new(
        harness.clone(),
        TuiAppConfig {
            screen_mode: ScreenMode::Fullscreen,
            theme_kind: ThemeKind::Dark,
            directory: None,
            ..Default::default()
        },
    );
    let mut ui = TestUi::start(app);

    // Build 3 linear turns, then rewind to turn 3 ("branch-old") and create a 2-turn active branch.
    ui.enter("turn-1");
    ui.enter("turn-2");
    ui.enter("branch-old");
    assert!(ui.screen().contains("branch-old-reply"), "{}", ui.screen());

    // Double-Esc opens linear rewind; click "branch-old" to fork from reply-2.
    ui.key(KeyCode::Esc, KeyModifiers::NONE);
    ui.key(KeyCode::Esc, KeyModifiers::NONE);
    assert!(
        ui.screen().contains("Rewind conversation"),
        "{}",
        ui.screen()
    );
    ui.click(ui.find("[you] branch-old"));
    // Replace composer draft with new active branch turns.
    ui.key(KeyCode::Char('c'), KeyModifiers::CONTROL);
    ui.enter("branch-new-turn1");
    assert!(ui.screen().contains("branch-new-1"), "{}", ui.screen());
    ui.enter("branch-new-turn2");
    assert!(ui.screen().contains("branch-new-2"), "{}", ui.screen());

    // Verify compact tree structure: shared trunk stays at column 0 (no staircase drift),
    // active branch sorts first with `├─ ` and `│  ` aligned, sibling branch uses `└─ ` and `   `.
    let points = harness.list_rewind_points();
    let projected: Vec<(&str, &str, bool, bool)> = points
        .iter()
        .map(|p| {
            (
                p.tree_prefix.as_str(),
                p.label.as_str(),
                p.is_active_branch,
                p.is_current_leaf,
            )
        })
        .collect();
    assert_eq!(
        projected,
        vec![
            ("", "turn-1", true, false),
            ("", "reply-1", true, false),
            ("", "turn-2", true, false),
            ("", "reply-2", true, false),
            ("├─ ", "branch-new-turn1", true, false),
            ("│  ", "branch-new-1", true, false),
            ("│  ", "branch-new-turn2", true, false),
            ("│  ", "branch-new-2", true, true),
            ("└─ ", "branch-old", false, false),
            ("   ", "branch-old-reply", false, false),
        ]
    );

    // Open `/tree` and verify visual layout + default selection lands on the current leaf.
    ui.enter("/tree");
    let tree_screen = ui.screen();
    assert!(
        tree_screen.contains("Session tree")
            && tree_screen.contains("├─ • [you] branch-new-turn1")
            && tree_screen.contains("│  ● [ai] branch-new-2")
            && tree_screen.contains("└─   [you] branch-old"),
        "{tree_screen}"
    );
    // Pressing Enter immediately keeps the current leaf (`branch-new-2`) active.
    let leaf_before = session.leaf_id();
    ui.key(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(session.leaf_id(), leaf_before);
    assert!(harness.capabilities().rewind.is_some());
    ui.stop();
}

#[cfg(feature = "jimu")]
#[test]
fn new_session_while_turn_running_resets_tui_to_idle_e2e() {
    use jimu_ai::{
        ApiKind, AssistantBlock, AssistantEventStream, AssistantMessage,
        AssistantMessageEvent as Ev, Model, SimpleStreamOptions, StopReason, StreamProvider,
        TextContent, TranscriptContext, Usage,
    };
    use jimu_kit::{KitSession, KitSessionConfig};
    use jimu_tui::{
        app::{ScreenMode, TuiApp, TuiAppConfig},
        engine::theme::ThemeKind,
        harness::{BoxFuture, SessionOption},
        jimu::{JimuHarness, JimuSessionController},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct BlockingFirstProvider {
        calls: AtomicUsize,
    }

    impl StreamProvider for BlockingFirstProvider {
        fn stream(
            &self,
            _model: &Model,
            _context: &TranscriptContext,
            options: SimpleStreamOptions,
        ) -> AssistantEventStream {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let cancel = options.cancel;
            if call == 0 {
                let s = futures_util::stream::unfold(Some(cancel), |state| async move {
                    let cancel = state?;
                    cancel.cancelled().await;
                    let msg = AssistantMessage {
                        content: Vec::new(),
                        api: ApiKind::OpenaiResponses,
                        provider: "mock".into(),
                        model: "mock-model".into(),
                        response_id: None,
                        thinking_level: None,
                        usage: Usage::default(),
                        stop_reason: StopReason::Aborted,
                        error_message: Some("aborted".into()),
                        diagnostics: Vec::new(),
                        duration_ms: None,
                        timestamp: 1,
                    };
                    Some((
                        Ev::Error {
                            reason: StopReason::Aborted,
                            error: msg,
                        },
                        None,
                    ))
                });
                Box::pin(s)
            } else {
                let msg = AssistantMessage {
                    content: vec![AssistantBlock::Text(TextContent::plain("fresh-reply"))],
                    api: ApiKind::OpenaiResponses,
                    provider: "mock".into(),
                    model: "mock-model".into(),
                    response_id: None,
                    thinking_level: None,
                    usage: Usage::default(),
                    stop_reason: StopReason::Stop,
                    error_message: None,
                    diagnostics: Vec::new(),
                    duration_ms: None,
                    timestamp: 2,
                };
                Box::pin(futures_util::stream::iter(vec![
                    Ev::TextDelta {
                        content_index: 0,
                        delta: "fresh-reply".into(),
                    },
                    Ev::Done {
                        reason: StopReason::Stop,
                        message: msg,
                    },
                ]))
            }
        }
    }

    struct SwapController {
        provider: Arc<BlockingFirstProvider>,
        current_id: Mutex<String>,
    }

    impl JimuSessionController for SwapController {
        fn current_session_id(&self) -> Option<String> {
            Some(self.current_id.lock().unwrap().clone())
        }

        fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionOption>, String>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn open_session(&self, _id: &str) -> BoxFuture<'_, Result<Arc<KitSession>, String>> {
            Box::pin(async { Err("unused".into()) })
        }

        fn create_session(&self) -> BoxFuture<'_, Result<Arc<KitSession>, String>> {
            let provider = Arc::clone(&self.provider);
            Box::pin(async move {
                let model = Model::new("mock-model", "mock", ApiKind::OpenaiResponses, "");
                let mut cfg = KitSessionConfig::new(model);
                cfg.skill_discovery.include_user_skills = false;
                let next = Arc::new(KitSession::new(provider, cfg).map_err(|e| e.to_string())?);
                *self.current_id.lock().unwrap() = next.session_id();
                Ok(next)
            })
        }
    }

    let provider = Arc::new(BlockingFirstProvider {
        calls: AtomicUsize::new(0),
    });
    let model = Model::new("mock-model", "mock", ApiKind::OpenaiResponses, "");
    let mut cfg = KitSessionConfig::new(model);
    cfg.skill_discovery.include_user_skills = false;
    let session = Arc::new(KitSession::new(provider.clone(), cfg).expect("session"));
    let controller = Arc::new(SwapController {
        provider: Arc::clone(&provider),
        current_id: Mutex::new(session.session_id()),
    });
    let harness = Arc::new(JimuHarness::new(session).with_session_controller(controller));

    let app = TuiApp::new(
        harness,
        TuiAppConfig {
            screen_mode: ScreenMode::Fullscreen,
            theme_kind: ThemeKind::Dark,
            directory: None,
            ..Default::default()
        },
    );
    let mut ui = TestUi::start(app);

    // Submit Turn 1 — stays in flight (Working) waiting on BlockingFirstProvider.
    ui.enter("old-turn");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);

    // Run `/new` while Turn 1 is running: must abort old turn, replace session, and reset to idle.
    ui.enter("/new");

    // First input on the new session must immediately start a foreground turn (NOT queue as `#1`).
    ui.enter("fresh-turn");
    let screen = ui.screen();
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "new session's first input must fire immediately as foreground turn; screen:\n{screen}"
    );
    assert!(
        screen.contains("fresh-reply"),
        "expected fresh-reply on screen:\n{screen}"
    );
    ui.stop();
}
