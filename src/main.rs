//! Prototype federation gateway used to try out a planner design:
//! greedy field assignment, then a pass that merges
//! fetches so each subgraph is called at most once per step. See `plan.rs`.

mod doc;
mod plan;
mod schema;

use async_graphql_parser::types::{BaseType, Type};
use doc::{Doc, Id, ROOT};
use axum::{
    body::Bytes,
    extract::State,
    http::header::CONTENT_TYPE,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use futures_util::future::{BoxFuture, FutureExt, Shared};
use futures_util::stream::{FuturesUnordered, StreamExt};
use plan::{EField, EntityPart, Expanded, Group, Merge, Plan, RepField, RouteCache};
use schema::{Kind, Schema};
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct Gateway {
    schema: Schema,
    client: reqwest::Client,
    merge: Merge,
    debug: bool,
    routes: RouteCache,
    plans: RwLock<HashMap<String, Arc<Plan>>>,
    /// Queries being executed right now, by operation and variables. `None`
    /// when `DEDUPE=off`.
    flights: Option<Mutex<HashMap<String, Flight>>>,
}

/// One execution that every identical concurrent query waits for.
type Flight = Shared<BoxFuture<'static, Bytes>>;

/// Whether an object with `typename` can be what a selection on `condition`
/// applies to. Interface objects have the interface as their type until a
/// fetch resolves it.
fn matches(schema: &Schema, typename: &str, condition: &str) -> bool {
    typename == condition || schema.subtype(typename, condition) || schema.subtype(condition, typename)
}

fn typename(doc: &Doc, object: Id) -> Option<&str> {
    doc.get(object, "__typename").and_then(|t| doc.str(t))
}

/// Finds every object at `path` in the response. Only the fields on the path
/// are followed.
fn locate(schema: &Schema, doc: &Doc, node: Id, path: &[(String, String)], out: &mut Vec<Id>) {
    match doc.kind(node) {
        doc::Kind::Arr => {
            for item in doc.children(node) {
                locate(schema, doc, item, path, out);
            }
        }
        doc::Kind::Obj => {
            let Some(((parent_type, key), rest)) = path.split_first() else {
                out.push(node);
                return;
            };
            if typename(doc, node).is_some_and(|t| !matches(schema, t, parent_type)) {
                return;
            }
            if let Some(child) = doc.get(node, key) {
                locate(schema, doc, child, rest, out);
            }
        }
        _ => {}
    }
}

struct Prepared {
    /// The request body, as JSON text.
    body: Vec<u8>,
    /// Per entity part: (the entity, index of its representation).
    targets: Vec<Vec<(Id, usize)>>,
}

fn prepare(schema: &Schema, group: &Group, doc: &Doc, client: &Map<String, Value>) -> Option<Prepared> {
    let mut body = b"{\"query\":".to_vec();
    serde_json::to_writer(&mut body, &group.query).unwrap();
    body.extend_from_slice(b",\"variables\":{");
    let mut targets = Vec::new();
    let mut any = group.has_root;
    // Parts for the same place share the search for it.
    let mut located: Vec<(&[(String, String)], Vec<Id>)> = Vec::new();

    for (index, part) in group.entities.iter().enumerate() {
        if index > 0 {
            body.push(b',');
        }
        body.extend_from_slice(format!("\"_r{}\":[", index).as_bytes());
        let found = match located.iter().position(|(path, _)| *path == part.path.as_slice()) {
            Some(found) => found,
            None => {
                let mut objects = Vec::new();
                locate(schema, doc, ROOT, &part.path, &mut objects);
                located.push((&part.path, objects));
                located.len() - 1
            }
        };
        let before = body.len();
        targets.push(representations(schema, part, doc, &located[found].1, &mut body));
        any |= body.len() > before;
        body.push(b']');
    }
    if !any {
        return None;
    }
    let mut first = group.entities.is_empty();
    for name in &group.vars {
        if let Some(value) = client.get(name) {
            if !std::mem::take(&mut first) {
                body.push(b',');
            }
            serde_json::to_writer(&mut body, name).unwrap();
            body.push(b':');
            serde_json::to_writer(&mut body, value).unwrap();
        }
    }
    body.extend_from_slice(b"}}");
    Some(Prepared { body, targets })
}

/// Writes the fields of a representation, read off an object in the
/// response. `false` if a field the entity is looked up by has no value.
fn rep_fields(doc: &Doc, object: Id, fields: &[RepField], out: &mut Vec<u8>, first: &mut bool) -> bool {
    for (index, field) in fields.iter().enumerate() {
        let value = doc.get(object, &field.key).filter(|v| doc.kind(*v) != doc::Kind::Null);
        if value.is_none() && field.required {
            return false;
        }
        // Of two fields with one name, the later one is sent.
        if fields[index + 1..].iter().any(|later| later.name == field.name) {
            continue;
        }
        if !std::mem::take(first) {
            out.push(b',');
        }
        out.push(b'"');
        out.extend_from_slice(field.name.as_bytes());
        out.extend_from_slice(b"\":");
        match value {
            None => out.extend_from_slice(b"null"),
            Some(value) => rep_value(doc, value, field, out),
        }
    }
    true
}

fn rep_value(doc: &Doc, value: Id, field: &RepField, out: &mut Vec<u8>) {
    let Some(sub) = &field.sub else {
        return doc.write(value, out);
    };
    match doc.kind(value) {
        doc::Kind::Arr => {
            out.push(b'[');
            for (index, item) in doc.children(value).enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                rep_value(doc, item, field, out);
            }
            out.push(b']');
        }
        doc::Kind::Obj => {
            let typename = typename(doc, value);
            let mut first = true;
            out.push(b'{');
            if let (true, Some(typename)) = (field.abstract_type, typename) {
                out.extend_from_slice(b"\"__typename\":\"");
                out.extend_from_slice(typename.as_bytes());
                out.push(b'"');
                first = false;
            }
            let fields = typename
                .and_then(|t| sub.get(t))
                .or_else(|| sub.values().next().filter(|_| sub.len() == 1));
            if let Some(fields) = fields {
                rep_fields(doc, value, fields, out, &mut first);
            }
            out.push(b'}');
        }
        _ => doc.write(value, out),
    }
}

/// Appends the representations of `entities` to `out`, separated by commas,
/// and returns which representation is whose.
fn representations(
    schema: &Schema,
    part: &EntityPart,
    doc: &Doc,
    entities: &[Id],
    out: &mut Vec<u8>,
) -> Vec<(Id, usize)> {
    let mut seen: schema::HashMap<Vec<u8>, usize> = Default::default();
    let mut representation = Vec::new();
    let mut targets = Vec::with_capacity(entities.len());
    for &entity in entities {
        let Some(typename) = typename(doc, entity) else {
            continue;
        };
        // Planned for this type, or for the interface object it started as.
        let Some(spec) = part.reps.get(typename).or_else(|| {
            part.reps
                .iter()
                .find(|(planned, _)| schema.subtype(typename, planned))
                .map(|(_, spec)| spec)
        }) else {
            continue;
        };
        representation.clear();
        representation.extend_from_slice(b"{\"__typename\":\"");
        representation.extend_from_slice(spec.typename.as_bytes());
        representation.push(b'"');
        if !rep_fields(doc, entity, &spec.fields, &mut representation, &mut false) {
            continue;
        }
        representation.push(b'}');
        // Identical representations are sent once.
        let index = match seen.get(&representation) {
            Some(index) => *index,
            None => {
                let index = seen.len();
                if index > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(&representation);
                seen.insert(representation.clone(), index);
                index
            }
        };
        targets.push((entity, index));
    }
    targets
}

async fn send(gateway: &Gateway, group: &Group, body: Vec<u8>) -> Result<Bytes, String> {
    let subgraph = &gateway.schema.subgraphs[group.subgraph];
    gateway
        .client
        .post(&subgraph.url)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("{}: {}", subgraph.name, e))?
        .bytes()
        .await
        .map_err(|e| format!("{}: {}", subgraph.name, e))
}

/// Runs the plan: a request is sent as soon as the responses it depends on
/// have been merged, so a request never waits for one it doesn't need.
/// A subgraph's errors are passed on and whatever it did return is used.
/// Returns the response as JSON text.
async fn execute(gateway: &Gateway, plan: &Plan, variables: &Map<String, Value>) -> Vec<u8> {
    let schema = &gateway.schema;
    let mut doc = Doc::new();
    let mut errors: Vec<Value> = Vec::new();
    let concrete = |name: &str| schema.types.get(name).is_some_and(|t| t.kind == Kind::Object);

    let count = plan.groups.len();
    let mut waiting: Vec<usize> = plan.groups.iter().map(|g| g.deps.len()).collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (index, group) in plan.groups.iter().enumerate() {
        for dep in &group.deps {
            dependents[*dep].push(index);
        }
    }
    let mut ready: Vec<usize> = (0..count).filter(|i| waiting[*i] == 0).collect();
    let mut running = FuturesUnordered::new();

    loop {
        while let Some(index) = ready.pop() {
            let group = &plan.groups[index];
            match prepare(schema, group, &doc, variables) {
                Some(prepared) => running.push(async move {
                    let response = send(gateway, group, prepared.body).await;
                    (index, prepared.targets, response)
                }),
                // Nothing to ask for: whatever waits on this can go ahead.
                None => {
                    for dependent in &dependents[index] {
                        waiting[*dependent] -= 1;
                        if waiting[*dependent] == 0 {
                            ready.push(*dependent);
                        }
                    }
                }
            }
        }

        let Some((index, targets, response)) = running.next().await else {
            break;
        };
        let group = &plan.groups[index];
        let service = &schema.subgraphs[group.subgraph].name;
        let response = response.and_then(|bytes| {
            doc.parse(bytes, group.aliased).map_err(|e| format!("{}: {}", service, e))
        });
        match response {
            Err(message) => errors.push(json!({ "message": message })),
            Ok(response) => {
                let failed = doc.get(response, "errors").filter(|e| doc.kind(*e) == doc::Kind::Arr);
                for error in failed.into_iter().flat_map(|failed| doc.children(failed)) {
                    let message = doc
                        .get(error, "message")
                        .and_then(|m| serde_json::from_str(&doc.to_string(m)).ok())
                        .unwrap_or(Value::Null);
                    errors.push(json!({ "message": message, "extensions": { "service": service } }));
                }
                let result = doc.get(response, "data").filter(|d| doc.kind(*d) == doc::Kind::Obj);
                if let Some(result) = result {
                    for (part, targets) in group.entities.iter().zip(targets) {
                        let entities = doc.get(result, &part.alias).filter(|e| doc.kind(*e) == doc::Kind::Arr);
                        let Some(entities) = entities else {
                            continue;
                        };
                        let entities: Vec<Id> = doc.children(entities).collect();
                        // The last place an entity goes takes it; the others
                        // get copies.
                        let mut last = vec![0; entities.len()];
                        for (position, (_, entity)) in targets.iter().enumerate() {
                            if let Some(last) = last.get_mut(*entity) {
                                *last = position;
                            }
                        }
                        for (position, (target, entity)) in targets.into_iter().enumerate() {
                            match entities.get(entity) {
                                Some(&node) if doc.kind(node) != doc::Kind::Null => {
                                    doc.merge(target, node, last[entity] != position, &concrete)
                                }
                                _ => {}
                            }
                        }
                    }
                    let fields: Vec<Id> = doc.children(result).collect();
                    for field in fields {
                        let entities = group.entities.iter().any(|p| p.alias.as_bytes() == doc.key(field));
                        if !entities {
                            doc.merge_field(ROOT, field, false, &concrete);
                        }
                    }
                }
            }
        }
        for dependent in &dependents[index] {
            waiting[*dependent] -= 1;
            if waiting[*dependent] == 0 {
                ready.push(*dependent);
            }
        }
    }

    let mut shaper = Shaper { schema, doc: &doc, errors, out: b"{\"data\":".to_vec() };
    let start = shaper.out.len();
    if shaper.object(ROOT, &plan.expanded, Some(plan.root)).is_err() {
        shaper.out.truncate(start);
        shaper.out.extend_from_slice(b"null");
    }
    if !shaper.errors.is_empty() {
        shaper.out.extend_from_slice(b",\"errors\":");
        serde_json::to_writer(&mut shaper.out, &shaper.errors).unwrap();
    }
    shaper.out.push(b'}');
    shaper.out
}

/// Shapes the collected data into what the client asked for: its fields, in
/// its order, without the fields the plan added, and valid for the schema the
/// client sees. The result is written as JSON text straight into `out`.
/// `Err` is a null in a non-null place on its way up to the nearest field
/// that may be null, which takes back what was written for it.
struct Shaper<'a> {
    schema: &'a Schema,
    doc: &'a Doc,
    errors: Vec<Value>,
    out: Vec<u8>,
}

struct Bubble;

impl Shaper<'_> {
    fn string(&mut self, value: &str) {
        serde_json::to_writer(&mut self.out, value).unwrap();
    }

    fn object(&mut self, object: Id, selection: &Expanded, known: Option<&str>) -> Result<(), Bubble> {
        let doc = self.doc;
        let typename = known.or_else(|| typename(doc, object)).unwrap_or_default();
        self.out.push(b'{');
        if let Some(fields) = selection.get(typename) {
            for (index, field) in fields.values().enumerate() {
                if index > 0 {
                    self.out.push(b',');
                }
                self.string(&field.key);
                self.out.push(b':');
                match &field.ty {
                    None => self.string(typename),
                    Some(ty) => self.value(doc.get(object, &field.key), field, ty)?,
                }
            }
        }
        self.out.push(b'}');
        Ok(())
    }

    /// Replaces what was written since `start` with a null.
    fn null(&mut self, start: usize, field: &EField, ty: &Type, reason: Option<String>) -> Result<(), Bubble> {
        self.out.truncate(start);
        if let Some(reason) = reason {
            self.errors.push(json!({ "message": reason }));
        }
        if ty.nullable {
            self.out.extend_from_slice(b"null");
            return Ok(());
        }
        self.errors.push(json!({
            "message": format!("Cannot return null for non-nullable field {}", field.name)
        }));
        Err(Bubble)
    }

    fn value(&mut self, value: Option<Id>, field: &EField, ty: &Type) -> Result<(), Bubble> {
        let doc = self.doc;
        let start = self.out.len();
        let Some(value) = value.filter(|v| doc.kind(*v) != doc::Kind::Null) else {
            return self.null(start, field, ty, None);
        };
        match (&ty.base, doc.kind(value)) {
            (BaseType::List(inner), doc::Kind::Arr) => {
                self.out.push(b'[');
                for (index, item) in doc.children(value).enumerate() {
                    if index > 0 {
                        self.out.push(b',');
                    }
                    if self.value(Some(item), field, inner).is_err() {
                        return self.null(start, field, ty, None);
                    }
                }
                self.out.push(b']');
                Ok(())
            }
            (BaseType::List(_), _) => {
                self.null(start, field, ty, Some(format!("Expected a list for {}", field.name)))
            }
            (BaseType::Named(_), doc::Kind::Obj) => {
                let Some(sub) = &field.sub else {
                    doc.write(value, &mut self.out);
                    return Ok(());
                };
                // The client can only be given types its schema has.
                if let Some(typename) = typename(doc, value) {
                    let visible = typename == field.ret
                        || self.schema.types.get(typename).is_some_and(|t| {
                            !t.inaccessible && self.schema.subtype(typename, &field.ret)
                        });
                    if !visible {
                        let reason = format!("{} cannot be a {}", field.ret, typename);
                        return self.null(start, field, ty, Some(reason));
                    }
                }
                match self.object(value, sub, None) {
                    Ok(()) => Ok(()),
                    Err(Bubble) => self.null(start, field, ty, None),
                }
            }
            (BaseType::Named(_), _) if field.sub.is_some() => {
                self.null(start, field, ty, Some(format!("Expected an object for {}", field.name)))
            }
            (BaseType::Named(_), _) => {
                if let Some(info) = self.schema.types.get(&field.ret).filter(|t| t.kind == Kind::Enum) {
                    let known = doc.str(value).is_some_and(|v| info.values.get(v) == Some(&false));
                    if !known {
                        let reason = format!("{} is not a value of enum {}", doc.to_string(value), field.ret);
                        return self.null(start, field, ty, Some(reason));
                    }
                }
                doc.write(value, &mut self.out);
                Ok(())
            }
        }
    }
}

fn plan_for(
    gateway: &Gateway,
    query: &str,
    operation_name: Option<&str>,
    variables: &Map<String, Value>,
) -> Result<Arc<Plan>, String> {
    // `@skip` and `@include` are applied before planning, so the plan
    // depends on the boolean variables as well as on the operation.
    let mut id = query.to_string();
    if let Some(name) = operation_name {
        id.push('\0');
        id.push_str(name);
    }
    for (name, value) in variables {
        if let Value::Bool(value) = value {
            id.push('\0');
            id.push_str(name);
            id.push(if *value { '1' } else { '0' });
        }
    }
    if let Some(plan) = gateway.plans.read().unwrap().get(&id) {
        return Ok(plan.clone());
    }
    let plan = Arc::new(plan::plan(
        &gateway.schema,
        &gateway.routes,
        query,
        operation_name,
        variables,
        gateway.merge,
    )?);
    if gateway.debug {
        eprintln!("plan: {}", plan_stats(&plan));
        for group in &plan.groups {
            let paths: Vec<String> = group
                .entities
                .iter()
                .map(|e| {
                    let path: Vec<&str> = e.path.iter().map(|(_, k)| k.as_str()).collect();
                    let mut types: Vec<&str> = e.reps.keys().map(String::as_str).collect();
                    types.sort();
                    format!("{}<{}>", path.join("."), types.join("|"))
                })
                .collect();
            eprintln!(
                "  step {} {} {}{}",
                group.step,
                gateway.schema.subgraphs[group.subgraph].name,
                if group.has_root { "(root) " } else { "" },
                paths.join(" ")
            );
            if std::env::var("PLAN_DEBUG").is_ok_and(|v| v == "query") {
                eprintln!("    {}", group.query);
            }
        }
    }
    gateway.plans.write().unwrap().insert(id, plan.clone());
    Ok(plan)
}

/// What the planner decided, before any data is known.
fn plan_stats(plan: &Plan) -> Value {
    json!({
        "steps": plan.steps,
        "requests": plan.groups.len(),
        "request_bound": plan.request_bound,
        "fetch_nodes": plan.fetch_nodes,
    })
}

struct Request<'a> {
    query: &'a str,
    operation_name: Option<&'a str>,
    variables: &'a Map<String, Value>,
}

fn read_request<'a>(request: &'a Value, empty: &'a Map<String, Value>) -> Result<Request<'a>, Json<Value>> {
    let Some(query) = request.get("query").and_then(Value::as_str) else {
        return Err(Json(json!({ "errors": [{ "message": "Missing query" }] })));
    };
    Ok(Request {
        query,
        operation_name: request.get("operationName").and_then(Value::as_str),
        variables: request.get("variables").and_then(Value::as_object).unwrap_or(empty),
    })
}

fn json_response(body: Bytes) -> Response {
    ([(CONTENT_TYPE, "application/json")], body).into_response()
}

async fn graphql(State(gateway): State<Arc<Gateway>>, Json(request): Json<Value>) -> Response {
    let empty = Map::new();
    let request = match read_request(&request, &empty) {
        Ok(request) => request,
        Err(response) => return response.into_response(),
    };
    let plan = match plan_for(&gateway, request.query, request.operation_name, request.variables) {
        Ok(plan) => plan,
        Err(message) => return Json(json!({ "errors": [{ "message": message }] })).into_response(),
    };
    // Identical queries that arrive while one is being executed share its
    // response. No request headers reach the subgraphs, so the operation and
    // its variables are all a response depends on. Mutations always run.
    let flights = gateway.flights.as_ref().filter(|_| plan.root == "Query");
    let Some(flights) = flights else {
        return json_response(execute(&gateway, &plan, request.variables).await.into());
    };
    let mut id = request.query.to_string();
    id.push('\0');
    id.push_str(request.operation_name.unwrap_or_default());
    id.push('\0');
    id.push_str(&serde_json::to_string(request.variables).unwrap());

    let flight = flights
        .lock()
        .unwrap()
        .entry(id.clone())
        .or_insert_with(|| {
            let gateway = gateway.clone();
            let variables = request.variables.clone();
            async move {
                let response = execute(&gateway, &plan, &variables).await;
                // Whoever asks from here on gets a fresh execution.
                if let Some(flights) = &gateway.flights {
                    flights.lock().unwrap().remove(&id);
                }
                Bytes::from(response)
            }
            .boxed()
            .shared()
        })
        .clone();
    json_response(flight.await)
}

/// `POST /plan` with a GraphQL request body: plans the operation without
/// executing it and returns the size of the plan.
async fn plan_only(State(gateway): State<Arc<Gateway>>, Json(request): Json<Value>) -> Json<Value> {
    let empty = Map::new();
    let request = match read_request(&request, &empty) {
        Ok(request) => request,
        Err(response) => return response,
    };
    match plan_for(&gateway, request.query, request.operation_name, request.variables) {
        Ok(plan) => Json(plan_stats(&plan)),
        Err(message) => Json(json!({ "errors": [{ "message": message }] })),
    }
}

#[tokio::main]
async fn main() {
    let path = std::env::var("SUPERGRAPH_PATH").unwrap_or("./supergraph.graphql".into());
    let sdl = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("Unable to read {}", path));
    let gateway = Arc::new(Gateway {
        schema: Schema::parse(&sdl),
        client: reqwest::Client::new(),
        // MERGE_MODE=asap never delays a fetch; the default also uses slack.
        merge: match std::env::var("MERGE_MODE").as_deref() {
            Ok("asap") => Merge::Asap,
            _ => Merge::Slack,
        },
        debug: std::env::var("PLAN_DEBUG").is_ok(),
        routes: RouteCache::default(),
        plans: RwLock::new(HashMap::new()),
        flights: match std::env::var("DEDUPE").as_deref() {
            Ok("off") => None,
            _ => Some(Mutex::new(HashMap::new())),
        },
    });

    let app = Router::new()
        .route("/graphql", post(graphql))
        .route("/plan", post(plan_only))
        .route("/health", axum::routing::get(|| async { "OK" }))
        .with_state(gateway);

    let port = std::env::var("PORT").unwrap_or("4000".into());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
