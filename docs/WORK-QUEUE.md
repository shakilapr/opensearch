# v2 work queue — delegable tasks

> The ordered *status* list is `docs/STATUS.md`; this file is the task detail (steps, pitfalls,
> gates) for the same work. Start there, pick the top unblocked item, take its detail here.

For coding agents (opencode, Claude Code, Codex) working unattended. Each task is **self-contained**,
sized to hours not minutes, and ends in a measurement. Read `docs/ARCHITECTURE.md` first: it is
the normative design, and a change that contradicts a named section is not an implementation.

A task is done when: code merged, tests pass, **its gate measured and written into
`docs/STATUS.md`**, and committed. A task whose gate cannot be measured is not done —
say so instead of reporting success.

---

## 0. Ground rules for the agent

```bash
export PATH="$HOME/.cargo/bin:$PATH"          # cargo is user-local on this host
cd /root/projects/search

# the verification that must pass before any commit
cargo test -p search-indexer -p search-agent0 -p search-api -p search-engine -p search-features
cargo build --release -p search-agent0         # default features include bge
```

Hard rules, each learned from a real failure:

1. **Never invent a number.** If a measurement did not run, write "not measured" and why.
2. **A schema change ships with a rebuild.** `scripts/rebuild-fleet-index.sh` (under `systemd-run`).
   Changing fields without rebuilding leaves every index unopenable (`_schema_hash` guard).
3. **Run long jobs under `systemd-run`** with `MemoryMax=16G` and `CPUQuota` — a normal process is
   OOM-killed at ~4 GB by `CONSTRAINT_MEMCG` even with 60 GB free.
4. **Never `pkill -f "agent0 crawl"`** from a tool call: the pattern matches the invoking shell. Kill
   by PID, or use the seed-file-qualified pattern in `scripts/restart-crawlers.sh`.
5. **Two processes may not write one SQLite root.** The crawler holds the writer; a batch pass must
   either stop it or use small batches with `busy_timeout` (and never two parallel writers).
6. **Commit per slice**, conventional commits (`feat:`, `fix:`, `test:`, `docs:`, `perf:`), clean
   working tree at hand-off. Do not batch unrelated work into one commit.
7. **Do not delete data.** `text/`, `blobs/`, `meta.sqlite`, `vectors.bin` are not yours to remove;
   propose it in the report instead.
8. **The deployed service is production.** `systemctl restart search-api` is allowed after a green
   build; anything destructive is not.
9. **Don't touch `/etc/systemd` drop-ins without reconciling all of them** — additive files silently
   conflict (a stale `no-lanes.conf` once overrode a measured weight of 0.0 for two days).
10. **Read the failure, not the symptom.** A lane that returns nothing, a build that "is slow", a test
    that "flakes" each has a known cause listed in §2; check it before redesigning.

One task per agent, one task per branch/worktree, never two agents on one data root:

```bash
git worktree add ../wt-A2 -b task/A2-index-audit main
cd ../wt-A2 && opencode run --model <model> \
  "$(sed -n '/^### A2 /,/^### A3 /p' docs/WORK-QUEUE.md)"
```

Data roots: `/var/lib/search` (primary, 19K docs) and `/var/lib/search-fleet/shard-{0..3}`
(1.54M docs). Serving: `127.0.0.1:8090`, federated over all five.

Quality bars that apply to every task:

```bash
# a graded run (600 cases, ~7 min): one process, fast mode, federated like serve
SEARCH_DATA_DIR=/var/lib/search SEARCH_READ_ONLY=1 \
  ./target/release/agent0 eval run --mode fast --lanes all --limit 10 --out ops/<name>.json

# lane deltas, one config per process, no inherited weights
scripts/eval-matrix.sh
```

---

## 1. Track A — make the rebuilt index lossless (highest value first)

The IM-3 rebuild proved the model, and exposed a gap: `agent0 index build` sets
`headings: String::new()` and `entities: vec![]`, so a rebuilt fleet index has **no headings and no
entities** — the two fields boosted at 2.0 and 3.0. Search still works; it ranks worse than the
ingest path would. This track closes that and makes it verifiable.

### A1 — ✅ done: persist headings and entities (migration 0009)

`document_derived(docver_id, headings, entities)` is written in the same commit as the document and
read by `agent0 index build`; applied to all five roots. Follow-up, still open:

**A1b — backfill the pre-existing corpus (~6 h).** 1.56M documents have no `document_derived` row.
Entities can be recovered from the pre-rebuild index (`tantivy.fulltext-<ts>` still holds STORED
`entities`); headings cannot, because they were indexed-only. Options: (a) recover entities from the
old index into `document_derived`; (b) re-crawl the top-N documents by inbound link or query traffic.
Gate: count of documents with non-empty headings before/after, and the 600-case nDCG delta.
Do not claim the ranking effect is known before measuring it — the title-derived set cannot see it.

### A2 — `agent0 index audit` (~3 h)

*Do:* one verb that reports, per root: index doc_count (from Tantivy, not the manifest),
`document_version` count (newest per resource, with text), documents missing an excerpt, documents
with empty headings, and index bytes/doc.
*Gate:* the numbers match `scripts/rebuild-fleet-index.sh`'s report on all five roots; run it in CI
style (exit non-zero when doc_count < 95% of metadata).
*Why it matters:* `_manifest.json` is only written by `publish_manifest`, so it drifts; every "the
index is behind" question this week was answered by hand with `count_docs`.

### A3 — excerpt quality: several windows, not the first 500 chars (~4 h)

*Why:* the excerpt is now the only body text the index keeps, so its content *is* the lexical body
signal. The first 500 characters of a scraped page are often navigation.
*Do:* implement an extractive selector in `excerpt_of` with evidence: score sentences by
query-independent salience (term density, position after the longest heading, presence of digits/
entities), keep ~500 chars. Keep it deterministic and cheap (<50 µs/doc).
*Gate:* on 200 sampled documents, human/LLM-judged "does the excerpt contain the page's subject?"
— report before/after; and 600-case nDCG must not drop.
*Risk:* any model-based selector in the index path is a violation of P1 if it is not batched. Keep
it rule-based or batch it.

### A4 — verify `/read` and export against the new model (~3 h)

*Do:* golden test: for 100 random documents across roots, `/read` text equals the text file; export
contains excerpts and hydrates bodies; `portable.rs` refuses (or counts) documents without text
instead of shipping empty bodies.
*Gate:* 100/100 byte-equal; export/import round-trip preserves ids and excerpts.

---

## 2. Track B — the matrices: coverage is the weak point

15 concepts over 1.56M documents is a lexicon, not a taxonomy. `doc_concept` covers 56.8% of
documents, `source_concept` and `concept_relation` are empty, and `web_link` was never captured.

### B1 — grow the concept lexicon to ~300, validated by reading titles (~8 h)

*Do:* extend `crates/retrieval/src/concept_tag.rs` (or the data file it loads) with concepts across
the corpus's real shape (software/docs, news, science, commerce); every concept gets aliases,
`match_name` (false for corpus-dominant homonyms), and a sense rule where two senses collide.
*Validation is mandatory:* for each new concept, print the top 20 tagged titles and read them. Count
false positives. A concept that tags >20% of the corpus is downweighted, never deleted.
*Gate:* ≥250 concepts; mean false-positive rate ≤10% on sampled tags; `concepts build --rebuild`
purges its model_version first (otherwise stale rows masquerade as success); 600-case nDCG not
worse; the oxytocin cross-disciplinary case still resolves.
*Pitfalls:* tag with a lexicon, not an LLM per document (750–800 docs/s vs hours); never slice a
`String` at a byte offset (CJK/Cyrillic queries panic); the paging cursor must be the column the
query orders *and* filters on.

### B2 — typed concept graph from a real taxonomy (~6 h)

*Do:* import OpenAlex subfields/domains for the scholarly slice, plus hand-written `related`/
`mechanism_of` edges for the top 50 concepts. Bound expansion to 1–2 hops and mark everything
inferred.
*Gate:* an expansion query adds candidates, is marked `inferred`, and does not reduce recall@20 on
the 600 set; publish traversal depth and candidate cap.

### B3 — source profiles, normalised (~5 h)

*Do:* aggregate `doc_concept` → `source_concept` with `doc_count`, `coverage`, concentration
(share of the source's documents), and a quality signal; never a raw weight sum.
*Gate:* on 50 queries where the right site is known, source recall@10; report the number even if
poor.

### B4 — capture the link graph at ingest (~6 h)

*Why:* `web_link` is empty and raw HTML is no longer retained (`SEARCH_KEEP_RAW_BLOBS` off), so the
navigation graph is currently unrecoverable. It must be captured before any future blob deletion is
even discussed.
*Do:* extract anchors/hrefs in the parser path, canonicalise targets, write `web_link` rows in the
ingest batch (one extra statement, not a second commit).
*Gate:* 1,000 ingested pages → edges per page and the share resolving to known `resource_id`s;
commit throughput unchanged (±5%).

### B5 — passage layer for the top of the result list (~6 h)

*Do:* split documents into ≤1,200-char passages at ingest (headings as boundaries), write `passage`
and `passage_concept` for documents that pass a quality threshold. Keep it opt-in per crawl.
*Gate:* passage-level rerank of the top 20 beats document-level on a 50-case hand-judged set.

---

## 3. Track C — make the learned sparse memory earn a weight

The encoder is trained and wired, and ships at 0.0. These tasks change that only if the measurement
does.

### C1 — train Model B on titles as well as bodies (~5 h)

*Why:* the 600-set labels a document by its own title; the encoder learned a body-similarity space,
so it cannot answer a title query. It cost 0.091 nDCG at w=0.8 for exactly that reason.
*Do:* build training pairs where `e_d` is the title embedding (and title+body), keep the same
`TopK(ReLU(We+b))` with the tied decoder, and evaluate **retrieval**, not reconstruction.
*Gate:* `agent0 features probe --set benchmarks/queries.corpus.jsonl` recall@100 before/after; then
the fusion sweep (0.1/0.2/0.4/0.8) on the 600 set, one process per config.

### C2 — IDF-weight the postings (~3 h)

*Do:* compute document frequency per feature and apply `idf_f` at query time (store `df` in the
postings header; it is one u32 per feature).
*Gate:* feature-lane nDCG at w=0.4 improves over C1's result; lane latency within 10%.

### C3 — guarantee a weight-0 lane costs nothing (~2 h)

*Do:* implement `lane_hint` on every lane that can ship at 0.0, and add a test that a 0-weight
configuration makes zero lane calls (`lane_ranks == {}`).
*Gate:* engine p50 with all weights 0.0 drops to the legacy path + ≤20 ms.

### C4 — a query encoder as its own model (Model D) (~8 h)

*Do:* train a small linear/projection query→feature map on (body-span query, document) pairs from
`benchmarks/queries.body.jsonl` + mined pairs; CPU, <2 ms/query.
*Gate:* `features probe` recall@100 over the body-span set beats the projected-teacher baseline,
and end-to-end p95 stays within budget.

---

## 4. Track D — ranking and routing, measured

### D1 — fit the stage-3 rerank weights (~4 h)

*Do:* use `SEARCH_RERANK_WEIGHTS` (7 values, `FEATURE_ORDER`) and fit on a held-out half of the 600
set; evaluate on the other half. Never fit and report on the same half.
*Gate:* nDCG@10 improves over fusion on the held-out half; otherwise leave it off and say so.

### D2 — router that never loses recall (~5 h)

*Do:* extend `crates/engine/src/route.rs` with query features (digits, quotes, question words,
concept count, language); measure recall@20 against searching everything.
*Gate:* recall@20 not reduced, and lane calls per query reduced (measured p50/p95).

### D3 — tune `c` only with a real instrument (~4 h, blocked on E1)

*Do:* sweep `SEARCH_FUSION_C` (20/60/120) on the cross-disciplinary set at n≥50 plus the 600 set;
report both. A single-lane document cannot outvote a two-lane one at c=60 — that arithmetic is the
decision, not a bug.
*Gate:* a documented `c` with its measurement; otherwise leave 60.

---

## 5. Track E — instruments (without these, every lane decision is a guess)

### E1 — 50-case cross-disciplinary set (~10 h)

*Screen order, mandatory:* (1) concept weight of the target ≥0.8, (2) zero content-word overlap
between query and target title, (3) read the target text to confirm it really answers the query.
28 cases generated by title pattern all scored 0.0000 because their targets were barely represented
in the matrix — a screen that skips step 1 measures sparsity, not retrieval.
*Gate:* 50 accepted cases; baseline/lane table (BM25, dense, concept, features) with the n=50 caveat
removed; then and only then may a fusion weight change.

### E2 — run the body-span set and publish it (~2 h)

*Do:* `eval run --set benchmarks/queries.body.jsonl` for bm25/dense/concept/features; write the
table into EXECUTION-STATUS-v2.md with the caveat that labels are sentence→document, not hand-judged.

### E3 — a source-recall instrument (~4 h)

*Do:* 50 queries with a known correct site; measure whether the site appears in the top 10 sources.

---

## 6. Track F — serving, latency, operations

### F1 — engine path under budget (~8 h)

*Current:* `engine:true` p50 620 ms, p95 2.5 s at 1.56M docs; legacy 35/97 ms. Target <150 ms p95
for the engine path with all shipped weights.
*Do:* attribute the time per stage (`trace` already reports phases), then fix the top cost. Candidates
in measured order: federation `reload()` per lane call (keyed on a dirty flag, never a timer),
multi-clause query cost in the engine, nav rounds.
*Gate:* the attribution table plus the new p50/p95, before/after, same query set.

### F2 — `/read` on the hot path (~3 h)

*Do:* cache `doc_id → text_path` (or negative lookups) per root with a bounded LRU; keep the
read-only metadata connections open per root.
*Gate:* `/read` p95 when called once per result for 40 results, before/after.

### F3 — env-drift check at startup (~2 h)

*Do:* log the effective `FusionPolicy` at boot and warn when a `SEARCH_FUSION_*` value differs from
the documented default with no measurement note. Reconcile the drop-ins into one `fusion.conf`.
*Gate:* `systemctl show search-api -p Environment` and the boot line agree with
ARCHITECTURE-v2.md §8.2; a deliberate override is visible in one place.

### F4 — `agent0 matrix report` (~4 h)

*Do:* one verb over every root printing M1–M6: rows, distinct documents, density, coverage,
distinct `model_version`s, and stale-version rows (rows whose version is not the current model).
*Gate:* the numbers equal §6 of ARCHITECTURE-v2.md (doc_feature 22.2M, doc_concept 885K,
doc_concept_dim 1.89M) at the time of running; stale rows detected by injecting one.

### F5 — rebuild runbook drill (~3 h)

*Do:* rebuild one shard from a copy, then restore the old directory, and time both. Write the runbook
into OPERATIONS.md with the failure modes observed (lock held by crawler, stale manifest, missing
text).
*Gate:* a written, executed runbook with timings, not a plan.

---

## 7. Track G — scale, only when measured

### G1 — index-size projection at 10M documents (~3 h)

*Do:* extrapolate from the measured 1.10 KB/doc with a stated caveat (excerpt length distribution is
the lever), and check it against 10M × 1.10 KB ≈ 11 GB + vectors 14 GB. Publish the table.

### G2 — brute-force vector search limit (~4 h)

*Do:* measure dense-lane latency vs corpus size (19K → 1.39M today); find the document count where
brute force exceeds 50 ms and state whether ANN is required.

### G3 — portable pack integrity (~4 h)

*Do:* export/import with excerpt-only documents; verify ids, excerpts, entities and timestamps; make
the pack's format version explicit.

---

## 8. Suggested order and sizing

| Order | Task | Hours | Why now |
| --- | --- | --- | --- |
| 1 | A1b backfill headings/entities | 6 | the rebuild left 1.56M documents without them |
| 2 | A2 index audit | 3 | every later gate needs it |
| 3 | B1 concept lexicon | 8 | coverage is the matrix's weakest dimension |
| 4 | C1 encoder on titles | 5 | cheapest path to a non-zero feature weight |
| 5 | E2 body-span run | 2 | a free second instrument |
| 6 | E1 cross-disciplinary 50 | 10 | unlocks D3 and any weight change |
| 7 | F1 engine latency | 8 | the path users actually hit with `engine:true` |
| 8 | A3 excerpt selection | 4 | improves the only body text in the index |
| 9 | B4 link capture | 6 | unrecoverable if deferred past any blob decision |
| 10 | D1/D2 rerank, router | 9 | needs E1 to mean anything |
| 11 | B2/B3 graph, sources | 11 | source retrieval is a stated capability |
| 12 | F3/F4/F5 ops | 9 | keeps measurements honest |
| 13 | C2/C3/C4, B5, G* | 27 | after the instruments exist |

Total: ~105 hours. Tasks 1–2 are prerequisites for trusting any later number; 3–6 are the shortest
route to a semantic lane that can be defended.

---

## 9. Hand-off template

End every task with this, in the commit body or the report:

```text
task:            <id> <title>
changed:         <files, one line each, why>
gate:            <the measurement, with the command>
result:          <before → after, or "not measured: <reason>">
regressions:     <any test or metric that moved the wrong way>
follow-ups:      <what this exposed, as concrete tasks>
```