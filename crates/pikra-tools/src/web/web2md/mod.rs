//! Convert downloaded pages to Markdown with site-specific rules and a generic extractor.

pub mod dom;
mod generic;
mod markdown;
mod parsers;
mod routing;

pub(crate) use routing::Routed;
pub use routing::{ParseContext, ParserRule, Web2Md, Web2MdParser};

use std::collections::BTreeMap;
use url::Url;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Web2MdDocument {
    pub url: Url,
    pub title: String,
    pub author: String,
    pub site: String,
    pub published: String,
    pub description: String,
    pub image: String,
    pub language: String,
    pub favicon: String,
    pub markdown: String,
    pub html: Option<String>,
    pub word_count: usize,
    pub parser: String,
    pub extras: BTreeMap<String, String>,
}

impl Web2MdDocument {
    pub fn new(url: Url, markdown: String, parser: impl Into<String>) -> Self {
        Self {
            url,
            title: String::new(),
            author: String::new(),
            site: String::new(),
            published: String::new(),
            description: String::new(),
            image: String::new(),
            language: String::new(),
            favicon: String::new(),
            word_count: markdown.split_whitespace().count(),
            markdown,
            html: None,
            parser: parser.into(),
            extras: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum Web2MdError {
    #[error("Enter a valid HTTP or HTTPS address without a username or password")]
    InvalidUrl,
    #[error("This address is not public")]
    PrivateAddress,
    #[error("Could not find the website")]
    Dns,
    #[error("Could not connect to the website")]
    Network,
    #[error("The request timed out")]
    Timeout,
    #[error("The request was canceled")]
    Cancelled,
    #[error("The website returned HTTP {0}")]
    HttpStatus(u16),
    #[error("The website sent an invalid redirect")]
    InvalidRedirect,
    #[error("The website redirected too many times")]
    TooManyRedirects,
    #[error("The response is too large")]
    TooLarge,
    #[error("The response is not valid UTF-8")]
    Decoding,
    #[error("The response is not a web page")]
    NotHtml,
    #[error("The page has no readable content")]
    NoContent,
    #[error("This post is unavailable")]
    PostUnavailable,
}
