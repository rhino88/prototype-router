//! Query planner.
//!
//! Three passes:
//!   1. Greedy assignment. Walk the operation once and give every field to a
//!      subgraph: stay in the subgraph that produced the parent object if it
//!      can resolve the field, otherwise hop to the subgraph that can have it
//!      ready soonest (see `Routes`), preferring one we already fetch from at
//!      that place in the response. Hops for the same subgraph at the same
//!      path are one fetch node, whatever the entity types involved.
//!   2. Leveling. Each node gets the earliest and the latest step it can run at
//!      without lengthening the plan.
//!   3. Merging. Nodes for the same subgraph are moved within that window so
//!      they land on the same step, and every (subgraph, step) group is sent as
//!      a single request with one aliased `_entities` field per node, or
//!      per set of nodes for the same path that can share one (`can_share`).
//!      Paths whose fields come out the same, selection and representations,
//!      share one too, so an entity that is at several of them is asked for
//!      once.
//!
//! Subgraph queries carry no more than the executor needs: where a field's
//! type is an object type its values are not asked for `__typename` and need
//! no type condition, and a key or `@requires` field the client selected under
//! its own name is read from there instead of being selected again.
//!
//! Where pass 1 has more than one way to get a field:
//!   - A root field that several subgraphs resolve goes to the one that can
//!     answer most of what is selected under it without leaving.
//!   - A field no entity fetch can reach (the type has no usable key where the
//!     field lives) is fetched by selecting its parent field again in a
//!     subgraph that has both, going up as far as needed. That is only done
//!     when the subgraph resolves the field for every type it was selected on.
//!   - Objects that come from an `@interfaceObject` subgraph have no concrete
//!     type yet. If the selection depends on the type, a fetch to a subgraph
//!     that knows the implementations resolves `__typename` first.

use crate::schema::{
    FieldInfo, FieldJoin, FieldSet, FsField, GraphId, HashMap, HashSet, IndexMap, Key, Kind, Schema,
};
use async_graphql_parser::{
    parse_query,
    types::{
        Directive, ExecutableDocument, OperationType, Selection, SelectionSet, Type,
    },
    Positioned,
};
use async_graphql_value::Value as GqlValue;
use serde_json::{Map, Value};
use std::{
    collections::BTreeSet,
    rc::Rc,
    sync::{atomic::AtomicUsize, Arc, Mutex},
};

/// The operation with fragments inlined, `@skip` and `@include` applied, and
/// every selection set split by the type it applies to:
/// type -> response key -> field. The type is an object type, or an interface
/// that some subgraph has as an `@interfaceObject`, for objects whose concrete
/// type is not known.
pub type Expanded = IndexMap<String, IndexMap<String, EField>>;

#[derive(Clone)]
pub struct EField {
    pub key: String,
    /// The key as it is written in the response: quoted, with the colon.
    pub written: Vec<u8>,
    pub name: String,
    /// Printed arguments, `(first: 5)`, or empty.
    pub args: String,
    /// The type the field was selected on: the enclosing field's type or the
    /// innermost type condition.
    pub via: String,
    /// `None` for `__typename`.
    pub ty: Option<Type>,
    pub ret: String,
    /// `ret` is an object type, so that is what every value is and
    /// subgraphs are not asked for `__typename`.
    pub concrete: bool,
    pub sub: Option<Arc<Expanded>>,
}

/// Prefix of the response keys used for fields the plan needs but the client
/// did not ask for (keys and `@requires` inputs).
pub const HIDDEN: &str = "_k_";

fn written_key(key: &str) -> Vec<u8> {
    let mut written = serde_json::to_vec(key).unwrap_or_default();
    written.push(b':');
    written
}

fn hidden_key(name: &str, args: &str) -> String {
    if args.is_empty() {
        return format!("{}{}", HIDDEN, name);
    }
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in args.bytes() {
        hash = (hash ^ byte as u64).wrapping_mul(0x100000001b3);
    }
    format!("{}{}_{:x}", HIDDEN, name, hash)
}

/// One field of an entity representation, read from the response.
#[derive(Clone, PartialEq)]
pub struct RepField {
    /// Name in the representation.
    pub name: String,
    /// Response key it is stored under.
    pub key: String,
    /// Key fields: without a value there is no entity to look up.
    pub required: bool,
    /// The value can be of several types, so it is sent with `__typename`.
    pub abstract_type: bool,
    pub sub: Option<HashMap<String, Vec<RepField>>>,
}

#[derive(Clone, PartialEq)]
pub struct RepSpec {
    /// `__typename` to send.
    pub typename: String,
    pub fields: Vec<RepField>,
}

/// One field on the way to a place in the response.
#[derive(PartialEq)]
pub struct PathStep {
    /// Type of the objects the field is read from.
    pub parent: String,
    pub key: String,
    /// The objects can be of other types too, so their type has to be looked
    /// at.
    pub check: bool,
}

pub struct EntityPart {
    pub alias: String,
    /// Where the entities are in the response. Several places share one
    /// `_entities` field when they ask the same of the same kind of entity.
    pub paths: Vec<Vec<PathStep>>,
    /// The entities are all of the one type `reps` has, and are not asked
    /// for `__typename`.
    pub concrete: bool,
    /// What to send in the representation, per type of the object found.
    pub reps: HashMap<String, RepSpec>,
}

/// One request to one subgraph.
pub struct Group {
    pub subgraph: GraphId,
    /// Step the planner scheduled the request at (1-based).
    pub step: usize,
    /// Requests whose responses this one needs.
    pub deps: Vec<usize>,
    pub query: String,
    pub has_root: bool,
    pub entities: Vec<EntityPart>,
    /// Client variables the query uses.
    pub vars: Vec<String>,
    /// The query has aliases made by `alias_for`.
    pub aliased: bool,
    pub mutation: bool,
    /// Requests that wait for this one.
    pub dependents: Vec<usize>,
    /// Size of the last response, to read the next into a buffer that fits.
    pub response_size: AtomicUsize,
}

pub struct Plan {
    pub expanded: Expanded,
    /// `Query` or `Mutation`.
    pub root: &'static str,
    pub groups: Vec<Group>,
    /// Longest chain of requests that wait for one another.
    pub steps: usize,
    pub fetch_nodes: usize,
    /// No way of merging these fetch nodes needs fewer requests than this.
    pub request_bound: usize,
    /// Number of nodes the last execution's responses were parsed into and
    /// the size of what it returned: what the next one makes room for.
    pub nodes: AtomicUsize,
    pub response_size: AtomicUsize,
}

const UNREACHABLE: usize = usize::MAX / 4;

/// For one object type, as seen from a fetch to one subgraph: how many more
/// steps it takes until each field can be available. 0 means the fetch can
/// select the field itself. A field that needs an entity fetch costs one step
/// more than the slowest of the key and `@requires` fields that fetch needs,
/// taking the cheapest subgraph and key that can supply it.
///
/// This only depends on the schema, so it is computed once and shared by
/// every operation. It is used to rank the options, not to rule any out.
pub struct Routes {
    cost: HashMap<String, usize>,
}

#[derive(Default)]
pub struct RouteCache(Mutex<HashMap<(String, GraphId, Vec<String>), Arc<Routes>>>);

fn inline_in(schema: &Schema, graph: GraphId, provided: &[String], object: &str, field: &str) -> bool {
    if provided.iter().any(|p| p == field) {
        return true;
    }
    schema.types[object]
        .fields
        .get(field)
        .and_then(|f| f.graphs.get(&graph))
        .is_some_and(|j| !j.external && !j.used_overridden && j.requires.is_empty())
}

impl RouteCache {
    fn get(&self, schema: &Schema, object: &str, graph: GraphId, provided: &[String]) -> Arc<Routes> {
        let id = (object.to_string(), graph, provided.to_vec());
        if let Some(routes) = self.0.lock().unwrap().get(&id) {
            return routes.clone();
        }

        let info = &schema.types[object];
        let mut cost: HashMap<String, usize> = info
            .fields
            .keys()
            .map(|field| {
                let inline = inline_in(schema, graph, provided, object, field);
                (field.clone(), if inline { 0 } else { UNREACHABLE })
            })
            .collect();
        // Costs only ever go down, so this settles after as many rounds as the
        // longest chain of prerequisites.
        loop {
            let mut changed = false;
            for (field, field_info) in &info.fields {
                if cost[field] == 0 {
                    continue;
                }
                let mut best = cost[field];
                for (target, join) in &field_info.graphs {
                    if join.external || join.used_overridden {
                        continue;
                    }
                    for key in info.graphs.get(target).into_iter().flatten() {
                        if key.names.contains(field) {
                            continue;
                        }
                        let slowest = key
                            .names
                            .iter()
                            .chain(join.requires_names.iter())
                            .map(|n| cost.get(n).copied().unwrap_or(UNREACHABLE))
                            .max()
                            .unwrap_or(0);
                        if slowest < UNREACHABLE {
                            best = best.min(slowest + 1);
                        }
                    }
                }
                if best < cost[field] {
                    cost.insert(field.clone(), best);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let routes = Arc::new(Routes { cost });
        self.0.lock().unwrap().insert(id, routes.clone());
        routes
    }
}

type PathId = usize;
type NodeId = usize;
type SelId = usize;

#[derive(Default)]
struct Sel {
    /// Type condition -> fields.
    by_type: IndexMap<String, Vec<SField>>,
}

struct SField {
    key: String,
    /// Set when `key` cannot be used in the subgraph request, see `alias_for`.
    alias: Option<String>,
    name: String,
    args: String,
    /// Shape of the field's type in the subgraph: wrappers, and the name for
    /// leaf types.
    shape: String,
    /// The object type every value of the field has, if there is only one.
    exact: Option<String>,
    sub: Option<SelId>,
}

/// Alias for a field whose response key is taken, in the same selection, by a
/// field of another type condition with a different shape: GraphQL does not
/// allow both under one key. The executor turns it back into the key.
fn alias_for(condition: &str, key: &str) -> String {
    format!("_a{}_{}{}", condition.len(), condition, key)
}

/// The response key an alias made by `alias_for` stands for.
pub fn unalias(key: &str) -> &str {
    let Some(rest) = key.strip_prefix("_a") else {
        return key;
    };
    let Some((length, rest)) = rest.split_once('_') else {
        return key;
    };
    match length.parse::<usize>() {
        Ok(length) if rest.len() > length && rest.is_char_boundary(length) => &rest[length..],
        _ => key,
    }
}

struct Node {
    subgraph: GraphId,
    /// `None` for root fetches.
    path: Option<PathId>,
    sel: SelId,
    deps: BTreeSet<NodeId>,
    reps: IndexMap<String, RepSpec>,
    /// A fetch of mutation fields: never shares a request.
    mutation: bool,
}

/// A place in the response, seen from the fetch that selects it.
#[derive(Clone)]
struct Pos<'a> {
    node: NodeId,
    sel: SelId,
    path: PathId,
    /// Fields the enclosing fields `@provides` here.
    provided: Vec<&'a FsField>,
    /// Type of the objects here, in this subgraph.
    ret: &'a str,
    /// Set when that type is an `@interfaceObject` in this subgraph.
    iobj: Option<&'a str>,
    parent: Option<Rc<Parent<'a>>>,
}

/// The field whose value a `Pos` is.
struct Parent<'a> {
    pos: Pos<'a>,
    object: String,
    key: String,
    name: String,
    args: String,
    via: String,
    sub: Arc<Expanded>,
}

type Hidden = Rc<(Vec<EField>, Vec<RepField>)>;

struct Planner<'a> {
    schema: &'a Schema,
    routes: &'a RouteCache,
    root: &'static str,
    sels: Vec<Sel>,
    nodes: Vec<Node>,
    /// Interned response paths: (parent path, parent type, response key).
    paths: Vec<(PathId, String, String)>,
    path_ids: HashMap<(PathId, String, String), PathId>,
    roots: HashMap<GraphId, NodeId>,
    /// The last fetch of mutation fields: the next one waits for it.
    last_mutation: Option<NodeId>,
    entity_nodes: HashMap<(GraphId, PathId, String), NodeId>,
    /// Which node fetches a leaf field: (path, type, response key).
    providers: HashMap<(PathId, String, String), NodeId>,
    in_progress: HashSet<(PathId, String, String)>,
    /// Where the selection under a composite field was first placed, by the
    /// path of the field's value.
    composites: HashMap<PathId, Pos<'a>>,
    /// Every selection there is at a path, per subgraph.
    placements: HashMap<(PathId, GraphId), Pos<'a>>,
    /// The fetch that finds the concrete type of interface objects at a path.
    resolvers: HashMap<PathId, NodeId>,
    /// Fields and representations for a field set, per type it is read from.
    hidden: HashMap<usize, HashMap<String, Hidden>>,
    /// Some field had to be given an alias, see `alias_for`.
    aliased: bool,
    /// Paths where every object has the same, known type.
    concrete_paths: HashSet<PathId>,
}

/// How far the merge pass may go.
#[derive(Clone, Copy, PartialEq)]
pub enum Merge {
    /// Only merge fetches that are ready at the same step. No fetch is ever
    /// delayed.
    Asap,
    /// Also delay fetches that have slack so they share a request with a later
    /// fetch to the same subgraph. The plan never gets deeper, but a fetch can
    /// be held back for a branch of the query that turns out to be empty at
    /// runtime.
    Slack,
}

pub fn plan(
    schema: &Schema,
    routes: &RouteCache,
    query: &str,
    operation_name: Option<&str>,
    variables: &Map<String, Value>,
    mode: Merge,
) -> Result<Plan, String> {
    let document = parse_query(query).map_err(|e| e.to_string())?;
    let (expanded, root, var_defs) = expand(schema, &document, operation_name, variables)?;

    let mut planner = Planner {
        schema,
        routes,
        root,
        sels: Vec::new(),
        nodes: Vec::new(),
        paths: vec![(0, String::new(), String::new())],
        path_ids: HashMap::default(),
        roots: HashMap::default(),
        last_mutation: None,
        entity_nodes: HashMap::default(),
        providers: HashMap::default(),
        in_progress: HashSet::default(),
        composites: HashMap::default(),
        placements: HashMap::default(),
        resolvers: HashMap::default(),
        hidden: HashMap::default(),
        aliased: false,
        concrete_paths: HashSet::default(),
    };

    if let Some(fields) = expanded.get(root) {
        for field in fields.values() {
            if field.name == "__typename" {
                continue;
            }
            let (_, info) = schema
                .owner(root, &field.name, root)
                .ok_or_else(|| format!("Unknown field {}.{}", root, field.name))?;
            // The subgraph that answers the most of what is under the field
            // itself, then one we already send a root fetch to.
            let mut graphs: Vec<(usize, bool, GraphId)> = Vec::new();
            for (graph, join) in &info.graphs {
                if join.external || join.used_overridden {
                    continue;
                }
                let missing = match &field.sub {
                    Some(sub) if info.graphs.len() > 1 => {
                        let (ret, provided) = planner.child_scope(&[], root, field, info, Some(join));
                        planner.estimate(*graph, &provided, ret, sub)
                    }
                    _ => 0,
                };
                let existing = if root == "Mutation" {
                    planner
                        .last_mutation
                        .is_some_and(|n| planner.nodes[n].subgraph == *graph)
                } else {
                    planner.roots.contains_key(graph)
                };
                graphs.push((missing, !existing, *graph));
            }
            graphs.sort();
            let graph = graphs
                .first()
                .map(|g| g.2)
                .ok_or_else(|| format!("No subgraph resolves {}.{}", root, field.name))?;
            let node = planner.root_node(graph);
            let pos = planner.root_pos(node);
            planner.plan_field(&pos, root, field)?;
        }
    }

    let (groups, steps) = planner.merge(mode, &var_defs)?;
    let fetch_nodes = planner.nodes.len();
    let request_bound = planner.request_bound();
    drop(planner);
    Ok(Plan {
        expanded,
        root,
        groups,
        steps,
        fetch_nodes,
        request_bound,
        nodes: AtomicUsize::new(0),
        response_size: AtomicUsize::new(0),
    })
}

// ---------------------------------------------------------------------------
// Operation normalization
// ---------------------------------------------------------------------------

struct Normalizer<'a> {
    schema: &'a Schema,
    document: &'a ExecutableDocument,
    variables: &'a Map<String, Value>,
    /// Boolean defaults of the operation's variables.
    defaults: HashMap<String, bool>,
}

type VarDefs = Vec<(String, String)>;

fn expand(
    schema: &Schema,
    document: &ExecutableDocument,
    operation_name: Option<&str>,
    variables: &Map<String, Value>,
) -> Result<(Expanded, &'static str, VarDefs), String> {
    let mut operations = document.operations.iter();
    let operation = match operation_name {
        Some(name) => operations
            .find(|(n, _)| n.is_some_and(|n| n.as_str() == name))
            .ok_or_else(|| format!("Unknown operation {}", name))?,
        None => {
            let first = operations.next().ok_or("No operation in the document")?;
            if operations.next().is_some() {
                return Err("The document has several operations: operationName is required".into());
            }
            first
        }
    };
    let operation = &operation.1.node;
    let root = match operation.ty {
        OperationType::Query => "Query",
        OperationType::Mutation => "Mutation",
        OperationType::Subscription => return Err("Subscriptions are not supported".into()),
    };
    if !schema.types.contains_key(root) {
        return Err(format!("The schema has no {} type", root));
    }

    let mut defaults = HashMap::default();
    let mut var_defs = Vec::new();
    for definition in &operation.variable_definitions {
        let definition = &definition.node;
        let name = definition.name.node.to_string();
        let mut printed = format!("${}:{}", name, definition.var_type.node);
        if let Some(default) = &definition.default_value {
            printed.push_str(&format!("={}", default.node));
            if let async_graphql_value::ConstValue::Boolean(b) = default.node {
                defaults.insert(name.clone(), b);
            }
        }
        var_defs.push((name, printed));
    }

    let normalizer = Normalizer {
        schema,
        document,
        variables,
        defaults,
    };
    let mut out = Expanded::default();
    normalizer.collect(&operation.selection_set.node, &[root.to_string()], root, &mut out)?;
    Ok((out, root, var_defs))
}

impl Normalizer<'_> {
    /// `@skip` and `@include`.
    fn included(&self, directives: &[Positioned<Directive>]) -> bool {
        for directive in directives {
            let directive = &directive.node;
            let skip = match directive.name.node.as_str() {
                "skip" => true,
                "include" => false,
                _ => continue,
            };
            let condition = match directive.get_argument("if").map(|v| &v.node) {
                Some(GqlValue::Boolean(b)) => *b,
                Some(GqlValue::Variable(name)) => self
                    .variables
                    .get(name.as_str())
                    .and_then(Value::as_bool)
                    .or_else(|| self.defaults.get(name.as_str()).copied())
                    .unwrap_or(false),
                _ => false,
            };
            if condition == skip {
                return false;
            }
        }
        true
    }

    /// Types a field of type `ty` is planned for: the object types the client
    /// can see and, where interface objects are involved, the interface.
    fn scope(&self, ty: &str) -> Vec<String> {
        let mut scope: Vec<String> = self
            .schema
            .possible(ty)
            .into_iter()
            .filter(|o| !self.schema.types[o].inaccessible)
            .collect();
        if self.schema.types.get(ty).is_some_and(|t| !t.iobj.is_empty()) {
            scope.push(ty.to_string());
        }
        scope
    }

    /// A literal has to be valid in the schema the client sees.
    fn check_value(&self, ty: &str, value: &GqlValue) -> Result<(), String> {
        let Some(info) = self.schema.types.get(ty) else {
            return Ok(());
        };
        match value {
            GqlValue::List(items) => {
                for item in items {
                    self.check_value(ty, item)?;
                }
            }
            GqlValue::Enum(name) if info.kind == Kind::Enum => {
                if info.values.get(name.as_str()) != Some(&false) {
                    return Err(format!("Value {} does not exist in enum {}", name, ty));
                }
            }
            GqlValue::Object(fields) if info.kind == Kind::Input => {
                for (name, value) in fields {
                    let field = info
                        .inputs
                        .get(name.as_str())
                        .filter(|f| !f.inaccessible)
                        .ok_or_else(|| format!("Field {} is not defined by type {}", name, ty))?;
                    self.check_value(&field.ty, value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn collect(
        &self,
        selection_set: &SelectionSet,
        scope: &[String],
        via: &str,
        out: &mut Expanded,
    ) -> Result<(), String> {
        let schema = self.schema;
        for selection in &selection_set.items {
            match &selection.node {
                Selection::Field(field) => {
                    let field = &field.node;
                    if !self.included(&field.directives) {
                        continue;
                    }
                    let name = field.name.node.as_str();
                    let key = field
                        .alias
                        .as_ref()
                        .map(|a| a.node.as_str())
                        .unwrap_or(name);
                    if name == "__typename" {
                        for object in scope {
                            out.entry(object.clone())
                                .or_default()
                                .entry(key.to_string())
                                .or_insert_with(|| EField {
                                    key: key.to_string(),
                                    written: written_key(key),
                                    name: name.to_string(),
                                    args: String::new(),
                                    via: via.to_string(),
                                    ty: None,
                                    ret: String::new(),
                                    concrete: false,
                                    sub: None,
                                });
                        }
                        continue;
                    }
                    let declared = schema
                        .types
                        .get(via)
                        .and_then(|t| t.fields.get(name))
                        .filter(|f| !f.inaccessible)
                        .ok_or_else(|| format!("Cannot query field \"{}\" on type \"{}\"", name, via))?;
                    for (argument, value) in &field.arguments {
                        let info = declared
                            .args
                            .get(argument.node.as_str())
                            .filter(|a| !a.inaccessible)
                            .ok_or_else(|| {
                                format!(
                                    "Unknown argument \"{}\" on field \"{}.{}\"",
                                    argument.node, via, name
                                )
                            })?;
                        self.check_value(&info.ty, &value.node)?;
                    }
                    let composite = schema.is_composite(&declared.ret);
                    if composite && field.selection_set.node.items.is_empty() {
                        return Err(format!("Field \"{}\" needs a selection", name));
                    }
                    for object in scope {
                        let Some(info) = schema.types.get(object).and_then(|t| t.fields.get(name))
                        else {
                            continue;
                        };
                        let fields = out.entry(object.clone()).or_default();
                        let entry = fields.entry(key.to_string()).or_insert_with(|| {
                            let args = if field.arguments.is_empty() {
                                String::new()
                            } else {
                                let args: Vec<String> = field
                                    .arguments
                                    .iter()
                                    .map(|(n, v)| format!("{}: {}", n.node, v.node))
                                    .collect();
                                format!("({})", args.join(", "))
                            };
                            EField {
                                key: key.to_string(),
                                written: written_key(key),
                                name: name.to_string(),
                                args,
                                via: via.to_string(),
                                ty: Some(info.ty.clone()),
                                ret: info.ret.clone(),
                                concrete: schema.is_object(&info.ret),
                                sub: composite.then(|| Arc::new(Expanded::default())),
                            }
                        });
                        if let Some(sub) = entry.sub.as_mut() {
                            let sub = Arc::get_mut(sub).expect("selection is not shared yet");
                            let child_scope = self.scope(&info.ret);
                            self.collect(&field.selection_set.node, &child_scope, &info.ret, sub)?;
                        }
                    }
                }
                Selection::InlineFragment(fragment) => {
                    let fragment = &fragment.node;
                    if !self.included(&fragment.directives) {
                        continue;
                    }
                    let condition = fragment.type_condition.as_ref().map(|t| t.node.on.node.as_str());
                    let narrowed = self.narrow(scope, condition)?;
                    self.collect(
                        &fragment.selection_set.node,
                        &narrowed,
                        condition.unwrap_or(via),
                        out,
                    )?;
                }
                Selection::FragmentSpread(spread) => {
                    if !self.included(&spread.node.directives) {
                        continue;
                    }
                    let name = &spread.node.fragment_name.node;
                    let fragment = &self
                        .document
                        .fragments
                        .get(name)
                        .ok_or_else(|| format!("Unknown fragment {}", name))?
                        .node;
                    let condition = fragment.type_condition.node.on.node.as_str();
                    let narrowed = self.narrow(scope, Some(condition))?;
                    self.collect(&fragment.selection_set.node, &narrowed, condition, out)?;
                }
            }
        }
        Ok(())
    }

    fn narrow(&self, scope: &[String], condition: Option<&str>) -> Result<Vec<String>, String> {
        let Some(condition) = condition else {
            return Ok(scope.to_vec());
        };
        if !self.schema.is_composite(condition) || self.schema.types[condition].inaccessible {
            return Err(format!("Unknown type \"{}\"", condition));
        }
        Ok(scope
            .iter()
            .filter(|s| self.schema.subtype(s, condition))
            .cloned()
            .collect())
    }
}

/// Shape of `condition.name` when all that is known is the selection: used
/// for fields that were added before a second type condition showed up.
fn shape_of(schema: &Schema, condition: &str, name: &str) -> String {
    match schema.types.get(condition).and_then(|t| t.fields.get(name)) {
        Some(info) => schema.shape(info, None),
        None => name.to_string(),
    }
}

fn merge_reps(target: &mut Vec<RepField>, source: &[RepField]) {
    for field in source {
        match target.iter_mut().find(|f| f.key == field.key) {
            None => target.push(field.clone()),
            Some(existing) => {
                existing.required |= field.required;
                if let (Some(existing), Some(sub)) = (existing.sub.as_mut(), field.sub.as_ref()) {
                    for (object, fields) in sub {
                        merge_reps(existing.entry(object.clone()).or_default(), fields);
                    }
                }
            }
        }
    }
}

/// Whether two representations have a field of the same name that is read
/// from different places in the response.
fn reps_conflict(a: &[RepField], b: &[RepField]) -> bool {
    a.iter().any(|x| {
        b.iter().filter(|y| y.name == x.name).any(|y| {
            x.key != y.key
                || match (&x.sub, &y.sub) {
                    (Some(x), Some(y)) => x
                        .iter()
                        .any(|(object, fields)| y.get(object).is_some_and(|other| reps_conflict(fields, other))),
                    (None, None) => false,
                    _ => true,
                }
        })
    })
}

// ---------------------------------------------------------------------------
// Pass 1: greedy assignment
// ---------------------------------------------------------------------------

impl<'a> Planner<'a> {
    fn new_sel(&mut self) -> SelId {
        self.sels.push(Sel::default());
        self.sels.len() - 1
    }

    fn root_node(&mut self, graph: GraphId) -> NodeId {
        let mutation = self.root == "Mutation";
        if mutation {
            // Mutation fields run in order: one fetch per run of fields that
            // go to the same subgraph, each waiting for the one before.
            if let Some(last) = self.last_mutation {
                if self.nodes[last].subgraph == graph {
                    return last;
                }
            }
        } else if let Some(node) = self.roots.get(&graph) {
            return *node;
        }
        let sel = self.new_sel();
        self.nodes.push(Node {
            subgraph: graph,
            path: None,
            sel,
            deps: self.last_mutation.filter(|_| mutation).into_iter().collect(),
            reps: IndexMap::default(),
            mutation,
        });
        let node = self.nodes.len() - 1;
        if mutation {
            self.last_mutation = Some(node);
        } else {
            self.roots.insert(graph, node);
        }
        node
    }

    fn root_pos(&self, node: NodeId) -> Pos<'a> {
        Pos {
            node,
            sel: self.nodes[node].sel,
            path: 0,
            provided: Vec::new(),
            ret: self.root,
            iobj: None,
            parent: None,
        }
    }

    fn entity_node(&mut self, graph: GraphId, path: PathId, requires: &str) -> NodeId {
        let key = (graph, path, requires.to_string());
        if let Some(node) = self.entity_nodes.get(&key) {
            return *node;
        }
        let sel = self.new_sel();
        self.nodes.push(Node {
            subgraph: graph,
            path: Some(path),
            sel,
            deps: BTreeSet::new(),
            reps: IndexMap::default(),
            mutation: false,
        });
        self.entity_nodes.insert(key, self.nodes.len() - 1);
        self.nodes.len() - 1
    }

    fn child_path(&mut self, parent: PathId, object: &str, key: &str) -> PathId {
        let id = (parent, object.to_string(), key.to_string());
        if let Some(path) = self.path_ids.get(&id) {
            return *path;
        }
        self.paths.push(id.clone());
        self.path_ids.insert(id, self.paths.len() - 1);
        self.paths.len() - 1
    }

    /// Whether `graph` can select `object.field` in place, at a position with
    /// these provided fields, and under which type condition.
    fn inline_at(
        &self,
        graph: GraphId,
        provided: &[&'a FsField],
        iobj: Option<&'a str>,
        object: &str,
        field: &EField,
        (owner, info): (&'a str, &'a FieldInfo),
    ) -> Option<&'a str> {
        let schema = self.schema;
        let usable = |join: &FieldJoin| !join.external && !join.used_overridden && join.requires.is_empty();
        match iobj {
            // The objects here are interface objects: only the interface's
            // own fields can be selected.
            Some(interface) => {
                if owner != interface {
                    // Selected on the interface, or part of what identifies
                    // the object: the same value for every implementation.
                    let info = &schema.types[interface];
                    let shared = (field.via == interface
                        || info.key_names.contains(&(graph, field.name.clone())))
                        && schema.types[interface]
                            .fields
                            .get(&field.name)
                            .and_then(|f| f.graphs.get(&graph))
                            .is_some_and(usable);
                    return shared.then_some(interface);
                }
            }
            None => {
                if owner != object || schema.types[owner].iobj.contains(&graph) {
                    return None;
                }
            }
        }
        if provided
            .iter()
            .any(|p| p.name == field.name && schema.applies(&p.conds, object))
        {
            return Some(owner);
        }
        info.graphs.get(&graph).filter(|j| usable(j)).map(|_| owner)
    }

    /// Type and provided fields of what is under `object.field` when it is
    /// selected in a subgraph, given what is provided where the field is.
    fn child_scope(
        &self,
        provided: &[&'a FsField],
        object: &str,
        field: &EField,
        info: &'a FieldInfo,
        join: Option<&'a FieldJoin>,
    ) -> (&'a str, Vec<&'a FsField>) {
        let mut inner: Vec<&'a FsField> = provided
            .iter()
            .filter(|p| p.name == field.name && self.schema.applies(&p.conds, object))
            .flat_map(|p| p.sub.iter())
            .collect();
        if let Some(join) = join {
            inner.extend(join.provides.iter());
        }
        let ret = join.and_then(|j| j.ty.as_deref()).unwrap_or(&info.ret);
        (ret, inner)
    }

    fn interface_object(&self, graph: GraphId, ty: &'a str) -> Option<&'a str> {
        self.schema
            .types
            .get(ty)
            .filter(|t| t.iobj.contains(&graph))
            .map(|_| ty)
    }

    /// How many of the fields in `sub` the subgraph cannot select itself,
    /// counting a field it has to leave for once whatever is under it.
    fn estimate(&self, graph: GraphId, provided: &[&'a FsField], ret: &'a str, sub: &Expanded) -> usize {
        let schema = self.schema;
        let iobj = self.interface_object(graph, ret);
        let mut missing = 0;
        for (object, fields) in sub {
            if iobj.is_none() && !schema.possible_in(graph, ret, object) {
                continue;
            }
            for field in fields.values() {
                if field.name == "__typename" {
                    continue;
                }
                let Some(owner) = schema.owner(object, &field.name, &field.via) else {
                    continue;
                };
                if self.inline_at(graph, provided, iobj, object, field, owner).is_none() {
                    missing += 1;
                    continue;
                }
                if let (Some(child), (_, info)) = (&field.sub, owner) {
                    let (child_ret, child_provided) =
                        self.child_scope(provided, object, field, info, info.graphs.get(&graph));
                    missing += self.estimate(graph, &child_provided, child_ret, child);
                }
            }
        }
        missing
    }

    /// The fields of a field set as hidden fields of `object`, and how to
    /// read them back into a representation.
    fn hidden_fields(&mut self, object: &str, fs: &'a FieldSet, required: bool) -> Hidden {
        let id = fs.as_ptr() as usize;
        if let Some(found) = self.hidden.get(&id).and_then(|m| m.get(object)) {
            return found.clone();
        }
        let built = Rc::new(self.build_hidden(object, fs, required));
        self.hidden
            .entry(id)
            .or_default()
            .insert(object.to_string(), built.clone());
        built
    }

    fn build_hidden(&self, object: &str, fs: &[FsField], required: bool) -> (Vec<EField>, Vec<RepField>) {
        let schema = self.schema;
        let mut fields: Vec<EField> = Vec::new();
        let mut reps: Vec<RepField> = Vec::new();
        for item in fs {
            if !schema.applies(&item.conds, object) {
                continue;
            }
            let Some(info) = schema.types.get(object).and_then(|t| t.fields.get(&item.name)) else {
                continue;
            };
            let key = hidden_key(&item.name, &item.args);
            if fields.iter().any(|f| f.key == key) {
                continue;
            }
            let composite = schema.is_composite(&info.ret);
            let mut sub = None;
            let mut rep_sub = None;
            if composite {
                let mut expanded = Expanded::default();
                let mut by_type = HashMap::default();
                let mut scope = schema.possible(&info.ret);
                if !schema.types[&info.ret].iobj.is_empty() {
                    scope.push(info.ret.clone());
                }
                for child in scope {
                    let (child_fields, child_reps) = self.build_hidden(&child, &item.sub, false);
                    if child_fields.is_empty() {
                        continue;
                    }
                    expanded.insert(
                        child.clone(),
                        child_fields.into_iter().map(|f| (f.key.clone(), f)).collect(),
                    );
                    by_type.insert(child, child_reps);
                }
                sub = Some(Arc::new(expanded));
                rep_sub = Some(by_type);
            }
            reps.push(RepField {
                name: item.name.clone(),
                key: key.clone(),
                required,
                abstract_type: composite && schema.types[&info.ret].kind != Kind::Object,
                sub: rep_sub,
            });
            fields.push(EField {
                written: written_key(&key),
                key,
                name: item.name.clone(),
                args: item.args.clone(),
                via: object.to_string(),
                ty: Some(info.ty.clone()),
                ret: info.ret.clone(),
                concrete: schema.is_object(&info.ret),
                sub,
            });
        }
        (fields, reps)
    }

    /// Every node that provides part of `field`, planned at `path`.
    fn collect_deps(&self, path: PathId, object: &str, field: &EField, deps: &mut BTreeSet<NodeId>) {
        let id = (path, object.to_string(), field.key.clone());
        let Some(sub) = &field.sub else {
            if let Some(node) = self.providers.get(&id) {
                deps.insert(*node);
            }
            return;
        };
        if let Some(child) = self.path_ids.get(&id).copied() {
            if let Some(pos) = self.composites.get(&child) {
                deps.insert(pos.node);
            }
            if let Some(resolver) = self.resolvers.get(&child) {
                deps.insert(*resolver);
            }
            for (child_object, fields) in sub.iter() {
                for child_field in fields.values() {
                    self.collect_deps(child, child_object, child_field, deps);
                }
            }
        }
    }

    /// An entity fetch to `graph` for the objects of type `object` at `pos`,
    /// looked up by `key`: plans the fields the representation needs and
    /// returns the fetch node. `send` is the `__typename` of the
    /// representation.
    #[allow(clippy::too_many_arguments)]
    fn entity_fetch(
        &mut self,
        pos: &Pos<'a>,
        object: &str,
        send: &str,
        graph: GraphId,
        key: &'a Key,
        requires: &'a FieldSet,
        requires_raw: &str,
    ) -> Result<NodeId, String> {
        let schema = self.schema;
        // Interface objects only have the interface's fields to offer.
        let source = match pos.iobj {
            Some(interface)
                if interface != object
                    && key
                        .fs
                        .iter()
                        .chain(requires.iter())
                        .all(|f| schema.types[interface].fields.contains_key(&f.name)) =>
            {
                interface
            }
            _ => object,
        };
        let mut deps = BTreeSet::from([pos.node]);
        let mut needed: [Vec<RepField>; 2] = [Vec::new(), Vec::new()];
        // What the client selected here.
        let selected = pos.parent.as_ref().map(|p| p.sub.clone());
        for (index, (fs, required)) in [(&key.fs, true), (requires, false)].into_iter().enumerate() {
            if fs.is_empty() {
                continue;
            }
            let hidden = self.hidden_fields(source, fs, required);
            if hidden.0.len() < fs.iter().filter(|f| schema.applies(&f.conds, source)).count() {
                return Err(format!("{} does not have the fields of {:?}", source, fs));
            }
            let mut reps = hidden.1.clone();
            for (field, rep) in hidden.0.iter().zip(reps.iter_mut()) {
                // A scalar the client asked for under its own name is read
                // from there instead of being selected a second time.
                let own = selected
                    .as_ref()
                    .and_then(|s| s.get(source))
                    .and_then(|fields| fields.get(&field.name))
                    .filter(|own| {
                        own.name == field.name
                            && own.args.is_empty()
                            && field.args.is_empty()
                            && own.sub.is_none()
                            && field.sub.is_none()
                    });
                let field = match own {
                    Some(own) => {
                        rep.key = own.key.clone();
                        own
                    }
                    None => field,
                };
                let provider = self.plan_field(pos, source, field)?;
                if field.sub.is_none() {
                    deps.insert(provider);
                } else {
                    self.collect_deps(pos.path, source, field, &mut deps);
                }
            }
            needed[index] = reps;
        }

        let node = self.entity_node(graph, pos.path, requires_raw);
        if pos.iobj.is_some_and(|interface| interface != object) {
            // The type has to be known before a fetch that depends on it.
            if let Some(resolver) = self.resolvers.get(&pos.path) {
                deps.insert(*resolver);
            }
        }
        deps.remove(&node);
        self.nodes[node].deps.extend(deps);
        if !self.nodes[node].reps.contains_key(object) {
            self.nodes[node].reps.insert(
                object.to_string(),
                RepSpec {
                    typename: send.to_string(),
                    fields: Vec::new(),
                },
            );
        }
        let spec = &mut self.nodes[node].reps[object];
        for reps in &needed {
            merge_reps(&mut spec.fields, reps);
        }
        Ok(node)
    }

    /// The position of an entity fetch's own selection.
    fn entity_pos(&self, node: NodeId, pos: &Pos<'a>) -> Pos<'a> {
        Pos {
            node,
            sel: self.nodes[node].sel,
            path: pos.path,
            provided: Vec::new(),
            ret: pos.ret,
            iobj: None,
            parent: pos.parent.clone(),
        }
    }

    /// Plans one field of `object` at `pos` and returns the node that fetches it.
    fn plan_field(&mut self, pos: &Pos<'a>, object: &str, field: &EField) -> Result<NodeId, String> {
        if field.name == "__typename" {
            // Every composite selection already asks for `__typename`.
            return Ok(self.resolvers.get(&pos.path).copied().unwrap_or(pos.node));
        }
        let id = (pos.path, object.to_string(), field.key.clone());
        match &field.sub {
            None => {
                if let Some(node) = self.providers.get(&id) {
                    return Ok(*node);
                }
            }
            Some(sub) => {
                let placed = self.path_ids.get(&id).and_then(|p| self.composites.get(p));
                if let Some(child) = placed.cloned() {
                    self.plan_children(&child, sub)?;
                    return Ok(child.node);
                }
            }
        }

        let schema = self.schema;
        let graph = self.nodes[pos.node].subgraph;
        let (owner, info) = schema
            .owner(object, &field.name, &field.via)
            .ok_or_else(|| format!("Unknown field {}.{}", object, field.name))?;

        if let Some(condition) =
            self.inline_at(graph, &pos.provided, pos.iobj, object, field, (owner, info))
        {
            let join = if condition == owner { info.graphs.get(&graph) } else { None };
            self.add_field(pos, condition, object, field, info, join, true)?;
            self.providers.insert(id, pos.node);
            return Ok(pos.node);
        }

        if !self.in_progress.insert(id.clone()) {
            return Err(format!(
                "Cannot plan {}.{}: its keys depend on each other",
                object, field.name
            ));
        }

        // Hop to a subgraph that resolves the field and can look the entity up.
        // Options are ranked without exploring them: the one that has the
        // field ready soonest first, then a subgraph we already fetch from at
        // this place in the response.
        let provided: Vec<String> = pos
            .provided
            .iter()
            .filter(|p| schema.applies(&p.conds, object))
            .map(|p| p.name.clone())
            .collect();
        let routes = self.routes.get(schema, object, graph, &provided);
        let mut options: Vec<(usize, bool, GraphId, usize)> = Vec::new();
        for (target, join) in &info.graphs {
            if join.external || join.used_overridden {
                continue;
            }
            // A field of an interface object is only there for the taking in
            // the subgraphs that have the interface as an object.
            if owner != object && !schema.types[owner].iobj.contains(target) {
                continue;
            }
            for (index, key) in schema.types[owner].graphs.get(target).into_iter().flatten().enumerate() {
                // A field can't be looked up by a key it is part of.
                if key.names.contains(&field.name) {
                    continue;
                }
                let slowest = key
                    .names
                    .iter()
                    .chain(join.requires_names.iter())
                    .map(|n| routes.cost.get(n).copied().unwrap_or(UNREACHABLE))
                    .max()
                    .unwrap_or(0);
                let existing = self.entity_nodes.contains_key(&(
                    *target,
                    pos.path,
                    join.requires_raw.clone(),
                ));
                options.push((slowest, !existing, *target, index));
            }
        }
        options.sort();

        let mut chosen = None;
        let mut failure = format!("No subgraph can resolve {}.{} from here", object, field.name);
        for (_, _, target, index) in options {
            let join = &info.graphs[&target];
            let key = &schema.types[owner].graphs[&target][index];
            match self.entity_fetch(pos, object, owner, target, key, &join.requires, &join.requires_raw) {
                Ok(node) => {
                    chosen = Some((node, join));
                    break;
                }
                Err(message) => failure = message,
            }
        }
        self.in_progress.remove(&id);

        let Some((node, join)) = chosen else {
            return self.refetch_parent(pos, object, field, owner, info, failure);
        };
        let entity = self.entity_pos(node, pos);
        self.add_field(&entity, owner, object, field, info, Some(join), true)?;
        self.providers.insert(id, node);
        Ok(node)
    }

    /// Last resort for a field no entity fetch reaches: select the parent
    /// field again in a subgraph that has this field too. Only valid if that
    /// subgraph resolves the field for every type it was selected on.
    fn refetch_parent(
        &mut self,
        pos: &Pos<'a>,
        object: &str,
        field: &EField,
        owner: &str,
        info: &'a FieldInfo,
        failure: String,
    ) -> Result<NodeId, String> {
        let schema = self.schema;
        let Some(parent) = pos.parent.clone() else {
            return Err(failure);
        };
        if owner != object {
            return Err(failure);
        }
        let graph = self.nodes[pos.node].subgraph;
        let mut graphs: Vec<GraphId> = info
            .graphs
            .iter()
            .filter(|(g, j)| **g != graph && !j.external && !j.used_overridden && j.requires.is_empty())
            .map(|(g, _)| *g)
            .collect();
        graphs.sort();
        for target in graphs {
            let Some(other) = self.place_in(&parent, target)? else {
                continue;
            };
            let inline = |sibling: &str, same: &EField| {
                schema.owner(sibling, &same.name, &same.via).is_some_and(|owner| {
                    self.inline_at(target, &other.provided, other.iobj, sibling, same, owner)
                        .is_some()
                })
            };
            let complete = parent.sub.iter().all(|(sibling, fields)| {
                fields.get(&field.key).is_none_or(|same| {
                    !schema.possible_in(target, other.ret, sibling) || inline(sibling, same)
                })
            });
            if complete && inline(object, field) {
                return self.plan_field(&other, object, field);
            }
        }
        Err(failure)
    }

    /// A selection in `graph` for the value of the field `parent` describes:
    /// an existing one, or the field selected again there. `None` if the
    /// subgraph cannot get to it.
    fn place_in(&mut self, parent: &Rc<Parent<'a>>, graph: GraphId) -> Result<Option<Pos<'a>>, String> {
        let schema = self.schema;
        let at = &parent.pos;
        let child_path = self.child_path(at.path, &parent.object, &parent.key);
        if let Some(found) = self.placements.get(&(child_path, graph)) {
            return Ok(Some(found.clone()));
        }
        let Some((owner, info)) = schema.owner(&parent.object, &parent.name, &parent.via) else {
            return Ok(None);
        };
        let Some(join) = info.graphs.get(&graph) else {
            return Ok(None);
        };
        if owner != parent.object || join.external || join.used_overridden || !join.requires.is_empty() {
            return Ok(None);
        }

        // Somewhere in `graph` to select the parent field: the root, a fetch
        // that is already at the parent object, an entity fetch for it, or
        // the same again one level up.
        let host = if at.path == 0 {
            if self.root == "Mutation" {
                // Selecting a mutation field twice would run it twice.
                return Ok(None);
            }
            let node = self.root_node(graph);
            Some(self.root_pos(node))
        } else if let Some(found) = self
            .placements
            .get(&(at.path, graph))
            .filter(|p| p.iobj.is_none() && schema.possible_in(graph, p.ret, &parent.object))
        {
            Some(found.clone())
        } else if let Some(node) = self
            .entity_nodes
            .get(&(graph, at.path, String::new()))
            .filter(|n| self.nodes[**n].reps.contains_key(&parent.object))
        {
            Some(self.entity_pos(*node, at))
        } else {
            let mut host = None;
            for key in schema.types[owner].graphs.get(&graph).into_iter().flatten() {
                if key.names.contains(&parent.name) {
                    continue;
                }
                const NONE: &FieldSet = &Vec::new();
                if let Ok(node) = self.entity_fetch(at, &parent.object, owner, graph, key, NONE, "") {
                    host = Some(self.entity_pos(node, at));
                    break;
                }
            }
            match (host, &at.parent) {
                (Some(host), _) => Some(host),
                (None, Some(above)) => self
                    .place_in(above, graph)?
                    .filter(|p| schema.possible_in(graph, p.ret, &parent.object)),
                (None, None) => None,
            }
        };
        let Some(host) = host else {
            return Ok(None);
        };

        let field = EField {
            key: parent.key.clone(),
            written: written_key(&parent.key),
            name: parent.name.clone(),
            args: parent.args.clone(),
            via: parent.via.clone(),
            ty: Some(info.ty.clone()),
            ret: info.ret.clone(),
            concrete: schema.is_object(&info.ret),
            sub: Some(parent.sub.clone()),
        };
        self.add_field(&host, owner, &parent.object, &field, info, Some(join), false)?;
        Ok(self.placements.get(&(child_path, graph)).cloned())
    }

    /// Adds `field` to the selection at `pos` under the type condition
    /// `condition`, and plans what is under it if `descend`.
    #[allow(clippy::too_many_arguments)]
    fn add_field(
        &mut self,
        pos: &Pos<'a>,
        condition: &str,
        object: &str,
        field: &EField,
        info: &'a FieldInfo,
        join: Option<&'a FieldJoin>,
        descend: bool,
    ) -> Result<(), String> {
        let existing = self.sels[pos.sel]
            .by_type
            .get(condition)
            .and_then(|fields| fields.iter().find(|f| f.key == field.key))
            .map(|f| f.sub);
        let Some(sub) = &field.sub else {
            if existing.is_none() {
                self.push_field(pos.sel, condition, field, info, join, None);
            }
            return Ok(());
        };

        let child_sel = match existing.flatten() {
            Some(sel) => sel,
            None => {
                let sel = self.new_sel();
                self.push_field(pos.sel, condition, field, info, join, Some(sel));
                sel
            }
        };

        let graph = self.nodes[pos.node].subgraph;
        let (ret, provided) = self.child_scope(&pos.provided, object, field, info, join);
        let path = self.child_path(pos.path, object, &field.key);
        if field.concrete {
            self.concrete_paths.insert(path);
        }
        let child = Pos {
            node: pos.node,
            sel: child_sel,
            path,
            provided,
            ret,
            iobj: self.interface_object(graph, ret),
            parent: Some(Rc::new(Parent {
                pos: pos.clone(),
                object: object.to_string(),
                key: field.key.clone(),
                name: field.name.clone(),
                args: field.args.clone(),
                via: field.via.clone(),
                sub: sub.clone(),
            })),
        };
        self.composites.entry(path).or_insert_with(|| child.clone());
        self.placements
            .entry((path, graph))
            .or_insert_with(|| child.clone());
        if descend {
            self.plan_children(&child, sub)?;
        }
        Ok(())
    }

    fn push_field(
        &mut self,
        sel: SelId,
        condition: &str,
        field: &EField,
        info: &FieldInfo,
        join: Option<&FieldJoin>,
        sub: Option<SelId>,
    ) {
        let by_type = &mut self.sels[sel].by_type;
        // Only worth working out when there is another type condition.
        let mut shape = String::new();
        let mut alias = None;
        if by_type.len() > 1 || by_type.keys().next().is_some_and(|c| c != condition) {
            shape = self.schema.shape(info, join);
            let clash = by_type.iter_mut().any(|(other, fields)| {
                other != condition
                    && fields.iter_mut().any(|f| {
                        if f.key != field.key {
                            return false;
                        }
                        if f.shape.is_empty() {
                            f.shape = shape_of(self.schema, other, &f.name);
                        }
                        f.alias.is_none() && f.shape != shape
                    })
            });
            if clash {
                alias = Some(alias_for(condition, &field.key));
                self.aliased = true;
            }
        } else if join.is_some_and(|j| j.ty_raw.is_some()) {
            shape = self.schema.shape(info, join);
        }
        by_type.entry(condition.to_string()).or_default().push(SField {
            key: field.key.clone(),
            alias,
            name: field.name.clone(),
            args: field.args.clone(),
            shape,
            exact: sub.and(self.schema.is_object(&info.ret).then(|| info.ret.clone())),
            sub,
        });
    }

    /// Subgraphs other than the one `pos` is in that could also have been
    /// asked for the value at `pos`, with the type it has there.
    fn alternatives(&self, pos: &Pos<'a>) -> Vec<(GraphId, &'a str)> {
        let schema = self.schema;
        let Some(parent) = &pos.parent else {
            return Vec::new();
        };
        let Some((owner, info)) = schema.owner(&parent.object, &parent.name, &parent.via) else {
            return Vec::new();
        };
        if owner != parent.object || info.graphs.len() < 2 {
            return Vec::new();
        }
        let graph = self.nodes[pos.node].subgraph;
        let above = &parent.pos;
        // Where the parent object can be had: by key, or wherever its own
        // parent field can be selected.
        let mut reachable: Option<Vec<GraphId>> = None;
        let mut out = Vec::new();
        for (other, join) in &info.graphs {
            if *other == graph || join.external || join.used_overridden || !join.requires.is_empty() {
                continue;
            }
            let there = if above.path == 0 {
                self.root != "Mutation"
            } else {
                reachable
                    .get_or_insert_with(|| {
                        let mut graphs: Vec<GraphId> = schema.types[owner]
                            .graphs
                            .iter()
                            .filter(|(_, keys)| !keys.is_empty())
                            .map(|(g, _)| *g)
                            .collect();
                        graphs.extend(
                            self.alternatives(above)
                                .into_iter()
                                .filter(|(g, ret)| schema.possible_in(*g, ret, owner))
                                .map(|(g, _)| g),
                        );
                        graphs
                    })
                    .contains(other)
            };
            if there {
                out.push((*other, join.ty.as_deref().unwrap_or(&info.ret)));
            }
        }
        out
    }

    fn plan_children(&mut self, pos: &Pos<'a>, sub: &Expanded) -> Result<(), String> {
        let schema = self.schema;
        let graph = self.nodes[pos.node].subgraph;

        if let Some(interface) = pos.iobj {
            // Interface objects. If every type is asked for the same fields of
            // the interface, there is no need to know which one each is.
            let shared = sub.get(interface);
            let uniform = sub.iter().all(|(object, fields)| {
                object == interface
                    || shared.is_some_and(|shared| {
                        fields.len() == shared.len()
                            && fields
                                .values()
                                .all(|f| f.via == interface && shared.contains_key(&f.key))
                    })
            }) && shared.is_none_or(|s| s.values().all(|f| f.name != "__typename"));
            if uniform {
                for field in shared.into_iter().flat_map(|s| s.values()) {
                    self.plan_field(pos, interface, field)?;
                }
                return Ok(());
            }
            self.plan_resolver(pos, interface)?;
            for (object, fields) in sub {
                if object == interface {
                    continue;
                }
                for field in fields.values() {
                    self.plan_field(pos, object, field)?;
                }
            }
            return Ok(());
        }

        // A union member or implementation that another subgraph, which
        // could equally have been asked for this value, does not have here
        // cannot be relied on: its fields are left out, as other federation
        // gateways do. That subgraph has to know the abstract type itself.
        let partial: Vec<(GraphId, &str)> = if sub.len() > 1 && schema.types[pos.ret].kind != Kind::Object {
            self.alternatives(pos)
                .into_iter()
                .filter(|(g, ret)| {
                    schema
                        .types
                        .get(*ret)
                        .is_some_and(|t| t.kind != Kind::Object && !t.iobj.contains(g))
                })
                .collect()
        } else {
            Vec::new()
        };

        let mut any = false;
        for (object, fields) in sub {
            // Types this subgraph can't return here never show up at runtime.
            if !schema.possible_in(graph, pos.ret, object) {
                continue;
            }
            any = true;
            if partial.iter().any(|(g, ret)| !schema.possible_in(*g, ret, object)) {
                continue;
            }
            for field in fields.values() {
                self.plan_field(pos, object, field)?;
            }
        }
        if !any {
            // An interface this subgraph has no implementation of: all there
            // is to select are the interface's own fields.
            if let Some(fields) = sub.get(pos.ret) {
                for field in fields.values() {
                    self.plan_field(pos, pos.ret, field)?;
                }
            }
        }
        Ok(())
    }

    /// A fetch that turns the interface objects at `pos` into objects of a
    /// concrete type, by asking a subgraph that knows the implementations.
    fn plan_resolver(&mut self, pos: &Pos<'a>, interface: &'a str) -> Result<NodeId, String> {
        if let Some(node) = self.resolvers.get(&pos.path) {
            return Ok(*node);
        }
        let schema = self.schema;
        let info = &schema.types[interface];
        let mut graphs: Vec<GraphId> = info
            .graphs
            .keys()
            .filter(|g| !info.iobj.contains(g))
            .copied()
            .collect();
        graphs.sort();
        for graph in graphs {
            for key in &info.graphs[&graph] {
                const NONE: &FieldSet = &Vec::new();
                let Ok(node) = self.entity_fetch(pos, interface, interface, graph, key, NONE, "") else {
                    continue;
                };
                let fields = self.sels[self.nodes[node].sel]
                    .by_type
                    .entry(interface.to_string())
                    .or_default();
                if !fields.iter().any(|f| f.key == "__typename") {
                    fields.push(SField {
                        key: "__typename".into(),
                        alias: None,
                        name: "__typename".into(),
                        args: String::new(),
                        shape: String::new(),
                        exact: None,
                        sub: None,
                    });
                }
                self.resolvers.insert(pos.path, node);
                return Ok(node);
            }
        }
        Err(format!("No subgraph can tell the type of a {}", interface))
    }

    // -----------------------------------------------------------------------
    // Pass 2 and 3: leveling and merging
    // -----------------------------------------------------------------------

    /// Earliest and latest step of every node, with `fixed` nodes pinned.
    fn windows(&self, fixed: &[Option<usize>], depth: usize) -> (Vec<usize>, Vec<usize>) {
        let n = self.nodes.len();
        let mut dependents: Vec<Vec<NodeId>> = vec![Vec::new(); n];
        for (id, node) in self.nodes.iter().enumerate() {
            for dep in &node.deps {
                dependents[*dep].push(id);
            }
        }

        fn earliest(
            id: NodeId,
            nodes: &[Node],
            fixed: &[Option<usize>],
            memo: &mut [usize],
        ) -> usize {
            if memo[id] != 0 {
                return memo[id];
            }
            let level = fixed[id].unwrap_or_else(|| {
                1 + nodes[id]
                    .deps
                    .iter()
                    .map(|d| earliest(*d, nodes, fixed, memo))
                    .max()
                    .unwrap_or(0)
            });
            memo[id] = level;
            level
        }
        fn latest(
            id: NodeId,
            dependents: &[Vec<NodeId>],
            fixed: &[Option<usize>],
            depth: usize,
            memo: &mut [usize],
        ) -> usize {
            if memo[id] != 0 {
                return memo[id];
            }
            let level = fixed[id].unwrap_or_else(|| {
                dependents[id]
                    .iter()
                    .map(|d| latest(*d, dependents, fixed, depth, memo) - 1)
                    .min()
                    .unwrap_or(depth)
            });
            memo[id] = level;
            level
        }

        let mut lo = vec![0; n];
        let mut hi = vec![0; n];
        for id in 0..n {
            earliest(id, &self.nodes, fixed, &mut lo);
        }
        let depth = depth.max(lo.iter().copied().max().unwrap_or(0));
        for id in 0..n {
            latest(id, &dependents, fixed, depth, &mut hi);
        }
        (lo, hi)
    }

    fn merge(&self, mode: Merge, var_defs: &VarDefs) -> Result<(Vec<Group>, usize), String> {
        let n = self.nodes.len();

        // The passes below assume the fetches can be ordered.
        let mut waiting: Vec<usize> = self.nodes.iter().map(|node| node.deps.len()).collect();
        let mut ready: Vec<NodeId> = (0..n).filter(|id| waiting[*id] == 0).collect();
        let mut ordered = 0;
        while let Some(id) = ready.pop() {
            ordered += 1;
            for (other, node) in self.nodes.iter().enumerate() {
                if node.deps.contains(&id) {
                    waiting[other] -= 1;
                    if waiting[other] == 0 {
                        ready.push(other);
                    }
                }
            }
        }
        if ordered != n {
            return Err("Cannot plan the operation: its fetches depend on each other".into());
        }

        let mut fixed: Vec<Option<usize>> = vec![None; n];
        let (lo, _) = self.windows(&fixed, 0);
        let depth = lo.iter().copied().max().unwrap_or(0);

        // Subgraphs with the most nodes have the most to gain from merging.
        let mut by_graph: IndexMap<GraphId, Vec<NodeId>> = IndexMap::default();
        for (id, node) in self.nodes.iter().enumerate() {
            if !node.mutation {
                by_graph.entry(node.subgraph).or_default().push(id);
            }
        }
        by_graph.sort_by(|_, a, _, b| b.len().cmp(&a.len()));

        for nodes in by_graph.values() {
            if nodes.len() == 1 || mode == Merge::Asap {
                continue;
            }
            // Interval stabbing: pin the node that has to run soonest at its
            // latest step, then pull every node whose window still contains
            // that step onto it. Windows are recomputed after every pin, since
            // pinning a node moves its dependencies' and dependents' windows.
            loop {
                let (_, hi) = self.windows(&fixed, depth);
                let Some(first) = nodes
                    .iter()
                    .copied()
                    .filter(|id| fixed[*id].is_none())
                    .min_by_key(|id| hi[*id])
                else {
                    break;
                };
                let step = hi[first];
                fixed[first] = Some(step);
                loop {
                    let (lo, hi) = self.windows(&fixed, depth);
                    let Some(next) = nodes
                        .iter()
                        .copied()
                        .find(|id| fixed[*id].is_none() && lo[*id] <= step && step <= hi[*id])
                    else {
                        break;
                    };
                    fixed[next] = Some(step);
                }
            }
        }

        // Everything else runs as early as it can.
        let (lo, _) = self.windows(&fixed, depth);

        // A fetch of mutation fields is a request of its own.
        let mut members: IndexMap<(usize, GraphId, usize), Vec<NodeId>> = IndexMap::default();
        for (id, node) in self.nodes.iter().enumerate() {
            let alone = if node.mutation { id + 1 } else { 0 };
            members.entry((lo[id], node.subgraph, alone)).or_default().push(id);
        }
        members.sort_keys();
        let mut group_of = vec![0; n];
        for (index, nodes) in members.values().enumerate() {
            for id in nodes {
                group_of[*id] = index;
            }
        }
        let mut groups: Vec<Group> = members
            .iter()
            .enumerate()
            .map(|(index, ((step, graph, _), nodes))| {
                let mut deps: Vec<usize> = nodes
                    .iter()
                    .flat_map(|id| self.nodes[*id].deps.iter().map(|d| group_of[*d]))
                    .filter(|d| *d != index)
                    .collect();
                deps.sort();
                deps.dedup();
                self.group(*graph, *step, deps, nodes, var_defs)
            })
            .collect();
        for index in 0..groups.len() {
            for dep in groups[index].deps.clone() {
                groups[dep].dependents.push(index);
            }
        }
        Ok((groups, depth))
    }

    /// Fetch nodes for the same subgraph that depend on one another can never
    /// share a request, so each subgraph needs at least as many requests as
    /// its longest such chain.
    fn request_bound(&self) -> usize {
        let graphs: BTreeSet<GraphId> = self.nodes.iter().map(|n| n.subgraph).collect();
        // Dependencies are not created in order, so settle by iteration.
        let mut bound = 0;
        for graph in graphs {
            let mut chain = vec![0usize; self.nodes.len()];
            loop {
                let mut changed = false;
                for (id, node) in self.nodes.iter().enumerate() {
                    let own = usize::from(node.subgraph == graph);
                    let longest = own + node.deps.iter().map(|d| chain[*d]).max().unwrap_or(0);
                    if longest > chain[id] {
                        chain[id] = longest;
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }
            bound += chain.iter().copied().max().unwrap_or(0);
        }
        bound
    }

    fn group(
        &self,
        graph: GraphId,
        step: usize,
        deps: Vec<usize>,
        nodes: &[NodeId],
        var_defs: &VarDefs,
    ) -> Group {
        let mut variables = Vec::new();
        let mut body = String::new();
        let mut entities = Vec::new();
        let mut has_root = false;
        let mut mutation = false;

        // Entity fetches for the same place are one `_entities` field when
        // they look the entities up the same way.
        let mut parts: Vec<(PathId, Vec<NodeId>, IndexMap<String, RepSpec>)> = Vec::new();
        for id in nodes {
            let node = &self.nodes[*id];
            let Some(path) = node.path else {
                has_root = true;
                mutation |= node.mutation;
                if let Some(fields) = self.sels[node.sel].by_type.get(self.root) {
                    if !body.is_empty() {
                        body.push(' ');
                    }
                    self.print_fields(fields, &mut body);
                }
                continue;
            };
            let shared = parts
                .iter_mut()
                .find(|(place, members, reps)| *place == path && self.can_share(members, reps, *id));
            match shared {
                Some((_, members, reps)) => {
                    for (object, spec) in &node.reps {
                        merge_reps(&mut reps[object].fields, &spec.fields);
                    }
                    members.push(*id);
                }
                None => parts.push((path, vec![*id], node.reps.clone())),
            }
        }

        // Places that ask the same of the same kind of entity are one field
        // as well: an entity that is in several of them is fetched once.
        let mut fields: Vec<(String, Vec<PathId>, IndexMap<String, RepSpec>)> = Vec::new();
        for (path, members, reps) in parts {
            let sels: Vec<SelId> = members.iter().map(|member| self.nodes[*member].sel).collect();
            let mut selection = String::new();
            self.print_fragments(&sels, &mut selection);
            if selection.is_empty() {
                selection.push_str("__typename");
            }
            match fields.iter_mut().find(|(other, _, theirs)| *other == selection && *theirs == reps) {
                Some((_, paths, _)) => paths.push(path),
                None => fields.push((selection, vec![path], reps)),
            }
        }

        for (selection, paths, reps) in fields {
            let index = entities.len();
            variables.push(format!("$_r{}:[_Any!]!", index));
            if !body.is_empty() {
                body.push(' ');
            }
            body.push_str(&format!("e{}:_entities(representations:$_r{}){{{}}}", index, index, selection));

            let concrete = reps.len() == 1 && paths.iter().all(|path| self.concrete_paths.contains(path));
            let paths = paths
                .into_iter()
                .map(|path| {
                    let mut steps = Vec::new();
                    let mut current = path;
                    while current != 0 {
                        let (parent, object, key) = &self.paths[current];
                        steps.push(PathStep {
                            parent: object.clone(),
                            key: key.clone(),
                            check: *parent != 0 && !self.concrete_paths.contains(parent),
                        });
                        current = *parent;
                    }
                    steps.reverse();
                    steps
                })
                .collect();
            entities.push(EntityPart {
                alias: format!("e{}", index),
                paths,
                concrete,
                reps: reps.into_iter().collect(),
            });
        }

        // Client variables the request mentions.
        let mut vars = Vec::new();
        for (name, definition) in var_defs {
            let needle = format!("${}", name);
            let used = body.match_indices(&needle).any(|(at, _)| {
                !body[at + needle.len()..]
                    .starts_with(|c: char| c.is_alphanumeric() || c == '_')
            });
            if used {
                variables.push(definition.clone());
                vars.push(name.clone());
            }
        }

        let query_has_alias = body.contains("_a");
        let operation = if mutation { "mutation" } else { "query" };
        let query = if variables.is_empty() && !mutation {
            format!("{{{}}}", body)
        } else if variables.is_empty() {
            format!("{}{{{}}}", operation, body)
        } else {
            format!("{}({}){{{}}}", operation, variables.join(","), body)
        };
        Group {
            subgraph: graph,
            step,
            deps,
            query,
            has_root,
            entities,
            vars,
            aliased: self.aliased && query_has_alias,
            mutation,
            dependents: Vec::new(),
            response_size: AtomicUsize::new(0),
        }
    }

    /// Whether the entity fetch `node` can be the same `_entities` field as
    /// `members`, whose representations are `reps`: the same types, looked up
    /// by the same key fields, no field of the representation with two
    /// values (`@requires` of one field with different arguments), and no
    /// response key that means two things.
    /// What one of them `@requires` is then sent to the others as well.
    fn can_share(&self, members: &[NodeId], reps: &IndexMap<String, RepSpec>, node: NodeId) -> bool {
        let node = &self.nodes[node];
        let keys = |spec: &RepSpec| -> BTreeSet<String> {
            spec.fields.iter().filter(|f| f.required).map(|f| f.key.clone()).collect()
        };
        let same_lookup = node.reps.len() == reps.len()
            && node.reps.iter().all(|(object, spec)| {
                reps.get(object).is_some_and(|other| {
                    other.typename == spec.typename
                        && keys(other) == keys(spec)
                        && !reps_conflict(&other.fields, &spec.fields)
                })
            });
        if !same_lookup {
            return false;
        }
        let fields = |sel: SelId| self.sels[sel].by_type.values().flatten();
        members.iter().all(|member| {
            fields(self.nodes[*member].sel).all(|theirs| {
                fields(node.sel).all(|ours| {
                    ours.alias.as_ref().unwrap_or(&ours.key) != theirs.alias.as_ref().unwrap_or(&theirs.key)
                        || (ours.name == theirs.name
                            && ours.args == theirs.args
                            && ours.shape == theirs.shape
                            && ours.sub.is_none()
                            && theirs.sub.is_none())
                })
            })
        })
    }

    fn print_fields<'s>(&'s self, fields: impl IntoIterator<Item = &'s SField>, out: &mut String) {
        let mut first = true;
        for field in fields {
            if !std::mem::take(&mut first) {
                out.push(' ');
            }
            let key = field.alias.as_ref().unwrap_or(&field.key);
            if key != &field.name {
                out.push_str(key);
                out.push(':');
            }
            out.push_str(&field.name);
            out.push_str(&field.args);
            let Some(sub) = field.sub else {
                continue;
            };
            out.push('{');
            // Values that can only be of one type need no `__typename` and
            // no type condition.
            let by_type = &self.sels[sub].by_type;
            let only = field.exact.as_ref().and_then(|exact| {
                by_type
                    .get(exact)
                    .filter(|fields| by_type.len() == 1 && !fields.is_empty())
            });
            match only {
                Some(fields) => self.print_fields(fields, out),
                None => {
                    out.push_str("__typename");
                    self.print_fragments(&[sub], out);
                }
            }
            out.push('}');
        }
    }

    /// The selections as one inline fragment per type condition. A field
    /// that several of them have is printed once.
    fn print_fragments(&self, sels: &[SelId], out: &mut String) {
        let mut by_type: IndexMap<&str, Vec<&SField>> = IndexMap::default();
        for sel in sels {
            for (object, fields) in &self.sels[*sel].by_type {
                let merged = by_type.entry(object.as_str()).or_default();
                for field in fields {
                    let key = field.alias.as_ref().unwrap_or(&field.key);
                    if !merged.iter().any(|other| other.alias.as_ref().unwrap_or(&other.key) == key) {
                        merged.push(field);
                    }
                }
            }
        }
        for (object, fields) in by_type {
            if fields.is_empty() {
                continue;
            }
            // Punctuation separates tokens, so only names need a space.
            if out.ends_with(|c: char| c.is_alphanumeric() || c == '_') {
                out.push(' ');
            }
            out.push_str("...on ");
            out.push_str(object);
            out.push('{');
            self.print_fields(fields, out);
            out.push('}');
        }
    }
}
