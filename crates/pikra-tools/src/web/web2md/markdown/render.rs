use std::collections::HashMap;

use ego_tree::NodeRef;
use scraper::{ElementRef, Node};

use super::super::dom::{collapse, text};
use super::super::generic::standardize::is_note_id;
use super::{escape, fence, image, inline_code, link, math, quote};

pub(in crate::web::web2md) fn convert(article: ElementRef<'_>) -> String {
    let definitions: HashMap<_, _> = article
        .descendent_elements()
        .filter_map(|element| {
            let id = element.attr("id")?;
            is_definition(element).then_some((id, element))
        })
        .collect();
    let mut renderer = Renderer {
        definitions,
        references: Vec::new(),
        labels: HashMap::new(),
    };
    let mut output = renderer.children(article).trim().to_owned();
    let mut index = 0;
    while let Some(id) = renderer.references.get(index).copied() {
        index += 1;
        let content = renderer.children(renderer.definitions[id]);
        let mut lines = content.trim().split('\n');
        let mut note = format!("[^{index}]: {}", lines.next().unwrap_or_default());
        for line in lines {
            note.push('\n');
            if !line.is_empty() {
                note.push_str("    ");
                note.push_str(line);
            }
        }
        output.push_str("\n\n");
        output.push_str(&note);
    }
    output
}

fn is_definition(element: ElementRef<'_>) -> bool {
    element.attr("role") == Some("doc-footnote")
        || element.attr("data-type") == Some("footnote")
        || (matches!(element.value().name(), "li" | "aside" | "p" | "div" | "dd")
            && element.attr("id").is_some_and(is_note_id))
}

struct Renderer<'a> {
    definitions: HashMap<&'a str, ElementRef<'a>>,
    references: Vec<&'a str>,
    labels: HashMap<&'a str, usize>,
}

impl<'a> Renderer<'a> {
    fn children(&mut self, element: ElementRef<'a>) -> String {
        let mut output = String::new();
        for child in element.children() {
            append(&mut output, &self.node(child));
        }
        output
    }

    fn node(&mut self, node: NodeRef<'a, Node>) -> String {
        match node.value() {
            Node::Text(text) => math::escape_prose(text),
            Node::Element(_) => ElementRef::wrap(node)
                .map(|element| self.element(element))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn element(&mut self, element: ElementRef<'a>) -> String {
        let tag = element.value().name();
        if is_definition(element)
            || matches!(element.attr("role"), Some("doc-endnotes" | "doc-footnotes"))
            || element.attr("class").is_some_and(|class| {
                class
                    .split_ascii_whitespace()
                    .any(|name| matches!(name, "footnotes" | "katex-html"))
            })
        {
            return String::new();
        }
        if let Some((math, block)) = math::render(element) {
            return if block { paragraph(&math) } else { math };
        }
        if let Some((kind, title)) = callout(element) {
            let mut body = format!("[!{kind}]");
            if !title.is_empty() {
                body.push(' ');
                body.push_str(&escape(&title));
            }
            let content = element
                .children()
                .filter(|node| ElementRef::wrap(*node).is_none_or(|child| !is_callout_title(child)))
                .map(|node| self.node(node))
                .collect::<Vec<_>>()
                .join("");
            if !content.trim().is_empty() {
                body.push('\n');
                body.push_str(content.trim());
            }
            return paragraph(&quote(&body));
        }

        match tag {
            "script" | "wbr" | "annotation" | "annotation-xml" => String::new(),
            "br" => "  \n".into(),
            "hr" => paragraph("---"),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let title = self.children(element);
                if title.trim().is_empty() {
                    String::new()
                } else {
                    paragraph(&format!(
                        "{} {}",
                        "#".repeat(usize::from(tag.as_bytes()[1] - b'0')),
                        collapse(title.trim())
                    ))
                }
            }
            "p" | "div" | "section" | "article" | "main" | "header" | "figure" | "figcaption"
            | "details" | "summary" | "dl" | "dt" | "dd" | "address" => {
                paragraph(&self.children(element))
            }
            "em" | "i" => emphasized(&self.children(element), "*"),
            "strong" | "b" => emphasized(&self.children(element), "**"),
            "del" | "s" | "strike" => emphasized(&self.children(element), "~~"),
            "code" | "kbd" | "samp" => {
                let code = element.text().collect::<String>();
                if code.trim().is_empty() {
                    code
                } else {
                    inline_code(code.trim())
                }
            }
            "pre" => {
                let code = element
                    .descendent_elements()
                    .find(|child| child.value().name() == "code")
                    .unwrap_or(element);
                paragraph(&fence(
                    &code_text(code),
                    language(code)
                        .or_else(|| language(element))
                        .unwrap_or_default(),
                ))
            }
            "blockquote" => paragraph(&quote(self.children(element).trim())),
            "a" => self.anchor(element),
            "img" => image(
                element.attr("alt").unwrap_or_default(),
                element.attr("src").unwrap_or_default(),
            ),
            "video" | "audio" => {
                let source = element
                    .attr("src")
                    .or_else(|| element.child_elements().find_map(|child| child.attr("src")));
                source
                    .map(|url| {
                        paragraph(&link(if tag == "video" { "Video" } else { "Audio" }, url))
                    })
                    .unwrap_or_default()
            }
            "ol" | "ul" => paragraph(&self.list(element)),
            "input" if element.attr("type") == Some("checkbox") => {
                if element.attr("checked").is_some() {
                    "[x] ".into()
                } else {
                    "[ ] ".into()
                }
            }
            "table" => paragraph(&self.table(element)),
            _ => self.children(element),
        }
    }

    fn anchor(&mut self, element: ElementRef<'a>) -> String {
        let Some(href) = element.attr("href") else {
            return self.children(element);
        };
        if let Some(id) = href.strip_prefix('#')
            && let Some((&id, _)) = self.definitions.get_key_value(id)
        {
            let next = self.references.len() + 1;
            let index = self.labels.entry(id).or_insert_with(|| {
                self.references.push(id);
                next
            });
            return format!("[^{index}]");
        }
        let label = self.children(element);
        if label.trim().is_empty() {
            return String::new();
        }
        let before = label.len() - label.trim_start().len();
        let after = label.trim_end().len();
        format!(
            "{}{}{}",
            &label[..before],
            link(label.trim(), href),
            &label[after..]
        )
    }

    fn list(&mut self, element: ElementRef<'a>) -> String {
        let ordered = element.value().name() == "ol";
        let start = element
            .attr("start")
            .and_then(|start| start.parse::<u32>().ok())
            .filter(|start| *start < 100_000_000)
            .unwrap_or(1);
        let mut items = Vec::new();
        for (index, item) in element
            .child_elements()
            .filter(|el| el.value().name() == "li")
            .enumerate()
        {
            if is_definition(item) {
                continue;
            }
            let marker = if ordered {
                format!("{}. ", start as usize + index)
            } else {
                "- ".into()
            };
            let mut body = String::new();
            for child in item.children() {
                if let Some(list) =
                    ElementRef::wrap(child).filter(|el| matches!(el.value().name(), "ul" | "ol"))
                {
                    if !body.trim().is_empty() {
                        body.truncate(body.trim_end().len());
                        body.push('\n');
                    }
                    body.push_str(&self.list(list));
                    body.push('\n');
                } else {
                    append(&mut body, &self.node(child));
                }
            }
            let mut lines = body.trim().split('\n');
            let mut output = format!("{marker}{}", lines.next().unwrap_or_default());
            for line in lines {
                output.push('\n');
                if !line.is_empty() {
                    output.push_str(&" ".repeat(marker.len()));
                    output.push_str(line);
                }
            }
            items.push(output);
        }
        items.join("\n")
    }

    fn table(&mut self, table: ElementRef<'a>) -> String {
        let mut rows = Vec::new();
        let mut alignments = Vec::new();
        let mut header = false;
        for row in table.child_elements().flat_map(|child| {
            if child.value().name() == "tr" {
                vec![child]
            } else if matches!(child.value().name(), "thead" | "tbody" | "tfoot") {
                child
                    .child_elements()
                    .filter(|el| el.value().name() == "tr")
                    .collect()
            } else {
                Vec::new()
            }
        }) {
            let mut cells = Vec::new();
            for cell in row
                .child_elements()
                .filter(|cell| matches!(cell.value().name(), "th" | "td"))
            {
                if rows.is_empty() {
                    header |= cell.value().name() == "th";
                    alignments.push(match cell.attr("align") {
                        Some("left") => ":---",
                        Some("right") => "---:",
                        Some("center") => ":---:",
                        _ => "---",
                    });
                }
                let content = self.children(cell);
                let cell = content
                    .trim()
                    .split('\n')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join("<br>");
                cells.push(cell.replace('|', "\\|"));
            }
            if !cells.is_empty() {
                rows.push(cells);
            }
        }

        let Some(columns) = rows.iter().map(Vec::len).max() else {
            return String::new();
        };
        if !header {
            rows.insert(0, vec![String::new(); columns]);
            alignments.clear();
        }
        // GFM fills missing cells and ignores cells beyond the header width.
        alignments.resize(rows[0].len(), "---");
        let mut rendered = Vec::new();
        for (index, row) in rows.into_iter().enumerate() {
            rendered.push(format!("| {} |", row.join(" | ")));
            if index == 0 {
                rendered.push(format!("| {} |", alignments.join(" | ")));
            }
        }
        if let Some(caption) = table
            .child_elements()
            .find(|el| el.value().name() == "caption")
        {
            rendered.push(format!("\n{}", self.children(caption).trim()));
        }
        rendered.join("\n")
    }
}

fn append(output: &mut String, next: &str) {
    if next.is_empty() {
        return;
    }
    if next.starts_with('\n') {
        let newlines = output.len() - output.trim_end_matches('\n').len();
        if newlines > 0 {
            output.truncate(output.len() - newlines);
        }
        output.push_str(next);
    } else if (output.ends_with(' ') || output.ends_with('\n'))
        && next.starts_with(' ')
        && next != "  \n"
    {
        output.push_str(next.trim_start_matches(' '));
    } else {
        output.push_str(next);
    }
}

fn paragraph(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        String::new()
    } else {
        format!("\n\n{text}\n\n")
    }
}

fn emphasized(content: &str, marker: &str) -> String {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return content.to_owned();
    }
    let start = content.len() - content.trim_start().len();
    let end = start + trimmed.len();
    format!(
        "{}{marker}{trimmed}{marker}{}",
        &content[..start],
        &content[end..]
    )
}

fn language(element: ElementRef<'_>) -> Option<&str> {
    element
        .attr("data-language")
        .or_else(|| element.attr("data-lang"))
        .or_else(|| {
            element
                .attr("class")?
                .split_ascii_whitespace()
                .find_map(|class| {
                    class
                        .strip_prefix("language-")
                        .or_else(|| class.strip_prefix("lang-"))
                        .or_else(|| class.strip_prefix("highlight-source-"))
                })
        })
        .filter(|lang| {
            !lang.is_empty()
                && lang
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '_' | '#' | '.'))
        })
}

fn code_text(element: ElementRef<'_>) -> String {
    use ego_tree::iter::Edge;

    let mut code = String::new();
    for edge in element.traverse() {
        match edge {
            Edge::Open(node) => match node.value() {
                Node::Text(text) => code.push_str(text),
                Node::Element(element) if element.name() == "br" => code.push('\n'),
                _ => {}
            },
            Edge::Close(node)
                if node
                    .value()
                    .as_element()
                    .is_some_and(|el| matches!(el.name(), "div" | "li"))
                    && !code.ends_with('\n') =>
            {
                code.push('\n');
            }
            _ => {}
        }
    }
    code
}

fn callout(element: ElementRef<'_>) -> Option<(String, String)> {
    let classes = element.attr("class").unwrap_or_default();
    let mut classes = classes.split_ascii_whitespace();
    let tag = element.value().name();
    let typ = element
        .attr("data-callout")
        .or_else(|| {
            element.attr("data-type").filter(|kind| {
                matches!(*kind, "note" | "tip" | "warning" | "important" | "caution")
            })
        })
        .or_else(|| {
            (classes
                .clone()
                .any(|class| matches!(class, "callout" | "admonition" | "markdown-alert"))
                || (tag == "aside"
                    && element
                        .attr("class")
                        .is_some_and(|class| class.contains("callout"))))
            .then(|| {
                classes
                    .find_map(|class| {
                        class
                            .strip_prefix("callout-")
                            .or_else(|| class.strip_prefix("markdown-alert-"))
                            .or_else(|| {
                                matches!(
                                    class,
                                    "note" | "tip" | "warning" | "important" | "caution"
                                )
                                .then_some(class)
                            })
                    })
                    .unwrap_or("note")
            })
        })?;
    let kind = typ
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(32)
        .collect::<String>()
        .to_ascii_uppercase();
    if kind.is_empty() {
        return None;
    }
    let title = element
        .child_elements()
        .find(|el| is_callout_title(*el))
        .map(text)
        .unwrap_or_default();
    Some((
        kind.clone(),
        if title.eq_ignore_ascii_case(&kind) {
            String::new()
        } else {
            title
        },
    ))
}

fn is_callout_title(element: ElementRef<'_>) -> bool {
    element.attr("class").is_some_and(|class| {
        class.split_ascii_whitespace().any(|name| {
            matches!(
                name,
                "callout-title" | "admonition-title" | "markdown-alert-title"
            )
        })
    })
}
