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
        response.contains("demo.rs") && response.contains("32K/100K"),
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
            "{}/{}",
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

#[cfg(feature = "clipboard-image")]
#[test]
fn image_path_paste_creates_chip_and_preserves_placeholder_in_transcript_e2e() {
    let _terminal = UI_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    struct ImageCaptureHarness {
        tx: Mutex<Option<mpsc::UnboundedSender<TuiEvent>>>,
        received: Mutex<Vec<UserInput>>,
    }

    impl TuiHarness for ImageCaptureHarness {
        fn submit(&self, input: UserInput) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async move {
                let text = input.text.clone();
                self.received.lock().unwrap().push(input);
                if let Some(tx) = self.tx.lock().unwrap().as_ref() {
                    let _ = tx.unbounded_send(TuiEvent::TurnStarted);
                    let _ = tx.unbounded_send(TuiEvent::UserMessage {
                        id: None,
                        text,
                        steered: false,
                    });
                    let _ = tx.unbounded_send(TuiEvent::TextDelta {
                        block_index: 0,
                        delta: "got image".into(),
                    });
                    let _ = tx.unbounded_send(TuiEvent::AssistantMessageEnd { error: None });
                    let _ = tx.unbounded_send(TuiEvent::TurnEnded);
                }
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
    }

    let tmp = std::env::temp_dir().join(format!("jimu-tui-img-paste-{} dir", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let png_path = tmp.join("sample shot.png");
    let img = image::RgbaImage::from_pixel(16, 16, image::Rgba([40, 120, 220, 255]));
    img.save(&png_path).unwrap();

    let harness = Arc::new(ImageCaptureHarness {
        tx: Mutex::new(None),
        received: Mutex::new(Vec::new()),
    });
    let app = TuiApp::new(
        harness.clone(),
        TuiAppConfig {
            screen_mode: ScreenMode::Fullscreen,
            theme_kind: ThemeKind::Dark,
            directory: Some(tmp.clone()),
            ..Default::default()
        },
    );
    let mut ui = TestUi::start(app);

    // 1. Pasting a quoted local image path inserts an atomic `[Image #1]` chip into the prompt.
    let quoted = format!("'{}'", png_path.display());
    ui.send(Event::Paste(quoted));
    assert!(
        ui.screen().contains("Image #1"),
        "prompt must show [Image #1] chip after path paste:\n{}",
        ui.screen()
    );

    // 2. Append user text and submit: model receives clean text + image bytes, while transcript keeps `[Image #1]`.
    ui.send(Event::Paste("inspect button".into()));
    ui.key(KeyCode::Enter, KeyModifiers::NONE);
    {
        let received = harness.received.lock().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].text.trim(), "inspect button");
        assert!(!received[0].text.contains("[Image #1]"));
        assert_eq!(received[0].images.len(), 1);
        assert_eq!(received[0].images[0].mime_type, "image/png");
    }
    let screen = ui.screen();
    assert!(
        screen.contains("[Image #1]") && screen.contains("inspect button"),
        "transcript bubble must keep [Image #1] alongside user text:\n{screen}"
    );

    // 3. Image-only submission via `file://` URI keeps `[Image #1]` in the transcript (no blank bubble).
    let file_uri = url::Url::from_file_path(&png_path).unwrap().to_string();
    ui.send(Event::Paste(file_uri));
    ui.key(KeyCode::Enter, KeyModifiers::NONE);
    {
        let received = harness.received.lock().unwrap();
        assert_eq!(received.len(), 2);
        assert!(received[1].text.trim().is_empty());
        assert_eq!(received[1].images.len(), 1);
    }
    assert!(
        ui.screen().contains("[Image #1]"),
        "image-only message must still display [Image #1] in transcript:\n{}",
        ui.screen()
    );

    ui.stop();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn default_chrome_hides_header_and_supports_custom_status_and_above_prompt_e2e() {
    use jimu_tui::harness::{
        ChromeSlots, ChromeState, PermissionModeCap, PermissionModeOption, SessionOption,
        SessionPickerCap,
    };
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};

    let _terminal = UI_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    struct StatusHarness {
        tx: Mutex<Option<mpsc::UnboundedSender<TuiEvent>>>,
        effort: Mutex<String>,
        perm: Mutex<String>,
    }

    impl ContextMeterCap for StatusHarness {
        fn context_usage(&self) -> Option<ContextMeterSnapshot> {
            Some(ContextMeterSnapshot {
                tokens: Some(23_000),
                estimated_tokens: 23_000,
                context_window: 300_000,
                percent: Some(7.6),
                compaction_threshold: Some(240_000),
            })
        }
    }

    impl ModelPickerCap for StatusHarness {
        fn current_model(&self) -> ModelOption {
            ModelOption {
                id: "opus-4".into(),
                name: "opus-4".into(),
                provider: "mock".into(),
                supports_thinking: true,
            }
        }
        fn available_models(&self) -> Vec<ModelOption> {
            vec![self.current_model()]
        }
        fn set_model(&self, _: &str) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }
        fn thinking_levels(&self) -> Vec<String> {
            vec!["low".into(), "high".into()]
        }
        fn current_thinking_level(&self) -> Option<String> {
            Some(self.effort.lock().unwrap().clone())
        }
        fn set_thinking_level(&self, level: &str) -> BoxFuture<'_, Result<(), String>> {
            let level = level.to_owned();
            Box::pin(async move {
                *self.effort.lock().unwrap() = level;
                Ok(())
            })
        }
        fn fast_mode(&self) -> bool {
            true
        }
    }

    impl PermissionModeCap for StatusHarness {
        fn current_mode(&self) -> String {
            self.perm.lock().unwrap().clone()
        }
        fn available_modes(&self) -> Vec<PermissionModeOption> {
            vec![PermissionModeOption {
                id: "bypassPermissions".into(),
                description: "Bypass".into(),
            }]
        }
        fn set_mode(&self, mode: &str) -> Result<(), String> {
            *self.perm.lock().unwrap() = mode.to_owned();
            Ok(())
        }
    }

    impl SessionPickerCap for StatusHarness {
        fn current_session_id(&self) -> Option<String> {
            Some("sess-42".into())
        }
        fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionOption>, String>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn switch_session(&self, _: &str) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }
        fn new_session(&self) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }
    }

    impl TuiHarness for StatusHarness {
        fn submit(&self, input: UserInput) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async move {
                if let Some(tx) = self.tx.lock().unwrap().as_ref() {
                    let _ = tx.unbounded_send(TuiEvent::TurnStarted);
                    let _ = tx.unbounded_send(TuiEvent::UserMessage {
                        id: None,
                        text: input.text,
                        steered: false,
                    });
                    let _ = tx.unbounded_send(TuiEvent::TextDelta {
                        block_index: 0,
                        delta: "ack".into(),
                    });
                    let _ = tx.unbounded_send(TuiEvent::AssistantMessageEnd { error: None });
                    let _ = tx.unbounded_send(TuiEvent::TurnEnded);
                }
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
                permission_mode: Some(self),
                session_picker: Some(self),
                ..Default::default()
            }
        }
    }

    // Part 1: default ChromeConfig hides top header and renders restrained bottom status line.
    let harness = Arc::new(StatusHarness {
        tx: Mutex::new(None),
        effort: Mutex::new("high".into()),
        perm: Mutex::new("bypassPermissions".into()),
    });
    assert!(!ChromeConfig::default().show_header);
    let app = TuiApp::new(
        harness.clone(),
        TuiAppConfig {
            screen_mode: ScreenMode::Fullscreen,
            theme_kind: ThemeKind::Dark,
            directory: Some(std::path::PathBuf::from("/tmp/demo-project")),
            chrome: ChromeConfig {
                app_name: Some("HiddenTitle".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let mut ui = TestUi::start(app);
    ui.enter("hello");
    ui.enter("/effort low");
    let screen = ui.screen();
    let lines: Vec<&str> = screen.lines().collect();
    let top_row = lines.first().copied().unwrap_or_default();
    assert!(
        !top_row.contains("HiddenTitle") && !top_row.contains("Theme"),
        "default show_header=false must not paint legacy top bar:\n{screen}"
    );
    assert!(
        !screen.contains("Effort: low"),
        "switching effort must not clutter transcript with Effort notice:\n{screen}"
    );
    let bottom_row = lines.last().copied().unwrap_or_default();
    assert!(
        bottom_row.contains("opus-4 low")
            && bottom_row.contains("fast")
            && bottom_row.contains("demo-project")
            && bottom_row.contains("23K/300K (8%)")
            && bottom_row.contains("bypassPermissions"),
        "default bottom status bar missing expected items: {bottom_row:?}\nfull:\n{screen}"
    );
    assert!(
        !bottom_row.contains('·'),
        "status bar must never join items with middle dot `·`: {bottom_row:?}"
    );
    ui.stop();

    // Part 2: custom ChromeSlots overrides `status_line`, adds `above_prompt`, and inspects extended `ChromeState`.
    struct CustomSlots;
    impl ChromeSlots for CustomSlots {
        fn above_prompt(&self, state: &ChromeState<'_>) -> Vec<Line<'static>> {
            vec![Line::from(format!(
                "ABOVE_BAR sess={} perm={}",
                state.session_id.unwrap_or("-"),
                state.permission_mode.unwrap_or("-")
            ))]
        }
        fn status_line(&self, state: &ChromeState<'_>) -> Option<Vec<Line<'static>>> {
            Some(vec![Line::from(Span::styled(
                format!(
                    "CUSTOM_STATUS effort={} fast={}",
                    state.effort.unwrap_or("-"),
                    state.fast_mode
                ),
                Style::default().fg(state.theme.text_primary),
            ))])
        }
    }

    let app2 = TuiApp::new(
        harness,
        TuiAppConfig {
            screen_mode: ScreenMode::Fullscreen,
            theme_kind: ThemeKind::Dark,
            directory: Some(std::path::PathBuf::from("/tmp/demo-project")),
            chrome: ChromeConfig {
                slots: Arc::new(CustomSlots),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let mut ui2 = TestUi::start(app2);
    ui2.enter("hi");
    let screen2 = ui2.screen();
    assert!(
        screen2.contains("ABOVE_BAR sess=sess-42 perm=bypassPermissions"),
        "above_prompt slot did not render:\n{screen2}"
    );
    assert!(
        screen2.contains("CUSTOM_STATUS effort=low fast=true"),
        "custom status_line did not override default:\n{screen2}"
    );
    assert!(
        !screen2.contains("23K/300K (8%)"),
        "default status_line should be replaced when custom returns Some:\n{screen2}"
    );
    ui2.stop();
}

#[test]
fn theme_overrides_and_component_decoration_hooks_e2e() {
    use jimu_tui::{
        engine::theme::{Theme, ThemeOverrides, cache as theme_cache},
        harness::{ChromeSlots, SurfaceDecoration, ToolRowView},
    };
    use ratatui::{
        style::{Color, Style},
        text::{Line, Span},
    };
    use std::collections::BTreeMap;

    let _terminal = UI_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    struct ModSlots;
    impl ChromeSlots for ModSlots {
        fn decorate_user_message(
            &self,
            _text: &str,
            _image_count: usize,
            _width: u16,
            _theme: &Theme,
        ) -> Option<SurfaceDecoration> {
            Some(SurfaceDecoration {
                prefix_lines: vec![Line::from(Span::styled(
                    "┌─ USER_MOD_TOP",
                    Style::default().fg(Color::Cyan),
                ))],
                suffix_lines: vec![Line::from("└─ USER_MOD_BOT")],
                left_border: Some(Color::Cyan),
                replace: None,
            })
        }

        fn decorate_tool_row(
            &self,
            row: &ToolRowView<'_>,
            _theme: &Theme,
        ) -> Option<SurfaceDecoration> {
            Some(SurfaceDecoration {
                prefix_lines: vec![Line::from(format!("TOOL_DECO {}", row.name))],
                suffix_lines: Vec::new(),
                left_border: Some(Color::Magenta),
                replace: None,
            })
        }
    }

    // 1. Verify `Theme::with_overrides` parses `#rrggbb`, `#rgb`, and ANSI color names accurately.
    let mut colors = BTreeMap::new();
    colors.insert("bg_base".into(), "#102030".into());
    colors.insert("accent_primary".into(), "#f0a".into());
    colors.insert("accent_error".into(), "bright_red".into());
    let custom_ov = ThemeOverrides {
        base: Some("dark".into()),
        colors,
    };
    let derived = Theme::dark().with_overrides(&custom_ov);
    assert_eq!(
        derived.accent_error,
        jimu_tui::engine::theme::quantize(Color::LightRed)
    );
    assert_eq!(
        derived.bg_base,
        jimu_tui::engine::theme::quantize(Color::Rgb(0x10, 0x20, 0x30))
    );
    assert_eq!(
        derived.accent_user,
        jimu_tui::engine::theme::quantize(Color::Rgb(0xff, 0x00, 0xaa))
    );

    // 2. Inject custom theme + component decoration hooks into live TuiApp and test `/theme <custom>` + `SetThemeOverrides`.
    let mut custom_themes = BTreeMap::new();
    custom_themes.insert("oceanic".into(), custom_ov);
    let harness = Arc::new(StandaloneHarness::new());
    let app = TuiApp::new(
        harness,
        TuiAppConfig {
            screen_mode: ScreenMode::Fullscreen,
            theme_kind: ThemeKind::Dark,
            custom_themes,
            chrome: ChromeConfig {
                slots: Arc::new(ModSlots),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let ctrl = app.controller();
    let mut ui = TestUi::start(app);

    ui.enter("/theme oceanic");
    assert_eq!(
        Theme::current().bg_base,
        jimu_tui::engine::theme::quantize(Color::Rgb(0x10, 0x20, 0x30))
    );

    // Live override via TuiController
    let mut live_colors = BTreeMap::new();
    live_colors.insert("accent_success".into(), "cyan".into());
    ctrl.set_theme_overrides(Some(ThemeOverrides {
        base: Some("dark".into()),
        colors: live_colors,
    }));
    ui.enter("decorated prompt");
    assert_eq!(
        Theme::current().accent_success,
        jimu_tui::engine::theme::quantize(Color::Cyan)
    );

    let screen = ui.screen();
    assert!(
        screen.contains("┌─ USER_MOD_TOP")
            && screen.contains("decorated prompt")
            && screen.contains("└─ USER_MOD_BOT"),
        "user decoration must wrap without losing native text:\n{screen}"
    );
    assert!(
        screen.contains("TOOL_DECO edit") && screen.contains("demo.rs"),
        "tool decoration must wrap without losing native tool row:\n{screen}"
    );

    // Clean up global overrides so other tests stay isolated.
    ctrl.set_theme_overrides(None);
    ui.stop();
    theme_cache::set_overrides(None);
}

#[test]
fn review_regressions_theme_chrome_tool_name_and_images_e2e() {
    use jimu_tui::{
        engine::theme::{Theme, ThemeOverrides, cache as theme_cache, parse_color},
        harness::{
            ChromeSlots, ChromeState, SurfaceDecoration, ToolRowView, render_default_status_line,
        },
    };
    use ratatui::{style::Color, text::Line};
    use std::collections::BTreeMap;

    let _terminal = UI_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // 1. Non-ASCII hex color specs must never panic on char boundary slicing.
    assert_eq!(parse_color("#中"), None);
    assert_eq!(parse_color("#中文"), None);
    assert_eq!(parse_color("#12g"), None);
    assert_eq!(parse_color("#f0a"), Some(Color::Rgb(0xff, 0x00, 0xaa)));

    // 2. `base: auto` preserves the resolved light/dark base theme; `base: terminal` keeps Reset
    // backgrounds while applying explicit color overrides.
    let auto_ov = ThemeOverrides {
        base: Some("auto".into()),
        colors: BTreeMap::from([("accent_user".into(), "#112233".into())]),
    };
    let light_auto = Theme::light().with_overrides(&auto_ov);
    assert_eq!(
        light_auto.bg_base,
        jimu_tui::engine::theme::quantize(Theme::light().bg_base)
    );
    assert_eq!(
        light_auto.accent_user,
        jimu_tui::engine::theme::quantize(Color::Rgb(0x11, 0x22, 0x33))
    );

    let term_ov = ThemeOverrides {
        base: Some("terminal".into()),
        colors: BTreeMap::from([("accent_user".into(), "#ff5500".into())]),
    };
    theme_cache::set(ThemeKind::Terminal);
    theme_cache::set_overrides(Some(term_ov));
    let term_current = Theme::current();
    assert_eq!(term_current.bg_base, Color::Reset);
    assert_eq!(
        term_current.accent_user,
        jimu_tui::engine::theme::quantize(Color::Rgb(0xff, 0x55, 0x00))
    );
    theme_cache::set_overrides(None);
    theme_cache::set(ThemeKind::Dark);

    // 3. Theme layering, ReplaceCustomThemes, header deduplication, default status line helper,
    // and real `ToolRowView.name` (`apply_patch` / `write` instead of `edit`).
    struct ToolAndHeaderHarness {
        tx: Mutex<Option<mpsc::UnboundedSender<TuiEvent>>>,
    }
    impl TuiHarness for ToolAndHeaderHarness {
        fn submit(&self, input: UserInput) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async move {
                if let Some(tx) = self.tx.lock().unwrap().as_ref() {
                    let _ = tx.unbounded_send(TuiEvent::TurnStarted);
                    let _ = tx.unbounded_send(TuiEvent::UserMessage {
                        id: None,
                        text: input.text,
                        steered: false,
                    });
                    let _ = tx.unbounded_send(TuiEvent::ToolStart {
                        call_id: "ap1".into(),
                        name: "apply_patch".into(),
                        effective_name: None,
                        args: json!({"patch": "*** Update File: src/lib.rs\n@@ -1 +1 @@\n-old\n+new"}),
                    });
                    let _ = tx.unbounded_send(TuiEvent::ToolEnd {
                        call_id: "ap1".into(),
                        name: "apply_patch".into(),
                        effective_name: None,
                        output: ToolOutput {
                            text: "patched".into(),
                            ..Default::default()
                        },
                    });
                    let _ = tx.unbounded_send(TuiEvent::ToolStart {
                        call_id: "wr1".into(),
                        name: "write".into(),
                        effective_name: None,
                        args: json!({"path": "new.rs", "content": "fn main() {}"}),
                    });
                    let _ = tx.unbounded_send(TuiEvent::ToolEnd {
                        call_id: "wr1".into(),
                        name: "write".into(),
                        effective_name: None,
                        output: ToolOutput {
                            text: "written".into(),
                            ..Default::default()
                        },
                    });
                    let _ = tx.unbounded_send(TuiEvent::TurnEnded);
                }
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
    }

    struct InspectSlots;
    impl ChromeSlots for InspectSlots {
        fn header(&self, _state: &ChromeState<'_>) -> Vec<Line<'static>> {
            vec![Line::from("CUSTOM_HDR_ONLY")]
        }
        fn status_line(&self, state: &ChromeState<'_>) -> Option<Vec<Line<'static>>> {
            Some(vec![
                render_default_status_line(state),
                Line::from("TAG_OK"),
            ])
        }
        fn decorate_tool_row(
            &self,
            row: &ToolRowView<'_>,
            _theme: &Theme,
        ) -> Option<SurfaceDecoration> {
            Some(SurfaceDecoration {
                prefix_lines: vec![Line::from(format!("REAL_TOOL={}", row.name))],
                ..Default::default()
            })
        }
    }

    let mut custom_themes = BTreeMap::new();
    custom_themes.insert(
        "ocean".into(),
        ThemeOverrides {
            base: Some("dark".into()),
            colors: BTreeMap::from([
                ("bg_base".into(), "#081828".into()),
                ("accent_user".into(), "#104060".into()),
            ]),
        },
    );
    let harness = Arc::new(ToolAndHeaderHarness {
        tx: Mutex::new(None),
    });
    let app = TuiApp::new(
        harness,
        TuiAppConfig {
            screen_mode: ScreenMode::Fullscreen,
            theme_kind: ThemeKind::Dark,
            directory: Some(std::path::PathBuf::from("/tmp/status-dir")),
            custom_themes,
            chrome: ChromeConfig {
                show_header: true,
                app_name: Some("BUILTIN_HDR_HIDDEN".into()),
                slots: Arc::new(InspectSlots),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let ctrl = app.controller();
    let mut ui = TestUi::start(app);

    // Select custom theme, apply overlay, then switch built-in theme: overlay colors survive.
    ui.enter("/theme ocean");
    ctrl.set_theme_overrides(Some(ThemeOverrides {
        base: None,
        colors: BTreeMap::from([("accent_user".into(), "#ff00ff".into())]),
    }));
    ctrl.request_redraw();
    ui.enter("run tools");
    assert_eq!(
        Theme::current().bg_base,
        jimu_tui::engine::theme::quantize(Color::Rgb(0x08, 0x18, 0x28))
    );
    assert_eq!(
        Theme::current().accent_user,
        jimu_tui::engine::theme::quantize(Color::Rgb(0xff, 0x00, 0xff))
    );

    // ReplaceCustomThemes updates active custom theme; removing it falls back cleanly to default.
    ctrl.replace_custom_themes(BTreeMap::from([(
        "ocean".into(),
        ThemeOverrides {
            base: Some("dark".into()),
            colors: BTreeMap::from([("bg_base".into(), "#122436".into())]),
        },
    )]));
    ui.key(KeyCode::Null, KeyModifiers::NONE);
    assert_eq!(
        Theme::current().bg_base,
        jimu_tui::engine::theme::quantize(Color::Rgb(0x12, 0x24, 0x36))
    );
    assert_eq!(
        Theme::current().accent_user,
        jimu_tui::engine::theme::quantize(Color::Rgb(0xff, 0x00, 0xff))
    );

    ctrl.replace_custom_themes(BTreeMap::new());
    ui.key(KeyCode::Null, KeyModifiers::NONE);
    assert_eq!(
        Theme::current().bg_base,
        jimu_tui::engine::theme::quantize(Theme::dark().bg_base)
    );
    assert_eq!(
        Theme::current().accent_user,
        jimu_tui::engine::theme::quantize(Color::Rgb(0xff, 0x00, 0xff))
    );

    // Switching built-in `/theme light` keeps overlay colors intact.
    ui.enter("/theme light");
    assert_eq!(
        Theme::current().bg_base,
        jimu_tui::engine::theme::quantize(Theme::light().bg_base)
    );
    assert_eq!(
        Theme::current().accent_user,
        jimu_tui::engine::theme::quantize(Color::Rgb(0xff, 0x00, 0xff))
    );

    ctrl.invalidate_history_layout();
    ui.key(KeyCode::Null, KeyModifiers::NONE);
    let screen = ui.screen();
    assert!(
        screen.contains("CUSTOM_HDR_ONLY") && !screen.contains("BUILTIN_HDR_HIDDEN"),
        "custom header must suppress duplicate built-in header:\n{screen}"
    );
    assert!(
        screen.contains("status-dir") && screen.contains("TAG_OK"),
        "render_default_status_line must preserve native dir alongside custom tag:\n{screen}"
    );
    assert!(
        screen.contains("REAL_TOOL=apply_patch") && screen.contains("REAL_TOOL=write"),
        "ToolRowView.name must expose real tool names rather than 'edit':\n{screen}"
    );

    ctrl.set_theme_overrides(None);
    ui.stop();
    theme_cache::set_overrides(None);
    theme_cache::set(ThemeKind::Dark);
}

#[cfg(feature = "clipboard-image")]
#[test]
fn image_retry_relative_cwd_multiline_dedup_and_batch_cap_e2e() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let _terminal = UI_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    struct RetryAndEchoHarness {
        tx: Mutex<Option<mpsc::UnboundedSender<TuiEvent>>>,
        fail_next: AtomicBool,
        received: Mutex<Vec<UserInput>>,
    }

    impl TuiHarness for RetryAndEchoHarness {
        fn submit(&self, input: UserInput) -> BoxFuture<'_, Result<(), String>> {
            Box::pin(async move {
                if self.fail_next.swap(false, Ordering::SeqCst) {
                    return Err("transient network error".into());
                }
                let echo = if input.images.is_empty() {
                    input.text.clone()
                } else {
                    let chips = (1..=input.images.len())
                        .map(|n| format!("[Image #{n}]"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if input.text.trim().is_empty() {
                        chips
                    } else {
                        format!("{chips} {}", input.text.trim())
                    }
                };
                self.received.lock().unwrap().push(input);
                if let Some(tx) = self.tx.lock().unwrap().as_ref() {
                    let _ = tx.unbounded_send(TuiEvent::TurnStarted);
                    let _ = tx.unbounded_send(TuiEvent::UserMessage {
                        id: None,
                        text: echo,
                        steered: false,
                    });
                    let _ = tx.unbounded_send(TuiEvent::TextDelta {
                        block_index: 0,
                        delta: "ok".into(),
                    });
                    let _ = tx.unbounded_send(TuiEvent::AssistantMessageEnd { error: None });
                    let _ = tx.unbounded_send(TuiEvent::TurnEnded);
                }
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
    }

    let tmp = std::env::temp_dir().join(format!("jimu-tui-img-reg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let img = image::RgbaImage::from_pixel(16, 16, image::Rgba([20, 180, 90, 255]));
    for i in 1..=12 {
        img.save(tmp.join(format!("p{i}.png"))).unwrap();
    }
    std::fs::write(tmp.join("corrupt.png"), b"not a valid png").unwrap();

    let harness = Arc::new(RetryAndEchoHarness {
        tx: Mutex::new(None),
        fail_next: AtomicBool::new(true),
        received: Mutex::new(Vec::new()),
    });
    let app = TuiApp::new(
        harness.clone(),
        TuiAppConfig {
            screen_mode: ScreenMode::Fullscreen,
            theme_kind: ThemeKind::Dark,
            directory: Some(tmp.clone()),
            ..Default::default()
        },
    );
    let mut ui = TestUi::start(app);

    // 1. Relative path `./p1.png` resolves against TUI `directory` (`-C`), fails once, and retries
    // without leaking `[Image #1]` into the text sent to the model.
    ui.send(Event::Paste("./p1.png".into()));
    ui.send(Event::Paste("retry body".into()));
    ui.key(KeyCode::Enter, KeyModifiers::NONE);
    assert!(harness.received.lock().unwrap().is_empty());
    // Retry immediately by pressing Enter on the restored draft.
    ui.key(KeyCode::Enter, KeyModifiers::NONE);
    {
        let rec = harness.received.lock().unwrap();
        assert_eq!(rec.len(), 1);
        assert_eq!(rec[0].text, "retry body");
        assert!(!rec[0].text.contains("[Image #1]"));
        assert_eq!(rec[0].images.len(), 1);
    }

    // 2. Multiline message with trailing image deduplicates cleanly (only 1 bubble, not 2).
    ui.send(Event::Paste("line_alpha\nline_beta\n".into()));
    ui.send(Event::Paste("./p2.png".into()));
    ui.key(KeyCode::Enter, KeyModifiers::NONE);
    let screen = ui.screen();
    assert_eq!(
        screen.matches("line_alpha").count(),
        1,
        "multiline + trailing image must not create duplicate bubbles:\n{screen}"
    );

    // 3. Batch with a corrupt image still inserts the valid images around it, and exceeding
    // IMAGE_CAP truncates to remaining capacity with a clear toast.
    ui.send(Event::Paste("./p1.png ./corrupt.png ./p2.png".into()));
    ui.key(KeyCode::Enter, KeyModifiers::NONE);
    {
        let rec = harness.received.lock().unwrap();
        assert_eq!(rec.len(), 3);
        assert_eq!(
            rec[2].images.len(),
            2,
            "corrupt file must not drop valid siblings"
        );
    }

    let twelve = (1..=12)
        .map(|i| format!("./p{i}.png"))
        .collect::<Vec<_>>()
        .join(" ");
    ui.send(Event::Paste(twelve));
    assert!(
        ui.screen().contains("Image limit reached"),
        "over-cap multi-image paste must display limit toast:\n{}",
        ui.screen()
    );
    ui.key(KeyCode::Enter, KeyModifiers::NONE);
    {
        let rec = harness.received.lock().unwrap();
        assert_eq!(rec.len(), 4);
        assert_eq!(
            rec[3].images.len(),
            jimu_tui::ui::views::prompt_widget::PromptWidget::IMAGE_CAP
        );
    }

    ui.stop();
    let _ = std::fs::remove_dir_all(&tmp);

    // 4. History restore with literal `[Image #1]` text alongside real image attachments must still
    // display the real `[Image #1]` `[Image #2]` chips (never hide real attachments).
    #[cfg(feature = "jimu")]
    {
        use jimu_ai::{
            ApiKind, AssistantBlock, AssistantMessage, AssistantMessageEvent as Ev, ContentPart,
            ImageContent, Model, StopReason, TextContent, Usage,
        };
        let provider = Arc::new(jimu_testkit::FauxProvider::new());
        provider.enqueue_events(vec![Ev::Done {
            reason: StopReason::Stop,
            message: AssistantMessage {
                content: vec![AssistantBlock::Text(TextContent::plain("seen"))],
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
            },
        }]);
        let model = Model::new("mock-model", "mock", ApiKind::OpenaiResponses, "");
        let mut cfg = jimu_kit::KitSessionConfig::new(model);
        cfg.skill_discovery.include_user_skills = false;
        let session = Arc::new(jimu_kit::KitSession::new(provider, cfg).expect("session"));
        let jimu_harness = jimu_tui::jimu::JimuHarness::new(Arc::clone(&session));
        futures_executor::block_on(session.prompt_parts(vec![
            ContentPart::Text(TextContent::plain("compare with [Image #1] literal")),
            ContentPart::Image(ImageContent {
                data: "AAAA".into(),
                mime_type: "image/png".into(),
            }),
            ContentPart::Image(ImageContent {
                data: "BBBB".into(),
                mime_type: "image/png".into(),
            }),
        ]))
        .expect("prompt_parts");
        let history = jimu_harness.initial_history();
        match &history[0] {
            jimu_tui::harness::HistoryMessage::User { text, .. } => {
                assert_eq!(
                    text,
                    "[Image #1] [Image #2] compare with [Image #1] literal"
                );
            }
            other => panic!("expected User history message, got {other:?}"),
        }
    }
}
