use serde_json::Value;
use url::Url;

use super::super::dom::{Dom, collapse, resolve, text};

#[derive(Default)]
pub(super) struct Metadata {
    pub title: String,
    pub author: String,
    pub site: String,
    pub published: String,
    pub description: String,
    pub image: String,
    pub language: String,
    pub favicon: String,
    pub article_body: String,
}

pub(super) fn extract(dom: &Dom, base: &Url) -> Metadata {
    let meta_selector = scraper::Selector::parse("meta[content]").expect("valid metadata selector");
    let entries: Vec<_> = dom
        .select(&meta_selector)
        .filter_map(|tag| {
            Some((
                tag.attr("property")
                    .or_else(|| tag.attr("name"))
                    .or_else(|| tag.attr("itemprop"))?,
                tag.attr("content")?.trim(),
            ))
        })
        .collect();
    let meta = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            entries
                .iter()
                .find(|(name, value)| name.eq_ignore_ascii_case(key) && !value.is_empty())
                .map(|(_, value)| *value)
        })
    };

    let json_selector = scraper::Selector::parse("script[type='application/ld+json']")
        .expect("valid schema selector");
    let schemas: Vec<Value> = dom
        .select(&json_selector)
        .filter_map(|tag| serde_json::from_str(tag.text().collect::<String>().trim()).ok())
        .collect();
    let property = |key| {
        schemas
            .iter()
            .find_map(|schema| schema_property(schema, key))
    };
    let field = |key| property(key).and_then(nonempty);
    let selector_text = |selector| {
        dom.query(selector)
            .map(text)
            .filter(|value| !value.is_empty())
    };
    let domain = base
        .host_str()
        .unwrap_or_default()
        .trim_start_matches("www.");
    let site = meta(&["og:site_name", "application-name"])
        .or_else(|| property("publisher").and_then(named))
        .unwrap_or(domain)
        .to_owned();
    let title = meta(&["og:title", "twitter:title"])
        .or_else(|| field("headline"))
        .map(str::to_owned)
        .or_else(|| selector_text("article h1, main h1, h1"))
        .or_else(|| selector_text("title"))
        .unwrap_or_default();
    let title = [" | ", " - ", " — ", " – ", " · "]
        .iter()
        .find_map(|sep| {
            let (label, suffix) = title.rsplit_once(sep)?;
            suffix.eq_ignore_ascii_case(&site).then_some(label)
        })
        .unwrap_or(&title)
        .trim()
        .to_owned();

    let author = meta(&["author", "article:author", "byl", "parsely-author"])
        .map(str::to_owned)
        .or_else(|| {
            property("author")
                .map(|value| {
                    value
                        .as_array()
                        .map(|authors| {
                            authors
                                .iter()
                                .filter_map(named)
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_else(|| named(value).unwrap_or_default().to_owned())
                })
                .filter(|name| !name.is_empty())
        })
        .or_else(|| selector_text("[rel='author'],[itemprop='author'],[class~='byline'],.author"))
        .unwrap_or_default();
    let author = author
        .strip_prefix("By ")
        .or_else(|| author.strip_prefix("by "))
        .unwrap_or(&author);
    let published = meta(&[
        "article:published_time",
        "datePublished",
        "date",
        "pubdate",
        "parsely-pub-date",
        "dc.date",
    ])
    .or_else(|| field("datePublished"))
    .or_else(|| {
        dom.query("article time[datetime],time[datetime]")
            .and_then(|time| time.attr("datetime"))
    })
    .unwrap_or_default();
    let description = meta(&["og:description", "description", "twitter:description"])
        .or_else(|| field("description"))
        .unwrap_or_default();
    let image = meta(&[
        "og:image:secure_url",
        "og:image",
        "og:image:url",
        "twitter:image",
        "thumbnail",
    ])
    .or_else(|| property("image").and_then(media_url))
    .and_then(|url| resolve(base, url, false))
    .unwrap_or_default();
    let language = dom
        .html
        .root_element()
        .attr("lang")
        .filter(|lang| !lang.is_empty())
        .or_else(|| field("inLanguage"))
        .or_else(|| meta(&["content-language", "og:locale"]))
        .unwrap_or_default()
        .replace('_', "-");
    let favicon = dom
        .query("link[rel~='icon'][href],link[rel='apple-touch-icon'][href]")
        .and_then(|icon| resolve(base, icon.attr("href")?, false))
        .unwrap_or_default();
    Metadata {
        title,
        author: collapse(author).trim().to_owned(),
        site,
        published: published.to_owned(),
        description: collapse(description).trim().to_owned(),
        image,
        language,
        favicon,
        article_body: field("articleBody").unwrap_or_default().trim().to_owned(),
    }
}

fn schema_property<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(values) = value.as_array() {
        return values.iter().find_map(|value| schema_property(value, key));
    }
    value
        .get(key)
        .filter(|value| !value.is_null())
        .or_else(|| {
            value
                .get("@graph")
                .and_then(|graph| schema_property(graph, key))
        })
        .or_else(|| {
            value
                .get("mainEntity")
                .and_then(|item| schema_property(item, key))
        })
}

fn nonempty(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn named(value: &Value) -> Option<&str> {
    nonempty(value).or_else(|| nonempty(&value["name"]))
}

fn media_url(value: &Value) -> Option<&str> {
    nonempty(value)
        .or_else(|| nonempty(&value["url"]))
        .or_else(|| nonempty(&value["contentUrl"]))
        .or_else(|| value.as_array()?.iter().find_map(media_url))
}
