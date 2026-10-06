//! The `console` namespace (<https://console.spec.whatwg.org/>).

use catpaw_js::{Fallible, Value};

use crate::Web;
use crate::generated as web;
use crate::page::{ConsoleLevel, Cx};

/// Applies the console's printf-style formatting when the first argument is
/// a format string, then joins what is left with spaces.
fn format(cx: &mut Cx<'_>, data: &[Value]) -> String {
    let Some(Value::String(template)) = data.first() else {
        return cx.script.display(data);
    };
    if !template.contains('%') || data.len() == 1 {
        return cx.script.display(data);
    }

    let mut out = String::new();
    let mut next = 1;
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(&specifier) = chars.peek() else {
            out.push('%');
            break;
        };
        if specifier == '%' {
            chars.next();
            out.push('%');
            continue;
        }
        if !matches!(specifier, 's' | 'd' | 'i' | 'f' | 'o' | 'O' | 'c') || next >= data.len() {
            out.push('%');
            continue;
        }
        chars.next();
        let arg = &data[next];
        next += 1;
        match specifier {
            // Styling has no effect on plain text.
            'c' => {}
            'd' | 'i' => match arg {
                Value::Number(n) if n.is_finite() => out.push_str(&format!("{}", n.trunc())),
                Value::Number(_) => out.push_str("NaN"),
                other => {
                    let text = cx.script.display(std::slice::from_ref(other));
                    match text.trim().parse::<f64>() {
                        Ok(n) if n.is_finite() => out.push_str(&format!("{}", n.trunc())),
                        _ => out.push_str("NaN"),
                    }
                }
            },
            _ => out.push_str(&cx.script.display(std::slice::from_ref(arg))),
        }
    }
    if next < data.len() {
        out.push(' ');
        out.push_str(&cx.script.display(&data[next..]));
    }
    out
}

fn log(cx: &mut Cx<'_>, level: ConsoleLevel, data: &[Value]) {
    let text = format(cx, data);
    emit(cx, level, text);
}

fn emit(cx: &mut Cx<'_>, level: ConsoleLevel, text: String) {
    let depth = cx.page.console_state.borrow().group_depth;
    let text = if depth == 0 {
        text
    } else {
        format!("{}{}", "  ".repeat(depth), text)
    };
    cx.page.log(level, text);
}

fn label_or_default(label: String) -> String {
    if label.is_empty() {
        "default".to_string()
    } else {
        label
    }
}

impl web::consoleImpl for Web {
    fn assert(cx: &mut Cx<'_>, condition: bool, data: Vec<Value>) -> Fallible<()> {
        if !condition {
            let detail = format(cx, &data);
            let text = if detail.is_empty() {
                "Assertion failed".to_string()
            } else {
                format!("Assertion failed: {detail}")
            };
            emit(cx, ConsoleLevel::Error, text);
        }
        Ok(())
    }

    fn clear(_cx: &mut Cx<'_>) -> Fallible<()> {
        Ok(())
    }

    fn debug(cx: &mut Cx<'_>, data: Vec<Value>) -> Fallible<()> {
        log(cx, ConsoleLevel::Debug, &data);
        Ok(())
    }

    fn error(cx: &mut Cx<'_>, data: Vec<Value>) -> Fallible<()> {
        log(cx, ConsoleLevel::Error, &data);
        Ok(())
    }

    fn info(cx: &mut Cx<'_>, data: Vec<Value>) -> Fallible<()> {
        log(cx, ConsoleLevel::Info, &data);
        Ok(())
    }

    fn log(cx: &mut Cx<'_>, data: Vec<Value>) -> Fallible<()> {
        log(cx, ConsoleLevel::Log, &data);
        Ok(())
    }

    fn table(
        cx: &mut Cx<'_>,
        tabular_data: Value,
        _properties: Option<Vec<String>>,
    ) -> Fallible<()> {
        log(cx, ConsoleLevel::Log, &[tabular_data]);
        Ok(())
    }

    fn trace(cx: &mut Cx<'_>, data: Vec<Value>) -> Fallible<()> {
        let detail = format(cx, &data);
        emit(cx, ConsoleLevel::Debug, format!("Trace: {detail}"));
        Ok(())
    }

    fn warn(cx: &mut Cx<'_>, data: Vec<Value>) -> Fallible<()> {
        log(cx, ConsoleLevel::Warn, &data);
        Ok(())
    }

    fn dir(cx: &mut Cx<'_>, item: Value, _options: Value) -> Fallible<()> {
        log(cx, ConsoleLevel::Log, &[item]);
        Ok(())
    }

    fn dirxml(cx: &mut Cx<'_>, data: Vec<Value>) -> Fallible<()> {
        log(cx, ConsoleLevel::Log, &data);
        Ok(())
    }

    fn count(cx: &mut Cx<'_>, label: String) -> Fallible<()> {
        let label = label_or_default(label);
        let count = {
            let mut state = cx.page.console_state.borrow_mut();
            let count = state.counts.entry(label.clone()).or_insert(0);
            *count += 1;
            *count
        };
        emit(cx, ConsoleLevel::Info, format!("{label}: {count}"));
        Ok(())
    }

    fn count_reset(cx: &mut Cx<'_>, label: String) -> Fallible<()> {
        let label = label_or_default(label);
        cx.page.console_state.borrow_mut().counts.remove(&label);
        Ok(())
    }

    fn group(cx: &mut Cx<'_>, data: Vec<Value>) -> Fallible<()> {
        if !data.is_empty() {
            log(cx, ConsoleLevel::Log, &data);
        }
        cx.page.console_state.borrow_mut().group_depth += 1;
        Ok(())
    }

    fn group_collapsed(cx: &mut Cx<'_>, data: Vec<Value>) -> Fallible<()> {
        <Web as web::consoleImpl>::group(cx, data)
    }

    fn group_end(cx: &mut Cx<'_>) -> Fallible<()> {
        let mut state = cx.page.console_state.borrow_mut();
        state.group_depth = state.group_depth.saturating_sub(1);
        Ok(())
    }

    fn time(cx: &mut Cx<'_>, label: String) -> Fallible<()> {
        let now = cx.page.clock.now();
        cx.page
            .console_state
            .borrow_mut()
            .timers
            .entry(label_or_default(label))
            .or_insert(now);
        Ok(())
    }

    fn time_log(cx: &mut Cx<'_>, label: String, data: Vec<Value>) -> Fallible<()> {
        let label = label_or_default(label);
        let started = cx.page.console_state.borrow().timers.get(&label).copied();
        if let Some(started) = started {
            let elapsed = cx.page.clock.now() - started;
            let extra = format(cx, &data);
            let text = if extra.is_empty() {
                format!("{label}: {elapsed:.3}ms")
            } else {
                format!("{label}: {elapsed:.3}ms {extra}")
            };
            emit(cx, ConsoleLevel::Log, text);
        }
        Ok(())
    }

    fn time_end(cx: &mut Cx<'_>, label: String) -> Fallible<()> {
        let label = label_or_default(label);
        let started = cx.page.console_state.borrow_mut().timers.remove(&label);
        if let Some(started) = started {
            let elapsed = cx.page.clock.now() - started;
            emit(cx, ConsoleLevel::Info, format!("{label}: {elapsed:.3}ms"));
        }
        Ok(())
    }
}
