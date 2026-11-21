//! Zero-dependency Thompson NFA regular expression engine for JSON Schema `pattern`.
//!
//! Guarantees linear-time $O(|input| \cdot |NFA|)$ matching (immune to ReDoS catastrophic
//! backtracking) and rejects invalid patterns as well as unsupported lookaround / backreference
//! assertions at schema compile time.

const MAX_NFA_STATES: usize = 16_384;
const MAX_QUANTIFIER_BOUND: usize = 4_096;

#[derive(Debug, Clone)]
pub(crate) struct CompiledPattern {
    insts: Vec<Inst>,
    start: usize,
}

#[derive(Debug, Clone)]
enum Inst {
    Char(char, usize),
    Dot(usize),
    Class(CharClass, usize),
    Anchor(Anchor, usize),
    Split(usize, usize),
    Match,
}

#[derive(Debug, Clone, Copy)]
enum Anchor {
    Start,
    End,
    WordBoundary,
    NonWordBoundary,
}

#[derive(Debug, Clone)]
struct CharClass {
    negated: bool,
    items: Vec<ClassItem>,
}

#[derive(Debug, Clone, Copy)]
enum ClassItem {
    Single(char),
    Range(char, char),
    Digit,
    NotDigit,
    Word,
    NotWord,
    Space,
    NotSpace,
}

impl CharClass {
    fn single(item: ClassItem) -> Self {
        Self {
            negated: false,
            items: vec![item],
        }
    }

    fn matches(&self, ch: char) -> bool {
        let hit = self.items.iter().any(|item| match *item {
            ClassItem::Single(c) => ch == c,
            ClassItem::Range(a, b) => ch >= a && ch <= b,
            ClassItem::Digit => ch.is_ascii_digit(),
            ClassItem::NotDigit => !ch.is_ascii_digit(),
            ClassItem::Word => is_word_char(ch),
            ClassItem::NotWord => !is_word_char(ch),
            ClassItem::Space => is_ecma_whitespace(ch),
            ClassItem::NotSpace => !is_ecma_whitespace(ch),
        });
        if self.negated { !hit } else { hit }
    }
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn is_ecma_whitespace(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c' | '\u{00a0}' | '\u{feff}'
    ) || c.is_whitespace()
}

fn anchor_matches(anchor: Anchor, chars: &[char], pos: usize) -> bool {
    match anchor {
        Anchor::Start => pos == 0,
        Anchor::End => pos == chars.len(),
        Anchor::WordBoundary | Anchor::NonWordBoundary => {
            let prev = pos > 0 && is_word_char(chars[pos - 1]);
            let curr = pos < chars.len() && is_word_char(chars[pos]);
            let is_boundary = prev != curr;
            match anchor {
                Anchor::WordBoundary => is_boundary,
                Anchor::NonWordBoundary => !is_boundary,
                _ => unreachable!(),
            }
        }
    }
}

impl CompiledPattern {
    pub(crate) fn is_match(&self, input: &str) -> bool {
        let chars: Vec<char> = input.chars().collect();
        let n = chars.len();
        let mut visited = vec![0usize; self.insts.len()];
        let mut epoch = 0usize;
        let mut curr: Vec<usize> = Vec::new();
        let mut next: Vec<usize> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();

        for i in 0..=n {
            epoch += 1;
            // JSON Schema `pattern` is unanchored unless `^` / `$` are specified.
            stack.push(self.start);
            for &pc in &curr {
                stack.push(pc);
            }
            curr.clear();

            while let Some(pc) = stack.pop() {
                if visited[pc] == epoch {
                    continue;
                }
                visited[pc] = epoch;
                match &self.insts[pc] {
                    Inst::Match => return true,
                    Inst::Split(a, b) => {
                        stack.push(*b);
                        stack.push(*a);
                    }
                    Inst::Anchor(anchor, target) => {
                        if anchor_matches(*anchor, &chars, i) {
                            stack.push(*target);
                        }
                    }
                    Inst::Char(_, _) | Inst::Dot(_) | Inst::Class(_, _) => {
                        curr.push(pc);
                    }
                }
            }

            if i == n {
                break;
            }

            let ch = chars[i];
            next.clear();
            for &pc in &curr {
                match &self.insts[pc] {
                    Inst::Char(expected, target) if *expected == ch => next.push(*target),
                    Inst::Dot(target) if !is_line_terminator(ch) => next.push(*target),
                    Inst::Class(class, target) if class.matches(ch) => next.push(*target),
                    _ => {}
                }
            }
            std::mem::swap(&mut curr, &mut next);
        }

        false
    }
}

pub(crate) fn compile_pattern(pattern: &str) -> Result<CompiledPattern, String> {
    let mut parser = Parser {
        chars: pattern.chars().collect(),
        pos: 0,
    };
    let ast = parser.parse_alt()?;
    if parser.pos < parser.chars.len() {
        return Err(format!(
            "unexpected '{}' at offset {}",
            parser.chars[parser.pos], parser.pos
        ));
    }

    let mut compiler = Compiler {
        insts: vec![Inst::Match],
    };
    let start = compiler.compile(&ast, 0)?;
    Ok(CompiledPattern {
        insts: compiler.insts,
        start,
    })
}

pub(crate) fn is_match(pattern: &str, input: &str) -> bool {
    match compile_pattern(pattern) {
        Ok(compiled) => compiled.is_match(input),
        Err(_) => false,
    }
}

#[derive(Debug, Clone)]
enum Expr {
    Empty,
    Char(char),
    Dot,
    Class(CharClass),
    Anchor(Anchor),
    Concat(Vec<Expr>),
    Alt(Vec<Expr>),
    Repeat {
        inner: Box<Expr>,
        min: usize,
        max: Option<usize>,
    },
}

struct Compiler {
    insts: Vec<Inst>,
}

impl Compiler {
    fn push(&mut self, inst: Inst) -> Result<usize, String> {
        if self.insts.len() >= MAX_NFA_STATES {
            return Err("pattern exceeds maximum compiled NFA size".into());
        }
        let idx = self.insts.len();
        self.insts.push(inst);
        Ok(idx)
    }

    fn compile(&mut self, expr: &Expr, next: usize) -> Result<usize, String> {
        match expr {
            Expr::Empty => Ok(next),
            Expr::Char(c) => self.push(Inst::Char(*c, next)),
            Expr::Dot => self.push(Inst::Dot(next)),
            Expr::Class(cc) => self.push(Inst::Class(cc.clone(), next)),
            Expr::Anchor(a) => self.push(Inst::Anchor(*a, next)),
            Expr::Concat(parts) => {
                let mut cur = next;
                for part in parts.iter().rev() {
                    cur = self.compile(part, cur)?;
                }
                Ok(cur)
            }
            Expr::Alt(branches) => {
                if branches.is_empty() {
                    return Ok(next);
                }
                let mut iter = branches.iter().rev();
                let last = iter.next().unwrap();
                let mut cur = self.compile(last, next)?;
                for branch in iter {
                    let b = self.compile(branch, next)?;
                    cur = self.push(Inst::Split(b, cur))?;
                }
                Ok(cur)
            }
            Expr::Repeat { inner, min, max } => {
                let tail = match max {
                    None => {
                        let split_idx = self.push(Inst::Split(0, next))?;
                        let body = self.compile(inner, split_idx)?;
                        self.insts[split_idx] = Inst::Split(body, next);
                        split_idx
                    }
                    Some(max_val) => {
                        let opt_count = max_val.saturating_sub(*min);
                        let mut cur = next;
                        for _ in 0..opt_count {
                            let body = self.compile(inner, cur)?;
                            cur = self.push(Inst::Split(body, next))?;
                        }
                        cur
                    }
                };
                let mut cur = tail;
                for _ in 0..*min {
                    cur = self.compile(inner, cur)?;
                }
                Ok(cur)
            }
        }
    }
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn next(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += 1;
        Some(ch)
    }

    fn parse_alt(&mut self) -> Result<Expr, String> {
        let mut branches = vec![self.parse_seq()?];
        while self.peek() == Some('|') {
            self.pos += 1;
            branches.push(self.parse_seq()?);
        }
        if branches.len() == 1 {
            Ok(branches.pop().unwrap())
        } else {
            Ok(Expr::Alt(branches))
        }
    }

    fn parse_seq(&mut self) -> Result<Expr, String> {
        let mut parts = Vec::new();
        while let Some(ch) = self.peek() {
            if ch == ')' || ch == '|' {
                break;
            }
            let atom = self.parse_atom()?;
            if let Some((min, max)) = self.try_parse_quantifier()? {
                if matches!(atom, Expr::Anchor(_)) {
                    return Err("cannot apply quantifier to anchor assertion".into());
                }
                parts.push(Expr::Repeat {
                    inner: Box::new(atom),
                    min,
                    max,
                });
            } else {
                parts.push(atom);
            }
        }
        match parts.len() {
            0 => Ok(Expr::Empty),
            1 => Ok(parts.pop().unwrap()),
            _ => Ok(Expr::Concat(parts)),
        }
    }

    fn parse_atom(&mut self) -> Result<Expr, String> {
        let ch = self
            .peek()
            .ok_or_else(|| "unexpected end of pattern".to_string())?;
        match ch {
            '*' | '+' | '?' => Err(format!("nothing to repeat before '{ch}'")),
            '^' => {
                self.pos += 1;
                Ok(Expr::Anchor(Anchor::Start))
            }
            '$' => {
                self.pos += 1;
                Ok(Expr::Anchor(Anchor::End))
            }
            '.' => {
                self.pos += 1;
                Ok(Expr::Dot)
            }
            '(' => self.parse_group(),
            '[' => self.parse_char_class(),
            '\\' => self.parse_escape(),
            '{' => {
                if self.looks_like_quantifier_brace() {
                    return Err("nothing to repeat before '{'".into());
                }
                self.pos += 1;
                Ok(Expr::Char('{'))
            }
            _ => {
                self.pos += 1;
                Ok(Expr::Char(ch))
            }
        }
    }

    fn looks_like_quantifier_brace(&self) -> bool {
        self.peek() == Some('{') && self.peek_at(1).is_some_and(|c| c.is_ascii_digit())
    }

    fn try_parse_quantifier(&mut self) -> Result<Option<(usize, Option<usize>)>, String> {
        let quant = match self.peek() {
            Some('*') => {
                self.pos += 1;
                Some((0, None))
            }
            Some('+') => {
                self.pos += 1;
                Some((1, None))
            }
            Some('?') => {
                self.pos += 1;
                Some((0, Some(1)))
            }
            Some('{') => self.try_parse_brace_quantifier()?,
            _ => None,
        };
        if quant.is_some() && self.peek() == Some('?') {
            // Non-greedy modifier; same acceptance language for boolean matching.
            self.pos += 1;
        }
        Ok(quant)
    }

    fn try_parse_brace_quantifier(&mut self) -> Result<Option<(usize, Option<usize>)>, String> {
        if !self.looks_like_quantifier_brace() {
            return Ok(None);
        }
        self.pos += 1; // consume '{'
        let min = self.parse_decimal_bound()?;
        match self.peek() {
            Some('}') => {
                self.pos += 1;
                Ok(Some((min, Some(min))))
            }
            Some(',') => {
                self.pos += 1;
                if self.peek() == Some('}') {
                    self.pos += 1;
                    Ok(Some((min, None)))
                } else {
                    let max = self.parse_decimal_bound()?;
                    if self.peek() != Some('}') {
                        return Err("unclosed or invalid '{m,n}' quantifier".into());
                    }
                    self.pos += 1;
                    if max < min {
                        return Err(format!(
                            "quantifier minimum {min} is greater than maximum {max}"
                        ));
                    }
                    Ok(Some((min, Some(max))))
                }
            }
            _ => Err("unclosed or invalid '{m,n}' quantifier".into()),
        }
    }

    fn parse_decimal_bound(&mut self) -> Result<usize, String> {
        let start = self.pos;
        let mut val: usize = 0;
        while let Some(c) = self.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            self.pos += 1;
            let digit = (c as u8 - b'0') as usize;
            val = val
                .checked_mul(10)
                .and_then(|v| v.checked_add(digit))
                .ok_or_else(|| "quantifier bound overflow".to_string())?;
            if val > MAX_QUANTIFIER_BOUND {
                return Err(format!(
                    "quantifier bound exceeds maximum supported limit ({MAX_QUANTIFIER_BOUND})"
                ));
            }
        }
        if self.pos == start {
            return Err("expected decimal bound in quantifier".into());
        }
        Ok(val)
    }

    fn parse_group(&mut self) -> Result<Expr, String> {
        self.pos += 1; // consume '('
        if self.peek() == Some('?') {
            match self.peek_at(1) {
                Some(':') => {
                    self.pos += 2;
                }
                Some('=') => {
                    return Err("unsupported lookahead syntax '(?=...)'".into());
                }
                Some('!') => {
                    return Err("unsupported negative lookahead syntax '(?!...)'".into());
                }
                Some('<') => match self.peek_at(2) {
                    Some('=') => {
                        return Err("unsupported lookbehind syntax '(?<=...)'".into());
                    }
                    Some('!') => {
                        return Err("unsupported negative lookbehind syntax '(?<!...)'".into());
                    }
                    _ => {
                        return Err("unsupported group syntax '(?<...)'".into());
                    }
                },
                _ => {
                    return Err("unsupported group extension '(?...'".into());
                }
            }
        }
        let inner = self.parse_alt()?;
        if self.next() != Some(')') {
            return Err("unclosed group '('".into());
        }
        Ok(inner)
    }

    fn parse_escape(&mut self) -> Result<Expr, String> {
        self.pos += 1; // consume '\'
        let ch = self
            .next()
            .ok_or_else(|| "trailing backslash in pattern".to_string())?;
        match ch {
            '1'..='9' => Err(format!("unsupported backreference '\\{ch}'")),
            '0' => {
                if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    Err("unsupported octal escape or backreference".into())
                } else {
                    Ok(Expr::Char('\0'))
                }
            }
            'k' => Err("unsupported named backreference '\\k'".into()),
            'p' | 'P' => Err(format!("unsupported Unicode property escape '\\{ch}'")),
            'd' => Ok(Expr::Class(CharClass::single(ClassItem::Digit))),
            'D' => Ok(Expr::Class(CharClass::single(ClassItem::NotDigit))),
            'w' => Ok(Expr::Class(CharClass::single(ClassItem::Word))),
            'W' => Ok(Expr::Class(CharClass::single(ClassItem::NotWord))),
            's' => Ok(Expr::Class(CharClass::single(ClassItem::Space))),
            'S' => Ok(Expr::Class(CharClass::single(ClassItem::NotSpace))),
            'b' => Ok(Expr::Anchor(Anchor::WordBoundary)),
            'B' => Ok(Expr::Anchor(Anchor::NonWordBoundary)),
            _ => Ok(Expr::Char(self.parse_char_escape_body(ch)?)),
        }
    }

    fn parse_char_escape_body(&mut self, ch: char) -> Result<char, String> {
        match ch {
            'n' => Ok('\n'),
            'r' => Ok('\r'),
            't' => Ok('\t'),
            'f' => Ok('\x0c'),
            'v' => Ok('\x0b'),
            'x' => self.parse_hex_char(2),
            'u' => {
                if self.peek() == Some('{') {
                    self.pos += 1;
                    let mut val: u32 = 0;
                    let mut digits = 0usize;
                    while let Some(c) = self.peek() {
                        if c == '}' {
                            break;
                        }
                        let d = hex_digit(c).ok_or_else(|| {
                            format!("invalid hex digit '{c}' in \\u{{...}} escape")
                        })?;
                        self.pos += 1;
                        digits += 1;
                        if digits > 6 {
                            return Err("\\u{...} code point out of range".into());
                        }
                        val = (val << 4) | (d as u32);
                    }
                    if digits == 0 || self.next() != Some('}') {
                        return Err("unclosed or empty \\u{...} escape".into());
                    }
                    char::from_u32(val).ok_or_else(|| format!("invalid Unicode scalar U+{val:X}"))
                } else {
                    self.parse_hex_char(4)
                }
            }
            'c' => {
                let ctrl = self
                    .next()
                    .ok_or_else(|| "incomplete \\c control escape".to_string())?;
                if ctrl.is_ascii_alphabetic() {
                    Ok(((ctrl as u8) & 0x1f) as char)
                } else {
                    Err(format!("invalid control escape '\\c{ctrl}'"))
                }
            }
            other => Ok(other),
        }
    }

    fn parse_hex_char(&mut self, count: usize) -> Result<char, String> {
        let mut val: u32 = 0;
        for _ in 0..count {
            let c = self
                .next()
                .ok_or_else(|| "incomplete hex escape sequence".to_string())?;
            let d = hex_digit(c).ok_or_else(|| format!("invalid hex digit '{c}'"))?;
            val = (val << 4) | (d as u32);
        }
        char::from_u32(val).ok_or_else(|| format!("invalid Unicode scalar U+{val:X}"))
    }

    fn parse_char_class(&mut self) -> Result<Expr, String> {
        self.pos += 1; // consume '['
        let negated = if self.peek() == Some('^') {
            self.pos += 1;
            true
        } else {
            false
        };

        let mut items = Vec::new();
        loop {
            let ch = self
                .peek()
                .ok_or_else(|| "unterminated character class '['".to_string())?;
            if ch == ']' {
                self.pos += 1;
                return Ok(Expr::Class(CharClass { negated, items }));
            }

            let first = self.parse_class_atom()?;
            if self.peek() == Some('-') && self.peek_at(1).is_some_and(|c| c != ']') {
                self.pos += 1; // consume '-'
                let second = self.parse_class_atom()?;
                match (first, second) {
                    (ClassItem::Single(start), ClassItem::Single(end)) => {
                        if start > end {
                            return Err(format!(
                                "character class range '{start}-{end}' out of order"
                            ));
                        }
                        items.push(ClassItem::Range(start, end));
                    }
                    _ => {
                        return Err("cannot use shorthand class inside character range".into());
                    }
                }
            } else {
                items.push(first);
            }
        }
    }

    fn parse_class_atom(&mut self) -> Result<ClassItem, String> {
        let ch = self
            .next()
            .ok_or_else(|| "unterminated character class '['".to_string())?;
        if ch != '\\' {
            return Ok(ClassItem::Single(ch));
        }
        let esc = self
            .next()
            .ok_or_else(|| "trailing backslash in character class".to_string())?;
        match esc {
            'd' => Ok(ClassItem::Digit),
            'D' => Ok(ClassItem::NotDigit),
            'w' => Ok(ClassItem::Word),
            'W' => Ok(ClassItem::NotWord),
            's' => Ok(ClassItem::Space),
            'S' => Ok(ClassItem::NotSpace),
            'b' => Ok(ClassItem::Single('\x08')),
            '0' => {
                if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    Err("unsupported octal escape in character class".into())
                } else {
                    Ok(ClassItem::Single('\0'))
                }
            }
            '1'..='9' => Err(format!("unsupported escape '\\{esc}' in character class")),
            'p' | 'P' => Err(format!("unsupported Unicode property escape '\\{esc}'")),
            other => Ok(ClassItem::Single(self.parse_char_escape_body(other)?)),
        }
    }
}

fn hex_digit(c: char) -> Option<u8> {
    match c {
        '0'..='9' => Some(c as u8 - b'0'),
        'a'..='f' => Some(c as u8 - b'a' + 10),
        'A'..='F' => Some(c as u8 - b'A' + 10),
        _ => None,
    }
}
