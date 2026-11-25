use futures_channel::{mpsc, oneshot};
use futures_util::future::poll_fn;
use futures_util::task::AtomicWaker;
use jimu_ai::{BoxFuture, CancelToken};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Wake, Waker};

struct DriverWake {
    stream_waker: AtomicWaker,
    waiters: Mutex<Vec<Waker>>,
}

impl DriverWake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stream_waker: AtomicWaker::new(),
            waiters: Mutex::new(Vec::new()),
        })
    }

    fn register(&self, waker: &Waker) {
        let mut waiters = self.waiters.lock().unwrap_or_else(|e| e.into_inner());
        if !waiters.iter().any(|existing| existing.will_wake(waker)) {
            waiters.push(waker.clone());
        }
    }

    fn wake_all(&self) {
        self.stream_waker.wake();
        let waiters = std::mem::take(&mut *self.waiters.lock().unwrap_or_else(|e| e.into_inner()));
        for waker in waiters {
            waker.wake();
        }
    }
}

impl Wake for DriverWake {
    fn wake(self: Arc<Self>) {
        self.wake_all();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.wake_all();
    }
}

pub(crate) struct SharedKitDriver {
    future: Mutex<Option<BoxFuture<'static, ()>>>,
    wake: Arc<DriverWake>,
    pub(crate) completed: AtomicBool,
}

struct WakeOnDrop<'a>(&'a DriverWake);

impl Drop for WakeOnDrop<'_> {
    fn drop(&mut self) {
        self.0.wake_all();
    }
}

impl SharedKitDriver {
    pub(crate) fn new(future: BoxFuture<'static, ()>) -> Arc<Self> {
        Arc::new(Self {
            future: Mutex::new(Some(future)),
            wake: DriverWake::new(),
            completed: AtomicBool::new(false),
        })
    }

    pub(crate) fn register_stream_waker(&self, waker: &Waker) {
        self.wake.stream_waker.register(waker);
    }

    /// Poll the underlying driver future at most once under a short synchronous lock using the
    /// driver's own shared [`Waker`]. Releasing the lock before returning `Poll::Pending` lets
    /// concurrent callers (`abort().await`, `wait_for_idle().await`) advance cancellation without
    /// deadlocking, while the shared waker guarantees I/O readiness wakes every registered caller
    /// even if a secondary waiter polled last and was subsequently dropped.
    pub(crate) fn poll_step(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.completed.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        self.wake.register(cx.waker());
        let shared_waker = Waker::from(Arc::clone(&self.wake));
        let mut shared_cx = Context::from_waker(&shared_waker);

        let mut guard = self.future.lock().unwrap_or_else(|e| e.into_inner());
        let Some(fut) = guard.as_mut() else {
            self.completed.store(true, Ordering::Release);
            drop(guard);
            self.wake.wake_all();
            return Poll::Ready(());
        };
        match fut.as_mut().poll(&mut shared_cx) {
            Poll::Ready(()) => {
                *guard = None;
                self.completed.store(true, Ordering::Release);
                drop(guard);
                self.wake.wake_all();
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }

    pub(crate) async fn drive(&self) {
        let _wake = WakeOnDrop(&self.wake);
        poll_fn(|cx| self.poll_step(cx)).await;
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

    pub(crate) fn is_compacting(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).active == Some(Activity::Compacting)
    }

    pub(crate) fn with_idle<R>(&self, f: impl FnOnce() -> R) -> Option<R> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active.is_none().then(f)
    }

    /// Atomically claim an activity and publish its driver under one lock so concurrent `abort()`
    /// never observes an active stream without a driver.
    pub(crate) fn begin_stream(
        &self,
        activity: Activity,
        pause_on_error: bool,
        on_abort_or_fail: Option<Arc<dyn Fn() + Send + Sync>>,
        build_driver: impl FnOnce(ActivityGuard) -> Arc<SharedKitDriver>,
    ) -> Result<Option<Arc<SharedKitDriver>>, crate::KitError> {
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
        let guard = ActivityGuard {
            state: Arc::clone(&self.state),
            cancel,
            pause_on_error,
            outcome: None,
            started: false,
            batch: Vec::new(),
            on_abort_or_fail,
        };
        let driver = build_driver(guard);
        state.active_driver = Some(Arc::downgrade(&driver));
        Ok(Some(driver))
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
    on_abort_or_fail: Option<Arc<dyn Fn() + Send + Sync>>,
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
            let failed_or_aborted =
                self.cancel.is_cancelled() || state.aborted || self.outcome != Some(true);
            if failed_or_aborted && let Some(cleanup) = &self.on_abort_or_fail {
                // Run cleanup (e.g. draining stale steering even on an unpolled stream dropped
                // right after `abort_now()`) BEFORE releasing `state.active = None` and waking
                // idle waiters so the next prompt can never observe leftover steering.
                cleanup();
            }
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
