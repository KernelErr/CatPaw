//! `document.evaluate()` and friends: the DOM face of `catpaw_dom::xpath`.
//!
//! Namespace prefixes are resolved when an expression is created, through
//! the resolver given (a node answers with `lookupNamespaceURI`), so that
//! evaluation itself never calls back into script. An iterator result goes
//! stale when the document changes, as in browsers.

use std::collections::HashMap;
use std::rc::Rc;

use catpaw_dom::NodeId;
use catpaw_dom::xpath::{self, Expression, XNode};
use catpaw_js::{Callback, Exception, Fallible, ObjectId, Value};

use crate::generated::{self as web};
use crate::page::Cx;
use crate::{Web, attributes, platform_object};

pub struct EvaluatorObject;
platform_object!(EvaluatorObject, XPathEvaluator);

pub struct ExpressionObject {
    expression: Rc<Expression>,
    namespaces: HashMap<String, String>,
}
platform_object!(ExpressionObject, XPathExpression);

const ANY_TYPE: u16 = 0;
const NUMBER_TYPE: u16 = 1;
const STRING_TYPE: u16 = 2;
const BOOLEAN_TYPE: u16 = 3;
const UNORDERED_NODE_ITERATOR_TYPE: u16 = 4;
const ORDERED_NODE_ITERATOR_TYPE: u16 = 5;
const UNORDERED_NODE_SNAPSHOT_TYPE: u16 = 6;
const ORDERED_NODE_SNAPSHOT_TYPE: u16 = 7;
const ANY_UNORDERED_NODE_TYPE: u16 = 8;
const FIRST_ORDERED_NODE_TYPE: u16 = 9;

pub struct ResultObject {
    result_type: u16,
    value: xpath::Value,
    /// For iterators: the next node to hand out.
    position: usize,
    /// The document version the iterator was made at.
    version: u64,
}
platform_object!(ResultObject, XPathResult);

fn syntax_error(e: &xpath::Error) -> Exception {
    Exception::syntax(format!("The string is not a valid XPath expression: {e}"))
}

/// Resolves every prefix the expression uses through `resolver`.
fn resolve_prefixes(
    cx: &mut Cx<'_>,
    expression: &Expression,
    resolver: Option<&Callback>,
) -> Fallible<HashMap<String, String>> {
    let mut namespaces = HashMap::new();
    for prefix in expression.prefixes() {
        let resolved = match resolver {
            Some(callback) => cx.script.call(
                callback,
                &Value::Undefined,
                &[Value::String(prefix.clone())],
            )?,
            None => Value::Null,
        };
        match resolved {
            Value::String(uri) if !uri.is_empty() => {
                namespaces.insert(prefix.clone(), uri);
            }
            _ => {
                return Err(Exception::namespace(format!(
                    "The namespace prefix `{prefix}` could not be resolved"
                )));
            }
        }
    }
    Ok(namespaces)
}

fn create_expression(
    cx: &mut Cx<'_>,
    source: &str,
    resolver: Option<&Callback>,
) -> Fallible<ObjectId> {
    let expression = xpath::compile(source).map_err(|e| syntax_error(&e))?;
    let namespaces = resolve_prefixes(cx, &expression, resolver)?;
    Ok(cx.page.alloc(ExpressionObject {
        expression: Rc::new(expression),
        namespaces,
    }))
}

fn evaluate_expression(
    cx: &mut Cx<'_>,
    expression: &Expression,
    namespaces: &HashMap<String, String>,
    context: NodeId,
    type_: u16,
) -> Fallible<ObjectId> {
    if type_ > FIRST_ORDERED_NODE_TYPE {
        return Err(Exception::not_supported(format!(
            "{type_} is not a valid XPath result type"
        )));
    }
    let (value, version) = {
        let dom = cx.dom();
        let value = xpath::evaluate(&dom, expression, context, namespaces).map_err(|e| {
            if e.unresolved_prefix.is_some() {
                Exception::namespace(e.message)
            } else {
                Exception::syntax(e.message)
            }
        })?;
        (value, dom.version())
    };
    let result_type = match type_ {
        ANY_TYPE => match value {
            xpath::Value::Nodes(_) => UNORDERED_NODE_ITERATOR_TYPE,
            xpath::Value::Number(_) => NUMBER_TYPE,
            xpath::Value::String(_) => STRING_TYPE,
            xpath::Value::Bool(_) => BOOLEAN_TYPE,
        },
        other => other,
    };
    let value = {
        let dom = cx.dom();
        match result_type {
            NUMBER_TYPE => xpath::Value::Number(xpath::to_number(&dom, &value)),
            STRING_TYPE => xpath::Value::String(xpath::to_string(&dom, &value)),
            BOOLEAN_TYPE => xpath::Value::Bool(xpath::to_boolean(&value)),
            _ => match value {
                nodes @ xpath::Value::Nodes(_) => nodes,
                _ => {
                    return Err(Exception::type_error(
                        "The expression cannot be converted to return the specified type.",
                    ));
                }
            },
        }
    };
    Ok(cx.page.alloc(ResultObject {
        result_type,
        value,
        position: 0,
        version,
    }))
}

fn node_value(cx: &Cx<'_>, node: XNode) -> Value {
    match node {
        XNode::Node(id) => Value::Node(id),
        XNode::Attr(element, index) => {
            let dom = cx.dom();
            match dom.element(element).and_then(|el| el.attrs.get(index)) {
                Some(attr) => Value::Object(attributes::object_for(cx.page, &dom, element, attr)),
                None => Value::Null,
            }
        }
    }
}

fn result<R>(cx: &Cx<'_>, this: ObjectId, f: impl FnOnce(&mut ResultObject) -> R) -> Fallible<R> {
    cx.page.with::<ResultObject, _>(this, f)
}

fn require_type(cx: &Cx<'_>, this: ObjectId, wanted: &[u16], what: &str) -> Fallible<()> {
    let actual = result(cx, this, |r| r.result_type)?;
    if wanted.contains(&actual) {
        Ok(())
    } else {
        Err(Exception::type_error(format!(
            "The result type is not compatible with the requested {what}."
        )))
    }
}

impl web::XPathEvaluatorImpl for Web {
    fn constructor(cx: &mut Cx<'_>) -> Fallible<ObjectId> {
        Ok(cx.page.alloc(EvaluatorObject))
    }
}

impl web::XPathEvaluatorBaseForObjectImpl for Web {
    fn create_expression(
        cx: &mut Cx<'_>,
        _this: ObjectId,
        expression: String,
        resolver: Option<Callback>,
    ) -> Fallible<ObjectId> {
        create_expression(cx, &expression, resolver.as_ref())
    }

    fn create_ns_resolver(
        _cx: &mut Cx<'_>,
        _this: ObjectId,
        node_resolver: NodeId,
    ) -> Fallible<NodeId> {
        Ok(node_resolver)
    }

    fn evaluate(
        cx: &mut Cx<'_>,
        _this: ObjectId,
        expression: String,
        context_node: NodeId,
        resolver: Option<Callback>,
        type_: u16,
        _result: Option<ObjectId>,
    ) -> Fallible<ObjectId> {
        let compiled = xpath::compile(&expression).map_err(|e| syntax_error(&e))?;
        let namespaces = resolve_prefixes(cx, &compiled, resolver.as_ref())?;
        evaluate_expression(cx, &compiled, &namespaces, context_node, type_)
    }
}

impl web::XPathEvaluatorBaseForNodeImpl for Web {
    fn create_expression(
        cx: &mut Cx<'_>,
        _this: NodeId,
        expression: String,
        resolver: Option<Callback>,
    ) -> Fallible<ObjectId> {
        create_expression(cx, &expression, resolver.as_ref())
    }

    fn create_ns_resolver(
        _cx: &mut Cx<'_>,
        _this: NodeId,
        node_resolver: NodeId,
    ) -> Fallible<NodeId> {
        Ok(node_resolver)
    }

    fn evaluate(
        cx: &mut Cx<'_>,
        _this: NodeId,
        expression: String,
        context_node: NodeId,
        resolver: Option<Callback>,
        type_: u16,
        _result: Option<ObjectId>,
    ) -> Fallible<ObjectId> {
        let compiled = xpath::compile(&expression).map_err(|e| syntax_error(&e))?;
        let namespaces = resolve_prefixes(cx, &compiled, resolver.as_ref())?;
        evaluate_expression(cx, &compiled, &namespaces, context_node, type_)
    }
}

impl web::XPathExpressionImpl for Web {
    fn evaluate(
        cx: &mut Cx<'_>,
        this: ObjectId,
        context_node: NodeId,
        type_: u16,
        _result: Option<ObjectId>,
    ) -> Fallible<ObjectId> {
        let (expression, namespaces) = cx
            .page
            .with::<ExpressionObject, _>(this, |e| (e.expression.clone(), e.namespaces.clone()))?;
        evaluate_expression(cx, &expression, &namespaces, context_node, type_)
    }
}

impl web::XPathResultImpl for Web {
    fn result_type(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u16> {
        result(cx, this, |r| r.result_type)
    }

    fn number_value(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<f64> {
        require_type(cx, this, &[NUMBER_TYPE], "number")?;
        result(cx, this, |r| match &r.value {
            xpath::Value::Number(n) => *n,
            _ => f64::NAN,
        })
    }

    fn string_value(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<String> {
        require_type(cx, this, &[STRING_TYPE], "string")?;
        result(cx, this, |r| match &r.value {
            xpath::Value::String(s) => s.clone(),
            _ => String::new(),
        })
    }

    fn boolean_value(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        require_type(cx, this, &[BOOLEAN_TYPE], "boolean")?;
        result(cx, this, |r| matches!(r.value, xpath::Value::Bool(true)))
    }

    fn single_node_value(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        require_type(
            cx,
            this,
            &[ANY_UNORDERED_NODE_TYPE, FIRST_ORDERED_NODE_TYPE],
            "single node",
        )?;
        let first = result(cx, this, |r| match &r.value {
            xpath::Value::Nodes(nodes) => nodes.first().copied(),
            _ => None,
        })?;
        Ok(first.map_or(Value::Null, |n| node_value(cx, n)))
    }

    fn invalid_iterator_state(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<bool> {
        let version = cx.dom().version();
        result(cx, this, |r| {
            matches!(
                r.result_type,
                UNORDERED_NODE_ITERATOR_TYPE | ORDERED_NODE_ITERATOR_TYPE
            ) && r.version != version
        })
    }

    fn snapshot_length(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<u32> {
        require_type(
            cx,
            this,
            &[UNORDERED_NODE_SNAPSHOT_TYPE, ORDERED_NODE_SNAPSHOT_TYPE],
            "snapshot",
        )?;
        result(cx, this, |r| match &r.value {
            xpath::Value::Nodes(nodes) => nodes.len() as u32,
            _ => 0,
        })
    }

    fn iterate_next(cx: &mut Cx<'_>, this: ObjectId) -> Fallible<Value> {
        require_type(
            cx,
            this,
            &[UNORDERED_NODE_ITERATOR_TYPE, ORDERED_NODE_ITERATOR_TYPE],
            "iterator",
        )?;
        if Self::invalid_iterator_state(cx, this)? {
            return Err(Exception::invalid_state(
                "The document has mutated since the result was returned.",
            ));
        }
        let next = result(cx, this, |r| match &r.value {
            xpath::Value::Nodes(nodes) => {
                let node = nodes.get(r.position).copied();
                if node.is_some() {
                    r.position += 1;
                }
                node
            }
            _ => None,
        })?;
        Ok(next.map_or(Value::Null, |n| node_value(cx, n)))
    }

    fn snapshot_item(cx: &mut Cx<'_>, this: ObjectId, index: u32) -> Fallible<Value> {
        require_type(
            cx,
            this,
            &[UNORDERED_NODE_SNAPSHOT_TYPE, ORDERED_NODE_SNAPSHOT_TYPE],
            "snapshot",
        )?;
        let node = result(cx, this, |r| match &r.value {
            xpath::Value::Nodes(nodes) => nodes.get(index as usize).copied(),
            _ => None,
        })?;
        Ok(node.map_or(Value::Null, |n| node_value(cx, n)))
    }
}
