use scraper::ElementRef;

use super::super::dom::text;

pub(super) fn boilerplate(element: ElementRef<'_>) -> bool {
    if matches!(
        element.value().name(),
        "body" | "main" | "article" | "code" | "pre"
    ) || is_preserved(element)
    {
        return false;
    }

    ["class", "id", "aria-label", "data-testid"]
        .iter()
        .filter_map(|attr| element.attr(attr))
        .any(|value| {
            let value = value.to_ascii_lowercase().replace('_', "-");
            value.split_ascii_whitespace().any(|token| {
                let token = token.trim_matches('-');
                [
                    "cookie",
                    "consent",
                    "newsletter",
                    "subscribe",
                    "breadcrumb",
                    "byline",
                    "sidebar",
                    "share",
                    "sharing",
                    "comment",
                    "comments",
                    "advertisement",
                    "advert",
                    "ad",
                    "ads",
                    "related",
                    "recommendations",
                    "pagination",
                    "social-share",
                    "related-posts",
                    "entry-meta",
                    "post-meta",
                    "site-header",
                    "site-footer",
                    "read-also",
                    "sponsored",
                    "next-post",
                    "previous-post",
                ]
                .iter()
                .any(|part| {
                    token == *part
                        || token
                            .strip_prefix(part)
                            .is_some_and(|suffix| suffix.starts_with('-'))
                        || token
                            .strip_suffix(part)
                            .is_some_and(|prefix| prefix.ends_with('-'))
                })
            })
        })
}

pub(super) fn is_preserved(element: ElementRef<'_>) -> bool {
    element
        .attr("role")
        .is_some_and(|role| matches!(role, "doc-footnotes" | "doc-footnote"))
        || element.attr("data-type").is_some_and(|value| {
            matches!(
                value,
                "footnote" | "note" | "tip" | "warning" | "important" | "caution"
            )
        })
        || element.attr("class").is_some_and(|class| {
            class.split_ascii_whitespace().any(|part| {
                part.starts_with("katex")
                    || part.starts_with("footnote")
                    || part.starts_with("MathJax")
                    || part.starts_with("callout")
                    || part.starts_with("admonition")
            })
        })
        || matches!(element.value().name(), "math" | "mjx-container")
        || element.attr("data-mathml").is_some()
        || element.attr("data-latex").is_some()
}

pub(super) fn duplicate_title(element: ElementRef<'_>, title: &str) -> bool {
    element.value().name() == "h1" && !title.is_empty() && text(element).eq_ignore_ascii_case(title)
}
