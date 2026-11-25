use scraper::ElementRef;

pub(super) fn render(element: ElementRef<'_>) -> Option<(String, bool)> {
    let tag = element.value().name();
    let class = element
        .attr("class")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let math_element = matches!(tag, "math" | "mjx-container" | "script")
        || class.split_ascii_whitespace().any(|name| {
            matches!(
                name,
                "katex"
                    | "katex-display"
                    | "mathjax"
                    | "mathjax_display"
                    | "mathjax_svg"
                    | "math"
                    | "math-inline"
                    | "math-display"
            )
        })
        || element.attr("data-mathml").is_some();
    let explicit = element
        .attr("data-latex")
        .or_else(|| element.attr("data-tex"));
    if !math_element && explicit.is_none() {
        return None;
    }
    let block = element.attr("display") == Some("block")
        || element.attr("display") == Some("true")
        || class.contains("display")
        || element
            .attr("type")
            .is_some_and(|kind| kind.contains("mode=display"))
        || (tag == "div" && math_element);
    let tex = explicit
        .map(str::to_owned)
        .or_else(|| {
            element
                .descendent_elements()
                .find(|el| {
                    el.value().name() == "annotation"
                        && el
                            .attr("encoding")
                            .is_some_and(|value| value.eq_ignore_ascii_case("application/x-tex"))
                })
                .map(|annotation| annotation.text().collect())
        })
        .or_else(|| {
            if tag == "math" {
                Some(latex(element, 0))
            } else if let Some(math) = element
                .descendent_elements()
                .find(|el| el.value().name() == "math")
            {
                Some(latex(math, 0))
            } else if let Some(encoded) = element.attr("data-mathml") {
                let fragment = scraper::Html::parse_fragment(encoded);
                let root = fragment.root_element();
                root.descendent_elements()
                    .find(|el| el.value().name() == "math")
                    .map(|math| latex(math, 0))
            } else if tag == "script" {
                Some(element.text().collect())
            } else {
                None
            }
        })?;
    let tex = tex.trim();
    if tex.is_empty() {
        return None;
    }
    Some((delimit(tex, block), block || tex.starts_with("$$")))
}

fn delimit(tex: &str, block: bool) -> String {
    if tex.starts_with('$') && tex.ends_with('$') {
        tex.to_owned()
    } else if block {
        format!("$$\n{tex}\n$$")
    } else {
        format!("${tex}$")
    }
}

/// Preserve explicit TeX delimiters in ordinary prose outside code.
pub(super) fn escape_prose(text: &str) -> String {
    use super::super::dom::collapse;

    let mut remaining = text;
    let mut output = String::new();
    while let Some((start, display)) = remaining.match_indices('\\').find_map(|(offset, _)| {
        match remaining.as_bytes().get(offset + 1) {
            Some(b'(') => Some((offset, false)),
            Some(b'[') => Some((offset, true)),
            _ => None,
        }
    }) {
        let end_marker = if display { "\\]" } else { "\\)" };
        let Some(end) = remaining[start + 2..].find(end_marker) else {
            break;
        };
        let end = start + 2 + end;
        output.push_str(&super::escape(&collapse(&remaining[..start])));
        if display {
            output.push_str("\n\n");
        }
        output.push_str(&delimit(remaining[start + 2..end].trim(), display));
        if display {
            output.push_str("\n\n");
        }
        remaining = &remaining[end + 2..];
    }
    output.push_str(&super::escape(&collapse(remaining)));
    output
}

fn latex(element: ElementRef<'_>, depth: usize) -> String {
    if depth >= 64 {
        return String::new();
    }
    let tag = element.value().name();
    if matches!(tag, "annotation" | "annotation-xml") {
        return String::new();
    }
    let terms: Vec<_> = element
        .children()
        .filter_map(|node| {
            if let Some(el) = ElementRef::wrap(node) {
                Some(latex(el, depth + 1))
            } else {
                node.value().as_text().map(|text| translate(text.trim()))
            }
        })
        .filter(|term| !term.is_empty())
        .collect();
    match (tag, terms.as_slice()) {
        ("mfrac", [numerator, denominator, ..]) => {
            format!("\\frac{{{numerator}}}{{{denominator}}}")
        }
        ("msub" | "munder", [base, sub, ..]) => format!("{base}_{{{sub}}}"),
        ("msup" | "mover", [base, sup, ..]) => format!("{base}^{{{sup}}}"),
        ("msubsup" | "munderover", [base, sub, sup, ..]) => format!("{base}_{{{sub}}}^{{{sup}}}"),
        ("msqrt", _) => format!("\\sqrt{{{}}}", terms.concat()),
        ("mroot", [base, root, ..]) => format!("\\sqrt[{root}]{{{base}}}"),
        ("mtext", _) => format!("\\text{{{}}}", terms.join(" ")),
        ("mfenced", _) => format!(
            "\\left{}{}\\right{}",
            element.attr("open").unwrap_or("("),
            terms.join(","),
            element.attr("close").unwrap_or(")")
        ),
        ("mtr", _) => terms.join(" & "),
        ("mtable", _) => format!("\\begin{{matrix}}{}\\end{{matrix}}", terms.join(" \\\\ ")),
        _ => terms.concat(),
    }
}

fn translate(text: &str) -> String {
    let mut output = String::new();
    for symbol in text.chars() {
        output.push_str(match symbol {
            '−' => "-",
            '×' => "\\times ",
            '÷' => "\\div ",
            '±' => "\\pm ",
            '≤' => "\\le ",
            '≥' => "\\ge ",
            '≠' => "\\ne ",
            '≈' => "\\approx ",
            '∑' => "\\sum ",
            '∏' => "\\prod ",
            '∫' => "\\int ",
            '∞' => "\\infty ",
            'α' => "\\alpha ",
            'β' => "\\beta ",
            'γ' => "\\gamma ",
            'δ' => "\\delta ",
            'θ' => "\\theta ",
            'λ' => "\\lambda ",
            'μ' => "\\mu ",
            'π' => "\\pi ",
            'σ' => "\\sigma ",
            'φ' => "\\phi ",
            'ω' => "\\omega ",
            _ => {
                output.push(symbol);
                continue;
            }
        });
    }
    output
}
