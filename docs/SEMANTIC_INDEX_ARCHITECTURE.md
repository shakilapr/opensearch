# Semantic index architecture

**Status:** schema landed (migration `0005_semantic_index.sql`); the models and the retrieval lanes
that consume it are not built yet. This document is the specification those lanes are written
against, and it records what already exists so nothing is rebuilt twice.

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

The five ids the project already uses are kept. Three are added. None of them is replaced.

| ID | Represents | Table today | Status |
| --- | --- | --- | --- |
| `DomainID` | internet hostname | `domain` | exists |
| `SourceID` | logical collection / site profile | `source_profile` | exists (topics empty — see §6) |
| `ResourceID` | fetchable URL | `resource` | exists, 1,090,184 rows |
| `WorkID` | the intellectual work, across copies | `work` | exists, 211 rows on the serving index |
| `ContentID` | exact content, by hash | `content` | exists, 1,070,364 rows |
| `PassageID` | a section/passage inside one content | `passage` | **new (0005)** |
| `ConceptID` | a topic or concept | `concept` | **new (0005)** |
| `SitemapID` | a sitemap document | `sitemap` | exists, 274,996 rows |

Two identity rules carried over from the design discussion:

* **A work and a URL are different identities.** One paper can live at a publisher, a preprint
  server and a repository; one URL can serve different revisions over time. `doc_concept` is keyed
  on `resource_id` (present for every fetch) and *carries* `work_id` when known, so associations can
  be aggregated to the work without losing the location.
* **A stable id survives a title or URL change.** Nothing in the semantic layer parses a URL or a
  title to find a document.

---

## 3. What the semantic index stores

Landed in `crates/metadata/migrations/0005_semantic_index.sql`:

| Table | Holds | Why it exists |
| --- | --- | --- |
| `concept` | id, canonical name, description, kind, status | the vocabulary documents are weighed against |
| `concept_alias` | id ↦ surface forms (per language) | query and text resolution |
| `concept_relation` | typed edges: `equivalent` / `broader` / `narrower` / `related` / typed | recall across disciplines |
| `doc_concept` | `(resource, concept, relation_kind)` ↦ weight, evidence, passage, via-concept, model version | **the weighted associations — the thing queries hit** |
| `passage` | id, content, ordinal, offsets, heading path | where inside a document a claim was observed |
| `passage_concept` | `(passage, concept)` ↦ weight, model version | passage-level evidence for the answer path |
| `web_link` | src resource, dst url/resource, rel, anchor, internal | the navigation graph (derived from raw HTML, unrecoverable later) |
| `model_registry` | `(model_id, version)` ↦ kind, status, params | every inference names its producer |

### The four design rules encoded in the schema

1. **Two directions.** `doc_concept` supports document→concepts directly, and concept→documents
   through `idx_doc_concept_concept (concept_id, weight DESC)`. The second direction is the search
   operation: it must never be a full-table scan.
2. **Direct and inferred are separate rows, never one blended number.**
   `PRIMARY KEY (resource_id, concept_id, relation_kind)` allows a direct claim (0.94, evidence
   passage P18) and an inferred claim (0.65, via pair-bonding, model m2) to coexist; readers prefer
   `direct`. This is what stops a model reinforcing its own earlier inference.
3. **Relation types are explicit**, not one generic "related" soup: a synonym, a hierarchy, and a
   scientific mechanism are different claims with different trust.
4. **Weights carry provenance**: `model_version`, `evidence`, `passage_id`, `via_concept_id`.

`CHECK (weight BETWEEN 0 AND 1)`, the self-relation ban, and the alias uniqueness constraint are
enforced by the database, not by the caller (tests: `crates/metadata/tests/semantic_tests.rs`).

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

Scoring detail that matters: **`concept_coverage` rewards covering several query aspects, not
accumulating many weak tags.** A document with chemistry 0.88 and love 0.64 beats a document with
chemistry 0.95 and nothing else.

| Document | Chemistry | Attachment | Expected |
| --- | --- | --- | --- |
| Oxytocin and pair bonding | high | high | strong candidate |
| Laboratory oxidation reactions | high | none | lower |
| Relationship counselling | none | high | lower |
| Hormonal influences on social behaviour | high | medium | relevant |

The final ordering still has to survive an evidence check — the semantic layer proposes, the
passage check disposes.

---

## 5. What already exists in the engine (so this is not rebuilt)

| Piece | State | Note |
| --- | --- | --- |
| BM25 lexical lane | live | 19,255 documents served |
| Dense lane (BGE-small, 384-d) | live but **weight 0.0** | `FusionPolicy::default()` gives BM25 1.0, source-first 0.8, dense 0.0 — the semantic judgement is computed and then discarded |
| Vector sidecar | live | `vectors.bin`: 384-d, 19,255 entries, byte-stable format |
| Entity graph | live on the serving index only | 14,659 entities, 130,359 co-occurrence edges; empty on all four fleet shards |
| Sitemaps / feeds | 274,996 / 18,061 rows | the discovery half of the source layer |
| Frontier | 9,050,755 URLs over 338,609 domains | discovered, mostly not yet fetched |
| Source profiles | 190,851 rows, **0 with topics** | the topical column is empty; the source layer is still to be built |
| ANN (`search-ann`) | written, unwired | brute force is fine at 19K, not at 1M |
| Models | none trained | `search-distill::LinearPolicy` is a fixture proxy |

Consequence for sequencing: **the first semantic win is a one-line fusion weight**, and the second
is populating `concept`/`doc_concept`, not a new embedding model.

---

## 6. Models, and what to train on CPU

Measured on this host (16 vCPU, 62 GB RAM, no GPU):

* embedding throughput with BGE-small at 2,000 chars/doc: **8 docs/s** → 1.09M documents ≈ **38 h**;
* a 1024-d model is ~3× the compute and 2.7× the bytes per vector (4.5 GB vs 1.6 GB for 1.09M);
* BGE truncates at 2,000 characters, so a long article is embedded from its opening — the exact
  failure mode the oxytocin example describes (the topic appears mid-article).

Recommended model set, in order of value per CPU-hour:

| Model | Job | Feasibility | Note |
| --- | --- | --- | --- |
| **B — multi-label concept classifier** (TF-IDF/HashingVectorizer + one-vs-rest linear, or fastText) | document → several `ConceptID`s with weights | high | the core model; incremental, retrainable on new documents only |
| **A — compact semantic representation** (fastText / LSA, 128–256-d) | cheap article and query vectors | high | benchmark against BGE-small before replacing anything |
| **C — concept-relation learner** | typed edges between concepts | medium | train on curated taxonomy edges + validated text relations, never on mere co-occurrence |
| **Topic hubs** (deterministic k-means over existing embeddings) | 10–20k hubs; each document stores its top ~6 hubs with weights | high | no training at all; 72 B/doc ≈ 78 MB for the fleet |

Labels come from, in this order: DOI/OpenAlex subject metadata, site sections and documentation
hierarchies, unambiguous terminology, then human review of uncertain and cross-domain cases.
Predicted labels are never promoted to ground truth automatically.

**Do not** train a transformer from scratch on this hardware; the quality per CPU-hour is far worse
than a distilled centroid/classifier pair.

---

## 7. Storage budget for the semantic layer

| Item | Size (1.09M documents) |
| --- | --- |
| `doc_concept`: 10 associations/doc, compact | ~160 MB raw, plus index overhead |
| concept vocabulary + aliases + relations | tens of MB |
| passage rows (text offsets only, no text) | ~150–300 MB |
| web_link (136 M edges projected from 125 anchors/page) | ~5–10 GB |
| 256-d int8 vectors, one per document | ~256 MB |
| topic-hub weights (top 6 hubs/doc) | ~78 MB |

Total: **well under 12 GB**, against **297 GB** of raw HTML and extracted text that the index-only
model deletes (§ `index-upgrade-plan.md`). The semantic layer is what makes deleting the articles
acceptable: it is the part that still knows what the deleted pages were about.

---

## 8. What must be true before this is called done

1. A query whose words appear in no title returns the right article — the oxytocin case, as a
   regression test with a real page, not a fixture.
2. `concept → documents` answers in a bounded time without scanning `doc_concept`.
3. Direct and inferred weights are separately visible in the API response, with the model version.
4. Re-running the classifier over a held-out set reproduces its weights (deterministic seeds,
   recorded versions).
5. An ablation shows semantic lanes beating BM25-only on the 600 graded queries in
   `benchmarks/queries.corpus.jsonl` — measured, not asserted.
