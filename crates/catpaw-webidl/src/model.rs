//! An owned Web IDL model, built from `weedle` parse trees.
//!
//! Partial definitions are merged into their targets as files are loaded, so
//! after loading a corpus every interface, mixin, namespace and dictionary
//! appears once with all of its members.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use weedle::Definition;
use weedle::argument::Argument as WArgument;
use weedle::attribute::{ExtendedAttribute, ExtendedAttributeList, IdentifierOrString};
use weedle::interface::{InterfaceMember, StringifierOrInheritOrStatic, StringifierOrStatic};
use weedle::literal::{
    ConstValue as WConstValue, DefaultValue as WDefaultValue, FloatLit, IntegerLit,
};
use weedle::mixin::MixinMember;
use weedle::namespace::NamespaceMember;
use weedle::types::{
    ConstType, FloatingPointType, IntegerType, NonAnyType, RecordKeyType, ReturnType, SingleType,
    Type as WType, UnionMemberType,
};

#[derive(Debug, Clone, PartialEq)]
pub enum ExtValue {
    None,
    Ident(String),
    String(String),
    IdentList(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExtAttr {
    pub name: String,
    pub value: ExtValue,
}

/// Extended attributes: `[Reflect="for", CEReactions]`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtAttrs(pub Vec<ExtAttr>);

impl ExtAttrs {
    pub fn has(&self, name: &str) -> bool {
        self.0.iter().any(|a| a.name == name)
    }

    pub fn get(&self, name: &str) -> Option<&ExtValue> {
        self.0.iter().find(|a| a.name == name).map(|a| &a.value)
    }

    /// The identifier or string value of `[name=value]`.
    pub fn value_str(&self, name: &str) -> Option<&str> {
        match self.get(name)? {
            ExtValue::Ident(s) | ExtValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn merge(&mut self, other: ExtAttrs) {
        for a in other.0 {
            if !self.has(&a.name) {
                self.0.push(a);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Any,
    Undefined,
    Boolean,
    Byte,
    Octet,
    Short,
    UnsignedShort,
    Long,
    UnsignedLong,
    LongLong,
    UnsignedLongLong,
    /// `float` and `double` reject NaN and the infinities.
    Float,
    Double,
    UnrestrictedFloat,
    UnrestrictedDouble,
    DomString,
    ByteString,
    UsvString,
    Object,
    Symbol,
    ArrayBuffer,
    /// `BufferSource`, `ArrayBufferView`, `DataView` and the typed arrays,
    /// by their IDL name.
    BufferLike(String),
    Named(String),
    Sequence(Box<Type>),
    FrozenArray(Box<Type>),
    Record(Box<Type>, Box<Type>),
    Promise(Box<Type>),
    Union(Vec<Type>),
    Nullable(Box<Type>),
}

impl Type {
    pub fn nullable(self, yes: bool) -> Type {
        if yes && !matches!(self, Type::Nullable(_) | Type::Any) {
            Type::Nullable(Box::new(self))
        } else {
            self
        }
    }

    pub fn is_nullable(&self) -> bool {
        matches!(self, Type::Nullable(_))
    }

    /// The type with one level of `?` removed.
    pub fn inner(&self) -> &Type {
        match self {
            Type::Nullable(t) => t,
            t => t,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DefaultValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Null,
    EmptyArray,
    EmptyDict,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConstValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Null,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Argument {
    pub name: String,
    pub ty: Type,
    /// Extended attributes on the type (`[LegacyNullToEmptyString]`, ...).
    pub type_ext: ExtAttrs,
    pub optional: bool,
    pub variadic: bool,
    pub default: Option<DefaultValue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Special {
    Getter,
    Setter,
    Deleter,
    LegacyCaller,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub name: String,
    pub ty: Type,
    pub type_ext: ExtAttrs,
    pub readonly: bool,
    pub is_static: bool,
    pub stringifier: bool,
    pub ext: ExtAttrs,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Operation {
    /// `None` for anonymous special operations (`getter Node? (unsigned long index);`).
    pub name: Option<String>,
    pub ret: Type,
    pub args: Vec<Argument>,
    pub is_static: bool,
    pub stringifier: bool,
    pub special: Option<Special>,
    pub ext: ExtAttrs,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Member {
    Const {
        name: String,
        ty: Type,
        value: ConstValue,
    },
    Attribute(Attribute),
    Operation(Operation),
    Constructor {
        args: Vec<Argument>,
        ext: ExtAttrs,
    },
    Iterable {
        key: Option<Type>,
        value: Type,
    },
    Maplike {
        key: Type,
        value: Type,
        readonly: bool,
    },
    Setlike {
        value: Type,
        readonly: bool,
    },
    /// A bare `stringifier;`.
    Stringifier,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InterfaceKind {
    #[default]
    Interface,
    Mixin,
    Namespace,
    CallbackInterface,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Interface {
    pub name: String,
    pub kind: InterfaceKind,
    pub parent: Option<String>,
    pub ext: ExtAttrs,
    pub members: Vec<Member>,
    /// Whether the non-partial definition has been seen.
    pub defined: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DictionaryMember {
    pub name: String,
    pub ty: Type,
    pub required: bool,
    pub default: Option<DefaultValue>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dictionary {
    pub name: String,
    pub parent: Option<String>,
    pub members: Vec<DictionaryMember>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Enum {
    pub name: String,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Typedef {
    pub name: String,
    pub ty: Type,
    pub type_ext: ExtAttrs,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallbackFunction {
    pub name: String,
    pub ret: Type,
    pub args: Vec<Argument>,
}

/// A merged IDL corpus.
#[derive(Debug, Default)]
pub struct Idl {
    pub interfaces: BTreeMap<String, Interface>,
    pub dictionaries: BTreeMap<String, Dictionary>,
    pub enums: BTreeMap<String, Enum>,
    pub typedefs: BTreeMap<String, Typedef>,
    pub callbacks: BTreeMap<String, CallbackFunction>,
    /// `(interface, mixin)` pairs from `A includes B;`.
    pub includes: Vec<(String, String)>,
}

/// Rewrites syntax newer than the parser understands into equivalent forms it
/// accepts:
///
/// - `[Exposed=*]` becomes `[Exposed=Window]`;
/// - numeric and parenthesised values of the HTML `Reflect*` attributes become
///   strings (`ReflectDefault=2` → `ReflectDefault="2"`,
///   `ReflectRange=(1, 1000)` → `ReflectRange="1,1000"`);
/// - `ObservableArray<T>` becomes `FrozenArray<T>`;
/// - Trusted Types unions (`(TrustedHTML or DOMString)`) collapse to their
///   string member.
pub fn preprocess(text: &str) -> String {
    let mut text = text.to_string();
    for trusted in [
        "TrustedHTML",
        "TrustedScriptURL",
        "TrustedScript",
        "TrustedType",
    ] {
        text = text
            .replace(&format!("{trusted} or "), "")
            .replace(&format!(" or {trusted}"), "");
    }
    for single in [
        "[LegacyNullToEmptyString] DOMString",
        "DOMString",
        "USVString",
    ] {
        text = text.replace(&format!("({single})"), single);
    }
    // SVG is not in the corpus; unions that only add an SVG element collapse
    // to their HTML member.
    text = text.replace(
        "(HTMLScriptElement or SVGScriptElement)",
        "HTMLScriptElement",
    );
    // Asynchronous iteration is newer than the parser: `async_iterable`
    // declarations are dropped (an async iterator can be provided in
    // script), and an `async_sequence<T>` argument is taken as `any`.
    text = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("async_iterable<"))
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    while let Some(start) = text.find("async_sequence<") {
        let end = text[start..]
            .find('>')
            .map_or(text.len(), |e| start + e + 1);
        text.replace_range(start..end, "any");
    }
    let text = text.as_str();
    let mut out = String::with_capacity(text.len() + 64);
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &text[i..];
        if rest.starts_with("Exposed=*") {
            out.push_str("Exposed=Window");
            i += "Exposed=*".len();
            continue;
        }
        if rest.starts_with("ObservableArray<") {
            out.push_str("FrozenArray<");
            i += "ObservableArray<".len();
            continue;
        }
        if let Some(after) = rest
            .strip_prefix("ReflectDefault=")
            .or_else(|| rest.strip_prefix("ReflectRange="))
        {
            let key_len = rest.len() - after.len();
            out.push_str(&rest[..key_len]);
            i += key_len;
            if let Some(inner) = after.strip_prefix('(') {
                if let Some(end) = inner.find(')') {
                    let value: String = inner[..end]
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect();
                    out.push('"');
                    out.push_str(&value);
                    out.push('"');
                    i += end + 2;
                    continue;
                }
            } else if !after.starts_with('"') {
                let end = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
                    .unwrap_or(after.len());
                out.push('"');
                out.push_str(&after[..end]);
                out.push('"');
                i += end;
                continue;
            }
            continue;
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn ext_attrs(list: &Option<ExtendedAttributeList<'_>>) -> ExtAttrs {
    let Some(list) = list else {
        return ExtAttrs::default();
    };
    ExtAttrs(
        list.body
            .list
            .iter()
            .map(|a| match a {
                ExtendedAttribute::NoArgs(n) => ExtAttr {
                    name: n.0.0.to_string(),
                    value: ExtValue::None,
                },
                ExtendedAttribute::Ident(i) => ExtAttr {
                    name: i.lhs_identifier.0.to_string(),
                    value: match i.rhs {
                        IdentifierOrString::Identifier(id) => ExtValue::Ident(id.0.to_string()),
                        IdentifierOrString::String(s) => ExtValue::String(s.0.to_string()),
                    },
                },
                ExtendedAttribute::IdentList(l) => ExtAttr {
                    name: l.identifier.0.to_string(),
                    value: ExtValue::IdentList(
                        l.list.body.list.iter().map(|i| i.0.to_string()).collect(),
                    ),
                },
                ExtendedAttribute::ArgList(a) => ExtAttr {
                    name: a.identifier.0.to_string(),
                    value: ExtValue::None,
                },
                ExtendedAttribute::NamedArgList(a) => ExtAttr {
                    name: a.lhs_identifier.0.to_string(),
                    value: ExtValue::Ident(a.rhs_identifier.0.to_string()),
                },
            })
            .collect(),
    )
}

fn integer_type(t: &IntegerType) -> Type {
    match t {
        IntegerType::LongLong(l) => {
            if l.unsigned.is_some() {
                Type::UnsignedLongLong
            } else {
                Type::LongLong
            }
        }
        IntegerType::Long(l) => {
            if l.unsigned.is_some() {
                Type::UnsignedLong
            } else {
                Type::Long
            }
        }
        IntegerType::Short(s) => {
            if s.unsigned.is_some() {
                Type::UnsignedShort
            } else {
                Type::Short
            }
        }
    }
}

fn float_type(t: &FloatingPointType) -> Type {
    match t {
        FloatingPointType::Float(f) if f.unrestricted.is_some() => Type::UnrestrictedFloat,
        FloatingPointType::Float(_) => Type::Float,
        FloatingPointType::Double(d) if d.unrestricted.is_some() => Type::UnrestrictedDouble,
        FloatingPointType::Double(_) => Type::Double,
    }
}

fn non_any(t: &NonAnyType<'_>) -> Type {
    macro_rules! simple {
        ($m:expr, $ty:expr) => {
            $ty.nullable($m.q_mark.is_some())
        };
    }
    match t {
        NonAnyType::Promise(p) => Type::Promise(Box::new(return_type(&p.generics.body))),
        NonAnyType::Integer(m) => integer_type(&m.type_).nullable(m.q_mark.is_some()),
        NonAnyType::FloatingPoint(m) => float_type(&m.type_).nullable(m.q_mark.is_some()),
        NonAnyType::Boolean(m) => simple!(m, Type::Boolean),
        NonAnyType::Byte(m) => simple!(m, Type::Byte),
        NonAnyType::Octet(m) => simple!(m, Type::Octet),
        NonAnyType::ByteString(m) => simple!(m, Type::ByteString),
        NonAnyType::DOMString(m) => simple!(m, Type::DomString),
        NonAnyType::USVString(m) => simple!(m, Type::UsvString),
        NonAnyType::Sequence(m) => {
            Type::Sequence(Box::new(wtype(&m.type_.generics.body))).nullable(m.q_mark.is_some())
        }
        NonAnyType::Object(m) => simple!(m, Type::Object),
        NonAnyType::Symbol(m) => simple!(m, Type::Symbol),
        NonAnyType::Error(m) => simple!(m, Type::Object),
        NonAnyType::ArrayBuffer(m) => simple!(m, Type::ArrayBuffer),
        NonAnyType::DataView(m) => simple!(m, Type::BufferLike("DataView".into())),
        NonAnyType::Int8Array(m) => simple!(m, Type::BufferLike("Int8Array".into())),
        NonAnyType::Int16Array(m) => simple!(m, Type::BufferLike("Int16Array".into())),
        NonAnyType::Int32Array(m) => simple!(m, Type::BufferLike("Int32Array".into())),
        NonAnyType::Uint8Array(m) => simple!(m, Type::BufferLike("Uint8Array".into())),
        NonAnyType::Uint16Array(m) => simple!(m, Type::BufferLike("Uint16Array".into())),
        NonAnyType::Uint32Array(m) => simple!(m, Type::BufferLike("Uint32Array".into())),
        NonAnyType::Uint8ClampedArray(m) => {
            simple!(m, Type::BufferLike("Uint8ClampedArray".into()))
        }
        NonAnyType::Float32Array(m) => simple!(m, Type::BufferLike("Float32Array".into())),
        NonAnyType::Float64Array(m) => simple!(m, Type::BufferLike("Float64Array".into())),
        NonAnyType::ArrayBufferView(m) => simple!(m, Type::BufferLike("ArrayBufferView".into())),
        NonAnyType::BufferSource(m) => simple!(m, Type::BufferLike("BufferSource".into())),
        NonAnyType::FrozenArrayType(m) => {
            Type::FrozenArray(Box::new(wtype(&m.type_.generics.body))).nullable(m.q_mark.is_some())
        }
        NonAnyType::RecordType(m) => {
            let (k, _, v) = &m.type_.generics.body;
            let key = match &**k {
                RecordKeyType::Byte(_) => Type::ByteString,
                RecordKeyType::DOM(_) => Type::DomString,
                RecordKeyType::USV(_) => Type::UsvString,
                RecordKeyType::NonAny(t) => non_any(t),
            };
            Type::Record(Box::new(key), Box::new(wtype(v))).nullable(m.q_mark.is_some())
        }
        NonAnyType::Identifier(m) => {
            Type::Named(m.type_.0.to_string()).nullable(m.q_mark.is_some())
        }
    }
}

fn wtype(t: &WType<'_>) -> Type {
    match t {
        WType::Single(SingleType::Any(_)) => Type::Any,
        WType::Single(SingleType::NonAny(n)) => non_any(n),
        WType::Union(u) => {
            let members = u
                .type_
                .body
                .list
                .iter()
                .map(|m| match m {
                    UnionMemberType::Single(s) => non_any(&s.type_),
                    UnionMemberType::Union(inner) => wtype(&WType::Union(inner.clone())),
                })
                .collect();
            Type::Union(members).nullable(u.q_mark.is_some())
        }
    }
}

fn return_type(t: &ReturnType<'_>) -> Type {
    match t {
        ReturnType::Undefined(_) => Type::Undefined,
        ReturnType::Type(t) => wtype(t),
    }
}

fn const_type(t: &ConstType<'_>) -> Type {
    match t {
        ConstType::Integer(m) => integer_type(&m.type_),
        ConstType::FloatingPoint(m) => float_type(&m.type_),
        ConstType::Boolean(_) => Type::Boolean,
        ConstType::Byte(_) => Type::Byte,
        ConstType::Octet(_) => Type::Octet,
        ConstType::Identifier(m) => Type::Named(m.type_.0.to_string()),
    }
}

fn integer_lit(l: &IntegerLit<'_>) -> i64 {
    let (text, radix) = match l {
        IntegerLit::Dec(d) => (d.0, 10),
        IntegerLit::Hex(h) => (h.0, 16),
        IntegerLit::Oct(o) => (o.0, 8),
    };
    let negative = text.starts_with('-');
    let digits = text.trim_start_matches('-');
    let digits = if radix == 16 {
        digits.trim_start_matches("0x").trim_start_matches("0X")
    } else {
        digits
    };
    let value = if digits.is_empty() {
        0
    } else {
        i64::from_str_radix(digits, radix).unwrap_or(0)
    };
    if negative { -value } else { value }
}

fn float_lit(l: &FloatLit<'_>) -> f64 {
    match l {
        FloatLit::Value(v) => v.0.parse().unwrap_or(0.0),
        FloatLit::NegInfinity(_) => f64::NEG_INFINITY,
        FloatLit::Infinity(_) => f64::INFINITY,
        FloatLit::NaN(_) => f64::NAN,
    }
}

fn default_value(v: &WDefaultValue<'_>) -> DefaultValue {
    match v {
        WDefaultValue::Boolean(b) => DefaultValue::Bool(b.0),
        WDefaultValue::EmptyArray(_) => DefaultValue::EmptyArray,
        WDefaultValue::EmptyDictionary(_) => DefaultValue::EmptyDict,
        WDefaultValue::Float(f) => DefaultValue::Float(float_lit(f)),
        WDefaultValue::Integer(i) => DefaultValue::Int(integer_lit(i)),
        WDefaultValue::Null(_) => DefaultValue::Null,
        WDefaultValue::String(s) => DefaultValue::String(s.0.to_string()),
    }
}

fn const_value(v: &WConstValue<'_>) -> ConstValue {
    match v {
        WConstValue::Boolean(b) => ConstValue::Bool(b.0),
        WConstValue::Float(f) => ConstValue::Float(float_lit(f)),
        WConstValue::Integer(i) => ConstValue::Int(integer_lit(i)),
        WConstValue::Null(_) => ConstValue::Null,
    }
}

fn arguments(list: &[WArgument<'_>]) -> Vec<Argument> {
    list.iter()
        .map(|a| match a {
            WArgument::Single(s) => Argument {
                name: s.identifier.0.to_string(),
                ty: wtype(&s.type_.type_),
                // The parser attaches a leading `[...]` to the argument
                // rather than to its type; both end up here.
                type_ext: {
                    let mut ext = ext_attrs(&s.type_.attributes);
                    ext.merge(ext_attrs(&s.attributes));
                    ext
                },
                optional: s.optional.is_some(),
                variadic: false,
                default: s.default.as_ref().map(|d| default_value(&d.value)),
            },
            WArgument::Variadic(v) => Argument {
                name: v.identifier.0.to_string(),
                ty: wtype(&v.type_),
                type_ext: ExtAttrs::default(),
                optional: true,
                variadic: true,
                default: None,
            },
        })
        .collect()
}

fn interface_members(members: &[InterfaceMember<'_>]) -> Vec<Member> {
    members
        .iter()
        .filter_map(|m| {
            Some(match m {
                InterfaceMember::Const(c) => Member::Const {
                    name: c.identifier.0.to_string(),
                    ty: const_type(&c.const_type),
                    value: const_value(&c.const_value),
                },
                InterfaceMember::Attribute(a) => Member::Attribute(Attribute {
                    name: a.identifier.0.to_string(),
                    ty: wtype(&a.type_.type_),
                    type_ext: ext_attrs(&a.type_.attributes),
                    readonly: a.readonly.is_some(),
                    is_static: matches!(a.modifier, Some(StringifierOrInheritOrStatic::Static(_))),
                    stringifier: matches!(
                        a.modifier,
                        Some(StringifierOrInheritOrStatic::Stringifier(_))
                    ),
                    ext: ext_attrs(&a.attributes),
                }),
                InterfaceMember::Constructor(c) => Member::Constructor {
                    args: arguments(&c.args.body.list),
                    ext: ext_attrs(&c.attributes),
                },
                InterfaceMember::Operation(o) => Member::Operation(Operation {
                    name: o.identifier.map(|i| i.0.to_string()),
                    ret: return_type(&o.return_type),
                    args: arguments(&o.args.body.list),
                    is_static: matches!(o.modifier, Some(StringifierOrStatic::Static(_))),
                    stringifier: matches!(o.modifier, Some(StringifierOrStatic::Stringifier(_))),
                    special: o.special.map(|s| match s {
                        weedle::interface::Special::Getter(_) => Special::Getter,
                        weedle::interface::Special::Setter(_) => Special::Setter,
                        weedle::interface::Special::Deleter(_) => Special::Deleter,
                        weedle::interface::Special::LegacyCaller(_) => Special::LegacyCaller,
                    }),
                    ext: ext_attrs(&o.attributes),
                }),
                InterfaceMember::Iterable(i) => match i {
                    weedle::interface::IterableInterfaceMember::Single(s) => Member::Iterable {
                        key: None,
                        value: wtype(&s.generics.body.type_),
                    },
                    weedle::interface::IterableInterfaceMember::Double(d) => Member::Iterable {
                        key: Some(wtype(&d.generics.body.0.type_)),
                        value: wtype(&d.generics.body.2.type_),
                    },
                },
                InterfaceMember::AsyncIterable(_) => return None,
                InterfaceMember::Maplike(m) => Member::Maplike {
                    key: wtype(&m.generics.body.0.type_),
                    value: wtype(&m.generics.body.2.type_),
                    readonly: m.readonly.is_some(),
                },
                InterfaceMember::Setlike(s) => Member::Setlike {
                    value: wtype(&s.generics.body.type_),
                    readonly: s.readonly.is_some(),
                },
                InterfaceMember::Stringifier(_) => Member::Stringifier,
            })
        })
        .collect()
}

fn mixin_members(members: &[MixinMember<'_>]) -> Vec<Member> {
    members
        .iter()
        .map(|m| match m {
            MixinMember::Const(c) => Member::Const {
                name: c.identifier.0.to_string(),
                ty: const_type(&c.const_type),
                value: const_value(&c.const_value),
            },
            MixinMember::Operation(o) => Member::Operation(Operation {
                name: o.identifier.map(|i| i.0.to_string()),
                ret: return_type(&o.return_type),
                args: arguments(&o.args.body.list),
                is_static: false,
                stringifier: o.stringifier.is_some(),
                special: None,
                ext: ext_attrs(&o.attributes),
            }),
            MixinMember::Attribute(a) => Member::Attribute(Attribute {
                name: a.identifier.0.to_string(),
                ty: wtype(&a.type_.type_),
                type_ext: ext_attrs(&a.type_.attributes),
                readonly: a.readonly.is_some(),
                is_static: false,
                stringifier: a.stringifier.is_some(),
                ext: ext_attrs(&a.attributes),
            }),
            MixinMember::Stringifier(_) => Member::Stringifier,
        })
        .collect()
}

fn namespace_members(members: &[NamespaceMember<'_>]) -> Vec<Member> {
    members
        .iter()
        .map(|m| match m {
            NamespaceMember::Operation(o) => Member::Operation(Operation {
                name: o.identifier.map(|i| i.0.to_string()),
                ret: return_type(&o.return_type),
                args: arguments(&o.args.body.list),
                is_static: true,
                stringifier: false,
                special: None,
                ext: ext_attrs(&o.attributes),
            }),
            NamespaceMember::Attribute(a) => Member::Attribute(Attribute {
                name: a.identifier.0.to_string(),
                ty: wtype(&a.type_.type_),
                type_ext: ext_attrs(&a.type_.attributes),
                readonly: true,
                is_static: true,
                stringifier: false,
                ext: ext_attrs(&a.attributes),
            }),
        })
        .collect()
}

impl Idl {
    pub fn new() -> Self {
        Self::default()
    }

    fn interface_entry(&mut self, name: &str, kind: InterfaceKind) -> &mut Interface {
        let entry = self
            .interfaces
            .entry(name.to_string())
            .or_insert_with(|| Interface {
                name: name.to_string(),
                kind,
                ..Interface::default()
            });
        entry.kind = kind;
        entry
    }

    /// Parses one IDL file and merges its definitions into the corpus.
    pub fn load(&mut self, source_name: &str, text: &str) -> Result<()> {
        let text = preprocess(text);
        // weedle panics (rather than erring) on trailing input it cannot parse.
        let parsed = std::panic::catch_unwind(|| {
            weedle::parse(&text).map_err(|e| truncate_err(&e.to_string()))
        });
        let definitions = match parsed {
            Ok(Ok(definitions)) => definitions,
            Ok(Err(e)) => return Err(anyhow!("{source_name}: IDL parse error: {e}")),
            Err(_) => {
                return Err(anyhow!(
                    "{source_name}: IDL parse error: unparsed input remains (unsupported syntax?)"
                ));
            }
        };
        for def in &definitions {
            match def {
                Definition::Interface(i) => {
                    let e = self.interface_entry(i.identifier.0, InterfaceKind::Interface);
                    e.defined = true;
                    e.parent = i.inheritance.map(|p| p.identifier.0.to_string());
                    e.ext.merge(ext_attrs(&i.attributes));
                    e.members.extend(interface_members(&i.members.body));
                }
                Definition::PartialInterface(i) => {
                    let e = self.interface_entry(i.identifier.0, InterfaceKind::Interface);
                    e.ext.merge(ext_attrs(&i.attributes));
                    e.members.extend(interface_members(&i.members.body));
                }
                Definition::InterfaceMixin(i) => {
                    let e = self.interface_entry(i.identifier.0, InterfaceKind::Mixin);
                    e.defined = true;
                    e.ext.merge(ext_attrs(&i.attributes));
                    e.members.extend(mixin_members(&i.members.body));
                }
                Definition::PartialInterfaceMixin(i) => {
                    let e = self.interface_entry(i.identifier.0, InterfaceKind::Mixin);
                    e.members.extend(mixin_members(&i.members.body));
                }
                Definition::Namespace(n) => {
                    let e = self.interface_entry(n.identifier.0, InterfaceKind::Namespace);
                    e.defined = true;
                    e.ext.merge(ext_attrs(&n.attributes));
                    e.members.extend(namespace_members(&n.members.body));
                }
                Definition::PartialNamespace(n) => {
                    let e = self.interface_entry(n.identifier.0, InterfaceKind::Namespace);
                    e.members.extend(namespace_members(&n.members.body));
                }
                Definition::CallbackInterface(c) => {
                    let e = self.interface_entry(c.identifier.0, InterfaceKind::CallbackInterface);
                    e.defined = true;
                    e.members.extend(interface_members(&c.members.body));
                }
                Definition::Callback(c) => {
                    self.callbacks.insert(
                        c.identifier.0.to_string(),
                        CallbackFunction {
                            name: c.identifier.0.to_string(),
                            ret: return_type(&c.return_type),
                            args: arguments(&c.arguments.body.list),
                        },
                    );
                }
                Definition::Dictionary(d) => {
                    let e = self
                        .dictionaries
                        .entry(d.identifier.0.to_string())
                        .or_default();
                    e.name = d.identifier.0.to_string();
                    e.parent = d.inheritance.map(|p| p.identifier.0.to_string());
                    e.members.extend(dictionary_members(&d.members.body));
                }
                Definition::PartialDictionary(d) => {
                    let e = self
                        .dictionaries
                        .entry(d.identifier.0.to_string())
                        .or_default();
                    e.name = d.identifier.0.to_string();
                    e.members.extend(dictionary_members(&d.members.body));
                }
                Definition::Enum(e) => {
                    self.enums.insert(
                        e.identifier.0.to_string(),
                        Enum {
                            name: e.identifier.0.to_string(),
                            values: e
                                .values
                                .body
                                .list
                                .iter()
                                .map(|v| v.value.0.to_string())
                                .collect(),
                        },
                    );
                }
                Definition::Typedef(t) => {
                    self.typedefs.insert(
                        t.identifier.0.to_string(),
                        Typedef {
                            name: t.identifier.0.to_string(),
                            ty: wtype(&t.type_.type_),
                            type_ext: ext_attrs(&t.type_.attributes),
                        },
                    );
                }
                Definition::IncludesStatement(i) => {
                    self.includes.push((
                        i.lhs_identifier.0.to_string(),
                        i.rhs_identifier.0.to_string(),
                    ));
                }
                Definition::Implements(i) => {
                    self.includes.push((
                        i.lhs_identifier.0.to_string(),
                        i.rhs_identifier.0.to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Follows typedefs until a non-typedef type is reached.
    pub fn resolve(&self, ty: &Type) -> Type {
        match ty {
            // Shared buffers are treated like ordinary buffer sources.
            Type::Named(name)
                if name == "AllowSharedBufferSource" || name == "SharedArrayBuffer" =>
            {
                Type::BufferLike("BufferSource".into())
            }
            Type::Named(name) => match self.typedefs.get(name) {
                Some(td) => self.resolve(&td.ty),
                None => ty.clone(),
            },
            Type::Nullable(inner) => self.resolve(inner).nullable(true),
            Type::Sequence(inner) => Type::Sequence(Box::new(self.resolve(inner))),
            Type::FrozenArray(inner) => Type::FrozenArray(Box::new(self.resolve(inner))),
            Type::Promise(inner) => Type::Promise(Box::new(self.resolve(inner))),
            Type::Record(k, v) => {
                Type::Record(Box::new(self.resolve(k)), Box::new(self.resolve(v)))
            }
            Type::Union(members) => {
                // Flatten nested unions, as WebIDL's "flattened member types".
                let mut flat = Vec::new();
                let mut nullable = false;
                for m in members {
                    // `(T or undefined)`: modelled as an absent value.
                    if matches!(m, Type::Named(n) if n == "undefined") {
                        nullable = true;
                        continue;
                    }
                    match self.resolve(m) {
                        Type::Union(inner) => flat.extend(inner),
                        Type::Nullable(inner) => {
                            nullable = true;
                            match *inner {
                                Type::Union(inner) => flat.extend(inner),
                                other => flat.push(other),
                            }
                        }
                        other => flat.push(other),
                    }
                }
                let ty = if flat.len() == 1 {
                    flat.remove(0)
                } else {
                    Type::Union(flat)
                };
                ty.nullable(nullable)
            }
            other => other.clone(),
        }
    }

    /// The mixins an interface includes.
    pub fn mixins_of<'a>(&'a self, interface: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.includes
            .iter()
            .filter(move |(i, _)| i == interface)
            .map(|(_, m)| m.as_str())
    }

    /// Whether `name` inherits from (or is) `ancestor`.
    pub fn inherits(&self, name: &str, ancestor: &str) -> bool {
        let mut cur = Some(name);
        while let Some(n) = cur {
            if n == ancestor {
                return true;
            }
            cur = self.interfaces.get(n).and_then(|i| i.parent.as_deref());
        }
        false
    }
}

fn dictionary_members(
    members: &[weedle::dictionary::DictionaryMember<'_>],
) -> Vec<DictionaryMember> {
    members
        .iter()
        .map(|m| DictionaryMember {
            name: m.identifier.0.to_string(),
            ty: wtype(&m.type_),
            required: m.required.is_some(),
            default: m.default.as_ref().map(|d| default_value(&d.value)),
        })
        .collect()
}

fn truncate_err(s: &str) -> String {
    let s = s.replace('\n', " ");
    if s.len() > 200 {
        format!("{}...", &s[..200])
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preprocess_rewrites_new_syntax() {
        let out = preprocess(
            "[Exposed=*] interface A { [Reflect, ReflectDefault=2, ReflectRange=(1, 1000)] attribute unsigned long span; attribute ObservableArray<B> xs; [ReflectDefault=1.0] attribute double v; };",
        );
        assert!(out.contains("[Exposed=Window]"));
        assert!(out.contains("ReflectDefault=\"2\""));
        assert!(out.contains("ReflectRange=\"1,1000\""));
        assert!(out.contains("FrozenArray<B>"));
        assert!(out.contains("ReflectDefault=\"1.0\""));
    }

    #[test]
    fn merges_partials_and_mixins() {
        let mut idl = Idl::new();
        idl.load(
            "a.idl",
            r#"
            [Exposed=Window] interface Node : EventTarget {
              const unsigned short ELEMENT_NODE = 1;
              readonly attribute Node? parentNode;
              Node appendChild(Node node);
            };
            partial interface Node { [CEReactions] attribute DOMString? textContent; };
            interface mixin ParentNode { Element? querySelector(DOMString selectors); };
            Node includes ParentNode;
            dictionary Init { boolean bubbles = false; required DOMString name; };
            enum Mode { "open", "closed" };
            typedef (Node or DOMString) NodeOrString;
            callback Fn = undefined (any... args);
            "#,
        )
        .unwrap();
        let node = &idl.interfaces["Node"];
        assert_eq!(node.parent.as_deref(), Some("EventTarget"));
        assert_eq!(node.members.len(), 4);
        assert!(matches!(
            &node.members[0],
            Member::Const {
                value: ConstValue::Int(1),
                ..
            }
        ));
        assert_eq!(
            idl.mixins_of("Node").collect::<Vec<_>>(),
            vec!["ParentNode"]
        );
        assert_eq!(idl.interfaces["ParentNode"].kind, InterfaceKind::Mixin);
        assert_eq!(idl.dictionaries["Init"].members.len(), 2);
        assert_eq!(idl.enums["Mode"].values, vec!["open", "closed"]);
        assert_eq!(
            idl.resolve(&Type::Named("NodeOrString".into())),
            Type::Union(vec![Type::Named("Node".into()), Type::DomString])
        );
        assert!(idl.callbacks["Fn"].args[0].variadic);
        assert!(idl.inherits("Node", "EventTarget"));
    }
}
