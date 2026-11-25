use serde_json::Value;

use super::rich_text::{RichText, Style, scalar_offsets};
use super::{data_link, id, items, media, span, string, web_url};
use crate::web::web2md::markdown::{escape, link, quote};

pub(super) fn body(status: &Value, include_quote: bool) -> String {
    let mut parts = Vec::new();
    let text = rich_text(status);
    if !text.is_empty() {
        parts.push(text);
    }
    if (!include_quote || parts.is_empty())
        && let Some(title) = status["article"]["title"]
            .as_str()
            .map(str::trim)
            .filter(|title| !title.is_empty())
    {
        let url = status["url"].as_str().and_then(web_url).unwrap_or_default();
        parts.push(format!("**{}**", link(&escape(title), &url)));
    }

    let attached = items(&status["media"]["all"]);
    if attached.is_empty() {
        for (key, kind) in [("photos", "photo"), ("videos", "video"), ("gifs", "gif")] {
            parts.extend(
                items(&status["media"][key])
                    .iter()
                    .filter_map(|item| media::post(item, kind)),
            );
        }
    } else {
        parts.extend(
            attached
                .iter()
                .filter_map(|item| media::post(item, "photo")),
        );
    }

    let choices = items(&status["poll"]["choices"]);
    if !choices.is_empty() {
        parts.push(
            choices
                .iter()
                .map(|choice| {
                    let label = escape(string(&choice["label"]));
                    match choice["percentage"].as_f64() {
                        Some(pct) => format!("- {label} ({pct}%)"),
                        None => format!("- {label}"),
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if attached.is_empty()
        && let Some(url) = status["card"]["url"].as_str().and_then(web_url)
        && let Some(title) = status["card"]["title"].as_str()
        && !title.is_empty()
    {
        parts.push(link(&escape(title), &url));
    }
    if include_quote && status["quote"].is_object() {
        parts.push(quote_post(&status["quote"]));
    }
    parts.join("\n\n")
}

pub(super) fn quote_post(status: &Value) -> String {
    let author = &status["author"];
    let handle = string(&author["screen_name"]);
    let name = author["name"].as_str().unwrap_or(handle);
    let label = if handle.is_empty() {
        escape(name)
    } else {
        format!("{} (@{})", escape(name), escape(handle))
    };
    let url = status["url"]
        .as_str()
        .and_then(web_url)
        .or_else(|| {
            id(&status["id"])
                .filter(|id| super::is_post_id(id))
                .map(|id| format!("https://x.com/i/status/{id}"))
        })
        .unwrap_or_default();
    let mut content = link(&label, &url);
    let body = body(status, false);
    if !body.is_empty() {
        content.push_str("\n\n");
        content.push_str(&body);
    }
    quote(&content)
}

fn rich_text(status: &Value) -> String {
    let raw = &status["raw_text"];
    let Some(source) = raw["text"].as_str() else {
        return paragraphs(&escape(&plain_text(status)));
    };
    let offsets = scalar_offsets(source);
    let count = offsets.len() - 1;
    let indices = &raw["display_text_range"];
    let start = indices[0].as_u64().unwrap_or_default().min(count as u64) as usize;
    let end = indices[1]
        .as_u64()
        .unwrap_or(count as u64)
        .clamp(start as u64, count as u64) as usize;
    let slice = |from: usize, to: usize| &source[offsets[from]..offsets[to]];
    let mut spans: Vec<_> = items(&raw["facets"])
        .iter()
        .filter_map(|facet| {
            let indices = &facet["indices"];
            Some((span(&indices[0], &indices[1])?, facet))
        })
        .collect();
    spans.sort_by_key(|(range, _)| range.start);

    let mut rich = RichText::default();
    let article_link = id(&status["article"]["id"]).map(|id| format!("x.com/i/article/{id}"));
    let mut cursor = start;
    let mut mapped = 0;
    let mut updates = Vec::new();
    for (range, facet) in &spans {
        let kind = string(&facet["type"]);
        if range.start < cursor || range.end > end || Style::parse(kind).is_some() {
            continue;
        }
        rich.text.push_str(slice(cursor, range.start));
        mapped += range.start - cursor;
        let original = slice(range.start, range.end);
        let url = data_link(kind, facet, original);
        let replacement = if kind == "media"
            || url.as_deref().is_some_and(|url| {
                article_link
                    .as_deref()
                    .is_some_and(|article| url.contains(article))
            }) {
            ""
        } else if kind == "url" {
            facet["display"]
                .as_str()
                .or(url.as_deref())
                .unwrap_or(original)
        } else {
            original
        };
        let next = mapped + replacement.chars().count();
        rich.text.push_str(replacement);
        if let Some(url) = url
            && next > mapped
        {
            rich.links.push((mapped..next, url));
        }
        updates.push((range, mapped, next));
        mapped = next;
        cursor = range.end;
    }
    rich.text.push_str(slice(cursor, end));
    let position = |original: usize| {
        let index = updates.partition_point(|(range, _, _)| range.start < original);
        if index == 0 {
            return original - start;
        }
        let (range, from, to) = updates[index - 1];
        if original >= range.end {
            to + (original - range.end)
        } else {
            from
        }
    };
    for (range, facet) in &spans {
        if let Some(style) = Style::parse(string(&facet["type"])) {
            let from = position(range.start.clamp(start, end));
            let to = position(range.end.clamp(start, end));
            rich.styles.push((from..to, style));
        }
    }
    paragraphs(&rich.render())
}

fn paragraphs(value: &str) -> String {
    value
        .replace("\r\n", "\n")
        .split("\n\n")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| part.replace('\n', "  \n"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(super) fn plain_text(status: &Value) -> String {
    let source = string(&status["text"]);
    let mut plain = String::with_capacity(source.len());
    for part in source.split_inclusive(char::is_whitespace) {
        let word = part.trim_end();
        if !is_attachment_link(word) {
            plain.push_str(part);
        }
    }
    plain.trim().to_owned()
}

fn is_attachment_link(word: &str) -> bool {
    if !word.starts_with("https://") && !word.starts_with("http://") {
        return false;
    }
    let Ok(url) = url::Url::parse(word) else {
        return false;
    };
    let path = url.path();
    matches!(url.host_str(), Some("x.com" | "twitter.com"))
        && (path.starts_with("/i/article/")
            || (path.contains("/status/")
                && (path.contains("/photo/") || path.contains("/video/"))))
}

pub(super) fn title(status: &Value) -> String {
    let text = plain_text(status);
    let head = text
        .lines()
        .map(|line| {
            line.split_whitespace()
                .skip_while(|word| word.starts_with('@'))
                .filter(|word| !word.starts_with("https://") && !word.starts_with("http://"))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .find(|line| !line.is_empty());
    if let Some(head) = head {
        let offset = head.char_indices().nth(100).map(|(offset, _)| offset);
        return offset
            .map(|offset| format!("{}…", head[..offset].trim_end()))
            .unwrap_or(head);
    }
    let author = &status["author"];
    let handle = string(&author["screen_name"]);
    let name = author["name"].as_str().unwrap_or(handle);
    status["article"]["title"]
        .as_str()
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if handle.is_empty() {
                name.to_owned()
            } else {
                format!("{name} (@{handle})")
            }
        })
}

pub(super) fn first_image(status: &Value) -> Option<String> {
    items(&status["media"]["all"])
        .iter()
        .chain(items(&status["media"]["photos"]))
        .find_map(|item| {
            if item["type"].as_str().is_none_or(|kind| kind == "photo") {
                item["url"].as_str().and_then(web_url)
            } else {
                item["thumbnail_url"].as_str().and_then(web_url)
            }
        })
}
