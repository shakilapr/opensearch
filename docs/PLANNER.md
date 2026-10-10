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
2. `KITANI_API_KEY` is set and non-empty.

`search_planner::from_env()` then returns `Some(LlmQueryPlanner)`; otherwise it returns `None` and
the caller injects no planner. `agent0 serve` logs exactly one info line naming the model it chose,
or the reason it is off (feature disabled vs. missing key).

## Environment variables

| Variable | Default | Meaning |
| --- | --- | --- |
| `SEARCH_PLANNER_ENABLED` | unset (off) | Opt-in switch. Unset/`0` = disabled. |
| `KITANI_API_KEY` | — | Credential. Read **only** from the environment; never logged or written to disk. |
| `SEARCH_PLANNER_MODEL` | `deepseek/deepseek-v4.1-flash-thinking` | Model id. The **slow reasoning** model by default; set it to `deepseek/deepseek-v4.1-flash` to trade latency for cost. |
| `SEARCH_PLANNER_BASE_URL` | `https://api.kitani.ai/v1` | OpenAI-chat-compatible endpoint. |
| `SEARCH_PLANNER_TIMEOUT_S` | `180` | Per-request HTTP timeout, seconds. |
| `SEARCH_PLANNER_CONCURRENCY` | `2` | Bound on simultaneous in-flight planning calls. |
| `SEARCH_PLANNER_MAX_EXPANSIONS` | `3` | Cap on expansions returned per query. The engine caps again (`plan::MAX_QUERY_VARIANTS` is 4 including the original). |
| `SEARCH_PLANNER_CACHE` | `<SEARCH_LLM_CACHE>/planner` (else `$SEARCH_DATA_DIR/llm-cache/planner`) | Cache directory. A distinct subdirectory keeps plan entries from interleaving with organize/answer entries that share the parent. |

A reasoning model (id containing `thinking`/`reason`/`r1`/`o1`) spends part of its budget on a
hidden trace, so the underlying client raises `max_tokens` to at least **8192** — without it the
model finishes with `finish_reason=length` and null content (observed live). The default model is
exactly that case, which is why the default timeout is a long 180 s.

## What the planner returns, and how it is used

The model is asked for one JSON object:

```json
{"expansions": ["paged attention", "\"memory complexity\"~3"], "notes": "entities + phrase"}
```

Each probe is a **Tantivy query string**, so the indexer's existing syntax is the contract: quoted
phrases with proximity slop (`"memory complexity"~3`), field-free terms, `+`/`-` operators. The
indexer needed no change for this — the parser already honours `"a b"~N`.

The engine then, for each planned variant (the original first, family 0, then each expansion under
a shared rewrite family):

* searches it in every lane alongside the original, and
* fuses per family, applying the §10 rule that a cluster of near-identical rewrites must not
  outvote the query the user typed.

Every expansion is still subject to `plan::variant_preserves_constraints`: a probe that drops an
identifier, a negation, a date, or a quoted phrase the query had is **rejected** downstream, so a
planner cannot smuggle in a rewrite that loses a constraint. The response `trace` records what the
planner produced (`planner: N expansion probe(s) [...]`) and any notes, so the decision is
auditable per request.

## Prompt budget

Constants in `crates/planner/src/prompt.rs`:

* **at most 4000 characters total** (`MAX_PROMPT_CHARS`) — never exceeded on the wire;
* **at most 200 characters per probe** (`MAX_EXPANSION_CHARS`), truncated not dropped;
* at most 12 analysis hint terms per category (identifiers, phrases, content terms);
* the free-text note is truncated to 300 characters.

The prompt text carries a version (`PROMPT_VERSION = "plan-v1"`) that is part of the cache key, so
a prompt change never reuses a plan minted under older instructions.

## Cost and latency

One completion per uncached query, bounded by `SEARCH_PLANNER_TIMEOUT_S` and the client's token
budget, with at most `SEARCH_PLANNER_CONCURRENCY` calls in flight. **A cached query costs nothing
and performs no network call** — which is what makes the slow reasoning model affordable: a query
is paid for once, then planned from disk forever.

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
`POST /search {engine:true}`.
