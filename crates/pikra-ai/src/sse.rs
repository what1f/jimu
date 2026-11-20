//! Incremental UTF-8 + LF/CRLF Server-Sent Events (SSE) parser and cancellable stream adapter.

use crate::{ByteStream, CancelToken, CancelledFuture, TransportError};
use futures_core::Stream;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A single decoded Server-Sent Event frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

impl SseEvent {
    /// Whether the SSE payload is the OpenAI-style `[DONE]` sentinel.
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }
}

/// Incremental SSE frame parser handling arbitrary UTF-8 and LF/CRLF chunk boundaries.
#[derive(Debug, Default)]
pub struct SseParser {
    utf8_tail: Vec<u8>,
    line_buf: String,
    pending_cr: bool,
    event_name: Option<String>,
    event_id: Option<String>,
    data_lines: Vec<String>,
    has_data: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes and collect any complete SSE frames decoded from this chunk.
    pub fn push_bytes(&mut self, chunk: &[u8], out: &mut VecDeque<SseEvent>) {
        if chunk.is_empty() {
            return;
        }
        let text = self.decode_utf8_chunk(chunk);
        self.push_str(&text, out);
    }

    /// Flush any remaining partial UTF-8 bytes, trailing line, and buffered SSE frame at EOF.
    pub fn finish(&mut self, out: &mut VecDeque<SseEvent>) {
        if !self.utf8_tail.is_empty() {
            let lossy = String::from_utf8_lossy(&self.utf8_tail).into_owned();
            self.utf8_tail.clear();
            self.push_str(&lossy, out);
        }
        if self.pending_cr {
            self.pending_cr = false;
            let line = std::mem::take(&mut self.line_buf);
            self.process_line(&line, out);
        } else if !self.line_buf.is_empty() {
            let line = std::mem::take(&mut self.line_buf);
            self.process_line(&line, out);
        }
        if let Some(ev) = self.flush_event() {
            out.push_back(ev);
        }
    }

    fn decode_utf8_chunk(&mut self, chunk: &[u8]) -> String {
        let mut combined;
        let bytes: &[u8] = if self.utf8_tail.is_empty() {
            chunk
        } else {
            combined = std::mem::take(&mut self.utf8_tail);
            combined.extend_from_slice(chunk);
            &combined
        };

        let mut decoded = String::new();
        let mut offset = 0;
        while offset < bytes.len() {
            match std::str::from_utf8(&bytes[offset..]) {
                Ok(valid) => {
                    decoded.push_str(valid);
                    offset = bytes.len();
                }
                Err(err) => {
                    let valid_up_to = err.valid_up_to();
                    if valid_up_to > 0 {
                        // SAFETY: guaranteed valid UTF-8 up to `valid_up_to`.
                        let valid = std::str::from_utf8(&bytes[offset..offset + valid_up_to])
                            .expect("valid utf-8 prefix");
                        decoded.push_str(valid);
                        offset += valid_up_to;
                    }
                    match err.error_len() {
                        Some(invalid_len) => {
                            decoded.push(char::REPLACEMENT_CHARACTER);
                            offset += invalid_len;
                        }
                        None => {
                            // Incomplete multi-byte sequence at end of chunk; keep for next push.
                            self.utf8_tail.extend_from_slice(&bytes[offset..]);
                            break;
                        }
                    }
                }
            }
        }
        decoded
    }

    fn push_str(&mut self, text: &str, out: &mut VecDeque<SseEvent>) {
        let mut chars = text.chars().peekable();
        if self.pending_cr {
            self.pending_cr = false;
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            let line = std::mem::take(&mut self.line_buf);
            self.process_line(&line, out);
        }

        while let Some(ch) = chars.next() {
            match ch {
                '\n' => {
                    let line = std::mem::take(&mut self.line_buf);
                    self.process_line(&line, out);
                }
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                        let line = std::mem::take(&mut self.line_buf);
                        self.process_line(&line, out);
                    } else if chars.peek().is_none() {
                        // Could be CRLF split across chunks; wait for next chunk or EOF.
                        self.pending_cr = true;
                    } else {
                        let line = std::mem::take(&mut self.line_buf);
                        self.process_line(&line, out);
                    }
                }
                other => self.line_buf.push(other),
            }
        }
    }

    fn process_line(&mut self, line: &str, out: &mut VecDeque<SseEvent>) {
        // Strip leading UTF-8 BOM on the very first line if present.
        let line = line.strip_prefix('\u{feff}').unwrap_or(line);
        if line.is_empty() {
            if let Some(ev) = self.flush_event() {
                out.push_back(ev);
            }
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, raw_value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event_name = Some(raw_value.to_string()),
            "data" => {
                self.data_lines.push(raw_value.to_string());
                self.has_data = true;
            }
            "id" => {
                if !raw_value.contains('\0') {
                    self.event_id = Some(raw_value.to_string());
                }
            }
            _ => {}
        }
    }

    fn flush_event(&mut self) -> Option<SseEvent> {
        if !self.has_data && self.event_name.is_none() {
            return None;
        }
        let ev = SseEvent {
            event: self.event_name.take(),
            data: self.data_lines.join("\n"),
            id: self.event_id.take(),
        };
        self.data_lines.clear();
        self.has_data = false;
        Some(ev)
    }
}

/// Stream of decoded `SseEvent` frames over a `ByteStream` with cooperative `CancelToken` wakeup.
pub struct SseStream {
    inner: ByteStream,
    cancel: CancelToken,
    cancel_fut: Pin<Box<CancelledFuture>>,
    parser: SseParser,
    queue: VecDeque<SseEvent>,
    eof: bool,
    terminated: bool,
}

impl SseStream {
    pub fn new(body: ByteStream, cancel: CancelToken) -> Self {
        let cancel_fut = Box::pin(cancel.cancelled());
        Self {
            inner: body,
            cancel,
            cancel_fut,
            parser: SseParser::new(),
            queue: VecDeque::new(),
            eof: false,
            terminated: false,
        }
    }
}

impl Stream for SseStream {
    type Item = Result<SseEvent, TransportError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }
        if self.cancel.is_cancelled() || self.cancel_fut.as_mut().poll(cx).is_ready() {
            self.terminated = true;
            return Poll::Ready(Some(Err(TransportError::Aborted)));
        }

        loop {
            if let Some(ev) = self.queue.pop_front() {
                return Poll::Ready(Some(Ok(ev)));
            }
            if self.eof {
                self.terminated = true;
                return Poll::Ready(None);
            }

            // Re-check cancellation before/after polling the underlying byte stream so the
            // cancel waker stays registered while waiting on a quiet socket/channel.
            if self.cancel_fut.as_mut().poll(cx).is_ready() {
                self.terminated = true;
                return Poll::Ready(Some(Err(TransportError::Aborted)));
            }

            let this = self.as_mut().get_mut();
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    this.parser.push_bytes(&bytes, &mut this.queue);
                }
                Poll::Ready(Some(Err(err))) => {
                    this.terminated = true;
                    return Poll::Ready(Some(Err(err)));
                }
                Poll::Ready(None) => {
                    this.eof = true;
                    this.parser.finish(&mut this.queue);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
