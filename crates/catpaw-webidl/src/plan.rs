//! Turns the IDL corpus plus the binding manifest into a concrete plan:
//! which interfaces get bindings, which members are implemented natively,
//! which are reflected content attributes or event handler attributes, and
//! which auxiliary types (dictionaries, enums, unions) must be generated.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::model::{
    Argument, Attribute, ConstValue, DefaultValue, Idl, InterfaceKind, Member, Operation, Special,
    Type,
};
use crate::names::snake;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MemberCfg {
    /// Members implemented natively (a trait method is generated for each).
    pub members: Vec<String>,
    /// Members defined as no-ops returning a default value.
    pub stubs: Vec<String>,
    /// Whether `new Interface()` is allowed (requires a `constructor` impl).
    pub constructor: bool,
    /// Generate indexed/named property access for the interface's special
    /// operations (the wrapper becomes an exotic object).
    pub exotic: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Manifest {
    pub interfaces: BTreeMap<String, MemberCfg>,
    pub mixins: BTreeMap<String, MemberCfg>,
    pub namespaces: BTreeMap<String, MemberCfg>,
    /// HTML local name → interface name.
    pub tags: BTreeMap<String, String>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }
}

/// How the receiver of a member is represented in implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {
    Node,
    Object,
    Window,
    EventTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReflectKind {
    String,
    NullableString,
    Bool,
    Long,
    UnsignedLong,
    Double,
    Url,
    TokenList,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reflect {
    /// Content attribute name.
    pub attr: String,
    pub kind: ReflectKind,
    /// `[ReflectDefault]` as written.
    pub default: Option<String>,
    /// `[ReflectNonNegative]`, `[ReflectPositive]`, `[ReflectPositiveWithFallback]`.
    pub limit: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AttrKind {
    Native,
    /// Getter and setter both reflect.
    Reflect(Reflect),
    EventHandler,
    Stub,
}

#[derive(Debug, Clone)]
pub struct PAttr {
    pub name: String,
    pub rust: String,
    pub ty: Type,
    pub null_to_empty: bool,
    pub readonly: bool,
    pub is_static: bool,
    pub kind: AttrKind,
    pub same_object: bool,
    pub put_forwards: Option<String>,
    pub replaceable: bool,
    pub stringifier: bool,
    /// Declared as `(T or undefined)`: an absent value reads as `undefined`
    /// rather than `null`.
    pub undefined_when_absent: bool,
    /// The interface or mixin whose trait declares this member.
    pub owner: String,
}

#[derive(Debug, Clone)]
pub struct PArg {
    pub name: String,
    pub rust: String,
    pub ty: Type,
    pub null_to_empty: bool,
    pub optional: bool,
    pub variadic: bool,
    pub default: Option<DefaultValue>,
}

#[derive(Debug, Clone)]
pub struct POverload {
    pub rust: String,
    pub args: Vec<PArg>,
    pub ret: Type,
}

impl POverload {
    pub fn min_args(&self) -> usize {
        self.args
            .iter()
            .filter(|a| !a.optional && !a.variadic)
            .count()
    }

    pub fn max_args(&self) -> Option<usize> {
        if self.args.iter().any(|a| a.variadic) {
            None
        } else {
            Some(self.args.len())
        }
    }
}

#[derive(Debug, Clone)]
pub struct POp {
    pub name: String,
    pub overloads: Vec<POverload>,
    pub is_static: bool,
    pub stub: bool,
    pub owner: String,
}

#[derive(Debug, Clone)]
pub struct PSpecial {
    pub rust: String,
    pub ty: Type,
    /// The interface whose trait declares the operation.
    pub owner: String,
    /// A setter's value is `[LegacyNullToEmptyString]`.
    pub null_to_empty: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Exotic {
    pub indexed_getter: Option<PSpecial>,
    pub named_getter: Option<PSpecial>,
    pub named_setter: Option<PSpecial>,
    pub named_deleter: Option<PSpecial>,
}

#[derive(Debug, Clone)]
pub enum Stringifier {
    Attribute(String),
    Method,
}

#[derive(Debug, Clone)]
pub struct PInterface {
    pub name: String,
    /// Nearest planned ancestor.
    pub parent: Option<String>,
    pub handle: Handle,
    pub kind: InterfaceKind,
    pub global: bool,
    pub constructor: Option<Vec<POverload>>,
    pub consts: Vec<(String, ConstValue)>,
    pub attrs: Vec<PAttr>,
    pub ops: Vec<POp>,
    pub exotic: Option<Exotic>,
    /// `iterable<V>` (value iterator) or `iterable<K, V>` (pair iterator).
    pub iterable: Option<(Option<Type>, Type)>,
    pub stringifier: Option<Stringifier>,
    /// Members this interface or mixin declares natively (the trait body).
    pub trait_members: Vec<TraitMember>,
}

#[derive(Debug, Clone)]
pub enum TraitMember {
    Getter {
        rust: String,
        ty: Type,
        is_static: bool,
    },
    Setter {
        rust: String,
        ty: Type,
        is_static: bool,
    },
    Op {
        overload: POverload,
        is_static: bool,
    },
    Constructor {
        overload: POverload,
    },
    IndexedGet {
        ty: Type,
    },
    NamedGet {
        ty: Type,
    },
    NamedSet {
        ty: Type,
    },
    NamedDelete,
    NamedProperties,
    Iterate {
        key: Type,
        value: Type,
    },
    Stringify,
}

#[derive(Debug, Clone)]
pub struct UnionDef {
    pub name: String,
    pub members: Vec<Type>,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Interfaces in an order where parents precede children.
    pub interfaces: Vec<PInterface>,
    /// Mixins and namespaces that own trait members.
    pub mixins: Vec<PInterface>,
    pub namespaces: Vec<PInterface>,
    pub dictionaries: Vec<String>,
    pub enums: Vec<String>,
    pub unions: Vec<UnionDef>,
    pub tags: BTreeMap<String, String>,
}

/// Classification of a named type, for emitters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Named {
    Node,
    Object,
    Window,
    EventTarget,
    Dictionary,
    Enum,
    Callback,
    CallbackInterface,
    Unknown,
}

pub struct Planner<'a> {
    pub idl: &'a Idl,
    pub manifest: &'a Manifest,
    planned: BTreeSet<String>,
    dictionaries: BTreeSet<String>,
    enums: BTreeSet<String>,
    unions: BTreeMap<String, UnionDef>,
}

fn is_event_handler_type(idl: &Idl, ty: &Type) -> bool {
    match ty {
        Type::Named(n) => {
            matches!(
                n.as_str(),
                "EventHandler" | "OnErrorEventHandler" | "OnBeforeUnloadEventHandler"
            ) || idl
                .typedefs
                .get(n)
                .is_some_and(|td| is_event_handler_type(idl, &td.ty))
        }
        Type::Nullable(inner) => is_event_handler_type(idl, inner),
        _ => false,
    }
}

impl<'a> Planner<'a> {
    pub fn new(idl: &'a Idl, manifest: &'a Manifest) -> Self {
        let planned = manifest.interfaces.keys().cloned().collect();
        Self {
            idl,
            manifest,
            planned,
            dictionaries: BTreeSet::new(),
            enums: BTreeSet::new(),
            unions: BTreeMap::new(),
        }
    }

    pub fn classify(&self, name: &str) -> Named {
        if name == "Window" || name == "WindowProxy" {
            return Named::Window;
        }
        if name == "EventTarget" {
            return Named::EventTarget;
        }
        if let Some(i) = self.idl.interfaces.get(name) {
            return match i.kind {
                InterfaceKind::CallbackInterface => Named::CallbackInterface,
                InterfaceKind::Interface if self.planned.contains(name) => {
                    if self.idl.inherits(name, "Node") {
                        Named::Node
                    } else {
                        Named::Object
                    }
                }
                _ => Named::Unknown,
            };
        }
        if self.idl.dictionaries.contains_key(name) {
            return Named::Dictionary;
        }
        if self.idl.enums.contains_key(name) {
            return Named::Enum;
        }
        if self.idl.callbacks.contains_key(name) {
            return Named::Callback;
        }
        Named::Unknown
    }

    /// Whether every named type inside `ty` can be converted.
    pub fn supported(&self, ty: &Type) -> bool {
        match ty {
            Type::Named(n) => self.classify(n) != Named::Unknown,
            Type::Nullable(t) | Type::Sequence(t) | Type::FrozenArray(t) | Type::Promise(t) => {
                self.supported(t)
            }
            Type::Record(k, v) => self.supported(k) && self.supported(v),
            Type::Union(members) => members.iter().all(|m| self.supported(m)),
            Type::Symbol => false,
            _ => true,
        }
    }

    /// The WebIDL-style name of a union member, used to name union enums
    /// and their variants.
    pub fn type_label(&self, ty: &Type) -> String {
        match ty {
            Type::Any => "Any".into(),
            Type::Undefined => "Undefined".into(),
            Type::Boolean => "Boolean".into(),
            Type::Byte => "Byte".into(),
            Type::Octet => "Octet".into(),
            Type::Short => "Short".into(),
            Type::UnsignedShort => "UnsignedShort".into(),
            Type::Long => "Long".into(),
            Type::UnsignedLong => "UnsignedLong".into(),
            Type::LongLong => "LongLong".into(),
            Type::UnsignedLongLong => "UnsignedLongLong".into(),
            Type::Float => "Float".into(),
            Type::Double => "Double".into(),
            Type::DomString | Type::UsvString | Type::ByteString => "String".into(),
            Type::Object => "Object".into(),
            Type::Symbol => "Symbol".into(),
            Type::ArrayBuffer => "ArrayBuffer".into(),
            Type::BufferLike(n) => n.clone(),
            Type::Named(n) => n.clone(),
            Type::Sequence(t) | Type::FrozenArray(t) => format!("{}Sequence", self.type_label(t)),
            Type::Record(k, v) => format!("{}{}Record", self.type_label(k), self.type_label(v)),
            Type::Promise(_) => "Promise".into(),
            Type::Union(members) => members
                .iter()
                .map(|m| self.type_label(m))
                .collect::<Vec<_>>()
                .join("Or"),
            Type::Nullable(t) => self.type_label(t),
        }
    }

    /// Records the auxiliary types a (resolved) type needs.
    fn note(&mut self, ty: &Type) {
        match ty {
            Type::Named(n) => match self.classify(n) {
                Named::Dictionary => {
                    if self.dictionaries.insert(n.clone()) {
                        let mut chain = Vec::new();
                        let mut cur = Some(n.clone());
                        while let Some(name) = cur {
                            let Some(d) = self.idl.dictionaries.get(&name) else {
                                break;
                            };
                            chain.push(d.clone());
                            cur = d.parent.clone();
                        }
                        for d in chain {
                            for m in &d.members {
                                let resolved = self.idl.resolve(&m.ty);
                                if let Some(pruned) = self.prune(&resolved) {
                                    self.note(&pruned);
                                }
                            }
                        }
                    }
                }
                Named::Enum => {
                    self.enums.insert(n.clone());
                }
                _ => {}
            },
            Type::Nullable(t) | Type::Sequence(t) | Type::FrozenArray(t) | Type::Promise(t) => {
                self.note(t)
            }
            Type::Record(k, v) => {
                self.note(k);
                self.note(v);
            }
            Type::Union(members) => {
                let name = self.type_label(ty);
                for m in members {
                    self.note(m);
                }
                self.unions.entry(name.clone()).or_insert(UnionDef {
                    name,
                    members: members.clone(),
                });
            }
            _ => {}
        }
    }

    /// Drops the members of unions that cannot be converted (interfaces
    /// without bindings), so that the rest of the union stays usable.
    /// `None` if nothing convertible is left.
    pub fn prune(&self, ty: &Type) -> Option<Type> {
        match ty {
            Type::Union(members) => {
                let mut kept: Vec<Type> = members.iter().filter_map(|m| self.prune(m)).collect();
                match kept.len() {
                    0 => None,
                    1 => kept.pop(),
                    _ => Some(Type::Union(kept)),
                }
            }
            Type::Nullable(inner) => self.prune(inner).map(|t| t.nullable(true)),
            Type::Sequence(inner) => self.prune(inner).map(|t| Type::Sequence(Box::new(t))),
            Type::FrozenArray(inner) => self.prune(inner).map(|t| Type::FrozenArray(Box::new(t))),
            Type::Record(key, value) => {
                Some(Type::Record(key.clone(), Box::new(self.prune(value)?)))
            }
            // A promise is passed along whatever it resolves to.
            Type::Promise(inner) => Some(Type::Promise(Box::new(
                self.prune(inner).unwrap_or(Type::Any),
            ))),
            other => self.supported(other).then(|| other.clone()),
        }
    }

    fn arg(&mut self, a: &Argument) -> Option<PArg> {
        let ty = self.prune(&self.idl.resolve(&a.ty))?;
        self.note(&ty);
        Some(PArg {
            name: a.name.clone(),
            rust: snake(&a.name),
            ty,
            null_to_empty: a.type_ext.has("LegacyNullToEmptyString"),
            optional: a.optional,
            variadic: a.variadic,
            default: a.default.clone(),
        })
    }

    fn overload(&mut self, rust: String, args: &[Argument], ret: &Type) -> Option<POverload> {
        let ret = self.prune(&self.idl.resolve(ret))?;
        self.note(&ret);
        let mut out = Vec::new();
        for a in args {
            match self.arg(a) {
                Some(a) => out.push(a),
                // An unsupported trailing optional argument is dropped; an
                // unsupported required one makes the overload unusable.
                None if a.optional => break,
                None => return None,
            }
        }
        Some(POverload {
            rust,
            args: out,
            ret,
        })
    }

    fn reflect(&self, a: &Attribute, ty: &Type) -> Option<Reflect> {
        let ext = &a.ext;
        let is_reflect = ext.has("Reflect")
            || ext.has("ReflectURL")
            || ext.has("ReflectNonNegative")
            || ext.has("ReflectPositive")
            || ext.has("ReflectPositiveWithFallback")
            || ext.has("ReflectSetter");
        if !is_reflect {
            return None;
        }
        let attr = ext
            .value_str("Reflect")
            .or_else(|| ext.value_str("ReflectURL"))
            .or_else(|| ext.value_str("ReflectSetter"))
            .map(str::to_string)
            .unwrap_or_else(|| a.name.to_ascii_lowercase());
        let limit = if ext.has("ReflectNonNegative") {
            "non_negative"
        } else if ext.has("ReflectPositiveWithFallback") {
            "positive_with_fallback"
        } else if ext.has("ReflectPositive") {
            "positive"
        } else {
            "none"
        };
        // `[ReflectSetter]` on a URL-valued attribute (`a.href`): the setter
        // writes the content attribute and the getter reads it back resolved.
        let setter_only = ext.has("ReflectSetter") && !ext.has("Reflect") && !ext.has("ReflectURL");
        let kind = match ty {
            _ if ext.has("ReflectURL") => ReflectKind::Url,
            Type::UsvString if setter_only => ReflectKind::Url,
            Type::DomString | Type::UsvString => ReflectKind::String,
            Type::Nullable(inner) if matches!(**inner, Type::DomString | Type::UsvString) => {
                ReflectKind::NullableString
            }
            Type::Boolean => ReflectKind::Bool,
            Type::Long => ReflectKind::Long,
            Type::UnsignedLong => ReflectKind::UnsignedLong,
            Type::Double => ReflectKind::Double,
            Type::Named(n) if n == "DOMTokenList" && self.planned.contains("DOMTokenList") => {
                ReflectKind::TokenList
            }
            // Enumerated attributes are approximated by plain string reflection.
            Type::Named(n) if self.idl.enums.contains_key(n) => return None,
            _ => return None,
        };
        Some(Reflect {
            attr,
            kind,
            default: ext.value_str("ReflectDefault").map(str::to_string),
            limit,
        })
    }

    /// Plans the members declared by one interface, mixin or namespace.
    /// Returns attributes, operations and the trait members they need.
    #[allow(clippy::type_complexity)]
    fn members(
        &mut self,
        owner: &str,
        cfg: Option<&MemberCfg>,
    ) -> Result<(
        Vec<PAttr>,
        Vec<POp>,
        Vec<TraitMember>,
        Vec<(String, ConstValue)>,
    )> {
        let Some(def) = self.idl.interfaces.get(owner) else {
            bail!("manifest names `{owner}`, which is not defined in the IDL corpus");
        };
        let empty = MemberCfg::default();
        let cfg = cfg.unwrap_or(&empty);
        let native: BTreeSet<&str> = cfg.members.iter().map(String::as_str).collect();
        let stubs: BTreeSet<&str> = cfg.stubs.iter().map(String::as_str).collect();
        let mut seen: BTreeSet<&str> = BTreeSet::new();

        let mut attrs = Vec::new();
        let mut ops: Vec<POp> = Vec::new();
        let mut traits = Vec::new();
        let mut consts = Vec::new();

        let members = def.members.clone();
        for m in &members {
            match m {
                Member::Const { name, value, .. } => consts.push((name.clone(), value.clone())),
                Member::Attribute(a) => {
                    let resolved = self.idl.resolve(&a.ty);
                    let ty = self.prune(&resolved).unwrap_or(resolved);
                    let rust = snake(&a.name);
                    let kind = if native.contains(a.name.as_str()) {
                        if !self.supported(&ty) {
                            bail!("{owner}.{}: type {:?} is not supported", a.name, a.ty);
                        }
                        AttrKind::Native
                    } else if stubs.contains(a.name.as_str()) {
                        AttrKind::Stub
                    } else if is_event_handler_type(self.idl, &a.ty) {
                        AttrKind::EventHandler
                    } else if let Some(r) = self.reflect(a, &ty) {
                        AttrKind::Reflect(r)
                    } else {
                        continue;
                    };
                    seen.insert(native.get(a.name.as_str()).copied().unwrap_or(""));
                    seen.insert(stubs.get(a.name.as_str()).copied().unwrap_or(""));
                    if kind == AttrKind::Native {
                        self.note(&ty);
                        traits.push(TraitMember::Getter {
                            rust: rust.clone(),
                            ty: ty.clone(),
                            is_static: a.is_static,
                        });
                        if !a.readonly {
                            traits.push(TraitMember::Setter {
                                rust: rust.clone(),
                                ty: ty.clone(),
                                is_static: a.is_static,
                            });
                        }
                    }
                    attrs.push(PAttr {
                        name: a.name.clone(),
                        rust,
                        ty,
                        null_to_empty: a.type_ext.has("LegacyNullToEmptyString"),
                        readonly: a.readonly,
                        is_static: a.is_static,
                        kind,
                        same_object: a.ext.has("SameObject"),
                        put_forwards: a.ext.value_str("PutForwards").map(str::to_string),
                        replaceable: a.ext.has("Replaceable"),
                        stringifier: a.stringifier,
                        undefined_when_absent: matches!(&a.ty, Type::Union(ms)
                            if ms.iter().any(|m| matches!(m, Type::Named(n) if n == "undefined"))),
                        owner: owner.to_string(),
                    });
                }
                Member::Operation(o) => {
                    let Some(name) = &o.name else { continue };
                    let is_native = native.contains(name.as_str());
                    let is_stub = stubs.contains(name.as_str());
                    if !is_native && !is_stub {
                        continue;
                    }
                    seen.insert(native.get(name.as_str()).copied().unwrap_or(""));
                    seen.insert(stubs.get(name.as_str()).copied().unwrap_or(""));
                    let existing = ops.iter().position(|p| p.name == *name);
                    let index = existing.map(|i| ops[i].overloads.len()).unwrap_or(0);
                    let rust = if index == 0 {
                        snake(name)
                    } else {
                        format!(
                            "{}_overload{}",
                            snake(name).trim_end_matches('_'),
                            index + 1
                        )
                    };
                    let Some(overload) = self.overload(rust, &o.args, &o.ret) else {
                        if is_native && existing.is_none() {
                            eprintln!(
                                "bindgen: skipping {owner}.{name} overload with unsupported types"
                            );
                        }
                        continue;
                    };
                    if is_native {
                        traits.push(TraitMember::Op {
                            overload: overload.clone(),
                            is_static: o.is_static,
                        });
                    }
                    match existing {
                        Some(i) => ops[i].overloads.push(overload),
                        None => ops.push(POp {
                            name: name.clone(),
                            overloads: vec![overload],
                            is_static: o.is_static,
                            stub: is_stub && !is_native,
                            owner: owner.to_string(),
                        }),
                    }
                }
                _ => {}
            }
        }
        for name in native.iter().chain(stubs.iter()) {
            let found =
                attrs.iter().any(|a| a.name == *name) || ops.iter().any(|o| o.name == *name);
            if !found {
                bail!(
                    "manifest lists `{owner}.{name}`, which the IDL does not define (or whose types are unsupported)"
                );
            }
        }
        Ok((attrs, ops, traits, consts))
    }

    fn mixin_handle(&self, mixin: &str) -> Option<Handle> {
        let mut handle = None;
        for (iface, m) in &self.idl.includes {
            if m != mixin || !self.planned.contains(iface) {
                continue;
            }
            let h = self.interface_handle(iface);
            match handle {
                None => handle = Some(h),
                Some(existing) if existing != h => return None,
                _ => {}
            }
        }
        handle
    }

    fn interface_handle(&self, name: &str) -> Handle {
        let global = self
            .idl
            .interfaces
            .get(name)
            .is_some_and(|i| i.ext.has("Global"));
        if global || name == "Window" {
            Handle::Window
        } else if name == "EventTarget" {
            Handle::EventTarget
        } else if self.idl.inherits(name, "Node") {
            Handle::Node
        } else {
            Handle::Object
        }
    }

    pub fn plan(mut self) -> Result<Plan> {
        let mut plan = Plan {
            tags: self.manifest.tags.clone(),
            ..Plan::default()
        };

        // Mixins first: their members are copied into including interfaces.
        let mut mixin_members: BTreeMap<String, (Vec<PAttr>, Vec<POp>)> = BTreeMap::new();
        let all_mixins: BTreeSet<String> = self
            .idl
            .includes
            .iter()
            .filter(|(i, _)| self.planned.contains(i))
            .map(|(_, m)| m.clone())
            .collect();
        for mixin in &all_mixins {
            if !self.idl.interfaces.contains_key(mixin) {
                continue;
            }
            let cfg = self.manifest.mixins.get(mixin);
            let (attrs, ops, traits, _) = self.members(mixin, cfg)?;
            if !traits.is_empty() {
                let Some(handle) = self.mixin_handle(mixin) else {
                    bail!("mixin `{mixin}` is included by interfaces with different handle kinds");
                };
                plan.mixins.push(PInterface {
                    name: mixin.clone(),
                    parent: None,
                    handle,
                    kind: InterfaceKind::Mixin,
                    global: false,
                    constructor: None,
                    consts: Vec::new(),
                    attrs: Vec::new(),
                    ops: Vec::new(),
                    exotic: None,
                    iterable: None,
                    stringifier: None,
                    trait_members: traits,
                });
            }
            mixin_members.insert(mixin.clone(), (attrs, ops));
        }
        for name in self.manifest.mixins.keys() {
            if !all_mixins.contains(name) {
                bail!("manifest mixin `{name}` is not included by any planned interface");
            }
        }

        // Interfaces, parents before children.
        let mut order: Vec<String> = Vec::new();
        let mut pending: Vec<String> = self.planned.iter().cloned().collect();
        while !pending.is_empty() {
            let before = pending.len();
            pending.retain(|name| {
                let parent = self.planned_parent(name);
                if parent.as_ref().is_none_or(|p| order.contains(p)) {
                    order.push(name.clone());
                    false
                } else {
                    true
                }
            });
            if pending.len() == before {
                bail!("inheritance cycle among {pending:?}");
            }
        }

        for name in &order {
            let Some(def) = self.idl.interfaces.get(name).cloned() else {
                bail!("manifest names interface `{name}`, which is not in the IDL corpus");
            };
            let cfg = &self.manifest.interfaces[name];
            let handle = self.interface_handle(name);
            let (mut attrs, mut ops, mut traits, consts) = self.members(name, Some(cfg))?;

            let mixins: Vec<String> = self.idl.mixins_of(name).map(str::to_string).collect();
            for mixin in mixins {
                if let Some((m_attrs, m_ops)) = mixin_members.get(&mixin) {
                    attrs.extend(m_attrs.iter().cloned());
                    ops.extend(m_ops.iter().cloned());
                }
            }

            // Constructors.
            let mut constructor = None;
            if cfg.constructor {
                let mut overloads = Vec::new();
                for m in &def.members {
                    if let Member::Constructor { args, .. } = m {
                        let rust = if overloads.is_empty() {
                            "constructor".to_string()
                        } else {
                            format!("constructor_overload{}", overloads.len() + 1)
                        };
                        let ret = Type::Named(name.clone());
                        if let Some(o) = self.overload(rust, args, &ret) {
                            traits.push(TraitMember::Constructor {
                                overload: o.clone(),
                            });
                            overloads.push(o);
                        }
                    }
                }
                if overloads.is_empty() {
                    bail!("`{name}` is marked constructible but declares no usable constructor");
                }
                constructor = Some(overloads);
            }

            // Special operations, iterables and stringifiers.
            let mut exotic = None;
            if cfg.exotic {
                let mut e = Exotic::default();
                // Special operations are inherited: look at the interface,
                // then at its ancestors that are exotic too, nearest first.
                let mut chain = vec![name.clone()];
                let mut cur = def.parent.clone();
                while let Some(p) = cur {
                    cur = self.idl.interfaces.get(&p).and_then(|i| i.parent.clone());
                    if self.manifest.interfaces.get(&p).is_some_and(|c| c.exotic) {
                        chain.push(p);
                    }
                }
                for owner in &chain {
                    let Some(owner_def) = self.idl.interfaces.get(owner).cloned() else {
                        continue;
                    };
                    let own = owner == name;
                    for m in &owner_def.members {
                        let Member::Operation(Operation {
                            special: Some(special),
                            args,
                            ret,
                            ..
                        }) = m
                        else {
                            continue;
                        };
                        let ret = self.idl.resolve(ret);
                        let first = args.first().map(|a| self.idl.resolve(&a.ty));
                        let indexed = matches!(first, Some(Type::UnsignedLong));
                        match (special, indexed) {
                            (Special::Getter, true) if e.indexed_getter.is_none() => {
                                self.note(&ret);
                                if own {
                                    traits.push(TraitMember::IndexedGet { ty: ret.clone() });
                                }
                                e.indexed_getter = Some(PSpecial {
                                    rust: "indexed_get".into(),
                                    ty: ret,
                                    owner: owner.clone(),
                                    null_to_empty: false,
                                });
                            }
                            (Special::Getter, false) if e.named_getter.is_none() => {
                                self.note(&ret);
                                if own {
                                    traits.push(TraitMember::NamedGet { ty: ret.clone() });
                                    traits.push(TraitMember::NamedProperties);
                                }
                                e.named_getter = Some(PSpecial {
                                    rust: "named_get".into(),
                                    ty: ret,
                                    owner: owner.clone(),
                                    null_to_empty: false,
                                });
                            }
                            (Special::Setter, false) if e.named_setter.is_none() => {
                                let value = args
                                    .get(1)
                                    .map(|a| self.idl.resolve(&a.ty))
                                    .unwrap_or(Type::DomString);
                                self.note(&value);
                                if own {
                                    traits.push(TraitMember::NamedSet { ty: value.clone() });
                                }
                                e.named_setter = Some(PSpecial {
                                    rust: "named_set".into(),
                                    ty: value,
                                    owner: owner.clone(),
                                    null_to_empty: args
                                        .get(1)
                                        .is_some_and(|a| a.type_ext.has("LegacyNullToEmptyString")),
                                });
                            }
                            (Special::Deleter, false) if e.named_deleter.is_none() => {
                                if own {
                                    traits.push(TraitMember::NamedDelete);
                                }
                                e.named_deleter = Some(PSpecial {
                                    rust: "named_delete".into(),
                                    ty: Type::Undefined,
                                    owner: owner.clone(),
                                    null_to_empty: false,
                                });
                            }
                            _ => {}
                        }
                    }
                }
                exotic = Some(e);
            }

            let mut iterable = None;
            let mut stringifier = None;
            for m in &def.members {
                match m {
                    Member::Iterable { key, value } => {
                        let value = self.idl.resolve(value);
                        let key = key.as_ref().map(|k| self.idl.resolve(k));
                        if let Some(k) = &key {
                            self.note(k);
                            self.note(&value);
                            traits.push(TraitMember::Iterate {
                                key: k.clone(),
                                value: value.clone(),
                            });
                        }
                        iterable = Some((key, value));
                    }
                    Member::Stringifier => {
                        traits.push(TraitMember::Stringify);
                        stringifier = Some(Stringifier::Method);
                    }
                    _ => {}
                }
            }
            if let Some(a) = attrs.iter().find(|a| a.stringifier) {
                stringifier = Some(Stringifier::Attribute(a.name.clone()));
            }
            if let Some(o) = ops.iter().find(|o| {
                def.members.iter().any(|m| matches!(m, Member::Operation(op) if op.stringifier && op.name.as_deref() == Some(&o.name)))
            }) {
                stringifier = Some(Stringifier::Attribute(o.name.clone()));
            }

            plan.interfaces.push(PInterface {
                name: name.clone(),
                parent: self.planned_parent(name),
                handle,
                kind: InterfaceKind::Interface,
                global: def.ext.has("Global"),
                constructor,
                consts,
                attrs,
                ops,
                exotic,
                iterable,
                stringifier,
                trait_members: traits,
            });
        }

        // Namespaces.
        for (name, cfg) in &self.manifest.namespaces {
            let (attrs, ops, traits, consts) = self.members(name, Some(cfg))?;
            plan.namespaces.push(PInterface {
                name: name.clone(),
                parent: None,
                handle: Handle::Window,
                kind: InterfaceKind::Namespace,
                global: false,
                constructor: None,
                consts,
                attrs,
                ops,
                exotic: None,
                iterable: None,
                stringifier: None,
                trait_members: traits,
            });
        }

        plan.dictionaries = self.dictionaries.iter().cloned().collect();
        plan.enums = self.enums.iter().cloned().collect();
        plan.unions = self.unions.values().cloned().collect();
        Ok(plan)
    }

    fn planned_parent(&self, name: &str) -> Option<String> {
        let mut cur = self.idl.interfaces.get(name)?.parent.clone();
        while let Some(p) = cur {
            if self.planned.contains(&p) {
                return Some(p);
            }
            cur = self.idl.interfaces.get(&p)?.parent.clone();
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Idl {
        let mut idl = Idl::new();
        idl.load(
            "t.idl",
            r#"
            [Exposed=Window] interface EventTarget {
              constructor();
              undefined addEventListener(DOMString type, EventListener? callback, optional (AddEventListenerOptions or boolean) options = {});
            };
            callback interface EventListener { undefined handleEvent(Event event); };
            dictionary EventListenerOptions { boolean capture = false; };
            dictionary AddEventListenerOptions : EventListenerOptions { boolean once = false; };
            [Exposed=Window] interface Event { readonly attribute DOMString type; };
            [Exposed=Window] interface Node : EventTarget {
              const unsigned short ELEMENT_NODE = 1;
              readonly attribute Node? parentNode;
              Node appendChild(Node node);
              undefined normalize();
            };
            [Exposed=Window] interface Element : Node {
              [CEReactions, Reflect] attribute DOMString id;
              [CEReactions, Reflect="class"] attribute DOMString className;
              attribute EventHandler onclick;
              readonly attribute Unplanned thing;
            };
            interface mixin ParentNode { Element? querySelector(DOMString selectors); };
            Element includes ParentNode;
            typedef EventHandlerNonNull? EventHandler;
            callback EventHandlerNonNull = any (Event event);
            "#,
        )
        .unwrap();
        idl
    }

    #[test]
    fn plans_native_reflect_and_handler_members() {
        let idl = corpus();
        let manifest = Manifest::parse(
            r#"
            [interfaces.EventTarget]
            members = ["addEventListener"]
            constructor = true
            [interfaces.Event]
            members = ["type"]
            [interfaces.Node]
            members = ["parentNode", "appendChild"]
            stubs = ["normalize"]
            [interfaces.Element]
            [mixins.ParentNode]
            members = ["querySelector"]
            "#,
        )
        .unwrap();
        let plan = Planner::new(&idl, &manifest).plan().unwrap();
        let names: Vec<_> = plan.interfaces.iter().map(|i| i.name.as_str()).collect();
        let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
        assert!(pos("EventTarget") < pos("Node") && pos("Node") < pos("Element"));

        let element = &plan.interfaces[pos("Element")];
        assert_eq!(element.handle, Handle::Node);
        assert_eq!(element.parent.as_deref(), Some("Node"));
        let kinds: Vec<_> = element
            .attrs
            .iter()
            .map(|a| (a.name.as_str(), &a.kind))
            .collect();
        assert!(matches!(kinds[0], ("id", AttrKind::Reflect(r)) if r.attr == "id"));
        assert!(matches!(kinds[1], ("className", AttrKind::Reflect(r)) if r.attr == "class"));
        assert!(matches!(kinds[2], ("onclick", AttrKind::EventHandler)));
        assert_eq!(
            kinds.len(),
            3,
            "the attribute of an unplanned type is omitted"
        );
        assert_eq!(element.ops[0].name, "querySelector");
        assert_eq!(element.ops[0].owner, "ParentNode");

        let node = &plan.interfaces[pos("Node")];
        assert!(node.ops.iter().any(|o| o.name == "normalize" && o.stub));
        assert_eq!(node.consts.len(), 1);
        assert_eq!(plan.mixins.len(), 1);
        assert_eq!(plan.mixins[0].handle, Handle::Node);

        let et = &plan.interfaces[pos("EventTarget")];
        assert!(et.constructor.is_some());
        assert!(
            plan.dictionaries
                .contains(&"AddEventListenerOptions".to_string())
        );
        assert!(
            plan.unions
                .iter()
                .any(|u| u.name == "AddEventListenerOptionsOrBoolean")
        );
    }

    #[test]
    fn rejects_unknown_manifest_members() {
        let idl = corpus();
        let manifest =
            Manifest::parse("[interfaces.Node]\nmembers = [\"nope\"]\n[interfaces.EventTarget]\n")
                .unwrap();
        assert!(Planner::new(&idl, &manifest).plan().is_err());
    }
}
