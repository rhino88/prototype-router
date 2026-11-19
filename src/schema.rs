//! The parts of a Federation v2 supergraph (join spec v0.3) the planner and
//! the executor need: which subgraph can resolve which field of which type,
//! with what keys and prerequisites, and what the client is allowed to see.

use async_graphql_parser::{
    parse_schema,
    types::{BaseType, ConstDirective, Type, TypeKind, TypeSystemDefinition},
    Positioned,
};
use async_graphql_value::ConstValue;
use std::hash::{BuildHasherDefault, Hasher};

/// FNV-1a. The maps here are keyed by short names and are looked up for every
/// field of every operation, where the standard library's hasher is most of
/// the planning time.
#[derive(Clone, Copy)]
pub struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Fnv(0xcbf29ce484222325)
    }
}

impl Hasher for Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 ^ *byte as u64).wrapping_mul(0x100000001b3);
        }
    }

    fn write_usize(&mut self, value: usize) {
        self.0 = (self.0 ^ value as u64).wrapping_mul(0x100000001b3);
    }

    fn finish(&self) -> u64 {
        // The low bits pick the bucket: fold the well-mixed high bits in.
        self.0 ^ (self.0 >> 29)
    }
}

pub type HashMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<Fnv>>;
pub type HashSet<K> = std::collections::HashSet<K, BuildHasherDefault<Fnv>>;
pub type IndexMap<K, V> = indexmap::IndexMap<K, V, BuildHasherDefault<Fnv>>;

pub type GraphId = usize;

pub struct Subgraph {
    pub name: String,
    pub url: String,
}

/// One field of a `@key`, `@requires` or `@provides` selection. Fragments are
/// flattened: a field under `... on T` carries `T` in `conds`.
#[derive(Debug)]
pub struct FsField {
    pub name: String,
    /// Arguments as written, `(currency: "USD")`, or empty.
    pub args: String,
    pub conds: Vec<String>,
    pub sub: FieldSet,
}

pub type FieldSet = Vec<FsField>;

pub struct Key {
    /// Top-level field names.
    pub names: Vec<String>,
    pub fs: FieldSet,
}

#[derive(Default)]
pub struct FieldJoin {
    pub external: bool,
    /// Overridden by another subgraph, kept only because that subgraph's
    /// `@requires` or `@provides` mention it.
    pub used_overridden: bool,
    pub requires: FieldSet,
    pub requires_names: Vec<String>,
    pub requires_raw: String,
    pub provides: FieldSet,
    /// Named type of the field in this subgraph when it differs from the
    /// supergraph's (a union member or an implementation).
    pub ty: Option<String>,
    /// The same with its list and non-null wrappers.
    pub ty_raw: Option<String>,
}

pub struct ArgInfo {
    /// Named type, wrappers removed.
    pub ty: String,
    pub inaccessible: bool,
}

pub struct FieldInfo {
    /// Named type the field returns, with list and non-null wrappers removed.
    pub ret: String,
    pub ty: Type,
    pub graphs: HashMap<GraphId, FieldJoin>,
    pub args: HashMap<String, ArgInfo>,
    pub inaccessible: bool,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Kind {
    Object,
    Interface,
    Union,
    Enum,
    Input,
    Other,
}

pub struct TypeInfo {
    pub kind: Kind,
    /// Resolvable keys per subgraph that defines the type. A subgraph that only
    /// references the type (`resolvable: false`) is present with no keys.
    pub graphs: HashMap<GraphId, Vec<Key>>,
    /// Top-level fields of every key, resolvable or not: (subgraph, field).
    pub key_names: HashSet<(GraphId, String)>,
    /// Interfaces: subgraphs where it is an `@interfaceObject`, that is, an
    /// object type standing in for every implementation.
    pub iobj: HashSet<GraphId>,
    pub fields: HashMap<String, FieldInfo>,
    /// The interfaces it implements, per subgraph.
    pub implements: HashMap<String, HashSet<GraphId>>,
    /// Unions: (subgraph, member).
    pub members: HashSet<(GraphId, String)>,
    /// Abstract types: every object type it can resolve to.
    pub possible: Vec<String>,
    pub inaccessible: bool,
    /// Enums: value -> inaccessible.
    pub values: HashMap<String, bool>,
    /// Input objects: field -> type.
    pub inputs: HashMap<String, ArgInfo>,
}

pub struct Schema {
    pub subgraphs: Vec<Subgraph>,
    pub types: HashMap<String, TypeInfo>,
}

fn string_arg(directive: &ConstDirective, name: &str) -> Option<String> {
    match directive.get_argument(name).map(|v| &v.node) {
        Some(ConstValue::String(s)) => Some(s.clone()),
        Some(ConstValue::Enum(e)) => Some(e.to_string()),
        _ => None,
    }
}

fn bool_arg(directive: &ConstDirective, name: &str) -> Option<bool> {
    match directive.get_argument(name).map(|v| &v.node) {
        Some(ConstValue::Boolean(b)) => Some(*b),
        _ => None,
    }
}

pub fn parse_field_set(raw: &str) -> FieldSet {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    parse_selection(&chars, &mut at, &[], &mut out);
    out
}

fn skip_ignored(chars: &[char], at: &mut usize) {
    while *at < chars.len() && (chars[*at].is_whitespace() || chars[*at] == ',') {
        *at += 1;
    }
}

fn read_name(chars: &[char], at: &mut usize) -> String {
    let start = *at;
    while *at < chars.len() && (chars[*at].is_alphanumeric() || chars[*at] == '_') {
        *at += 1;
    }
    chars[start..*at].iter().collect()
}

fn parse_selection(chars: &[char], at: &mut usize, conds: &[String], out: &mut FieldSet) {
    loop {
        skip_ignored(chars, at);
        if *at >= chars.len() {
            return;
        }
        if chars[*at] == '}' {
            *at += 1;
            return;
        }
        if chars[*at] == '.' {
            while *at < chars.len() && chars[*at] == '.' {
                *at += 1;
            }
            skip_ignored(chars, at);
            let mut inner = conds.to_vec();
            if read_name(chars, at) == "on" {
                skip_ignored(chars, at);
                inner.push(read_name(chars, at));
                skip_ignored(chars, at);
            }
            if *at < chars.len() && chars[*at] == '{' {
                *at += 1;
                parse_selection(chars, at, &inner, out);
            }
            continue;
        }
        let name = read_name(chars, at);
        if name.is_empty() {
            *at += 1;
            continue;
        }
        skip_ignored(chars, at);
        let mut args = String::new();
        if *at < chars.len() && chars[*at] == '(' {
            let start = *at;
            let mut depth = 0;
            let mut in_string = false;
            while *at < chars.len() {
                let c = chars[*at];
                if in_string {
                    if c == '\\' {
                        *at += 1;
                    } else if c == '"' {
                        in_string = false;
                    }
                } else if c == '"' {
                    in_string = true;
                } else if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth -= 1;
                    if depth == 0 {
                        *at += 1;
                        break;
                    }
                }
                *at += 1;
            }
            args = chars[start..(*at).min(chars.len())].iter().collect();
            skip_ignored(chars, at);
        }
        let mut sub = Vec::new();
        if *at < chars.len() && chars[*at] == '{' {
            *at += 1;
            parse_selection(chars, at, &[], &mut sub);
        }
        out.push(FsField {
            name,
            args,
            conds: conds.to_vec(),
            sub,
        });
    }
}

fn named(ty: &Type) -> String {
    match &ty.base {
        BaseType::Named(name) => name.to_string(),
        BaseType::List(inner) => named(inner),
    }
}

fn directives<'a>(
    all: &'a [Positioned<ConstDirective>],
    name: &'a str,
) -> impl Iterator<Item = &'a ConstDirective> {
    all.iter()
        .map(|d| &d.node)
        .filter(move |d| d.name.node == name)
}

fn inaccessible(all: &[Positioned<ConstDirective>]) -> bool {
    directives(all, "inaccessible").next().is_some()
}

impl Schema {
    pub fn parse(sdl: &str) -> Schema {
        let document = parse_schema(sdl).expect("invalid supergraph SDL");
        let definitions: Vec<_> = document
            .definitions
            .iter()
            .filter_map(|d| match d {
                TypeSystemDefinition::Type(t) => Some(&t.node),
                _ => None,
            })
            .collect();

        let mut subgraphs = Vec::new();
        let mut graph_ids: HashMap<String, GraphId> = HashMap::default();
        for def in &definitions {
            if def.name.node != "join__Graph" {
                continue;
            }
            if let TypeKind::Enum(e) = &def.kind {
                for value in &e.values {
                    let Some(d) = directives(&value.node.directives, "join__graph").next() else {
                        continue;
                    };
                    graph_ids.insert(value.node.value.node.to_string(), subgraphs.len());
                    subgraphs.push(Subgraph {
                        name: string_arg(d, "name").unwrap(),
                        url: string_arg(d, "url").unwrap(),
                    });
                }
            }
        }
        let graph = |d: &ConstDirective| string_arg(d, "graph").and_then(|g| graph_ids.get(&g).copied());

        let mut types: HashMap<String, TypeInfo> = HashMap::default();
        for def in &definitions {
            let name = def.name.node.to_string();
            let (kind, field_defs) = match &def.kind {
                TypeKind::Object(o) => (Kind::Object, Some(&o.fields)),
                TypeKind::Interface(i) => (Kind::Interface, Some(&i.fields)),
                TypeKind::Union(_) => (Kind::Union, None),
                TypeKind::Enum(_) => (Kind::Enum, None),
                TypeKind::InputObject(_) => (Kind::Input, None),
                _ => (Kind::Other, None),
            };

            let mut graphs: HashMap<GraphId, Vec<Key>> = HashMap::default();
            let mut iobj = HashSet::default();
            let mut key_names = HashSet::default();
            for d in directives(&def.directives, "join__type") {
                let Some(g) = graph(d) else { continue };
                let keys = graphs.entry(g).or_default();
                if bool_arg(d, "isInterfaceObject") == Some(true) {
                    iobj.insert(g);
                }
                let key = string_arg(d, "key").map(|key| parse_field_set(&key));
                for field in key.iter().flatten() {
                    key_names.insert((g, field.name.clone()));
                }
                if bool_arg(d, "resolvable") == Some(false) {
                    continue;
                }
                if let Some(fs) = key {
                    keys.push(Key {
                        names: fs.iter().map(|f| f.name.clone()).collect(),
                        fs,
                    });
                }
            }

            let mut implements: HashMap<String, HashSet<GraphId>> = HashMap::default();
            for d in directives(&def.directives, "join__implements") {
                if let (Some(g), Some(interface)) = (graph(d), string_arg(d, "interface")) {
                    implements.entry(interface).or_default().insert(g);
                }
            }

            let mut members = HashSet::default();
            for d in directives(&def.directives, "join__unionMember") {
                if let (Some(g), Some(member)) = (graph(d), string_arg(d, "member")) {
                    members.insert((g, member));
                }
            }

            let mut values = HashMap::default();
            if let TypeKind::Enum(e) = &def.kind {
                for value in &e.values {
                    values.insert(
                        value.node.value.node.to_string(),
                        inaccessible(&value.node.directives),
                    );
                }
            }

            let mut inputs = HashMap::default();
            if let TypeKind::InputObject(input) = &def.kind {
                for field in &input.fields {
                    inputs.insert(
                        field.node.name.node.to_string(),
                        ArgInfo {
                            ty: named(&field.node.ty.node),
                            inaccessible: inaccessible(&field.node.directives),
                        },
                    );
                }
            }

            let mut fields = HashMap::default();
            for field in field_defs.into_iter().flatten() {
                let field = &field.node;
                let mut joins: HashMap<GraphId, FieldJoin> = HashMap::default();
                let mut any = false;
                for d in directives(&field.directives, "join__field") {
                    any = true;
                    let Some(g) = graph(d) else { continue };
                    let requires_raw = string_arg(d, "requires").unwrap_or_default();
                    let requires = parse_field_set(&requires_raw);
                    joins.insert(
                        g,
                        FieldJoin {
                            external: bool_arg(d, "external").unwrap_or(false),
                            used_overridden: bool_arg(d, "usedOverridden").unwrap_or(false),
                            requires_names: requires.iter().map(|f| f.name.clone()).collect(),
                            requires,
                            requires_raw: requires_raw.split_whitespace().collect::<Vec<_>>().join(" "),
                            provides: string_arg(d, "provides")
                                .map(|r| parse_field_set(&r))
                                .unwrap_or_default(),
                            ty: string_arg(d, "type").map(|t| {
                                t.chars().filter(|c| !matches!(c, '[' | ']' | '!')).collect()
                            }),
                            ty_raw: string_arg(d, "type"),
                        },
                    );
                }
                // No @join__field: every subgraph that defines the type has it.
                if !any {
                    for g in graphs.keys() {
                        joins.insert(*g, FieldJoin::default());
                    }
                }
                fields.insert(
                    field.name.node.to_string(),
                    FieldInfo {
                        ret: named(&field.ty.node),
                        ty: field.ty.node.clone(),
                        graphs: joins,
                        args: field
                            .arguments
                            .iter()
                            .map(|a| {
                                (
                                    a.node.name.node.to_string(),
                                    ArgInfo {
                                        ty: named(&a.node.ty.node),
                                        inaccessible: inaccessible(&a.node.directives),
                                    },
                                )
                            })
                            .collect(),
                        inaccessible: inaccessible(&field.directives),
                    },
                );
            }

            types.insert(
                name,
                TypeInfo {
                    kind,
                    graphs,
                    key_names,
                    iobj,
                    fields,
                    implements,
                    members,
                    possible: Vec::new(),
                    inaccessible: inaccessible(&def.directives),
                    values,
                    inputs,
                },
            );
        }

        let mut possible: HashMap<String, Vec<String>> = HashMap::default();
        let mut names: Vec<&String> = types.keys().collect();
        names.sort();
        for name in names {
            let info = &types[name];
            match info.kind {
                Kind::Object => {
                    for interface in info.implements.keys() {
                        possible.entry(interface.clone()).or_default().push(name.clone());
                    }
                }
                Kind::Union => {
                    let mut members: Vec<String> =
                        info.members.iter().map(|(_, m)| m.clone()).collect();
                    members.sort();
                    members.dedup();
                    possible.insert(name.clone(), members);
                }
                _ => {}
            }
        }
        for (name, list) in possible {
            if let Some(info) = types.get_mut(&name) {
                info.possible = list;
            }
        }

        Schema { subgraphs, types }
    }

    /// Object types a value of type `ty` can be.
    pub fn possible(&self, ty: &str) -> Vec<String> {
        match self.types.get(ty) {
            Some(info) if info.kind == Kind::Object => vec![ty.to_string()],
            Some(info) => info.possible.clone(),
            None => Vec::new(),
        }
    }

    pub fn is_composite(&self, ty: &str) -> bool {
        self.types
            .get(ty)
            .is_some_and(|t| matches!(t.kind, Kind::Object | Kind::Interface | Kind::Union))
    }

    /// Whether `graph` can return the object type `object` where `ty` is expected.
    pub fn possible_in(&self, graph: GraphId, ty: &str, object: &str) -> bool {
        let (Some(expected), Some(info)) = (self.types.get(ty), self.types.get(object)) else {
            return false;
        };
        if info.kind != Kind::Object || !info.graphs.contains_key(&graph) {
            return false;
        }
        match expected.kind {
            Kind::Object => ty == object,
            Kind::Interface => info.implements.get(ty).is_some_and(|g| g.contains(&graph)),
            Kind::Union => expected.members.contains(&(graph, object.to_string())),
            _ => false,
        }
    }

    /// What two fields under one response key have to agree on: the list and
    /// non-null wrappers and, for leaf types, the type.
    pub fn shape(&self, info: &FieldInfo, join: Option<&FieldJoin>) -> String {
        let raw = match join.and_then(|j| j.ty_raw.as_ref()) {
            Some(raw) => raw.clone(),
            None => info.ty.to_string(),
        };
        let name: String = raw.chars().filter(|c| !matches!(c, '[' | ']' | '!')).collect();
        if self.is_composite(&name) {
            raw.replace(&name, "{}")
        } else {
            raw
        }
    }

    /// Whether a value of type `ty` satisfies the type condition `cond`.
    pub fn subtype(&self, ty: &str, cond: &str) -> bool {
        if ty == cond {
            return true;
        }
        let Some(info) = self.types.get(ty) else {
            return false;
        };
        info.implements.contains_key(cond)
            || self
                .types
                .get(cond)
                .is_some_and(|c| c.kind == Kind::Union && c.possible.iter().any(|p| p == ty))
    }

    /// Whether a field under the type conditions `conds` applies to `object`.
    pub fn applies(&self, conds: &[String], object: &str) -> bool {
        conds.iter().all(|c| self.subtype(object, c))
    }

    /// Where the definition of `object.name` that the client means comes from,
    /// when the field was selected on the type `via`: the type to select it on
    /// in a subgraph, and its joins. That is `object` itself unless the field
    /// is contributed by an `@interfaceObject`.
    pub fn owner<'s>(&'s self, object: &str, name: &str, via: &str) -> Option<(&'s str, &'s FieldInfo)> {
        let (object_name, info) = self.types.get_key_value(object)?;
        if info.kind == Kind::Object && via != object {
            // Selected on an interface that only interface objects define this
            // field for: their value wins over an implementation's own field.
            if let Some((via_name, via_info)) = self.types.get_key_value(via) {
                if via_info.kind == Kind::Interface && !via_info.iobj.is_empty() {
                    if let Some(field) = via_info.fields.get(name) {
                        if !field.graphs.is_empty()
                            && field.graphs.keys().all(|g| via_info.iobj.contains(g))
                        {
                            return Some((via_name, field));
                        }
                    }
                }
            }
        }
        let field = info.fields.get(name)?;
        if field.graphs.is_empty() && info.kind == Kind::Object {
            // In name order, so the choice does not depend on the map.
            let mut interfaces: Vec<&String> = info.implements.keys().collect();
            interfaces.sort();
            for interface in interfaces {
                let Some((interface_name, interface_info)) = self.types.get_key_value(interface)
                else {
                    continue;
                };
                if let Some(inherited) = interface_info.fields.get(name) {
                    if inherited.graphs.keys().any(|g| interface_info.iobj.contains(g)) {
                        return Some((interface_name, inherited));
                    }
                }
            }
        }
        Some((object_name, field))
    }
}
