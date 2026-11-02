# Documentation

One architecture document, one plan, one status, one benchmark record, one work queue. Everything
else is either a frozen contract (`specs/`) or history (`legacy/`).

## Read in this order

| File | What it is | Changes when |
| --- | --- | --- |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | the system: principles, storage layers, index model, matrices, retrieval, invariants, code map | the design changes |
| [`PLAN.md`](PLAN.md) | phases, order, and the gate that decides each | a phase starts or ends |
| [`STATUS.md`](STATUS.md) | measured state and what is left, ordered by dependency | after any measurement |
| [`BENCHMARKS.md`](BENCHMARKS.md) | every number the other documents quote, with its command | after any run |
| [`WORK-QUEUE.md`](WORK-QUEUE.md) | hours-sized, self-contained tasks for coding agents | when a task is finished or added |
| [`CONCEPT-RANKING.md`](CONCEPT-RANKING.md) | the analysis for ranking on concepts and relatedness rather than words | the ranking design changes |
| [`OPERATIONS.md`](OPERATIONS.md) | deploy, rebuild, monitor, troubleshoot | an operational step changes |

## `specs/` — frozen contracts

Implementations must match these; they are not revised casually.

| File | Covers |
| --- | --- |
| `ENGINE_SPEC.md` | `crates/engine` interfaces and the ranking cascade |
| `STAGE0_SPEC.md` … `STAGE10_SPEC.md` | the stage-by-stage contract of the original build |
| `SEMANTIC_INDEX_ARCHITECTURE.md` | the identity and semantic schema |
| `CRAWLER.md` | crawling and scraping (the universal crawler design) |
| `AGENT_NATIVE_SEARCH_ENGINE.md` | the product mission and surfaces |
| `ANSWER.md`, `PLANNER.md`, `MCP.md` | the optional query-time stages and service surfaces |
| `PORTABLE_MODEL.md`, `DISTRIBUTION.md` | dataset packs and their transport |
| `ROADMAP.md`, `DECISIONS.md`, `CONTRIBUTING.md` | direction, recorded decisions, contribution rules |

## `legacy/` — history, not guidance

Superseded designs and dated snapshots, kept because the reasoning still explains decisions:
`architecture-update-v2.md` (the original v2 target), `PLAN-v2.md` / `EXECUTION-PLAN-v2.md` /
`EXECUTION-STATUS-v2.md` (the phased execution that produced the current design), `STAGE_STATUS.md`,
`current-architecture.md` (the 2026-10-27 snapshot), `index-upgrade-plan.md`,
`semantic-brain-design.md`, `analysis-next-move.md`, `next-move.md`, `work-plan.md`,
`QA_AUDIT_2026-10-11.md`, `SLOC_AND_DATA_REPORT.md`, `recieved.md`, `to-be-sent.md`.

If a legacy document contradicts `ARCHITECTURE.md`, the architecture wins. If a legacy document is
the only place a decision's reasoning lives, move that reasoning into `ARCHITECTURE.md` (or
`DECISIONS.md`) rather than citing it.

## House rules

- Numbers in `ARCHITECTURE.md`, `PLAN.md` and `STATUS.md` must be reproducible from
  `BENCHMARKS.md`; a number without a command is a rumour.
- A document that describes a component must name the crate that implements it.
- When a phase ends, update `STATUS.md` and `BENCHMARKS.md` in the same commit.