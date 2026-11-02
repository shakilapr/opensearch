# Ranking on concepts, not keywords — analysis and design

Question: what is the optimal way for Hexxica to rank without relying on keywords, using concepts and
their relatedness? This document is the analysis, the scoring model, what is implemented, and what
the measurements say.

Design context: `docs/ARCHITECTURE.md`. Measurements: `docs/STATUS.md`, `docs/BENCHMARKS.md`.

---

## 1. What "not keywords" can and cannot mean here

A ranking that ignores text entirely cannot resolve a query, because the query is text. The useful
distinction is **where lexical matching is allowed to act**:

| Stage | Lexical allowed? | Why |
| --- | --- | --- |
| Query → concept | **no** (it is the thing to fix) | matching query words to *concept names* is still keyword matching; it fails when the query uses words the lexicon does not list |
| Candidate generation | yes, as one proposer among several | recall must not depend on a single expert |
| **Ranking** | **no** | a document's position must be decided by which concepts it carries and how those relate, not by whether its words repeat the query's |

So the target is: **concepts decide the order; words may only propose candidates.** Today the concept
lane is a fusion lane at weight 0.0, so words decide everything. That is the gap.

## 2. What a concept-based ranker needs

Four pieces, in dependency order. The first two are missing or thin today; this is the analysis that
matters.

### 2.1 Concept identity for the query — currently lexical

`ConceptLane::concepts_for` matches query tokens against concept names and aliases with a crude
stemmer. It works for "romance" and "chemical" (→ chemistry) and fails for "pair bonding" unless an
alias exists, and for any phrasing that shares no word with the lexicon. Fifteen concepts make this
worse: most queries resolve to nothing, and the lane then contributes nothing.

**Optimal fix:** represent each concept by the centroid of its member documents' embeddings, and
resolve a query by cosine similarity against those centroids. That is still local, bounded CPU work
(one 384-d embedding + 300 dot products ≈ milliseconds) and it needs no alias list. Alias matching
stays as a high-confidence fast path; centroids are the general path.

### 2.2 Concept relatedness — now built from the corpus

`concept_relation` was empty. An external taxonomy (OpenAlex, SKOS) describes scholarly subfields; it
is the wrong instrument for a corpus of software docs, news, commerce and science, and it cannot say
that *in this corpus* `chemistry` and `romance` co-occur because papers about pair-bonding are tagged
both.

**Implemented:** `agent0 concepts relate` computes pointwise mutual information over the document ×
concept matrix:

```
PMI(a,b) = ln( (n_ab · N) / (n_a · n_b) )        strength = PMI / (1 + PMI)  ∈ (0,1)
```

Keeping only positive PMI (above chance), requiring `n_ab ≥ 3`, and retaining the top-k neighbours
per concept. The pass purges its own `cooccur` rows before writing, so a grown lexicon cannot leave
stale edges behind. Rows carry `model_version`, and hand-written relations (`related`, `mechanism_of`)
in the same table are untouched.

Why this is the right relatedness signal: it is *measured from how the corpus actually uses the
concepts*, it improves automatically as the lexicon grows, and it is symmetric — a query can enter
from either concept.

### 2.3 Association strength and dimension — already stored

`doc_concept.weight` (scalar) and `doc_concept_dim` (`direct`, `contextual`, `mechanism`) already
encode *how* a document relates to a concept. The ranker must keep the dimensions separate: a page
that explains a mechanism is a different answer from a page that merely mentions the topic, and the
query's shape decides which matters.

### 2.4 Specificity — IDF, which the lane did not have

A concept on 32% of the corpus (`c-ml` measured 6,167 of 19,255 documents early on) is weak evidence.
Multiplying each concept's contribution by `1 + ln((N+1)/(df+1))` (floored at 0.1) lets a rare concept
decide an order while a ubiquitous one only contributes. Downweight, never delete: deleting turns a
weak answer into no answer.

## 3. The scoring model

For a query `q`:

1. **Resolve** — `Q = {(c, w_c)}`:
   * `w_c = 1.0` for a concept named or aliased by the query;
   * `w_c = λ · strength(c, c')` for each neighbour `c'` of a resolved concept (λ = 0.4);
   * normalise so `max w_c = 1`; keep the top 12 concepts.
2. **Score a document** — sum over resolved and expanded concepts:

```
S(d|q) = Σ_c  w_c · idf(c) · [ w_direct·R1(d,c) + w_ctx·R2(d,c) + w_mech·R3(d,c) ] · kind(d,c)
```

   where `kind = 1.0` for a direct association and `0.5` for an inferred one, and `R*` are the
   dimensions (falling back to the scalar weight when a document has no dimensional rows).
3. **Rank** — order by `S(d|q)`, with the lexical lanes allowed only to propose candidates.

The dimension weights follow the query's shape (`DimensionWeights::mechanic_if_asked`): a how/why
query emphasises `mechanism`, a topic query `direct`. That is a rule, not a learned model, and it is
measured against the balanced default.

Concretely, "chemical reactions that affect romance" resolves to `c-chemistry` and `c-romance`; the
graph adds `c-neuroscience`, `c-hormones`, `c-psychology` at a discount; a paper on oxytocin and
pair-bonding scores on `c-hormones` + `c-psychology` even if its title never says "romance".

## 4. Implemented (2026-11-02)

| Piece | Where | State |
| --- | --- | --- |
| Co-occurrence graph builder | `agent0 concepts relate` (`crates/agent0/src/conceptbuild.rs`) | **done**, run on all five roots |
| Graph storage | `concept_relation`, `relation='cooccur'`, `strength`, `model_version` | **done**, migration-free |
| Graph read + expansion | `ConceptLane::resolve_weighted`, `with_expansion` | **done**, unit-tested |
| Specificity (IDF) | `ConceptLane::concept_weight` | **done** |
| Dimension choice by query shape | `DimensionWeights::mechanic_if_asked` | **done** |
| Concept-only ranking mode | `--lanes concept` with `SEARCH_FUSION_BM25=0 SEARCH_FUSION_CONCEPT=1` | **done** (env), measured below |
| Semantic query → concept (centroids) | not built — design in §2.1 | **next** |
| Lexicon breadth (15 concepts) | not done | **next** |

## 5. Measurements

*(filled from `ops/bench-2026-11-02/`; see BENCHMARKS.md for the commands and the caveats)*

Measured 2026-11-02, federated over five roots, crawlers active (so latencies are contended):

| Set | Ranking | nDCG@10 | recall@10 | MRR |
| --- | --- | --- | --- | --- |
| 600 corpus-derived | BM25 | 0.2112 | 0.4200 | 0.1476 |
| 600 corpus-derived | concept-only | _pending_ | _pending_ | _pending_ |
| cross-disciplinary (n=6) | BM25 | 0.1052 | 0.1667 | 0.0833 |
| cross-disciplinary (n=6) | **concept-only** | **0.2718** | **0.3333** | **0.2500** |

The result that matters: on queries whose words do not appear in the answer, concept-only ranking
scores **2.6× BM25** and returns twice the relevant documents. On the title-derived set it is expected
to lose — that set's labels are the document's own words — and the number is reported when it lands
rather than suppressed.

## 6. What would make this the primary ranker

1. **Resolve queries to concepts without their words** (centroids) — otherwise concept ranking
   inherits the lexicon's vocabulary.
2. **Grow the lexicon to ~300 concepts** with title-validated tags, so coverage rises from 32.5%.
3. **A query-family-split graded set** (1000 train / 200 val / 200 test), pooled from all lanes, so
   the dimension weights and λ can be fitted rather than set.
4. **Diversity in result selection**: one document per concept cluster near the top, so a single
   concept cannot fill the page.

Until (1) and (2) exist, concept-only ranking is expected to lose to BM25 on queries whose words are
in the index — and the measurement should show exactly that, not be tuned to hide it.