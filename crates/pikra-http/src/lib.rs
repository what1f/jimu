//! Reqwest/rustls streaming `HttpTransport` implementation for native Rust hosts.

use bytes::Bytes;
use futures_core::Stream;
use pikra_ai::{
    BoxFuture, ByteStream, CancelToken, CancelledFuture, HttpRequest, HttpStreamResponse,
    HttpTransport, TransportError,
};
use reqwest::header::{HeaderName, HeaderValue};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const TRUNCATED_SUFFIX: &str = "...[truncated]";

fn decode_capped_utf8(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(err) if err.error_len().is_none() => {
            String::from_utf8_lossy(&bytes[..err.valid_up_to()]).into_owned()
        }
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Native HTTP streaming transport backed by `reqwest` + `rustls` with no disk cache.
#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Build a new `ReqwestTransport` with default rustls settings (no disk cache)
    /// and a strict same-origin redirect policy so API credentials never leak cross-origin.
    pub fn new() -> Result<Self, reqwest::Error> {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many redirects");
            }
            if let Some(first) = attempt.previous().first() {
                let next = attempt.url();
                let same_origin = first.scheme() == next.scheme()
                    && first.host_str() == next.host_str()
                    && first.port_or_known_default() == next.port_or_known_default();
                if !same_origin {
                    return attempt.error("cross-origin redirect refused for credential safety");
                }
            }
            attempt.follow()
        });
        let client = reqwest::Client::builder().redirect(policy).build()?;
        Ok(Self { client })
    }

    /// Wrap a pre-configured `reqwest::Client`.
    pub fn from_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

struct CancellableByteStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    cancel: CancelToken,
    cancel_fut: Pin<Box<CancelledFuture>>,
    terminated: bool,
}

impl Stream for CancellableByteStream {
    type Item = Result<Bytes, TransportError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }
        if self.cancel.is_cancelled() || self.cancel_fut.as_mut().poll(cx).is_ready() {
            self.terminated = true;
            return Poll::Ready(Some(Err(TransportError::Aborted)));
        }

        let this = self.as_mut().get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => Poll::Ready(Some(Ok(bytes))),
            Poll::Ready(Some(Err(err))) => {
                this.terminated = true;
                let mapped = if err.is_timeout() {
                    TransportError::Timeout
                } else {
                    TransportError::Network(err.to_string())
                };
                Poll::Ready(Some(Err(mapped)))
            }
            Poll::Ready(None) => {
                this.terminated = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl HttpTransport for ReqwestTransport {
    fn send(
        &self,
        request: HttpRequest,
        cancel: CancelToken,
    ) -> BoxFuture<'_, Result<HttpStreamResponse, TransportError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(TransportError::Aborted);
            }

            let method = reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|e| TransportError::Network(format!("invalid HTTP method: {e}")))?;

            let mut builder = self.client.request(method, &request.url);
            if let Some(timeout_ms) = request.timeout_ms {
                builder = builder.timeout(Duration::from_millis(timeout_ms));
            }
            for (name, value) in &request.headers {
                let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                    TransportError::Network(format!("invalid header name `{name}`: {e}"))
                })?;
                let header_value = HeaderValue::from_str(value).map_err(|e| {
                    TransportError::Network(format!("invalid header value for `{name}`: {e}"))
                })?;
                builder = builder.header(header_name, header_value);
            }
            if !request.body.is_empty() {
                builder = builder.body(request.body);
            }

            let response = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(TransportError::Aborted),
                res = builder.send() => res.map_err(|err| {
                    if err.is_timeout() {
                        TransportError::Timeout
                    } else {
                        TransportError::Network(err.to_string())
                    }
                })?,
            };

            let status = response.status().as_u16();
            let headers: Vec<(String, String)> = response
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();

            if !response.status().is_success() {
                let mut response = response;
                let mut buf = Vec::new();
                let mut truncated = false;
                loop {
                    let next = tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Err(TransportError::Aborted),
                        res = response.chunk() => res,
                    };
                    match next {
                        Ok(Some(chunk)) => {
                            let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(buf.len());
                            if chunk.len() > remaining {
                                buf.extend_from_slice(&chunk[..remaining]);
                                truncated = true;
                                break;
                            }
                            buf.extend_from_slice(&chunk);
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
                let mut body = decode_capped_utf8(&buf);
                if truncated {
                    body.push_str(TRUNCATED_SUFFIX);
                }
                return Err(TransportError::HttpStatus {
                    status,
                    headers,
                    body,
                });
            }

            let cancel_fut = Box::pin(cancel.cancelled());
            let body: ByteStream = Box::pin(CancellableByteStream {
                inner: Box::pin(response.bytes_stream()),
                cancel,
                cancel_fut,
                terminated: false,
            });

            Ok(HttpStreamResponse {
                status,
                headers,
                body,
            })
        })
    }
}
