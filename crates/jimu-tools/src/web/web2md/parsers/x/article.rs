use std::collections::{HashMap, HashSet};
use std::ops::Range;

use serde_json::Value;

use super::rich_text::{RichText, Style};
use super::{data_link, id, items, media, span, string, web_url};
use crate::web::web2md::markdown::{escape, fence, link, quote};

pub(super) struct Article<'a> {
    blocks: &'a [Value],
    entities: HashMap<String, &'a Value>,
    media: HashMap<String, &'a Value>,
}

impl<'a> Article<'a> {
    pub fn new(article: &'a Value) -> Self {
        let raw = &article["content"]["entityMap"];
        let mut entities = HashMap::new();
        if let Some(map) = raw.as_object() {
            for (key, entity) in map {
                entities.insert(key.clone(), entity.get("value").unwrap_or(entity));
            }
        } else {
            for (index, entity) in items(raw).iter().enumerate() {
                let key = id(&entity["key"]).unwrap_or_else(|| index.to_string());
                entities.insert(key, entity.get("value").unwrap_or(entity));
            }
        }
        let media = items(&article["media_entities"])
            .iter()
            .filter_map(|item| {
                let key = id(&item["media_id"]).or_else(|| id(&item["id"]))?;
                Some((key, item.get("media_info").unwrap_or(item)))
            })
            .collect();
        Self {
            blocks: items(&article["content"]["blocks"]),
            entities,
            media,
        }
    }

    pub fn embedded_posts(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.blocks
            .iter()
            .flat_map(|block| self.block_entities(block).map(|(_, entity)| entity))
            .filter_map(tweet_entity)
            .filter(|id| seen.insert(id.clone()))
            .collect()
    }

    fn block_entities<'b>(
        &'b self,
        block: &'b Value,
    ) -> impl Iterator<Item = (&'b Value, &'a Value)> + 'b {
        items(&block["entityRanges"]).iter().filter_map(|range| {
            let key = id(&range["key"])?;
            Some((range, *self.entities.get(&key)?))
        })
    }

    pub fn render(&self, embeds: &HashMap<String, String>) -> String {
        let mut output = String::new();
        let mut code = Vec::new();
        let mut levels = Vec::new();
        let mut previous = "";
        for block in self.blocks {
            let kind = string(&block["type"]);
            if kind == "code-block" {
                code.push(string(&block["text"]));
                continue;
            }
            flush_code(&mut output, &mut code, &mut levels, &mut previous);
            let rendered = if let Some(markdown) = self
                .block_entities(block)
                .find(|(_, entity)| entity_kind(entity).eq_ignore_ascii_case("MARKDOWN"))
                .and_then(|(_, entity)| entity["data"]["markdown"].as_str())
            {
                markdown.trim().to_owned()
            } else {
                let atomic = self
                    .block_entities(block)
                    .filter_map(|(_, entity)| self.atomic(entity, embeds))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if atomic.is_empty() {
                    self.inline(block)
                } else {
                    atomic
                }
            };
            let rendered = rendered.trim();
            if rendered.is_empty() {
                continue;
            }
            let (block, separator) = match kind {
                "unordered-list-item" | "ordered-list-item" => {
                    let ordered = kind == "ordered-list-item";
                    let depth = block["depth"].as_u64().unwrap_or_default().min(32) as usize;
                    let separator = if levels.is_empty() { "\n\n" } else { "\n" };
                    (list_item(&mut levels, ordered, depth, rendered), separator)
                }
                "blockquote" => (
                    quote(&hard_breaks(rendered, "")),
                    if previous == kind { "\n>\n" } else { "\n\n" },
                ),
                _ => {
                    let level = match kind {
                        "header-one" => 2,
                        "header-two" => 3,
                        "header-three" => 4,
                        "header-four" | "header-five" | "header-six" => 5,
                        _ => 0,
                    };
                    if level == 0 {
                        (
                            if kind == "unstyled" || kind.is_empty() {
                                hard_breaks(rendered, "")
                            } else {
                                rendered.to_owned()
                            },
                            "\n\n",
                        )
                    } else {
                        (
                            format!("{} {}", "#".repeat(level), rendered.replace('\n', " ")),
                            "\n\n",
                        )
                    }
                }
            };
            if !matches!(kind, "unordered-list-item" | "ordered-list-item") {
                levels.clear();
            }
            push(&mut output, &block, separator);
            previous = kind;
        }
        flush_code(&mut output, &mut code, &mut levels, &mut previous);
        output
    }

    fn inline(&self, block: &Value) -> String {
        let mut rich = RichText {
            text: string(&block["text"]).to_owned(),
            ..Default::default()
        };
        for style in items(&block["inlineStyleRanges"]) {
            if let Some(range) = offset_range(style)
                && let Some(style) = Style::parse(string(&style["style"]))
            {
                rich.styles.push((range, style));
            }
        }
        for (range, entity) in self.block_entities(block) {
            if entity_kind(entity).eq_ignore_ascii_case("LINK")
                && let Some(range) = offset_range(range)
                && let Some(url) = entity["data"]["url"]
                    .as_str()
                    .or_else(|| entity["data"]["href"].as_str())
                    .and_then(web_url)
            {
                rich.links.push((range, url));
            }
        }
        for kind in ["urls", "mentions", "hashtags", "cashtags"] {
            for data in items(&block["data"][kind]) {
                if let Some(range) = span(&data["fromIndex"], &data["toIndex"])
                    && let Some(url) = data_link(kind, data, string(&data["text"]))
                {
                    rich.links.push((range, url));
                }
            }
        }
        rich.render()
    }

    fn atomic(&self, entity: &Value, embeds: &HashMap<String, String>) -> Option<String> {
        let data = &entity["data"];
        match entity_kind(entity).to_ascii_uppercase().as_str() {
            "MEDIA" => {
                let caption = string(&data["caption"]).trim();
                let mut rendered = items(&data["mediaItems"])
                    .iter()
                    .filter_map(|item| {
                        let id = id(&item["mediaId"]).or_else(|| id(&item["media_id"]))?;
                        media::article(self.media.get(&id)?, caption)
                    })
                    .collect::<Vec<_>>();
                if rendered.is_empty() {
                    return None;
                }
                if !caption.is_empty() {
                    rendered.push(format!("*{}*", escape(caption)));
                }
                Some(rendered.join("\n\n"))
            }
            "TWEET" => {
                let id = tweet_entity(entity)?;
                let url = format!("https://x.com/i/status/{id}");
                Some(
                    embeds
                        .get(&id)
                        .cloned()
                        .unwrap_or_else(|| quote(&link(&url, &url))),
                )
            }
            "DIVIDER" => Some("---".into()),
            "LATEX" | "TEX" => {
                let tex = data["markdown"]
                    .as_str()
                    .or_else(|| data["latex"].as_str())
                    .or_else(|| data["tex"].as_str())?
                    .trim();
                if tex.is_empty() {
                    None
                } else if tex.starts_with('$') {
                    Some(tex.to_owned())
                } else {
                    Some(format!("$$\n{tex}\n$$"))
                }
            }
            _ => None,
        }
    }
}

fn offset_range(value: &Value) -> Option<Range<usize>> {
    let start = value["offset"].as_u64()?.try_into().ok()?;
    let length = usize::try_from(value["length"].as_u64()?).ok()?;
    let start: usize = start;
    Some(start..start.checked_add(length)?)
}

fn tweet_entity(entity: &Value) -> Option<String> {
    if !entity_kind(entity).eq_ignore_ascii_case("TWEET") {
        return None;
    }
    id(&entity["data"]["tweetId"]).filter(|id| super::is_post_id(id))
}

fn entity_kind(entity: &Value) -> &str {
    string(&entity["type"])
}

fn push(output: &mut String, body: &str, separator: &str) {
    if !output.is_empty() {
        output.push_str(separator);
    }
    output.push_str(body);
}

fn flush_code(
    output: &mut String,
    code: &mut Vec<&str>,
    levels: &mut Vec<ListLevel>,
    previous: &mut &str,
) {
    if !code.is_empty() {
        push(output, &fence(&code.join("\n"), ""), "\n\n");
        code.clear();
        levels.clear();
        *previous = "";
    }
}

#[derive(Clone)]
struct ListLevel {
    ordered: bool,
    number: usize,
    width: usize,
}

fn list_item(levels: &mut Vec<ListLevel>, ordered: bool, depth: usize, text: &str) -> String {
    levels.truncate(depth + 1);
    if levels.len() == depth + 1 && levels[depth].ordered != ordered {
        levels.pop();
    }
    levels.resize_with(depth + 1, || ListLevel {
        ordered,
        number: 0,
        width: if ordered { 3 } else { 2 },
    });
    levels[depth].number += 1;
    let marker = if ordered {
        format!("{}. ", levels[depth].number)
    } else {
        "- ".into()
    };
    levels[depth].width = marker.len();
    let indent = levels[..depth].iter().map(|level| level.width).sum();
    let indent = " ".repeat(indent);
    let continuation = format!("{indent}{}", " ".repeat(marker.len()));
    format!("{indent}{marker}{}", hard_breaks(text, &continuation))
}

pub(super) fn hard_breaks(text: &str, indent: &str) -> String {
    text.split('\n')
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                line.to_owned()
            } else {
                format!("{indent}{}", line.trim_start())
            }
        })
        .collect::<Vec<_>>()
        .join("  \n")
}
