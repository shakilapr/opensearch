# The portable knowledge model

A compiled, self-contained knowledge database built from **real scraped web data**, structured
offline by a cheap model, and queryable by any AI on another machine.

    scrape (machine A)            compile (machine A)                  query (machine B)
    ─────────────────             ───────────────────                  ─────────────────
    agent0 ingest <url> ...  ──▶  agent0 export <dir> [--organize]  ──▶ agent0 import <dir>
    live index (tantivy)          portable pack (JSONL + manifest)      agent0 serve / search-mcp

Nothing is distilled and no model weights are produced: the artifact is a **hardcoded database of
real web pages** plus the structure a cheap LLM gave them. The pack format is the frozen
Stage-6 format (`docs/specs/STAGE6_SPEC.md`); this work adds the two verbs and the seams they need,
not a new format.

## Commands

    # 1. Compile the live index into a portable pack.
    #    --raw            copy the crawler's own fields, no model call (free, fast)
    #    (default)        structure each page with the cheap model
    #    --limit N        cap the number of pages
    #    --name/--version label the pack
    SEARCH_DATA_DIR=/var/lib/search agent0 export /tmp/web-pack --name web --version 1

    # 2. Load a pack anywhere. No network, no robots, no frontier — pure ingest.
    SEARCH_DATA_DIR=/tmp/other-machine agent0 import /tmp/web-pack

    # 3. Serve it to any agent/AI.
    SEARCH_DATA_DIR=/tmp/other-machine SEARCH_BIND=127.0.0.1:8080 agent0 serve
    # or the MCP server:  search-mcp

`import` is idempotent: re-importing the same pack reports `unchanged` and does not duplicate
documents. It reuses the same non-fetching ingest seam as the crawler (`import_record`), so an
imported page becomes a first-class document — searchable, addressable, identical to a crawled one.

## The pack

    <dir>/manifest.json          name, version, schema, created_at, per-shard checksums
    <dir>/<name>-00000.jsonl     one JSON record per line (streamable, diffable)

`export` reads the index through `Indexer::all_docs()`, which folds the live base ∪ overlay by
`doc_id` (newer wins), drops tombstones and not-yet-visible docs, and returns docs sorted by
`doc_id` — so **two exports of the same index are byte-identical**. Export is lossy by design in
one direction: `headings`/`entities` are indexed but not `STOREd`, so they are not carried.

## Structuring with the cheap model

`crates/organize` (`search-organize`) turns one scraped page into one pack record: a `summary`,
`topics`, `doc_type`, `language`, `key_entities` and an `id_hint`, with the source text kept as
the record text. Design points that matter in practice:

* **Cache.** Completions are cached on disk, keyed on `(base_url, model, prompt)`, with atomic
  writes and single-flight per key. Re-running an export over already-organized pages costs
  nothing and is the normal way to resume after a failure.
* **Bounded parallelism.** `SEARCH_LLM_CONCURRENCY` (default 8) caps in-flight requests; the
  fan-out is a task set, so a batch of hundreds of pages is a configuration change, not a code one.
* **Escalation, once.** If the primary model's output fails validation, the escalation model — a
  *different* model id, so the escalation is not a silent retry of the same model — is asked once.
  A reasoning model needs a large `max_tokens` budget: with a small one it spends it on the hidden
  trace and returns `finish_reason=length` with `content: null`, so the budget is raised
  automatically for an id containing `thinking`/`reason`/`r1`/`o1`.
* **Timeout.** 180 s. A generous cap rather than a measured need: the providers in use answer in
  ~3 s even on a 24,000-character prompt (measured 2026-10-13), and the headroom covers a longer
  page plus a fallback that has to be tried after the primary stalls.
* **Partial batches survive.** One page failing does not discard the batch; failures are reported
  per URL and counted in the export summary.
* **Incremental and resumable (`PackWriter`).** Records are written to disk as they are produced —
  a shard every 500, with the manifest rewritten atomically after each one — so a killed export
  keeps everything written so far. A re-run reads the record ids already in the pack and skips
  those pages, continuing the same shards. Before this the record set was buffered and written
  once at the end, so a 6-hour export of ~87k pages left an **empty** directory every time and
  could never finish inside its timeout. Resume is by content (record id), not by count, so it
  needs no bookkeeping file. A manifest that fails validation is an error, never a silent restart.

Environment: `CROWBOT_API_KEY` (required; never hardcoded), `CROWBOT_BASE_URL`,
`CROWBOT_MODEL` (default `deepseek-v4.1-flash`),
`CROWBOT_ESCALATE_MODEL` (default `deepseek-v4.1-flash`),
`CROWBOT_TIMEOUT_S`, `SEARCH_LLM_CACHE`, `SEARCH_LLM_CONCURRENCY`.

## Measured (2026-10-09, this host)

Over a real harvest of 25 Wikipedia / docs.rs / arxiv pages (`agent0 ingest` per URL):

| step | result |
| --- | --- |
| scrape + index | 25/26 URLs ingested; the one failure was a real HTTP 404 |
| `export --raw` | 25 records, one shard, checksummed manifest |
| `import` into a **fresh** data dir | 25 imported, 0 unchanged, 0 failed |
| search the fresh index | real hits with real titles (`Inverted_index`, `Information_retrieval`, …) |
| `export` (organized) | 24–25 records structured by `deepseek-v4.1-flash` — served by the kitani relay on that date; that provider is retired, the model id is now served by crowbot — ~300 ms–48 s per page |
| cache after a re-run | 27 hits / 2 misses — re-runs of organized pages are free |

## Honest limits

* `doc_type`/`language` live in the record `extras`, not in the Tantivy schema, so they are not
  yet searchable facets — they travel in the pack and can be promoted later.
* Export is lossy on non-`STOREd` fields (`headings`, `entities`).
* Single box: no sharded/parallel export; the pack is written by one process.
* The organizer trusts the model for `summary`/`topics` quality; failures are detected structurally
  (schema/parse), not semantically.
