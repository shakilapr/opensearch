# Semantic index architecture

**Status:** the identity and semantic schema is landed and tested — migrations
`0005_semantic_index.sql` and `0006_identity_layer.sql`, 39 metadata tests green
(`cargo test -p search-metadata`). No model is trained yet and no retrieval lane consumes the
associations yet. This document is the specification those two pieces are written against, and it
records what already exists so nothing is built twice.

Scope: how Hexxica stores *meaning* separately from *pages*. The engine keeps a compact knowledge
layer — identifiers, concepts, weighted associations — and fetches the actual document only when a
query needs it. Nothing here stores article bodies.

---

## 0. The intended behaviour (the requirement, in one example)

> The search engine should incorporate a large semantic vector model that acts as an intelligent
> knowledge layer within the index. Instead of relying primarily on keywords, article titles, or
> exact text matches, the system should identify the different concepts and topics associated with
> each document and assign weighted relationships based on their relevance. A single article may be
> connected to multiple topics with different strengths, allowing it to be discovered through
> queries that use different terminology or approach the subject from another perspective.
>
> For example, a research paper discussing the effects of oxytocin and other chemicals on human
> attachment may have a title that does not mention love or romance. The semantic index should
> recognize its relevance to chemistry, neuroscience, hormones, emotional bonding, and romantic
> relationships, assigning appropriate weights to each connection. When someone searches for
> "chemical reactions that affect romance," the engine should identify the relationship between the
> query's concepts and those associated with the paper, even when the exact search terms are absent
> from its title.
>
> Each article should have a unique identifier linked to its semantic representations and weighted
> concept associations. Website identifiers, URLs, sitemaps, and other source information should be
> maintained in a separate metadata database. This allows the semantic index to remain compact,
> storing relationships and document identifiers rather than complete web pages. When a query
> matches relevant concepts, the engine retrieves the associated article identifiers, resolves
> their source URLs, and directs the requesting AI system to the relevant pages for further reading
> and analysis.
>
> The semantic model should continuously improve its understanding of relationships between topics
> as new documents are processed, updating the weighted associations without requiring the entire
> index to be rebuilt. The objective is to create a compact, interconnected knowledge structure that
> can discover relevant information across different disciplines and terminology while minimizing
> storage and unnecessary web crawling.

Everything below is the mechanism for that paragraph, mapped onto the code that exists.

---

## 1. Three components, and what each one is allowed to know

| Component | Knows | Does **not** know |
| --- | --- | --- |
| **Identity database** (`meta.sqlite`, SQLite) | what each id means, where the bytes are, whether they are still available | nothing about meaning |
| **Semantic knowledge index** (the tables in §3) | which articles, passages, concepts and sites relate, and how strongly | nothing about URLs, file paths, or HTML |
| **Models** (CPU-trained, versioned) | how to read text and produce weights | they never store documents; they only write associations |

The rule that keeps this rebuildable: **the semantic index points at stable ids, never at file
locations or vector positions.** Replace a model, rebuild the postings, re-embed everything — the
document identities do not move.

---

## 2. Identifiers

The five ids the project already uses are kept. Three are added, plus a website identity above the
domain. None is replaced or renamed.

| ID | Represents | Table | Status |
| --- | --- | --- | --- |
| `DomainID` | DNS hostname | `domain` | exists |
| `WebID` | website / logical collection, may span hosts | `website`, `website_host` | **new (0006)** |
| `SourceID` | the profile a crawl is scheduled by | `source_profile` | exists (topics empty — §5) |
| `SectionID` | a section inside a website (`/docs/v2/`) | `source_section` | **new (0006)** |
| `ResourceID` | fetchable URL | `resource` | exists, 1,090,184 rows |
| `WorkID` | the intellectual work, across copies | `work`, `work_location` | exists / **new (0006)** |
| `ContentID` | exact content, by hash | `content` | exists, 1,070,364 rows |
| `PassageID` | a passage inside one content | `passage` | **new (0005)** |
| `ConceptID` | a topic or concept | `concept` | **new (0005)** |
| `FeatureID` | a latent feature learned by a specific model version | — | **planned** (see `PLAN-v2.md` §3); a feature id is meaningless without its model version |
| `SitemapID` | a sitemap document | `sitemap`, `sitemap_entry` | exists / **new (0006)** |

### The separations the schema enforces

* **A website is not a hostname.** `website_host` is many-to-many: one site can serve
  `example.org` and `docs.example.org`; one hostname can host several unrelated sites (a shared
  platform, a university with independent departments). Collapsing them would make "the site about
  X" mean "whatever shares a DNS name".
* **A work is not a location.** `work_location(work_id, resource_id, relationship, preferred)`
  records a publisher copy, a preprint and a repository copy as *one* work. Without it the same
  paper is indexed three times and competes with itself.
* **`doc_concept` is keyed on `resource_id`** — present for every fetch — and *carries* `work_id`
  when known, so associations aggregate to the work without losing the location.
* **A stable id survives a title or URL change.** Nothing in the semantic layer parses a URL or a
  title to find a document; `resource.web_id` and `resource.section_id` are nullable, so the
  identity layer is populated incrementally and every existing row keeps working (test:
  `resources_work_without_an_identity_and_accept_one_later`).

---

## 3. What the database contains

### 3.1 Full table map, against the code

| Proposed table | In Hexxica | Note |
| --- | --- | --- |
| `website` | **new (0006)** | plus `website_host` for the many-to-many host mapping |
| `source_section` | **new (0006)** | `path_prefix` makes a section machine-usable |
| `sitemap` | exists (0003) | 274,996 rows across the fleet |
| `sitemap_entry` | **new (0006)** | advertised `lastmod` kept apart from fetched facts |
| `resource` | exists (0001) | + `web_id`, `section_id` columns (0006) |
| `work` | exists (0003) | 211 rows on the serving index |
| `work_location` | **new (0006)** | one row per copy, at most one `preferred` |
| `content_version` | `content` + `document_version` | the existing pair already separates exact bytes (hash) from revisions (`version_no`) |
| `passage` | **new (0005)** | offsets + heading path; **no text stored** |
| `web_link` | **new (0005)** | src resource, dst url/resource, rel, anchor; unrecoverable once raw HTML is deleted |
| `concept` | **new (0005)** | name, description, kind, status |
| `concept_alias` | **new (0005)** | per-language surface forms |
| `concept_relation` | **new (0005)** | typed edges |
| `document_concept` | `doc_concept` (0005) | keyed on `resource_id`, carries `work_id` |
| `passage_concept` | **new (0005)** | passage-level evidence |
| `source_concept` | **new (0006)** | aggregated, normalised by `doc_count` |
| `model_registry` | **new (0005)** | `(model_id, version)` → kind, status, params |

Operational state (fetching, robots policy, freshness, crawl leases, index publication) already
exists in the same database and is extended rather than recreated.

### 3.2 The four design rules encoded in the schema

1. **Two directions.** `doc_concept` supports document→concepts directly and concept→documents
   through `idx_doc_concept_concept (concept_id, weight DESC)`. The second direction is the search
   operation; it must never be a full-table scan. For millions of rows the postings are materialised
   as sorted lists and rebuilt from SQLite, which stays the source of truth.
2. **Direct and inferred are separate rows, never one blended number.**
   `PRIMARY KEY (resource_id, concept_id, relation_kind)` lets a direct claim (0.94, evidence
   passage P18) and an inferred claim (0.65, via pair-bonding, model m2) coexist; readers prefer
   `direct`. This is what stops a model reinforcing its own earlier inference.
3. **Relation types are explicit** — `equivalent`, `broader`, `narrower`, `related`, or a typed
   scientific relation (`mechanism_of`, `causes`, `located_in`) — because a synonym, a hierarchy and
   a mechanism are different claims with different trust.
4. **Every weight carries provenance**: `model_version`, `evidence`, `passage_id`,
   `via_concept_id`. Weights are `CHECK (0 ≤ w ≤ 1)`; a self-relation is rejected; an alias belongs
   to exactly one concept.

### 3.3 Ambiguity is modelled, not averaged

"Love" as attachment and "love" in a song title are two concepts, not one concept with two meanings.
The schema supports this through `concept_alias` uniqueness per concept plus a `related` relation
between the two — never by letting one alias point at two concepts, which would make every query
about either return both.

### 3.4 Sitemap data is a hint, not a fact

`sitemap_entry.advertised_last_mod` is what the sitemap claimed; `resource.modified_at` is what the
fetch observed. They are different columns on purpose (test:
`sitemap_entries_keep_advertised_hints_apart_from_fetched_facts`), because a sitemap promises that a
URL was listed, not that the page exists or contains anything.

---

## 4. How a query is answered

Worked through the oxytocin example, with the ids as the schema stores them.

```
offline (per document, as it is ingested)
  text ──▶ Model B (multi-label concept classifier)
        ──▶ doc_concept: (A501, C12 chemistry,  0.88, direct, passage P4)
                         (A501, C93 hormones,   0.96, direct, passage P18)
                         (A501, C207 love,      0.64, direct, passage P21)
        ──▶ vectors: one per document/passage (BGE or a trained 128/256-d model)
        ──▶ web_link: outlinks + anchor text
        ──▶ source_concept: aggregated to the website, normalised by doc_count

query "chemical reactions that affect romance"
  1. parse + embed the query                          (no corpus scan)
  2. concept resolution: C12 chemistry, C41 chemical reaction, C207 love
  3. concept → documents via postings:
        C207 ⇒ A501, A609, A820…        (weights, direct preferred)
        C41  ⇒ A501, A118…
  4. coverage scoring: a document that satisfies BOTH aspects outranks one that satisfies one
        S(d,q) = α·dense(d,q) + β·concept_coverage(d,q) + γ·lexical(d,q)
  5. evidence: for the top-k, fetch the page (or read the stored excerpt) and verify the passage
  6. return ids + resolved URLs; the requesting AI reads the source
```

`concept_coverage` rewards covering several query aspects, **not** accumulating many weak tags:

| Document | Chemistry | Attachment | Expected |
| --- | --- | --- | --- |
| Oxytocin and pair bonding | high | high | strong candidate |
| Laboratory oxidation reactions | high | none | lower |
| Relationship counselling | none | high | lower |
| Hormonal influences on social behaviour | high | medium | relevant |

The semantic layer proposes; the passage check disposes. A candidate the evidence pass cannot
support is demoted, not returned.

---

## 5. What already exists in the engine (so this is not rebuilt)

| Piece | State | Note |
| --- | --- | --- |
| BM25 lexical lane | live | 19,255 documents served |
| Dense lane (BGE-small, 384-d) | live but **weight 0.0** | `FusionPolicy::default()` gives BM25 1.0, source-first 0.8, dense 0.0 — the semantic judgement is computed and then discarded |
| Vector sidecar | live | `vectors.bin`: 384-d, 19,255 entries, byte-stable |
| Entity graph | live on the serving index only | 14,659 entities, 130,359 co-occurrence edges; empty on all four fleet shards |
| Sitemaps / feeds | 274,996 / 18,061 rows | the discovery half of the source layer |
| Frontier | 9,050,755 URLs over 338,609 domains | discovered, mostly not yet fetched |
| Source profiles | 190,851 rows, **0 with topics** | the topical column is empty; the source layer is still to be built |
| ANN (`search-ann`) | written, unwired | brute force is fine at 19K, not at 1M |
| Models | none trained | `search-distill::LinearPolicy` is a fixture proxy |

Consequence for sequencing: **the first semantic win is a one-line fusion weight**, and the second
is populating `concept`/`doc_concept` — not a new embedding model.

---

## 6. Models: what to train on CPU, and what it costs

Measured on this host (16 vCPU, 62 GB RAM, no GPU):

* embedding throughput with BGE-small at 2,000 chars/doc: **8 docs/s** → 1.09M documents ≈ **38 h**
  (17 h for the 501K published fleet, 40 min for the serving index);
* a 1024-d model is ~3× the compute and 2.7× the bytes per vector (4.5 GB vs 1.6 GB for 1.09M);
* BGE truncates at 2,000 characters, so a long article is embedded from its opening — the exact
  failure the oxytocin example describes, where the topic appears mid-article.

| Model | Job | CPU feasibility | Note |
| --- | --- | --- | --- |
| **B — multi-label concept classifier** (TF-IDF / `HashingVectorizer` + one-vs-rest linear, or fastText) | document → several `ConceptID`s with weights | high | the core model; incremental — new labels retrain the classifier, not the representation |
| **A — compact representation** (fastText or LSA, 128–256-d) | cheap article and query vectors | high | benchmark against BGE-small before replacing it |
| **C — concept-relation learner** | typed edges between concepts | medium | train on curated taxonomy edges and validated text relations — **never** on mere co-occurrence |
| **Topic hubs** (deterministic k-means over existing embeddings) | 10–20k hubs; each document keeps its top ~6 with weights | high | no training; ~72 B/doc ≈ 78 MB for the fleet |

**Do not** train a transformer from scratch on this hardware: quality per CPU-hour is far worse than
a distilled centroid/classifier pair. Keep BGE-small as the benchmark and as an optional retrieval
signal; a custom model replaces it only when an ablation shows better quality or a real efficiency
win.

### 6.1 Where the labels come from

In descending order of reliability, and never promoted automatically:

1. DOI / OpenAlex subject metadata for scholarly works. OpenAlex's 4 domains → 26 fields → 252
   subfields → 4,516 topics is a ready multi-label hierarchy for the research slice; import the
   *hierarchy*, not the database.
2. Site sections and documentation hierarchies — weak but cheap topic labels, and `source_section`
   now exists to hold them.
3. Unambiguous terminology and title patterns.
4. Human review of uncertain, conflicting and cross-domain examples.

Staged loop: structured labels → weak labels → train → predict → review the uncertain → add verified
examples → retrain → compare on a held-out set. Predicted labels are never treated as ground truth.

Initial experimental envelope: 50K–100K training articles, 1K–5K concepts, 128–256 dimensions,
5–15 associations per article. These are starting bounds, not benchmarked optima.

---

## 7. Storage budget for the semantic layer

| Item | Size (1.09M documents) |
| --- | --- |
| `doc_concept`: 10 associations/doc, compact | ~160 MB raw, plus index overhead |
| concept vocabulary + aliases + relations | tens of MB |
| `passage` rows (offsets only, no text) | ~150–300 MB |
| `web_link` (~136 M edges projected from 125 anchors/page) | ~5–10 GB |
| 256-d int8 vectors, one per document | ~256 MB |
| topic-hub weights (top 6/doc) | ~78 MB |

**Well under 12 GB**, against **297 GB** of raw HTML and extracted text that the index-only model
deletes (`index-upgrade-plan.md`). The semantic layer is what makes deleting the articles
acceptable: it is the part that still knows what the deleted pages were about.

---

## 8. Reliability requirements

Explicit acceptance criteria, beyond ids and weights:

- **Concept identity management** — near-duplicates (`romantic bonding` vs `pair bonding`) are merged
  deliberately and the merge is recorded, not left as two unrelated concepts.
- **Model versioning** — every inferred association names its producer; rollback is a filter, not a
  re-run.
- **Evidence tracking** — an association can name the passage that justifies it.
- **Source aggregation** — a site's profile is normalised by `doc_count`, so 200,000 pages do not
  outrank a precise 20-page site by volume alone.
- **Calibration** — model outputs are scores, not probabilities; comparisons stay inside one model
  version until calibration is measured.
- **Incremental indexing** — only changed documents, or documents affected by a model upgrade, are
  reprocessed.
- **Concept discovery** — emerging topics can be added, with duplicate suppression so the vocabulary
  does not grow one concept per document.
- **Content availability** — `resource` records whether text is cached or must be re-fetched; the
  index-only model makes this a first-class state, not an error.
- **Training-data provenance** — sources, licences and permitted uses are recorded per training set.

---

## 9. Implementation order

The consolidated, gated sequence is in **`docs/PLAN-v2.md`**; this section is the semantic-layer
slice of it. Two things precede all of it: the answer path must stop discarding its output, and the
corpus must be reachable (federation), because a semantic layer over 3.8% of the corpus measures
nothing.

1. **Identity and semantic schema** — done: migrations 0005 and 0006, 39 tests.
2. **Concept dictionary** — a small verified vocabulary with aliases and typed relations (hundreds of
   concepts, not a taxonomy dump).
3. **CPU baseline classifier** — multi-label, sparse features, deterministic seed, versioned in
   `model_registry`.
4. **Bidirectional postings** — materialise concept→documents from `doc_concept`; SQLite stays the
   source of truth.
5. **Integrate with Tantivy** — retrieve from lexical *and* concept indexes; add the vector lane as a
   separately benchmarked path; **give the dense lane a non-zero fusion weight**.
6. **Source concept profiles** — aggregate `doc_concept` → `source_concept`, normalised.
7. **Relationship learner** — typed edges, with bounded inference propagation.
8. **Scale to the corpus** — batched, checkpointed, model-versioned; process the 589,572 documents
   whose text is already on disk before crawling anything new.

Every step is measured against the 600 graded queries in `benchmarks/queries.corpus.jsonl`; a step
that cannot show a gain is not kept.

---

## 10. What "done" means

1. A query whose words appear in no title returns the right article — the oxytocin case as a
   regression test against a real crawled page, not a fixture.
2. `concept → documents` answers in bounded time without scanning `doc_concept`.
3. Direct and inferred weights are separately visible in the API response, with the model version.
4. Re-running the classifier over a held-out set reproduces its weights (deterministic seeds,
   recorded versions).
5. An ablation shows the semantic lanes beating BM25-only on the graded query set — measured, not
   asserted.
