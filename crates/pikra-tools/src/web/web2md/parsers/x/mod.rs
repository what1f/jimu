// X parser adapted from web2md-swift. See ../../LICENSE-MIT.

mod article;
mod media;
mod post;
mod rich_text;

use std::collections::HashMap;
use std::ops::Range;
use std::sync::LazyLock;

use chrono::{DateTime, SecondsFormat};
use pikra_ai::BoxFuture;
use serde_json::Value;
use url::Url;

use crate::web::web2md::dom::resolve;
use crate::web::web2md::markdown::image;
use crate::web::web2md::{ParseContext, ParserRule, Web2MdDocument, Web2MdError, Web2MdParser};

const HOSTS: &[&str] = &[
    "x.com",
    "twitter.com",
    "fxtwitter.com",
    "fixupx.com",
    "vxtwitter.com",
    "fixvx.com",
    "twittpr.com",
    "nitter.net",
    "xcancel.com",
];
static X: LazyLock<Url> = LazyLock::new(|| Url::parse("https://x.com/").expect("valid X URL"));
static API: LazyLock<Url> =
    LazyLock::new(|| Url::parse("https://api.fxtwitter.com/").expect("valid FxTwitter URL"));

struct XParser;

pub(in crate::web::web2md) fn rule() -> ParserRule {
    ParserRule::for_url(
        |url| {
            url.host_str().is_some_and(|host| {
                let host = host.trim_end_matches('.');
                HOSTS.iter().any(|domain| {
                    host == *domain
                        || host
                            .strip_suffix(domain)
                            .is_some_and(|prefix| prefix.ends_with('.'))
                })
            }) && reference(url).is_some()
        },
        XParser,
    )
    .exclusive()
}

impl Web2MdParser for XParser {
    fn name(&self) -> &str {
        "x"
    }

    fn parse<'a>(
        &'a self,
        context: &'a mut ParseContext,
    ) -> BoxFuture<'a, Result<Option<Web2MdDocument>, Web2MdError>> {
        Box::pin(async move {
            let Some((user, post_id)) =
                reference(&context.url).map(|(user, id)| (user.to_owned(), id.to_owned()))
            else {
                return Ok(None);
            };
            let thread = recover(request(context, &format!("2/thread/{post_id}")).await)?;
            let (status, thread) =
                if let Some(response) = thread.filter(|response| available(&response["status"])) {
                    let mut response = response;
                    let status = response["status"].take();
                    let thread = response["thread"].take();
                    (status, thread)
                } else {
                    let status = status(context, &user, &post_id)
                        .await?
                        .ok_or(Web2MdError::PostUnavailable)?;
                    (status, Value::Null)
                };
            let author = &status["author"];
            let handle = string(&author["screen_name"]);
            let canonical = status["url"]
                .as_str()
                .and_then(|url| Url::parse(url).ok())
                .filter(|url| {
                    matches!(url.scheme(), "http" | "https")
                        && url.username().is_empty()
                        && url.password().is_none()
                })
                .unwrap_or_else(|| context.url.clone());
            let mut document = Web2MdDocument::new(canonical, String::new(), self.name());
            document.author = author["name"].as_str().unwrap_or(handle).to_owned();
            document.site = "X".into();
            document.published = date(&status);
            document.language = status["lang"]
                .as_str()
                .filter(|lang| !matches!(*lang, "und" | "zxx" | "qme" | "qam"))
                .unwrap_or_default()
                .to_owned();
            document.favicon = "https://abs.twimg.com/favicons/twitter.3.ico".into();
            document
                .extras
                .insert("authorHandle".into(), handle.to_owned());
            document
                .extras
                .insert("postID".into(), id(&status["id"]).unwrap_or(post_id));
            if let Some(avatar) = author["avatar_url"].as_str().and_then(web_url) {
                document.extras.insert("authorAvatar".into(), avatar);
            }

            let raw_article = &status["article"];
            if raw_article.is_object()
                && (raw_article["content"]["blocks"].is_array() || raw_article["title"].is_string())
            {
                render_article(context, raw_article, &status, &mut document).await?;
            } else {
                let author_id = id(&author["id"]);
                let mut rendered: Vec<_> = items(&thread)
                    .iter()
                    .filter(|item| {
                        available(item)
                            && (author_id.is_none() || id(&item["author"]["id"]) == author_id)
                    })
                    .collect();
                if rendered.is_empty() {
                    rendered.push(&status);
                }
                let first = rendered[0];
                document.title = post::title(first);
                document.description = post::plain_text(first).chars().take(300).collect();
                document.image = post::first_image(first).unwrap_or_default();
                document.extras.insert(
                    "type".into(),
                    if rendered.len() > 1 { "thread" } else { "post" }.into(),
                );
                document.markdown = rendered
                    .into_iter()
                    .map(|post| post::body(post, true))
                    .filter(|body| !body.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n");
            }
            document.word_count = document.markdown.split_whitespace().count();
            Ok(Some(document))
        })
    }
}

async fn render_article(
    context: &mut ParseContext,
    raw: &Value,
    status_value: &Value,
    document: &mut Web2MdDocument,
) -> Result<(), Web2MdError> {
    let article = article::Article::new(raw);
    let mut embeds = HashMap::new();
    // Unresolved or excess embeds retain their original post links.
    for id in article.embedded_posts().into_iter().take(24) {
        if let Some(post) = status(context, "i", &id).await? {
            embeds.insert(id, post::quote_post(&post));
        }
    }
    document.title = raw["title"]
        .as_str()
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| post::title(status_value));
    if let Some(created) = raw["created_at"].as_str() {
        document.published = iso_date(created);
    }
    document.description = string(&raw["preview_text"]).trim().to_owned();
    let cover = &raw["cover_media"];
    document.image = cover["media_info"]["original_img_url"]
        .as_str()
        .or_else(|| cover["original_img_url"].as_str())
        .and_then(web_url)
        .unwrap_or_default();
    document.extras.insert("type".into(), "article".into());
    if let Some(id) = id(&raw["id"]) {
        document.extras.insert("articleID".into(), id);
    }
    for (field, key) in [
        ("original_img_width", "imageWidth"),
        ("original_img_height", "imageHeight"),
    ] {
        if let Some(value) = id(&cover["media_info"][field]) {
            document.extras.insert(key.into(), value);
        }
    }
    let mut body = article.render(&embeds);
    if body.is_empty() {
        body = crate::web::web2md::markdown::escape(&document.description);
    }
    document.markdown = if document.image.is_empty() {
        body
    } else {
        format!("{}\n\n{body}", image("", &document.image))
            .trim_end()
            .to_owned()
    };
    Ok(())
}

fn reference(url: &Url) -> Option<(&str, &str)> {
    let mut parts = url.path_segments()?;
    let user = parts.next()?;
    let mut kind = parts.next()?;
    if user.eq_ignore_ascii_case("i") && kind.eq_ignore_ascii_case("web") {
        kind = parts.next()?;
    }
    let id = parts.next()?;
    if !matches!(
        kind.to_ascii_lowercase().as_str(),
        "status" | "statuses" | "article"
    ) || !is_post_id(id)
        || !(1..=20).contains(&user.len())
        || !user.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        return None;
    }
    Some((user, id))
}

fn is_post_id(id: &str) -> bool {
    (1..=25).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit())
}

fn request(
    context: &ParseContext,
    path: &str,
) -> BoxFuture<'static, Result<Option<Value>, Web2MdError>> {
    match API.join(path) {
        Ok(url) => context.json(url),
        Err(_) => Box::pin(async { Err(Web2MdError::InvalidUrl) }),
    }
}

fn available(value: &Value) -> bool {
    value.is_object()
        && (value["id"].is_string() || value["id"].is_number())
        && value["type"] != "tombstone"
}

fn recover(result: Result<Option<Value>, Web2MdError>) -> Result<Option<Value>, Web2MdError> {
    match result {
        // API availability can differ between versions. Policy failures are final.
        Err(Web2MdError::Network | Web2MdError::Timeout) => Ok(None),
        other => other,
    }
}

async fn status(
    context: &mut ParseContext,
    user: &str,
    id: &str,
) -> Result<Option<Value>, Web2MdError> {
    if let Some(mut response) = recover(request(context, &format!("2/status/{id}")).await)?
        && available(&response["status"])
    {
        return Ok(Some(response["status"].take()));
    }
    Ok(
        recover(request(context, &format!("{user}/status/{id}")).await)?.and_then(
            |mut response| available(&response["tweet"]).then(|| response["tweet"].take()),
        ),
    )
}

fn items(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn string(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn id(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|value| value.to_string()))
}

fn span(from: &Value, to: &Value) -> Option<Range<usize>> {
    let start = usize::try_from(from.as_u64()?).ok()?;
    let end = usize::try_from(to.as_u64()?).ok()?;
    (start < end).then_some(start..end)
}

fn web_url(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else if value.starts_with('/')
        || value
            .split('/')
            .next()
            .is_some_and(|part| part.contains(':'))
    {
        resolve(&X, value, false)
    } else {
        resolve(&X, &format!("https://{value}"), false)
    }
}

fn data_link(kind: &str, data: &Value, text: &str) -> Option<String> {
    match kind {
        "url" | "urls" => data["replacement"]
            .as_str()
            .or_else(|| data["expanded"].as_str())
            .or_else(|| data["url"].as_str())
            .or_else(|| data["expanded_url"].as_str())
            .or_else(|| data["original"].as_str())
            .or(Some(text))
            .and_then(web_url),
        "mention" | "mentions" => route_segment("", text.trim_start_matches('@')),
        "hashtag" | "hashtags" => route_segment("hashtag", text.trim_start_matches(['#', '＃'])),
        "cashtag" | "cashtags" | "symbol" => {
            let mut url = X.join("search").ok()?;
            url.query_pairs_mut()
                .append_pair("q", &format!("${}", text.trim_start_matches('$')));
            Some(url.into())
        }
        _ => None,
    }
}

fn route_segment(prefix: &str, value: &str) -> Option<String> {
    if value.is_empty() {
        return None;
    }
    let mut url = X.clone();
    let mut path = url.path_segments_mut().ok()?;
    path.pop_if_empty();
    if !prefix.is_empty() {
        path.push(prefix);
    }
    path.push(value);
    drop(path);
    Some(url.into())
}

fn date(status: &Value) -> String {
    if let Some(timestamp) = status["created_timestamp"].as_i64()
        && let Some(date) = DateTime::from_timestamp(timestamp, 0)
    {
        return date.to_rfc3339_opts(SecondsFormat::Secs, true);
    }
    let raw = string(&status["created_at"]);
    DateTime::parse_from_str(raw, "%a %b %d %H:%M:%S %z %Y")
        .map(|date| date.to_utc().to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|_| iso_date(raw))
}

fn iso_date(raw: &str) -> String {
    DateTime::parse_from_rfc3339(raw)
        .map(|date| date.to_utc().to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|_| raw.to_owned())
}
