use std::collections::{BTreeSet, HashMap};

pub(crate) struct Document<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub properties: &'a str,
}

/// Rank matching indices with per-field length normalization and name bonuses.
pub(crate) fn score_documents(query: &str, documents: &[Document<'_>]) -> Vec<usize> {
    const WEIGHTS: [f64; 3] = [3.0, 1.0, 0.5];
    const K1: f64 = 1.2;
    const B: f64 = 0.75;

    let query_tokens = tokenize(query);
    if query_tokens.is_empty() {
        return (0..documents.len()).collect();
    }
    let terms: BTreeSet<&str> = query_tokens.iter().map(String::as_str).collect();
    let fields: Vec<_> = documents
        .iter()
        .map(|doc| [doc.name, doc.description, doc.properties].map(tokenize))
        .collect();
    let count = fields.len() as f64;
    let avg_lengths: [f64; 3] = std::array::from_fn(|i| {
        fields.iter().map(|doc| doc[i].len()).sum::<usize>() as f64 / count.max(1.0)
    });
    let mut frequencies: HashMap<&str, usize> = HashMap::new();
    for document in &fields {
        let present: BTreeSet<_> = document.iter().flatten().map(String::as_str).collect();
        for term in present {
            *frequencies.entry(term).or_default() += 1;
        }
    }

    let mut ranked = Vec::new();
    for (index, doc) in fields.iter().enumerate() {
        let mut score = 0.0;
        for &term in &terms {
            let df = frequencies.get(term).copied().unwrap_or_default() as f64;
            let idf = ((count - df + 0.5) / (df + 0.5)).ln_1p();
            for (i, tokens) in doc.iter().enumerate() {
                let tf = tokens.iter().filter(|token| token.as_str() == term).count() as f64;
                let length_norm = 1.0 - B + B * tokens.len() as f64 / avg_lengths[i].max(1.0);
                score += WEIGHTS[i] * idf * tf * (K1 + 1.0) / (tf + K1 * length_norm);
            }
            if doc[0].iter().any(|token| token == term) {
                score += 1.0;
            } else if term.chars().count() >= 2
                && doc[0].iter().any(|token| token.starts_with(term))
            {
                score += 0.5;
            }
        }
        if doc[0] == query_tokens {
            score += 3.0;
        } else if doc[0].starts_with(&query_tokens) {
            score += 1.0;
        }
        if score > 0.0 {
            ranked.push((index, score));
        }
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().map(|(index, _)| index).collect()
}

fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut previous = None::<char>;
    let mut cjk = None::<char>;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if is_cjk(ch) {
            if !word.is_empty() {
                tokens.push(std::mem::take(&mut word));
            }
            tokens.push(ch.to_string());
            if let Some(prev) = cjk {
                tokens.push(String::from_iter([prev, ch]));
            }
            cjk = Some(ch);
            previous = None;
            continue;
        }
        cjk = None;
        if !ch.is_alphanumeric() {
            if !word.is_empty() {
                tokens.push(std::mem::take(&mut word));
            }
            previous = None;
            continue;
        }
        let boundary = ch.is_uppercase()
            && previous.is_some_and(|prev| {
                prev.is_lowercase()
                    || prev.is_numeric()
                    || (prev.is_uppercase() && chars.peek().is_some_and(|c| c.is_lowercase()))
            });
        if boundary && !word.is_empty() {
            tokens.push(std::mem::take(&mut word));
        }
        word.extend(ch.to_lowercase());
        previous = Some(ch);
    }
    if !word.is_empty() {
        tokens.push(word);
    }
    tokens
}

fn is_cjk(ch: char) -> bool {
    matches!(ch,
        '\u{3040}'..='\u{30ff}' | '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}'
        | '\u{ac00}'..='\u{d7af}' | '\u{f900}'..='\u{faff}' | '\u{20000}'..='\u{2fa1f}'
        | '\u{30000}'..='\u{323af}'
    )
}
