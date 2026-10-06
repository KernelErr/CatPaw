//! The URL of a hyperlink element, piece by piece: `a.pathname` and the
//! like (<https://html.spec.whatwg.org/multipage/#api-for-a-and-area-elements>).
//!
//! The `href` attribute is the only state: each read resolves it against
//! the element's document, and each write puts the edited URL back.

use catpaw_dom::NodeId;
use catpaw_js::Fallible;
use url::Url;

use crate::generated as web;
use crate::page::Cx;
use crate::{Web, element, node};

/// The URL an element's `href` attribute resolves to, if it does.
fn link_url(cx: &Cx<'_>, element: NodeId) -> Option<Url> {
    let href = element::get_attr(cx, element, "href")?;
    node::base_url(cx, element)?.join(href.trim()).ok()
}

fn read(cx: &Cx<'_>, element: NodeId, part: impl FnOnce(&Url) -> &str) -> Fallible<String> {
    node::check(cx, element)?;
    Ok(link_url(cx, element).map_or_else(String::new, |url| part(&url).to_string()))
}

/// Edits the element's URL and writes it back. Without a URL there is
/// nothing to edit.
fn edit(cx: &mut Cx<'_>, element: NodeId, change: impl FnOnce(&mut Url)) -> Fallible<()> {
    node::check(cx, element)?;
    let Some(mut url) = link_url(cx, element) else {
        return Ok(());
    };
    change(&mut url);
    element::set_attr(cx, element, "href", url.to_string())
}

impl web::HyperlinkElementUtilsImpl for Web {
    fn origin(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(link_url(cx, this).map_or_else(String::new, |url| url::quirks::origin(&url)))
    }

    fn protocol(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(link_url(cx, this).map_or_else(
            || ":".to_string(),
            |url| url::quirks::protocol(&url).to_string(),
        ))
    }

    fn set_protocol(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        edit(cx, this, |url| {
            let _ = url::quirks::set_protocol(url, &value);
        })
    }

    fn username(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        read(cx, this, url::quirks::username)
    }

    fn set_username(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        edit(cx, this, |url| {
            let _ = url::quirks::set_username(url, &value);
        })
    }

    fn password(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        read(cx, this, url::quirks::password)
    }

    fn set_password(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        edit(cx, this, |url| {
            let _ = url::quirks::set_password(url, &value);
        })
    }

    fn host(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        read(cx, this, url::quirks::host)
    }

    fn set_host(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        edit(cx, this, |url| {
            let _ = url::quirks::set_host(url, &value);
        })
    }

    fn hostname(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        read(cx, this, url::quirks::hostname)
    }

    fn set_hostname(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        edit(cx, this, |url| {
            let _ = url::quirks::set_hostname(url, &value);
        })
    }

    fn port(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        read(cx, this, url::quirks::port)
    }

    fn set_port(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        edit(cx, this, |url| {
            let _ = url::quirks::set_port(url, &value);
        })
    }

    fn pathname(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        read(cx, this, url::quirks::pathname)
    }

    fn set_pathname(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        edit(cx, this, |url| url::quirks::set_pathname(url, &value))
    }

    fn search(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        read(cx, this, url::quirks::search)
    }

    fn set_search(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        edit(cx, this, |url| url::quirks::set_search(url, &value))
    }

    fn hash(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        read(cx, this, url::quirks::hash)
    }

    fn set_hash(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        edit(cx, this, |url| url::quirks::set_hash(url, &value))
    }
}
