//! Lightweight zero-regex JSON Schema string `format` validators.

pub(crate) fn check_format(format: &str, value: &str) -> bool {
    match format {
        "uuid" => is_uuid(value),
        "date" => is_date(value),
        "date-time" => is_date_time(value),
        "email" => is_email(value),
        "uri" => is_uri(value),
        "url" => is_url(value),
        "json-pointer" => is_json_pointer(value),
        // Unknown formats are annotations in JSON Schema and do not fail validation.
        _ => true,
    }
}

fn is_uuid(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        if matches!(i, 8 | 13 | 18 | 23) {
            if b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

fn is_date(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let Some(year) = parse_digits(&bytes[0..4]) else {
        return false;
    };
    let Some(month) = parse_digits(&bytes[5..7]) else {
        return false;
    };
    let Some(day) = parse_digits(&bytes[8..10]) else {
        return false;
    };
    if !(1..=12).contains(&month) {
        return false;
    }
    let max_day = days_in_month(year, month);
    (1..=max_day).contains(&day)
}

fn is_date_time(s: &str) -> bool {
    let bytes = s.as_bytes();
    // Minimum: YYYY-MM-DDTHH:MM:SSZ (20 bytes)
    if bytes.len() < 20 {
        return false;
    }
    if !is_date(&s[0..10]) {
        return false;
    }
    if !matches!(bytes[10], b'T' | b't') {
        return false;
    }
    if bytes[13] != b':' || bytes[16] != b':' {
        return false;
    }
    let Some(hour) = parse_digits(&bytes[11..13]) else {
        return false;
    };
    let Some(minute) = parse_digits(&bytes[14..16]) else {
        return false;
    };
    let Some(second) = parse_digits(&bytes[17..19]) else {
        return false;
    };
    if hour > 23 || minute > 59 || second > 60 {
        return false;
    }

    let mut pos = 19;
    if pos < bytes.len() && bytes[pos] == b'.' {
        pos += 1;
        let frac_start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos == frac_start {
            return false;
        }
    }

    if pos >= bytes.len() {
        return false;
    }
    match bytes[pos] {
        b'Z' | b'z' => {
            if pos + 1 != bytes.len() {
                return false;
            }
            if second == 60 {
                return hour == 23 && minute == 59;
            }
            true
        }
        b'+' | b'-' => {
            if pos + 6 != bytes.len() || bytes[pos + 3] != b':' {
                return false;
            }
            let Some(off_h) = parse_digits(&bytes[pos + 1..pos + 3]) else {
                return false;
            };
            let Some(off_m) = parse_digits(&bytes[pos + 4..pos + 6]) else {
                return false;
            };
            if off_h > 23 || off_m > 59 {
                return false;
            }
            if second == 60 {
                let sign: i32 = if bytes[pos] == b'+' { 1 } else { -1 };
                let utc_min =
                    (hour as i32 * 60 + minute as i32 - sign * (off_h as i32 * 60 + off_m as i32))
                        .rem_euclid(24 * 60);
                return utc_min == 23 * 60 + 59;
            }
            true
        }
        _ => false,
    }
}

fn is_email(s: &str) -> bool {
    if s.is_empty() || s.len() > 254 {
        return false;
    }
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    if domain.contains('@') {
        return false;
    }
    if local.is_empty() || local.len() > 64 || domain.is_empty() || domain.len() > 255 {
        return false;
    }
    if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return false;
    }
    if !local.bytes().all(is_email_local_byte) {
        return false;
    }
    is_valid_domain(domain)
}

fn is_email_local_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'/'
                | b'='
                | b'?'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
                | b'.'
        )
}

fn is_valid_domain(domain: &str) -> bool {
    if domain.starts_with('.') || domain.ends_with('.') || !domain.contains('.') {
        return false;
    }
    let mut last_label = "";
    for label in domain.split('.') {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        let bytes = label.as_bytes();
        if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
            return false;
        }
        if !bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-') {
            return false;
        }
        last_label = label;
    }
    last_label.len() >= 2 && last_label.bytes().any(|b| b.is_ascii_alphabetic())
}

fn is_uri(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once(':') else {
        return false;
    };
    if !is_valid_scheme(scheme) {
        return false;
    }
    if rest.is_empty() {
        return false;
    }
    if !is_valid_uri_chars(rest) {
        return false;
    }
    if let Some(after_slashes) = rest.strip_prefix("//") {
        let authority_end = after_slashes
            .find(['/', '?', '#'])
            .unwrap_or(after_slashes.len());
        let authority = &after_slashes[..authority_end];
        if !is_valid_authority(authority, scheme.eq_ignore_ascii_case("file")) {
            return false;
        }
    }
    true
}

fn is_url(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once("://") else {
        return false;
    };
    if !is_valid_scheme(scheme) || !is_valid_uri_chars(rest) {
        return false;
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    is_valid_authority(authority, scheme.eq_ignore_ascii_case("file"))
}

fn is_valid_scheme(scheme: &str) -> bool {
    let bytes = scheme.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

fn is_valid_uri_chars(rest: &str) -> bool {
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b <= 0x20 || b >= 0x7f || matches!(b, b'<' | b'>' | b'"' | b'{' | b'}' | b'|' | b'\\' | b'^' | b'`') {
            return false;
        }
        if b == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return false;
            }
            i += 3;
            continue;
        }
        i += 1;
    }
    true
}

fn is_valid_authority(authority: &str, allow_empty_host: bool) -> bool {
    let hostport = match authority.rsplit_once('@') {
        Some((_, hp)) => hp,
        None => authority,
    };
    if hostport.is_empty() {
        return allow_empty_host;
    }
    if let Some(rest) = hostport.strip_prefix('[') {
        let Some((ip, tail)) = rest.split_once(']') else {
            return false;
        };
        if ip.is_empty() || !ip.bytes().all(|b| b.is_ascii_hexdigit() || matches!(b, b':' | b'.')) {
            return false;
        }
        return match tail.strip_prefix(':') {
            Some(port) => port.is_empty() || port.bytes().all(|b| b.is_ascii_digit()),
            None => tail.is_empty(),
        };
    }
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (hostport, None),
    };
    if host.is_empty() && !allow_empty_host {
        return false;
    }
    if let Some(port) = port
        && !port.is_empty()
        && !port.bytes().all(|b| b.is_ascii_digit())
    {
        return false;
    }
    true
}

fn is_json_pointer(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    let bytes = s.as_bytes();
    if bytes[0] != b'/' {
        return false;
    }
    let mut i = 1;
    while i < bytes.len() {
        if bytes[i] == b'~' {
            if i + 1 >= bytes.len() || !matches!(bytes[i + 1], b'0' | b'1') {
                return false;
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    true
}

fn parse_digits(bytes: &[u8]) -> Option<u32> {
    let mut acc: u32 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc * 10 + (b - b'0') as u32;
    }
    Some(acc)
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}
