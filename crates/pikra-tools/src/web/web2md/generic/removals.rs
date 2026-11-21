use scraper::ElementRef;

use super::super::dom::Dom;
use super::constants::CLUTTER;
use super::content_patterns::{boilerplate, is_preserved};

pub(super) fn clean(dom: &mut Dom) {
    for id in dom.ids(&CLUTTER) {
        dom.remove(id);
    }

    // Do not descend into detached subtrees. Limit retained nesting before recursive rendering.
    let mut pending = vec![(dom.html.root_element().id(), 0)];
    while let Some((id, depth)) = pending.pop() {
        let Some(element) = dom.element(id) else {
            continue;
        };
        if depth >= 96 || hidden(element) || boilerplate(element) {
            dom.remove(id);
        } else {
            pending.extend(
                element
                    .child_elements()
                    .map(|child| (child.id(), depth + 1)),
            );
        }
    }
}

fn hidden(element: ElementRef<'_>) -> bool {
    if is_preserved(element) {
        return false;
    }
    if element
        .attr("aria-hidden")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
    {
        return true;
    }
    element.attr("style").is_some_and(|style| {
        style
            .split(';')
            .filter_map(|rule| rule.split_once(':'))
            .any(|(key, value)| {
                let value = value.split('!').next().unwrap_or_default().trim();
                matches!(
                    (
                        key.trim().to_ascii_lowercase().as_str(),
                        value.to_ascii_lowercase().as_str()
                    ),
                    ("display", "none") | ("visibility", "hidden" | "collapse")
                )
            })
    })
}
