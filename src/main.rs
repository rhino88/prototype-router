//! Prototype federation gateway used to try out a planner design:
//! greedy field assignment, then a pass that merges
//! fetches so each subgraph is called at most once per step. See `plan.rs`.

mod plan;
mod schema;

use async_graphql_parser::types::{BaseType, Type};
use axum::{extract::State, routing::post, Json, Router};
use futures_util::stream::{FuturesUnordered, StreamExt};
use plan::{EField, EntityPart, Expanded, Group, Merge, Plan, RepField, RouteCache};
use schema::{Kind, Schema};
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};


struct Gateway {
    schema: Schema,
    client: reqwest::Client,
    merge: Merge,
    debug: bool,
    routes: RouteCache,
    plans: RwLock<HashMap<String, Arc<Plan>>>,
}

#[derive(Clone)]
enum Seg {
    Key(String),
    Index(usize),
}

fn get<'a>(mut value: &'a Value, pointer: &[Seg]) -> &'a Value {
    for seg in pointer {
        value = match seg {
            Seg::Key(key) => &value[key.as_str()],
            Seg::Index(index) => &value[*index],
        };
    }
    value
}

fn get_mut<'a>(mut value: &'a mut Value, pointer: &[Seg]) -> &'a mut Value {
    for seg in pointer {
        value = match seg {
            Seg::Key(key) => &mut value[key.as_str()],
            Seg::Index(index) => &mut value[*index],
        };
    }
    value
}

fn merge(schema: &Schema, target: &mut Value, source: Value) {
    match (target, source) {
        (Value::Object(target), Value::Object(source)) => {
            for (key, value) in source {
                match target.get_mut(&key) {
                    Some(existing) => {
                        // An interface object's name never replaces the
                        // concrete type another subgraph has told us.
                        if key == "__typename" && existing.is_string() {
                            let concrete = value
                                .as_str()
                                .and_then(|t| schema.types.get(t))
                                .is_some_and(|t| t.kind == Kind::Object);
                            if !concrete {
                                continue;
                            }
                        }
                        merge(schema, existing, value)
                    }
                    None => {
                        target.insert(key, value);
                    }
                }
            }
        }
        (Value::Array(target), Value::Array(source)) if target.len() == source.len() => {
            for (existing, value) in target.iter_mut().zip(source) {
                merge(schema, existing, value);
            }
        }
        // A subgraph that failed to resolve a field doesn't erase what
        // another one returned for it.
        (target, Value::Null) if !target.is_null() => {}
        (target, source) => *target = source,
    }
}

/// Puts fields the request had to alias back under their response keys.
fn unalias(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(unalias),
        Value::Object(object) => {
            if object.keys().any(|k| plan::unalias(k).len() != k.len()) {
                *object = std::mem::take(object)
                    .into_iter()
                    .map(|(key, value)| (plan::unalias(&key).to_string(), value))
                    .collect();
            }
            object.values_mut().for_each(unalias);
        }
        _ => {}
    }
}

/// Whether an object with `typename` can be what a selection on `condition`
/// applies to. Interface objects have the interface as their type until a
/// fetch resolves it.
fn matches(schema: &Schema, typename: &str, condition: &str) -> bool {
    typename == condition || schema.subtype(typename, condition) || schema.subtype(condition, typename)
}

/// Finds every object at `path` in the response.
fn locate(
    schema: &Schema,
    value: &Value,
    path: &[(String, String)],
    depth: usize,
    current: &mut Vec<Seg>,
    out: &mut Vec<Vec<Seg>>,
) {
    match value {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                current.push(Seg::Index(index));
                locate(schema, item, path, depth, current, out);
                current.pop();
            }
        }
        Value::Object(object) => {
            if depth == path.len() {
                out.push(current.clone());
                return;
            }
            let (parent_type, key) = &path[depth];
            if let Some(Value::String(typename)) = object.get("__typename") {
                if !matches(schema, typename, parent_type) {
                    return;
                }
            }
            if let Some(child) = object.get(key) {
                current.push(Seg::Key(key.clone()));
                locate(schema, child, path, depth + 1, current, out);
                current.pop();
            }
        }
        _ => {}
    }
}

struct Prepared {
    body: Value,
    /// Per entity part: (where the entity is, index of its representation).
    targets: Vec<Vec<(Vec<Seg>, usize)>>,
}

fn prepare(schema: &Schema, group: &Group, data: &Value, client: &Map<String, Value>) -> Option<Prepared> {
    let mut variables = Map::new();
    let mut targets = Vec::new();
    let mut any = group.has_root;

    for (index, part) in group.entities.iter().enumerate() {
        let (representations, part_targets) = representations(schema, part, data);
        any |= !representations.is_empty();
        variables.insert(format!("_r{}", index), Value::Array(representations));
        targets.push(part_targets);
    }
    if !any {
        return None;
    }
    for name in &group.vars {
        if let Some(value) = client.get(name) {
            variables.insert(name.clone(), value.clone());
        }
    }
    Some(Prepared {
        body: json!({ "query": group.query, "variables": variables }),
        targets,
    })
}

/// Reads the fields of a representation off an object in the response.
/// `false` if a field the entity is looked up by has no value.
fn rep_fields(object: &Map<String, Value>, fields: &[RepField], out: &mut Map<String, Value>) -> bool {
    for field in fields {
        let value = match object.get(&field.key) {
            None | Some(Value::Null) => {
                if field.required {
                    return false;
                }
                Value::Null
            }
            Some(value) => rep_value(value, field),
        };
        out.insert(field.name.clone(), value);
    }
    true
}

fn rep_value(value: &Value, field: &RepField) -> Value {
    let Some(sub) = &field.sub else {
        return value.clone();
    };
    match value {
        Value::Array(items) => Value::Array(items.iter().map(|i| rep_value(i, field)).collect()),
        Value::Object(object) => {
            let typename = object.get("__typename").and_then(Value::as_str);
            let mut out = Map::new();
            if let (true, Some(typename)) = (field.abstract_type, typename) {
                out.insert("__typename".into(), Value::String(typename.to_string()));
            }
            let fields = typename
                .and_then(|t| sub.get(t))
                .or_else(|| sub.values().next().filter(|_| sub.len() == 1));
            if let Some(fields) = fields {
                rep_fields(object, fields, &mut out);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn representations(
    schema: &Schema,
    part: &EntityPart,
    data: &Value,
) -> (Vec<Value>, Vec<(Vec<Seg>, usize)>) {
    let mut pointers = Vec::new();
    locate(schema, data, &part.path, 0, &mut Vec::new(), &mut pointers);

    let mut representations = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut targets = Vec::new();
    for pointer in pointers {
        let Value::Object(entity) = get(data, &pointer) else {
            continue;
        };
        let Some(Value::String(typename)) = entity.get("__typename") else {
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
        let mut representation = Map::new();
        representation.insert("__typename".into(), Value::String(spec.typename.clone()));
        if !rep_fields(entity, &spec.fields, &mut representation) {
            continue;
        }
        // Identical representations are sent once.
        let representation = Value::Object(representation);
        let id = representation.to_string();
        let index = *seen.entry(id).or_insert_with(|| {
            representations.push(representation);
            representations.len() - 1
        });
        targets.push((pointer, index));
    }
    (representations, targets)
}

async fn send(gateway: &Gateway, group: &Group, body: Value) -> Result<Value, String> {
    let subgraph = &gateway.schema.subgraphs[group.subgraph];
    gateway
        .client
        .post(&subgraph.url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("{}: {}", subgraph.name, e))?
        .json()
        .await
        .map_err(|e| format!("{}: {}", subgraph.name, e))
}

/// Runs the plan: a request is sent as soon as the responses it depends on
/// have been merged, so a request never waits for one it doesn't need.
/// A subgraph's errors are passed on and whatever it did return is used.
async fn execute(gateway: &Gateway, plan: &Plan, variables: &Map<String, Value>) -> Value {
    let schema = &gateway.schema;
    let mut data = Value::Object(Map::new());
    let mut errors: Vec<Value> = Vec::new();

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
            match prepare(schema, group, &data, variables) {
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
        match response {
            Err(message) => errors.push(json!({ "message": message })),
            Ok(mut response) => {
                if let Some(Value::Array(failed)) = response.get_mut("errors").map(Value::take) {
                    let service = &schema.subgraphs[plan.groups[index].subgraph].name;
                    for error in failed {
                        errors.push(json!({
                            "message": error.get("message").cloned().unwrap_or(Value::Null),
                            "extensions": { "service": service },
                        }));
                    }
                }
                if plan.groups[index].aliased {
                    if let Some(data) = response.get_mut("data") {
                        unalias(data);
                    }
                }
                if let Some(Value::Object(mut result)) = response.get_mut("data").map(Value::take) {
                    for (part, targets) in plan.groups[index].entities.iter().zip(targets) {
                        let Some(Value::Array(entities)) = result.remove(&part.alias) else {
                            continue;
                        };
                        for (pointer, entity) in targets {
                            match entities.get(entity) {
                                Some(Value::Null) | None => {}
                                Some(entity) => merge(schema, get_mut(&mut data, &pointer), entity.clone()),
                            }
                        }
                    }
                    merge(schema, &mut data, Value::Object(result));
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

    let mut shaper = Shaper { schema, errors };
    let data = match &data {
        Value::Object(object) => shaper
            .object(object, &plan.expanded, Some(plan.root))
            .unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let mut response = Map::new();
    response.insert("data".into(), data);
    if !shaper.errors.is_empty() {
        response.insert("errors".into(), Value::Array(shaper.errors));
    }
    Value::Object(response)
}

/// Shapes the collected data into what the client asked for: its fields, in
/// its order, without the fields the plan added, and valid for the schema the
/// client sees. `Err` is a null in a non-null place on its way up to the
/// nearest field that may be null.
struct Shaper<'a> {
    schema: &'a Schema,
    errors: Vec<Value>,
}

struct Bubble;

impl Shaper<'_> {
    fn object(
        &mut self,
        object: &Map<String, Value>,
        selection: &Expanded,
        typename: Option<&str>,
    ) -> Result<Value, Bubble> {
        let typename = typename
            .or_else(|| object.get("__typename").and_then(Value::as_str))
            .unwrap_or_default();
        let mut out = Map::new();
        if let Some(fields) = selection.get(typename) {
            for field in fields.values() {
                let value = match &field.ty {
                    None => Value::String(typename.to_string()),
                    Some(ty) => self.value(object.get(&field.key), field, ty)?,
                };
                out.insert(field.key.clone(), value);
            }
        }
        Ok(Value::Object(out))
    }

    fn null(&mut self, field: &EField, ty: &Type, reason: Option<String>) -> Result<Value, Bubble> {
        if let Some(reason) = reason {
            self.errors.push(json!({ "message": reason }));
        }
        if ty.nullable {
            return Ok(Value::Null);
        }
        self.errors.push(json!({
            "message": format!("Cannot return null for non-nullable field {}", field.name)
        }));
        Err(Bubble)
    }

    fn value(&mut self, value: Option<&Value>, field: &EField, ty: &Type) -> Result<Value, Bubble> {
        let value = match value {
            None | Some(Value::Null) => return self.null(field, ty, None),
            Some(value) => value,
        };
        match (&ty.base, value) {
            (BaseType::List(inner), Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match self.value(Some(item), field, inner) {
                        Ok(item) => out.push(item),
                        Err(Bubble) => return self.null(field, ty, None),
                    }
                }
                Ok(Value::Array(out))
            }
            (BaseType::List(_), _) => self.null(field, ty, Some(format!("Expected a list for {}", field.name))),
            (BaseType::Named(_), Value::Object(object)) => {
                let Some(sub) = &field.sub else {
                    return Ok(value.clone());
                };
                // The client can only be given types its schema has.
                if let Some(typename) = object.get("__typename").and_then(Value::as_str) {
                    let visible = typename == field.ret
                        || self.schema.types.get(typename).is_some_and(|t| {
                            !t.inaccessible && self.schema.subtype(typename, &field.ret)
                        });
                    if !visible {
                        let reason = format!("{} cannot be a {}", field.ret, typename);
                        return self.null(field, ty, Some(reason));
                    }
                }
                match self.object(object, sub, None) {
                    Ok(value) => Ok(value),
                    Err(Bubble) => self.null(field, ty, None),
                }
            }
            (BaseType::Named(_), _) if field.sub.is_some() => {
                self.null(field, ty, Some(format!("Expected an object for {}", field.name)))
            }
            (BaseType::Named(_), _) => {
                if let Some(info) = self.schema.types.get(&field.ret).filter(|t| t.kind == Kind::Enum) {
                    let known = value.as_str().is_some_and(|v| info.values.get(v) == Some(&false));
                    if !known {
                        let reason = format!("{} is not a value of enum {}", value, field.ret);
                        return self.null(field, ty, Some(reason));
                    }
                }
                Ok(value.clone())
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

async fn graphql(State(gateway): State<Arc<Gateway>>, Json(request): Json<Value>) -> Json<Value> {
    let empty = Map::new();
    let request = match read_request(&request, &empty) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let plan = match plan_for(&gateway, request.query, request.operation_name, request.variables) {
        Ok(plan) => plan,
        Err(message) => return Json(json!({ "errors": [{ "message": message }] })),
    };
    Json(execute(&gateway, &plan, request.variables).await)
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
