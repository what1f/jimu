use std::sync::{Arc, OnceLock};

use jimu_ai::{BoxFuture, CancelToken, HttpTransport};
use serde_json::Value;
use url::Url;

use super::dom::Dom;
use super::{Web2MdDocument, Web2MdError, generic, parsers};
use crate::web::http::{Download, Fetcher, PAGE_ACCEPT, ResponseHead};
use crate::web::policy::{WebFetchConfig, parse_url};

/// Returns `Ok(None)` to let the router try another parser.
pub trait Web2MdParser: Send + Sync {
    fn name(&self) -> &str;

    fn parse<'a>(
        &'a self,
        context: &'a mut ParseContext,
    ) -> BoxFuture<'a, Result<Option<Web2MdDocument>, Web2MdError>>;
}

type UrlMatcher = Arc<dyn Fn(&Url) -> bool + Send + Sync>;
type PageMatcher = Arc<dyn Fn(&Dom) -> bool + Send + Sync>;

#[derive(Clone)]
pub struct ParserRule {
    url: Option<UrlMatcher>,
    page: Option<PageMatcher>,
    parser: Arc<dyn Web2MdParser>,
    exclusive: bool,
}

impl ParserRule {
    pub fn for_url(
        matches: impl Fn(&Url) -> bool + Send + Sync + 'static,
        parser: impl Web2MdParser + 'static,
    ) -> Self {
        Self {
            url: Some(Arc::new(matches)),
            page: None,
            parser: Arc::new(parser),
            exclusive: false,
        }
    }

    pub fn for_page(
        matches: impl Fn(&Dom) -> bool + Send + Sync + 'static,
        parser: impl Web2MdParser + 'static,
    ) -> Self {
        Self {
            url: None,
            page: Some(Arc::new(matches)),
            parser: Arc::new(parser),
            exclusive: false,
        }
    }

    pub fn with_url(mut self, matches: impl Fn(&Url) -> bool + Send + Sync + 'static) -> Self {
        self.url = Some(Arc::new(matches));
        self
    }

    /// Stop routing if this rule matches but its parser declines or fails.
    pub fn exclusive(mut self) -> Self {
        self.exclusive = true;
        self
    }
}

/// One conversion, including a shared download and a lazily parsed HTML tree.
pub struct ParseContext {
    pub url: Url,
    fetcher: Fetcher,
    cancel: CancelToken,
    download: Option<Result<Download, Web2MdError>>,
    dom: Option<Dom>,
    api_head: Arc<OnceLock<ResponseHead>>,
}

impl ParseContext {
    pub async fn document(&mut self) -> Result<&Dom, Web2MdError> {
        if self.dom.is_none() {
            let page = self.download().await?;
            if !page.is_html() {
                return Err(Web2MdError::NotHtml);
            }
            self.dom = Some(Dom::parse(page.text()?, page.url.clone()));
        }
        self.dom.as_ref().ok_or(Web2MdError::NotHtml)
    }

    pub fn json(&self, url: Url) -> BoxFuture<'static, Result<Option<Value>, Web2MdError>> {
        let fetcher = self.fetcher.clone();
        let cancel = self.cancel.clone();
        let head = self.api_head.clone();
        Box::pin(async move {
            match fetcher.get(url, "application/json", &cancel).await {
                Ok(data) => {
                    let value = data
                        .text()
                        .ok()
                        .and_then(|text| serde_json::from_str(text).ok());
                    if value.is_some() {
                        head.get_or_init(|| data.head());
                    }
                    Ok(value)
                }
                Err(Web2MdError::HttpStatus(_)) => Ok(None),
                Err(error) => Err(error),
            }
        })
    }

    async fn download(&mut self) -> Result<&Download, Web2MdError> {
        if self.download.is_none() {
            self.download = Some(
                self.fetcher
                    .get(self.url.clone(), PAGE_ACCEPT, &self.cancel)
                    .await,
            );
        }
        match self.download.as_ref() {
            Some(Ok(download)) => Ok(download),
            Some(Err(error)) => Err(error.clone()),
            None => Err(Web2MdError::Network),
        }
    }
}

#[derive(Clone)]
pub struct Web2Md {
    pub(crate) fetcher: Fetcher,
    rules: Vec<ParserRule>,
}

pub(crate) enum Routed {
    Document(Box<Web2MdDocument>, Option<ResponseHead>),
    Other(Download),
}

impl Web2Md {
    /// The transport must return redirect responses for requests bearing
    /// `x-no-redirect`, so each destination can be checked before it is contacted.
    /// Hosts should use the same address policy when connecting or route requests through
    /// a trusted proxy that enforces it. A preflight DNS lookup does not pin the connection.
    pub fn new(transport: Arc<dyn HttpTransport>, config: WebFetchConfig) -> Self {
        Self {
            fetcher: Fetcher {
                transport,
                config,
                private_origin: None,
            },
            rules: vec![parsers::x::rule()],
        }
    }

    /// Replace the built-in site rules.
    pub fn with_rules(mut self, rules: Vec<ParserRule>) -> Self {
        self.rules = rules;
        self
    }

    /// Register a rule ahead of the built-in rules of the same stage.
    pub fn register(&mut self, rule: ParserRule) {
        self.rules.insert(0, rule);
    }

    pub fn parser_name_for_url(&self, url: &Url) -> Option<&str> {
        self.rules
            .iter()
            .find(|rule| rule.page.is_none() && rule.url.as_ref().is_some_and(|test| test(url)))
            .map(|rule| rule.parser.name())
    }

    pub async fn convert(
        &self,
        url: &str,
        cancel: CancelToken,
    ) -> Result<Web2MdDocument, Web2MdError> {
        match self.route(parse_url(url)?, cancel, None).await? {
            Routed::Document(document, _) => Ok(*document),
            Routed::Other(_) => Err(Web2MdError::NotHtml),
        }
    }

    pub async fn convert_html(
        &self,
        html: &str,
        url: &str,
        cancel: CancelToken,
    ) -> Result<Web2MdDocument, Web2MdError> {
        let url = parse_url(url)?;
        let download = Download {
            url: url.clone(),
            status: 200,
            mime: "text/html".into(),
            bytes: html.as_bytes().to_vec(),
        };
        match self.route(url, cancel, Some(download)).await? {
            Routed::Document(document, _) => Ok(*document),
            Routed::Other(_) => Err(Web2MdError::NotHtml),
        }
    }

    pub(crate) async fn route(
        &self,
        url: Url,
        cancel: CancelToken,
        supplied: Option<Download>,
    ) -> Result<Routed, Web2MdError> {
        let offline = supplied.is_some();
        let mut context = ParseContext {
            url,
            fetcher: self.fetcher.clone(),
            cancel,
            download: supplied.map(Ok),
            dom: None,
            api_head: Arc::default(),
        };
        let mut last_error = None;
        let document = self.extract(&mut context, offline, &mut last_error).await;
        if context.cancel.is_cancelled() {
            return Err(Web2MdError::Cancelled);
        }
        let download = context.download.and_then(Result::ok);
        match document {
            Ok(Some(document)) => {
                let head = download.as_ref().map(Download::head).or_else(|| {
                    let head = context.api_head.get()?;
                    Some(ResponseHead {
                        url: document.url.clone(),
                        status: head.status,
                        mime: head.mime.clone(),
                    })
                });
                Ok(Routed::Document(Box::new(document), head))
            }
            Ok(None) => download
                .filter(|page| !page.is_html())
                .map(Routed::Other)
                .ok_or_else(|| last_error.unwrap_or(Web2MdError::NoContent)),
            Err(error) => Err(error),
        }
    }

    async fn extract(
        &self,
        context: &mut ParseContext,
        offline: bool,
        last_error: &mut Option<Web2MdError>,
    ) -> Result<Option<Web2MdDocument>, Web2MdError> {
        if !offline {
            for rule in self.rules.iter().filter(|rule| rule.page.is_none()) {
                if rule.url.as_ref().is_some_and(|test| test(&context.url))
                    && let Some(document) = attempt(rule, context, last_error).await?
                {
                    return Ok(Some(document));
                }
            }
        }
        let (final_url, is_html) = {
            let download = context.download().await?;
            (download.url.clone(), download.is_html())
        };
        if final_url != context.url {
            context.url = final_url;
            if !offline {
                for rule in self.rules.iter().filter(|rule| rule.page.is_none()) {
                    if rule.url.as_ref().is_some_and(|test| test(&context.url))
                        && let Some(document) = attempt(rule, context, last_error).await?
                    {
                        return Ok(Some(document));
                    }
                }
            }
        }
        if !is_html {
            return Ok(None);
        }
        for rule in &self.rules {
            if let Some(test) = &rule.page
                && rule.url.as_ref().is_none_or(|test| test(&context.url))
                && test(context.document().await?)
                && let Some(document) = attempt(rule, context, last_error).await?
            {
                return Ok(Some(document));
            }
        }
        let dom = context.document().await?;
        Ok(generic::extract(dom))
    }
}

async fn attempt(
    rule: &ParserRule,
    context: &mut ParseContext,
    last_error: &mut Option<Web2MdError>,
) -> Result<Option<Web2MdDocument>, Web2MdError> {
    match rule.parser.parse(context).await {
        Ok(Some(document)) if !document.markdown.trim().is_empty() => Ok(Some(document)),
        Err(error) if rule.exclusive || matches!(error, Web2MdError::Cancelled) => Err(error),
        Ok(_) if rule.exclusive => Err(Web2MdError::NoContent),
        Err(error) => {
            *last_error = Some(error);
            Ok(None)
        }
        Ok(_) => Ok(None),
    }
}
