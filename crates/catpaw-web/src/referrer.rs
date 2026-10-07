//! Which referrer a request carries
//! (<https://w3c.github.io/webappsec-referrer-policy/#determine-requests-referrer>).
//! The network host sends what is decided here as it is.

use url::Url;

use crate::generated::ReferrerPolicy;
use crate::page::PageState;

/// The referrer for a request to `target` from `source` under `policy`;
/// `None` means no `Referer` header at all.
pub(crate) fn determine(policy: ReferrerPolicy, source: &Url, target: &Url) -> Option<Url> {
    if !matches!(source.scheme(), "http" | "https") {
        return None;
    }
    let mut full = source.clone();
    full.set_fragment(None);
    let _ = full.set_username("");
    let _ = full.set_password(None);
    let origin_only = Url::parse(&format!("{}/", source.origin().ascii_serialization())).ok()?;
    // A very long referrer is cut down to its origin.
    let full = if full.as_str().len() > 4096 {
        origin_only.clone()
    } else {
        full
    };
    let same_origin = source.origin() == target.origin();
    let downgrade = source.scheme() == "https" && target.scheme() != "https";
    match policy {
        ReferrerPolicy::NoReferrer => None,
        ReferrerPolicy::UnsafeUrl => Some(full),
        ReferrerPolicy::Origin => Some(origin_only),
        ReferrerPolicy::SameOrigin => same_origin.then_some(full),
        ReferrerPolicy::OriginWhenCrossOrigin => Some(if same_origin { full } else { origin_only }),
        ReferrerPolicy::StrictOrigin => (!downgrade).then_some(origin_only),
        ReferrerPolicy::NoReferrerWhenDowngrade => (!downgrade).then_some(full),
        ReferrerPolicy::Empty | ReferrerPolicy::StrictOriginWhenCrossOrigin => {
            if same_origin {
                Some(full)
            } else if downgrade {
                None
            } else {
                Some(origin_only)
            }
        }
    }
}

/// The policy a token names, if it names one.
pub(crate) fn from_token(token: &str) -> Option<ReferrerPolicy> {
    Some(match token {
        "no-referrer" => ReferrerPolicy::NoReferrer,
        "no-referrer-when-downgrade" => ReferrerPolicy::NoReferrerWhenDowngrade,
        "same-origin" => ReferrerPolicy::SameOrigin,
        "origin" => ReferrerPolicy::Origin,
        "strict-origin" => ReferrerPolicy::StrictOrigin,
        "origin-when-cross-origin" => ReferrerPolicy::OriginWhenCrossOrigin,
        "strict-origin-when-cross-origin" => ReferrerPolicy::StrictOriginWhenCrossOrigin,
        "unsafe-url" => ReferrerPolicy::UnsafeUrl,
        _ => return None,
    })
}

/// The policy a `Referrer-Policy` header sets: the last token that names
/// one (<https://w3c.github.io/webappsec-referrer-policy/#parse-referrer-policy-from-header>).
pub(crate) fn from_header(value: &str) -> Option<ReferrerPolicy> {
    value
        .split(',')
        .filter_map(|token| from_token(token.trim()))
        .next_back()
}

/// The referrer of a request the document makes on its own behalf (a
/// script, a style sheet), under the default policy.
pub(crate) fn for_document(page: &PageState, target: &Url) -> Option<Url> {
    determine(ReferrerPolicy::Empty, &page.url.borrow(), target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn policies_decide_the_referrer() {
        let page = url("https://a.test/dir/page.html?q=1#frag");
        let same = url("https://a.test/other");
        let other = url("https://b.test/x");
        let http = url("http://b.test/x");
        let full = "https://a.test/dir/page.html?q=1";
        let as_str = |r: Option<Url>| r.map(|u| u.to_string());
        assert_eq!(
            as_str(determine(ReferrerPolicy::Empty, &page, &same)).as_deref(),
            Some(full)
        );
        assert_eq!(
            as_str(determine(ReferrerPolicy::Empty, &page, &other)).as_deref(),
            Some("https://a.test/")
        );
        assert_eq!(determine(ReferrerPolicy::Empty, &page, &http), None);
        assert_eq!(determine(ReferrerPolicy::NoReferrer, &page, &same), None);
        assert_eq!(
            as_str(determine(ReferrerPolicy::UnsafeUrl, &page, &http)).as_deref(),
            Some(full)
        );
        assert_eq!(
            as_str(determine(ReferrerPolicy::Origin, &page, &same)).as_deref(),
            Some("https://a.test/")
        );
        assert_eq!(determine(ReferrerPolicy::SameOrigin, &page, &other), None);
        assert_eq!(
            as_str(determine(
                ReferrerPolicy::NoReferrerWhenDowngrade,
                &page,
                &other
            ))
            .as_deref(),
            Some(full)
        );
        assert_eq!(
            from_header("bogus, no-referrer, unsafe-url,nonsense"),
            Some(ReferrerPolicy::UnsafeUrl)
        );
        assert_eq!(from_header("nonsense"), None);
    }
}
