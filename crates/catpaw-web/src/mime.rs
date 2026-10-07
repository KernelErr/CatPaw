//! MIME types, parsed and serialized as the MIME Sniffing standard has it
//! (<https://mimesniff.spec.whatwg.org/#parsing-a-mime-type>).

/// A parsed MIME type: lowercase type and subtype, and the parameters in
/// order, with lowercase names and their values as given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MimeType {
    pub type_: String,
    pub subtype: String,
    pub parameters: Vec<(String, String)>,
}

fn is_http_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

fn is_quoted_string_token(c: char) -> bool {
    c == '\t' || (' '..='~').contains(&c) || ('\u{80}'..='\u{ff}').contains(&c)
}

/// Collects an HTTP quoted string whose opening quote is at `pos`: its
/// value (escapes resolved) and the position after its closing quote.
fn collect_quoted(chars: &[char], mut pos: usize) -> (String, usize) {
    let mut value = String::new();
    pos += 1;
    loop {
        while pos < chars.len() && chars[pos] != '"' && chars[pos] != '\\' {
            value.push(chars[pos]);
            pos += 1;
        }
        if pos >= chars.len() {
            break;
        }
        let c = chars[pos];
        pos += 1;
        if c == '"' {
            break;
        }
        // A backslash escapes what follows; at the end it stands for itself.
        match chars.get(pos) {
            Some(&escaped) => {
                value.push(escaped);
                pos += 1;
            }
            None => {
                value.push('\\');
                break;
            }
        }
    }
    (value, pos)
}

/// Parses `input`; `None` if it is not a MIME type.
pub fn parse(input: &str) -> Option<MimeType> {
    let chars: Vec<char> = input.trim_matches(is_http_whitespace).chars().collect();
    let mut pos = 0;
    while pos < chars.len() && chars[pos] != '/' {
        pos += 1;
    }
    let type_: String = chars[..pos].iter().collect();
    if type_.is_empty() || !type_.chars().all(is_token_char) || pos >= chars.len() {
        return None;
    }
    pos += 1;
    let subtype_start = pos;
    while pos < chars.len() && chars[pos] != ';' {
        pos += 1;
    }
    let subtype: String = chars[subtype_start..pos].iter().collect();
    let subtype = subtype.trim_end_matches(is_http_whitespace);
    if subtype.is_empty() || !subtype.chars().all(is_token_char) {
        return None;
    }
    let mut mime = MimeType {
        type_: type_.to_ascii_lowercase(),
        subtype: subtype.to_ascii_lowercase(),
        parameters: Vec::new(),
    };
    while pos < chars.len() {
        pos += 1;
        while pos < chars.len() && is_http_whitespace(chars[pos]) {
            pos += 1;
        }
        let name_start = pos;
        while pos < chars.len() && chars[pos] != ';' && chars[pos] != '=' {
            pos += 1;
        }
        let name: String = chars[name_start..pos].iter().collect();
        let name = name.to_ascii_lowercase();
        if pos < chars.len() {
            if chars[pos] == ';' {
                continue;
            }
            pos += 1;
        }
        if pos >= chars.len() {
            break;
        }
        let value = if chars[pos] == '"' {
            let (value, after) = collect_quoted(&chars, pos);
            pos = after;
            while pos < chars.len() && chars[pos] != ';' {
                pos += 1;
            }
            value
        } else {
            let value_start = pos;
            while pos < chars.len() && chars[pos] != ';' {
                pos += 1;
            }
            let value: String = chars[value_start..pos].iter().collect();
            let value = value.trim_end_matches(is_http_whitespace).to_string();
            if value.is_empty() {
                continue;
            }
            value
        };
        if !name.is_empty()
            && name.chars().all(is_token_char)
            && value.chars().all(is_quoted_string_token)
            && !mime.parameters.iter().any(|(n, _)| *n == name)
        {
            mime.parameters.push((name, value));
        }
    }
    Some(mime)
}

impl MimeType {
    /// `type/subtype`.
    pub fn essence(&self) -> String {
        format!("{}/{}", self.type_, self.subtype)
    }

    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.parameters
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Sets a parameter, in place if it exists.
    pub fn set_parameter(&mut self, name: &str, value: &str) {
        match self.parameters.iter_mut().find(|(n, _)| n == name) {
            Some((_, v)) => *v = value.to_string(),
            None => self.parameters.push((name.to_string(), value.to_string())),
        }
    }

    /// <https://mimesniff.spec.whatwg.org/#serialize-a-mime-type>
    pub fn serialize(&self) -> String {
        let mut out = self.essence();
        for (name, value) in &self.parameters {
            out.push(';');
            out.push_str(name);
            out.push('=');
            if !value.is_empty() && value.chars().all(is_token_char) {
                out.push_str(value);
            } else {
                out.push('"');
                for c in value.chars() {
                    if c == '"' || c == '\\' {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push('"');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_serializes() {
        let mime = parse("YO/yo;charset=x;yo=YO; X=y").unwrap();
        assert_eq!(mime.essence(), "yo/yo");
        assert_eq!(mime.serialize(), "yo/yo;charset=x;yo=YO;x=y");
        let mut fixed = parse(
            "text/x-pink-unicorn; charset=windows-1252; charset=bogus; notrelated; charset=ascii",
        )
        .unwrap();
        fixed.set_parameter("charset", "UTF-8");
        assert_eq!(fixed.serialize(), "text/x-pink-unicorn;charset=UTF-8");
        assert_eq!(
            parse("text/plain;charset=\" utf-8\"")
                .unwrap()
                .parameter("charset"),
            Some(" utf-8")
        );
        assert_eq!(
            parse("text/plain;charset=\"a\\\"b\"").unwrap().serialize(),
            "text/plain;charset=\"a\\\"b\""
        );
        assert_eq!(parse("text/plain;charset=").unwrap().parameters, vec![]);
        assert!(parse("text; charset=ascii").is_none());
        assert!(parse("").is_none());
        assert!(parse("charset=ascii").is_none());
        assert!(parse("text/").is_none());
    }
}
