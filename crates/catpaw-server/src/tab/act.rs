//! Actions on a tab, and what they led to.

use std::cell::Cell;
use std::fmt::Write as _;
use std::rc::Rc;

use catpaw_agent::snapshot::{quote, truncate};
use catpaw_engine::{
    ActionError, DialogPolicy, EngineError, FrameId, InputError, LoopLimits, Value,
};
use catpaw_protocol::params::{self, ActionOptions, DialogChoice, SnapshotMode};
use catpaw_protocol::wording::{ErrorCode, advice, consequence};
use catpaw_web::page::Cx;
use catpaw_web::{agent, input};

use super::{
    Aim, EVAL_BYTES, GroupState, Use, View, floor_char_boundary, is_live, parse_url, png_size,
};
use crate::output::{CallResult, Failure, ToolOutput};

/// The options of a select that the words name (label, value, then
/// loosely, when only one fits), or an error listing the options.
fn choose_options(
    state: &catpaw_web::PageState,
    select: catpaw_dom::NodeId,
    wanted: &[String],
    what: &str,
) -> Result<Vec<(catpaw_dom::NodeId, String)>, Failure> {
    let all: Vec<(catpaw_dom::NodeId, String, String)> = agent::options_of(state, select)
        .into_iter()
        .map(|o| {
            let (label, value) = agent::option_label_and_value(state, o);
            (o, label, value)
        })
        .collect();
    let mut chosen = Vec::new();
    for wanted in wanted.iter().cloned() {
        let w = wanted.split_whitespace().collect::<Vec<_>>().join(" ");
        let found = all
            .iter()
            .find(|(_, label, _)| *label == w)
            .or_else(|| all.iter().find(|(_, _, value)| *value == wanted))
            .or_else(|| {
                all.iter()
                    .find(|(_, label, _)| label.to_lowercase() == w.to_lowercase())
            })
            .or_else(|| {
                // Loosely, when only one option fits: the same words,
                // punctuation aside ("Price low to high" for "Price (low
                // to high)"), or words all in the label.
                let key = loose(&wanted);
                let words: Vec<&str> = key.split(' ').filter(|w| !w.is_empty()).collect();
                let same: Vec<_> = all.iter().filter(|(_, l, _)| loose(l) == key).collect();
                let within: Vec<_> = all
                    .iter()
                    .filter(|(_, l, _)| {
                        let label = loose(l);
                        let label: Vec<&str> = label.split(' ').collect();
                        !words.is_empty() && words.iter().all(|w| label.contains(w))
                    })
                    .collect();
                match (same.as_slice(), within.as_slice()) {
                    ([one], _) => Some(*one),
                    ([], [one]) => Some(*one),
                    _ => None,
                }
            });
        match found {
            Some((node, label, _)) => chosen.push((*node, label.clone())),
            None => {
                let listed: Vec<String> = all
                    .iter()
                    .take(30)
                    .map(|(_, label, _)| quote(&truncate(label, 60)))
                    .collect();
                let mut message = format!("no option {} in {what}", quote(&wanted));
                let _ = write!(message, "; options: {}", listed.join(", "));
                if all.len() > 30 {
                    let _ = write!(message, " (+{} more)", all.len() - 30);
                }
                return Err(Failure::new(ErrorCode::NotFound, message));
            }
        }
    }
    Ok(chosen)
}

/// `true`/`false` as a field's value may say it.
fn flag(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "checked" | "1" => Some(true),
        "false" | "no" | "off" | "unchecked" | "0" => Some(false),
        _ => None,
    }
}

/// How a click call asks to click.
fn click_options(p: &params::Click) -> Result<input::ClickOptions, Failure> {
    let mut how = input::ClickOptions {
        button: match p.button {
            None | Some(params::MouseButton::Left) => 0,
            Some(params::MouseButton::Middle) => 1,
            Some(params::MouseButton::Right) => 2,
        },
        count: p.count.unwrap_or(1).clamp(1, 3),
        ..input::ClickOptions::default()
    };
    for key in p.modifiers.iter().flatten() {
        match key.trim().to_ascii_lowercase().as_str() {
            "control" | "ctrl" => how.ctrl = true,
            "shift" => how.shift = true,
            "alt" | "option" => how.alt = true,
            "meta" | "cmd" | "command" => how.meta = true,
            _ => {
                return Err(Failure::bad_argument(format!(
                    "{key:?} is not a modifier (Control, Shift, Alt, Meta)"
                )));
            }
        }
    }
    Ok(how)
}

/// Text as option matching compares it loosely: lowercase words, with
/// punctuation gone.
fn loose(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

impl GroupState {
    /// Runs `f` with dialogs answered as `options` say, then back to
    /// dismissing them.
    fn with_dialogs<R>(&mut self, options: &ActionOptions, f: impl FnOnce(&mut Self) -> R) -> R {
        let accept = options.dialog == Some(DialogChoice::Accept)
            || (options.prompt_text.is_some() && options.dialog.is_none());
        self.page.set_dialog_policy(DialogPolicy {
            accept,
            prompt_text: options.prompt_text.clone(),
        });
        let result = f(self);
        self.page.set_dialog_policy(DialogPolicy::default());
        result
    }

    /// Runs an input action on the aimed-at element's page, then reports.
    fn act_on<R>(
        &mut self,
        tab: u32,
        aim: Aim,
        status: String,
        options: &ActionOptions,
        view: View,
        action: impl FnOnce(&mut Cx<'_>, &Aim) -> Result<R, InputError>,
    ) -> CallResult {
        let base = self.baseline(tab);
        let result = self.with_dialogs(options, |g| {
            g.page.input_in(aim.frame, |cx| action(cx, &aim))
        });
        let mut report = self.finish(tab, &base);
        report.acted = Some(aim.r);
        match result {
            Ok(_) => self.page_result(tab, status, report, options.snapshot, view),
            Err(e) => Err(self.action_failure(tab, &aim, e, report)),
        }
    }

    /// Runs an input action on the tab's focused element (or its document).
    fn act_on_focus(
        &mut self,
        tab: u32,
        status: String,
        options: &ActionOptions,
        view: View,
        action: impl FnOnce(&mut Cx<'_>) -> Result<(), InputError>,
    ) -> CallResult {
        let (root, _) = self.root_state(tab)?;
        let base = self.baseline(tab);
        let result = self.with_dialogs(options, |g| g.page.input_in(root, action));
        let report = self.finish(tab, &base);
        match result {
            Ok(()) => self.page_result(tab, status, report, options.snapshot, view),
            Err(ActionError::Input(InputError::Detached)) => {
                Err(Failure::bad_argument("nothing has focus").with(advice::NOTHING_FOCUSED))
            }
            Err(e) => Err(Failure::new(ErrorCode::NavigationFailed, e.to_string())),
        }
    }

    pub(crate) fn navigate(&mut self, tab: u32, p: params::Navigate, view: View) -> CallResult {
        let (root, _) = self.root_state(tab)?;
        let base = self.baseline(tab);
        let (status, result) = match (p.url, p.go) {
            (Some(url), None) => {
                let url = parse_url(&url)?;
                ("ok navigate".to_string(), self.page.goto_in(root, url))
            }
            (None, Some(go)) => {
                let word = match go {
                    params::Go::Back => "back",
                    params::Go::Forward => "forward",
                    params::Go::Reload => "reload",
                };
                let result = if root == FrameId(0) {
                    match go {
                        params::Go::Back => self.page.back(),
                        params::Go::Forward => self.page.forward(),
                        params::Go::Reload => self.page.reload(),
                    }
                    .map_err(ActionError::from)
                } else if go == params::Go::Reload {
                    let url = self.page.url_of(root).ok_or_else(|| {
                        Failure::new(ErrorCode::NoTab, format!("t{tab} is closed"))
                    })?;
                    self.page.goto_in(root, url)
                } else {
                    return Err(Failure::new(
                        ErrorCode::Unsupported,
                        format!("{word} in a tab a page opened"),
                    ));
                };
                (format!("ok {word}"), result)
            }
            (Some(_), Some(_)) => {
                return Err(Failure::bad_argument("pass url or go, not both"));
            }
            (None, None) => {
                return Err(Failure::bad_argument(
                    "pass url, or go: back, forward or reload",
                ));
            }
        };
        let report = self.finish(tab, &base);
        if let Err(e) = result {
            let message = match &e {
                ActionError::Engine(EngineError::Net(net)) => net.to_string(),
                other => other.to_string(),
            };
            return Err(Failure::new(ErrorCode::NavigationFailed, message));
        }
        let mut status = status;
        if report.navigated.is_none() && report.same_document.is_none() && p.go.is_some() {
            status.push_str(" (no page to go to)");
        }
        let mode = p.snapshot.or(Some(SnapshotMode::Full));
        self.page_result(tab, status, report, mode, view)
    }

    pub(crate) fn click(&mut self, tab: u32, p: params::Click, view: View) -> CallResult {
        let options = p.options();
        let aim = self.aim(tab, &p.target)?;
        let force = p.force;
        if !force && aim.point.is_none() {
            self.actionable(tab, &aim, true)?;
        }
        let how = click_options(&p)?;
        let verb = match (how.button, how.count) {
            (0, 1) => "click".to_string(),
            (0, 2) => "double-click".to_string(),
            (0, _) => "triple-click".to_string(),
            (2, 1) => "right-click".to_string(),
            (1, 1) => "middle-click".to_string(),
            (button, count) => {
                let name = if button == 2 { "right" } else { "middle" };
                format!("{name}-click x{count}")
            }
        };
        let mut status = format!("ok {verb} {}", self.aimed(tab, &aim));
        let held: Vec<&str> = [
            (how.ctrl, "Control"),
            (how.shift, "Shift"),
            (how.alt, "Alt"),
            (how.meta, "Meta"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, name)| *name)
        .collect();
        if !held.is_empty() {
            let _ = write!(status, " with {}", held.join("+"));
        }
        self.act_on(tab, aim, status, &options, view, move |cx, aim| {
            match aim.point {
                Some((x, y)) => {
                    input::click_at_with(cx, x, y, how);
                    Ok(())
                }
                // Forced: the element gets the click, whatever covers it.
                None if force => match input::click_element_with(cx, aim.node, how) {
                    Err(
                        InputError::Occluded { .. }
                        | InputError::NotVisible
                        | InputError::OutOfReach,
                    ) => {
                        catpaw_web::activation::click(cx, aim.node, true);
                        Ok(())
                    }
                    other => other.map(drop),
                },
                None => input::click_element_with(cx, aim.node, how).map(drop),
            }
        })
    }

    pub(crate) fn type_text(&mut self, tab: u32, p: params::Type, view: View) -> CallResult {
        let options = p.options();
        let aim = match &p.target {
            Some(target) => self.aim_for(tab, target, Use::Text)?,
            None => {
                let (root, state) = self.root_state(tab)?;
                let node = agent::focused(&state).ok_or_else(|| {
                    Failure::bad_argument("nothing has focus").with(advice::NOTHING_FOCUSED)
                })?;
                let r = self.ref_for(tab, root, node).unwrap_or(0);
                Aim {
                    frame: root,
                    node,
                    r,
                    point: None,
                    retargeted: None,
                }
            }
        };
        self.actionable(tab, &aim, true)?;
        let mut status = format!("ok type {}", self.aimed(tab, &aim));
        if p.submit {
            status.push_str(" + Enter");
        }
        let secret = self.page.frame_state(aim.frame).is_some_and(|state| {
            let dom = state.dom.borrow();
            dom.is_html_element(aim.node, "input")
                && dom
                    .attr(aim.node, "type")
                    .is_some_and(|t| t.trim().eq_ignore_ascii_case("password"))
        });
        let text = p.text;
        let (append, submit) = (p.append, p.submit);
        // A value the agent sets in full is its own; one it adds to still
        // holds what the user typed.
        if !append && let Some(state) = self.page.frame_state(aim.frame) {
            agent::unmask_value(state, aim.node);
        }
        let mut output = self.act_on(tab, aim, status, &options, view, move |cx, aim| {
            if append {
                input::focus(cx, aim.node)?;
                input::type_text(cx, &text)?;
            } else {
                input::fill(cx, aim.node, &text)?;
            }
            if submit {
                input::press(cx, "Enter")?;
            }
            Ok(())
        });
        if let Ok(output) = &mut output {
            output.secret_input = secret;
        }
        output
    }

    /// Sets several fields as one action, each as what it is: text, checked
    /// or not, or options; then Enter in the last one when asked. Every
    /// target is found before anything is set.
    pub(crate) fn fill(&mut self, tab: u32, p: params::Fill, view: View) -> CallResult {
        enum Step {
            Text(String),
            /// Checked or not, and an ARIA checkbox's state now.
            Check(bool, Option<bool>),
            Choose(Vec<catpaw_dom::NodeId>),
        }
        let options = p.options();
        if p.fields.is_empty() {
            return Err(Failure::bad_argument(
                "fill needs fields: [{\"target\":…, \"value\":…}]",
            ));
        }
        let mut plan: Vec<(Aim, Step)> = Vec::new();
        let mut shown = Vec::new();
        let mut secret = false;
        for field in &p.fields {
            let wants = match &field.value {
                params::FillValue::Checked(_) => Use::Check,
                params::FillValue::Options(_) => Use::Choose,
                params::FillValue::Text(_) => Use::Text,
            };
            let aim = self.aim_for(tab, &field.target, wants)?;
            let what = self.aimed(tab, &aim);
            let state = self
                .page
                .frame_state(aim.frame)
                .cloned()
                .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
            let role = self
                .tabs
                .get(&tab)
                .and_then(|t| t.refs.entry(aim.r))
                .map(|e| e.role)
                .unwrap_or("");
            let (is_select, password, aria_checked) = {
                let dom = state.dom.borrow();
                let is_input = dom.is_html_element(aim.node, "input");
                let password = is_input
                    && dom
                        .attr(aim.node, "type")
                        .is_some_and(|t| t.trim().eq_ignore_ascii_case("password"));
                let aria = (!is_input).then(|| {
                    dom.attr(aim.node, "aria-checked")
                        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
                });
                (dom.is_html_element(aim.node, "select"), password, aria)
            };
            let checkable = matches!(
                role,
                "checkbox" | "radio" | "switch" | "menuitemcheckbox" | "menuitemradio"
            );
            let step = if is_select {
                let wanted = match &field.value {
                    params::FillValue::Text(text) => vec![text.clone()],
                    params::FillValue::Options(list) => list.clone(),
                    params::FillValue::Checked(_) => {
                        return Err(Failure::bad_argument(format!(
                            "{what} takes an option's label, not true or false"
                        )));
                    }
                };
                let chosen = choose_options(&state, aim.node, &wanted, &what)?;
                let labels: Vec<String> = chosen.iter().map(|(_, l)| quote(l)).collect();
                shown.push(format!("{what} ← {}", labels.join(", ")));
                Step::Choose(chosen.into_iter().map(|(n, _)| n).collect())
            } else if checkable {
                let want = match &field.value {
                    params::FillValue::Checked(on) => Some(*on),
                    params::FillValue::Text(text) => flag(text),
                    params::FillValue::Options(_) => None,
                }
                .ok_or_else(|| Failure::bad_argument(format!("{what} takes true or false")))?;
                shown.push(format!(
                    "{what} ← {}",
                    if want { "checked" } else { "unchecked" }
                ));
                Step::Check(
                    want,
                    if role == "radio" && !want {
                        None
                    } else {
                        aria_checked
                    },
                )
            } else {
                let params::FillValue::Text(text) = &field.value else {
                    return Err(Failure::bad_argument(format!("{what} takes text")));
                };
                secret |= password;
                let value = if password {
                    "***".to_string()
                } else {
                    quote(&truncate(text, 60))
                };
                shown.push(format!("{what} ← {value}"));
                Step::Text(text.clone())
            };
            self.actionable(tab, &aim, true)?;
            plan.push((aim, step));
        }
        let mut status = format!("ok fill {}", shown.join(", "));
        if p.submit {
            status.push_str(" + Enter");
        }
        for (aim, step) in &plan {
            if matches!(step, Step::Text(_))
                && let Some(state) = self.page.frame_state(aim.frame)
            {
                agent::unmask_value(state, aim.node);
            }
        }
        let base = self.baseline(tab);
        let submit = p.submit;
        let mut failed: Option<(usize, ActionError)> = None;
        self.with_dialogs(&options, |g| {
            for (i, (aim, step)) in plan.iter().enumerate() {
                let node = aim.node;
                let result = g.page.input_in(aim.frame, |cx| match step {
                    Step::Text(text) => input::fill(cx, node, text),
                    Step::Check(want, aria) => match aria {
                        // An ARIA checkbox flips when clicked.
                        Some(now) if now == want => Ok(()),
                        Some(_) => input::click_element(cx, node).map(drop),
                        None => input::set_checked(cx, node, *want),
                    },
                    Step::Choose(nodes) => input::select_options(cx, node, nodes),
                });
                if let Err(e) = result {
                    failed = Some((i, e));
                    return;
                }
            }
            if submit && let Some((aim, _)) = plan.last() {
                let node = aim.node;
                let pressed = g.page.input_in(aim.frame, |cx| {
                    input::focus(cx, node)?;
                    input::press(cx, "Enter")
                });
                if let Err(e) = pressed {
                    failed = Some((plan.len() - 1, e));
                }
            }
        });
        let mut report = self.finish(tab, &base);
        report.acted = plan.last().map(|(aim, _)| aim.r);
        if let Some((i, e)) = failed {
            return Err(self.action_failure(tab, &plan[i].0, e, report));
        }
        let mut output = self.page_result(tab, status, report, options.snapshot, view)?;
        output.secret_input = secret;
        Ok(output)
    }

    pub(crate) fn press(&mut self, tab: u32, p: params::Press, view: View) -> CallResult {
        let options = p.options();
        let key = crate::target::normalize_key(&p.key).map_err(Failure::bad_argument)?;
        let repeat = p.repeat.unwrap_or(1).clamp(1, 50);
        let times = if repeat > 1 {
            format!(" x{repeat}")
        } else {
            String::new()
        };
        match &p.target {
            Some(target) => {
                let aim = self.aim(tab, target)?;
                let status = format!("ok press {key}{times} on {}", self.aimed(tab, &aim));
                self.act_on(tab, aim, status, &options, view, move |cx, aim| {
                    input::focus(cx, aim.node)?;
                    for _ in 0..repeat {
                        input::press(cx, &key)?;
                    }
                    Ok(())
                })
            }
            None => {
                let status = format!("ok press {key}{times}");
                self.act_on_focus(tab, status, &options, view, move |cx| {
                    for _ in 0..repeat {
                        input::press(cx, &key)?;
                    }
                    Ok(())
                })
            }
        }
    }

    pub(crate) fn select(&mut self, tab: u32, p: params::Select, view: View) -> CallResult {
        let options = p.options();
        let aim = self.aim_for(tab, &p.target, Use::Choose)?;
        let state = self
            .page
            .frame_state(aim.frame)
            .cloned()
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let what = self.aimed(tab, &aim);
        if !state.dom.borrow().is_html_element(aim.node, "select") {
            return Err(Failure::new(
                ErrorCode::NotActionable,
                format!("{what} is not a <select>"),
            )
            .with(advice::NOT_A_SELECT));
        }
        let chosen = choose_options(&state, aim.node, &p.option.to_vec(), &what)?;
        self.actionable(tab, &aim, true)?;
        let labels: Vec<String> = chosen.iter().map(|(_, l)| quote(l)).collect();
        let status = format!("ok select {what} ← {}", labels.join(", "));
        let nodes: Vec<catpaw_dom::NodeId> = chosen.iter().map(|(n, _)| *n).collect();
        self.act_on(tab, aim, status, &options, view, move |cx, aim| {
            input::select_options(cx, aim.node, &nodes)
        })
    }

    pub(crate) fn act(&mut self, tab: u32, p: params::Act, view: View) -> CallResult {
        let options = p.options();
        let kind = p.kind;
        if kind == params::ActKind::Upload {
            return self.upload(tab, p, view);
        }
        if kind == params::ActKind::Drag {
            return self.drag(tab, p, view);
        }
        if kind == params::ActKind::Scroll && (p.target.is_none() || p.dy.is_some()) {
            return self.scroll(tab, p, view);
        }
        let target = p
            .target
            .ok_or_else(|| Failure::bad_argument(format!("{} needs a target", kind.as_str())))?;
        let wants = match kind {
            params::ActKind::Check | params::ActKind::Uncheck => Use::Check,
            params::ActKind::Clear => Use::Text,
            _ => Use::Any,
        };
        let aim = self.aim_for(tab, &target, wants)?;
        let what = self.aimed(tab, &aim);
        let mut aria_checked = None;
        if matches!(kind, params::ActKind::Check | params::ActKind::Uncheck) {
            let role = self
                .tabs
                .get(&tab)
                .and_then(|t| t.refs.entry(aim.r))
                .map(|e| e.role)
                .unwrap_or("");
            if !matches!(
                role,
                "checkbox" | "radio" | "switch" | "menuitemcheckbox" | "menuitemradio"
            ) {
                return Err(Failure::new(
                    ErrorCode::NotActionable,
                    format!("{what} is not a checkbox or radio button"),
                )
                .with(advice::NOT_CHECKABLE));
            }
            let state = self
                .page
                .frame_state(aim.frame)
                .cloned()
                .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
            let dom = state.dom.borrow();
            if !dom.is_html_element(aim.node, "input") {
                aria_checked = Some(
                    dom.attr(aim.node, "aria-checked")
                        .is_some_and(|v| v.eq_ignore_ascii_case("true")),
                );
            }
        }
        let enabled = matches!(
            kind,
            params::ActKind::Check | params::ActKind::Uncheck | params::ActKind::Clear
        );
        self.actionable(tab, &aim, enabled)?;
        let status = format!("ok {} {what}", kind.as_str());
        self.act_on(
            tab,
            aim,
            status,
            &options,
            view,
            move |cx, aim| match kind {
                params::ActKind::Hover => input::hover_element(cx, aim.node),
                params::ActKind::Check | params::ActKind::Uncheck => {
                    let want = kind == params::ActKind::Check;
                    match aria_checked {
                        // An ARIA checkbox flips when clicked.
                        Some(now) if now == want => Ok(()),
                        Some(_) => input::click_element(cx, aim.node).map(drop),
                        None => input::set_checked(cx, aim.node, want),
                    }
                }
                params::ActKind::Focus => input::focus(cx, aim.node),
                params::ActKind::Clear => input::fill(cx, aim.node, ""),
                params::ActKind::Scroll => {
                    agent::scroll_into_view(cx, aim.node);
                    Ok(())
                }
                // Uploads and drags went their own ways above.
                params::ActKind::Upload | params::ActKind::Drag => Ok(()),
            },
        )
    }

    /// Scrolls the window, or with a target the content of that element
    /// (or of the nearest element around it that scrolls), by `dy` (90% of
    /// the viewport by default); says how far it really moved.
    fn scroll(&mut self, tab: u32, p: params::Act, view: View) -> CallResult {
        let options = p.options();
        let (_, state) = self.root_state(tab)?;
        let dy =
            p.dy.unwrap_or_else(|| f64::from(agent::viewport(&state).1) * 0.9) as f32;
        let moved = Rc::new(Cell::new(0.0_f32));
        let out = moved.clone();
        let (what, mut result) = match p.target.as_deref() {
            Some(target) => {
                let aim = self.aim(tab, target)?;
                let what = format!(" {}", self.aimed(tab, &aim));
                let result = self.act_on(
                    tab,
                    aim,
                    "ok scroll".to_string(),
                    &options,
                    view,
                    move |cx, aim| {
                        out.set(agent::scroll_within(cx, aim.node, dy));
                        Ok(())
                    },
                )?;
                (what, result)
            }
            None => {
                let result =
                    self.act_on_focus(tab, "ok scroll".to_string(), &options, view, move |cx| {
                        let before = agent::window_scroll(cx.page).1;
                        agent::scroll_by(cx, 0.0, dy);
                        out.set(agent::window_scroll(cx.page).1 - before);
                        Ok(())
                    })?;
                (String::new(), result)
            }
        };
        let moved = moved.get().round();
        let mut status = format!("ok scroll{what} {moved:.0}px");
        if (moved - dy.round()).abs() >= 1.0 {
            let edge = if dy > 0.0 { "the end" } else { "the start" };
            let _ = write!(status, " (asked {:.0}px; at {edge})", dy.round());
        }
        if let Some(rest) = result.text.strip_prefix("ok scroll") {
            result.text = format!("{status}{rest}");
        }
        Ok(result)
    }

    /// Drags an element onto another, both in the same frame.
    fn drag(&mut self, tab: u32, p: params::Act, view: View) -> CallResult {
        let options = p.options();
        let target = p
            .target
            .as_deref()
            .ok_or_else(|| Failure::bad_argument("drag needs target: what to drag"))?;
        let to =
            p.to.as_deref()
                .ok_or_else(|| Failure::bad_argument("drag needs to: where to drop it"))?;
        let aim = self.aim(tab, target)?;
        let onto = self.aim(tab, to)?;
        if onto.frame != aim.frame {
            return Err(Failure::new(
                ErrorCode::Unsupported,
                "dragging from one frame into another",
            ));
        }
        let status = format!(
            "ok drag {} → {}",
            self.aimed(tab, &aim),
            self.aimed(tab, &onto)
        );
        let onto = onto.node;
        self.act_on(tab, aim, status, &options, view, move |cx, aim| {
            input::drag_element(cx, aim.node, onto)
        })
    }

    /// Chooses local files in a file input. The input may be hidden (sites
    /// often hide it behind a styled button): only a disabled one refuses.
    fn upload(&mut self, tab: u32, p: params::Act, view: View) -> CallResult {
        let options = p.options();
        let target = p
            .target
            .as_deref()
            .ok_or_else(|| Failure::bad_argument("upload needs target: the file input"))?;
        let paths = p
            .files
            .filter(|f| !f.is_empty())
            .ok_or_else(|| Failure::bad_argument("upload needs files: paths of local files"))?;
        let names = crate::files::describe(self.files_root.as_deref(), &paths)?;
        let files = crate::files::read(self.files_root.as_deref(), &paths)?;
        let aim = self.aim(tab, target)?;
        let what = self.aimed(tab, &aim);
        let one_only = self
            .page
            .frame_state(aim.frame)
            .is_some_and(|state| state.dom.borrow().attr(aim.node, "multiple").is_none());
        if files.len() > 1 && one_only {
            return Err(Failure::bad_argument(format!(
                "{what} takes one file (it has no multiple attribute)"
            )));
        }
        let status = format!("ok upload {what} ← {names}");
        self.act_on(tab, aim, status, &options, view, move |cx, aim| {
            input::choose_files(cx, aim.node, files)
        })
    }

    pub(crate) fn screenshot(&mut self, tab: u32, p: params::Screenshot) -> CallResult {
        let (root, state) = self.root_state(tab)?;
        let png = self
            .page
            .screenshot_of(root, p.full_page)
            .ok_or_else(|| Failure::new(ErrorCode::NoTab, format!("t{tab} is closed")))?;
        let (w, h) = png_size(&png).unwrap_or(agent::viewport(&state));
        let what = if p.full_page { "full page" } else { "viewport" };
        Ok(ToolOutput {
            text: format!("ok screenshot t{tab} {what} {w}x{h}"),
            image: Some(png),
            ..ToolOutput::default()
        })
    }

    pub(crate) fn evaluate(
        &mut self,
        tab: u32,
        p: params::Evaluate,
        limits: &LoopLimits,
    ) -> CallResult {
        let (root, _) = self.root_state(tab)?;
        let aim = match &p.target {
            Some(target) => Some(self.aim(tab, target)?),
            None => None,
        };
        let frame = aim.map(|a| a.frame).unwrap_or(root);
        // Refs the script names, for `$ref("e12")`.
        let mut table = Vec::new();
        {
            let page = &self.page;
            let entry = self.tabs.get_mut(&tab).expect("root_state found the tab");
            entry.sync(page);
            for word in p
                .script
                .split(|c: char| !c.is_ascii_alphanumeric())
                .filter(|w| w.starts_with('e'))
            {
                if table.iter().any(|(name, _): &(String, Value)| name == word) {
                    continue;
                }
                if let Ok(key) = entry.refs.lookup(word, |key| is_live(page, key))
                    && FrameId(key.frame) == frame
                {
                    table.push((word.to_string(), Value::Node(key.node)));
                }
            }
        }
        let el = aim.map(|a| Value::Node(a.node)).unwrap_or(Value::Undefined);
        let prelude = "const $ref = (r) => __catpaw_refs[String(r).replace(/^\\[?(ref=)?/, \"\").replace(/\\]$/, \"\")] ?? null;\n";
        // `document.title;` is an expression too.
        let expression = p.script.trim().trim_end_matches(';');
        let as_expression = format!(
            "{prelude}const __catpaw_value = (\n{expression}\n);\nreturn typeof __catpaw_value === \"function\" ? __catpaw_value(el) : __catpaw_value;"
        );
        let as_body = format!("{prelude}{}", p.script);
        let base = self.baseline(tab);
        let params = ["el", "__catpaw_refs"];
        let args = vec![el, Value::Record(table)];
        // An expression is run as one (its value is the result), anything
        // else as a function body; which, is settled before anything runs,
        // so that the script runs once.
        let source = if self.page.compiles_in(frame, &params, &as_expression) {
            &as_expression
        } else {
            &as_body
        };
        let result = self.page.call_in(frame, &params, source, args, limits);
        let _ = self.page.follow_navigations();
        let report = self.finish(tab, &base);
        match result {
            Ok(value) => {
                let mut text = format!("ok evaluate{}", report.suffix());
                for line in report.lines.iter().filter(|l| !l.starts_with("  ")) {
                    if line.starts_with(&format!("! {}", consequence::NOT_SETTLED)) {
                        continue;
                    }
                    text.push('\n');
                    text.push_str(line);
                }
                text.push('\n');
                if value.len() > EVAL_BYTES {
                    let cut = floor_char_boundary(&value, EVAL_BYTES);
                    let _ = write!(
                        text,
                        "{}\n[truncated at {cut} of {} bytes]",
                        &value[..cut],
                        value.len()
                    );
                } else {
                    text.push_str(&value);
                }
                Ok(ToolOutput {
                    held: report.held,
                    dropped_holds: report.dropped,
                    ..ToolOutput::ok(text)
                })
            }
            Err(e) => {
                let mut failure = Failure::new(
                    ErrorCode::ScriptError,
                    truncate(e.lines().next().unwrap_or(""), 300),
                );
                for line in report.lines {
                    failure = failure.with(line);
                }
                Err(failure.holding(report.held, report.dropped))
            }
        }
    }
}
