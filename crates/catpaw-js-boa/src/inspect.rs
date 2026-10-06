//! Rendering script values as text, the way a console shows them.

use boa_engine::object::builtins::JsArray;
use boa_engine::{Context, JsObject, JsString, JsValue, js_string};

/// Objects nested deeper than this are abbreviated.
const MAX_DEPTH: usize = 2;
/// Elements or properties shown per object before eliding the rest.
const MAX_ITEMS: usize = 100;
/// Upper bound on the length of one rendered value.
const MAX_LENGTH: usize = 20_000;

/// Describes host objects (DOM nodes and other platform objects) that the
/// generic inspector cannot look into.
pub type DescribeNative<'a> = &'a dyn Fn(&JsObject, &mut Context) -> Option<String>;

struct Inspector<'a> {
    native: DescribeNative<'a>,
    /// Objects currently being rendered, for cycle detection.
    stack: Vec<JsObject>,
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

fn get_string(obj: &JsObject, key: JsString, context: &mut Context) -> Option<String> {
    obj.get(key, context)
        .ok()
        .and_then(|v| v.as_string())
        .map(|s| s.to_std_string_lossy())
}

fn inherits_from(obj: &JsObject, proto: &JsObject) -> bool {
    let mut current = obj.prototype();
    // Bounded, in case a proxy answers with an endless chain.
    for _ in 0..64 {
        match current {
            Some(p) if JsObject::equals(&p, proto) => return true,
            Some(p) => current = p.prototype(),
            None => return false,
        }
    }
    false
}

fn is_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

impl Inspector<'_> {
    fn value(&mut self, value: &JsValue, depth: usize, top: bool, context: &mut Context) -> String {
        if let Some(s) = value.as_string() {
            let s = s.to_std_string_lossy();
            return if top { s } else { quote(&s) };
        }
        let Some(obj) = value.as_object() else {
            return value.display().to_string();
        };
        self.object(&obj, value, depth, context)
    }

    fn object(
        &mut self,
        obj: &JsObject,
        value: &JsValue,
        depth: usize,
        context: &mut Context,
    ) -> String {
        if let Some(text) = (self.native)(obj, context) {
            return text;
        }
        if obj.is_callable() {
            return match get_string(obj, js_string!("name"), context) {
                Some(name) if !name.is_empty() => format!("[Function: {name}]"),
                _ => "[Function (anonymous)]".to_string(),
            };
        }
        let intrinsics = context.intrinsics().constructors();
        let (error_proto, date_proto, regexp_proto, object_proto) = (
            intrinsics.error().prototype(),
            intrinsics.date().prototype(),
            intrinsics.regexp().prototype(),
            intrinsics.object().prototype(),
        );
        if inherits_from(obj, &error_proto) {
            return self.error(obj, context);
        }
        if inherits_from(obj, &date_proto) || inherits_from(obj, &regexp_proto) {
            return value
                .to_string(context)
                .map(|s| s.to_std_string_lossy())
                .unwrap_or_else(|_| "[object]".to_string());
        }
        if self.stack.iter().any(|seen| JsObject::equals(seen, obj)) {
            return "[Circular]".to_string();
        }

        let is_array = obj.is_array();
        if depth > MAX_DEPTH {
            return if is_array { "[Array]" } else { "[Object]" }.to_string();
        }
        self.stack.push(obj.clone());
        let text = if is_array {
            self.array(obj, depth, context)
        } else {
            let plain = obj
                .prototype()
                .is_none_or(|p| JsObject::equals(&p, &object_proto));
            self.properties(obj, plain, depth, context)
        };
        self.stack.pop();
        text
    }

    fn error(&mut self, obj: &JsObject, context: &mut Context) -> String {
        let name = get_string(obj, js_string!("name"), context).unwrap_or_else(|| "Error".into());
        let mut text = match get_string(obj, js_string!("message"), context) {
            Some(message) if !message.is_empty() => format!("{name}: {message}"),
            _ => name,
        };
        // The engine's `stack` holds the frames only, one per line.
        if let Some(stack) = get_string(obj, js_string!("stack"), context) {
            let frames = stack.trim_end();
            if !frames.is_empty() {
                text.push('\n');
                text.push_str(frames);
            }
        }
        text
    }

    fn array(&mut self, obj: &JsObject, depth: usize, context: &mut Context) -> String {
        let Ok(array) = JsArray::from_object(obj.clone()) else {
            return "[Array]".to_string();
        };
        let length = array.length(context).unwrap_or(0) as usize;
        if length == 0 {
            return "[]".to_string();
        }
        let mut parts = Vec::new();
        for i in 0..length.min(MAX_ITEMS) {
            let item = obj.get(i, context).unwrap_or_default();
            parts.push(self.value(&item, depth + 1, false, context));
        }
        if length > MAX_ITEMS {
            parts.push(format!("... {} more items", length - MAX_ITEMS));
        }
        format!("[ {} ]", parts.join(", "))
    }

    fn properties(
        &mut self,
        obj: &JsObject,
        plain: bool,
        depth: usize,
        context: &mut Context,
    ) -> String {
        // `Object.keys` gives exactly the own enumerable string keys.
        let keys_fn = context
            .intrinsics()
            .constructors()
            .object()
            .constructor()
            .get(js_string!("keys"), context)
            .ok()
            .and_then(|f| f.as_callable());
        let keys: Vec<JsValue> = keys_fn
            .and_then(|f| {
                f.call(&JsValue::undefined(), &[obj.clone().into()], context)
                    .ok()
            })
            .and_then(|v| v.as_object())
            .and_then(|o| JsArray::from_object(o).ok())
            .map(|array| {
                let length = array.length(context).unwrap_or(0);
                (0..length)
                    .filter_map(|i| array.at(i as i64, context).ok())
                    .collect()
            })
            .unwrap_or_default();

        let class = if plain {
            None
        } else {
            obj.get(js_string!("constructor"), context)
                .ok()
                .and_then(|c| c.as_object())
                .and_then(|c| get_string(&c, js_string!("name"), context))
                .filter(|name| !name.is_empty() && name != "Object")
        };

        let mut parts = Vec::new();
        for key in keys.iter().take(MAX_ITEMS) {
            let Some(name) = key.as_string() else {
                continue;
            };
            let item = obj.get(name.clone(), context).unwrap_or_default();
            let name = name.to_std_string_lossy();
            let shown = if is_identifier(&name) {
                name
            } else {
                quote(&name)
            };
            parts.push(format!(
                "{shown}: {}",
                self.value(&item, depth + 1, false, context)
            ));
        }
        if keys.len() > MAX_ITEMS {
            parts.push(format!("... {} more properties", keys.len() - MAX_ITEMS));
        }
        let body = if parts.is_empty() {
            "{}".to_string()
        } else {
            format!("{{ {} }}", parts.join(", "))
        };
        match class {
            Some(class) => format!("{class} {body}"),
            None => body,
        }
    }
}

fn truncate(mut text: String) -> String {
    if text.len() > MAX_LENGTH {
        let mut end = MAX_LENGTH;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

/// Renders `values` separated by spaces, like `console.log`.
pub fn display(values: &[JsValue], context: &mut Context, native: DescribeNative<'_>) -> String {
    let mut inspector = Inspector {
        native,
        stack: Vec::new(),
    };
    values
        .iter()
        .map(|v| truncate(inspector.value(v, 0, true, context)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Renders a thrown value for an "Uncaught ..." message.
pub fn describe_thrown(
    value: &JsValue,
    context: &mut Context,
    native: DescribeNative<'_>,
) -> String {
    display(std::slice::from_ref(value), context, native)
}

#[cfg(test)]
mod tests {
    use super::*;
    use boa_engine::Source;

    fn show(source: &str) -> String {
        let mut context = Context::default();
        let value = context.eval(Source::from_bytes(source)).unwrap();
        display(&[value], &mut context, &|_, _| None)
    }

    #[test]
    fn renders_like_a_console() {
        assert_eq!(show("'plain text'"), "plain text");
        assert_eq!(show("42"), "42");
        assert_eq!(show("undefined"), "undefined");
        assert_eq!(show("[1, 'two', [3]]"), "[ 1, 'two', [ 3 ] ]");
        assert_eq!(
            show("({a: 1, 'b-c': {d: null}})"),
            "{ a: 1, 'b-c': { d: null } }"
        );
        assert_eq!(show("({})"), "{}");
        assert_eq!(show("(function named() {})"), "[Function: named]");
        assert!(show("new TypeError('bad')").starts_with("TypeError: bad"));
        assert_eq!(
            show("class P { constructor() { this.x = 1; } }; new P()"),
            "P { x: 1 }"
        );
        assert_eq!(show("var o = {}; o.self = o; o"), "{ self: [Circular] }");
        assert_eq!(
            show("({a: {b: {c: {d: 1}}}})"),
            "{ a: { b: { c: [Object] } } }"
        );
    }
}
