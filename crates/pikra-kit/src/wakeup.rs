use futures_channel::{mpsc, oneshot};
use futures_util::lock::Mutex as AsyncMutex;
use futures_util::task::AtomicWaker;
use pikra_ai::{BoxFuture, CancelToken};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

pub(crate) struct SharedKitDriver {
    pub(crate) future: AsyncMutex<Option<BoxFuture<'static, ()>>>,
    pub(crate) stream_waker: AtomicWaker,
    pub(crate) completed: AtomicBool,
}

struct WakeOnDrop<'a>(&'a AtomicWaker);

impl Drop for WakeOnDrop<'_> {
    fn drop(&mut self) {
        self.0.wake();
    }
}

impl SharedKitDriver {
    pub(crate) fn new(future: BoxFuture<'static, ()>) -> Arc<Self> {
        Arc::new(Self {
            future: AsyncMutex::new(Some(future)),
            stream_waker: AtomicWaker::new(),
            completed: AtomicBool::new(false),
        })
    }

    pub(crate) async fn drive(&self) {
        let _wake = WakeOnDrop(&self.stream_waker);
        let mut guard = self.future.lock().await;
        if let Some(fut) = guard.as_mut() {
            fut.as_mut().await;
            *guard = None;
            self.completed.store(true, Ordering::Release);
        }
    }
}

/// A background event supplied by the host. The XML payload is sent as provided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundNotification {
    pub source_id: String,
    pub kind: String,
    pub summary: String,
    pub xml_payload: String,
    pub timestamp_ms: u64,
}

/// Identifies why a session started a prompt stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KitTurnOrigin {
    Foreground,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Activity {
    Prompt(KitTurnOrigin),
    Compacting,
}

#[derive(Default)]
struct InboxState {
    active: Option<Activity>,
    aborted: bool,
    active_cancel: Option<CancelToken>,
    active_driver: Option<Weak<SharedKitDriver>>,
    idle_waiters: Vec<oneshot::Sender<()>>,
    paused: bool,
    pending: Vec<BackgroundNotification>,
    subscribers: Vec<mpsc::UnboundedSender<()>>,
}

impl InboxState {
    fn ready(&self) -> bool {
        self.active.is_none() && !self.paused && !self.pending.is_empty()
    }

    fn notify_if_ready(&mut self, was_ready: bool) {
        if !was_ready && self.ready() {
            self.subscribers
                .retain(|sender| sender.unbounded_send(()).is_ok());
        }
    }
}

/// Cloneable, thread-safe producer for session background events.
#[derive(Clone)]
pub struct BackgroundEventSender {
    state: Arc<Mutex<InboxState>>,
}

impl BackgroundEventSender {
    /// Enqueue a notification. Wake subscribers when a background turn can start.
    pub fn send(&self, notification: BackgroundNotification) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let was_ready = state.ready();
        state.pending.push(notification);
        state.notify_if_ready(was_ready);
    }
}

/// A coalescing, runtime-independent inbox for host notifications.
#[derive(Default)]
pub struct BackgroundInbox {
    state: Arc<Mutex<InboxState>>,
}

impl BackgroundInbox {
    pub fn sender(&self) -> BackgroundEventSender {
        BackgroundEventSender {
            state: Arc::clone(&self.state),
        }
    }

    /// Subscribe to readiness transitions. A new subscriber gets a wakeup if work is ready.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<()> {
        let (tx, rx) = mpsc::unbounded();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.subscribers.retain(|sender| !sender.is_closed());
        if state.ready() {
            let _ = tx.unbounded_send(());
        }
        state.subscribers.push(tx);
        rx
    }

    /// Take pending events as a single message while idle, including while paused.
    pub fn drain_prompt(&self) -> Option<String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.is_some() || state.pending.is_empty() {
            return None;
        }
        let pending = std::mem::take(&mut state.pending);
        drop(state);
        Some(format_prompt(&pending))
    }

    pub fn resume(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let was_ready = state.ready();
        state.paused = false;
        state.notify_if_ready(was_ready);
    }

    pub fn is_paused(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).paused
    }

    pub(crate) fn is_active(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active
            .is_some()
    }

    pub(crate) fn with_idle<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active.is_none().then(f)
    }

    /// Return None when a background prompt was requested but nothing is ready.
    pub(crate) fn begin(
        &self,
        activity: Activity,
        pause_on_error: bool,
    ) -> Result<Option<ActivityGuard>, crate::KitError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.is_some() {
            return Err(crate::KitError::Busy);
        }
        if activity == Activity::Prompt(KitTurnOrigin::Background) && !state.ready() {
            return Ok(None);
        }
        let cancel = CancelToken::new();
        state.active = Some(activity);
        state.aborted = false;
        state.active_cancel = Some(cancel.clone());
        state.active_driver = None;
        Ok(Some(ActivityGuard {
            state: Arc::clone(&self.state),
            cancel,
            pause_on_error,
            outcome: None,
            started: false,
            batch: Vec::new(),
        }))
    }

    pub(crate) fn attach_driver(&self, driver: &Arc<SharedKitDriver>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.is_some() {
            state.active_driver = Some(Arc::downgrade(driver));
        }
    }

    pub(crate) fn abort_prompt(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.is_some() {
            state.aborted = true;
            if let Some(cancel) = &state.active_cancel {
                cancel.cancel();
            }
        }
    }

    pub(crate) fn begin_abort(
        &self,
    ) -> (Option<oneshot::Receiver<()>>, Option<Arc<SharedKitDriver>>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.is_some() {
            state.aborted = true;
            if let Some(cancel) = &state.active_cancel {
                cancel.cancel();
            }
            let (tx, rx) = oneshot::channel();
            state.idle_waiters.push(tx);
            let driver = state.active_driver.as_ref().and_then(Weak::upgrade);
            (Some(rx), driver)
        } else {
            (None, None)
        }
    }

    pub(crate) fn begin_wait_for_idle(
        &self,
    ) -> (Option<oneshot::Receiver<()>>, Option<Arc<SharedKitDriver>>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.active.is_some() {
            let (tx, rx) = oneshot::channel();
            state.idle_waiters.push(tx);
            let driver = state.active_driver.as_ref().and_then(Weak::upgrade);
            (Some(rx), driver)
        } else {
            (None, None)
        }
    }
}

pub(crate) struct ActivityGuard {
    state: Arc<Mutex<InboxState>>,
    cancel: CancelToken,
    pause_on_error: bool,
    outcome: Option<bool>,
    started: bool,
    batch: Vec<BackgroundNotification>,
}

impl ActivityGuard {
    /// Leave a reserved background batch queued until the returned stream is polled.
    pub(crate) fn start(&mut self, origin: KitTurnOrigin) -> Option<String> {
        self.started = true;
        if origin == KitTurnOrigin::Background {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            self.batch = std::mem::take(&mut state.pending);
            return Some(format_prompt(&self.batch));
        }
        None
    }

    pub(crate) fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    pub(crate) fn aborted(&self) -> bool {
        self.cancel.is_cancelled() || self.state.lock().unwrap_or_else(|e| e.into_inner()).aborted
    }

    pub(crate) fn delivered(&mut self) {
        self.batch.clear();
    }

    pub(crate) fn complete(&mut self, success: bool) {
        self.outcome = Some(success);
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        let waiters = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if !self.batch.is_empty() {
                self.batch.append(&mut state.pending);
                state.pending = std::mem::take(&mut self.batch);
            }
            if matches!(state.active, Some(Activity::Prompt(_))) {
                if self.pause_on_error
                    && (state.aborted || (self.started && self.outcome != Some(true)))
                {
                    state.paused = true;
                } else if self.outcome == Some(true) {
                    state.paused = false;
                }
            }
            state.active = None;
            state.active_cancel = None;
            state.active_driver = None;
            state.notify_if_ready(false);
            std::mem::take(&mut state.idle_waiters)
        };
        for tx in waiters {
            let _ = tx.send(());
        }
    }
}

fn format_prompt(notifications: &[BackgroundNotification]) -> String {
    let mut prompt = String::from("<background_events>\n");
    for event in notifications {
        prompt.push_str(&event.xml_payload);
        prompt.push('\n');
    }
    prompt.push_str("</background_events>");
    prompt
}
