//! Native in-process Pikra adapter (`#[cfg(feature = "pikra")]`) connecting [`KitSession`]
//! directly to [`crate::harness::TuiHarness`] with zero ACP serialization overhead.

mod approval;
mod events;

pub use approval::{PikraApprovalMiddleware, PikraInteractionSink};

use crate::harness::{
    BackgroundTask, BoxEventStream, BoxFuture, CompactionCap, ContextMeterCap,
    ContextMeterSnapshot, HarnessCapabilities, HistoryMessage, ModelOption, ModelPickerCap,
    PermissionModeCap, QueueCap, QueuedItem, RewindCap, RewindPoint, SessionOption,
    SessionPickerCap, SkillSummary, SkillsCap, TasksCap, TuiEvent, TuiHarness, UsageStats,
    UserInput,
};
use events::{
    KitEventTranslator, context_usage_to_snapshot, queued_summary_to_item, restore_history,
    user_input_to_parts,
};
use futures_channel::mpsc;
use futures_core::Stream;
use futures_util::StreamExt;
use pikra_agent::AgentMessage;
use pikra_ai::{AssistantBlock, ContentPart, Message, Model, ThinkingLevel};
use pikra_kit::KitSession;
use pikra_session::{SessionEntryPayload, SessionTreeNode};
use pikra_tools::{MonitorManager, ProcessSessionManager};
use std::collections::{HashSet, VecDeque};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::task::{Context, Poll};

/// Host-supplied session lifecycle controller for `/resume` and `/new`.
///
/// Kept behind a trait so `pikra-tui` never hardcodes product-specific session directories.
pub trait PikraSessionController: Send + Sync {
    fn current_session_id(&self) -> Option<String>;
    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionOption>, String>>;
    fn open_session(&self, session_id: &str) -> BoxFuture<'_, Result<Arc<KitSession>, String>>;
    fn create_session(&self) -> BoxFuture<'_, Result<Arc<KitSession>, String>>;
}

enum DriverCommand {
    Submit(UserInput),
    ReplaceSession(Arc<KitSession>),
}

/// Runtime-agnostic native adapter wrapping one active [`KitSession`].
///
/// Requires no `tokio::spawn`: the event stream returned by [`TuiHarness::subscribe`] drives
/// foreground prompts, background wakeups, and middleware interaction events directly inside
/// the caller's executor (compatible with `tokio`, `LocalPool`, or any standard async runtime).
pub struct PikraHarness {
    session: RwLock<Arc<KitSession>>,
    sink: PikraInteractionSink,
    cmd_tx: Mutex<Option<mpsc::UnboundedSender<DriverCommand>>>,
    process_manager: Option<ProcessSessionManager>,
    monitor_manager: Option<MonitorManager>,
    session_controller: Option<Arc<dyn PikraSessionController>>,
    permission_mode: Option<Arc<dyn PermissionModeCap>>,
    models: Vec<Model>,
    serial: AtomicU64,
}

impl PikraHarness {
    /// Wrap a [`KitSession`] with a fresh [`PikraInteractionSink`].
    pub fn new(session: Arc<KitSession>) -> Self {
        Self::with_interaction_sink(session, PikraInteractionSink::new())
    }

    /// Wrap a [`KitSession`] sharing an existing [`PikraInteractionSink`] (typically created
    /// beforehand to construct [`PikraApprovalMiddleware`]).
    pub fn with_interaction_sink(session: Arc<KitSession>, sink: PikraInteractionSink) -> Self {
        Self {
            session: RwLock::new(session),
            sink,
            cmd_tx: Mutex::new(None),
            process_manager: None,
            monitor_manager: None,
            session_controller: None,
            permission_mode: None,
            models: Vec::new(),
            serial: AtomicU64::new(1),
        }
    }

    /// Access the currently active [`KitSession`].
    pub fn session(&self) -> Arc<KitSession> {
        self.session
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replace the active [`KitSession`] (e.g. after `/resume` or `/new`).
    fn swap_session(&self, next: Arc<KitSession>) {
        *self.session.write().unwrap_or_else(|e| e.into_inner()) = Arc::clone(&next);
        if let Some(tx) = self
            .cmd_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            let _ = tx.unbounded_send(DriverCommand::ReplaceSession(next));
        }
        self.emit_context_snapshot();
        self.emit_queue_snapshot();
    }

    /// Access the interaction sink for custom tools or middleware.
    pub fn interaction_sink(&self) -> PikraInteractionSink {
        self.sink.clone()
    }

    /// Attach a [`ProcessSessionManager`] so running background shells appear in the status strip.
    pub fn with_process_manager(mut self, manager: ProcessSessionManager) -> Self {
        self.process_manager = Some(manager);
        self
    }

    /// Attach a [`MonitorManager`] so running monitors appear in the status strip.
    pub fn with_monitor_manager(mut self, manager: MonitorManager) -> Self {
        self.monitor_manager = Some(manager);
        self
    }

    /// Attach a host session controller to unlock `/resume` and `/new`.
    pub fn with_session_controller(mut self, controller: Arc<dyn PikraSessionController>) -> Self {
        self.session_controller = Some(controller);
        self
    }

    /// Attach a host permission-mode controller to unlock `/permissions` and `Shift+Tab`.
    pub fn with_permission_mode(mut self, controller: Arc<dyn PermissionModeCap>) -> Self {
        self.permission_mode = Some(controller);
        self
    }

    /// Provide selectable models to unlock the `/model` picker capability.
    pub fn with_models(mut self, models: Vec<Model>) -> Self {
        self.models = models;
        self
    }

    fn next_id(&self) -> String {
        let n = self.serial.fetch_add(1, Ordering::Relaxed);
        format!("pikra-tui-{n}")
    }

    fn emit_queue_snapshot(&self) {
        let items: Vec<QueuedItem> = self
            .session()
            .pending_inputs()
            .into_iter()
            .map(queued_summary_to_item)
            .collect();
        self.sink.emit(TuiEvent::QueueChanged(items));
    }

    fn emit_context_snapshot(&self) {
        if let Some(usage) = self.session().context_usage() {
            self.sink
                .emit(TuiEvent::ContextUsageUpdate(context_usage_to_snapshot(
                    usage,
                )));
        }
    }
}

impl TuiHarness for PikraHarness {
    fn submit(&self, input: UserInput) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let tx = self
                .cmd_tx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .ok_or_else(|| "TUI event stream is not subscribed".to_owned())?;
            tx.unbounded_send(DriverCommand::Submit(input))
                .map_err(|_| "TUI event stream closed".to_owned())
        })
    }

    fn subscribe(&self) -> BoxEventStream {
        let (event_tx, event_rx) = mpsc::unbounded();
        let (cmd_tx, cmd_rx) = mpsc::unbounded();
        self.sink.attach(event_tx.clone());
        *self.cmd_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(cmd_tx);
        self.emit_context_snapshot();
        self.emit_queue_snapshot();
        let session = self.session();
        let wake_rx = session.subscribe_background_wakeups();
        Box::pin(PikraEventStream {
            session,
            event_tx,
            event_rx,
            cmd_rx,
            wake_rx,
            active_turn: None,
            pending_submits: VecDeque::new(),
        })
    }

    fn abort(&self) -> BoxFuture<'_, ()> {
        let session = self.session();
        Box::pin(async move {
            session.abort().await;
            self.emit_queue_snapshot();
            self.emit_context_snapshot();
        })
    }

    fn steer(&self, input: UserInput) -> BoxFuture<'_, Result<(), String>> {
        let session = self.session();
        Box::pin(async move {
            let id = self.next_id();
            if input.images.is_empty() {
                session.steer(id, input.text);
            } else {
                session
                    .steer_parts(id, user_input_to_parts(input), false)
                    .await;
            }
            self.emit_queue_snapshot();
            Ok(())
        })
    }

    fn follow_up(&self, input: UserInput) -> BoxFuture<'_, Result<(), String>> {
        let session = self.session();
        Box::pin(async move {
            let id = self.next_id();
            session.follow_up(id, input.text);
            self.emit_queue_snapshot();
            Ok(())
        })
    }

    fn initial_history(&self) -> Vec<HistoryMessage> {
        restore_history(self.session().visible_messages()).0
    }

    fn initial_usage(&self) -> UsageStats {
        restore_history(self.session().visible_messages()).1
    }

    fn capabilities(&self) -> HarnessCapabilities<'_> {
        let has_tasks = self.process_manager.is_some() || self.monitor_manager.is_some();
        HarnessCapabilities {
            context_meter: Some(self),
            queue: Some(self),
            compaction: Some(self),
            model_picker: (!self.models.is_empty()).then_some(self as &dyn ModelPickerCap),
            permission_mode: self.permission_mode.as_deref(),
            session_picker: self
                .session_controller
                .is_some()
                .then_some(self as &dyn SessionPickerCap),
            tasks: has_tasks.then_some(self as &dyn TasksCap),
            skills: Some(self),
            mcp: None,
            rewind: Some(self),
        }
    }
}

impl ContextMeterCap for PikraHarness {
    fn context_usage(&self) -> Option<ContextMeterSnapshot> {
        self.session()
            .context_usage()
            .map(context_usage_to_snapshot)
    }
}

impl QueueCap for PikraHarness {
    fn pending_inputs(&self) -> Vec<QueuedItem> {
        self.session()
            .pending_inputs()
            .into_iter()
            .map(queued_summary_to_item)
            .collect()
    }

    fn remove_queued(&self, id: &str) -> bool {
        let removed = self.session().remove_queued_input(id);
        if removed {
            self.emit_queue_snapshot();
        }
        removed
    }
}

impl CompactionCap for PikraHarness {
    fn compact(&self) -> BoxFuture<'_, Result<bool, String>> {
        let session = self.session();
        Box::pin(async move {
            let res = session.compact().await.map_err(|e| e.to_string())?;
            self.emit_context_snapshot();
            Ok(res.is_some())
        })
    }
}

impl SkillsCap for PikraHarness {
    fn list_skills(&self) -> Vec<SkillSummary> {
        let session = self.session();
        session.reload_skills();
        session
            .skills()
            .skills()
            .iter()
            .map(|s| SkillSummary {
                name: s.name.clone(),
                description: s.description.clone(),
            })
            .collect()
    }

    fn reload_skills(&self) -> bool {
        self.session().reload_skills()
    }
}

impl SessionPickerCap for PikraHarness {
    fn current_session_id(&self) -> Option<String> {
        self.session_controller
            .as_ref()
            .and_then(|ctrl| ctrl.current_session_id())
    }

    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionOption>, String>> {
        let ctrl = self.session_controller.clone();
        Box::pin(async move {
            let ctrl = ctrl.ok_or_else(|| "session switching is unavailable".to_owned())?;
            ctrl.list_sessions().await
        })
    }

    fn switch_session(&self, session_id: &str) -> BoxFuture<'_, Result<(), String>> {
        let ctrl = self.session_controller.clone();
        let id = session_id.to_owned();
        Box::pin(async move {
            let ctrl = ctrl.ok_or_else(|| "session switching is unavailable".to_owned())?;
            self.session().abort().await;
            let next = ctrl.open_session(&id).await?;
            self.swap_session(next);
            Ok(())
        })
    }

    fn new_session(&self) -> BoxFuture<'_, Result<(), String>> {
        let ctrl = self.session_controller.clone();
        Box::pin(async move {
            let ctrl = ctrl.ok_or_else(|| "session creation is unavailable".to_owned())?;
            self.session().abort().await;
            let next = ctrl.create_session().await?;
            self.swap_session(next);
            Ok(())
        })
    }
}

impl RewindCap for PikraHarness {
    fn list_rewind_points(&self) -> Vec<RewindPoint> {
        let session = self.session();
        let tree = session.tree();
        let leaf_id = session.leaf_id();
        build_rewind_points(&tree, leaf_id.as_deref())
    }

    fn rewind_to(&self, id: &str) -> BoxFuture<'_, Result<Option<String>, String>> {
        let session = self.session();
        let target_id = id.to_owned();
        Box::pin(async move {
            session.abort().await;
            let tree = session.tree();
            let Some(found) = find_tree_entry(&tree, &target_id) else {
                return Err(format!("checkpoint `{target_id}` not found"));
            };
            // Selecting a user message rewinds to right before it (its parent entry) and
            // refills the user's text into the composer for edit-and-resend (creating a new branch).
            // Selecting an assistant or compaction node switches the active leaf directly to it.
            let (fork_target, refill) = match &found.payload {
                SessionEntryPayload::Message {
                    message: AgentMessage::Llm(Message::User(user)),
                } => {
                    let text = extract_user_text(&user.content);
                    if let Some(parent) = found.parent_id.as_deref() {
                        (parent.to_owned(), (!text.is_empty()).then_some(text))
                    } else {
                        (found.id.clone(), None)
                    }
                }
                _ => (found.id.clone(), None),
            };
            session.fork(&fork_target).map_err(|e| e.to_string())?;
            self.emit_context_snapshot();
            self.emit_queue_snapshot();
            Ok(refill)
        })
    }
}

impl TasksCap for PikraHarness {
    fn list_tasks(&self) -> Vec<BackgroundTask> {
        let mut out = Vec::new();
        if let Some(pm) = &self.process_manager {
            for proc_info in pm.list_running() {
                out.push(BackgroundTask {
                    id: format!("proc:{}", proc_info.id),
                    kind: if proc_info.backgrounded {
                        "shell-bg".into()
                    } else {
                        "shell".into()
                    },
                    title: format!("shell #{}", proc_info.id),
                    command: proc_info.command,
                });
            }
        }
        if let Some(mm) = &self.monitor_manager {
            for mon in mm.list_running() {
                out.push(BackgroundTask {
                    id: format!("mon:{}", mon.id),
                    kind: "monitor".into(),
                    title: mon.name,
                    command: mon.command,
                });
            }
        }
        out
    }

    fn kill_task(&self, id: &str) -> bool {
        if let Some(raw) = id.strip_prefix("proc:")
            && let Ok(num) = raw.parse::<u32>()
            && let Some(pm) = &self.process_manager
        {
            return pm.kill(num);
        }
        if let Some(raw) = id.strip_prefix("mon:")
            && let Some(mm) = &self.monitor_manager
        {
            return mm.kill(raw);
        }
        false
    }
}

impl ModelPickerCap for PikraHarness {
    fn current_model(&self) -> ModelOption {
        model_to_option(&self.session().model())
    }

    fn available_models(&self) -> Vec<ModelOption> {
        self.models.iter().map(model_to_option).collect()
    }

    fn set_model(&self, model_id: &str) -> BoxFuture<'_, Result<(), String>> {
        let model_id = model_id.to_owned();
        let target = self
            .models
            .iter()
            .find(|m| m.id == model_id || m.name == model_id)
            .cloned();
        let session = self.session();
        Box::pin(async move {
            let model = target.ok_or_else(|| format!("unknown model `{model_id}`"))?;
            session.set_model(model).map_err(|e| e.to_string())?;
            self.emit_context_snapshot();
            Ok(())
        })
    }

    fn thinking_levels(&self) -> Vec<String> {
        vec![
            "off".into(),
            "minimal".into(),
            "low".into(),
            "medium".into(),
            "high".into(),
            "xhigh".into(),
            "max".into(),
        ]
    }

    fn current_thinking_level(&self) -> Option<String> {
        let label = match self.session().thinking_level() {
            ThinkingLevel::Off => "off",
            ThinkingLevel::None => "none",
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        };
        Some(label.into())
    }

    fn set_thinking_level(&self, level: &str) -> BoxFuture<'_, Result<(), String>> {
        let parsed = ThinkingLevel::from_str(level);
        let session = self.session();
        Box::pin(async move {
            let level = parsed?;
            session.set_thinking_level(level).map_err(|e| e.to_string())
        })
    }
}

fn model_to_option(model: &Model) -> ModelOption {
    ModelOption {
        id: model.id.clone(),
        name: model.name.clone(),
        provider: model.provider.clone(),
        supports_thinking: model.supports_reasoning,
    }
}

// ---------------------------------------------------------------------------
// Tree projection for /rewind (/tree) and double-Esc
// ---------------------------------------------------------------------------

struct VisibleNode {
    id: String,
    parent_id: Option<String>,
    role: &'static str,
    label: String,
    timestamp: Option<String>,
    prompt_text: Option<String>,
    contains_active_leaf: bool,
    is_current_leaf: bool,
    children: Vec<VisibleNode>,
}

fn find_tree_entry<'a>(
    nodes: &'a [SessionTreeNode],
    id: &str,
) -> Option<&'a pikra_session::SessionEntry> {
    for node in nodes {
        if node.entry.id == id {
            return Some(&node.entry);
        }
        if let Some(found) = find_tree_entry(&node.children, id) {
            return Some(found);
        }
    }
    None
}

fn collect_active_path(
    nodes: &[SessionTreeNode],
    leaf_id: &str,
    out: &mut HashSet<String>,
) -> bool {
    for node in nodes {
        if node.entry.id == leaf_id || collect_active_path(&node.children, leaf_id, out) {
            out.insert(node.entry.id.clone());
            return true;
        }
    }
    false
}

fn extract_user_text(content: &[ContentPart]) -> String {
    let mut parts = Vec::new();
    for item in content {
        match item {
            ContentPart::Text(t) if !t.text.trim().is_empty() => parts.push(t.text.as_str()),
            ContentPart::Image(_) => parts.push("[image]"),
            _ => {}
        }
    }
    parts.join(" ")
}

fn preview_line(raw: &str, max_chars: usize) -> String {
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (i, ch) in collapsed.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

fn format_timestamp_ms(ms: u64) -> Option<String> {
    if ms == 0 {
        return None;
    }
    let dt = chrono::DateTime::from_timestamp_millis(ms as i64)?;
    Some(
        dt.with_timezone(&chrono::Local)
            .format("%H:%M:%S")
            .to_string(),
    )
}

fn visible_entry_info(
    entry: &pikra_session::SessionEntry,
) -> Option<(&'static str, String, Option<String>)> {
    match &entry.payload {
        SessionEntryPayload::Message {
            message: AgentMessage::Llm(Message::User(user)),
        } => {
            let full = extract_user_text(&user.content);
            let label = preview_line(&full, 72);
            if label.is_empty() {
                None
            } else {
                Some(("user", label, Some(full)))
            }
        }
        SessionEntryPayload::Message {
            message: AgentMessage::Llm(Message::Assistant(assistant)),
        } => {
            let mut text = String::new();
            let mut tools = Vec::new();
            for block in &assistant.content {
                match block {
                    AssistantBlock::Text(t) if !t.text.trim().is_empty() => {
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(t.text.trim());
                    }
                    AssistantBlock::ToolCall(tc) => tools.push(tc.name.as_str()),
                    _ => {}
                }
            }
            let label = if !text.is_empty() {
                preview_line(&text, 72)
            } else if !tools.is_empty() {
                preview_line(&format!("tool: {}", tools.join(", ")), 72)
            } else {
                return None;
            };
            Some(("assistant", label, None))
        }
        SessionEntryPayload::Compaction { summary, .. } => {
            Some(("compaction", preview_line(summary, 72), None))
        }
        _ => None,
    }
}

fn project_visible(
    node: &SessionTreeNode,
    active_ids: &HashSet<String>,
    leaf_id: Option<&str>,
) -> Vec<VisibleNode> {
    let mut children = Vec::new();
    for child in &node.children {
        children.extend(project_visible(child, active_ids, leaf_id));
    }
    if let Some((role, label, prompt_text)) = visible_entry_info(&node.entry) {
        let on_active = active_ids.contains(&node.entry.id);
        let child_has_active = children.iter().any(|c| c.contains_active_leaf);
        let is_current_leaf =
            leaf_id == Some(node.entry.id.as_str()) || (on_active && !child_has_active);
        vec![VisibleNode {
            id: node.entry.id.clone(),
            parent_id: node.entry.parent_id.clone(),
            role,
            label,
            timestamp: format_timestamp_ms(node.entry.timestamp),
            prompt_text,
            contains_active_leaf: on_active || child_has_active,
            is_current_leaf,
            children,
        }]
    } else {
        children
    }
}

fn tree_has_branches(roots: &[VisibleNode]) -> bool {
    if roots.len() > 1 {
        return true;
    }
    fn walk(node: &VisibleNode) -> bool {
        if node.children.len() > 1 {
            return true;
        }
        node.children.iter().any(walk)
    }
    roots.iter().any(walk)
}

fn build_rewind_points(tree: &[SessionTreeNode], leaf_id: Option<&str>) -> Vec<RewindPoint> {
    let mut active_ids = HashSet::new();
    if let Some(leaf) = leaf_id {
        collect_active_path(tree, leaf, &mut active_ids);
    }
    let mut roots = Vec::new();
    for root in tree {
        roots.extend(project_visible(root, &active_ids, leaf_id));
    }
    if roots.is_empty() {
        return Vec::new();
    }

    let branched = tree_has_branches(&roots);
    let mut out = Vec::new();

    if !branched {
        // Linear session: flatten chain and present newest first.
        let mut cur = roots.first();
        while let Some(node) = cur {
            out.push(RewindPoint {
                id: node.id.clone(),
                parent_id: node.parent_id.clone(),
                role: node.role.into(),
                label: node.label.clone(),
                timestamp: node.timestamp.clone(),
                depth: 0,
                tree_prefix: String::new(),
                is_active_branch: node.contains_active_leaf,
                is_leaf: node.children.is_empty(),
                is_current_leaf: node.is_current_leaf,
                prompt_text: node.prompt_text.clone(),
            });
            cur = node.children.first();
        }
        out.reverse();
        return out;
    }

    // Branched session: Pi-style indented tree preserving sibling branches.
    fn emit_tree(
        node: &VisibleNode,
        depth: usize,
        prefix: &str,
        is_last: bool,
        is_root: bool,
        out: &mut Vec<RewindPoint>,
    ) {
        let branch_glyph = if is_root {
            ""
        } else if is_last {
            "└─ "
        } else {
            "├─ "
        };
        let tree_prefix = format!("{prefix}{branch_glyph}");
        out.push(RewindPoint {
            id: node.id.clone(),
            parent_id: node.parent_id.clone(),
            role: node.role.into(),
            label: node.label.clone(),
            timestamp: node.timestamp.clone(),
            depth,
            tree_prefix,
            is_active_branch: node.contains_active_leaf,
            is_leaf: node.children.is_empty(),
            is_current_leaf: node.is_current_leaf,
            prompt_text: node.prompt_text.clone(),
        });
        let child_prefix = if is_root {
            prefix.to_owned()
        } else if is_last {
            format!("{prefix}   ")
        } else {
            format!("{prefix}│  ")
        };
        let total = node.children.len();
        for (idx, child) in node.children.iter().enumerate() {
            emit_tree(
                child,
                depth + 1,
                &child_prefix,
                idx + 1 == total,
                false,
                out,
            );
        }
    }

    let root_count = roots.len();
    for (idx, root) in roots.iter().enumerate() {
        emit_tree(
            root,
            0,
            "",
            idx + 1 == root_count,
            root_count == 1,
            &mut out,
        );
    }
    out
}

/// Self-driving event stream that pumps `KitSession` turns inside `poll_next` without requiring
/// `tokio::spawn`.
struct PikraEventStream {
    session: Arc<KitSession>,
    event_tx: mpsc::UnboundedSender<TuiEvent>,
    event_rx: mpsc::UnboundedReceiver<TuiEvent>,
    cmd_rx: mpsc::UnboundedReceiver<DriverCommand>,
    wake_rx: mpsc::UnboundedReceiver<()>,
    active_turn: Option<BoxFuture<'static, ()>>,
    pending_submits: VecDeque<UserInput>,
}

impl PikraEventStream {
    fn start_foreground_turn(&mut self, input: UserInput) {
        let session = self.session.clone();
        let tx = self.event_tx.clone();
        self.active_turn = Some(Box::pin(async move {
            let parts = user_input_to_parts(input);
            match session.prompt_parts_stream(parts) {
                Ok(mut stream) => {
                    let mut translator = KitEventTranslator::default();
                    let mut buf = Vec::new();
                    while let Some(ev) = stream.next().await {
                        translator.translate(ev, &mut buf);
                        for out in buf.drain(..) {
                            let _ = tx.unbounded_send(out);
                        }
                        if let Some(usage) = session.context_usage() {
                            let _ = tx.unbounded_send(TuiEvent::ContextUsageUpdate(
                                context_usage_to_snapshot(usage),
                            ));
                        }
                        let queue: Vec<QueuedItem> = session
                            .pending_inputs()
                            .into_iter()
                            .map(queued_summary_to_item)
                            .collect();
                        let _ = tx.unbounded_send(TuiEvent::QueueChanged(queue));
                    }
                }
                Err(err) => {
                    let _ = tx.unbounded_send(TuiEvent::Error(err.to_string()));
                }
            }
        }));
    }

    fn start_background_turn(&mut self) {
        let session = self.session.clone();
        let tx = self.event_tx.clone();
        self.active_turn = Some(Box::pin(async move {
            match session.run_background_turn() {
                Ok(Some(mut stream)) => {
                    let mut translator = KitEventTranslator::default();
                    let mut buf = Vec::new();
                    while let Some(ev) = stream.next().await {
                        translator.translate(ev, &mut buf);
                        for out in buf.drain(..) {
                            let _ = tx.unbounded_send(out);
                        }
                        if let Some(usage) = session.context_usage() {
                            let _ = tx.unbounded_send(TuiEvent::ContextUsageUpdate(
                                context_usage_to_snapshot(usage),
                            ));
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    let _ = tx.unbounded_send(TuiEvent::Error(err.to_string()));
                }
            }
        }));
    }
}

impl Stream for PikraEventStream {
    type Item = TuiEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            // 1. Drain any pending driver commands (foreground submits or session swaps).
            while let Poll::Ready(Some(cmd)) = Pin::new(&mut this.cmd_rx).poll_next(cx) {
                match cmd {
                    DriverCommand::Submit(input) => this.pending_submits.push_back(input),
                    DriverCommand::ReplaceSession(next) => {
                        this.active_turn = None;
                        this.pending_submits.clear();
                        this.wake_rx = next.subscribe_background_wakeups();
                        this.session = next;
                    }
                }
            }

            // 2. Poll the active turn future if one is running.
            if let Some(fut) = this.active_turn.as_mut()
                && fut.as_mut().poll(cx).is_ready()
            {
                this.active_turn = None;
            }

            // 3. Start next foreground submit or background wakeup when idle.
            if this.active_turn.is_none() {
                if let Some(input) = this.pending_submits.pop_front() {
                    this.start_foreground_turn(input);
                    continue;
                }
                if let Poll::Ready(Some(())) = Pin::new(&mut this.wake_rx).poll_next(cx) {
                    this.start_background_turn();
                    continue;
                }
            }

            // 4. Deliver any queued TUI events.
            match Pin::new(&mut this.event_rx).poll_next(cx) {
                Poll::Ready(Some(ev)) => return Poll::Ready(Some(ev)),
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
