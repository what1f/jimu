// X parser adapted from web2md-swift. See ../../LICENSE-MIT.

mod article;
mod media;
mod post;
mod rich_text;

use std::collections::HashMap;
use std::ops::Range;
use std::sync::LazyLock;

use jimu_ai::BoxFuture;
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
        && let Some(formatted) = format_unix_seconds_utc(timestamp)
    {
        return formatted;
    }
    let raw = string(&status["created_at"]);
    parse_twitter_date(raw)
        .or_else(|| parse_rfc3339_utc(raw))
        .unwrap_or_else(|| raw.to_owned())
}

fn iso_date(raw: &str) -> String {
    parse_rfc3339_utc(raw).unwrap_or_else(|| raw.to_owned())
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let m = u64::from(month);
    let d = u64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe as i64 - 719_468)
}

fn format_unix_seconds_utc(secs: i64) -> Option<String> {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days.checked_add(719_468)?;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(m <= 2);
    if !(0..=9999).contains(&year) {
        return None;
    }
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;
    Some(format!(
        "{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

fn parse_hms(raw: &str) -> Option<(i64, i64, i64)> {
    let mut parts = raw.split(':');
    let h: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let s: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some()
        || !(0..=23).contains(&h)
        || !(0..=59).contains(&m)
        || !(0..=59).contains(&s)
    {
        return None;
    }
    Some((h, m, s))
}

fn parse_twitter_date(raw: &str) -> Option<String> {
    // Example: "Wed Oct 10 20:19:24 +0000 2018"
    let parts: Vec<&str> = raw.split_whitespace().collect();
    if parts.len() != 6 {
        return None;
    }
    let month = match parts[1] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let day: u32 = parts[2].parse().ok()?;
    let (h, m, s) = parse_hms(parts[3])?;
    let tz = parts[4];
    if tz.len() != 5 {
        return None;
    }
    let sign: i64 = match &tz[0..1] {
        "+" => 1,
        "-" => -1,
        _ => return None,
    };
    let tz_h: i64 = tz[1..3].parse().ok()?;
    let tz_m: i64 = tz[3..5].parse().ok()?;
    if tz_h > 23 || tz_m > 59 {
        return None;
    }
    let year: i64 = parts[5].parse().ok()?;
    let days = days_from_civil(year, month, day)?;
    let offset = sign * (tz_h * 3_600 + tz_m * 60);
    let secs = days.checked_mul(86_400)? + h * 3_600 + m * 60 + s - offset;
    format_unix_seconds_utc(secs)
}

fn parse_rfc3339_utc(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if !matches!(bytes[10], b'T' | b't' | b' ') {
        return None;
    }
    let year: i64 = raw.get(0..4)?.parse().ok()?;
    let month: u32 = raw.get(5..7)?.parse().ok()?;
    let day: u32 = raw.get(8..10)?.parse().ok()?;
    let (h, m, s) = parse_hms(raw.get(11..19)?)?;
    let mut rest = raw.get(19..)?;
    if let Some(after_dot) = rest.strip_prefix('.') {
        let frac_len = after_dot.bytes().take_while(|b| b.is_ascii_digit()).count();
        if frac_len == 0 {
            return None;
        }
        rest = &after_dot[frac_len..];
    }
    let offset = if matches!(rest, "Z" | "z") {
        0
    } else if rest.len() == 6 && rest.as_bytes()[3] == b':' {
        let sign: i64 = match &rest[0..1] {
            "+" => 1,
            "-" => -1,
            _ => return None,
        };
        let tz_h: i64 = rest[1..3].parse().ok()?;
        let tz_m: i64 = rest[4..6].parse().ok()?;
        if tz_h > 23 || tz_m > 59 {
            return None;
        }
        sign * (tz_h * 3_600 + tz_m * 60)
    } else {
        return None;
    };
    let days = days_from_civil(year, month, day)?;
    let secs = days.checked_mul(86_400)? + h * 3_600 + m * 60 + s - offset;
    format_unix_seconds_utc(secs)
}
