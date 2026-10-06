//! Reflection of content attributes as IDL attributes
//! (<https://html.spec.whatwg.org/multipage/#reflect>). The generated glue
//! calls these for every attribute the HTML IDL marks `[Reflect*]`.

use catpaw_dom::NodeId;
use catpaw_js::{Exception, Fallible, ObjectId};

use crate::element::{self, TokenListObject};
use crate::page::Cx;

/// The largest value an `unsigned long` reflecting attribute may hold.
const MAX_REFLECTED: u32 = 2_147_483_647;

fn is_html_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0C' | '\r')
}

/// <https://html.spec.whatwg.org/multipage/#rules-for-parsing-integers>
pub(crate) fn parse_integer(input: &str) -> Option<i64> {
    let s = input.trim_start_matches(is_html_whitespace);
    let (negative, rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let digits = &rest[..end];
    if digits.is_empty() {
        return None;
    }
    // Saturate on overflow; callers range-check the result.
    let value = digits.parse::<i64>().unwrap_or(i64::MAX);
    Some(if negative { -value } else { value })
}

/// <https://html.spec.whatwg.org/multipage/#rules-for-parsing-floating-point-number-values>
pub(crate) fn parse_float(input: &str) -> Option<f64> {
    let s = input.trim_start_matches(is_html_whitespace);
    let bytes = s.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'-' | b'+')) {
        i += 1;
    }
    let int_start = i;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let mut seen_digit = i > int_start;
    if bytes.get(i) == Some(&b'.') && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i += 1;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        seen_digit = true;
    }
    if !seen_digit {
        return None;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(bytes.get(j), Some(b'-' | b'+')) {
            j += 1;
        }
        if bytes.get(j).is_some_and(u8::is_ascii_digit) {
            while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
            i = j;
        }
    }
    s[..i]
        .trim_start_matches('+')
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

pub fn get_string(cx: &mut Cx<'_>, this: NodeId, attr: &str) -> Fallible<String> {
    Ok(element::get_attr(cx, this, attr).unwrap_or_default())
}

pub fn set_string(cx: &mut Cx<'_>, this: NodeId, attr: &str, value: String) -> Fallible<()> {
    element::set_attr(cx, this, attr, value)
}

pub fn get_nullable_string(cx: &mut Cx<'_>, this: NodeId, attr: &str) -> Fallible<Option<String>> {
    Ok(element::get_attr(cx, this, attr))
}

pub fn set_nullable_string(
    cx: &mut Cx<'_>,
    this: NodeId,
    attr: &str,
    value: Option<String>,
) -> Fallible<()> {
    match value {
        Some(value) => element::set_attr(cx, this, attr, value),
        None => {
            element::remove_attr(cx, this, attr);
            Ok(())
        }
    }
}

pub fn get_bool(cx: &mut Cx<'_>, this: NodeId, attr: &str) -> Fallible<bool> {
    Ok(element::get_attr(cx, this, attr).is_some())
}

pub fn set_bool(cx: &mut Cx<'_>, this: NodeId, attr: &str, value: bool) -> Fallible<()> {
    if value {
        element::set_attr(cx, this, attr, String::new())
    } else {
        element::remove_attr(cx, this, attr);
        Ok(())
    }
}

pub fn get_long(
    cx: &mut Cx<'_>,
    this: NodeId,
    attr: &str,
    default: i32,
    limit: &str,
) -> Fallible<i32> {
    let non_negative = limit == "non_negative";
    // A non-negative attribute without an explicit default reads as -1.
    let default = if non_negative && default == 0 {
        -1
    } else {
        default
    };
    let parsed = element::get_attr(cx, this, attr)
        .and_then(|v| parse_integer(&v))
        .and_then(|v| i32::try_from(v).ok())
        .filter(|v| !non_negative || *v >= 0);
    Ok(parsed.unwrap_or(default))
}

pub fn set_long(
    cx: &mut Cx<'_>,
    this: NodeId,
    attr: &str,
    value: i32,
    limit: &str,
) -> Fallible<()> {
    if limit == "non_negative" && value < 0 {
        return Err(Exception::index_size(format!(
            "The value {value} is negative"
        )));
    }
    element::set_attr(cx, this, attr, value.to_string())
}

pub fn get_unsigned_long(
    cx: &mut Cx<'_>,
    this: NodeId,
    attr: &str,
    default: u32,
    limit: &str,
) -> Fallible<u32> {
    let positive = limit != "none";
    let default = if positive && default == 0 { 1 } else { default };
    let minimum = if positive { 1 } else { 0 };
    let parsed = element::get_attr(cx, this, attr)
        .and_then(|v| parse_integer(&v))
        .filter(|v| (minimum..=i64::from(MAX_REFLECTED)).contains(v))
        .map(|v| v as u32);
    Ok(parsed.unwrap_or(default))
}

pub fn set_unsigned_long(
    cx: &mut Cx<'_>,
    this: NodeId,
    attr: &str,
    value: u32,
    default: u32,
    limit: &str,
) -> Fallible<()> {
    let positive = limit != "none";
    let default = if positive && default == 0 { 1 } else { default };
    if limit == "positive" && value == 0 {
        return Err(Exception::index_size("The value must be greater than zero"));
    }
    let out_of_range = value > MAX_REFLECTED || (positive && value == 0);
    let value = if out_of_range { default } else { value };
    element::set_attr(cx, this, attr, value.to_string())
}

pub fn get_double(
    cx: &mut Cx<'_>,
    this: NodeId,
    attr: &str,
    default: f64,
    limit: &str,
) -> Fallible<f64> {
    let positive = limit != "none";
    let parsed = element::get_attr(cx, this, attr)
        .and_then(|v| parse_float(&v))
        .filter(|v| !positive || *v > 0.0);
    Ok(parsed.unwrap_or(default))
}

pub fn set_double(
    cx: &mut Cx<'_>,
    this: NodeId,
    attr: &str,
    value: f64,
    limit: &str,
) -> Fallible<()> {
    if limit != "none" && value <= 0.0 {
        return Ok(());
    }
    element::set_attr(cx, this, attr, value.to_string())
}

/// A URL attribute: the value resolved against the document base URL, or the
/// raw value when it does not parse.
pub fn get_url(cx: &mut Cx<'_>, this: NodeId, attr: &str) -> Fallible<String> {
    let Some(value) = element::get_attr(cx, this, attr) else {
        return Ok(String::new());
    };
    Ok(match cx.page.resolve_url(&value) {
        Some(url) => url.to_string(),
        None => value,
    })
}

pub fn get_token_list(cx: &mut Cx<'_>, this: NodeId, attr: &str) -> Fallible<ObjectId> {
    Ok(cx.page.alloc(TokenListObject {
        element: this,
        attr: attr.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_integers_like_html() {
        assert_eq!(parse_integer("42"), Some(42));
        assert_eq!(parse_integer("  -7px"), Some(-7));
        assert_eq!(parse_integer("+3"), Some(3));
        assert_eq!(parse_integer("x1"), None);
        assert_eq!(parse_integer(""), None);
        assert_eq!(parse_integer("99999999999999999999"), Some(i64::MAX));
    }

    #[test]
    fn parses_floats_like_html() {
        assert_eq!(parse_float("1.5"), Some(1.5));
        assert_eq!(parse_float(" -2e3x"), Some(-2000.0));
        assert_eq!(parse_float(".5"), Some(0.5));
        assert_eq!(parse_float("5."), Some(5.0));
        assert_eq!(parse_float("1e"), Some(1.0));
        assert_eq!(parse_float("abc"), None);
    }
}
