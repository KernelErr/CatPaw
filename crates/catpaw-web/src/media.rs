//! `window.matchMedia` and `window.screen`.
//!
//! The page is headless: its "device" is the configured viewport, and the
//! screen is exactly as large as that viewport.

use catpaw_js::{Callback, EventTargetRef, Fallible, ObjectId};
use catpaw_style::{MediaQueryList, StyleOptions};

use crate::generated as web;
use crate::page::{Cx, PageState};
use crate::{Web, events, platform_object};

pub struct MediaQueryListObject {
    query: MediaQueryList,
}
platform_object!(MediaQueryListObject, MediaQueryList);

pub struct ScreenObject;
platform_object!(ScreenObject, Screen);

/// The device media queries are evaluated against.
pub(crate) fn device(page: &PageState) -> StyleOptions {
    StyleOptions {
        viewport_width: page.config.viewport_width as f32,
        viewport_height: page.config.viewport_height as f32,
        device_pixel_ratio: page.config.device_pixel_ratio as f32,
        base_url: page.url.borrow().clone(),
        dark_mode: false,
    }
}

/// `window.matchMedia(query)`.
pub(crate) fn match_media(page: &PageState, query: &str) -> ObjectId {
    page.alloc(MediaQueryListObject {
        query: MediaQueryList::parse(query),
    })
}

impl web::MediaQueryListImpl for Web {
    fn media(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        cx.page
            .with::<MediaQueryListObject, _>(this, |m| m.query.text())
    }

    fn matches(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        let device = device(cx.page);
        cx.page
            .with::<MediaQueryListObject, _>(this, |m| m.query.matches(&device))
    }

    // The legacy spelling of add/removeEventListener('change', ...).
    fn add_listener(cx: &mut Cx<'_>, this: ObjectId, callback: Option<Callback>) -> Fallible<()> {
        if let Some(callback) = callback {
            let target = EventTargetRef::Object(this);
            events::add_listener(cx, target, "change", callback, false, false, false);
        }
        Ok(())
    }

    fn remove_listener(
        cx: &mut Cx<'_>,
        this: ObjectId,
        callback: Option<Callback>,
    ) -> Fallible<()> {
        if let Some(callback) = callback {
            events::remove_listener(cx, EventTargetRef::Object(this), "change", &callback, false);
        }
        Ok(())
    }
}

impl web::ScreenImpl for Web {
    fn avail_width(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<i32> {
        Ok(cx.page.config.viewport_width as i32)
    }

    fn avail_height(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<i32> {
        Ok(cx.page.config.viewport_height as i32)
    }

    fn width(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<i32> {
        Ok(cx.page.config.viewport_width as i32)
    }

    fn height(cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<i32> {
        Ok(cx.page.config.viewport_height as i32)
    }

    fn color_depth(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<u32> {
        Ok(24)
    }

    fn pixel_depth(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<u32> {
        Ok(24)
    }
}
