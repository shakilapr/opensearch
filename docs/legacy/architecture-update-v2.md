> **Status: rationale, not the normative spec.** `docs/ARCHITECTURE-v2.md` (rev 2.1) is the
> normative document: it states the implemented data model, the matrices with measured row counts,
> and what is deliberately absent. This file is kept because the reasoning for each component lives
> here; where the two differ, ARCHITECTURE-v2.md wins.

# Hexxica — Neural Associative Search Architecture

**Revision:** 2.0 — LLM-Inspired Semantic Memory  
**Architecture:** Sparse–Dense Hybrid Retrieval with Learned Routing  
**Primary language:** Rust  
**Initial hardware:** CPU-only server, 16 vCPU and approximately 64 GB RAM  
**Long-term design scale:** Billions of article identifiers and trillions of potential semantic relationships

---

## 1. Architectural Objective

Hexxica is an AI-native web discovery and retrieval engine designed to identify relevant information across the internet without permanently storing the complete contents of every discovered webpage.

The system should maintain a compact, continuously evolving representation of websites, articles, concepts, and their semantic relationships.

Rather than depending primarily on exact keyword matching, Hexxica should learn how information relates to different subjects, including indirect, interdisciplinary, and context-dependent relationships.

Its semantic intelligence should be generated largely during indexing, allowing search queries to execute using minimal computation.

The engine should combine the associative capabilities of neural representations with the retrieval efficiency of compressed inverted indexes.

The principal architectural objective is:

**Develop a neural associative memory capable of identifying relevant articles through learned semantic relationships, retrieving their identifiers in milliseconds, and resolving those identifiers to original web sources.**

The engine must remain independent of mandatory LLM inference during search.

## 2. Core Architectural Principle

Advanced language models do not store every learned relationship as an explicit database entry. Their representations encode many overlapping features within learned multidimensional spaces.

Hexxica should adopt a related principle.

An article should not be assigned one category or represented by one embedding alone. Instead, its information should be represented through multiple learned semantic features, each with an associated activation strength.

These features form a sparse, weighted associative network.

A search query activates relevant features, which retrieve associated article identifiers.

The engine should preserve both explicit and implicit semantic knowledge:

**Explicit knowledge** consists of identifiable concepts, validated relationships, article metadata, and source information.

**Latent knowledge** consists of learned neural features that capture similarities and associations not necessarily represented by a human-defined taxonomy.

**Inferred knowledge** consists of relationships derived from existing associations when a query requires them.

Only selected explicit and latent associations should be stored. The system must not attempt to materialize every possible relationship between all articles.

---

## 3. High-Level Architecture

```text
                  OFFLINE INDEXING

           Articles / Documents / Pages
                       |
                       v
             Content Normalization
                       |
                       v
          Semantic Representation Model
                       |
            +----------+----------+
            |                     |
            v                     v
      Dense Features       Explicit Concepts
            |                     |
            v                     v
      Sparse Feature       Typed Relationships
         Encoder                  |
            |                     |
            +----------+----------+
                       |
                       v
           Neural Associative Index
                       |
          +------------+------------+
          |            |            |
          v            v            v
      Feature       Concept       Vector
      Postings      Graph         Sidecar
          |            |            |
          +------------+------------+
                       |
                       v
                Article IDs
                       |
                       v
            Identity / URL Database


                   ONLINE SEARCH

                 User Query
                       |
                       v
           Lightweight Query Encoder
                       |
                       v
             Sparse Expert Router
                       |
             +---------+---------+
             |         |         |
             v         v         v
          Semantic   Lexical   Dense ANN
          Postings    Index     Fallback
             |         |         |
             +---------+---------+
                       |
                       v
              Candidate Selection
                       |
                       v
           Query-Aware Neural Ranking
                       |
                       v
               Top Article IDs
                       |
                       v
               URL Resolution
                       |
                       v
              AI Agent / MCP / API
```

The expensive neural processing occurs primarily during offline indexing.

The online system performs lightweight query encoding, selective index traversal, and bounded relevance ranking.

---

## 4. Identity and Metadata Architecture

Each website, article, content version, and concept must have a stable identity independent of its learned semantic representations.

The metadata layer should retain the existing Hexxica identity model wherever possible.

| Identifier | Purpose |
|---|---|
| `DomainID` | Identifies a hostname |
| `SourceID` / `WebID` | Identifies a website or logical information collection |
| `ResourceID` | Identifies a canonical fetchable URL |
| `WorkID` / `ArticleID` | Identifies a logical article, research paper, or document |
| `ContentID` | Identifies a particular content representation or version |
| `PassageID` | Identifies a passage within a versioned document |
| `ConceptID` | Identifies a stable semantic concept |
| `FeatureID` | Identifies a feature learned by a specific neural model |
| `SitemapID` | Identifies a sitemap resource |

A `FeatureID` must be associated with a specific model version because learned features can change during retraining.

A `ConceptID`, by contrast, should remain stable when possible.

The system must not equate neural feature identities with human-defined concept identities.

### Identity database responsibilities

The identity database should contain canonical URLs, source membership, sitemap references, redirects, article titles, known authors, publication dates, retrieval timestamps, version relationships, and content-availability information.

It should also maintain mappings between logical articles and their different source locations.

The neural index should primarily reference compact numerical IDs.

It must not repeatedly store URL strings, complete article text, or detailed website metadata inside semantic posting lists.

For initial deployment, the existing SQLite metadata layer can remain the authoritative store. At much larger scale, identities can be partitioned into local key-value stores with globally stable IDs.

---

## 5. Neural Semantic Representation

### 5.1 Dense document representation

Each indexed article should first receive a semantic representation from an embedding model.

This representation captures the overall meaning of the document and provides a continuous representation from which additional features can be learned.

The initial implementation should retain the existing BGE embedding model as a baseline.

Larger embedding models, such as BGE-M3 or Qwen3-Embedding, may be evaluated as offline teachers.

The production architecture must not require these larger models on the interactive query path.

### 5.2 Learned sparse semantic features

The central neural memory should be produced using a sparse feature encoder.

A promising architecture is a Top-K sparse autoencoder trained on existing document embeddings, augmented with retrieval-oriented training.

Given a document embedding \(e_d\), the feature representation may be:

\[
z_d=\operatorname{TopK}(\operatorname{ReLU}(We_d+b))
\]

where:

- \(e_d\) is the dense document representation.
- \(W\) and \(b\) are learned parameters.
- \(z_d\) contains the activated semantic features.
- Top-K retains only a small number of activations.

A proposed initial configuration is 4,096–8,192 available features, with approximately 16–32 active features per document.

These are experimental starting values.

The model should be evaluated for retrieval accuracy, not only its ability to reconstruct the original embedding.

### 5.3 Multiple semantic perspectives

A document may discuss multiple subjects from different perspectives.

Hexxica should preserve this information through article-level representations, selected passage representations, and explicitly associated concepts.

It may also learn several low-dimensional aspect projections from a shared document representation.

This borrows the general idea of sharing a compressed representation across multiple learned perspectives, without copying transformer key–value caching mechanisms directly.

For example, an article discussing the biochemical mechanisms of romantic attachment might activate features associated with:

- Neurochemistry.
- Endocrinology.
- Human relationships.
- Pair bonding.
- Behavioral psychology.
- Experimental biology.

Different queries should activate and combine these associations differently.

---

## 6. Multidimensional Article–Concept Relationships

The semantic index should support multiple weighted relationships between the same article and concept.

For a given article \(d\) and concept \(c\), define an association vector:

\[
R(d,c)=[r_1,r_2,\ldots,r_m]
\]

These dimensions can describe different aspects of the association.

For example:

| Dimension | Meaning |
|---|---|
| \(r_1\) | Direct topical relevance |
| \(r_2\) | Contextual or indirect relevance |
| \(r_3\) | Relevance to mechanisms or processes |
| \(r_4\) | Relevance to a particular interpretation or perspective |

Evidence quality, factual support, and model confidence should be recorded separately when they cannot be represented by reliably trained and calibrated dimensions.

The relationship dimensions should be learned or explicitly validated. They must not merely be assumed to exist because several arbitrary output heads were created.

### Example

A research paper about oxytocin and pair bonding could have a strong direct association with neurochemistry and a weaker contextual association with romantic attachment.

When a user searches for chemical processes involved in romance, the query model should emphasize the relevant biochemical and relationship-oriented dimensions.

When another user searches for hormonal regulation, the same article could be ranked differently.

The document does not need to be reindexed for every query interpretation.

### Sparse storage

These relationships should be represented as sparse postings rather than a dense article-by-concept matrix.

A posting contains:

```text
FeatureID or ConceptID
    -> ArticleID
    -> Quantized weights
    -> Flags
    -> Evidence reference (when available)
    -> Model version / generation
```

Only meaningful associations should be materialized.

The forward representation, ArticleID to active features, should also be retained to support efficient updates and reranking.

---

## 7. Sparse Mixture-of-Experts Retrieval

Cutting-edge Mixture-of-Experts language models activate only a fraction of their available computation for each input.

Hexxica should adopt the same efficiency principle for retrieval.

Instead of searching every index partition equally, a lightweight learned router should identify relevant search strategies and semantic regions.

These retrieval experts may include:

- Sparse neural feature retrieval.
- Explicit concept retrieval.
- Lexical search.
- Source-specific search.
- Dense approximate-nearest-neighbor retrieval.
- Specialized scientific or technical indexes.

The router does not initially need to be a collection of large neural networks. Each expert can be an existing retrieval strategy or index partition.

A small routing model should estimate which experts are useful for a particular query.

For example, a question about chemistry and romantic relationships could prioritize semantic features connected to biology, neurochemistry, and interpersonal relationships.

The architecture should preserve a shared global retrieval path to prevent router errors from excluding relevant documents.

Routing quality, load balancing, and fallback coverage must be measured.

---

## 8. Sparse Attention-Inspired Candidate Selection

DeepSeek-V3.2 uses a lightweight learned indexer to select important tokens before applying more expensive attention.

Hexxica should adopt the corresponding retrieval principle:

**Use inexpensive scoring to select likely relevant article IDs before applying more sophisticated neural ranking.**

The search engine should not compute neural attention over the entire corpus.

Instead, it should:

1. Encode the query into sparse semantic features.
2. Identify relevant posting lists.
3. Retrieve candidates using compressed indexes.
4. Prune unpromising candidate blocks.
5. Select the strongest candidates.
6. Apply expensive relevance calculations only to that shortlist.

Block-Max WAND and SEISMIC-style retrieval are promising candidates for the sparse selection stage.

This makes the cost of search depend primarily on selected index blocks and candidate counts rather than total corpus size.

---

## 9. Compressed Semantic Inverted Index

The primary search structure should be a compressed weighted inverted index.

Each semantic feature maintains a list of associated article identifiers and their quantized weights.

The index should support:

- Sorted article identifiers.
- Delta encoding.
- SIMD-friendly compression.
- Quantized feature weights.
- Block-level score summaries.
- Skip structures.
- Fast top-k retrieval.
- Incremental immutable segments.

The initial implementation can use 8-bit association weights, provided quantization experiments demonstrate acceptable ranking accuracy.

More aggressive 4-bit quantization should be evaluated later.

The retrieval engine should avoid decompressing complete posting lists when only a small number of candidates can enter the top results.

A small mutable overlay can accept new associations while immutable segments support efficient search.

Published generations must keep document identifiers, posting lists, and model versions consistent.

---

## 10. Matryoshka and Compressed Dense Representations

Dense embeddings should provide an additional retrieval path rather than becoming the only search mechanism.

Matryoshka-style representations are particularly relevant because they allow retrieval using shorter prefixes of an embedding when supported by the trained model.

The system should investigate coarse-to-fine dense retrieval.

An illustrative progression is:

- Initial semantic comparison using 64–128 dimensions.
- More detailed comparison using 256 dimensions.
- Full representation for selected candidates.

These operations must use models specifically trained to preserve useful information at the selected dimensions.

Simply truncating an arbitrary embedding is not sufficient.

At larger scale, compressed vectors may be indexed using IVF-PQ, DiskANN, or another benchmarked approximate-nearest-neighbor implementation.

Dense search should recover documents that the sparse feature vocabulary fails to retrieve.

It should not require scanning every stored vector during ordinary search.

---

## 11. Attention-Inspired Multistage Ranking

Advanced neural models combine information through multiple learned representations.

Hexxica should adapt this principle using query-dependent ranking rather than corpus-wide transformer attention.

### First stage: Fast candidate retrieval

Retrieve candidates through sparse features, lexical matching, explicit concepts, and optional ANN.

### Second stage: Lightweight neural ranking

Evaluate a bounded candidate set using:

- Sparse semantic similarity.
- Dense similarity where available.
- Multidimensional article–concept associations.
- Coverage of separate query concepts.
- Source and document quality signals.
- Freshness where relevant.
- Direct versus inferred relationship strength.

A small CPU-trained network or gradient-boosted ranker can combine these features.

### Third stage: Optional fine-grained reranking

For complex searches, use a late-interaction model or a more expensive semantic scorer on only a small number of shortlisted documents.

This stage should be optional under the latency budget.

A conceptual final score is:

\[
S(d,q)=
\alpha(q)S_{\text{sparse}}
+\beta(q)S_{\text{dense}}
+\gamma(q)S_{\text{concept}}
+\delta(q)S_{\text{lexical}}
+\eta(q)S_{\text{coverage}}
\]

The query-dependent coefficients and component scores must be trained or calibrated before direct combination.

The engine should initially use rank fusion as a robust baseline, then compare it with learned ranking.

---

## 12. External Associative Memory

Hexxica should borrow the external-memory principle used in retrieval-augmented neural architectures.

The trained models provide the ability to locate related information.

The external index stores the addresses of that information.

The memory relationship is:

```text
Semantic Feature
       |
       v
Weighted ArticleIDs
       |
       v
Article Identity
       |
       v
Source and URL Records
       |
       v
Original Website
```

This allows the index to grow without continually retraining the model to memorize new documents.

A newly discovered article can be encoded, associated with features, and inserted into the index while the query model remains unchanged.

Periodic retraining should improve retrieval quality, but must not be required for ordinary document ingestion.

The separate identity database resolves semantic search results into accessible sources.

---

## 13. Concept Relationships and Graph Memory

In addition to latent neural features, Hexxica should maintain a compact graph of stable concepts.

Relationships should be typed rather than represented as generic similarity alone.

Examples include:

- Equivalent concept.
- Broader concept.
- Narrower concept.
- Related subject.
- Biological or physical mechanism.
- Scientific association.
- Citation relationship.
- Source or document hierarchy.

Graph traversal should be bounded.

Frequently useful expansions can be precomputed and cached.

The engine should avoid unlimited propagation of semantic weights, which could gradually make unrelated articles appear relevant.

Relationships inferred through the graph must remain distinguishable from associations directly supported by document content.

The graph should improve candidate discovery but should not replace passage-level evidence.

---

## 14. Custom CPU-Trainable Models

Hexxica should train small specialized models instead of attempting to train a large language model from scratch.

### Model A — Offline semantic teacher

Use an existing pretrained embedding model to produce high-quality document representations.

Retain BGE-small as the baseline and evaluate more capable teachers on representative data.

A larger teacher may be computationally expensive but is used only during batch indexing or training.

### Model B — Sparse neural feature encoder

Train a Top-K sparse feature model using the teacher's embeddings.

Its output becomes the learned semantic memory.

Training should combine sparse representation objectives with retrieval-oriented objectives.

### Model C — Multi-label concept predictor

Predict stable ConceptIDs and context-sensitive associations for documents and passages.

Use structured metadata, trusted classifications, and validated examples as supervision.

### Model D — Query encoder

Train or distill a small, CPU-efficient model that maps search queries into the same feature space used by documents.

This model must be optimized for low latency.

Static embedding models, sparse linear query models, and small neural networks should be evaluated.

### Model E — Query router and ranker

Train a small model to choose retrieval strategies and combine relevance features.

The model should avoid unnecessary expert activation while preserving global search recall.

### Training objectives

Use a combination of retrieval supervision, teacher distillation, concept prediction, and sparsity.

A general training objective may be:

\[
L=
\lambda_1L_{\text{retrieval}}
+\lambda_2L_{\text{distillation}}
+\lambda_3L_{\text{concept}}
+\lambda_4L_{\text{sparsity}}
\]

Sparse-autoencoder reconstruction can be included as an additional auxiliary objective.

The model should learn from positive examples, hard negative examples, and representative cross-disciplinary queries.

Unsupervised vector reconstruction alone must not be treated as proof of retrieval quality.

---

## 15. Search Latency Architecture

The online search path must never require:

- A hosted LLM call.
- Full-corpus vector scanning.
- Full graph traversal.
- Reading complete article content.
- Large transformer inference.
- Synchronous web crawling.

A recommended execution sequence is:

```text
Query
  |
  v
Normalize and encode
  |
  v
Sparse feature activation
  |
  v
Lightweight expert routing
  |
  +--> Semantic postings
  +--> Lexical search
  +--> Dense ANN when needed
  |
  v
Merge candidate IDs
  |
  v
Lightweight neural ranking
  |
  v
Resolve source metadata
  |
  v
Return results
```

The initial target for the local search API should be below 50 ms p95 on appropriately sized, warm indexes.

A sub-20 ms fast path is a stretch goal.

These targets exclude downloading webpages and external model inference. They are not guaranteed performance at billion-document scale on the initial single server.

The system must monitor p50, p95, and p99 latency, CPU consumption, memory use, and recall.

When a request exceeds its retrieval budget, it should return useful partial results rather than blocking indefinitely.

---

## 16. Storage and Scalability

The architecture should never allocate a complete article-by-concept matrix.

Store only activated features and supported explicit associations.

For example, one billion articles with an average of 32 stored associations per article produces 32 billion explicit associations.

At an illustrative average of 8 bytes per compact association, their raw storage would be approximately 256 GB before additional indexes and overhead.

The compressed representation should use local numeric identifiers, quantized values, and immutable segments.

The architecture should separate:

**Hot memory:** Query model, routing tables, frequently used postings, block summaries, caches.

**Warm storage:** Compressed semantic postings, selected vectors, forward indexes, and document identity mappings.

**Cold storage:** Rarely used content, older versions, large documents, and temporary raw crawl material.

The system must scale by partitioning documents and posting lists across search nodes when necessary.

Each node should perform local retrieval and ranking before returning its best candidates to a coordinator.

A global fallback must remain available when semantic routing excludes useful partitions.

Trillion-scale potential relationships should predominantly remain implicit in learned representations, not become trillions of independently stored edges.

---

## 17. Website-Level Semantic Intelligence

Article associations should contribute to website and source-section profiles.

For example, a website containing many relevant articles about endocrinology should acquire a useful representation of that subject.

However, source relevance must not be calculated by simply adding every article's weight.

Source profiles should use normalized aggregates that account for:

- Concentration of relevant documents.
- Coverage of a topic.
- Document quality.
- Source specialization.
- Evidence reliability.
- Freshness.
- Repeated and duplicate content.

The same neural memory can therefore support both article retrieval and source discovery.

This enables a query to locate a relevant website even if no specific article from that website is currently stored in the full-text index.

The source's URL catalog and sitemap structure can then guide targeted online retrieval.

---

## 18. Incremental Learning and Index Updates

Hexxica should distinguish between updating knowledge and retraining models.

New articles should normally require only semantic encoding, concept association, and index insertion.

Retraining should occur when sufficient new evidence or evaluation results justify it.

All learned associations must record the model version that produced them.

New model versions should generate separate index generations or use an explicit migration procedure.

The engine must not mix incompatible feature spaces inside the same search index.

Stable source, resource, and article identifiers should remain unchanged across model upgrades.

This allows the neural memory to evolve without rebuilding the entire identity database.

---

## 19. Migration From the Existing Hexxica Engine

The redesigned index should be introduced incrementally.

### Stage 1 — Restore the existing system

Recover disk capacity, repair answer delivery, reconcile the indexing backlog, and enable search over the existing fleet.

Eliminate duplicate source-first ranking behavior.

Separate local search from optional answer generation.

### Stage 2 — Build a semantic training corpus

Select a representative set of existing articles covering multiple disciplines.

Create query–article relevance examples, including difficult semantic mismatches and cross-topic relationships.

### Stage 3 — Train the first neural sparse memory

Use the existing embedding model as a teacher.

Train the sparse feature encoder and build bidirectional feature postings.

Compare against BM25 and the existing dense retrieval baseline.

### Stage 4 — Implement fast CPU query encoding

Train or distill a compact query model.

Integrate sparse postings with the Rust retrieval engine.

Measure end-to-end query time and retrieval quality.

### Stage 5 — Introduce learned routing and ranking

Add selective retrieval experts and query-aware ranking.

Maintain a global lexical fallback.

### Stage 6 — Build source intelligence

Aggregate article concepts into source profiles.

Connect those profiles with the existing sitemap and URL discovery records.

### Stage 7 — Introduce compressed multivector retrieval

Add Matryoshka-style coarse-to-fine representations and selected fine-grained semantic reranking if benchmarks justify them.

### Stage 8 — Scale and distribute

Optimize compression, index publication, routing, caching, and shard-level processing.

Begin distributed deployment only when single-node benchmarks show it is necessary.

---

## 20. Evaluation Requirements

Every architectural improvement must be evaluated against the existing system.

The primary quality metrics should be:

- Recall@20.
- nDCG@10.
- MRR@10.
- Source Recall@10.
- Evidence coverage.
- Cross-disciplinary query recall.
- Query-concept coverage.
- Hard-negative discrimination.

The primary performance metrics should be:

- p50, p95, and p99 search latency.
- Queries per second.
- CPU time per query.
- Memory footprint.
- Index size per article.
- Storage per semantic association.
- Incremental indexing throughput.
- ANN recall and candidate count.
- Model training and enrichment cost.

The benchmark must include articles whose titles and terminology differ substantially from the queries they should answer.

A title-as-query benchmark is insufficient to validate semantic intelligence.

The project should maintain independent evaluations for retrieval correctness and system performance.

---

## 21. Final Architectural Decision

Hexxica should adopt a **Neural Associative Search Index** as its primary semantic retrieval subsystem.

Its essential components are:

1. A powerful offline document representation model.
2. A custom-trained sparse semantic feature encoder.
3. A compact, multidimensional weighted association index.
4. A lightweight online query encoder.
5. Sparse expert routing inspired by Mixture-of-Experts models.
6. Fast candidate selection inspired by sparse attention.
7. Compressed inverted indexes and optional ANN search.
8. Query-dependent neural ranking over bounded candidate sets.
9. A compact explicit concept relationship graph.
10. A separate identity and source database.

The system should borrow the *principles* of advanced language models rather than reproduce their complete architecture.

**Its intelligence should be developed during indexing, its associations should be stored compactly, and its search should execute using only the small amount of computation required to identify the most relevant article IDs.**

The final goal is a continuously improving semantic memory capable of linking distant concepts, recognizing multiple interpretations of the same information, and locating relevant webpages extremely quickly without permanently storing the entire web.

---

## Research Foundations

- [DeepSeek-V3.2 — Sparse Attention](https://huggingface.co/docs/transformers/model_doc/deepseek_v32)
- [Qwen3-Next — Hybrid Attention and Sparse MoE](https://qwen.ai/blog?id=qwen3-next)
- [Decoding Dense Embeddings with Sparse Autoencoders — EMNLP 2025](https://aclanthology.org/2025.emnlp-main.1345/)
- [Sparse Latent Features for Retrieval — NAACL 2025](https://aclanthology.org/2025.naacl-short.58/)
- [Matryoshka Representation Learning — Google Research](https://research.google/pubs/matryoshka-representation-learning/)
- [ColBERTv2 — Multi-Vector Retrieval](https://aclanthology.org/2022.naacl-main.272/)
- [SEISMIC — Efficient Learned Sparse Retrieval](https://arxiv.org/abs/2404.18812)
- [Compact and Efficient Indexes for Learned Sparse Retrieval — October 2026](https://arxiv.org/abs/2610.12300)
- [Model2Vec Rust Implementation](https://github.com/MinishLab/model2vec-rs)
- [Qwen3 Embedding Models](https://qwenlm.github.io/blog/qwen3-embedding/)
