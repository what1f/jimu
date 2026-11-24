#![cfg(not(target_os = "ios"))]

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use futures_channel::mpsc;
use futures_executor::LocalPool;
use futures_util::task::LocalSpawnExt;
use pikra_tui::{
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
        pikra_tui::engine::theme::Theme::current_kind(),
        ThemeKind::Light
    );
    if dark != ratatui::style::Color::Reset {
        assert_ne!(ui.frame.borrow()[canvas()].bg, dark);
    }
    ui.enter("/theme dark");
    assert_eq!(
        pikra_tui::engine::theme::Theme::current_kind(),
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
            labels: pikra_tui::app::Labels([("Switch theme".into(), "Cambiar tema".into())].into()),
            ..Default::default()
        },
    ));
    assert!(silent.screen().contains("Cambiar tema"));
    silent.stop();
    assert!(silent.status().is_empty());
}

#[cfg(feature = "pikra")]
#[test]
fn pikra_native_harness_approval_context_queue_and_wakeup_e2e() {
    let _terminal = UI_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use pikra_agent::{AgentToolResult, ClosureTool};
    use pikra_ai::{
        ApiKind, AssistantBlock, AssistantMessage, AssistantMessageEvent as Ev, Model, StopReason,
        TextContent, ToolCall, ToolDefinition, Usage,
    };
    use pikra_kit::{BackgroundNotification, KitSession, KitSessionConfig, KitToolSpec};
    use pikra_testkit::FauxProvider;
    use pikra_tui::pikra::{PikraApprovalMiddleware, PikraHarness, PikraInteractionSink};
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

    let sink = PikraInteractionSink::new();
    let approval = PikraApprovalMiddleware::new(sink.clone(), |ctx| ctx.tool_call.name == "write");

    let mut kit_cfg = KitSessionConfig::new(model);
    kit_cfg.skill_discovery.include_user_skills = false;
    kit_cfg.tools.push(KitToolSpec::resident(write_tool));
    kit_cfg.middlewares.push(Arc::new(approval));

    let session = Arc::new(KitSession::new(provider, kit_cfg).unwrap());
    // Append a hidden meta message to prove it never leaks into the TUI transcript.
    session.append_meta("TOP_SECRET_SYSTEM_REMINDER");
    let harness = Arc::new(PikraHarness::with_interaction_sink(session.clone(), sink));
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
    let format = pikra_tui::ui::views::context_bar::fmt_tokens;
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
