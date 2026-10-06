//! XPath 1.0 over the arena: the language behind `document.evaluate()`.
//!
//! An expression is compiled once ([`compile`]) and evaluated against a
//! context node ([`evaluate`]). Node-sets may hold attributes, which have
//! no node of their own in the arena, so results are [`XNode`]s.
//!
//! Name tests follow what browsers do for HTML: an unprefixed test matches
//! HTML elements in an HTML document whatever their namespace, comparing
//! names without regard to ASCII case. The namespace axis is empty, and
//! variables are not available (the DOM offers no way to bind them).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fmt;

use html5ever::ns;

use crate::arena::{Dom, NodeId, NodeKind};

// ---------------------------------------------------------------- values

/// A member of a node-set: a node, or an attribute of an element.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum XNode {
    Node(NodeId),
    /// The attribute at this index of the element's attribute list.
    Attr(NodeId, usize),
}

/// The result of an expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// In document order, without duplicates.
    Nodes(Vec<XNode>),
    Bool(bool),
    Number(f64),
    String(String),
}

/// What went wrong compiling or evaluating an expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub message: String,
    /// The prefix in a name test that the resolver could not resolve.
    pub unresolved_prefix: Option<String>,
}

impl Error {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            unresolved_prefix: None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

// ------------------------------------------------------------------- AST

/// A compiled expression.
#[derive(Clone, Debug)]
pub struct Expression {
    expr: Expr,
    /// Every namespace prefix the expression's name tests use.
    prefixes: Vec<String>,
}

impl Expression {
    /// The namespace prefixes that need resolving before evaluation.
    pub fn prefixes(&self) -> &[String] {
        &self.prefixes
    }
}

#[derive(Clone, Debug)]
enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Compare(Comparison, Box<Expr>, Box<Expr>),
    Arith(Arith, Box<Expr>, Box<Expr>),
    Neg(Box<Expr>),
    Union(Box<Expr>, Box<Expr>),
    Literal(String),
    Number(f64),
    Variable(String),
    Call(String, Vec<Expr>),
    Path(PathStart, Vec<Step>),
}

#[derive(Clone, Debug)]
enum PathStart {
    /// `/...`: the root of the context node's tree.
    Root,
    /// A relative path: the context node.
    Context,
    /// `expr[pred]/...`: a filtered primary expression.
    Filter(Box<Expr>, Vec<Expr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Comparison {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug)]
enum Arith {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Debug)]
struct Step {
    axis: Axis,
    test: NodeTest,
    predicates: Vec<Expr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis {
    Ancestor,
    AncestorOrSelf,
    Attribute,
    Child,
    Descendant,
    DescendantOrSelf,
    Following,
    FollowingSibling,
    Namespace,
    Parent,
    Preceding,
    PrecedingSibling,
    /// `self::`
    Own,
}

impl Axis {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "ancestor" => Axis::Ancestor,
            "ancestor-or-self" => Axis::AncestorOrSelf,
            "attribute" => Axis::Attribute,
            "child" => Axis::Child,
            "descendant" => Axis::Descendant,
            "descendant-or-self" => Axis::DescendantOrSelf,
            "following" => Axis::Following,
            "following-sibling" => Axis::FollowingSibling,
            "namespace" => Axis::Namespace,
            "parent" => Axis::Parent,
            "preceding" => Axis::Preceding,
            "preceding-sibling" => Axis::PrecedingSibling,
            "self" => Axis::Own,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
enum NodeTest {
    /// `*`
    Any,
    /// `prefix:*`
    Prefixed(String),
    /// `name` or `prefix:name`
    Name(Option<String>, String),
    /// `node()`
    Node,
    /// `text()`
    Text,
    /// `comment()`
    Comment,
    /// `processing-instruction()` or `processing-instruction("target")`
    Pi(Option<String>),
}

// ----------------------------------------------------------------- lexer

#[derive(Clone, Debug, PartialEq)]
enum Token {
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    DotDot,
    At,
    Comma,
    ColonColon,
    /// `*`, `prefix:*`, `name`, `prefix:name`
    NameTest(Option<String>, Option<String>),
    NodeType(String),
    Op(String),
    Function(String),
    Axis(String),
    Literal(String),
    Number(f64),
    Variable(String),
}

fn is_name_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '\u{B7}')
}

fn tokenize(source: &str) -> Result<Vec<Token>, Error> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // Whether `*` and the operator names are operators here (XPath 1.0 §3.7).
        let operator_position = matches!(
            tokens.last(),
            Some(t) if !matches!(
                t,
                Token::At | Token::ColonColon | Token::LParen | Token::LBracket | Token::Comma | Token::Op(_)
            )
        );
        let two = || chars.get(i + 1).copied();
        let token = match c {
            '(' => Token::LParen,
            ')' => Token::RParen,
            '[' => Token::LBracket,
            ']' => Token::RBracket,
            '@' => Token::At,
            ',' => Token::Comma,
            '|' | '+' | '-' | '=' => Token::Op(c.to_string()),
            '/' if two() == Some('/') => {
                i += 1;
                Token::Op("//".to_string())
            }
            '/' => Token::Op("/".to_string()),
            '!' if two() == Some('=') => {
                i += 1;
                Token::Op("!=".to_string())
            }
            '<' | '>' if two() == Some('=') => {
                i += 1;
                Token::Op(format!("{c}="))
            }
            '<' | '>' => Token::Op(c.to_string()),
            ':' if two() == Some(':') => {
                i += 1;
                Token::ColonColon
            }
            '.' if two() == Some('.') => {
                i += 1;
                Token::DotDot
            }
            '.' if two().is_some_and(|d| d.is_ascii_digit()) => {
                let start = i;
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                tokens.push(Token::Number(text.parse().unwrap_or(f64::NAN)));
                continue;
            }
            '.' => Token::Dot,
            '*' if operator_position => Token::Op("*".to_string()),
            '*' => Token::NameTest(None, None),
            '"' | '\'' => {
                let start = i + 1;
                let mut j = start;
                while j < chars.len() && chars[j] != c {
                    j += 1;
                }
                if j >= chars.len() {
                    return Err(Error::new("unterminated string literal"));
                }
                i = j + 1;
                tokens.push(Token::Literal(chars[start..j].iter().collect()));
                continue;
            }
            '$' => {
                let start = i + 1;
                let mut j = start;
                while j < chars.len() && (is_name_char(chars[j]) || chars[j] == ':') {
                    j += 1;
                }
                if j == start {
                    return Err(Error::new("expected a variable name after `$`"));
                }
                i = j;
                tokens.push(Token::Variable(chars[start..j].iter().collect()));
                continue;
            }
            d if d.is_ascii_digit() => {
                let start = i;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
                if i < chars.len() && chars[i] == '.' {
                    i += 1;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let text: String = chars[start..i].iter().collect();
                tokens.push(Token::Number(text.parse().unwrap_or(f64::NAN)));
                continue;
            }
            c if is_name_start(c) => {
                let start = i;
                let mut j = i;
                while j < chars.len() && is_name_char(chars[j]) {
                    j += 1;
                }
                let first: String = chars[start..j].iter().collect();
                i = j;
                if operator_position {
                    if matches!(first.as_str(), "and" | "or" | "mod" | "div") {
                        tokens.push(Token::Op(first));
                        continue;
                    }
                    return Err(Error::new(format!("unexpected name `{first}`")));
                }
                // A prefix, or a qualified name.
                let mut prefix = None;
                let mut local = first;
                if i < chars.len() && chars[i] == ':' && chars.get(i + 1) != Some(&':') {
                    match chars.get(i + 1) {
                        Some('*') => {
                            i += 2;
                            tokens.push(Token::NameTest(Some(local), None));
                            continue;
                        }
                        Some(&n) if is_name_start(n) => {
                            let start = i + 1;
                            let mut j = start;
                            while j < chars.len() && is_name_char(chars[j]) {
                                j += 1;
                            }
                            prefix = Some(local);
                            local = chars[start..j].iter().collect();
                            i = j;
                        }
                        _ => return Err(Error::new("expected a name after `:`")),
                    }
                }
                // Look past whitespace to classify the name.
                let mut k = i;
                while k < chars.len() && chars[k].is_whitespace() {
                    k += 1;
                }
                let next = chars.get(k).copied();
                let next2 = chars.get(k + 1).copied();
                if prefix.is_none() && next == Some(':') && next2 == Some(':') {
                    tokens.push(Token::Axis(local));
                } else if next == Some('(') {
                    if prefix.is_none()
                        && matches!(
                            local.as_str(),
                            "comment" | "text" | "processing-instruction" | "node"
                        )
                    {
                        tokens.push(Token::NodeType(local));
                    } else {
                        tokens.push(Token::Function(match prefix {
                            Some(p) => format!("{p}:{local}"),
                            None => local,
                        }));
                    }
                } else {
                    tokens.push(Token::NameTest(prefix, Some(local)));
                }
                continue;
            }
            other => return Err(Error::new(format!("unexpected character `{other}`"))),
        };
        tokens.push(token);
        i += 1;
    }
    Ok(tokens)
}

// ---------------------------------------------------------------- parser

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    prefixes: Vec<String>,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Some(Token::Op(o)) if o == op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: Token, what: &str) -> Result<(), Error> {
        if self.peek() == Some(&token) {
            self.pos += 1;
            Ok(())
        } else {
            Err(Error::new(format!("expected {what}")))
        }
    }

    fn note_prefix(&mut self, prefix: &Option<String>) {
        if let Some(p) = prefix
            && !self.prefixes.contains(p)
        {
            self.prefixes.push(p.clone());
        }
    }

    fn expr(&mut self) -> Result<Expr, Error> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Expr, Error> {
        let mut left = self.and_expr()?;
        while self.eat_op("or") {
            let right = self.and_expr()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr, Error> {
        let mut left = self.equality_expr()?;
        while self.eat_op("and") {
            let right = self.equality_expr()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn equality_expr(&mut self) -> Result<Expr, Error> {
        let mut left = self.relational_expr()?;
        loop {
            let op = if self.eat_op("=") {
                Comparison::Eq
            } else if self.eat_op("!=") {
                Comparison::Ne
            } else {
                return Ok(left);
            };
            let right = self.relational_expr()?;
            left = Expr::Compare(op, Box::new(left), Box::new(right));
        }
    }

    fn relational_expr(&mut self) -> Result<Expr, Error> {
        let mut left = self.additive_expr()?;
        loop {
            let op = if self.eat_op("<=") {
                Comparison::Le
            } else if self.eat_op(">=") {
                Comparison::Ge
            } else if self.eat_op("<") {
                Comparison::Lt
            } else if self.eat_op(">") {
                Comparison::Gt
            } else {
                return Ok(left);
            };
            let right = self.additive_expr()?;
            left = Expr::Compare(op, Box::new(left), Box::new(right));
        }
    }

    fn additive_expr(&mut self) -> Result<Expr, Error> {
        let mut left = self.multiplicative_expr()?;
        loop {
            let op = if self.eat_op("+") {
                Arith::Add
            } else if self.eat_op("-") {
                Arith::Sub
            } else {
                return Ok(left);
            };
            let right = self.multiplicative_expr()?;
            left = Expr::Arith(op, Box::new(left), Box::new(right));
        }
    }

    fn multiplicative_expr(&mut self) -> Result<Expr, Error> {
        let mut left = self.unary_expr()?;
        loop {
            let op = if self.eat_op("*") {
                Arith::Mul
            } else if self.eat_op("div") {
                Arith::Div
            } else if self.eat_op("mod") {
                Arith::Mod
            } else {
                return Ok(left);
            };
            let right = self.unary_expr()?;
            left = Expr::Arith(op, Box::new(left), Box::new(right));
        }
    }

    fn unary_expr(&mut self) -> Result<Expr, Error> {
        if self.eat_op("-") {
            let inner = self.unary_expr()?;
            return Ok(Expr::Neg(Box::new(inner)));
        }
        self.union_expr()
    }

    fn union_expr(&mut self) -> Result<Expr, Error> {
        let mut left = self.path_expr()?;
        while self.eat_op("|") {
            let right = self.path_expr()?;
            left = Expr::Union(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn starts_location_path(&self) -> bool {
        matches!(
            self.peek(),
            Some(
                Token::Dot
                    | Token::DotDot
                    | Token::At
                    | Token::NameTest(..)
                    | Token::NodeType(_)
                    | Token::Axis(_)
            )
        ) || matches!(self.peek(), Some(Token::Op(o)) if o == "/" || o == "//")
    }

    fn path_expr(&mut self) -> Result<Expr, Error> {
        if self.starts_location_path() {
            return self.location_path();
        }
        let primary = self.primary_expr()?;
        let mut predicates = Vec::new();
        while self.peek() == Some(&Token::LBracket) {
            predicates.push(self.predicate()?);
        }
        let mut steps = Vec::new();
        if self.eat_op("//") {
            steps.push(descendant_or_self_step());
            self.relative_location_path(&mut steps)?;
        } else if self.eat_op("/") {
            self.relative_location_path(&mut steps)?;
        } else if predicates.is_empty() {
            return Ok(primary);
        }
        Ok(Expr::Path(
            PathStart::Filter(Box::new(primary), predicates),
            steps,
        ))
    }

    fn location_path(&mut self) -> Result<Expr, Error> {
        let mut steps = Vec::new();
        if self.eat_op("//") {
            steps.push(descendant_or_self_step());
            self.relative_location_path(&mut steps)?;
            return Ok(Expr::Path(PathStart::Root, steps));
        }
        if self.eat_op("/") {
            if self.starts_location_path() {
                self.relative_location_path(&mut steps)?;
            }
            return Ok(Expr::Path(PathStart::Root, steps));
        }
        self.relative_location_path(&mut steps)?;
        Ok(Expr::Path(PathStart::Context, steps))
    }

    fn relative_location_path(&mut self, steps: &mut Vec<Step>) -> Result<(), Error> {
        steps.push(self.step()?);
        loop {
            if self.eat_op("//") {
                steps.push(descendant_or_self_step());
                steps.push(self.step()?);
            } else if self.eat_op("/") {
                steps.push(self.step()?);
            } else {
                return Ok(());
            }
        }
    }

    fn step(&mut self) -> Result<Step, Error> {
        match self.peek() {
            Some(Token::Dot) => {
                self.pos += 1;
                return Ok(Step {
                    axis: Axis::Own,
                    test: NodeTest::Node,
                    predicates: Vec::new(),
                });
            }
            Some(Token::DotDot) => {
                self.pos += 1;
                return Ok(Step {
                    axis: Axis::Parent,
                    test: NodeTest::Node,
                    predicates: Vec::new(),
                });
            }
            _ => {}
        }
        let axis = match self.peek() {
            Some(Token::At) => {
                self.pos += 1;
                Axis::Attribute
            }
            Some(Token::Axis(name)) => {
                let axis = Axis::parse(name)
                    .ok_or_else(|| Error::new(format!("unknown axis `{name}`")))?;
                self.pos += 1;
                self.expect(Token::ColonColon, "`::`")?;
                axis
            }
            _ => Axis::Child,
        };
        let test = match self.next() {
            Some(Token::NameTest(prefix, local)) => {
                self.note_prefix(&prefix);
                match (prefix, local) {
                    (None, None) => NodeTest::Any,
                    (Some(p), None) => NodeTest::Prefixed(p),
                    (p, Some(l)) => NodeTest::Name(p, l),
                }
            }
            Some(Token::NodeType(kind)) => {
                self.expect(Token::LParen, "`(`")?;
                let test = match kind.as_str() {
                    "node" => NodeTest::Node,
                    "text" => NodeTest::Text,
                    "comment" => NodeTest::Comment,
                    _ => {
                        if let Some(Token::Literal(target)) = self.peek().cloned() {
                            self.pos += 1;
                            NodeTest::Pi(Some(target))
                        } else {
                            NodeTest::Pi(None)
                        }
                    }
                };
                self.expect(Token::RParen, "`)`")?;
                test
            }
            _ => return Err(Error::new("expected a node test")),
        };
        let mut predicates = Vec::new();
        while self.peek() == Some(&Token::LBracket) {
            predicates.push(self.predicate()?);
        }
        Ok(Step {
            axis,
            test,
            predicates,
        })
    }

    fn predicate(&mut self) -> Result<Expr, Error> {
        self.expect(Token::LBracket, "`[`")?;
        let expr = self.expr()?;
        self.expect(Token::RBracket, "`]`")?;
        Ok(expr)
    }

    fn primary_expr(&mut self) -> Result<Expr, Error> {
        match self.next() {
            Some(Token::Variable(name)) => Ok(Expr::Variable(name)),
            Some(Token::LParen) => {
                let inner = self.expr()?;
                self.expect(Token::RParen, "`)`")?;
                Ok(inner)
            }
            Some(Token::Literal(text)) => Ok(Expr::Literal(text)),
            Some(Token::Number(n)) => Ok(Expr::Number(n)),
            Some(Token::Function(name)) => {
                self.expect(Token::LParen, "`(`")?;
                let mut args = Vec::new();
                if self.peek() != Some(&Token::RParen) {
                    loop {
                        args.push(self.expr()?);
                        if self.peek() == Some(&Token::Comma) {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                }
                self.expect(Token::RParen, "`)`")?;
                Ok(Expr::Call(name, args))
            }
            Some(other) => Err(Error::new(format!("unexpected token {other:?}"))),
            None => Err(Error::new("unexpected end of expression")),
        }
    }
}

fn descendant_or_self_step() -> Step {
    Step {
        axis: Axis::DescendantOrSelf,
        test: NodeTest::Node,
        predicates: Vec::new(),
    }
}

/// Compiles an expression, or says why it is not XPath.
pub fn compile(source: &str) -> Result<Expression, Error> {
    let tokens = tokenize(source)?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        prefixes: Vec::new(),
    };
    let expr = parser.expr()?;
    if parser.pos < parser.tokens.len() {
        return Err(Error::new(format!(
            "unexpected token {:?}",
            parser.tokens[parser.pos]
        )));
    }
    Ok(Expression {
        expr,
        prefixes: parser.prefixes,
    })
}

// ------------------------------------------------------------- evaluation

struct Evaluator<'a> {
    dom: &'a Dom,
    namespaces: &'a HashMap<String, String>,
    /// Document-order keys, built the first time a node-set needs sorting.
    order: HashMap<NodeId, (u32, u32)>,
    roots_seen: u32,
}

struct Context {
    node: XNode,
    position: usize,
    size: usize,
}

/// Evaluates a compiled expression with `context` as the context node.
/// `namespaces` maps the prefixes in [`Expression::prefixes`] to URIs;
/// a prefix missing from it is an error.
pub fn evaluate(
    dom: &Dom,
    expression: &Expression,
    context: NodeId,
    namespaces: &HashMap<String, String>,
) -> Result<Value, Error> {
    if let Some(prefix) = expression
        .prefixes
        .iter()
        .find(|p| !namespaces.contains_key(*p))
    {
        return Err(Error {
            message: format!("the namespace prefix `{prefix}` is not resolvable"),
            unresolved_prefix: Some(prefix.clone()),
        });
    }
    let mut evaluator = Evaluator {
        dom,
        namespaces,
        order: HashMap::new(),
        roots_seen: 0,
    };
    let cx = Context {
        node: XNode::Node(context),
        position: 1,
        size: 1,
    };
    evaluator.eval(&expression.expr, &cx)
}

impl<'a> Evaluator<'a> {
    fn eval(&mut self, expr: &Expr, cx: &Context) -> Result<Value, Error> {
        Ok(match expr {
            Expr::Or(a, b) => {
                let a = self.eval(a, cx)?;
                Value::Bool(
                    self.boolean(&a) || {
                        let b = self.eval(b, cx)?;
                        self.boolean(&b)
                    },
                )
            }
            Expr::And(a, b) => {
                let a = self.eval(a, cx)?;
                Value::Bool(
                    self.boolean(&a) && {
                        let b = self.eval(b, cx)?;
                        self.boolean(&b)
                    },
                )
            }
            Expr::Compare(op, a, b) => {
                let a = self.eval(a, cx)?;
                let b = self.eval(b, cx)?;
                Value::Bool(self.compare(*op, &a, &b))
            }
            Expr::Arith(op, a, b) => {
                let a = self.eval(a, cx)?;
                let b = self.eval(b, cx)?;
                let (a, b) = (self.number(&a), self.number(&b));
                Value::Number(match op {
                    Arith::Add => a + b,
                    Arith::Sub => a - b,
                    Arith::Mul => a * b,
                    Arith::Div => a / b,
                    Arith::Mod => a % b,
                })
            }
            Expr::Neg(a) => {
                let a = self.eval(a, cx)?;
                Value::Number(-self.number(&a))
            }
            Expr::Union(a, b) => {
                let a = self.eval(a, cx)?;
                let b = self.eval(b, cx)?;
                match (a, b) {
                    (Value::Nodes(mut a), Value::Nodes(b)) => {
                        a.extend(b);
                        Value::Nodes(self.normalize(a))
                    }
                    _ => return Err(Error::new("`|` needs node-sets on both sides")),
                }
            }
            Expr::Literal(s) => Value::String(s.clone()),
            Expr::Number(n) => Value::Number(*n),
            Expr::Variable(name) => {
                return Err(Error::new(format!("the variable `${name}` is not bound")));
            }
            Expr::Call(name, args) => self.call(name, args, cx)?,
            Expr::Path(start, steps) => {
                let mut nodes = match start {
                    PathStart::Root => vec![XNode::Node(self.root(cx.node))],
                    PathStart::Context => vec![cx.node],
                    PathStart::Filter(primary, predicates) => {
                        let value = self.eval(primary, cx)?;
                        let Value::Nodes(nodes) = value else {
                            if steps.is_empty() && predicates.is_empty() {
                                return Ok(value);
                            }
                            return Err(Error::new("a filter needs a node-set"));
                        };
                        let mut nodes = self.normalize(nodes);
                        for predicate in predicates {
                            nodes = self.filter(nodes, predicate)?;
                        }
                        nodes
                    }
                };
                for step in steps {
                    let mut out = Vec::new();
                    for &node in &nodes {
                        let along = self.axis_nodes(step.axis, node);
                        let mut matched: Vec<XNode> = along
                            .into_iter()
                            .filter(|&n| self.matches(step.axis, &step.test, n))
                            .collect();
                        for predicate in &step.predicates {
                            matched = self.filter(matched, predicate)?;
                        }
                        out.extend(matched);
                    }
                    nodes = self.normalize(out);
                }
                Value::Nodes(nodes)
            }
        })
    }

    /// Keeps the nodes for which `predicate` holds. `nodes` are in axis
    /// order, nearest first along a reverse axis, so a node's position is
    /// its proximity position.
    fn filter(&mut self, nodes: Vec<XNode>, predicate: &Expr) -> Result<Vec<XNode>, Error> {
        let size = nodes.len();
        let mut out = Vec::new();
        for (i, node) in nodes.into_iter().enumerate() {
            let position = i + 1;
            let cx = Context {
                node,
                position,
                size,
            };
            let value = self.eval(predicate, &cx)?;
            let keep = match value {
                Value::Number(n) => n == position as f64,
                other => self.boolean(&other),
            };
            if keep {
                out.push(node);
            }
        }
        Ok(out)
    }

    // ---- axes --------------------------------------------------------

    fn axis_nodes(&self, axis: Axis, node: XNode) -> Vec<XNode> {
        let dom = self.dom;
        let (id, is_attr) = match node {
            XNode::Node(id) => (id, false),
            XNode::Attr(id, _) => (id, true),
        };
        match axis {
            Axis::Own => vec![node],
            Axis::Child if is_attr => Vec::new(),
            Axis::Child => dom.children(id).map(XNode::Node).collect(),
            Axis::Descendant if is_attr => Vec::new(),
            Axis::Descendant => dom.descendants(id).map(XNode::Node).collect(),
            Axis::DescendantOrSelf if is_attr => vec![node],
            Axis::DescendantOrSelf => dom.traverse(id).map(XNode::Node).collect(),
            Axis::Parent if is_attr => vec![XNode::Node(id)],
            Axis::Parent => dom.parent(id).map(XNode::Node).into_iter().collect(),
            Axis::Ancestor if is_attr => dom.traverse_up(id).map(XNode::Node).collect(),
            Axis::Ancestor => dom.ancestors(id).map(XNode::Node).collect(),
            Axis::AncestorOrSelf => {
                let mut out = vec![node];
                let start = if is_attr { Some(id) } else { dom.parent(id) };
                let mut cur = start;
                while let Some(n) = cur {
                    out.push(XNode::Node(n));
                    cur = dom.parent(n);
                }
                out
            }
            Axis::FollowingSibling if is_attr => Vec::new(),
            Axis::FollowingSibling => {
                let mut out = Vec::new();
                let mut cur = dom.next_sibling(id);
                while let Some(n) = cur {
                    out.push(XNode::Node(n));
                    cur = dom.next_sibling(n);
                }
                out
            }
            Axis::PrecedingSibling if is_attr => Vec::new(),
            Axis::PrecedingSibling => {
                let mut out = Vec::new();
                let mut cur = dom.prev_sibling(id);
                while let Some(n) = cur {
                    out.push(XNode::Node(n));
                    cur = dom.prev_sibling(n);
                }
                out
            }
            Axis::Following => {
                // Everything after the node in document order, less its
                // descendants: the subtrees of the following siblings of
                // the node and of each ancestor.
                let mut out = Vec::new();
                let mut cur = Some(id);
                while let Some(n) = cur {
                    let mut sibling = dom.next_sibling(n);
                    while let Some(s) = sibling {
                        out.extend(dom.traverse(s).map(XNode::Node));
                        sibling = dom.next_sibling(s);
                    }
                    cur = dom.parent(n);
                }
                out
            }
            Axis::Preceding => {
                // Everything before the node in document order, less its
                // ancestors, nearest first.
                let mut out = Vec::new();
                let mut cur = Some(id);
                while let Some(n) = cur {
                    let mut sibling = dom.prev_sibling(n);
                    while let Some(s) = sibling {
                        let mut subtree: Vec<XNode> = dom.traverse(s).map(XNode::Node).collect();
                        subtree.reverse();
                        out.extend(subtree);
                        sibling = dom.prev_sibling(s);
                    }
                    cur = dom.parent(n);
                }
                out
            }
            Axis::Attribute if is_attr => Vec::new(),
            Axis::Attribute => match dom.element(id) {
                Some(el) => el
                    .attrs
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| a.name.ns != ns!(xmlns))
                    .map(|(i, _)| XNode::Attr(id, i))
                    .collect(),
                None => Vec::new(),
            },
            Axis::Namespace => Vec::new(),
        }
    }

    fn matches(&self, axis: Axis, test: &NodeTest, node: XNode) -> bool {
        let dom = self.dom;
        match node {
            XNode::Attr(id, index) => {
                if axis != Axis::Attribute {
                    return false;
                }
                let Some(attr) = dom.element(id).and_then(|el| el.attrs.get(index)) else {
                    return false;
                };
                match test {
                    NodeTest::Any | NodeTest::Node => true,
                    NodeTest::Prefixed(p) => self.ns_of(p) == Some(&*attr.name.ns),
                    NodeTest::Name(prefix, local) => {
                        let ns_ok = match prefix {
                            Some(p) => self.ns_of(p) == Some(&*attr.name.ns),
                            None => attr.name.ns.is_empty(),
                        };
                        ns_ok && *attr.name.local == **local
                    }
                    NodeTest::Text | NodeTest::Comment | NodeTest::Pi(_) => false,
                }
            }
            XNode::Node(id) => match test {
                NodeTest::Node => true,
                NodeTest::Text => matches!(dom.kind(id), NodeKind::Text(_)),
                NodeTest::Comment => matches!(dom.kind(id), NodeKind::Comment(_)),
                NodeTest::Pi(target) => match dom.kind(id) {
                    NodeKind::ProcessingInstruction { target: t, .. } => {
                        target.as_ref().is_none_or(|wanted| wanted == t)
                    }
                    _ => false,
                },
                NodeTest::Any => axis != Axis::Attribute && dom.is_element(id),
                NodeTest::Prefixed(p) => {
                    axis != Axis::Attribute
                        && dom
                            .element(id)
                            .is_some_and(|el| self.ns_of(p) == Some(&*el.name.ns))
                }
                NodeTest::Name(prefix, local) => {
                    if axis == Axis::Attribute {
                        return false;
                    }
                    let Some(el) = dom.element(id) else {
                        return false;
                    };
                    let html_in_html =
                        el.is_html() && dom.document_data_of(id).is_none_or(|d| !d.is_xml);
                    match prefix {
                        Some(p) => self.ns_of(p) == Some(&*el.name.ns) && *el.name.local == **local,
                        None if html_in_html => {
                            (*el.name.local).eq_ignore_ascii_case(local.as_str())
                        }
                        None => el.name.ns.is_empty() && *el.name.local == **local,
                    }
                }
            },
        }
    }

    fn ns_of(&self, prefix: &str) -> Option<&str> {
        self.namespaces.get(prefix).map(String::as_str)
    }

    // ---- document order ----------------------------------------------

    fn root(&self, node: XNode) -> NodeId {
        let id = match node {
            XNode::Node(id) | XNode::Attr(id, _) => id,
        };
        self.dom.root_of(id)
    }

    fn order_key(&mut self, node: XNode) -> (u32, u32, u32) {
        let (id, attr) = match node {
            XNode::Node(id) => (id, 0),
            XNode::Attr(id, index) => (id, index as u32 + 1),
        };
        if !self.order.contains_key(&id) {
            let root = self.dom.root_of(id);
            self.roots_seen += 1;
            let root_index = self.roots_seen;
            for (i, n) in self.dom.traverse(root).enumerate() {
                self.order.insert(n, (root_index, i as u32));
            }
        }
        let (root_index, index) = self.order[&id];
        (root_index, index, attr)
    }

    /// Document order without duplicates.
    fn normalize(&mut self, nodes: Vec<XNode>) -> Vec<XNode> {
        let mut seen = HashSet::with_capacity(nodes.len());
        let mut keyed: Vec<((u32, u32, u32), XNode)> = nodes
            .into_iter()
            .filter(|n| seen.insert(*n))
            .map(|n| (self.order_key(n), n))
            .collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        keyed.into_iter().map(|(_, n)| n).collect()
    }

    // ---- conversions ---------------------------------------------------

    pub fn string_value(&self, node: XNode) -> String {
        let dom = self.dom;
        match node {
            XNode::Attr(id, index) => dom
                .element(id)
                .and_then(|el| el.attrs.get(index))
                .map(|a| a.value.clone())
                .unwrap_or_default(),
            XNode::Node(id) => match dom.kind(id) {
                NodeKind::Text(t) => t.clone(),
                NodeKind::Comment(t) => t.clone(),
                NodeKind::ProcessingInstruction { data, .. } => data.clone(),
                NodeKind::Doctype(_) => String::new(),
                NodeKind::Element(_) | NodeKind::Document(_) | NodeKind::DocumentFragment(_) => {
                    let mut out = String::new();
                    for n in dom.descendants(id) {
                        if let NodeKind::Text(t) = dom.kind(n) {
                            out.push_str(t);
                        }
                    }
                    out
                }
            },
        }
    }

    fn string(&self, value: &Value) -> String {
        match value {
            Value::Nodes(nodes) => nodes
                .first()
                .map(|&n| self.string_value(n))
                .unwrap_or_default(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => number_to_string(*n),
            Value::String(s) => s.clone(),
        }
    }

    fn number(&self, value: &Value) -> f64 {
        match value {
            Value::Number(n) => *n,
            Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::String(s) => string_to_number(s),
            Value::Nodes(_) => string_to_number(&self.string(value)),
        }
    }

    fn boolean(&self, value: &Value) -> bool {
        match value {
            Value::Nodes(nodes) => !nodes.is_empty(),
            Value::Bool(b) => *b,
            Value::Number(n) => *n != 0.0 && !n.is_nan(),
            Value::String(s) => !s.is_empty(),
        }
    }

    fn compare(&self, op: Comparison, a: &Value, b: &Value) -> bool {
        // Node-sets compare existentially: true if any member does.
        match (a, b) {
            (Value::Nodes(xs), Value::Nodes(ys)) => {
                let ys: Vec<String> = ys.iter().map(|&n| self.string_value(n)).collect();
                xs.iter().any(|&x| {
                    let x = self.string_value(x);
                    ys.iter().any(|y| {
                        self.compare_atoms(op, &Value::String(x.clone()), &Value::String(y.clone()))
                    })
                })
            }
            (Value::Nodes(xs), other) => xs.iter().any(|&x| {
                let x = self.string_value(x);
                self.compare_atoms(op, &Value::String(x), other)
            }),
            (other, Value::Nodes(ys)) => ys.iter().any(|&y| {
                let y = self.string_value(y);
                self.compare_atoms(op, other, &Value::String(y))
            }),
            _ => self.compare_atoms(op, a, b),
        }
    }

    fn compare_atoms(&self, op: Comparison, a: &Value, b: &Value) -> bool {
        match op {
            Comparison::Eq | Comparison::Ne => {
                let equal = match (a, b) {
                    (Value::Bool(_), _) | (_, Value::Bool(_)) => self.boolean(a) == self.boolean(b),
                    (Value::Number(_), _) | (_, Value::Number(_)) => {
                        self.number(a) == self.number(b)
                    }
                    _ => self.string(a) == self.string(b),
                };
                equal == (op == Comparison::Eq)
            }
            Comparison::Lt => self.number(a) < self.number(b),
            Comparison::Le => self.number(a) <= self.number(b),
            Comparison::Gt => self.number(a) > self.number(b),
            Comparison::Ge => self.number(a) >= self.number(b),
        }
    }

    // ---- the core function library -------------------------------------

    fn call(&mut self, name: &str, args: &[Expr], cx: &Context) -> Result<Value, Error> {
        let arity = |min: usize, max: usize| -> Result<(), Error> {
            if args.len() < min || args.len() > max {
                Err(Error::new(format!(
                    "`{name}()` takes {} argument(s), not {}",
                    if min == max {
                        min.to_string()
                    } else {
                        format!("{min} to {max}")
                    },
                    args.len()
                )))
            } else {
                Ok(())
            }
        };
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.eval(arg, cx)?);
        }
        let value_or_context = |this: &Self, values: &[Value]| -> Value {
            values
                .first()
                .cloned()
                .unwrap_or_else(|| Value::String(this.string_value(cx.node)))
        };
        Ok(match name {
            "last" => {
                arity(0, 0)?;
                Value::Number(cx.size as f64)
            }
            "position" => {
                arity(0, 0)?;
                Value::Number(cx.position as f64)
            }
            "count" => {
                arity(1, 1)?;
                match &values[0] {
                    Value::Nodes(nodes) => Value::Number(nodes.len() as f64),
                    _ => return Err(Error::new("`count()` needs a node-set")),
                }
            }
            "id" => {
                arity(1, 1)?;
                let ids: Vec<String> = match &values[0] {
                    Value::Nodes(nodes) => nodes
                        .iter()
                        .flat_map(|&n| {
                            self.string_value(n)
                                .split_whitespace()
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        })
                        .collect(),
                    other => self
                        .string(other)
                        .split_whitespace()
                        .map(str::to_string)
                        .collect(),
                };
                let root = self.root(cx.node);
                let found: Vec<XNode> = self
                    .dom
                    .traverse(root)
                    .filter(|&n| {
                        self.dom
                            .element(n)
                            .and_then(|el| el.attr("id"))
                            .is_some_and(|id| ids.iter().any(|w| w == id))
                    })
                    .map(XNode::Node)
                    .collect();
                Value::Nodes(found)
            }
            "local-name" | "namespace-uri" | "name" => {
                arity(0, 1)?;
                let node = match values.first() {
                    Some(Value::Nodes(nodes)) => nodes.first().copied(),
                    Some(_) => return Err(Error::new(format!("`{name}()` needs a node-set"))),
                    None => Some(cx.node),
                };
                let Some(node) = node else {
                    return Ok(Value::String(String::new()));
                };
                let (ns, prefix, local) = self.expanded_name(node);
                Value::String(match name {
                    "local-name" => local,
                    "namespace-uri" => ns,
                    _ => match prefix {
                        Some(p) if !p.is_empty() => format!("{p}:{local}"),
                        _ => local,
                    },
                })
            }
            "string" => {
                arity(0, 1)?;
                let v = value_or_context(self, &values);
                Value::String(self.string(&v))
            }
            "concat" => {
                arity(2, usize::MAX)?;
                Value::String(values.iter().map(|v| self.string(v)).collect())
            }
            "starts-with" => {
                arity(2, 2)?;
                Value::Bool(
                    self.string(&values[0])
                        .starts_with(&self.string(&values[1])),
                )
            }
            "contains" => {
                arity(2, 2)?;
                Value::Bool(self.string(&values[0]).contains(&self.string(&values[1])))
            }
            "substring-before" => {
                arity(2, 2)?;
                let (s, t) = (self.string(&values[0]), self.string(&values[1]));
                Value::String(s.find(&t).map(|i| s[..i].to_string()).unwrap_or_default())
            }
            "substring-after" => {
                arity(2, 2)?;
                let (s, t) = (self.string(&values[0]), self.string(&values[1]));
                Value::String(
                    s.find(&t)
                        .map(|i| s[i + t.len()..].to_string())
                        .unwrap_or_default(),
                )
            }
            "substring" => {
                arity(2, 3)?;
                let s: Vec<char> = self.string(&values[0]).chars().collect();
                let start = xpath_round(self.number(&values[1]));
                let end = match values.get(2) {
                    Some(len) => start + xpath_round(self.number(len)),
                    None => f64::INFINITY,
                };
                // Positions are 1-based; a character at position p is kept
                // when start <= p < end, with NaN comparing false.
                let out: String = s
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| {
                        let p = (*i + 1) as f64;
                        p >= start && p < end
                    })
                    .map(|(_, c)| *c)
                    .collect();
                Value::String(out)
            }
            "string-length" => {
                arity(0, 1)?;
                let v = value_or_context(self, &values);
                Value::Number(self.string(&v).chars().count() as f64)
            }
            "normalize-space" => {
                arity(0, 1)?;
                let v = value_or_context(self, &values);
                Value::String(
                    self.string(&v)
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
            "translate" => {
                arity(3, 3)?;
                let s = self.string(&values[0]);
                let from: Vec<char> = self.string(&values[1]).chars().collect();
                let to: Vec<char> = self.string(&values[2]).chars().collect();
                let out: String = s
                    .chars()
                    .filter_map(|c| match from.iter().position(|&f| f == c) {
                        Some(i) => to.get(i).copied(),
                        None => Some(c),
                    })
                    .collect();
                Value::String(out)
            }
            "boolean" => {
                arity(1, 1)?;
                Value::Bool(self.boolean(&values[0]))
            }
            "not" => {
                arity(1, 1)?;
                Value::Bool(!self.boolean(&values[0]))
            }
            "true" => {
                arity(0, 0)?;
                Value::Bool(true)
            }
            "false" => {
                arity(0, 0)?;
                Value::Bool(false)
            }
            "lang" => {
                arity(1, 1)?;
                let wanted = self.string(&values[0]).to_ascii_lowercase();
                let id = match cx.node {
                    XNode::Node(id) | XNode::Attr(id, _) => id,
                };
                let lang = std::iter::once(id)
                    .chain(self.dom.ancestors(id))
                    .find_map(|n| {
                        let el = self.dom.element(n)?;
                        el.attr_ns(&ns!(xml), "lang")
                            .or_else(|| el.attr("lang"))
                            .map(str::to_ascii_lowercase)
                    });
                Value::Bool(lang.is_some_and(|l| {
                    l == wanted
                        || l.strip_prefix(&wanted)
                            .is_some_and(|rest| rest.starts_with('-'))
                }))
            }
            "number" => {
                arity(0, 1)?;
                let v = value_or_context(self, &values);
                Value::Number(self.number(&v))
            }
            "sum" => {
                arity(1, 1)?;
                match &values[0] {
                    Value::Nodes(nodes) => Value::Number(
                        nodes
                            .iter()
                            .map(|&n| string_to_number(&self.string_value(n)))
                            .sum(),
                    ),
                    _ => return Err(Error::new("`sum()` needs a node-set")),
                }
            }
            "floor" => {
                arity(1, 1)?;
                Value::Number(self.number(&values[0]).floor())
            }
            "ceiling" => {
                arity(1, 1)?;
                Value::Number(self.number(&values[0]).ceil())
            }
            "round" => {
                arity(1, 1)?;
                Value::Number(xpath_round(self.number(&values[0])))
            }
            _ => return Err(Error::new(format!("unknown function `{name}()`"))),
        })
    }

    /// (namespace URI, prefix, local name) of a node; empty for nodes
    /// without names.
    fn expanded_name(&self, node: XNode) -> (String, Option<String>, String) {
        let dom = self.dom;
        match node {
            XNode::Attr(id, index) => dom
                .element(id)
                .and_then(|el| el.attrs.get(index))
                .map(|a| {
                    (
                        a.name.ns.to_string(),
                        a.name.prefix.as_ref().map(|p| p.to_string()),
                        a.name.local.to_string(),
                    )
                })
                .unwrap_or_default(),
            XNode::Node(id) => match dom.kind(id) {
                NodeKind::Element(el) => (
                    el.name.ns.to_string(),
                    el.name.prefix.as_ref().map(|p| p.to_string()),
                    el.name.local.to_string(),
                ),
                NodeKind::ProcessingInstruction { target, .. } => {
                    (String::new(), None, target.clone())
                }
                _ => (String::new(), None, String::new()),
            },
        }
    }
}

/// An evaluator for conversions alone, which never consult namespaces.
fn converter(dom: &Dom) -> Evaluator<'_> {
    static EMPTY: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
    Evaluator {
        dom,
        namespaces: EMPTY.get_or_init(HashMap::new),
        order: HashMap::new(),
        roots_seen: 0,
    }
}

/// The string value of a node, as `string()` gives it.
pub fn string_value(dom: &Dom, node: XNode) -> String {
    converter(dom).string_value(node)
}

/// `string(value)`.
pub fn to_string(dom: &Dom, value: &Value) -> String {
    converter(dom).string(value)
}

/// `number(value)`.
pub fn to_number(dom: &Dom, value: &Value) -> f64 {
    converter(dom).number(value)
}

/// `boolean(value)`.
pub fn to_boolean(value: &Value) -> bool {
    match value {
        Value::Nodes(nodes) => !nodes.is_empty(),
        Value::Bool(b) => *b,
        Value::Number(n) => *n != 0.0 && !n.is_nan(),
        Value::String(s) => !s.is_empty(),
    }
}

/// XPath's `round()`: half rounds towards positive infinity.
fn xpath_round(n: f64) -> f64 {
    if n.is_nan() || n.is_infinite() {
        return n;
    }
    if (-0.5..0.0).contains(&n) {
        return -0.0;
    }
    (n + 0.5).floor()
}

/// `string()` of a number: integers without a point, `NaN`, `Infinity`.
pub fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.to_string()
    } else if n == 0.0 {
        "0".to_string()
    } else if n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i128)
    } else {
        format!("{n}")
    }
}

/// `number()` of a string: an optional sign, digits and a point, with
/// surrounding whitespace; anything else is NaN.
pub fn string_to_number(s: &str) -> f64 {
    let t = s.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'));
    let digits = t.strip_prefix('-').unwrap_or(t);
    let valid = !digits.is_empty()
        && digits != "."
        && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
        && digits.chars().filter(|&c| c == '.').count() <= 1;
    if !valid {
        return f64::NAN;
    }
    t.parse().unwrap_or(f64::NAN)
}

// ----------------------------------------------------------------- order

impl Dom {
    /// The node and its ancestors, nearest first, for an attribute's owner.
    fn traverse_up(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        std::iter::once(id).chain(self.ancestors(id))
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Value::Number(a), Value::Number(b)) => a.partial_cmp(b),
            (Value::String(a), Value::String(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html::{HtmlParseOptions, parse_html};

    fn page() -> (Dom, NodeId) {
        let dom = parse_html(
            r#"<!doctype html><html lang="en"><head><title>T</title></head><body id="b">
<div id="a" class="x" data-n="3"><p>one</p><p class="y">two</p><span hx-on:click="go()">s</span></div>
<ul><li>1</li><li>2</li><li>3</li></ul>
<!-- c --><p>last</p></body></html>"#,
            &HtmlParseOptions::default(),
        )
        .dom;
        let root = dom.document();
        (dom, root)
    }

    fn eval(dom: &Dom, root: NodeId, expr: &str) -> Value {
        let compiled = compile(expr).unwrap_or_else(|e| panic!("{expr}: {e}"));
        evaluate(dom, &compiled, root, &HashMap::new()).unwrap_or_else(|e| panic!("{expr}: {e}"))
    }

    fn names(dom: &Dom, value: &Value) -> Vec<String> {
        match value {
            Value::Nodes(nodes) => nodes
                .iter()
                .map(|&n| match n {
                    XNode::Node(id) => match dom.kind(id) {
                        NodeKind::Element(el) => el.name.local.to_string(),
                        NodeKind::Text(t) => format!("#text({})", t.trim()),
                        NodeKind::Comment(t) => format!("#comment({})", t.trim()),
                        NodeKind::Document(_) => "#document".to_string(),
                        _ => "#other".to_string(),
                    },
                    XNode::Attr(id, i) => {
                        format!("@{}", dom.element(id).unwrap().attrs[i].name.local)
                    }
                })
                .collect(),
            other => panic!("not a node-set: {other:?}"),
        }
    }

    #[test]
    fn location_paths() {
        let (dom, root) = page();
        assert_eq!(
            names(&dom, &eval(&dom, root, "/html/body/div/p")),
            ["p", "p"]
        );
        assert_eq!(names(&dom, &eval(&dom, root, "//p")), ["p", "p", "p"]);
        assert_eq!(names(&dom, &eval(&dom, root, "//P")), ["p", "p", "p"]);
        assert_eq!(names(&dom, &eval(&dom, root, "//div/p[2]")), ["p"]);
        assert_eq!(
            names(&dom, &eval(&dom, root, "//p[@class='y']/text()")),
            ["#text(two)"]
        );
        assert_eq!(
            names(&dom, &eval(&dom, root, "//li[last()]/text()")),
            ["#text(3)"]
        );
        assert_eq!(
            names(&dom, &eval(&dom, root, "//li[position() < 3]")),
            ["li", "li"]
        );
        assert_eq!(
            names(&dom, &eval(&dom, root, "//span/ancestor::*")),
            ["html", "body", "div"]
        );
        assert_eq!(
            names(&dom, &eval(&dom, root, "//span/ancestor::*[1]")),
            ["div"]
        );
        assert_eq!(
            names(&dom, &eval(&dom, root, "//div/@*")),
            ["@id", "@class", "@data-n"]
        );
        assert_eq!(
            names(&dom, &eval(&dom, root, "//body/comment()")),
            ["#comment(c)"]
        );
        assert_eq!(
            names(&dom, &eval(&dom, root, "//ul/following-sibling::p")),
            ["p"]
        );
        assert_eq!(
            names(&dom, &eval(&dom, root, "//ul/preceding::p")),
            ["p", "p"]
        );
        assert_eq!(
            names(&dom, &eval(&dom, root, "//ul/preceding::p[1]/text()")),
            ["#text(two)"]
        );
        assert_eq!(
            names(&dom, &eval(&dom, root, "//div/following::li | //div/p")),
            ["p", "p", "li", "li", "li"]
        );
        assert_eq!(names(&dom, &eval(&dom, root, "id('a')/span")), ["span"]);
        assert_eq!(
            names(&dom, &eval(&dom, root, "(//p)[3]/text()")),
            ["#text(last)"]
        );
        assert_eq!(names(&dom, &eval(&dom, root, "//p/..")), ["body", "div"]);
        assert_eq!(names(&dom, &eval(&dom, root, "/")), ["#document"]);
    }

    #[test]
    fn the_htmx_query_finds_attributes_by_name() {
        let (dom, root) = page();
        let expr =
            r#".//*[@*[ starts-with(name(), "hx-on:") or starts-with(name(), "data-hx-on:") ]]"#;
        assert_eq!(names(&dom, &eval(&dom, root, expr)), ["span"]);
    }

    #[test]
    fn functions_and_operators() {
        let (dom, root) = page();
        let s = |e: &str| match eval(&dom, root, e) {
            Value::String(s) => s,
            Value::Number(n) => number_to_string(n),
            Value::Bool(b) => b.to_string(),
            other => panic!("{other:?}"),
        };
        assert_eq!(s("count(//li)"), "3");
        assert_eq!(s("string(//div/@data-n) + 1"), "4");
        assert_eq!(s("sum(//li) div 2"), "3");
        assert_eq!(s("7 mod 3"), "1");
        assert_eq!(s("-(2 * 3)"), "-6");
        assert_eq!(s("concat('a', 'b', 1)"), "ab1");
        assert_eq!(s("normalize-space('  a   b ')"), "a b");
        assert_eq!(s("substring('12345', 2, 3)"), "234");
        assert_eq!(s("substring('12345', 1.5, 2.6)"), "234");
        assert_eq!(s("substring-before('a-b', '-')"), "a");
        assert_eq!(s("substring-after('a-b', '-')"), "b");
        assert_eq!(s("translate('bar', 'abc', 'ABC')"), "BAr");
        assert_eq!(s("string-length(//title)"), "1");
        assert_eq!(s("contains(//p[1], 'on')"), "true");
        assert_eq!(s("//li = '2'"), "true");
        assert_eq!(s("//li = 4"), "false");
        assert_eq!(s("//li > 2"), "true");
        assert_eq!(s("not(//nothing)"), "true");
        assert_eq!(s("boolean(//p)"), "true");
        assert_eq!(s("lang('en')"), "false");
        assert_eq!(s("boolean(//body[lang('en')])"), "true");
        assert_eq!(s("string(//p[lang('EN')]/text())"), "one");
        assert_eq!(s("round(2.5)"), "3");
        assert_eq!(s("round(-2.5)"), "-2");
        assert_eq!(s("floor(1.9) + ceiling(1.1)"), "3");
        assert_eq!(s("number('abc')"), "NaN");
        assert_eq!(s("1 div 0"), "Infinity");
        assert_eq!(s("string(0.5)"), "0.5");
        assert_eq!(s("local-name(//div/@data-n)"), "data-n");
        assert_eq!(s("name(//span/@*)"), "hx-on:click");
        assert_eq!(s("namespace-uri(//div)"), "http://www.w3.org/1999/xhtml");
        assert_eq!(s("string(//title/text()) = 'T'"), "true");
        assert_eq!(s("'a' = 'a' and 1 != 2 or false()"), "true");
    }

    #[test]
    fn syntax_errors_are_reported() {
        for bad in ["", "//", "//p[", "foo(", "1 +", "@", "'open", "//p]"] {
            assert!(compile(bad).is_err(), "{bad:?} should not compile");
        }
        let (dom, root) = page();
        let expr = compile("//x:p").unwrap();
        assert_eq!(expr.prefixes(), ["x"]);
        let err = evaluate(&dom, &expr, root, &HashMap::new()).unwrap_err();
        assert_eq!(err.unresolved_prefix.as_deref(), Some("x"));
        let mut ns = HashMap::new();
        ns.insert("x".to_string(), "http://www.w3.org/1999/xhtml".to_string());
        assert_eq!(
            names(&dom, &evaluate(&dom, &expr, root, &ns).unwrap()).len(),
            3
        );
    }
}
