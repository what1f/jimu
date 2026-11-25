use std::collections::HashMap;

use ego_tree::{NodeId, iter::Edge};
use scraper::ElementRef;

use super::super::Web2MdDocument;
use super::super::dom::Dom;
use super::super::markdown;
use super::{constants::CANDIDATES, metadata, removals, standardize};

pub(in crate::web::web2md) fn extract(source: &Dom) -> Option<Web2MdDocument> {
    let base = source.base_url();
    let metadata = metadata::extract(source, &base);
    let mut dom = source.clone();
    removals::clean(&mut dom);
    let article = main_content(&dom)?;
    standardize::clean(&mut dom, article, &base, &metadata.title);
    let article = dom.element(article)?;
    let mut markdown = markdown::convert(article);
    let html = article.inner_html();

    // Some pages put their full article in JSON-LD while the visible body is a teaser.
    if markdown.split_whitespace().count() < 50
        && metadata.article_body.split_whitespace().count()
            > markdown.split_whitespace().count() * 2
    {
        markdown = markdown::escape(&metadata.article_body);
    }
    if markdown.is_empty() {
        return None;
    }

    let mut document = Web2MdDocument::new(source.url.clone(), markdown, "generic");
    document.html = Some(html);
    document.title = metadata.title;
    document.author = metadata.author;
    document.site = metadata.site;
    document.published = metadata.published;
    document.description = metadata.description;
    document.image = metadata.image;
    document.language = metadata.language;
    document.favicon = metadata.favicon;
    Some(document)
}

#[derive(Clone, Copy, Default)]
struct Stats {
    text: usize,
    link_text: usize,
    prose: usize,
    rich: usize,
}

fn main_content(dom: &Dom) -> Option<NodeId> {
    let body = dom.query("body")?;
    let mut stats: HashMap<NodeId, Stats> = HashMap::new();
    // Retain a summary per node so deeply nested candidates take one postorder walk.
    for edge in body.traverse() {
        let Edge::Close(node) = edge else { continue };
        let mut summary = Stats::default();
        for child in node.children().filter_map(|child| stats.get(&child.id())) {
            summary.text += child.text;
            summary.link_text += child.link_text;
            summary.prose += child.prose;
            summary.rich += child.rich;
        }
        if let Some(text) = node.value().as_text() {
            summary.text = text.trim().chars().count();
        }
        if let Some(element) = ElementRef::wrap(node) {
            match element.value().name() {
                "a" => summary.link_text = summary.text,
                "p" if summary.text >= 30 => summary.prose += 1,
                "pre" | "blockquote" | "table" | "img" | "math" => summary.rich += 1,
                _ => {}
            }
        }
        stats.insert(node.id(), summary);
    }

    dom.select(&CANDIDATES)
        .filter_map(|element| {
            let stats = stats.get(&element.id())?;
            if stats.text == 0 && stats.rich == 0 {
                return None;
            }
            let body = element.value().name() == "body";
            let confidence = if element.attr("itemprop") == Some("articleBody")
                || element.attr("data-defuddle").is_some()
            {
                3.0
            } else if element.value().name() == "article" {
                2.5
            } else if element.value().name() == "main" || element.attr("role") == Some("main") {
                1.9
            } else if body {
                0.9
            } else if [
                "content",
                "post-content",
                "entry-content",
                "article-body",
                "markdown-body",
            ]
            .iter()
            .any(|name| {
                element.attr("id") == Some(*name)
                    || element.value().classes().any(|class| class == *name)
            }) {
                1.8
            } else {
                1.0
            };
            let text_weight = (stats.text.saturating_sub(stats.link_text) as f64).sqrt();
            let signal = text_weight
                + (stats.prose as f64).sqrt() * 12.0
                + (stats.rich as f64).sqrt() * 10.0;
            let density = stats.link_text as f64 / stats.text.max(1) as f64;
            let score = signal * confidence * (1.0 - density * 0.85);
            Some((element.id(), score))
        })
        .max_by(|(_, a), (_, b)| a.total_cmp(b))
        .map(|(id, _)| id)
        .or(Some(body.id()))
}
