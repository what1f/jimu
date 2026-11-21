use std::ops::Range;

use crate::web::web2md::markdown::{destination, escape_text, inline_code};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Style {
    Bold,
    Italic,
    Strike,
    Code,
}

impl Style {
    pub fn parse(style: &str) -> Option<Self> {
        match style.to_ascii_lowercase().as_str() {
            "bold" => Some(Self::Bold),
            "italic" => Some(Self::Italic),
            "strikethrough" => Some(Self::Strike),
            "code" => Some(Self::Code),
            _ => None,
        }
    }

    fn marker(self) -> &'static str {
        match self {
            Self::Bold => "**",
            Self::Italic => "*",
            Self::Strike => "~~",
            Self::Code => "`",
        }
    }
}

#[derive(Default)]
pub(super) struct RichText {
    pub text: String,
    pub styles: Vec<(Range<usize>, Style)>,
    pub links: Vec<(Range<usize>, String)>,
}

enum Event {
    Style(Style, i32),
    Link(Option<usize>),
}

impl RichText {
    pub fn render(mut self) -> String {
        let offsets = scalar_offsets(&self.text);
        let len = offsets.len() - 1;
        let utf16 = utf16_offsets(&self.text);
        if self
            .styles
            .iter()
            .map(|(range, _)| range.end)
            .chain(self.links.iter().map(|(range, _)| range.end))
            .any(|end| end > len && end < utf16.len())
        {
            for range in self
                .styles
                .iter_mut()
                .map(|(range, _)| range)
                .chain(self.links.iter_mut().map(|(range, _)| range))
            {
                range.start = utf16[range.start.min(utf16.len() - 1)];
                range.end = utf16[range.end.min(utf16.len() - 1)];
            }
        }

        let mut events = Vec::new();
        for (range, style) in &self.styles {
            if range.start < range.end.min(len) {
                events.push((range.start, Event::Style(*style, 1)));
                events.push((range.end.min(len), Event::Style(*style, -1)));
            }
        }
        let mut link_end = 0;
        self.links.sort_by_key(|(range, _)| range.start);
        for (index, (range, _)) in self.links.iter().enumerate() {
            let end = range.end.min(len);
            if range.start >= link_end && range.start < end {
                events.push((range.start, Event::Link(Some(index))));
                events.push((end, Event::Link(None)));
                link_end = end;
            }
        }
        events.sort_by_key(|(pos, _)| *pos);

        let mut rendered = Inline {
            links: &self.links,
            output: String::new(),
            open: Vec::new(),
            link: None,
        };
        let mut active = [0i32; 4];
        let mut link = None;
        let mut start = 0;
        for (position, event) in events {
            if start < position {
                rendered.segment(&self.text[offsets[start]..offsets[position]], &active, link);
                start = position;
            }
            match event {
                Event::Style(style, delta) => active[style as usize] += delta,
                Event::Link(id) => link = id,
            }
        }
        rendered.segment(&self.text[offsets[start]..], &active, link);
        rendered.close_link();
        rendered.output
    }
}

struct Inline<'a> {
    links: &'a [(Range<usize>, String)],
    output: String,
    open: Vec<Style>,
    link: Option<usize>,
}

impl Inline<'_> {
    fn segment(&mut self, text: &str, active: &[i32; 4], link: Option<usize>) {
        let body = text.trim_start();
        if body.is_empty() {
            self.output.push_str(text);
            return;
        }
        if self.link != link {
            self.close_link();
        }
        if let Some(end) = self
            .open
            .iter()
            .position(|style| active[*style as usize] <= 0)
        {
            self.close_styles(end);
        }
        self.output.push_str(&text[..text.len() - body.len()]);
        if let Some(link) = link
            && self.link.is_none()
        {
            self.output.push('[');
            self.link = Some(link);
        }
        for style in [Style::Bold, Style::Italic, Style::Strike] {
            if active[style as usize] > 0 && !self.open.contains(&style) {
                self.output.push_str(style.marker());
                self.open.push(style);
            }
        }
        if active[Style::Code as usize] > 0 {
            self.output.push_str(&inline_code(body.trim_end()));
            self.output.push_str(&body[body.trim_end().len()..]);
        } else {
            let line_start = self.output.is_empty() || self.output.ends_with('\n');
            self.output.push_str(&escape_text(body, line_start));
        }
    }

    fn close(&mut self, marker: &str) {
        let end = self.output.trim_end().len();
        self.output.insert_str(end, marker);
    }

    fn close_styles(&mut self, remaining: usize) {
        while self.open.len() > remaining {
            if let Some(style) = self.open.pop() {
                self.close(style.marker());
            }
        }
    }

    fn close_link(&mut self) {
        self.close_styles(0);
        if let Some(index) = self.link.take() {
            self.close(&format!("]({})", destination(&self.links[index].1)));
        }
    }
}

pub(super) fn scalar_offsets(text: &str) -> Vec<usize> {
    text.char_indices()
        .map(|(offset, _)| offset)
        .chain([text.len()])
        .collect()
}

fn utf16_offsets(text: &str) -> Vec<usize> {
    let mut offsets = Vec::new();
    for (index, c) in text.chars().enumerate() {
        offsets.push(index);
        if c.len_utf16() == 2 {
            offsets.push(index + 1);
        }
    }
    offsets.push(text.chars().count());
    offsets
}
