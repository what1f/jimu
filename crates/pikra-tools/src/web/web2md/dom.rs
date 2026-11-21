//! The shared HTML5 document used by selectors and content extractors.

use ego_tree::NodeId;
pub use scraper::{ElementRef, Selector};
use scraper::{Html, Node};
use url::Url;

#[derive(Clone)]
pub struct Dom {
    pub(crate) html: Html,
    pub url: Url,
}

impl Dom {
    pub fn parse(html: &str, url: Url) -> Self {
        Self {
            html: Html::parse_document(html),
            url,
        }
    }

    pub fn select<'a, 'b>(
        &'a self,
        selector: &'b Selector,
    ) -> impl Iterator<Item = ElementRef<'a>> + 'b
    where
        'a: 'b,
    {
        self.html.select(selector)
    }

    pub fn query(&self, css: &str) -> Option<ElementRef<'_>> {
        self.html.select(&Selector::parse(css).ok()?).next()
    }

    pub(super) fn ids(&self, css: &Selector) -> Vec<NodeId> {
        self.select(css).map(|e| e.id()).collect()
    }

    pub(super) fn element(&self, id: NodeId) -> Option<ElementRef<'_>> {
        ElementRef::wrap(self.html.tree.get(id)?)
    }

    pub(super) fn remove(&mut self, id: NodeId) {
        if let Some(mut node) = self.html.tree.get_mut(id) {
            node.detach();
        }
    }

    pub(super) fn edit(&mut self, id: NodeId, edit: impl FnOnce(&mut scraper::node::Element)) {
        if let Some(mut node) = self.html.tree.get_mut(id)
            && let Node::Element(element) = node.value()
        {
            edit(element);
        }
    }

    pub(super) fn unwrap(&mut self, id: NodeId) {
        let children: Vec<NodeId> = match self.html.tree.get(id) {
            Some(node) => node.children().map(|child| child.id()).collect(),
            None => return,
        };
        if let Some(mut node) = self.html.tree.get_mut(id) {
            for child in children {
                node.insert_id_before(child);
            }
            node.detach();
        }
    }

    pub(super) fn move_to(&mut self, id: NodeId, parent: NodeId) {
        if id == parent {
            return;
        }
        if let Some(mut parent) = self.html.tree.get_mut(parent) {
            parent.append_id(id);
        }
    }

    pub(super) fn base_url(&self) -> Url {
        self.query("base[href]")
            .and_then(|element| resolve(&self.url, element.attr("href")?, false))
            .and_then(|base| Url::parse(&base).ok())
            .unwrap_or_else(|| self.url.clone())
    }
}

pub(super) fn text(element: ElementRef<'_>) -> String {
    collapse(&element.text().collect::<String>())
        .trim()
        .to_owned()
}

pub(super) fn collapse(value: &str) -> String {
    if !value.chars().any(char::is_whitespace) {
        return value.to_owned();
    }
    let mut output = String::with_capacity(value.len());
    let mut whitespace = false;
    for character in value.chars() {
        if character.is_whitespace() {
            if !whitespace {
                output.push(' ');
            }
            whitespace = true;
        } else {
            output.push(character);
            whitespace = false;
        }
    }
    output
}

pub(super) fn resolve(base: &Url, value: &str, link: bool) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let url = base.join(value).ok()?;
    match url.scheme() {
        "http" | "https" if url.username().is_empty() && url.password().is_none() => {
            Some(url.into())
        }
        "mailto" | "tel" if link => Some(url.into()),
        _ => None,
    }
}
