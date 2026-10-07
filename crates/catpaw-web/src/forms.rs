//! Forms: owners, selectedness, submission and reset
//! (<https://html.spec.whatwg.org/multipage/forms.html>).
//!
//! Submission builds the entry list, lets `formdata` listeners add to it,
//! encodes it by the form's method and enctype and asks the embedder to
//! navigate; `dialog` and script-only schemes do nothing here.

use catpaw_dom::{Dom, NodeId};
use catpaw_js::{EventTargetRef, Exception, Fallible, ObjectId};

use crate::collections::{self, ListSource};
use crate::element::{self, child_text_content};
use crate::events::{self, Event, EventData};
use crate::generated::{self as web, InterfaceId};
use crate::page::{Cx, NavigationRequest, PageState};
use crate::{Web, file_api, node};

fn is_html(dom: &Dom, el: NodeId, local: &str) -> bool {
    dom.is_html_element(el, local)
}

/// The form a control belongs to: the one its `form` attribute names, if
/// that is a form in the same tree, else the nearest form ancestor.
pub(crate) fn form_owner(dom: &Dom, control: NodeId) -> Option<NodeId> {
    if let Some(id) = dom.attr(control, "form") {
        let root = dom.root_of(control);
        return dom
            .descendants(root)
            .find(|&n| is_html(dom, n, "form") && dom.attr(n, "id") == Some(id));
    }
    dom.ancestors(control).find(|&a| is_html(dom, a, "form"))
}

/// Elements listed in `form.elements`, in tree order.
pub(crate) fn listed_controls(dom: &Dom, form: NodeId) -> Vec<NodeId> {
    let root = dom.root_of(form);
    dom.descendants(root)
        .filter(|&n| {
            let listed = dom.element(n).is_some_and(|el| {
                el.is_html()
                    && match &*el.name.local {
                        "button" | "fieldset" | "object" | "output" | "select" | "textarea" => true,
                        "input" => !el
                            .attr("type")
                            .is_some_and(|t| t.eq_ignore_ascii_case("image")),
                        _ => false,
                    }
            });
            listed && form_owner(dom, n) == Some(form)
        })
        .collect()
}

/// A control that takes part in submission.
fn is_submittable(dom: &Dom, el: NodeId) -> bool {
    dom.element(el).is_some_and(|e| {
        e.is_html() && matches!(&*e.name.local, "button" | "input" | "select" | "textarea")
    })
}

pub(crate) fn is_disabled(dom: &Dom, el: NodeId) -> bool {
    std::iter::once(el).chain(dom.ancestors(el)).any(|a| {
        dom.element(a).is_some_and(|e| {
            e.is_html()
                && e.has_attr("disabled")
                && matches!(
                    &*e.name.local,
                    "input" | "select" | "textarea" | "fieldset" | "button"
                )
        })
    })
}

// ---------------------------------------------------------------- options

/// The options of a select, in tree order (those in `optgroup`s too).
pub(crate) fn options_of(dom: &Dom, select: NodeId) -> Vec<NodeId> {
    dom.children(select)
        .flat_map(|child| {
            if is_html(dom, child, "optgroup") {
                dom.children(child).collect::<Vec<_>>()
            } else {
                vec![child]
            }
        })
        .filter(|&n| is_html(dom, n, "option"))
        .collect()
}

/// An option's selectedness: what script set, else its attribute.
pub(crate) fn is_selected(cx: &Cx<'_>, option: NodeId) -> bool {
    let dirty = cx
        .page
        .form_state
        .borrow()
        .get(&option)
        .and_then(|s| s.selected);
    dirty.unwrap_or_else(|| cx.dom().attr(option, "selected").is_some())
}

fn set_selectedness(cx: &Cx<'_>, option: NodeId, value: bool) {
    cx.page
        .form_state
        .borrow_mut()
        .entry(option)
        .or_default()
        .selected = Some(value);
    // Script decided: the select shows what it was told, even nothing.
    let select = {
        let dom = cx.dom();
        dom.ancestors(option).find(|&a| is_html(&dom, a, "select"))
    };
    if let Some(select) = select {
        cx.page
            .form_state
            .borrow_mut()
            .entry(select)
            .or_default()
            .no_fallback = true;
    }
}

/// The selectedness setting algorithm runs again for `select`: with
/// nothing selected, a single select shows its first option.
pub(crate) fn selectedness_reset(page: &PageState, select: NodeId) {
    if let Some(state) = page.form_state.borrow_mut().get_mut(&select) {
        state.no_fallback = false;
    }
}

/// Nodes were inserted or are being removed: the selects they sit in run
/// the selectedness setting algorithm again.
pub(crate) fn options_changed(page: &PageState, nodes: &[NodeId]) {
    let selects: Vec<NodeId> = {
        let dom = page.dom.borrow();
        nodes
            .iter()
            .filter_map(|&n| dom.ancestors(n).find(|&a| is_html(&dom, a, "select")))
            .collect()
    };
    for select in selects {
        selectedness_reset(page, select);
    }
}

/// Selects an option; in a single-select, the others become unselected.
pub(crate) fn select_option(cx: &Cx<'_>, option: NodeId, selected: bool) {
    set_selectedness(cx, option, selected);
    if !selected {
        return;
    }
    let others: Vec<NodeId> = {
        let dom = cx.dom();
        let Some(select) = dom.ancestors(option).find(|&a| is_html(&dom, a, "select")) else {
            return;
        };
        if dom.attr(select, "multiple").is_some() {
            return;
        }
        options_of(&dom, select)
            .into_iter()
            .filter(|&o| o != option)
            .collect()
    };
    for other in others {
        set_selectedness(cx, other, false);
    }
}

/// The options shown as selected: the selected ones, or for a single-select
/// with none, the first enabled one.
pub(crate) fn selected_options(cx: &Cx<'_>, select: NodeId) -> Vec<NodeId> {
    let (options, multiple) = {
        let dom = cx.dom();
        (
            options_of(&dom, select),
            dom.attr(select, "multiple").is_some(),
        )
    };
    let mut selected: Vec<NodeId> = options
        .iter()
        .copied()
        .filter(|&o| is_selected(cx, o))
        .collect();
    let no_fallback = cx
        .page
        .form_state
        .borrow()
        .get(&select)
        .is_some_and(|s| s.no_fallback);
    if selected.is_empty() && !multiple && !no_fallback {
        let dom = cx.dom();
        if let Some(first) = options.iter().copied().find(|&o| !is_disabled(&dom, o)) {
            selected.push(first);
        }
    }
    if !multiple && selected.len() > 1 {
        selected.truncate(1);
    }
    selected
}

/// An option's value: its attribute, or its text.
pub(crate) fn option_value(dom: &Dom, option: NodeId) -> String {
    match dom.attr(option, "value") {
        Some(v) => v.to_string(),
        None => option_text(dom, option),
    }
}

pub(crate) fn option_text(dom: &Dom, option: NodeId) -> String {
    child_text_content(dom, option)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// ------------------------------------------------------------- validation

/// Whether a control satisfies its constraints (`required` and the email
/// shape); `invalid` fires on each that does not.
fn validate(cx: &mut Cx<'_>, form: NodeId) -> bool {
    let controls: Vec<NodeId> = {
        let dom = cx.dom();
        listed_controls(&dom, form)
            .into_iter()
            .filter(|&c| is_submittable(&dom, c) && !is_disabled(&dom, c))
            .collect()
    };
    let mut valid = true;
    for control in controls {
        if control_is_valid(cx, control) {
            continue;
        }
        valid = false;
        events::fire(cx, EventTargetRef::Node(control), "invalid", false, true);
    }
    valid
}

fn control_is_valid(cx: &mut Cx<'_>, control: NodeId) -> bool {
    let (local, type_, required, name, readonly) = {
        let dom = cx.dom();
        let el = dom.element(control).expect("a control");
        (
            el.name.local.to_string(),
            el.attr("type")
                .map(|t| t.trim().to_ascii_lowercase())
                .unwrap_or_else(|| "text".to_string()),
            el.has_attr("required"),
            el.attr("name").unwrap_or_default().to_string(),
            el.has_attr("readonly"),
        )
    };
    if readonly {
        return true;
    }
    match local.as_str() {
        "input" => match type_.as_str() {
            "checkbox" => !required || element::is_checked(cx, control),
            "radio" => {
                if !required {
                    return true;
                }
                // Any button of the group satisfies the requirement.
                let group: Vec<NodeId> = {
                    let dom = cx.dom();
                    let owner = form_owner(&dom, control);
                    dom.descendants(dom.root_of(control))
                        .filter(|&n| {
                            is_html(&dom, n, "input")
                                && dom
                                    .attr(n, "type")
                                    .is_some_and(|t| t.eq_ignore_ascii_case("radio"))
                                && !name.is_empty()
                                && dom.attr(n, "name") == Some(name.as_str())
                                && form_owner(&dom, n) == owner
                        })
                        .collect()
                };
                group.iter().any(|&n| element::is_checked(cx, n))
            }
            "submit" | "button" | "reset" | "image" | "hidden" | "file" => true,
            _ => {
                let value =
                    <Web as web::HTMLInputElementImpl>::value(cx, control).unwrap_or_default();
                if required && value.is_empty() {
                    return false;
                }
                if type_ == "email" && !value.is_empty() {
                    return value.split(',').all(|part| {
                        let part = part.trim();
                        let Some((local, domain)) = part.rsplit_once('@') else {
                            return false;
                        };
                        !local.is_empty() && !domain.is_empty() && !domain.contains('@')
                    });
                }
                true
            }
        },
        "textarea" => {
            !required
                || !<Web as web::HTMLTextAreaElementImpl>::value(cx, control)
                    .unwrap_or_default()
                    .is_empty()
        }
        "select" => {
            if !required {
                return true;
            }
            let selected = selected_options(cx, control);
            let dom = cx.dom();
            selected
                .first()
                .is_some_and(|&o| !option_value(&dom, o).is_empty())
        }
        _ => true,
    }
}

// ------------------------------------------------------------- submission

/// How a form asks to be submitted.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Submission {
    /// `form.submit()`: no validation, no `submit` event.
    FromSubmitMethod,
    /// A button, `requestSubmit()` or implicit submission.
    Normal,
}

fn submitter_attr(
    cx: &Cx<'_>,
    form: NodeId,
    submitter: Option<NodeId>,
    name: &str,
) -> Option<String> {
    let dom = cx.dom();
    submitter
        .and_then(|s| dom.attr(s, &format!("form{name}")).map(str::to_string))
        .or_else(|| dom.attr(form, name).map(str::to_string))
}

/// <https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#form-submission-algorithm>
pub(crate) fn submit(cx: &mut Cx<'_>, form: NodeId, submitter: Option<NodeId>, how: Submission) {
    {
        let dom = cx.dom();
        if !dom.contains(form) || !dom.is_connected(form) {
            return;
        }
    }
    if how == Submission::Normal {
        let no_validate = {
            let dom = cx.dom();
            dom.attr(form, "novalidate").is_some()
                || submitter.is_some_and(|s| dom.attr(s, "formnovalidate").is_some())
        };
        if !no_validate && !validate(cx, form) {
            return;
        }
        let mut event = Event::new("submit", true, true, cx.page.clock.peek());
        event.iface = InterfaceId::SubmitEvent;
        event.trusted = true;
        event.data = EventData::Submit { submitter };
        let event = cx.page.alloc(event);
        if !events::dispatch(cx, EventTargetRef::Node(form), event) {
            return;
        }
        // A listener may have taken the form out of the document.
        let dom = cx.dom();
        if !dom.contains(form) || !dom.is_connected(form) {
            return;
        }
    }

    let entries = match file_api::entry_list(cx, form, submitter) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let form_data = file_api::form_data_object(cx.page, entries);
    cx.pin(form_data);
    let mut event = Event::new("formdata", true, false, cx.page.clock.peek());
    event.iface = InterfaceId::FormDataEvent;
    event.trusted = true;
    event.data = EventData::FormData { form_data };
    let event = cx.page.alloc(event);
    events::dispatch(cx, EventTargetRef::Node(form), event);

    let method = submitter_attr(cx, form, submitter, "method")
        .map(|m| m.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let method = match method.as_str() {
        "post" => "POST",
        "dialog" => {
            cx.unpin(form_data);
            return;
        }
        _ => "GET",
    };
    let enctype = submitter_attr(cx, form, submitter, "enctype")
        .map(|e| e.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let action = submitter_attr(cx, form, submitter, "action")
        .filter(|a| !a.trim().is_empty())
        .and_then(|a| {
            node::base_url(cx, form)
                .unwrap_or_else(|| cx.page.base_url())
                .join(a.trim())
                .ok()
        })
        .unwrap_or_else(|| cx.page.url.borrow().clone());

    let request = match (action.scheme(), method) {
        ("javascript", _) => None,
        (_, "GET") => {
            let query = file_api::urlencoded_body(cx, form_data).unwrap_or_default();
            let mut url = action.clone();
            url.set_query(Some(&query));
            Some(NavigationRequest::get(url, false))
        }
        (_, _) => {
            let body = match enctype.as_str() {
                "multipart/form-data" => file_api::multipart_body(cx, form_data).ok(),
                "text/plain" => file_api::text_plain_body(cx, form_data)
                    .ok()
                    .map(|b| (b, "text/plain".to_string())),
                _ => file_api::urlencoded_body(cx, form_data).ok().map(|b| {
                    (
                        b.into_bytes(),
                        "application/x-www-form-urlencoded".to_string(),
                    )
                }),
            };
            body.map(|(bytes, content_type)| NavigationRequest {
                method: "POST".to_string(),
                body: Some((content_type, bytes)),
                ..NavigationRequest::get(action.clone(), false)
            })
        }
    };
    cx.unpin(form_data);
    if let Some(request) = request {
        *cx.page.navigation.borrow_mut() = Some(request);
    }
}

/// <https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#resetting-a-form>
pub(crate) fn reset(cx: &mut Cx<'_>, form: NodeId) {
    if !events::fire(cx, EventTargetRef::Node(form), "reset", true, true) {
        return;
    }
    let controls: Vec<NodeId> = {
        let dom = cx.dom();
        let mut controls = listed_controls(&dom, form);
        for select in controls.clone() {
            if is_html(&dom, select, "select") {
                controls.extend(options_of(&dom, select));
            }
        }
        controls
    };
    let mut state = cx.page.form_state.borrow_mut();
    for control in controls {
        state.remove(&control);
    }
}

/// The button that a form submits with when Enter is pressed in a field:
/// its first submit button in tree order.
pub(crate) fn default_button(dom: &Dom, form: NodeId) -> Option<NodeId> {
    listed_controls(dom, form).into_iter().find(|&c| {
        dom.element(c).is_some_and(|el| {
            let type_ = el.attr("type").map(|t| t.trim().to_ascii_lowercase());
            match &*el.name.local {
                "button" => matches!(type_.as_deref(), None | Some("submit")),
                "input" => matches!(type_.as_deref(), Some("submit") | Some("image")),
                _ => false,
            }
        })
    })
}

/// <https://html.spec.whatwg.org/multipage/form-control-infrastructure.html#implicit-submission>
pub(crate) fn implicit_submission(cx: &mut Cx<'_>, control: NodeId) {
    let (form, button, field_count) = {
        let dom = cx.dom();
        let Some(form) = form_owner(&dom, control) else {
            return;
        };
        let button = default_button(&dom, form);
        let fields = listed_controls(&dom, form)
            .into_iter()
            .filter(|&c| {
                is_html(&dom, c, "input")
                    && matches!(
                        dom.attr(c, "type")
                            .map(|t| t.trim().to_ascii_lowercase())
                            .as_deref(),
                        None | Some(
                            "text"
                                | "search"
                                | "url"
                                | "tel"
                                | "email"
                                | "password"
                                | "date"
                                | "month"
                                | "week"
                                | "time"
                                | "datetime-local"
                                | "number"
                        )
                    )
            })
            .count();
        (form, button, fields)
    };
    match button {
        Some(button) => {
            if !is_disabled(&cx.dom(), button) {
                crate::activation::click(cx, button, true);
            }
        }
        None if field_count == 1 => submit(cx, form, None, Submission::Normal),
        None => {}
    }
}

// ---------------------------------------------------------------- bindings

fn check_form(cx: &Cx<'_>, form: NodeId) -> Fallible<()> {
    node::check(cx, form)?;
    if !is_html(&cx.dom(), form, "form") {
        return Err(Exception::type_error("not a form element"));
    }
    Ok(())
}

impl web::HTMLFormElementImpl for Web {
    fn submit(cx: &mut Cx<'_>, this: NodeId) -> Fallible<()> {
        check_form(cx, this)?;
        submit(cx, this, None, Submission::FromSubmitMethod);
        Ok(())
    }

    fn request_submit(cx: &mut Cx<'_>, this: NodeId, submitter: Option<NodeId>) -> Fallible<()> {
        check_form(cx, this)?;
        if let Some(button) = submitter {
            let dom = cx.dom();
            let is_submit_button = dom.element(button).is_some_and(|el| {
                el.is_html()
                    && match &*el.name.local {
                        "button" => el
                            .attr("type")
                            .is_none_or(|t| t.eq_ignore_ascii_case("submit")),
                        "input" => el.attr("type").is_some_and(|t| {
                            t.eq_ignore_ascii_case("submit") || t.eq_ignore_ascii_case("image")
                        }),
                        _ => false,
                    }
            });
            if !is_submit_button {
                return Err(Exception::type_error(
                    "The submitter is not a submit button",
                ));
            }
            if form_owner(&dom, button) != Some(this) {
                return Err(Exception::not_found(
                    "The submitter is not owned by this form",
                ));
            }
        }
        submit(cx, this, submitter, Submission::Normal);
        Ok(())
    }

    fn reset(cx: &mut Cx<'_>, this: NodeId) -> Fallible<()> {
        check_form(cx, this)?;
        reset(cx, this);
        Ok(())
    }

    fn elements(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        check_form(cx, this)?;
        Ok(collections::html_collection_as(
            cx.page,
            ListSource::FormControls(this),
            InterfaceId::HTMLFormControlsCollection,
        ))
    }

    fn length(cx: &mut Cx<'_>, this: NodeId) -> Fallible<u32> {
        check_form(cx, this)?;
        Ok(listed_controls(&cx.dom(), this).len() as u32)
    }

    fn check_validity(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        check_form(cx, this)?;
        Ok(validate(cx, this))
    }

    fn action(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        let attr = cx.dom().attr(this, "action").map(str::to_string);
        Ok(match attr {
            Some(a) if !a.trim().is_empty() => node::base_url(cx, this)
                .unwrap_or_else(|| cx.page.base_url())
                .join(a.trim())
                .map(|u| u.to_string())
                .unwrap_or(a),
            _ => cx.page.url.borrow().to_string(),
        })
    }

    fn set_action(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        element::set_attr(cx, this, "action", value)
    }

    fn method(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        let value = cx
            .dom()
            .attr(this, "method")
            .map(|m| m.trim().to_ascii_lowercase());
        Ok(match value.as_deref() {
            Some("post") => "post",
            Some("dialog") => "dialog",
            _ => "get",
        }
        .to_string())
    }

    fn set_method(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        element::set_attr(cx, this, "method", value)
    }

    fn enctype(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        let value = cx
            .dom()
            .attr(this, "enctype")
            .map(|m| m.trim().to_ascii_lowercase());
        Ok(match value.as_deref() {
            Some("multipart/form-data") => "multipart/form-data",
            Some("text/plain") => "text/plain",
            _ => "application/x-www-form-urlencoded",
        }
        .to_string())
    }

    fn set_enctype(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        element::set_attr(cx, this, "enctype", value)
    }

    fn encoding(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        <Self as web::HTMLFormElementImpl>::enctype(cx, this)
    }

    fn set_encoding(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        element::set_attr(cx, this, "enctype", value)
    }

    fn target(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(cx
            .dom()
            .attr(this, "target")
            .map(str::to_string)
            .unwrap_or_default())
    }

    fn set_target(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        element::set_attr(cx, this, "target", value)
    }

    fn report_validity(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        check_form(cx, this)?;
        Ok(validate(cx, this))
    }
}

impl web::HTMLSelectElementImpl for Web {
    fn value(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        let selected = selected_options(cx, this);
        let dom = cx.dom();
        Ok(selected
            .first()
            .map(|&o| option_value(&dom, o))
            .unwrap_or_default())
    }

    fn set_value(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        let options = options_of(&cx.dom(), this);
        let mut matched = false;
        for option in options {
            let is_match = !matched && option_value(&cx.dom(), option) == value;
            set_selectedness(cx, option, is_match);
            matched |= is_match;
        }
        Ok(())
    }

    fn selected_index(cx: &mut Cx<'_>, this: NodeId) -> Fallible<i32> {
        node::check(cx, this)?;
        let selected = selected_options(cx, this);
        let options = options_of(&cx.dom(), this);
        Ok(selected
            .first()
            .and_then(|s| options.iter().position(|o| o == s))
            .map_or(-1, |i| i as i32))
    }

    fn set_selected_index(cx: &mut Cx<'_>, this: NodeId, index: i32) -> Fallible<()> {
        node::check(cx, this)?;
        let options = options_of(&cx.dom(), this);
        for (i, option) in options.into_iter().enumerate() {
            set_selectedness(cx, option, i as i32 == index);
        }
        Ok(())
    }

    fn options(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        node::check(cx, this)?;
        Ok(collections::html_collection_as(
            cx.page,
            ListSource::Options(this),
            InterfaceId::HTMLOptionsCollection,
        ))
    }

    fn length(cx: &mut Cx<'_>, this: NodeId) -> Fallible<u32> {
        node::check(cx, this)?;
        Ok(options_of(&cx.dom(), this).len() as u32)
    }

    fn set_length(cx: &mut Cx<'_>, this: NodeId, length: u32) -> Fallible<()> {
        node::check(cx, this)?;
        let options = options_of(&cx.dom(), this);
        if (length as usize) < options.len() {
            for option in &options[length as usize..] {
                <Web as web::ChildNodeImpl>::remove(cx, *option)?;
            }
        } else {
            for _ in options.len()..length as usize {
                let option = cx.dom_mut().create_html_element("option", Vec::new());
                <Web as web::NodeImpl>::append_child(cx, this, option)?;
            }
        }
        Ok(())
    }

    fn item(cx: &mut Cx<'_>, this: NodeId, index: u32) -> Fallible<Option<NodeId>> {
        node::check(cx, this)?;
        Ok(options_of(&cx.dom(), this).get(index as usize).copied())
    }

    fn selected_options(cx: &mut Cx<'_>, this: NodeId) -> Fallible<ObjectId> {
        node::check(cx, this)?;
        let selected = selected_options(cx, this);
        Ok(collections::static_html_collection(cx.page, selected))
    }

    fn form(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        node::check(cx, this)?;
        Ok(form_owner(&cx.dom(), this))
    }
}

impl web::HTMLOptionElementImpl for Web {
    fn selected(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        node::check(cx, this)?;
        Ok(is_selected(cx, this))
    }

    fn set_selected(cx: &mut Cx<'_>, this: NodeId, value: bool) -> Fallible<()> {
        node::check(cx, this)?;
        select_option(cx, this, value);
        Ok(())
    }

    fn default_selected(cx: &mut Cx<'_>, this: NodeId) -> Fallible<bool> {
        node::check(cx, this)?;
        Ok(cx.dom().attr(this, "selected").is_some())
    }

    fn set_default_selected(cx: &mut Cx<'_>, this: NodeId, value: bool) -> Fallible<()> {
        node::check(cx, this)?;
        if value {
            element::set_attr(cx, this, "selected", String::new())
        } else {
            element::remove_attr(cx, this, "selected");
            Ok(())
        }
    }

    fn value(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(option_value(&cx.dom(), this))
    }

    fn set_value(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        element::set_attr(cx, this, "value", value)
    }

    fn text(cx: &mut Cx<'_>, this: NodeId) -> Fallible<String> {
        node::check(cx, this)?;
        Ok(option_text(&cx.dom(), this))
    }

    fn set_text(cx: &mut Cx<'_>, this: NodeId, value: String) -> Fallible<()> {
        node::check(cx, this)?;
        <Web as web::NodeImpl>::set_text_content(cx, this, Some(value))
    }

    fn index(cx: &mut Cx<'_>, this: NodeId) -> Fallible<i32> {
        node::check(cx, this)?;
        let dom = cx.dom();
        let Some(select) = dom.ancestors(this).find(|&a| is_html(&dom, a, "select")) else {
            return Ok(0);
        };
        Ok(options_of(&dom, select)
            .iter()
            .position(|&o| o == this)
            .map_or(0, |i| i as i32))
    }

    fn form(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        node::check(cx, this)?;
        let dom = cx.dom();
        let select = dom.ancestors(this).find(|&a| is_html(&dom, a, "select"));
        Ok(select.and_then(|s| form_owner(&dom, s)))
    }
}

impl web::HTMLButtonElementImpl for Web {
    fn form(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        node::check(cx, this)?;
        Ok(form_owner(&cx.dom(), this))
    }
}

impl web::HTMLLabelElementImpl for Web {
    fn form(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        node::check(cx, this)?;
        let control = labeled_control(&cx.dom(), this);
        Ok(control.and_then(|c| form_owner(&cx.dom(), c)))
    }

    fn control(cx: &mut Cx<'_>, this: NodeId) -> Fallible<Option<NodeId>> {
        node::check(cx, this)?;
        Ok(labeled_control(&cx.dom(), this))
    }
}

/// The control a label is for: the element its `for` names, else the first
/// labelable descendant.
pub(crate) fn labeled_control(dom: &Dom, label: NodeId) -> Option<NodeId> {
    let labelable = |n: NodeId| {
        dom.element(n).is_some_and(|el| {
            el.is_html()
                && match &*el.name.local {
                    "button" | "meter" | "output" | "progress" | "select" | "textarea" => true,
                    "input" => !el
                        .attr("type")
                        .is_some_and(|t| t.eq_ignore_ascii_case("hidden")),
                    _ => false,
                }
        })
    };
    if let Some(id) = dom.attr(label, "for") {
        let root = dom.root_of(label);
        return dom
            .descendants(root)
            .find(|&n| dom.attr(n, "id") == Some(id))
            .filter(|&n| labelable(n));
    }
    dom.descendants(label).find(|&n| labelable(n))
}

impl web::HTMLFormControlsCollectionImpl for Web {
    fn named_item(
        cx: &mut Cx<'_>,
        this: ObjectId,
        name: String,
    ) -> Fallible<Option<web::RadioNodeListOrElement>> {
        <Self as web::HTMLFormControlsCollectionImpl>::named_get(cx, this, &name)
    }

    fn named_get(
        cx: &mut Cx<'_>,
        this: ObjectId,
        name: &str,
    ) -> Fallible<Option<web::RadioNodeListOrElement>> {
        if name.is_empty() {
            return Ok(None);
        }
        let matches: Vec<NodeId> = {
            let items = collections::items_of(cx, this)?;
            let dom = cx.dom();
            items
                .into_iter()
                .filter(|&n| dom.attr(n, "id") == Some(name) || dom.attr(n, "name") == Some(name))
                .collect()
        };
        Ok(match matches.len() {
            0 => None,
            1 => Some(web::RadioNodeListOrElement::Element(matches[0])),
            _ => Some(web::RadioNodeListOrElement::RadioNodeList(
                collections::radio_node_list(cx.page, matches),
            )),
        })
    }

    fn named_properties(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Vec<String>> {
        let items = collections::items_of(cx, this)?;
        let dom = cx.dom();
        let mut names = Vec::new();
        for item in items {
            for attr in ["id", "name"] {
                if let Some(value) = dom.attr(item, attr)
                    && !value.is_empty()
                    && !names.iter().any(|n| n == value)
                {
                    names.push(value.to_string());
                }
            }
        }
        Ok(names)
    }
}

impl web::RadioNodeListImpl for Web {
    fn value(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        let items = collections::items_of(cx, this)?;
        for item in items {
            let is_radio = {
                let dom = cx.dom();
                is_html(&dom, item, "input")
                    && dom
                        .attr(item, "type")
                        .is_some_and(|t| t.eq_ignore_ascii_case("radio"))
            };
            if is_radio && element::is_checked(cx, item) {
                return Ok(cx
                    .dom()
                    .attr(item, "value")
                    .map_or_else(|| "on".to_string(), str::to_string));
            }
        }
        Ok(String::new())
    }

    fn set_value(cx: &mut Cx<'_>, this: ObjectId, value: String) -> Fallible<()> {
        let items = collections::items_of(cx, this)?;
        let target = {
            let dom = cx.dom();
            items.into_iter().find(|&item| {
                is_html(&dom, item, "input")
                    && dom
                        .attr(item, "type")
                        .is_some_and(|t| t.eq_ignore_ascii_case("radio"))
                    && dom.attr(item, "value").unwrap_or("on") == value
            })
        };
        if let Some(radio) = target {
            element::set_checked(cx, radio, true);
        }
        Ok(())
    }
}

impl web::SubmitEventImpl for Web {
    fn submitter(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Option<NodeId>> {
        cx.page.with::<Event, _>(this, |e| match &e.data {
            EventData::Submit { submitter } => *submitter,
            _ => None,
        })
    }
}

impl web::FormDataEventImpl for Web {
    fn form_data(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<ObjectId> {
        let form_data = cx.page.with::<Event, _>(this, |e| match &e.data {
            EventData::FormData { form_data } => Some(*form_data),
            _ => None,
        })?;
        form_data.ok_or_else(|| Exception::invalid_state("the event has no form data"))
    }
}
