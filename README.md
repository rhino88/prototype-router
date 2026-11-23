# prototype-router

A prototype GraphQL federation gateway, written to try out a query planner design. It reads an Apollo Federation v2 supergraph (join spec v0.3), plans operations across subgraphs and executes them.

It is an experiment, not a production router. See [Limitations](#limitations).

## Planner

Three passes, described in more detail in the module comment of [`src/plan.rs`](src/plan.rs):

1. **Greedy assignment.** Each field goes to a subgraph: stay in the subgraph that produced the parent object if it can resolve the field, otherwise hop to the subgraph that can have it ready soonest. Hops to the same subgraph at the same response path are one fetch.
2. **Leveling.** Each fetch gets the earliest and the latest step it can run at without making the plan deeper.
3. **Merging.** Fetches to the same subgraph are moved within that window onto the same step, and each (subgraph, step) group is sent as one HTTP request with one aliased `_entities` field per fetch. Fetches for the same place in the response share one field when they look the entities up the same way, and so do places that ask the same of the same kind of entity, so an entity that is in several of them is fetched once.

Subgraph queries carry no more than the executor needs. Where a field's type is an object type, its values are not asked for `__typename` and get no type condition, and a key or `@requires` field the client selected itself is not selected a second time under a hidden alias.

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
| `MAX_CONNECTIONS` | 100 | Requests in flight per subgraph. More than that wait and are sent in the order they came. `0` for no limit. |
| `DEDUPE` | on | Identical queries that arrive while one is executing share its response. `off` executes every request. Mutations always execute. |
| `PLAN_DEBUG` | unset | Print each new plan to stderr. `PLAN_DEBUG=query` also prints the subgraph queries. |

Endpoints:

- `POST /graphql`: GraphQL over HTTP (`query`, `variables`, `operationName`).
- `POST /plan`: same body; plans without executing and returns the size of the plan (steps, requests, fetch nodes).
- `GET /health`

## Results

Two sessions are recorded here. The Linux VM section is a full re-run of the audit and the benchmark on 2026-11-19. The MacBook Pro section is the previous session and is left as it was.

### Linux VM, 2026-11-19

`prototype-router` at commit `6dc7500`, release build (`lto = "fat"`, `codegen-units = 1`) with rustc 1.100.0. The image's rustc 1.83 cannot read this `Cargo.lock` (`edition2024`). No router code was changed.

- Machine: 4 vCPU Intel Xeon (family 6, model 207, 1 thread per core), 16 GB RAM (15 GiB reported by `free`), Ubuntu 24.04.5, kernel 6.12.113, KVM. k6 v2.4.0.
- Audit: [Federation Gateway Audit](https://github.com/graphql-hive/federation-gateway-audit) at `9c874b9`, the same commit as the tables below. Both merge modes.
- Benchmark: [GraphQL Gateways Benchmark](https://github.com/graphql-hive/graphql-gateways-benchmark) at `6423e4f` on the branch of [PR #1041](https://github.com/graphql-hive/graphql-gateways-benchmark/pull/1041). Same gateway versions as the Mac session. One run each, in one session.
- CPU placement: the harness pins k6 to core 0 and the gateway to the rest. A probe of that default saturated k6 (two hot threads on one core, about 3,900 RPS) while the prototype used about 30% of one core, so the number was k6's. These runs set `LOAD_CPUSET=0-3` and `GATEWAY_CPUSET=0-3`, which the harness accepts, so k6, the gateway and the subgraphs share all 4 cores. That is the same sharing the Mac session had, where `taskset` does not exist.
- With dedupe on, k6 still used most of the CPU (about 280% during the prototype's constant run, gateway about 40% of one core). Those rows are partly how fast k6 can issue the same query. The dedupe-off rows are where the gateway and the subgraphs do the work.
- `prototype-router` is the default merge mode. `prototype-router (asap)` is `MERGE_MODE=asap`.

#### Federation compatibility

Both merge modes passed every case. There were no failing tests. The other gateways were not re-run; their rows in the Mac section are the results committed in the audit repository at `9c874b9`.

| Gateway | Compatibility | Test cases | Test suites | Failing tests |
| --- | --- | --- | --- | --- |
| **prototype-router** | 100.00% | 210 / 210 | 48 / 48 | none |
| **prototype-router (asap)** | 100.00% | 210 / 210 | 48 / 48 | none |

Per suite, both modes, failed = 0:

| Suite | Cases | Suite | Cases |
| --- | --- | --- | --- |
| abstract-types | 18 | child-type-mismatch | 4 |
| circular-reference-interface | 2 | complex-entity-call | 1 |
| corrupted-supergraph-node-id | 10 | enum-intersection | 5 |
| fed1-external-extends | 4 | fed1-external-extends-resolvable | 1 |
| fed1-external-extension | 4 | fed2-external-extends | 4 |
| fed2-external-extension | 4 | include-skip | 4 |
| input-object-intersection | 3 | interface-object-indirect-extension | 1 |
| interface-object-with-requires | 7 | keys-mashup | 1 |
| mutations | 4 | mysterious-external | 2 |
| nested-provides | 2 | node | 1 |
| non-resolvable-interface-object | 7 | null-keys | 1 |
| override-type-interface | 4 | override-with-requires | 4 |
| parent-entity-call | 1 | parent-entity-call-complex | 1 |
| partial-union | 2 | partial-union-complex | 5 |
| provides-on-interface | 2 | provides-on-union | 2 |
| requires-circular | 2 | requires-interface | 5 |
| requires-requires | 5 | requires-with-argument | 5 |
| requires-with-argument-conflict | 1 | requires-with-fragments | 6 |
| shared-root | 2 | simple-entity-call | 1 |
| simple-inaccessible | 4 | simple-interface-object | 13 |
| simple-override | 2 | simple-requires-provides | 12 |
| skip-entity-fetch | 7 | typename | 6 |
| unavailable-override | 2 | union-interface-distributed | 10 |
| union-intersection | 12 | union-overfetching | 4 |

Against the previous prototype rows (also 210 / 210 and 48 / 48) nothing moved. The audit ranking against the other gateways is unchanged, because those rows were not re-run.

#### Throughput, constant load

50 VUs for 60 s after a 15 s warmup, 4 subgraphs, the same query every time. Error rate is the share of requests that failed the harness checks (HTTP 200, no GraphQL errors). Every gateway was 0%.

| Gateway | RPS | Avg (ms) | p50 | p90 | p95 | p99 | p99.9 | CPU max (%) | Mem max (MB) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **prototype-router (asap)** | 11,613 | 4.2 | 3.6 | 7.3 | 8.9 | 12.4 | 17.7 | 41 | 31 |
| **prototype-router** | 11,412 | 4.2 | 3.7 | 7.4 | 9.0 | 12.6 | 18.4 | 40 | 30 |
| hive-router | 9,965 | 4.9 | 4.4 | 8.0 | 9.5 | 12.8 | 17.8 | 65 | 95 |
| cosmo | 7,822 | 6.2 | 5.8 | 9.5 | 10.8 | 14.0 | 19.1 | 97 | 78 |
| hive-gateway-router-runtime | 4,357 | 11.3 | 10.8 | 13.6 | 14.9 | 19.0 | 33.6 | 109 | 814 |
| grafbase | 3,547 | 13.9 | 13.4 | 18.3 | 20.3 | 25.1 | 33.8 | 217 | 64 |
| hive-gateway | 3,505 | 14.0 | 13.4 | 16.4 | 17.8 | 23.0 | 56.8 | 107 | 886 |
| apollo-router-v3-incremental | 951 | 51.9 | 51.3 | 65.1 | 69.5 | 78.0 | 89.9 | 315 | 153 |
| apollo-router | 840 | 58.8 | 58.2 | 73.2 | 77.8 | 86.6 | 100.1 | 323 | 182 |
| apollo-router-v3 | 702 | 70.2 | 69.5 | 88.2 | 94.0 | 106.4 | 124.3 | 325 | 187 |
| apollo-gateway | 299 | 164.6 | 160.3 | 199.7 | 210.5 | 236.9 | 287.2 | 274 | 1,358 |

On the Mac the same order was prototype, hive-router, cosmo, then a gap. Absolute RPS is lower here (prototype 11.4k vs 19.7k). The prototype's CPU stayed near 40% of one core, so this is not its ceiling on this machine.

#### Throughput, stress

Ramp from 50 to 500 VUs and back over 60 s. Every gateway had a 0% error rate, including `hive-gateway`, which stopped accepting connections in the Mac stress run.

| Gateway | RPS | Avg (ms) | p50 | p90 | p95 | p99 | p99.9 | CPU max (%) | Mem max (MB) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **prototype-router** | 11,053 | 15.4 | 12.5 | 31.9 | 38.9 | 53.3 | 76.5 | 40 | 47 |
| **prototype-router (asap)** | 10,972 | 15.6 | 12.7 | 32.5 | 39.7 | 55.3 | 79.4 | 41 | 43 |
| hive-router | 9,784 | 17.3 | 14.6 | 34.2 | 41.6 | 57.0 | 79.2 | 64 | 120 |
| cosmo | 8,167 | 22.7 | 19.0 | 45.0 | 53.9 | 75.2 | 105.2 | 96 | 114 |
| hive-gateway-router-runtime | 4,154 | 51.5 | 49.6 | 91.7 | 103.8 | 124.6 | 149.1 | 103 | 1,117 |
| hive-gateway | 3,864 | 53.9 | 51.5 | 93.7 | 104.5 | 137.2 | 163.6 | 112 | 1,121 |
| grafbase | 3,592 | 61.9 | 57.2 | 117.1 | 134.2 | 165.9 | 209.9 | 216 | 255 |
| apollo-router-v3-incremental | 821 | 261.1 | 231.8 | 532.7 | 606.0 | 733.9 | 918.8 | 302 | 538 |
| apollo-router | 734 | 291.1 | 266.4 | 587.0 | 674.5 | 816.1 | 967.8 | 309 | 873 |
| apollo-router-v3 | 612 | 346.0 | 311.3 | 708.4 | 802.8 | 994.1 | 1,194.1 | 309 | 871 |
| apollo-gateway | 258 | 794.2 | 749.1 | 1,627.1 | 1,792.3 | 2,249.3 | 3,215.5 | 250 | 1,677 |

#### Throughput, dedupe off

The same two runs with dedupe switched off, using the settings listed in the Mac section. Cosmo got `ENGINE_ENABLE_SINGLE_FLIGHT=false` and `ENGINE_ENABLE_INBOUND_REQUEST_DEDUPLICATION=false`. Its idle pool was left at the default of 20; on this kernel that did not exhaust ephemeral ports (100% success). On macOS the same default failed and the pool was raised to 100. Apollo Gateway has no dedupe switch; its rows are a second run in this session, not a copy of the rows above.

Constant load, 0% error rate for every gateway:

| Gateway | RPS | Avg (ms) | p50 | p90 | p95 | p99 | p99.9 | CPU max (%) | Mem max (MB) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **prototype-router** | 2,962 | 16.7 | 16.3 | 22.2 | 24.3 | 29.4 | 38.2 | 60 | 55 |
| hive-router | 2,057 | 24.0 | 23.5 | 31.5 | 34.3 | 40.8 | 52.1 | 115 | 119 |
| grafbase | 1,272 | 39.0 | 38.5 | 48.7 | 52.1 | 59.6 | 74.0 | 116 | 77 |
| cosmo | 1,209 | 41.0 | 40.0 | 55.8 | 61.0 | 72.0 | 90.7 | 195 | 169 |
| apollo-router | 571 | 86.5 | 85.8 | 106.6 | 113.1 | 126.1 | 146.9 | 277 | 195 |
| apollo-router-v3 | 524 | 94.2 | 93.5 | 115.4 | 122.1 | 135.7 | 157.0 | 285 | 207 |
| hive-gateway-router-runtime | 485 | 102.0 | 100.6 | 111.6 | 118.6 | 138.1 | 169.9 | 116 | 966 |
| apollo-router-v3-incremental | 442 | 112.3 | 74.6 | 92.7 | 98.3 | 111.0 | 29,492.1 | 272 | 171 |
| apollo-gateway | 338 | 145.9 | 144.1 | 169.3 | 177.3 | 195.2 | 224.2 | 266 | 1,302 |
| hive-gateway | 225 | 219.3 | 209.3 | 284.9 | 309.0 | 337.6 | 493.1 | 135 | 1,204 |

Stress, 0% error rate for every gateway:

| Gateway | RPS | Avg (ms) | p50 | p90 | p95 | p99 | p99.9 | CPU max (%) | Mem max (MB) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| **prototype-router** | 2,841 | 80.1 | 75.6 | 153.0 | 169.2 | 202.1 | 247.3 | 64 | 109 |
| hive-router | 1,919 | 117.7 | 115.1 | 226.7 | 242.9 | 264.5 | 279.4 | 114 | 233 |
| cosmo | 1,253 | 178.2 | 170.9 | 340.7 | 362.9 | 393.9 | 420.5 | 187 | 773 |
| grafbase | 1,145 | 196.0 | 178.6 | 396.1 | 444.9 | 526.5 | 599.5 | 119 | 312 |
| apollo-router-v3-incremental | 535 | 405.6 | 368.0 | 851.6 | 954.6 | 1,144.7 | 1,438.7 | 261 | 672 |
| apollo-router | 487 | 443.1 | 393.8 | 923.8 | 1,052.7 | 1,275.4 | 1,500.7 | 266 | 922 |
| hive-gateway-router-runtime | 450 | 469.3 | 471.8 | 810.5 | 851.3 | 969.1 | 7,053.4 | 114 | 1,524 |
| apollo-router-v3 | 421 | 508.8 | 456.8 | 1,100.0 | 1,219.9 | 1,426.7 | 1,654.1 | 273 | 890 |
| apollo-gateway | 294 | 711.2 | 677.2 | 1,402.6 | 1,567.0 | 1,895.5 | 2,437.4 | 250 | 1,618 |
| hive-gateway | 207 | 820.5 | 772.5 | 1,304.2 | 1,387.9 | 2,845.9 | 21,690.6 | 134 | 1,547 |

`apollo-router-v3-incremental` at constant load and `hive-gateway` under stress kept a 0% error rate, but p99.9 is a single stall (29 s and 22 s) rather than the body of the distribution. p99 for those two rows is 111 ms and 2.8 s.

With dedupe off the prototype did 2,962 RPS at constant load against hive-router's 2,057, about 44% more. On the Mac that gap was about 41% (5,188 vs 3,680). The order under stress is the same as on the Mac: prototype, hive-router, cosmo, grafbase.

#### Query planning, 4 subgraphs

One VU, 1000 iterations, warm and cold. Overhead is cold minus warm. Every gateway had a 0% error rate. Plan shape is sequential steps / subgraph requests (`simple` is 1 / 1 for everyone).

Planning overhead, p50 / p90 / p95 / p99 in ms:

| Gateway | simple | federated | complex |
| --- | --- | --- | --- |
| grafbase | 0.05 / 0.16 / 0.19 / 0.27 | 0.15 / 0.49 / 0.61 / 0.80 | 0.28 / 0.77 / 0.93 / 1.12 |
| **prototype-router (asap)** | 0.03 / 0.09 / 0.11 / 0.25 | 0.20 / 0.41 / 0.55 / 1.24 | 0.37 / 0.62 / 0.75 / 1.32 |
| **prototype-router** | 0.03 / 0.10 / 0.12 / 0.28 | 0.21 / 0.41 / 0.52 / 1.23 | 0.40 / 0.65 / 0.74 / 1.06 |
| hive-router | 0.15 / 0.27 / 0.32 / 0.64 | 0.87 / 1.16 / 1.27 / 1.49 | 1.45 / 1.82 / 1.93 / 2.28 |
| cosmo | 0.28 / 0.38 / 0.42 / 0.62 | 1.92 / 2.29 / 2.59 / 10.74 | 4.41 / 5.01 / 7.43 / 18.12 |
| hive-gateway-router-runtime | 0.56 / 1.08 / 1.62 / 2.36 | 2.03 / 3.24 / 3.54 / 4.46 | 3.13 / 4.61 / 4.87 / 5.69 |
| apollo-router | 0.51 / 0.72 / 0.81 / 1.07 | 2.69 / 3.36 / 3.54 / 4.07 | 4.78 / 5.49 / 5.82 / 6.67 |
| apollo-gateway | 0.55 / 1.30 / 1.61 / 2.13 | 2.73 / 4.09 / 4.68 / 6.48 | 4.48 / 6.07 / 6.92 / 10.87 |
| apollo-router-v3 | 0.57 / 0.77 / 0.85 / 1.02 | 2.78 / 3.43 / 3.71 / 4.28 | 4.85 / 5.61 / 5.94 / 6.59 |
| apollo-router-v3-incremental | 0.57 / 0.79 / 0.89 / 1.12 | 2.92 / 3.60 / 3.78 / 4.37 | 4.87 / 5.73 / 6.08 / 6.78 |
| hive-gateway | 0.98 / 1.48 / 2.72 / 3.79 | 9.66 / 11.74 / 12.40 / 16.85 | 14.32 / 17.63 / 19.21 / 28.90 |

Plan quality:

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

The prototype's plans match the Mac session exactly, including `search` on the large graph below (7 steps in the default mode, 6 with `asap`). Grafbase still has the lowest overhead on `federated` and `complex`. The prototype is next, then hive-router.

#### Query planning, 80 subgraphs

200 iterations. Overhead p50 / p99 in ms. `hive-gateway` aborted with a V8 out-of-memory error during the measured iterations (813 of 2,012 requests succeeded, 40.4%). Its plan was traced before that, in `setup`, so the shape below is from a completed execution. Its overhead percentiles are only the requests that succeeded before the abort. On the Mac the same gateway finished at 84.5% success and did not abort.

| Gateway | product_page | search | checkout | order_history | Success |
| --- | --- | --- | --- | --- | --- |
| grafbase | 5.5 / 6.8 | 2.8 / 4.0 | 0.83 / 2.1 | 1.6 / 3.2 | 100% |
| **prototype-router (asap)** | 8.5 / 12.2 | 3.5 / 5.7 | 1.3 / 2.7 | 2.2 / 3.7 | 100% |
| **prototype-router** | 15.6 / 22.6 | 5.3 / 9.2 | 1.8 / 4.7 | 3.2 / 5.4 | 100% |
| hive-gateway | 124 / 651 | 57 / 367 | 29 / 264 | 51 / 412 | 40.4% |
| apollo-router-v3-incremental | 151 / 188 | 70 / 92 | 26 / 35 | 63 / 82 | 100% |
| hive-router | 313 / 375 | 63 / 83 | 15 / 20 | 27 / 32 | 100% |
| hive-gateway-router-runtime | 380 / 444 | 89 / 137 | 24 / 43 | 39 / 57 | 100% |
| apollo-router-v3 | 464 / 536 | 186 / 247 | 63 / 75 | 103 / 129 | 100% |
| apollo-router | 467 / 542 | 188 / 279 | 63 / 78 | 104 / 122 | 100% |
| apollo-gateway | 556 / 749 | 211 / 262 | 83 / 132 | 135 / 194 | 100% |
| cosmo | 1,213 / 1,409 | 221 / 407 | 45 / 89 | 97 / 181 | 100% |

Plan quality, sequential steps / subgraph requests:

| Gateway | product_page | search | checkout | order_history |
| --- | --- | --- | --- | --- |
| **prototype-router** | 7 / 49 | 7 / 17 | 7 / 51 | 8 / 56 |
| cosmo | 7 / 63 | 6 / 18 | 7 / 63 | 8 / 69 |
| hive-router | 7 / 64 | 6 / 19 | 7 / 63 | 8 / 70 |
| hive-gateway-router-runtime | 7 / 64 | 6 / 19 | 7 / 63 | 8 / 70 |
| **prototype-router (asap)** | 7 / 64 | 6 / 19 | 7 / 65 | 8 / 72 |
| grafbase | 7 / 92 | 6 / 39 | 7 / 97 | 8 / 127 |
| apollo-router, apollo-router-v3, apollo-gateway | 10 / 100 | 6 / 59 | 9 / 78 | 8 / 127 |
| hive-gateway | 11 / 72 | 7 / 19 | 14 / 70 | 11 / 75 |
| apollo-router-v3-incremental | 13 / 91 | 7 / 47 | 14 / 72 | 8 / 113 |

The prototype's plans match the Mac session. Grafbase's fetch counts moved by one or two (Mac was 7/91, 6/39, 7/99, 8/131); steps are inferred from request timing, and the trace keeps the shallowest of three runs. On this VM Grafbase's overhead was lower than `prototype-router (asap)` on every large query, including `product_page`, where the Mac session had asap ahead (6.1 ms vs Grafbase 8.1 ms). That is a different CPU, not a change in the plans.

### Previous results, MacBook Pro

The prototype rows were run on 2026-11-19 at commit `236ce71`; the other gateways' rows are from 2026-11-18. `prototype-router` is the default merge mode; `prototype-router (asap)` is `MERGE_MODE=asap`; The tables headed "dedupe off" were run for every gateway in one session on 2026-11-19.

In short: it passes the whole compatibility audit, plans about as fast as Grafbase and faster than everything else, and sends the fewest subgraph requests of any gateway tested. Its throughput is the highest measured when identical concurrent queries share an execution, which is what the throughput benchmark sends; with dedupe turned off on every gateway it is also the highest, about 40% ahead of Hive Router at constant load and under stress.

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

[GraphQL Gateways Benchmark](https://github.com/graphql-hive/graphql-gateways-benchmark), on the branch of [PR #1041](https://github.com/graphql-hive/graphql-gateways-benchmark/pull/1041), which adds the query planning benchmark and the 80 subgraph supergraph. Every gateway was run locally, one after another in one session on 2026-11-18. The prototype rows were run again on the same machine the next day, after the executor was rewritten, so they are not from the same session as the rows they are compared with.

- Machine: MacBook Pro, Apple M1 Max (10 cores), 64 GB, macOS 27.2, k6 v2.4.0.
- No CPU pinning (macOS has no `taskset`), so k6, the subgraphs and the gateway share all cores. Read the numbers as a ranking on this machine, not as the published CI figures.
- One run per gateway and mode.
- Versions: Apollo Router 2.18.0, Apollo Router 3.0.0-preview.0 (classic and incremental planner), `@apollo/gateway` 2.14.4, Cosmo 0.360.1, Grafbase 0.53.5, Hive Router 0.3.5, `@graphql-hive/gateway` 2.15.2, `@graphql-hive/router-runtime` 1.6.5.

#### Throughput, constant load

50 VUs for 60 s after a 15 s warmup, 4 subgraphs, the same query every time. Every gateway had a 100% success rate.

| Gateway | RPS | Avg (ms) | p50 (ms) | p95 (ms) | p99.9 (ms) | CPU max (%) | Mem max (MB) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **prototype-router (asap)** | 19,978 | 2.3 | 2.2 | 4.1 | 9.2 | 134 | 75 |
| **prototype-router** | 19,741 | 2.4 | 2.2 | 4.1 | 10.6 | 127 | 67 |
| hive-router | 15,365 | 3.1 | 2.6 | 6.3 | 30.2 | 159 | 112 |
| cosmo | 15,363 | 3.1 | 2.9 | 5.4 | 11.9 | 233 | 86 |
| grafbase | 5,942 | 8.2 | 7.3 | 14.4 | 57.4 | 480 | 52 |
| hive-gateway-router-runtime | 5,054 | 9.7 | 9.1 | 12.6 | 35.4 | n/a | n/a |
| hive-gateway | 4,270 | 11.5 | 10.6 | 14.4 | 71.3 | n/a | n/a |
| apollo-router-v3-incremental | 1,595 | 31.0 | 28.8 | 48.3 | 143.1 | 743 | 200 |
| apollo-router-v3 | 1,272 | 38.7 | 37.7 | 54.7 | 86.9 | 790 | 245 |
| apollo-router | 1,250 | 39.5 | 38.4 | 56.1 | 97.2 | 793 | 226 |
| apollo-gateway | 733 | 67.4 | 64.5 | 119.4 | 208.9 | n/a | n/a |

CPU and memory are `n/a` for the Node gateways: on macOS the monitor only follows the launcher process and its direct children, and it missed the Node process.

#### Throughput, stress

Ramp from 50 to 500 VUs and back over 60 s. Every gateway had a 100% success rate.

| Gateway | RPS | Avg (ms) | p50 (ms) | p95 (ms) | p99.9 (ms) | CPU max (%) | Mem max (MB) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **prototype-router** | 21,471 | 7.2 | 5.4 | 19.1 | 48.5 | 136 | 82 |
| **prototype-router (asap)** | 21,343 | 7.2 | 5.4 | 18.8 | 50.7 | 139 | 79 |
| hive-router | 17,653 | 10.3 | 8.4 | 25.7 | 64.2 | 168 | 156 |
| cosmo | 16,383 | 9.8 | 7.5 | 24.7 | 60.4 | 275 | 124 |
| grafbase | 7,172 | 31.3 | 28.9 | 67.1 | 98.8 | 496 | 203 |
| hive-gateway-router-runtime | 5,803 | 37.9 | 37.6 | 67.1 | 98.9 | n/a | n/a |
| hive-gateway | 5,351 | 40.1 | 40.0 | 69.3 | 116.6 | n/a | n/a |
| apollo-router-v3-incremental | 1,610 | 136.6 | 127.8 | 302.6 | 429.2 | 751 | 646 |
| apollo-router | 1,349 | 162.3 | 154.0 | 339.6 | 647.9 | 785 | 1,029 |
| apollo-router-v3 | 1,276 | 171.0 | 162.5 | 364.5 | 523.4 | 798 | 1,053 |
| apollo-gateway | 723 | 294.8 | 286.9 | 643.2 | 840.6 | n/a | n/a |

Dedupe decides these tables because every virtual user sends the same query: identical queries that arrive while one is executing share its response, so most requests cost no subgraph calls. The benchmark turns it on for every gateway that has it: Hive Router, Hive Gateway and Cosmo dedupe whole client requests, as the prototype does, and Apollo Router, Grafbase and Cosmo dedupe identical subgraph requests.

#### Throughput, dedupe off

The same two runs with dedupe switched off on every gateway, so each request is planned from cache, fetched and assembled in full. All of these were run in one session on 2026-11-19. The `prototype-router` and `hive-router` rows were run again later that day, one after the other, after the changes described under the tables; the machine was somewhat slower by then (Hive Router measured 4,038 and 3,688 RPS in the morning), so compare those two rows with each other rather than with the rest.

| Gateway | Setting changed |
| --- | --- |
| prototype-router | `DEDUPE=off` |
| hive-router | `traffic_shaping.router.dedupe.enabled: false` and `traffic_shaping.all.dedupe_enabled: false` |
| cosmo | `ENGINE_ENABLE_SINGLE_FLIGHT=false`, `ENGINE_ENABLE_INBOUND_REQUEST_DEDUPLICATION=false`, and `traffic_shaping.all.max_idle_conns_per_host: 100` (see below) |
| grafbase | `traffic_shaping.inflight_deduplication = false` |
| hive-gateway, hive-gateway-router-runtime | `inboundInflightRequestDeduplication: false` |
| apollo-router (all three) | `traffic_shaping.all.deduplicate_query: false` |
| apollo-gateway | none: it has no dedupe, so its rows are the 2026-11-18 ones from the tables above |

Constant load, 100% success rate for every gateway:

| Gateway | RPS | Avg (ms) | p50 (ms) | p95 (ms) | p99.9 (ms) | CPU max (%) | Mem max (MB) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **prototype-router** | 5,188 | 9.4 | 8.6 | 15.2 | 62.2 | 164 | 77 |
| hive-router | 3,680 | 13.4 | 10.9 | 28.4 | 138.0 | 274 | 153 |
| cosmo | 2,704 | 18.3 | 17.2 | 29.4 | 60.9 | 420 | 232 |
| grafbase | 2,496 | 19.8 | 18.4 | 31.0 | 76.8 | 306 | 75 |
| apollo-router-v3-incremental | 1,393 | 35.5 | 34.6 | 49.7 | 79.1 | 595 | 230 |
| apollo-router | 1,268 | 39.0 | 38.1 | 53.9 | 80.8 | 603 | 279 |
| apollo-router-v3 | 1,124 | 44.0 | 43.1 | 60.1 | 87.3 | 626 | 290 |
| apollo-gateway | 733 | 67.4 | 64.5 | 119.4 | 208.9 | n/a | n/a |
| hive-gateway-router-runtime | 676 | 73.5 | 73.0 | 81.7 | 119.1 | n/a | n/a |
| hive-gateway | 288 | 171.8 | 157.5 | 291.7 | 434.9 | n/a | n/a |

Stress, 100% success rate for every gateway except `hive-gateway`:

| Gateway | RPS | Avg (ms) | p50 (ms) | p95 (ms) | p99.9 (ms) | CPU max (%) | Mem max (MB) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **prototype-router** | 4,829 | 47.1 | 41.6 | 110.4 | 187.1 | 175 | 145 |
| hive-router | 3,467 | 65.5 | 63.8 | 137.7 | 199.8 | 290 | 287 |
| cosmo | 2,618 | 86.0 | 85.4 | 172.1 | 231.8 | 428 | 822 |
| grafbase | 2,278 | 99.4 | 87.9 | 230.4 | 330.0 | 319 | 299 |
| apollo-router-v3-incremental | 1,193 | 185.9 | 168.2 | 424.6 | 602.3 | 611 | 784 |
| apollo-router | 1,099 | 201.0 | 183.7 | 461.4 | 624.4 | 609 | 1,086 |
| apollo-router-v3 | 955 | 229.9 | 210.1 | 523.4 | 719.7 | 637 | 1,092 |
| apollo-gateway | 723 | 294.8 | 286.9 | 643.2 | 840.6 | n/a | n/a |
| hive-gateway-router-runtime | 651 | 340.0 | 331.2 | 663.5 | 3,736.5 | n/a | n/a |
| hive-gateway | failed | | | | | n/a | n/a |

- `hive-gateway` stopped accepting connections about 45 s into the stress run, at around 400 VUs, in both of two attempts (4% and 10% of requests succeeded). Until then it served about 265 RPS.
- Cosmo needed a larger idle connection pool to be measurable here. With its default of 20 idle connections per subgraph it opened new connections fast enough to use up all 16,384 of macOS's ephemeral ports within ten seconds, and requests then failed in k6 (511 RPS, 96% success). The rows above are with the pool raised to 100, which is Cosmo's own per-host connection limit.
- Cosmo and Grafbase were run a second time for these rows; Grafbase's first run agreed with the one shown (2,388 and 2,250 RPS).

Before the executor was rewritten (commit `aff2581`, no dedupe) the prototype measured 1,739 RPS at constant load and 1,513 RPS under stress, where its memory reached 1.2 GB. At commit `a90aae6` it measured 3,671 and 3,643 RPS. What changed after that:

- The subgraphs use about three times the CPU the gateway does in this benchmark, so most of the gain is in what they are asked: no `__typename` and no type condition where the type is known, no key field selected twice, and one `_entities` field for every place that asks the same of the same entities. The one large request of this query became about a quarter cheaper for its subgraph, and throughput rose by about 30% with the response byte for byte the same.
- Subgraph requests go through a small HTTP/1.1 client instead of `reqwest`, whose connection pool lock was where the gateway's threads waited. With the smaller responses to parse, gateway CPU per request roughly halved.
- At most 100 requests are in flight per subgraph (`MAX_CONNECTIONS`), the same default as Hive Router and Cosmo. It does not change throughput; under stress it brought p99.9 from about 230 ms to about 185 ms over two runs each.

Run again that day on the build of `a90aae6`, under stress and in the same slower conditions as the two rows above, it measured 3,504 and 3,544 RPS.

#### Query planning, 4 subgraphs

One VU sends each query warm (cached) and cold (unique operation name and aliases, so the plan cache misses). Planning overhead is the paired `cold - warm` latency: parsing, validation, normalization and planning. 1000 iterations; every gateway had a 100% success rate.

Planning overhead p50 in ms:

| Gateway | simple | federated | complex |
| --- | --- | --- | --- |
| grafbase | 0.03 | 0.10 | 0.19 |
| **prototype-router (asap)** | 0.03 | 0.12 | 0.27 |
| **prototype-router** | 0.03 | 0.14 | 0.30 |
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
| **prototype-router (asap)** | 6.1 | 3.0 | 1.1 | 1.9 |
| grafbase | 8.1 | 2.5 | 0.9 | 1.7 |
| **prototype-router** | 12.0 | 4.0 | 1.3 | 2.5 |
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

- With slack merging the prototype sends 19% to 23% fewer requests than Hive Router on `product_page`, `checkout` and `order_history` at the same depth, and plans 12 to 24 times faster.
- On `search` the default mode measured 7 steps where `asap` and most other gateways measured 6. Merging is meant not to deepen the plan, so this needs a look. Steps are inferred from request timing, not read from the plan.
- Planning overhead covers everything a gateway does for a new operation. The prototype does less there than the others (see [Limitations](#limitations): no variable validation, no authorization directives), which flatters its numbers.
- Subgraph requests count HTTP requests. The prototype puts several entity fetches in one request under aliases; the other gateways send one fetch per request.
- The planning numbers of the prototype roughly halved between the two runs without a change to the planner: the build now uses mimalloc and link-time optimization. The plans themselves are the same.

### Reproducing

Audit: copy [`harness/federation-gateway-audit`](harness/federation-gateway-audit) to `gateways/prototype-router` in the audit repository, then:

```sh
cd gateways/prototype-router && PROTOTYPE_ROUTER_DIR=/path/to/prototype-router ./install.sh && cd ../..
make test-prototype-router                   # default merge mode
MERGE_MODE=asap make test-prototype-router
```

Benchmark: add a `gateways/prototype` directory to the benchmark repository whose `run.sh` starts the release binary with `SUPERGRAPH_PATH` (default `../apollo-router/supergraph.graphql`), start the subgraphs with `make run-subgraphs`, then run `make test gateway=prototype mode=<constant|stress|planning|planning-large>`. For the dedupe-off tables, prefix it with `DEDUPE=off` and change the other gateways' settings as listed there.

## Limitations

- No introspection (`__schema`, `__type`) beyond `__typename`.
- No subscriptions, no `@defer`.
- No progressive `@override` (`label:`), no `@authenticated` / `@requiresScopes` / `@policy`.
- Variable values are not validated against the schema; only literals are.
- Subgraph errors are passed on with the message and the subgraph name, without paths.
- The planner's maps use a fast, non-collision-resistant hash, and the plan cache is unbounded. Do not expose it to untrusted clients.
- No header forwarding, timeouts, TLS configuration or telemetry. A subgraph request is only sent again when it got no answer at all, and a mutation never is.
- Subgraphs at `http://` URLs are called by a small HTTP/1.1 client of its own ([`src/http.rs`](src/http.rs)); other URLs go through `reqwest`.
