//! What form controls show in screenshots. A control's current value,
//! checkedness and chosen options live in the page, not in the tree, so
//! the painter asks here.

use catpaw_dom::{Dom, NodeId};
use catpaw_paint::{ControlFace, Gauge};

use crate::forms;
use crate::page::PageState;

/// What the control `node` shows inside its box, if it is a control.
pub(crate) fn control_face(page: &PageState, dom: &Dom, node: NodeId) -> Option<ControlFace> {
    let el = dom.element(node)?;
    if !el.is_html() {
        return None;
    }
    match &*el.name.local {
        "input" => input_face(page, dom, node),
        "textarea" => {
            let dirty = page
                .form_state
                .borrow()
                .get(&node)
                .and_then(|s| s.value.clone());
            let value = dirty.unwrap_or_else(|| crate::element::child_text_content(dom, node));
            Some(with_placeholder(dom, node, value, |text, placeholder| {
                ControlFace::Area { text, placeholder }
            }))
        }
        "select" => Some(select_face(page, dom, node)),
        "progress" => {
            let max = number_attr(dom, node, "max")
                .filter(|m| *m > 0.0)
                .unwrap_or(1.0);
            let fraction = number_attr(dom, node, "value").map(|v| (v / max).clamp(0.0, 1.0));
            Some(ControlFace::Gauge {
                kind: Gauge::Progress,
                fraction: fraction.map(|f| f as f32),
            })
        }
        "meter" => {
            let min = number_attr(dom, node, "min").unwrap_or(0.0);
            let max = number_attr(dom, node, "max")
                .filter(|m| *m > min)
                .unwrap_or(min + 1.0);
            let value = number_attr(dom, node, "value").unwrap_or(min);
            Some(ControlFace::Gauge {
                kind: Gauge::Meter,
                fraction: Some(((value - min) / (max - min)).clamp(0.0, 1.0) as f32),
            })
        }
        _ => None,
    }
}

fn number_attr(dom: &Dom, node: NodeId, name: &str) -> Option<f64> {
    dom.attr(node, name)?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

/// A field's face: its text, or its placeholder when the text is empty.
fn with_placeholder(
    dom: &Dom,
    node: NodeId,
    text: String,
    face: impl FnOnce(String, bool) -> ControlFace,
) -> ControlFace {
    if text.is_empty()
        && let Some(placeholder) = dom.attr(node, "placeholder")
    {
        let placeholder: String = placeholder
            .chars()
            .filter(|c| !matches!(c, '\n' | '\r'))
            .collect();
        if !placeholder.is_empty() {
            return face(placeholder, true);
        }
    }
    face(text, false)
}

fn input_face(page: &PageState, dom: &Dom, node: NodeId) -> Option<ControlFace> {
    let kind = dom
        .attr(node, "type")
        .map(|t| t.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let state = page.form_state.borrow().get(&node).cloned();
    let value = || {
        state
            .as_ref()
            .and_then(|s| s.value.clone())
            .or_else(|| dom.attr(node, "value").map(str::to_string))
            .unwrap_or_default()
    };
    Some(match kind.as_str() {
        "hidden" | "image" => return None,
        "checkbox" | "radio" => ControlFace::Check {
            radio: kind == "radio",
            checked: state
                .as_ref()
                .and_then(|s| s.checked)
                .unwrap_or_else(|| dom.attr(node, "checked").is_some()),
        },
        "submit" | "reset" | "button" => ControlFace::Button {
            label: dom
                .attr(node, "value")
                .map(str::to_string)
                .unwrap_or_else(|| match kind.as_str() {
                    "submit" => "Submit".to_string(),
                    "reset" => "Reset".to_string(),
                    _ => String::new(),
                }),
        },
        "file" => {
            let names = crate::file_api::chosen_file_names(page, node);
            ControlFace::File {
                label: match names.as_slice() {
                    [] => "No file chosen".to_string(),
                    [one] => one.clone(),
                    many => format!("{} files", many.len()),
                },
            }
        }
        "range" => {
            let min = number_attr(dom, node, "min").unwrap_or(0.0);
            let max = number_attr(dom, node, "max")
                .filter(|m| *m > min)
                .unwrap_or(min + 100.0);
            let value = value()
                .trim()
                .parse::<f64>()
                .ok()
                .unwrap_or(min + (max - min) / 2.0);
            ControlFace::Gauge {
                kind: Gauge::Range,
                fraction: Some(((value - min) / (max - min)).clamp(0.0, 1.0) as f32),
            }
        }
        "color" => ControlFace::Color {
            rgb: parse_hex_color(&value()).unwrap_or([0, 0, 0]),
        },
        _ => {
            // One line: line breaks are not part of a field's value.
            let text: String = value()
                .chars()
                .filter(|c| !matches!(c, '\n' | '\r'))
                .collect();
            let text = if kind == "password" {
                "\u{2022}".repeat(text.chars().count())
            } else {
                text
            };
            with_placeholder(dom, node, text, |text, placeholder| ControlFace::Field {
                text,
                placeholder,
            })
        }
    })
}

/// `#rrggbb`, as a colour input's value always is once sanitized.
fn parse_hex_color(value: &str) -> Option<[u8; 3]> {
    let hex = value.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

fn select_face(page: &PageState, dom: &Dom, select: NodeId) -> ControlFace {
    let label = |option: NodeId| match dom.attr(option, "label") {
        Some(label) if !label.is_empty() => label.to_string(),
        _ => forms::option_text(dom, option),
    };
    let rows = dom
        .attr(select, "size")
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    if dom.attr(select, "multiple").is_some() || rows > 1 {
        let options = forms::options_of(dom, select)
            .into_iter()
            .map(|o| (label(o), forms::option_selected(page, o)))
            .collect();
        return ControlFace::ListBox { options };
    }
    let shown = forms::displayed_options(page, select).first().copied();
    ControlFace::DropDown {
        label: shown.map(label).unwrap_or_default(),
    }
}
