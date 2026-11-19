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

## Results

Run on 2026-11-18 at commit `aff2581`. `prototype-router` is the default merge mode; `prototype-router (asap)` is `MERGE_MODE=asap`.

In short: it passes the whole compatibility audit, plans faster than everything except Grafbase, and sends the fewest subgraph requests of any gateway tested. Its throughput is poor: about a ninth of Hive Router's and Cosmo's.

### Federation compatibility

[Federation Gateway Audit](https://github.com/graphql-hive/federation-gateway-audit) at commit `9c874b9`.

| Gateway | Compatibility | Test cases | Test suites |
| --- | --- | --- | --- |
| **prototype-router** | 100.00% | 210 / 210 | 48 / 48 |
| **prototype-router (asap)** | 100.00% | 210 / 210 | 48 / 48 |
| Hive Router 0.3.11 | 100.00% | 210 / 210 | 48 / 48 |
| Cosmo Router 0.365.2 | 99.05% | 208 / 210 | 46 / 48 |
| Hive Gateway 2.15.6 | 99.05% | 208 / 210 | 47 / 48 |
| Hive Gateway (Rust QP, router-runtime 1.6.9) | 97.62% | 205 / 210 | 46 / 48 |
| Apollo Router v3 (dev-v3.x, incremental planner) | 97.14% | 204 / 210 | 45 / 48 |
| Apollo Router 2.19.0 | 95.24% | 200 / 210 | 43 / 48 |
| Hot Chocolate Fusion 16.7.3 | 95.24% | 200 / 210 | 44 / 48 |
| Apollo Gateway 2.14.4 | 94.76% | 199 / 210 | 42 / 48 |
| Apollo Router v3 (dev-v3.x) | 94.76% | 199 / 210 | 42 / 48 |
| Grafbase Gateway 0.53.5 | 88.57% | 186 / 210 | 40 / 48 |

The prototype rows were run locally. The other rows are the results committed in the audit repository at that commit; they were not re-run.

### Benchmarks

[GraphQL Gateways Benchmark](https://github.com/graphql-hive/graphql-gateways-benchmark), on the branch of [PR #1041](https://github.com/graphql-hive/graphql-gateways-benchmark/pull/1041), which adds the query planning benchmark and the 80 subgraph supergraph. Every gateway was run locally, one after another in one session.

- Machine: MacBook Pro, Apple M1 Max (10 cores), 64 GB, macOS 27.2, k6 v2.4.0.
- No CPU pinning (macOS has no `taskset`), so k6, the subgraphs and the gateway share all cores. Read the numbers as a ranking on this machine, not as the published CI figures.
- One run per gateway and mode.
- Versions: Apollo Router 2.18.0, Apollo Router 3.0.0-preview.0 (classic and incremental planner), `@apollo/gateway` 2.14.4, Cosmo 0.360.1, Grafbase 0.53.5, Hive Router 0.3.5, `@graphql-hive/gateway` 2.15.2, `@graphql-hive/router-runtime` 1.6.5.

#### Throughput, constant load

50 VUs for 60 s after a 15 s warmup, 4 subgraphs, the same query every time. Every gateway had a 100% success rate.

| Gateway | RPS | Avg (ms) | p50 (ms) | p95 (ms) | p99.9 (ms) | CPU max (%) | Mem max (MB) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| hive-router | 15,365 | 3.1 | 2.6 | 6.3 | 30.2 | 159 | 112 |
| cosmo | 15,363 | 3.1 | 2.9 | 5.4 | 11.9 | 233 | 86 |
| grafbase | 5,942 | 8.2 | 7.3 | 14.4 | 57.4 | 480 | 52 |
| hive-gateway-router-runtime | 5,054 | 9.7 | 9.1 | 12.6 | 35.4 | n/a | n/a |
| hive-gateway | 4,270 | 11.5 | 10.6 | 14.4 | 71.3 | n/a | n/a |
| **prototype-router** | 1,739 | 28.5 | 25.3 | 50.3 | 131.3 | 489 | 83 |
| **prototype-router (asap)** | 1,598 | 31.0 | 27.2 | 56.6 | 164.2 | 461 | 95 |
| apollo-router-v3-incremental | 1,595 | 31.0 | 28.8 | 48.3 | 143.1 | 743 | 200 |
| apollo-router-v3 | 1,272 | 38.7 | 37.7 | 54.7 | 86.9 | 790 | 245 |
| apollo-router | 1,250 | 39.5 | 38.4 | 56.1 | 97.2 | 793 | 226 |
| apollo-gateway | 733 | 67.4 | 64.5 | 119.4 | 208.9 | n/a | n/a |

CPU and memory are `n/a` for the Node gateways: on macOS the monitor only follows the launcher process and its direct children, and it missed the Node process.

#### Throughput, stress

Ramp from 50 to 500 VUs and back over 60 s. Every gateway had a 100% success rate.

| Gateway | RPS | Avg (ms) | p50 (ms) | p95 (ms) | p99.9 (ms) | CPU max (%) | Mem max (MB) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| hive-router | 17,653 | 10.3 | 8.4 | 25.7 | 64.2 | 168 | 156 |
| cosmo | 16,383 | 9.8 | 7.5 | 24.7 | 60.4 | 275 | 124 |
| grafbase | 7,172 | 31.3 | 28.9 | 67.1 | 98.8 | 496 | 203 |
| hive-gateway-router-runtime | 5,803 | 37.9 | 37.6 | 67.1 | 98.9 | n/a | n/a |
| hive-gateway | 5,351 | 40.1 | 40.0 | 69.3 | 116.6 | n/a | n/a |
| apollo-router-v3-incremental | 1,610 | 136.6 | 127.8 | 302.6 | 429.2 | 751 | 646 |
| **prototype-router** | 1,513 | 140.9 | 120.5 | 343.3 | 562.6 | 506 | 1,167 |
| **prototype-router (asap)** | 1,458 | 145.9 | 123.7 | 365.7 | 596.9 | 498 | 926 |
| apollo-router | 1,349 | 162.3 | 154.0 | 339.6 | 647.9 | 785 | 1,029 |
| apollo-router-v3 | 1,276 | 171.0 | 162.5 | 364.5 | 523.4 | 798 | 1,053 |
| apollo-gateway | 723 | 294.8 | 286.9 | 643.2 | 840.6 | n/a | n/a |

The prototype's memory reached 1.2 GB under stress, the highest measured.

#### Query planning, 4 subgraphs

One VU sends each query warm (cached) and cold (unique operation name and aliases, so the plan cache misses). Planning overhead is the paired `cold - warm` latency: parsing, validation, normalization and planning. 1000 iterations; every gateway had a 100% success rate.

Planning overhead p50 in ms:

| Gateway | simple | federated | complex |
| --- | --- | --- | --- |
| grafbase | 0.03 | 0.10 | 0.19 |
| **prototype-router (asap)** | 0.04 | 0.28 | 0.40 |
| **prototype-router** | 0.05 | 0.27 | 0.57 |
| hive-router | 0.10 | 0.71 | 1.37 |
| cosmo | 0.18 | 1.66 | 4.32 |
| apollo-router-v3-incremental | 0.39 | 1.82 | 3.17 |
| apollo-gateway | 0.39 | 1.87 | 3.14 |
| apollo-router | 0.40 | 1.80 | 3.29 |
| hive-gateway-router-runtime | 0.41 | 1.77 | 3.01 |
| apollo-router-v3 | 0.46 | 2.13 | 3.62 |
| hive-gateway | 0.84 | 6.37 | 10.16 |

Plan quality, as sequential steps / subgraph requests seen by the subgraphs for one execution (`simple` is 1 / 1 for everyone):

| Gateway | federated | complex |
| --- | --- | --- |
| **prototype-router** | 4 / 6 | 4 / 6 |
| hive-router | 4 / 7 | 4 / 8 |
| hive-gateway-router-runtime | 4 / 7 | 4 / 8 |
| cosmo | 4 / 7 | 4 / 8 |
| **prototype-router (asap)** | 4 / 8 | 4 / 8 |
| hive-gateway | 3 / 7 | 4 / 11 |
| grafbase | 4 / 13 | 4 / 29 |
| apollo-router-v3-incremental | 4 / 13 | 4 / 30 |
| apollo-router, apollo-router-v3, apollo-gateway | 4 / 16 | 4 / 33 |

#### Query planning, 80 subgraphs

The same benchmark against the generated ecommerce supergraph from PR #1041. 200 iterations.

Planning overhead p50 in ms:

| Gateway | product_page | search | checkout | order_history |
| --- | --- | --- | --- | --- |
| grafbase | 8.1 | 2.5 | 0.9 | 1.7 |
| **prototype-router (asap)** | 13.7 | 4.2 | 1.6 | 2.6 |
| **prototype-router** | 23.7 | 5.6 | 1.8 | 3.5 |
| hive-gateway ¹ | 94 | 44 | 23 | 40 |
| apollo-router-v3-incremental | 112 | 46 | 17 | 45 |
| hive-router | 288 | 72 | 17 | 30 |
| apollo-router | 328 | 127 | 41 | 74 |
| apollo-router-v3 | 390 | 139 | 43 | 76 |
| apollo-gateway | 423 | 154 | 60 | 99 |
| hive-gateway-router-runtime | 437 | 116 | 29 | 49 |
| cosmo | 1,180 | 200 | 39 | 93 |

¹ hive-gateway finished with an 84.5% success rate; all others had 100%.

Plan quality, as sequential steps / subgraph requests:

| Gateway | product_page | search | checkout | order_history |
| --- | --- | --- | --- | --- |
| **prototype-router** | 7 / 49 | 7 / 17 | 7 / 51 | 8 / 56 |
| cosmo | 7 / 63 | 6 / 18 | 7 / 63 | 8 / 69 |
| hive-router | 7 / 64 | 6 / 19 | 7 / 63 | 8 / 70 |
| hive-gateway-router-runtime | 7 / 64 | 6 / 19 | 7 / 63 | 8 / 70 |
| **prototype-router (asap)** | 7 / 64 | 6 / 19 | 7 / 65 | 8 / 72 |
| grafbase | 7 / 91 | 6 / 39 | 7 / 99 | 8 / 131 |
| apollo-router, apollo-router-v3, apollo-gateway | 10 / 100 | 6 / 59 | 9 / 78 | 8 / 127 |
| hive-gateway | 11 / 72 | 7 / 19 | 14 / 69 | 11 / 76 |
| apollo-router-v3-incremental | 13 / 91 | 7 / 47 | 14 / 72 | 8 / 113 |

Notes on reading these:

- With slack merging the prototype sends 19% to 23% fewer requests than Hive Router on `product_page`, `checkout` and `order_history` at the same depth, and plans 8 to 12 times faster.
- On `search` the default mode measured 7 steps where `asap` and most other gateways measured 6. Merging is meant not to deepen the plan, so this needs a look. Steps are inferred from request timing, not read from the plan.
- Planning overhead covers everything a gateway does for a new operation. The prototype does less there than the others (see [Limitations](#limitations): no variable validation, no authorization directives), which flatters its numbers.
- Subgraph requests count HTTP requests. The prototype puts several entity fetches in one request under aliases; the other gateways send one fetch per request.

### Reproducing

Audit: copy [`harness/federation-gateway-audit`](harness/federation-gateway-audit) to `gateways/prototype-router` in the audit repository, then:

```sh
cd gateways/prototype-router && PROTOTYPE_ROUTER_DIR=/path/to/prototype-router ./install.sh && cd ../..
make test-prototype-router                   # default merge mode
MERGE_MODE=asap make test-prototype-router
```

Benchmark: add a `gateways/prototype` directory to the benchmark repository whose `run.sh` starts the release binary with `SUPERGRAPH_PATH` (default `../apollo-router/supergraph.graphql`), start the subgraphs with `make run-subgraphs`, then run `make test gateway=prototype mode=<constant|stress|planning|planning-large>`.

## Limitations

- No introspection (`__schema`, `__type`) beyond `__typename`.
- No subscriptions, no `@defer`.
- No progressive `@override` (`label:`), no `@authenticated` / `@requiresScopes` / `@policy`.
- Variable values are not validated against the schema; only literals are.
- Subgraph errors are passed on with the message and the subgraph name, without paths.
- The planner's maps use a fast, non-collision-resistant hash, and the plan cache is unbounded. Do not expose it to untrusted clients.
- No header forwarding, timeouts, retries, TLS configuration or telemetry.
