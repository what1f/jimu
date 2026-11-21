use crate::session::global_pool;
use crate::types::{FfiHttpRequestHead, PikraStatus};
use bytes::Bytes;
use futures_channel::{mpsc, oneshot};
use futures_core::Stream;
use futures_util::future::{Either, select};
use pikra_ai::{
    BoxFuture, CancelToken, CancelledFuture, HttpRequest, HttpStreamResponse, HttpTransport,
    TransportError,
};
use std::ffi::{CStr, CString, c_char, c_void};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

const HTTP_CHUNK_CHANNEL_CAPACITY: usize = 256;

pub type PikraHttpStartFn = extern "C" fn(
    ctx: *mut c_void,
    request_id: u64,
    request_json: *const c_char,
    body_ptr: *const u8,
    body_len: usize,
    stream: *mut PikraHttpStream,
);

pub type PikraHttpCancelFn = extern "C" fn(ctx: *mut c_void, request_id: u64);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PikraHttpCallbacks {
    pub ctx: *mut c_void,
    pub start: Option<PikraHttpStartFn>,
    pub cancel: Option<PikraHttpCancelFn>,
    pub destroy: Option<crate::session::PikraVoidDoneFn>,
}

#[derive(Clone, Copy)]
pub(crate) struct SendHttpCallbacks {
    raw: PikraHttpCallbacks,
}

// SAFETY: Host guarantees `ctx` and callback pointers remain valid and thread-safe until
// `destroy` is invoked when `FfiSession` drops.
unsafe impl Send for SendHttpCallbacks {}
unsafe impl Sync for SendHttpCallbacks {}

impl SendHttpCallbacks {
    pub(crate) fn new(raw: PikraHttpCallbacks) -> Result<Self, String> {
        if raw.start.is_none() {
            return Err("http_callbacks.start must not be null".to_string());
        }
        Ok(Self { raw })
    }

    pub(crate) fn fire_destroy(&self) {
        if let Some(f) = self.raw.destroy {
            f(self.raw.ctx);
        }
    }

    fn fire_start(
        &self,
        request_id: u64,
        request_json: &CStr,
        body: &[u8],
        stream: *mut PikraHttpStream,
    ) {
        if let Some(f) = self.raw.start {
            let body_ptr = if body.is_empty() {
                std::ptr::null()
            } else {
                body.as_ptr()
            };
            f(
                self.raw.ctx,
                request_id,
                request_json.as_ptr(),
                body_ptr,
                body.len(),
                stream,
            );
        }
    }

    pub(crate) fn fire_cancel(&self, request_id: u64) {
        if let Some(f) = self.raw.cancel {
            f(self.raw.ctx, request_id);
        }
    }
}

struct HttpRequestLifecycle {
    req_id: u64,
    callbacks: SendHttpCallbacks,
    cancelled: AtomicBool,
    finished: AtomicBool,
    cancel_fired: AtomicBool,
    terminal_error: Mutex<Option<TransportError>>,
    finish_notify: Mutex<Option<oneshot::Sender<()>>>,
}

impl HttpRequestLifecycle {
    fn fire_cancel_once(&self) {
        self.cancelled.store(true, Ordering::Release);
        if !self.finished.load(Ordering::Acquire) && !self.cancel_fired.swap(true, Ordering::AcqRel)
        {
            self.callbacks.fire_cancel(self.req_id);
        }
    }

    fn mark_finished(&self) {
        self.finished.store(true, Ordering::Release);
        if let Some(tx) = self
            .finish_notify
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = tx.send(());
        }
    }

    fn set_terminal_error(&self, err: TransportError) {
        let mut guard = self
            .terminal_error
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(err);
        }
    }

    fn take_terminal_error(&self) -> Option<TransportError> {
        self.terminal_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

type HttpHeadResult = Result<(u16, Vec<(String, String)>), TransportError>;

/// Opaque HTTP stream sink handle owned by the host until [`pikra_http_finish`] is called.
pub struct PikraHttpStream {
    head_tx: Mutex<Option<oneshot::Sender<HttpHeadResult>>>,
    body_tx: Mutex<Option<mpsc::Sender<Bytes>>>,
    lifecycle: Arc<HttpRequestLifecycle>,
}

impl PikraHttpStream {
    pub(crate) fn on_response(
        &self,
        status_code: u16,
        headers: Vec<(String, String)>,
    ) -> PikraStatus {
        if self.lifecycle.cancelled.load(Ordering::Acquire) {
            return PikraStatus::ErrCancelled;
        }
        let tx = self
            .head_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        match tx {
            Some(sender) => {
                if sender.send(Ok((status_code, headers))).is_ok() {
                    PikraStatus::Ok
                } else {
                    PikraStatus::ErrCancelled
                }
            }
            None => PikraStatus::ErrInvalidArgument,
        }
    }

    pub(crate) fn push_chunk(&self, bytes: Bytes) -> PikraStatus {
        if self.lifecycle.cancelled.load(Ordering::Acquire) {
            return PikraStatus::ErrCancelled;
        }
        if bytes.is_empty() {
            return PikraStatus::Ok;
        }
        let mut guard = self.body_tx.lock().unwrap_or_else(|e| e.into_inner());
        let Some(tx) = guard.as_mut() else {
            return PikraStatus::ErrClosed;
        };
        match tx.try_send(bytes) {
            Ok(()) => PikraStatus::Ok,
            Err(err) if err.is_full() => PikraStatus::ErrBackpressure,
            Err(_) => PikraStatus::ErrCancelled,
        }
    }

    pub(crate) fn finish(self, error_kind: i32, error_message: Option<String>) {
        self.lifecycle.mark_finished();

        let err = if error_kind == 0 {
            None
        } else {
            let msg = error_message.unwrap_or_else(|| "HTTP transport error".to_string());
            Some(match error_kind {
                1 => TransportError::Aborted,
                2 => TransportError::Timeout,
                _ => TransportError::Network(msg),
            })
        };

        if let Some(head_tx) = self
            .head_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let head_err = err.clone().unwrap_or_else(|| {
                TransportError::Network("HTTP stream finished before response headers".to_string())
            });
            let _ = head_tx.send(Err(head_err));
        }

        if let Some(err) = err {
            self.lifecycle.set_terminal_error(err);
        }

        // Dropping `body_tx` signals EOF on `body_rx`.
        let _ = self
            .body_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}

struct FfiByteStream {
    rx: mpsc::Receiver<Bytes>,
    cancel: CancelToken,
    cancel_fut: Pin<Box<CancelledFuture>>,
    lifecycle: Arc<HttpRequestLifecycle>,
    done: bool,
}

impl Stream for FfiByteStream {
    type Item = Result<Bytes, TransportError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }

        if self.cancel.is_cancelled() || self.cancel_fut.as_mut().poll(cx).is_ready() {
            self.done = true;
            self.lifecycle.fire_cancel_once();
            return Poll::Ready(Some(Err(TransportError::Aborted)));
        }

        match Pin::new(&mut self.rx).poll_next(cx) {
            Poll::Ready(Some(chunk)) => Poll::Ready(Some(Ok(chunk))),
            Poll::Ready(None) => {
                self.done = true;
                if let Some(err) = self.lifecycle.take_terminal_error() {
                    Poll::Ready(Some(Err(err)))
                } else {
                    Poll::Ready(None)
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for FfiByteStream {
    fn drop(&mut self) {
        if !self.done && !self.lifecycle.finished.load(Ordering::Acquire) {
            self.lifecycle.fire_cancel_once();
        }
    }
}

pub(crate) struct FfiHttpTransport {
    callbacks: SendHttpCallbacks,
    next_request_id: AtomicU64,
}

impl FfiHttpTransport {
    pub(crate) fn new(callbacks: SendHttpCallbacks) -> Self {
        Self {
            callbacks,
            next_request_id: AtomicU64::new(1),
        }
    }
}

impl HttpTransport for FfiHttpTransport {
    fn send(
        &self,
        request: HttpRequest,
        cancel: CancelToken,
    ) -> BoxFuture<'_, Result<HttpStreamResponse, TransportError>> {
        let callbacks = self.callbacks;
        let req_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(TransportError::Aborted);
            }

            let head = FfiHttpRequestHead {
                method: request.method,
                url: request.url,
                headers: request.headers,
                timeout_ms: request.timeout_ms,
            };
            let head_json = serde_json::to_string(&head)
                .map_err(|e| TransportError::Network(format!("serialize HTTP request: {e}")))?;
            let head_cstr = CString::new(head_json)
                .map_err(|e| TransportError::Network(format!("invalid HTTP request JSON: {e}")))?;

            let (head_tx, head_rx) = oneshot::channel();
            let (body_tx, body_rx) = mpsc::channel(HTTP_CHUNK_CHANNEL_CAPACITY);
            let (finish_tx, finish_rx) = oneshot::channel();

            let lifecycle = Arc::new(HttpRequestLifecycle {
                req_id,
                callbacks,
                cancelled: AtomicBool::new(false),
                finished: AtomicBool::new(false),
                cancel_fired: AtomicBool::new(false),
                terminal_error: Mutex::new(None),
                finish_notify: Mutex::new(Some(finish_tx)),
            });

            // Immediately fire host `cancel_http(ctx, req_id)` whenever `CancelToken` cancels.
            {
                let cancel_watch = cancel.clone();
                let lc = Arc::clone(&lifecycle);
                global_pool().spawn_ok(async move {
                    let _ = select(cancel_watch.cancelled(), finish_rx).await;
                    if cancel_watch.is_cancelled() && !lc.finished.load(Ordering::Acquire) {
                        lc.fire_cancel_once();
                    }
                });
            }

            let stream_box = Box::new(PikraHttpStream {
                head_tx: Mutex::new(Some(head_tx)),
                body_tx: Mutex::new(Some(body_tx)),
                lifecycle: Arc::clone(&lifecycle),
            });
            let stream_ptr = Box::into_raw(stream_box);

            callbacks.fire_start(req_id, &head_cstr, &request.body, stream_ptr);

            match select(head_rx, cancel.cancelled()).await {
                Either::Left((Ok(Ok((status, headers))), _)) => {
                    if cancel.is_cancelled() {
                        lifecycle.fire_cancel_once();
                        return Err(TransportError::Aborted);
                    }
                    let cancel_fut = Box::pin(cancel.cancelled());
                    Ok(HttpStreamResponse {
                        status,
                        headers,
                        body: Box::pin(FfiByteStream {
                            rx: body_rx,
                            cancel,
                            cancel_fut,
                            lifecycle,
                            done: false,
                        }),
                    })
                }
                Either::Left((Ok(Err(err)), _)) => Err(err),
                Either::Left((Err(_), _)) => {
                    if cancel.is_cancelled() {
                        lifecycle.fire_cancel_once();
                        Err(TransportError::Aborted)
                    } else {
                        Err(TransportError::Network(
                            "HTTP stream dropped before headers arrived".to_string(),
                        ))
                    }
                }
                Either::Right(((), _)) => {
                    lifecycle.fire_cancel_once();
                    Err(TransportError::Aborted)
                }
            }
        })
    }
}
