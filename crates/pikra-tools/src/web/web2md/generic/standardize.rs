use std::collections::HashSet;
use std::sync::LazyLock;

use ego_tree::NodeId;
use scraper::Selector;
use url::Url;

use super::super::dom::{Dom, resolve};
use super::constants::FOOTNOTES;
use super::content_patterns::duplicate_title;

static LINKS: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a[href]").expect("valid links selector"));

pub(super) fn clean(dom: &mut Dom, root: NodeId, base: &Url, title: &str) {
    let Some(article) = dom.element(root) else {
        return;
    };
    let fragments: HashSet<String> = article
        .select(&LINKS)
        .filter_map(|anchor| anchor.attr("href")?.strip_prefix('#'))
        .map(str::to_owned)
        .collect();
    let external_notes: Vec<_> = dom
        .query("body")
        .into_iter()
        .flat_map(|body| body.select(&FOOTNOTES))
        .filter(|note| note.attr("id").is_some_and(|id| fragments.contains(id)))
        .filter(|note| !note.ancestors().any(|parent| parent.id() == root))
        .filter(|note| !article.ancestors().any(|parent| parent.id() == note.id()))
        .map(|note| note.id())
        .collect();
    for note in external_notes {
        dom.move_to(note, root);
    }

    let Some(article) = dom.element(root) else {
        return;
    };
    let ids: Vec<_> = article
        .descendent_elements()
        .map(|element| element.id())
        .collect();
    for id in ids {
        let Some(element) = dom.element(id) else {
            continue;
        };
        if duplicate_title(element, title)
            || (element.value().name() == "a"
                && element
                    .attr("href")
                    .is_some_and(|href| href.starts_with('#'))
                && element
                    .parent()
                    .and_then(scraper::ElementRef::wrap)
                    .is_some_and(|parent| {
                        matches!(
                            parent.value().name(),
                            "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                        )
                    }))
        {
            dom.remove(id);
            continue;
        }

        let target = match element.value().name() {
            "a" => Some(("href", element.attr("href"))),
            "img" => Some((
                "src",
                ["data-src", "data-original", "data-lazy-src", "src"]
                    .into_iter()
                    .find_map(|attr| element.attr(attr).filter(|value| !value.is_empty())),
            )),
            "video" | "audio" | "source" => Some(("src", element.attr("src"))),
            _ => None,
        };
        let resolved = target.and_then(|(name, value)| {
            let value = value?;
            let is_note = name == "href"
                && value.strip_prefix('#').is_some_and(|id| {
                    is_note_id(id)
                        || element.attr("role") == Some("doc-noteref")
                        || element.attr("data-footnote-ref").is_some()
                });
            let url = if is_note {
                value.to_owned()
            } else {
                resolve(base, value, name == "href")?
            };
            Some((name, url))
        });

        dom.edit(id, |element| {
            element.attrs.retain(|(name, _)| {
                matches!(
                    name.local.as_ref(),
                    "id" | "class"
                        | "title"
                        | "alt"
                        | "role"
                        | "start"
                        | "colspan"
                        | "rowspan"
                        | "align"
                        | "type"
                        | "checked"
                        | "display"
                        | "encoding"
                        | "lang"
                        | "open"
                        | "close"
                        | "data-language"
                        | "data-lang"
                        | "data-latex"
                        | "data-tex"
                        | "data-mathml"
                        | "data-callout"
                        | "data-type"
                        | "data-footnote-ref"
                )
            });
            if let Some((name, url)) = resolved {
                let mut attribute = element.name.clone();
                attribute.local = name.into();
                attribute.prefix = None;
                attribute.ns = "".into();
                element.attrs.push((attribute, url.as_str().into()));
            }
        });
    }
}

pub(in crate::web::web2md) fn is_note_id(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    lower.starts_with("footnote")
        || lower.starts_with("cite_note")
        || lower.strip_prefix("fn").is_some_and(|suffix| {
            suffix.starts_with(['-', ':']) || suffix.starts_with(|c: char| c.is_ascii_digit())
        })
}
