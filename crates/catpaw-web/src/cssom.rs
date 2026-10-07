//! The CSSOM: `CSSStyleSheet`, its rules, `document.styleSheets`,
//! `adoptedStyleSheets` and the `sheet` of `<style>` and `<link>`.
//!
//! A sheet is a list of top-level rules, parsed and serialised by
//! `catpaw_style::cssom`, so `cssRules[i].cssText` is canonical and an
//! edited sheet reaches the style engine as the join of its rules. The
//! sheet of an element follows the element's text: when that changes, the
//! rules are parsed afresh and script's edits are gone, as in browsers,
//! where a new sheet replaces the old one.
//!
//! Constructed sheets adopted by the document apply after the document's
//! own. Sheets adopted by a shadow root are kept and reported, but without
//! style scoping they do not apply to anything.
//!
//! Not there: `CSSStyleRule.style`, the rules nested in grouping rules,
//! `ownerRule`, and `@import` (dropped at parse time, since the engine has
//! no loader for it yet).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use catpaw_dom::{Dom, NodeId, NodeKind};
use catpaw_js::{Exception, Fallible, ObjectId, PromiseRef, Value};
use catpaw_style::cssom::{self, Rule, STYLE_RULE};
use url::Url;

use crate::generated::{self as web, CSSStyleSheetInit, InterfaceId, MediaListOrString};
use crate::page::{Cx, PageState};
use crate::{Web, platform_object, stylesheets};

struct RuleEntry {
    id: u64,
    rule: Rule,
    object: Option<ObjectId>,
}

pub struct SheetObject {
    /// The `<style>` or `<link>` the sheet belongs to; none when constructed.
    owner: Option<NodeId>,
    base_url: Option<Url>,
    rules: Vec<RuleEntry>,
    next_rule_id: u64,
    media: Vec<String>,
    disabled: bool,
    href: Option<String>,
    title: Option<String>,
    /// For an element's sheet: a hash of the text the rules came from.
    source_hash: u64,
    media_object: Option<ObjectId>,
}
platform_object!(SheetObject, CSSStyleSheet);

impl SheetObject {
    fn set_rules(&mut self, rules: Vec<Rule>) {
        self.rules = rules
            .into_iter()
            .map(|rule| {
                self.next_rule_id += 1;
                RuleEntry {
                    id: self.next_rule_id,
                    rule,
                    object: None,
                }
            })
            .collect();
    }

    /// The sheet's text for the style engine.
    fn text(&self) -> String {
        let mut out = String::new();
        for entry in &self.rules {
            out.push_str(&entry.rule.css_text);
            out.push('\n');
        }
        out
    }

    fn constructed(&self) -> bool {
        self.owner.is_none()
    }
}

pub struct RuleObject {
    sheet: ObjectId,
    rule_id: u64,
    kind: u16,
}
platform_object!(RuleObject, |r| if r.kind == STYLE_RULE {
    InterfaceId::CSSStyleRule
} else {
    InterfaceId::CSSRule
});

pub struct RuleListObject {
    sheet: ObjectId,
}
platform_object!(RuleListObject, CSSRuleList);

pub struct SheetListObject {
    root: NodeId,
}
platform_object!(SheetListObject, StyleSheetList);

pub struct MediaListObject {
    sheet: ObjectId,
}
platform_object!(MediaListObject, MediaList);

fn hash_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Runs `f` on a sheet, first bringing an element's sheet in line with the
/// element's current text.
fn sheet<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&mut SheetObject) -> R) -> Fallible<R> {
    let owner = cx
        .page
        .try_with::<SheetObject, _>(this, |s| s.owner)
        .flatten();
    if let Some(owner) = owner {
        let text = {
            let dom = cx.dom();
            element_sheet_text(cx.page, &dom, owner)
        };
        if let Some(text) = text {
            sync_element_sheet(cx.page, this, &text);
        }
    }
    cx.page.with::<SheetObject, _>(this, f)
}

fn parse_media(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(|m| m.to_ascii_lowercase())
        .collect()
}

/// Whether `element` owns a sheet: a CSS `<style>`, or a `<link>` whose
/// sheet has loaded; and that sheet's text.
fn element_sheet_text(page: &PageState, dom: &Dom, element: NodeId) -> Option<String> {
    let el = dom.element(element)?;
    if !el.is_html() {
        return None;
    }
    match &*el.name.local {
        "style" => {
            let is_css = el.attr("type").is_none_or(|t| {
                let t = t.trim();
                t.is_empty() || t.eq_ignore_ascii_case("text/css")
            });
            is_css.then(|| crate::element::child_text_content(dom, element))
        }
        "link" => stylesheets::loaded_link_text(page, element).map(|t| t.to_string()),
        _ => None,
    }
}

/// The sheet object of an element, made on first use. The object stays
/// pinned for the page's lifetime: an element owns its sheet.
pub(crate) fn sheet_of_element(cx: &mut Cx<'_>, element: NodeId) -> Option<ObjectId> {
    let known = cx.page.styles.sheet_objects.borrow().get(&element).copied();
    let (text, href, title, media, is_connected) = {
        let dom = cx.dom();
        let text = element_sheet_text(cx.page, &dom, element)?;
        let el = dom.element(element)?;
        let href = (&*el.name.local == "link")
            .then(|| stylesheets::link_url(cx.page, element).map(|u| u.to_string()))
            .flatten();
        (
            text,
            href,
            el.attr("title").map(str::to_string),
            el.attr("media").map(parse_media).unwrap_or_default(),
            dom.is_connected(element),
        )
    };
    if !is_connected {
        return None;
    }
    if let Some(id) = known {
        sync_element_sheet(cx.page, id, &text);
        return Some(id);
    }
    let base_url = cx.page.url.borrow().clone();
    let mut object = SheetObject {
        owner: Some(element),
        base_url: Some(base_url.clone()),
        rules: Vec::new(),
        next_rule_id: 0,
        media,
        disabled: false,
        href,
        title,
        source_hash: hash_text(&text),
        media_object: None,
    };
    object.set_rules(cssom::parse_rules(&text, Some(&base_url), true));
    let id = cx.page.alloc(object);
    cx.pin(id);
    cx.page
        .styles
        .sheet_objects
        .borrow_mut()
        .insert(element, id);
    Some(id)
}

/// Re-parses an element's sheet when the element's text has changed.
pub(crate) fn sync_element_sheet(page: &PageState, id: ObjectId, text: &str) {
    let hash = hash_text(text);
    let _ = page.with::<SheetObject, _>(id, |s| {
        if s.source_hash != hash {
            s.source_hash = hash;
            let rules = cssom::parse_rules(text, s.base_url.as_ref(), true);
            s.set_rules(rules);
        }
    });
}

/// What the style engine should see of an element's sheet, once script
/// has a sheet object for it: `None` when the sheet is disabled.
pub(crate) fn engine_view(page: &PageState, id: ObjectId) -> Option<(Vec<String>, String)> {
    page.with::<SheetObject, _>(id, |s| (!s.disabled).then(|| (s.media.clone(), s.text())))
        .ok()
        .flatten()
}

/// The elements whose sheets `styleSheets` lists, in tree order.
fn sheet_owners(page: &PageState, dom: &Dom, root: NodeId) -> Vec<NodeId> {
    dom.descendants(root)
        .filter(|&n| element_sheet_text(page, dom, n).is_some())
        .collect()
}

fn rule_object(cx: &mut Cx<'_>, sheet_id: ObjectId, index: usize) -> Fallible<Option<ObjectId>> {
    let Some((rule_id, kind, known)) = sheet(cx, sheet_id, |s| {
        s.rules.get(index).map(|e| (e.id, e.rule.kind, e.object))
    })?
    else {
        return Ok(None);
    };
    if let Some(id) = known
        && cx
            .page
            .try_with::<RuleObject, _>(id, |r| r.rule_id == rule_id)
            == Some(true)
    {
        return Ok(Some(id));
    }
    let id = cx.page.alloc(RuleObject {
        sheet: sheet_id,
        rule_id,
        kind,
    });
    sheet(cx, sheet_id, |s| {
        if let Some(entry) = s.rules.get_mut(index) {
            entry.object = Some(id);
        }
    })?;
    Ok(Some(id))
}

fn with_rule<R>(
    cx: &Cx<'_>,
    this: ObjectId,
    f: impl FnOnce(&mut RuleEntry) -> R,
) -> Fallible<Option<R>> {
    let (sheet_id, rule_id) = cx
        .page
        .with::<RuleObject, _>(this, |r| (r.sheet, r.rule_id))?;
    sheet(cx, sheet_id, |s| {
        s.rules.iter_mut().find(|e| e.id == rule_id).map(f)
    })
}

fn changed(page: &PageState) {
    page.styles.edits.set(page.styles.edits.get() + 1);
}

fn require_constructed(cx: &Cx<'_>, this: ObjectId) -> Fallible<()> {
    if sheet(cx, this, |s| s.constructed())? {
        Ok(())
    } else {
        Err(Exception::not_allowed(
            "Can't call this method on non-constructed CSSStyleSheets.",
        ))
    }
}

// ---------------------------------------------------------------- bindings

impl web::StyleSheetImpl for Web {
    fn type_(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<String> {
        Ok("text/css".to_string())
    }

    fn href(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<String>> {
        sheet(cx, this, |s| s.href.clone())
    }

    fn owner_node(
        cx: &mut Cx<'_>,
        this: ObjectId,
    ) -> Fallible<Option<web::ElementOrProcessingInstruction>> {
        Ok(sheet(cx, this, |s| s.owner)?.map(web::ElementOrProcessingInstruction::Element))
    }

    fn parent_style_sheet(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<Option<ObjectId>> {
        Ok(None)
    }

    fn title(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<String>> {
        sheet(cx, this, |s| s.title.clone())
    }

    fn media(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        if let Some(id) = sheet(cx, this, |s| s.media_object)? {
            return Ok(id);
        }
        let id = cx.page.alloc(MediaListObject { sheet: this });
        cx.pin(id);
        sheet(cx, this, |s| s.media_object = Some(id))?;
        Ok(id)
    }

    fn disabled(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        sheet(cx, this, |s| s.disabled)
    }

    fn set_disabled(cx: &mut Cx<'_>, this: ObjectId, value: bool) -> Fallible<()> {
        sheet(cx, this, |s| s.disabled = value)?;
        changed(cx.page);
        Ok(())
    }
}

impl web::CSSStyleSheetImpl for Web {
    fn constructor(cx: &mut Cx<'_>, options: CSSStyleSheetInit) -> Fallible<ObjectId> {
        let document_url = cx.page.url.borrow().clone();
        let base_url = match options.base_url {
            Some(text) => Some(document_url.join(&text).map_err(|_| {
                Exception::not_allowed(format!("The base URL `{text}` is not valid"))
            })?),
            None => Some(document_url),
        };
        let media = match options.media {
            MediaListOrString::String(text) => parse_media(&text),
            MediaListOrString::MediaList(list) => {
                let owner = cx.page.with::<MediaListObject, _>(list, |m| m.sheet)?;
                sheet(cx, owner, |s| s.media.clone())?
            }
        };
        Ok(cx.page.alloc(SheetObject {
            owner: None,
            base_url,
            rules: Vec::new(),
            next_rule_id: 0,
            media,
            disabled: options.disabled,
            href: None,
            title: None,
            source_hash: 0,
            media_object: None,
        }))
    }

    fn owner_rule(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<Option<ObjectId>> {
        Ok(None)
    }

    fn css_rules(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        sheet(cx, this, |_| ())?;
        Ok(cx.page.alloc(RuleListObject { sheet: this }))
    }

    fn rules(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        Self::css_rules(cx, this)
    }

    /// <https://drafts.csswg.org/cssom/#dom-cssstylesheet-insertrule>
    fn insert_rule(cx: &mut Cx<'_>, this: ObjectId, rule: String, index: u32) -> Fallible<u32> {
        let (len, base) = sheet(cx, this, |s| (s.rules.len(), s.base_url.clone()))?;
        let index = index as usize;
        if index > len {
            return Err(Exception::index_size(
                "The index provided is larger than the maximum index.",
            ));
        }
        let parsed = cssom::parse_rule(&rule, base.as_ref())
            .map_err(|e| Exception::syntax(format!("Failed to parse the rule '{rule}': {e}")))?;
        if parsed.kind == cssom::IMPORT_RULE && sheet(cx, this, |s| s.constructed())? {
            return Err(Exception::syntax(
                "@import rules are not allowed in constructed style sheets.",
            ));
        }
        sheet(cx, this, |s| {
            s.next_rule_id += 1;
            s.rules.insert(
                index,
                RuleEntry {
                    id: s.next_rule_id,
                    rule: parsed,
                    object: None,
                },
            );
        })?;
        changed(cx.page);
        Ok(index as u32)
    }

    fn delete_rule(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<()> {
        let removed = sheet(cx, this, |s| {
            let index = index as usize;
            if index < s.rules.len() {
                s.rules.remove(index);
                true
            } else {
                false
            }
        })?;
        if !removed {
            return Err(Exception::index_size(
                "The index provided is larger than the maximum index.",
            ));
        }
        changed(cx.page);
        Ok(())
    }

    fn replace(cx: &mut Cx<'_>, this: ObjectId, text: String) -> Fallible<PromiseRef> {
        let promise = cx.script.new_promise();
        match Self::replace_sync(cx, this, text) {
            Ok(()) => cx.script.resolve_promise(&promise, Value::Object(this)),
            Err(e) => cx.script.reject_promise(&promise, e),
        }
        Ok(promise)
    }

    fn replace_sync(cx: &mut Cx<'_>, this: ObjectId, text: String) -> Fallible<()> {
        require_constructed(cx, this)?;
        let base = sheet(cx, this, |s| s.base_url.clone())?;
        let rules = cssom::parse_rules(&text, base.as_ref(), false);
        sheet(cx, this, |s| s.set_rules(rules))?;
        changed(cx.page);
        Ok(())
    }

    fn add_rule(
        cx: &mut Cx<'_>,
        this: ObjectId,
        selector: String,
        style: String,
        index: Option<u32>,
    ) -> Fallible<i32> {
        let index = match index {
            Some(i) => i,
            None => sheet(cx, this, |s| s.rules.len() as u32)?,
        };
        Self::insert_rule(cx, this, format!("{selector} {{ {style} }}"), index)?;
        Ok(-1)
    }

    fn remove_rule(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<()> {
        Self::delete_rule(cx, this, index)
    }
}

impl web::CSSRuleListImpl for Web {
    fn item(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<ObjectId>> {
        let sheet_id = cx.page.with::<RuleListObject, _>(this, |l| l.sheet)?;
        rule_object(cx, sheet_id, index as usize)
    }

    fn length(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        let sheet_id = cx.page.with::<RuleListObject, _>(this, |l| l.sheet)?;
        sheet(cx, sheet_id, |s| s.rules.len() as u32)
    }

    fn indexed_get(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<ObjectId>> {
        Self::item(cx, this, index)
    }
}

impl web::CSSRuleImpl for Web {
    fn css_text(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        Ok(with_rule(cx, this, |e| e.rule.css_text.clone())?.unwrap_or_default())
    }

    /// Setting `cssText` does nothing, as the specification says.
    fn set_css_text(_cx: &mut Cx<'_>, _this: ObjectId, _value: String) -> Fallible<()> {
        Ok(())
    }

    fn parent_rule(_cx: &mut Cx<'_>, _this: ObjectId) -> Fallible<Option<ObjectId>> {
        Ok(None)
    }

    fn parent_style_sheet(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<ObjectId>> {
        let sheet_id = cx.page.with::<RuleObject, _>(this, |r| r.sheet)?;
        Ok(with_rule(cx, this, |_| ())?.map(|_| sheet_id))
    }

    fn type_(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u16> {
        cx.page.with::<RuleObject, _>(this, |r| r.kind)
    }
}

impl web::CSSStyleRuleImpl for Web {
    fn selector_text(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        Ok(with_rule(cx, this, |e| e.rule.selector_text.clone())?
            .flatten()
            .unwrap_or_default())
    }

    /// A selector that does not parse leaves the rule as it was.
    fn set_selector_text(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        let Some(css_text) = with_rule(cx, this, |e| e.rule.css_text.clone())? else {
            return Ok(());
        };
        let Some(body) = css_text.find('{').map(|i| &css_text[i..]) else {
            return Ok(());
        };
        let sheet_id = cx.page.with::<RuleObject, _>(this, |r| r.sheet)?;
        let base = sheet(cx, sheet_id, |s| s.base_url.clone())?;
        if let Ok(parsed) = cssom::parse_rule(&format!("{value} {body}"), base.as_ref())
            && parsed.kind == STYLE_RULE
        {
            with_rule(cx, this, |e| e.rule = parsed)?;
            changed(cx.page);
        }
        Ok(())
    }
}

impl web::StyleSheetListImpl for Web {
    fn item(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<ObjectId>> {
        let root = cx.page.with::<SheetListObject, _>(this, |l| l.root)?;
        let owner = {
            let dom = cx.dom();
            sheet_owners(cx.page, &dom, root)
                .get(index as usize)
                .copied()
        };
        Ok(owner.and_then(|el| sheet_of_element(cx, el)))
    }

    fn length(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        let root = cx.page.with::<SheetListObject, _>(this, |l| l.root)?;
        let dom = cx.dom();
        Ok(sheet_owners(cx.page, &dom, root).len() as u32)
    }

    fn indexed_get(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<ObjectId>> {
        Self::item(cx, this, index)
    }
}

impl web::MediaListImpl for Web {
    fn media_text(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        let owner = cx.page.with::<MediaListObject, _>(this, |m| m.sheet)?;
        sheet(cx, owner, |s| s.media.join(", "))
    }

    fn set_media_text(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        let owner = cx.page.with::<MediaListObject, _>(this, |m| m.sheet)?;
        sheet(cx, owner, |s| s.media = parse_media(&value))?;
        changed(cx.page);
        Ok(())
    }

    fn length(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        let owner = cx.page.with::<MediaListObject, _>(this, |m| m.sheet)?;
        sheet(cx, owner, |s| s.media.len() as u32)
    }

    fn item(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<String>> {
        let owner = cx.page.with::<MediaListObject, _>(this, |m| m.sheet)?;
        sheet(cx, owner, |s| s.media.get(index as usize).cloned())
    }

    fn append_medium(cx: &mut Cx<'_>, this: ObjectId, medium: String) -> Fallible<()> {
        let owner = cx.page.with::<MediaListObject, _>(this, |m| m.sheet)?;
        let medium = medium.trim().to_ascii_lowercase();
        if medium.is_empty() {
            return Ok(());
        }
        sheet(cx, owner, |s| {
            if !s.media.contains(&medium) {
                s.media.push(medium);
            }
        })?;
        changed(cx.page);
        Ok(())
    }

    fn delete_medium(cx: &mut Cx<'_>, this: ObjectId, medium: String) -> Fallible<()> {
        let owner = cx.page.with::<MediaListObject, _>(this, |m| m.sheet)?;
        let medium = medium.trim().to_ascii_lowercase();
        let removed = sheet(cx, owner, |s| {
            let before = s.media.len();
            s.media.retain(|m| m != &medium);
            s.media.len() != before
        })?;
        if !removed {
            return Err(Exception::not_found(format!(
                "Failed to delete '{medium}' in 'MediaList': no such medium"
            )));
        }
        changed(cx.page);
        Ok(())
    }

    fn indexed_get(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Option<String>> {
        Self::item(cx, this, index)
    }
}

impl web::LinkStyleImpl for Web {
    fn sheet(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<ObjectId>> {
        Ok(sheet_of_element(cx, this))
    }
}

/// `styleSheets` and `adoptedStyleSheets`, for documents and shadow roots.
pub(crate) fn style_sheets(cx: &mut Cx<'_>, root: NodeId) -> ObjectId {
    cx.page.alloc(SheetListObject { root })
}

pub(crate) fn adopted_style_sheets(cx: &Cx<'_>, root: NodeId) -> Vec<ObjectId> {
    cx.page
        .styles
        .adopted
        .borrow()
        .get(&root)
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn set_adopted_style_sheets(
    cx: &mut Cx<'_>,
    root: NodeId,
    sheets: Vec<ObjectId>,
) -> Fallible<()> {
    for &id in &sheets {
        if !sheet(cx, id, |s| s.constructed())? {
            return Err(Exception::not_allowed(
                "Can't adopt non-constructed stylesheets.",
            ));
        }
    }
    let is_root = matches!(
        cx.dom().kind(root),
        NodeKind::Document(_) | NodeKind::DocumentFragment(_)
    );
    if !is_root {
        return Ok(());
    }
    let old = cx
        .page
        .styles
        .adopted
        .borrow_mut()
        .insert(root, sheets.clone())
        .unwrap_or_default();
    for id in &sheets {
        cx.pin(*id);
    }
    for id in old {
        cx.unpin(id);
    }
    changed(cx.page);
    Ok(())
}

/// The document's adopted sheets as the style engine should see them:
/// a key, the media list and the text of each enabled one.
pub(crate) fn adopted_for_engine(
    page: &PageState,
    document: NodeId,
) -> Vec<(u64, Vec<String>, String)> {
    let adopted = page.styles.adopted.borrow();
    let Some(sheets) = adopted.get(&document) else {
        return Vec::new();
    };
    sheets
        .iter()
        .filter_map(|&id| {
            let (media, text) = engine_view(page, id)?;
            let mut hasher = DefaultHasher::new();
            id.hash(&mut hasher);
            text.hash(&mut hasher);
            Some((hasher.finish(), media, text))
        })
        .collect()
}
