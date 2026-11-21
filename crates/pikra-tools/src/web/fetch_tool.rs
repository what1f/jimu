use std::fmt::Write;
use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use pikra_agent::{AgentTool, AgentToolResult, ToolProgressCallback};
use pikra_ai::{BoxFuture, CancelToken, ContentPart, HttpTransport, ImageContent, ToolDefinition};
use pikra_schema::{
    bool_schema, int_schema, obj_schema, str_schema, with_bounds, with_description,
};
use serde_json::{Value, json};

use super::http::{Download, PAGE_ACCEPT, ResponseHead};
use super::policy::{WebFetchConfig, parse_url};
use super::web2md::Routed;
use super::web2md::Web2Md;
use crate::truncate_output;

/// Fetch public URLs and return readable Markdown, text, or an image.
#[derive(Clone)]
pub struct WebFetchTool {
    web2md: Web2Md,
}

impl WebFetchTool {
    /// See [`Web2Md::new`] for the transport's redirect contract.
    pub fn new(transport: Arc<dyn HttpTransport>, config: WebFetchConfig) -> Self {
        Self::with_web2md(Web2Md::new(transport, config))
    }

    pub fn with_web2md(web2md: Web2Md) -> Self {
        Self { web2md }
    }
}

impl AgentTool for WebFetchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_fetch".into(),
            description: "Fetch a web page, text document, or image from a URL.".into(),
            parameters: obj_schema([
                (
                    "url",
                    with_description(str_schema(), "HTTP or HTTPS URL."),
                    true,
                ),
                (
                    "max_bytes",
                    with_description(
                        with_bounds(int_schema(), Some(0.0), None),
                        "Maximum bytes of text to return.",
                    ),
                    false,
                ),
                (
                    "raw",
                    with_description(bool_schema(), "Return the original page as text."),
                    false,
                ),
            ]),
        }
    }

    fn execute(
        &self,
        _tool_call_id: String,
        args: Value,
        cancel: CancelToken,
        _on_update: ToolProgressCallback,
    ) -> BoxFuture<'_, Result<AgentToolResult, String>> {
        Box::pin(async move {
            let input = args["url"].as_str().ok_or("Enter an HTTP or HTTPS URL")?;
            let url = parse_url(input).map_err(|error| error.to_string())?;
            let cap = self.web2md.fetcher.config.max_output_bytes;
            let max = match &args["max_bytes"] {
                Value::Null => cap,
                value => value
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or("Enter a non-negative whole number for max_bytes")?
                    .min(cap),
            };
            let raw = match &args["raw"] {
                Value::Null => false,
                value => value.as_bool().ok_or("Use true or false for raw")?,
            };

            let routed = if raw {
                self.web2md
                    .fetcher
                    .get(url, PAGE_ACCEPT, &cancel)
                    .await
                    .map(Routed::Other)
            } else {
                self.web2md.route(url, cancel, None).await
            }
            .map_err(|error| error.to_string())?;

            match routed {
                Routed::Other(download) => response(download, max),
                Routed::Document(document, head) => {
                    let mut text = String::new();
                    if !document.title.is_empty() {
                        let _ = writeln!(text, "Title: {}", document.title);
                    }
                    let _ = writeln!(text, "URL: {}", document.url);
                    if !document.author.is_empty() {
                        let _ = writeln!(text, "Author: {}", document.author);
                    }
                    if !document.published.is_empty() {
                        let _ = writeln!(text, "Published: {}", document.published);
                    }
                    text.push('\n');
                    text.push_str(&document.markdown);
                    let truncated = truncate_output(&text, max);
                    let mut result = AgentToolResult::text(truncated.text);
                    let mut details = head.as_ref().map(response_details).unwrap_or_else(|| {
                        json!({
                            "final_url": document.url.as_str(),
                            "status": null,
                            "content_type": null,
                        })
                    });
                    details["title"] = json!(document.title);
                    details["parser"] = json!(document.parser);
                    details["truncated"] = json!(truncated.truncated);
                    result.details = details;
                    Ok(result)
                }
            }
        })
    }
}

fn response_details(head: &ResponseHead) -> Value {
    json!({
        "final_url": head.url.as_str(),
        "status": head.status,
        "content_type": head.mime,
        "title": null,
        "parser": null,
        "truncated": false,
    })
}

fn response(download: Download, max: usize) -> Result<AgentToolResult, String> {
    let details = response_details(&download.head());
    if download.is_image() {
        return Ok(AgentToolResult {
            content: vec![ContentPart::Image(ImageContent {
                data: STANDARD.encode(download.bytes),
                mime_type: download.mime,
            })],
            details,
            structured_content: None,
            is_error: false,
            terminate: false,
        });
    }
    let text = download.text().map_err(|error| error.to_string())?;
    let truncated = truncate_output(text, max);
    let mut result = AgentToolResult::text(truncated.text);
    result.details = details;
    result.details["truncated"] = json!(truncated.truncated);
    Ok(result)
}
