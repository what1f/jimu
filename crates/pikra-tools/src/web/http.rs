use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use pikra_ai::{CancelToken, HttpRequest, HttpTransport, TransportError};
use url::Url;

use super::policy::{WebFetchConfig, check_destination};
use super::web2md::Web2MdError;

const MAX_REDIRECTS: usize = 5;
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);
pub(crate) const PAGE_ACCEPT: &str =
    "text/html,application/xhtml+xml,application/json,text/plain,text/markdown,image/*,*/*;q=0.8";

#[derive(Clone)]
pub(crate) struct Fetcher {
    pub transport: Arc<dyn HttpTransport>,
    pub config: WebFetchConfig,
}

pub(crate) struct Download {
    pub url: Url,
    pub status: u16,
    pub mime: String,
    pub bytes: Vec<u8>,
}

pub(crate) struct ResponseHead {
    pub url: Url,
    pub status: u16,
    pub mime: String,
}

impl Download {
    pub fn head(&self) -> ResponseHead {
        ResponseHead {
            url: self.url.clone(),
            status: self.status,
            mime: self.mime.clone(),
        }
    }

    pub fn text(&self) -> Result<&str, Web2MdError> {
        std::str::from_utf8(&self.bytes)
            .map(|text| text.strip_prefix('\u{feff}').unwrap_or(text))
            .map_err(|_| Web2MdError::Decoding)
    }

    pub fn is_html(&self) -> bool {
        matches!(self.mime.as_str(), "text/html" | "application/xhtml+xml")
            || (self.mime.is_empty()
                && self.text().is_ok_and(|text| {
                    let start = text.trim_start();
                    ["<!doctype html", "<html", "<head", "<body"]
                        .iter()
                        .any(|prefix| {
                            start
                                .get(..prefix.len())
                                .is_some_and(|s| s.eq_ignore_ascii_case(prefix))
                        })
                }))
    }

    pub fn is_image(&self) -> bool {
        is_image(&self.mime)
    }
}

impl Fetcher {
    pub async fn get(
        &self,
        url: Url,
        accept: &str,
        cancel: &CancelToken,
    ) -> Result<Download, Web2MdError> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Web2MdError::Cancelled),
            result = tokio::time::timeout(TIMEOUT, self.download(url, accept, cancel)) => {
                result.unwrap_or(Err(Web2MdError::Timeout))
            }
        }
    }

    async fn download(
        &self,
        mut url: Url,
        accept: &str,
        cancel: &CancelToken,
    ) -> Result<Download, Web2MdError> {
        for hop in 0..=MAX_REDIRECTS {
            check_destination(&url, &self.config).await?;
            url.set_fragment(None);
            let request = HttpRequest {
                method: "GET".into(),
                url: url.to_string(),
                headers: vec![
                    ("x-pikra-no-redirect".into(), "1".into()),
                    ("accept".into(), accept.into()),
                    (
                        "user-agent".into(),
                        concat!("Pikra/", env!("CARGO_PKG_VERSION")).into(),
                    ),
                ],
                body: Default::default(),
                timeout_ms: Some(TIMEOUT.as_millis() as u64),
            };
            let response = self.transport.send(request, cancel.clone()).await;
            let (status, headers) = match &response {
                Ok(response) => (response.status, &response.headers),
                Err(TransportError::HttpStatus {
                    status, headers, ..
                }) => (*status, headers),
                Err(TransportError::Aborted) => return Err(Web2MdError::Cancelled),
                Err(TransportError::Timeout) => return Err(Web2MdError::Timeout),
                Err(TransportError::Network(_)) => return Err(Web2MdError::Network),
            };
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if hop == MAX_REDIRECTS {
                    return Err(Web2MdError::TooManyRedirects);
                }
                let location = header(headers, "location")
                    .filter(|location| !location.trim().is_empty())
                    .ok_or(Web2MdError::InvalidRedirect)?;
                url = url
                    .join(location)
                    .map_err(|_| Web2MdError::InvalidRedirect)?;
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(Web2MdError::HttpStatus(status));
            }
            let mime = header(headers, "content-type")
                .unwrap_or_default()
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            let limit = if is_image(&mime) {
                self.config.max_download_bytes.min(MAX_IMAGE_BYTES)
            } else {
                self.config.max_download_bytes
            };
            if header(headers, "content-length")
                .and_then(|length| length.parse::<u64>().ok())
                .is_some_and(|length| length > limit as u64)
            {
                return Err(Web2MdError::TooLarge);
            }

            let mut stream = response.map_err(|_| Web2MdError::HttpStatus(status))?.body;
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| match error {
                    TransportError::Aborted => Web2MdError::Cancelled,
                    TransportError::Timeout => Web2MdError::Timeout,
                    _ => Web2MdError::Network,
                })?;
                if chunk.len() > limit - bytes.len() {
                    return Err(Web2MdError::TooLarge);
                }
                bytes.extend_from_slice(&chunk);
            }
            return Ok(Download {
                url,
                status,
                mime,
                bytes,
            });
        }
        Err(Web2MdError::TooManyRedirects)
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn is_image(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}
