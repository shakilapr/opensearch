# Query planning (`search-planner`)

Query planning is an **opt-in** stage that runs *before* retrieval. It is implemented by the
`search-planner` crate (`crates/planner`), which implements `search_engine::QueryPlanner` and is
wired into `agent0 serve`. It deconstructs a query into a small number of **expansion probes** —
canonical entity names, phrase-with-slop probes — which the engine then searches as §8 query
variants alongside the original. When it is off, or when it fails, search behaves exactly as
before: only the original query is searched.

## Why it exists

The deterministic analyzer (`crates/engine/src/analyze.rs`) can split a query, pull out
identifiers and quoted phrases, and classify intent, but it cannot read a conversational turn,
resolve a pronoun, or name the canonical entity behind a paraphrase. Those are model jobs. The
`QueryPlanner` seam is where they plug in — the same shape as `Answerer` and `EvidenceVerifier`:
synchronous, total, and consumed only through its output, so the engine stays deterministic and
offline.

## Enable it

Query planning is disabled unless **both** conditions hold:

1. `SEARCH_PLANNER_ENABLED` is truthy (`1`, `true`, `on`, `yes`); and
2. `CROWBOT_API_KEY` is set and non-empty.

`search_planner::from_env()` then returns `Some(LlmQueryPlanner)`; otherwise it returns `None` and
the caller injects no planner. `agent0 serve` logs exactly one info line naming the model it chose,
or the reason it is off (feature disabled vs. missing key).

## Environment variables

| Variable | Default | Meaning |
| --- | --- | --- |
| `SEARCH_PLANNER_ENABLED` | unset (off) | Opt-in switch. Unset/`0` = disabled. |
| `CROWBOT_API_KEY` | — | Credential. Read **only** from the environment; never logged or written to disk. |
| `SEARCH_PLANNER_MODEL` | `deepseek-v4.1-flash` | Model id for planning. Point it at a stronger model to trade latency for plan quality; the on-disk cache means each query is paid for once. |
| `SEARCH_PLANNER_BASE_URL` | `https://api.crowbot.sh/v1` | OpenAI-chat-compatible endpoint. |
| `SEARCH_PLANNER_TIMEOUT_S` | `180` | Per-request HTTP timeout, seconds. |
| `SEARCH_PLANNER_CONCURRENCY` | `2` | Bound on simultaneous in-flight planning calls. |
| `SEARCH_PLANNER_MAX_EXPANSIONS` | `3` | Cap on expansions returned per query. The engine caps again (`plan::MAX_QUERY_VARIANTS` is 4 including the original). |
| `SEARCH_PLANNER_CACHE` | `<SEARCH_LLM_CACHE>/planner` (else `$SEARCH_DATA_DIR/llm-cache/planner`) | Cache directory. A distinct subdirectory keeps plan entries from interleaving with organize/answer entries that share the parent. |

A reasoning model (id containing `thinking`/`reason`/`r1`/`o1`) spends part of its budget on a
hidden trace, so the underlying client raises `max_tokens` to at least **8192** — without it the
model finishes with `finish_reason=length` and null content (observed live). The default model is
exactly that case, which is why the default timeout is a long 180 s.

## What the planner returns, and how it is used

The prompt and the response parser live in `search-organize`'s `queryplan` module
(`crates/organize/src/queryplan.rs`) — the live-verified implementation — and `search-planner`
owns only the *synchronous* engine seam and the mapping onto the engine's probe list. One prompt
for one job.

The model is asked for one JSON object:

```json
{"query": "+flash +attention +memory -economic", "entities": ["FlashAttention"],
 "phrases": ["flash attention"], "negatives": ["economic"]}
```

`query` is a **Tantivy query string**, so the indexer's existing syntax is the contract:
`+term` required, `-term` excluded, `"a b"~3` phrase proximity. The indexer needed no change for
this — the parser already honours all three. Multi-word `phrases` are folded into `query` as
`"a b"~3` by the parser, because retrieval only ever sees one string and a live run showed the
model reporting phrases separately (a phrase it identified would otherwise be silently lost).

`search-planner` maps that onto `search_engine::PlannedQueries`:

* **`expansions`** — the rewritten `query`, when it differs from the original. The engine searches
  it as one §8 variant, in a rewrite family, alongside the original (family 0), and fuses per
  family under the §10 cap that stops a cluster of rewrites from outvoting the query the user
  typed. A query the planner left unchanged yields **no** variant — the identity case.
* **`notes`** — the canonical entities, phrases and negatives, surfaced in the response `trace`.
  This is what makes the decision auditable per request, and the entities are the input the §9.4
  graph walk wants.

Every expansion is still subject to `plan::variant_preserves_constraints`: a probe that drops an
identifier, a negation, a date, or a quoted phrase the query had is **rejected** downstream, so a
planner cannot smuggle in a rewrite that loses a constraint.

## Prompt shape

The prompt is `search-organize`'s (`build_query_prompt`): it teaches the Tantivy syntax
(`+term`, `-term`, `"a b"~3`), forbids answering the question, and states the JSON contract. It
embeds the question verbatim and, when supplied, at most **6** prior conversation turns (for
resolving pronouns). The engine currently passes **no** history — it has no conversation state —
so this is the forward-looking half of the contract.

## Cost and latency

One completion per uncached query, bounded by `SEARCH_PLANNER_TIMEOUT_S` and the client's token
budget, with at most `SEARCH_PLANNER_CONCURRENCY` calls in flight. **A cached query costs nothing
and performs no network call** — which is what makes a slower planner model affordable: a query
is paid for once, then planned from disk forever. The default is crowbot's `deepseek-v4.1-flash`;
set `SEARCH_PLANNER_MODEL` to trade latency for a stronger model per deployment.

## Cache

The crate reuses `search-organize`'s client rather than re-implementing cache/single-flight/
concurrency. One file per `(base_url, model, prompt)`, keyed by `sha256(base_url \0 model \0
prompt)`, at `<cache>/<first two hex>/<hash>.json`. Writes are atomic (temp file + rename), so a
partial write can never poison the cache, and a corrupt entry degrades to a fresh call rather than
a hard error. `LlmQueryPlanner::cache_stats()` reports hits, misses and entry count.

## Failure is never fatal

`QueryPlanner::plan` is **total**: a transport error, a timeout, or an unparseable response
becomes an **empty plan plus a note** (`planner unavailable: …` / `unparseable model output (…)`),
which is exactly the no-planner behaviour. A search can never fail, or change its results, because
query planning failed. The client also moves the call to a fresh thread when invoked from inside a
Tokio runtime, so it never nests `block_on` on the caller's runtime.

## Measuring it

The engine's ablation harness (`crates/bench`) and the acceptance suite measure the engine over a
fixture; neither currently exercises a live planner (it needs a key). What is pinned hermetically
is the *mechanism*: that expansions are searched, that the family cap holds, that an empty or
failing plan is a no-op, and that the planner reaches the engine end to end through
`POST /search {engine:true}`. The prompt itself was checked live against crowbot (three questions,
9.3 s — see the `queryplan` commit); the *quality* delta on the corpus is still unmeasured.
