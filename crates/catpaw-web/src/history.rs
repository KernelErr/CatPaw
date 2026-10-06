//! The `History` interface over the document's session history.
//!
//! Only same-document entries live here (`pushState`, `replaceState`,
//! fragment navigations). Loading another document replaces the page, so
//! going back past the first entry does nothing.

use catpaw_js::{EventTargetRef, Exception, Fallible, ObjectId, Value};
use url::Url;

use crate::event_loop::queue_task;
use crate::events::{self, EventData};
use crate::generated::{self as web, InterfaceId, ScrollRestoration};
use crate::page::Cx;
use crate::{Web, platform_object};

pub struct HistoryObject;
platform_object!(HistoryObject, History);

pub struct HistoryEntry {
    pub url: Url,
    /// The entry's state, already structured-cloned.
    pub state: Value,
}

pub struct HistoryState {
    pub entries: Vec<HistoryEntry>,
    pub index: usize,
    pub scroll_restoration: ScrollRestoration,
}

impl HistoryState {
    pub fn new(url: Url) -> Self {
        Self {
            entries: vec![HistoryEntry {
                url,
                state: Value::Null,
            }],
            index: 0,
            scroll_restoration: ScrollRestoration::Auto,
        }
    }
}

fn set_document_url(cx: &Cx<'_>, url: &Url) {
    *cx.page.url.borrow_mut() = url.clone();
    cx.dom_mut().document_data_mut().url = Some(url.clone());
}

/// Adds (or, with `replace`, overwrites) the current entry and makes `url`
/// the document URL.
pub(crate) fn commit_entry(cx: &Cx<'_>, url: Url, state: Value, replace: bool) {
    set_document_url(cx, &url);
    let mut history = cx.page.history.borrow_mut();
    let index = history.index;
    if replace {
        history.entries[index] = HistoryEntry { url, state };
    } else {
        history.entries.truncate(index + 1);
        history.entries.push(HistoryEntry { url, state });
        history.index = index + 1;
    }
}

fn without_fragment(url: &Url) -> Url {
    let mut url = url.clone();
    url.set_fragment(None);
    url
}

/// Fires `hashchange` at the window.
pub(crate) fn fire_hashchange(cx: &mut Cx<'_>, old_url: &Url, new_url: &Url) {
    let mut event = events::Event::new("hashchange", false, false, cx.page.clock.peek());
    event.iface = InterfaceId::HashChangeEvent;
    event.trusted = true;
    event.data = EventData::HashChange {
        old_url: old_url.to_string(),
        new_url: new_url.to_string(),
    };
    let event = cx.page.alloc(event);
    events::dispatch(cx, EventTargetRef::Window, event);
}

/// Moves `delta` entries through the session history.
fn traverse(cx: &mut Cx<'_>, delta: i32) {
    let (old_url, new_url, state) = {
        let mut history = cx.page.history.borrow_mut();
        let Some(target) = history
            .index
            .checked_add_signed(delta as isize)
            .filter(|&i| i < history.entries.len())
        else {
            return;
        };
        if target == history.index {
            return;
        }
        let old_url = history.entries[history.index].url.clone();
        history.index = target;
        let entry = &history.entries[target];
        (old_url, entry.url.clone(), entry.state.clone())
    };
    set_document_url(cx, &new_url);

    let mut event = events::Event::new("popstate", false, false, cx.page.clock.peek());
    event.iface = InterfaceId::PopStateEvent;
    event.trusted = true;
    event.data = EventData::PopState { state };
    let event = cx.page.alloc(event);
    events::dispatch(cx, EventTargetRef::Window, event);

    if old_url.fragment() != new_url.fragment()
        && without_fragment(&old_url) == without_fragment(&new_url)
    {
        fire_hashchange(cx, &old_url, &new_url);
    }
}

/// <https://html.spec.whatwg.org/multipage/#shared-history-push/replace-state-steps>
fn push_or_replace(
    cx: &mut Cx<'_>,
    data: Value,
    url: Option<String>,
    replace: bool,
) -> Fallible<()> {
    let state = cx.script.structured_clone(&data)?;
    let current = cx.page.url.borrow().clone();
    let new_url = match url {
        None => current.clone(),
        Some(input) => {
            let parsed = cx.page.resolve_url(&input).ok_or_else(|| {
                Exception::security(format!("'{input}' cannot be parsed as a URL"))
            })?;
            // Only the path, query and fragment may change.
            if parsed.origin() != current.origin()
                || parsed.username() != current.username()
                || parsed.password() != current.password()
            {
                return Err(Exception::security(format!(
                    "A history state object with URL '{parsed}' cannot be created in a document with origin '{}'",
                    current.origin().ascii_serialization()
                )));
            }
            parsed
        }
    };
    commit_entry(cx, new_url, state, replace);
    Ok(())
}

impl web::HistoryImpl for Web {
    fn length(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<u32> {
        Ok(cx.page.history.borrow().entries.len() as u32)
    }

    fn scroll_restoration(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<ScrollRestoration> {
        Ok(cx.page.history.borrow().scroll_restoration)
    }

    fn set_scroll_restoration(
        cx: &mut Cx<'_>,
        _this: ObjectId,
        value: ScrollRestoration,
    ) -> Fallible<()> {
        cx.page.history.borrow_mut().scroll_restoration = value;
        Ok(())
    }

    fn state(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<Value> {
        let history = cx.page.history.borrow();
        Ok(history.entries[history.index].state.clone())
    }

    fn go(cx: &mut Cx<'_>, _this: ObjectId, delta: i32) -> Fallible<()> {
        if delta != 0 {
            queue_task(cx.page, "history traversal", move |cx| traverse(cx, delta));
        }
        Ok(())
    }

    fn back(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<()> {
        queue_task(cx.page, "history traversal", |cx| traverse(cx, -1));
        Ok(())
    }

    fn forward(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<()> {
        queue_task(cx.page, "history traversal", |cx| traverse(cx, 1));
        Ok(())
    }

    fn push_state(
        cx: &mut Cx<'_>,
        _this: ObjectId,
        data: Value,
        _unused: String,
        url: Option<String>,
    ) -> Fallible<()> {
        push_or_replace(cx, data, url, false)
    }

    fn replace_state(
        cx: &mut Cx<'_>,
        _this: ObjectId,
        data: Value,
        _unused: String,
        url: Option<String>,
    ) -> Fallible<()> {
        push_or_replace(cx, data, url, true)
    }
}
