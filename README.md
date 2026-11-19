# prototype-router

A prototype GraphQL federation gateway, written to try out a query planner design. It reads an Apollo Federation v2 supergraph (join spec v0.3), plans operations across subgraphs and executes them.

It is an experiment, not a production router. See [Limitations](#limitations).

## Planner

Three passes, described in more detail in the module comment of [`src/plan.rs`](src/plan.rs):

1. **Greedy assignment.** Each field goes to a subgraph: stay in the subgraph that produced the parent object if it can resolve the field, otherwise hop to the subgraph that can have it ready soonest. Hops to the same subgraph at the same response path are one fetch.
2. **Leveling.** Each fetch gets the earliest and the latest step it can run at without making the plan deeper.
3. **Merging.** Fetches to the same subgraph are moved within that window onto the same step, and each (subgraph, step) group is sent as one HTTP request with one aliased `_entities` field per fetch.

## Running

```sh
cargo build --release
SUPERGRAPH_PATH=./supergraph.graphql ./target/release/prototype-router
```

| Variable | Default | |
| --- | --- | --- |
| `SUPERGRAPH_PATH` | `./supergraph.graphql` | Supergraph SDL. Subgraph URLs are taken from it. |
| `PORT` | `4000` | |
| `MERGE_MODE` | slack | `asap` never delays a fetch to share a request; the default may, within its slack. |
| `PLAN_DEBUG` | unset | Print each new plan to stderr. `PLAN_DEBUG=query` also prints the subgraph queries. |

Endpoints:

- `POST /graphql`: GraphQL over HTTP (`query`, `variables`, `operationName`).
- `POST /plan`: same body; plans without executing and returns the size of the plan (steps, requests, fetch nodes).
- `GET /health`

## Compatibility

Passes all 210 tests in the 48 test cases of the [Federation Gateway Audit](https://github.com/graphql-hive/federation-gateway-audit), in both merge modes.

## Limitations

- No introspection (`__schema`, `__type`) beyond `__typename`.
- No subscriptions, no `@defer`.
- No progressive `@override` (`label:`), no `@authenticated` / `@requiresScopes` / `@policy`.
- Variable values are not validated against the schema; only literals are.
- Subgraph errors are passed on with the message and the subgraph name, without paths.
- The planner's maps use a fast, non-collision-resistant hash, and the plan cache is unbounded. Do not expose it to untrusted clients.
- No header forwarding, timeouts, retries, TLS configuration or telemetry.
