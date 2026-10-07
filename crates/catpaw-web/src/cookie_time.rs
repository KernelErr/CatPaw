//! Cookie expiry in page time.
//!
//! The cookie jar keeps real time; a page may run on a clock of its own (a
//! fixed time origin, for repeatable runs). A cookie script writes with an
//! `expires` date is therefore handed to the jar with a `max-age` measured
//! on the page's clock: a ten-minute session cookie lasts ten minutes,
//! whatever the page's date.

/// Unix milliseconds of a cookie date (RFC 6265, section 5.1.1), or
/// `None` when it does not parse.
pub fn parse_cookie_date(text: &str) -> Option<f64> {
    let mut time: Option<(u32, u32, u32)> = None;
    let mut day: Option<u32> = None;
    let mut month: Option<u32> = None;
    let mut year: Option<i64> = None;
    let delimiter = |c: char| {
        c == '\t'
            || (' '..='/').contains(&c)
            || (';'..='@').contains(&c)
            || ('['..='`').contains(&c)
            || ('{'..='~').contains(&c)
    };
    for token in text.split(delimiter).filter(|t| !t.is_empty()) {
        if time.is_none() {
            let parts: Vec<&str> = token.split(':').collect();
            if parts.len() == 3
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.len() <= 2 && p.bytes().all(|b| b.is_ascii_digit()))
            {
                time = Some((
                    parts[0].parse().ok()?,
                    parts[1].parse().ok()?,
                    parts[2].parse().ok()?,
                ));
                continue;
            }
        }
        let digits: String = token.chars().take_while(char::is_ascii_digit).collect();
        if day.is_none() && (1..=2).contains(&digits.len()) {
            day = digits.parse().ok();
            continue;
        }
        if month.is_none() && token.len() >= 3 {
            let name = token[..3].to_ascii_lowercase();
            let months = [
                "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
            ];
            if let Some(i) = months.iter().position(|m| *m == name) {
                month = Some(i as u32 + 1);
                continue;
            }
        }
        if year.is_none() && (2..=4).contains(&digits.len()) {
            let mut y: i64 = digits.parse().ok()?;
            if (70..=99).contains(&y) {
                y += 1900;
            } else if (0..=69).contains(&y) && digits.len() <= 2 {
                y += 2000;
            }
            year = Some(y);
        }
    }
    let ((h, m, s), day, month, year) = (time?, day?, month?, year?);
    if !(1..=31).contains(&day) || year < 1601 || h > 23 || m > 59 || s > 59 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let (y, mo) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * i64::from(mo) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + i64::from(h) * 3600 + i64::from(m) * 60 + i64::from(s)) * 1000) as f64)
}

/// A cookie string with its `expires` date turned into a `max-age`
/// counted from `now_ms` (the page's clock), unless it has a `max-age`
/// already or the date does not parse.
pub fn relative_expiry(cookie: &str, now_ms: f64) -> String {
    let parts: Vec<&str> = cookie.split(';').collect();
    let attribute = |name: &str| {
        parts.iter().skip(1).position(|p| {
            p.trim()
                .split('=')
                .next()
                .is_some_and(|n| n.trim().eq_ignore_ascii_case(name))
        })
    };
    if attribute("max-age").is_some() {
        return cookie.to_string();
    }
    let Some(i) = attribute("expires") else {
        return cookie.to_string();
    };
    let value = parts[i + 1].split_once('=').map(|(_, v)| v).unwrap_or("");
    let Some(expires) = parse_cookie_date(value) else {
        return cookie.to_string();
    };
    let seconds = ((expires - now_ms) / 1000.0).floor() as i64;
    let mut out: Vec<String> = parts.iter().map(|p| p.to_string()).collect();
    out[i + 1] = format!(" max-age={}", seconds.max(0));
    out.join(";")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_dates_parse() {
        assert_eq!(
            parse_cookie_date("Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(0.0)
        );
        assert_eq!(
            parse_cookie_date("Wed, 21-Oct-2015 07:28:00 GMT"),
            Some(1_445_412_480_000.0)
        );
        assert_eq!(parse_cookie_date("not a date"), None);
    }

    #[test]
    fn expiry_is_counted_on_the_page_clock() {
        let now = parse_cookie_date("Mon, 21 Sep 2026 12:00:00 GMT").unwrap();
        assert_eq!(
            relative_expiry(
                "session=bob; expires=Mon, 21 Sep 2026 12:10:00 GMT; path=/",
                now
            ),
            "session=bob; max-age=600; path=/"
        );
        assert_eq!(
            relative_expiry("a=1; Max-Age=5; expires=x", now),
            "a=1; Max-Age=5; expires=x"
        );
        assert_eq!(
            relative_expiry("gone=1; expires=Thu, 01 Jan 1970 00:00:00 GMT", now),
            "gone=1; max-age=0"
        );
    }
}
