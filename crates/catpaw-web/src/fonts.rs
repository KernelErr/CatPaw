//! The CSS Font Loading API: `document.fonts` and `FontFace`.
//!
//! No font is fetched or shaped here, since text is never laid out, so a
//! face counts as loaded the moment a script asks for it. What the API
//! keeps is the set itself: scripts waiting on `document.fonts.ready` or
//! `load()` carry on, and the faces they add can be read back.

use std::cell::RefCell;
use std::collections::HashMap;

use catpaw_dom::NodeId;
use catpaw_js::{Fallible, ObjectId, PromiseRef, Value};

use crate::generated::{
    self as web, FontFaceDescriptors, FontFaceLoadStatus, FontFaceSetLoadStatus,
    StringOrBufferSource,
};
use crate::page::{Cx, PageState};
use crate::{Web, platform_object};

pub struct FontFaceObject {
    family: String,
    descriptors: FontFaceDescriptors,
    status: FontFaceLoadStatus,
    /// `[[FontStatusPromise]]`, made when first needed.
    loaded: Option<PromiseRef>,
}
platform_object!(FontFaceObject, FontFace);

#[derive(Default)]
pub struct FontFaceSetObject {
    /// The faces added by script, pinned while they are in the set.
    faces: Vec<ObjectId>,
    ready: Option<PromiseRef>,
}
platform_object!(FontFaceSetObject, FontFaceSet);

/// Each document's `fonts`, pinned for the page's lifetime.
#[derive(Default)]
pub struct FontSets {
    by_document: RefCell<HashMap<NodeId, ObjectId>>,
}

fn face<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&mut FontFaceObject) -> R) -> Fallible<R> {
    cx.page.with::<FontFaceObject, _>(this, f)
}

fn set<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&mut FontFaceSetObject) -> R) -> Fallible<R> {
    cx.page.with::<FontFaceSetObject, _>(this, f)
}

/// Marks a face loaded and settles its promise with the face itself.
fn finish_loading(cx: &mut Cx<'_>, face_id: ObjectId) -> Fallible<PromiseRef> {
    let (status, loaded) = face(cx, face_id, |f| (f.status, f.loaded.clone()))?;
    let promise = match loaded {
        Some(p) => p,
        None => {
            let p = cx.script.new_promise();
            face(cx, face_id, |f| f.loaded = Some(p.clone()))?;
            p
        }
    };
    if status != FontFaceLoadStatus::Loaded {
        face(cx, face_id, |f| f.status = FontFaceLoadStatus::Loaded)?;
        cx.script.resolve_promise(&promise, Value::Object(face_id));
    }
    Ok(promise)
}

/// Whether `font` (a CSS font shorthand) names the face's family.
fn names_family(font: &str, family: &str) -> bool {
    let family = family.trim().trim_matches(|c| c == '"' || c == '\'');
    !family.is_empty()
        && font
            .to_ascii_lowercase()
            .contains(&family.to_ascii_lowercase())
}

macro_rules! descriptor_accessors {
    ($($get:ident, $set:ident => $field:ident;)*) => {
        $(
            fn $get(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
                face(cx, this, |f| f.descriptors.$field.clone())
            }

            fn $set(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
                face(cx, this, |f| f.descriptors.$field = value)
            }
        )*
    };
}

impl web::FontFaceImpl for Web {
    fn family(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        face(cx, this, |f| f.family.clone())
    }

    fn set_family(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        face(cx, this, |f| f.family = value)
    }

    descriptor_accessors! {
        style, set_style => style;
        weight, set_weight => weight;
        stretch, set_stretch => stretch;
        unicode_range, set_unicode_range => unicode_range;
        feature_settings, set_feature_settings => feature_settings;
        variation_settings, set_variation_settings => variation_settings;
        display, set_display => display;
        ascent_override, set_ascent_override => ascent_override;
        descent_override, set_descent_override => descent_override;
        line_gap_override, set_line_gap_override => line_gap_override;
    }

    fn status(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<FontFaceLoadStatus> {
        face(cx, this, |f| f.status)
    }

    fn load(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        finish_loading(cx, this)
    }

    fn loaded(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        let (status, loaded) = face(cx, this, |f| (f.status, f.loaded.clone()))?;
        if let Some(p) = loaded {
            return Ok(p);
        }
        if status == FontFaceLoadStatus::Loaded {
            return finish_loading(cx, this);
        }
        let p = cx.script.new_promise();
        face(cx, this, |f| f.loaded = Some(p.clone()))?;
        Ok(p)
    }

    fn constructor(
        cx: &mut Cx<'_>,
        family: String,
        source: StringOrBufferSource,
        descriptors: FontFaceDescriptors,
    ) -> Fallible<ObjectId> {
        let id = cx.page.alloc(FontFaceObject {
            family,
            descriptors,
            status: FontFaceLoadStatus::Unloaded,
            loaded: None,
        });
        // Binary data needs no fetch: the face loads as soon as it exists,
        // once the constructor has handed it over.
        if matches!(source, StringOrBufferSource::BufferSource(_)) {
            face(cx, id, |f| f.status = FontFaceLoadStatus::Loading)?;
            cx.page.queue_microtask(move |cx| {
                let _ = finish_loading(cx, id);
            });
        }
        Ok(id)
    }
}

impl web::FontFaceSetImpl for Web {
    fn add(cx: &mut Cx<'_>, this: ObjectId, font: ObjectId) -> Fallible<ObjectId> {
        let added = set(cx, this, |s| {
            if s.faces.contains(&font) {
                false
            } else {
                s.faces.push(font);
                true
            }
        })?;
        if added {
            cx.pin(font);
        }
        Ok(this)
    }

    fn delete(cx: &mut Cx<'_>, this: ObjectId, font: ObjectId) -> Fallible<bool> {
        let removed = set(cx, this, |s| {
            let before = s.faces.len();
            s.faces.retain(|&f| f != font);
            s.faces.len() != before
        })?;
        if removed {
            cx.unpin(font);
        }
        Ok(removed)
    }

    fn clear(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<()> {
        let faces = set(cx, this, std::mem::take)?.faces;
        for face in faces {
            cx.unpin(face);
        }
        Ok(())
    }

    fn load(cx: &mut Cx<'_>, this: ObjectId, font: String, _text: String) -> Fallible<PromiseRef> {
        let faces = set(cx, this, |s| s.faces.clone())?;
        let mut matching = Vec::new();
        for id in faces {
            if face(cx, id, |f| names_family(&font, &f.family))? {
                finish_loading(cx, id)?;
                matching.push(Value::Object(id));
            }
        }
        let promise = cx.script.new_promise();
        cx.script.resolve_promise(&promise, Value::Array(matching));
        Ok(promise)
    }

    fn check(_cx: &mut Cx<'_>, _this: ObjectId, _font: String, _text: String) -> Fallible<bool> {
        Ok(true)
    }

    fn ready(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<PromiseRef> {
        if let Some(p) = set(cx, this, |s| s.ready.clone())? {
            return Ok(p);
        }
        let p = cx.script.new_promise();
        cx.script.resolve_promise(&p, Value::Object(this));
        set(cx, this, |s| s.ready = Some(p.clone()))?;
        Ok(p)
    }

    fn status(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<FontFaceSetLoadStatus> {
        Ok(FontFaceSetLoadStatus::Loaded)
    }

    fn set_values(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<ObjectId>> {
        set(cx, this, |s| s.faces.clone())
    }
}

pub(crate) fn fonts_of(cx: &mut Cx<'_>, document: NodeId) -> ObjectId {
    let page: &PageState = cx.page;
    let known = page.fonts.by_document.borrow().get(&document).copied();
    if let Some(id) = known {
        return id;
    }
    let id = page.alloc(FontFaceSetObject::default());
    cx.pin(id);
    cx.page.fonts.by_document.borrow_mut().insert(document, id);
    id
}

impl web::FontFaceSourceImpl for Web {
    fn fonts(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        Ok(fonts_of(cx, this))
    }
}
