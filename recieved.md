# Full Architecture Analysis — Hexxica Agent-Native Search Engine

Major architectural gaps

Based on your architecture report measured on 23 October 2026, the system has a solid foundation for building a search engine, but its biggest problems are not missing features. They are incomplete integration, inefficient data handling, weak retrieval quality, and serious operational bottlenecks.

The design is considerably more advanced than what the live system currently delivers.

## 1. Executive assessment

Documents being served

# 19,255

Against 501,161 published fleet documents

Fleet-to-serving ratio

# 26×

Most indexed content is not searchable through the live service

Available disk

# 0 GB

Crawling and uncached answer delivery affected

Semantic ranking weight

# 0%

Dense and graph rankings do not contribute to fusion

Values are from the supplied snapshot, not a new inspection of the running server.&#x20;

Pasted text(20261024-005618).txt

Pasted text(20261024-005618).txt

### My assessment by subsystem

| Subsystem                 | Assessment                    | Main reason                                                           |
| ------------------------- | ----------------------------- | --------------------------------------------------------------------- |
| Overall design            | Good foundation               | Clear separation of crawling, indexing, retrieval, and answering      |
| Web crawler               | Partially mature              | Several adapters and safety mechanisms exist, but crawling is stopped |
| Storage and indexing      | Critical                      | Full disk, write amplification, fleet not served                      |
| Lexical search            | Functional                    | Tantivy BM25 works                                                    |
| Semantic search           | Ineffective in ranking        | Dense fusion weight is zero                                           |
| Entity graph              | Underutilized                 | Built on serving index but has no ranking effect                      |
| Reranking                 | Not implemented in production | Existing stages are mostly candidate limits                           |
| LLM integration           | Functional but fragile        | Provider chaining works; answer delivery can fail after generation    |
| Query planning            | Disabled                      | Speculative design, not evaluated                                     |
| Search quality evaluation | Critical gap                  | No meaningful real-corpus quality measurements                        |
| Scalability               | Unproven                      | One host, several disconnected stores, limited benchmarks             |
| Operations                | Critical                      | No free storage and deployed code differs from repository             |

## 2. The biggest architectural problem

You have several reasonably sophisticated subsystems, but they do not form one complete, optimized search pipeline.

Three examples demonstrate this:

1. The crawler has produced a large corpus, but the search API only accesses a much smaller index.
2. Dense embeddings and an entity graph exist, but the ranking policy ignores their scores.
3. The answer model generates responses, but a failed cache write can prevent those responses from reaching users.

These are integration failures, not simply limitations of the chosen algorithms.

The priority should be to make the existing systems work together before investing in more sophisticated models, crawling adapters, or distributed infrastructure.

## 3. How the architecture actually works

Current architecture — simplified

Web discovery and crawling

Seeds · Sitemap · RSS · Links · HTTP · Chromium · PDF

Parsing and indexing

Normalized documents · SQLite · Tantivy · Sidecars

Serving index

## 19,255

Live searchable documents

Connected

Four fleet shards

## 501,161

Published documents

Not served

Search API and engine

BM25

Fusion weight: 1.0

Source-first

Fusion weight: 0.8

Dense BGE

No ranking contribution

Entity graph

No ranking contribution

RRF fusion → Candidate caps → Evidence coverage

No production cross-encoder reranking

Optional hosted LLM answer

OpenRouter model chain → Cache → API response → UI / MCP

### 3.1 Crawling and discovery

The crawler's job is to find documents, download them, extract useful text, and prepare them for search.

Its pipeline is:

1. Start with seed URLs.
2. Discover more URLs through links, sitemaps, and RSS/Atom feeds.
3. Add URLs to the crawl frontier.
4. Fetch pages while respecting robots rules and per-site request limits.
5. Use Chromium for pages requiring JavaScript.
6. Route HTML and PDF content to the appropriate parser.
7. Normalize the content and identify duplicates.
8. Write document information into storage and indexing systems.

What is good: The architecture separates discovery, fetching, parsing, and indexing. Adaptive per-origin politeness, conditional HTTP requests, and MinHash near-duplicate detection are sensible choices.

What is weak: The crawler supports fewer document types than the original design promises. Common Crawl and several structured-data adapters are only fixture-tested. There is no OCR queue, and the crawl process was stopped.

A second concern is the difference between fetching and publishing. The fleet has 1,090,184 resource records but only 501,161 published index documents. That difference is not automatically a failure because resources include tombstones and unpublished records. However, you need a breakdown of why documents fail to reach the published index.&#x20;

Pasted text(20261024-005618).txt

Pasted text(20261024-005618).txt



### 3.2 Storage and indexing

The architecture uses three main forms of storage.

| Component     | Function                                                         | Architectural concern                                                 |
| ------------- | ---------------------------------------------------------------- | --------------------------------------------------------------------- |
| SQLite        | Document metadata, relationships, and operational state          | Writes and transactions can become a bottleneck during heavy indexing |
| Tantivy       | Full-text inverted index and BM25 retrieval                      | Segment merging and publication cost                                  |
| Sidecar files | Embeddings, entity-document mappings, and related retrieval data | Must remain consistent with document IDs and index generations        |

The division is reasonable. You do not need PostgreSQL, Elasticsearch, or a distributed database just because the corpus is large.

The immediate problem is how these systems are updated together.

The newer repository implements durable generation allocation, two-phase publication, and improved bulk ingestion. However, these changes are not in the deployed binary.

The report also acknowledges approximately 7× write amplification remains in the newer indexing path.

That means reducing unnecessary writes and segment churn is still essential before scaling further.&#x20;

Pasted text(20261024-005618).txt



### 3.3 Search and retrieval

When someone enters a query, the search engine retrieves candidate documents through multiple retrieval lanes.

BM25: Finds documents matching query terms, with ranking based on term frequency, document length, and term rarity. This is effective for exact terminology, technical names, identifiers, and specialized expressions.

Source-first: Searches the same underlying index but gives additional emphasis to source-routed results. It is an accelerator for certain queries, not an independent external search index.

Dense retrieval: Uses the local BGE embedding model to compare the query's meaning with document vectors. This should help when relevant documents use different words.

Entity graph: Uses entity connections and personalized PageRank to find related documents. It could be useful for exploring relationships, but the graph is based on entity co-occurrence rather than verified semantic relations.

The problem is the fusion step.

The score is:

\\[ S(d)=\sum_l \frac{w_l}{60+r_l(d)} \\]

where \\(w_l\\) is the lane weight and \\(r_l(d)\\) is the document's rank in that lane.

Under the current weights, a document ranked first by both active lanes scores:

\\[ \frac{1}{61}+\frac{0.8}{61}=0.029508 \\]

A document found only by dense and graph retrieval receives zero fusion contribution, regardless of how highly those lanes ranked it.

Therefore, the current engine is predominantly a lexical search engine, despite performing additional semantic and graph work.&#x20;

Pasted text(20261024-005618).txt



### 3.4 Ranking and evidence selection

The planned architecture describes a sophisticated cascade with feature ranking, cross-encoder reranking, and evidence verification.

In production, the reported stages mostly do this:

Retrieve and fuse candidate documents

Keep the first N results

Keep a smaller first N

Check query-token coverage

Pass selected evidence to the answer model

Candidate truncation is not reranking. It does not improve the relative order of documents.

The token-based evidence verifier also cannot reliably establish whether a passage actually supports the user's question.

For example, a document mentioning STM32, CAN, and 500 kbit/s does not necessarily establish that the particular STM32 model supports CAN communication at that speed.

That is a retrieval correctness problem, not merely an answer-generation problem.

### 3.5 LLM layer

The LLM design is more practical than the original fully agentic proposal.

It separates three roles:

- Offline structuring of pages into portable packs.
- Speculative query planning and query expansion.
- Answer generation from retrieved evidence.

Only answer generation blocks the normal serving path. Planning is disabled, and offline structuring does not run inside user searches.

The provider chain attempts models in sequence. A transport failure moves to the next model, and the attempt is recorded for debugging and the UI.

This is good for resilience against individual model failures. However, the live list uses one OpenRouter endpoint, and the independently configured fallback provider is disabled. It is model-level failover, not full provider-level redundancy.

The answer model receives at most eight passages with an 8,000-character combined evidence cap. It can generate a cited answer, but the restricted evidence window can omit relevant material for complicated questions.

There is also no evidence that the system validates whether each generated citation genuinely supports its claim.&#x20;

Pasted text(20261024-005618).txt



## 4. Bottlenecks ranked by severity

The following table separates confirmed problems from architectural risks that still need testing.

| Priority | Bottleneck                      | Evidence      | Impact                                        |
| -------- | ------------------------------- | ------------- | --------------------------------------------- |
| P0       | Disk exhaustion                 | Measured      | Service failures, stopped indexing            |
| P0       | Answer cache write failure      | Observed      | Generated answers are lost                    |
| P0       | Fleet disconnected from serving | Confirmed     | Large retrieval coverage loss                 |
| P1       | Dense/graph weight zero         | Confirmed     | No semantic ranking benefit                   |
| P1       | No effective reranker           | Confirmed     | Weaker relevance ordering                     |
| P1       | Weak evidence verification      | Confirmed     | Unsupported answers can pass                  |
| P1       | No real-corpus evaluation       | Confirmed     | Quality improvements cannot be demonstrated   |
| P1       | Write amplification             | Reported      | Excess I/O, disk usage, slower indexing       |
| P1       | Deployment drift                | Confirmed     | Running system misses important fixes         |
| P2       | Brute-force dense retrieval     | Architectural | Increasing CPU cost as corpus grows           |
| P2       | LLM provider dependence         | Confirmed     | Latency and reliability risks                 |
| P2       | SQLite write contention         | Risk to test  | Potential indexing throughput limit           |
| P2       | Single-box deployment           | Architectural | Shared resource contention and failure domain |

### 4.1 Disk exhaustion and write amplification

The reported disk utilization is the most urgent operational problem.

Major reported storage consumers

GB; reported filesystem total is 709 GB, with 0 GB available

0 GB85 GB170 GB255 GB340 GBCrawl fleetProject / build outputsOther used space

The 160 GB remainder is derived from 684 GB used minus the two named consumers. It is not a measured separate storage category. The project/build and fleet directories are not necessarily entirely disposable.

There are two different problems here.

Problem A — insufficient physical space. Crawling, merging index segments, writing cache entries, and rebuilding binaries need writable space.

Problem B — excessive write amplification. Even after adding disk space, writing the same logical data repeatedly increases storage I/O and causes unnecessary segment merging.

The newer base-segment bulk importer helps reduce some writes but does not eliminate the remaining amplification.

Recommended changes:

- Batch documents into larger, bounded index transactions.
- Avoid publishing a new generation for every small indexing change.
- Reduce repeated segment rewrites.
- Apply backpressure when merge work or disk usage exceeds a threshold.
- Reuse existing index segments instead of rebuilding unaffected data.
- Keep crawler storage, build artifacts, and disposable caches under separate limits.
- Measure bytes written per indexed document and merge time separately.

Do not immediately redesign the storage backend. First determine which operations generate the amplification.

### 4.2 Fleet indexing exists, but fleet retrieval does not

Available index size comparison

Live serving index

19,255

Published fleet corpus

501,161

The serving index is 3.84% of the fleet's published document count. This is a size comparison, not proof that exactly 96.16% of unique documents are missing.

This might be the most significant bottleneck affecting real-world search usefulness.

The crawler has already produced a much larger indexable corpus. But that work does not benefit live users.

You have two main strategies.

Option A: Merge all shards into one serving index.

Advantages: simpler searching, consistent scoring statistics, and one unified retrieval pipeline.

Disadvantages: potentially substantial temporary storage, high merge I/O, and longer rebuild operations.

Option B: Federated search over the four existing shards.

Advantages: immediate access to existing indexed documents without duplicating the entire corpus, parallel retrieval, and easier incremental activation.

Disadvantages: cross-shard ranking requires care, document IDs must remain globally unique, and multi-shard query latency must be measured.

I recommend Option B initially, given the limited storage and CPU-only server.

A practical approach is to give every shard a read-only Tantivy searcher, retrieve its top candidates concurrently, deduplicate them, and perform global fusion and reranking.

Do not simply combine raw BM25 scores from independently built indexes. Their statistics may differ. Shard-level rank fusion or calibrated scoring is safer until a global ranking method is evaluated.

The lack of embeddings and entity graphs in the four fleet shards means this would initially expand lexical search. Semantic retrieval can be added incrementally.

### 4.3 Dense retrieval has poor scalability characteristics

Your serving index uses 384-dimensional BGE embeddings with brute-force cosine similarity.

The number of vector dimensions examined per query grows directly with the number of documents.

| Corpus              | Vector dimensions examined |
| ------------------- | -------------------------- |
| 19,255 documents    | 7.39 million               |
| 100,000 documents   | 38.4 million               |
| 501,161 documents   | 192.45 million             |
| 1,000,000 documents | 384 million                |

These are theoretical full-scan element counts, not measured CPU operations or latencies.

Interestingly, the embeddings themselves are not a major storage concern.

For 501,161 documents, 384-dimensional float32 embeddings would occupy approximately 770 MB before index overhead.

This is manageable with the reported 62 GB RAM.

The primary concern is repeatedly scanning all those vectors for every query, especially when supporting concurrent users.

A better design would use approximate nearest-neighbor search, such as HNSW, or an appropriately benchmarked compressed-vector index.

Your `search-ann` crate already implements relevant indexing methods but is not used by the serving application.

However, there is no justification for switching algorithms blindly. Benchmark a full scan against ANN at 19K, 100K, and 500K documents, comparing recall, memory, and p95 latency.

An even more immediate optimization: do not spend CPU computing lanes with zero ranking contribution unless their diagnostics are explicitly requested or otherwise used.

### 4.4 Ranking architecture is more complex than its actual behavior

Your current ranking pipeline has the structure of an advanced search engine but not the ranking capability implied by that structure.

I would use a simpler, fully functional progression:

Stage 1 — Candidate generation

BM25 + semantic retrieval + source routing

Stage 2 — Weighted fusion

Combine independent retrieval evidence

Stage 3 — Actual reranking

Reorder a small candidate shortlist

Stage 4 — Evidence selection

Select relevant passages and check constraints

Stage 5 — Optional answer generation

Generate and validate cited responses

I would test these fusion weights as an initial experiment, not deploy them as assumed optimal settings:

| Lane         | Current | Experimental starting point |
| ------------ | ------- | --------------------------- |
| BM25         | 1.0     | 1.0                         |
| Source-first | 0.8     | 0.4                         |
| Dense        | 0.0     | 0.7                         |
| Graph        | 0.0     | 0.0 initially               |

The graph should remain disabled in fusion until evaluation demonstrates that it improves retrieval. Entity co-occurrence graphs can introduce noisy associations and unrelated results.

A real reranker should only operate on a bounded number of candidates, such as the top 20–50. That controls CPU overhead while allowing meaningful relevance improvements.

## 5. Latency and throughput analysis

The system defines these search budgets:

| Mode | Retrieval deadline | Rounds | Rerank cap |
| ---- | ------------------ | ------ | ---------- |
| Fast | 200 ms             | 1      | 30         |
| Auto | 3 s                | 2      | 100        |
| Deep | 20 s               | 5      | 300        |

These appear to be retrieval and planning budgets, not guaranteed end-to-end HTTP response times.

Answer generation has a separate configured timeout of 40 seconds in the reported deployment. Consequently, an auto-mode request with answer generation can take much longer than its nominal three-second search budget.

The report does not provide actual p50 or p95 latency distributions, so it is not possible to identify the slowest functioning query stage from measurement alone.

### Probable latency contributors

1\. Dense retrieval

The full vector scan becomes increasingly expensive as the index grows. Its relevance contribution is currently zero.

2\. Graph traversal

Graph processing may consume unnecessary CPU without changing ranking. Its actual runtime cost is not measured.

3\. Hosted LLM completion

Remote response time, provider rate limiting, and model failover can dominate answer latency. The recorded historical 83-second planner call demonstrates why blocking an interactive search on remote planning was problematic.

4\. Storage operations

Index reads can compete with merge jobs, crawler writes, SQLite transactions, and other disk activity. Whether this contention currently dominates requires measurement.

5\. Repeated or unnecessary work

The report describes several stages being run even though their outputs cannot affect the final ordering. These are candidates for immediate removal from the default request path.

### Essential instrumentation

Each request should generate stage-level timing data such as:

```
query_total_ms
  engine_plan_ms
  bm25_ms
  source_first_ms
  dense_ms
  graph_ms
  fusion_ms
  rerank_ms
  evidence_verification_ms
  llm_wait_ms
  llm_generation_ms
  cache_read_ms
  cache_write_ms
```

Also record query mode, corpus generation, candidate count, cache hit, provider attempts, errors, and result count.

Track p50, p95, and p99, not just averages.

Without this instrumentation, selecting performance optimizations is largely guesswork.

## 6. Model architecture: strengths and hidden problems

### 6.1 The provider chain is useful, but success criteria are weak

The chain accepts the first provider/model that returns a technically successful response.

That does not necessarily mean the answer is correct.

A model could return a fluent but unsupported answer, and the chain would treat that as success.

A better approach distinguishes transport success, output validity, and evidence validity.

For example:

- Transport succeeds.
- Output matches the required schema.
- Citation indices exist.
- Referenced evidence is present.
- Answer is either supported or explicitly marks uncertainty.

The first three or four checks can be inexpensive and deterministic. Actual factual support requires stronger verification and evaluation.

### 6.2 Cache design has a correctness and reliability issue

The observed failure is especially important:

```
LLM request succeeds
        ↓
Answer generated
        ↓
Cache write attempted
        ↓
ENOSPC
        ↓
Error propagated
        ↓
answer = null
```

That is the wrong dependency for an optional performance cache.

A better flow is:

```
LLM request succeeds
        ↓
Answer generated
        ↓
Attempt cache write
        ├─ Success → return answer
        └─ Failure → log warning → return answer
```

Cache writes should be best-effort unless caching is explicitly required for a correctness guarantee.

Also audit the cache key for provider identity, model settings, prompt version, and evidence content. A cache must never return an answer for materially different source evidence.

### 6.3 The context limit may restrict answer quality

The eight-passage limit is sensible for speed and cost, but may be too restrictive for:

- Comparing multiple technical systems.
- Questions requiring several independent sources.
- Long documents.
- Multi-step evidence synthesis.
- Queries containing many constraints.

I would not simply increase the character limit globally.

Instead, use a query-complexity-dependent evidence budget, passage deduplication, and coverage-aware selection. Give more evidence to complex questions only when retrieval quality justifies it.

### 6.4 Planner architecture is correct in principle

Making LLM planning speculative instead of blocking is a good decision.

The first query can proceed without a model. A later identical query may benefit from cached query decomposition.

However, when planning is re-enabled, the same query may produce different results depending on whether its plan is ready.

That behavior should be evaluated and traced. The 83-second historical planner latency also makes persistent caching and strict background concurrency limits important.

For now, I would leave it disabled until the basic search quality is measured.

## 7. Search quality is a bigger unknown than raw speed

The report contains a working live query with 80 matches and evidence coverage of 1.0.

That demonstrates that the system returns results. It does not establish that its ordering or generated answers are accurate.

The only described evaluation artifact contains eight queries with answer generation disabled. The 30-document, 50-query fixture is useful for regression tests but is not representative of a real 500K-document corpus.&#x20;

Pasted text(20261024-005618).txt

Pasted text(20261024-005618).txt



I would establish a benchmark before changing ranking parameters.

Recommended evaluation framework

| Retrieval recall  | Recall\@20                                |
| ----------------- | ----------------------------------------- |
| Ranking relevance | nDCG\@10, MRR\@10                         |
| Evidence quality  | Constraint coverage and passage relevance |
| Answer quality    | Supported-claim rate, citation accuracy   |
| Performance       | p50/p95 latency, CPU and I/O              |
| Reliability       | Errors, timeouts, cache failures          |

Create a few hundred representative queries covering exact lookup, conceptual search, ambiguous terms, technical questions, recent events, and multi-source questions.

Run ablations: BM25 only, BM25 plus source-first, hybrid with dense, hybrid with graph, then hybrid with reranking.

Only retain a more expensive component when it produces measurable benefits.

## 8. Reliability, security, and maintenance gaps

These are important architectural audits. They are not confirmed vulnerabilities; the report does not contain enough information to verify their implementation.

| Area                 | What should be checked                                             |
| -------------------- | ------------------------------------------------------------------ |
| Crawling security    | Blocking private and internal addresses, including redirects       |
| Browser escalation   | Chromium sandboxing, process limits, timeouts                      |
| Parser resilience    | Oversized PDFs, decompression limits, malformed documents          |
| Prompt injection     | Untrusted crawled text influencing model instructions              |
| API abuse protection | Authentication where needed, rate limits, query limits             |
| MCP access           | Tool permissions and restrictions on actions                       |
| Index consistency    | SQLite metadata and Tantivy publication surviving partial failures |
| Cache retention      | Disk quotas, expiration, eviction behavior                         |
| Recovery             | Backups, rebuild procedures, restart and rollback testing          |
| Observability        | Request logs, storage alerts, crawl failures, indexing lag         |

The system is especially exposed operationally because it is a single host carrying crawling, indexing, search, and model-serving orchestration.

A single-box design is not inherently bad. For a corpus of this size, a well-designed single host may be simpler and more efficient than a distributed system.

But the workloads should be separated and controlled so a heavy indexing operation cannot exhaust resources required by the search API.

## 9. What I would build instead

I would keep the Rust-based modular architecture and improve its integration, rather than rewrite it.

The target design would look like this:

Recommended target architecture

Discovery → Fetch workers → Parsing

Durable ingestion queue and storage controls

Shard 0 + Shard 1

Shard 2 + Shard 3

Parallel shard search → Global fusion

Optional reranker → Evidence verifier

Result API → Optional hosted LLM → UI / MCP

Shared observability, versioned generations, bounded queues, cache quotas and resource monitoring

The key architectural changes would be:

- Use the existing fleet directly through read-only, parallel shard search.
- Separate crawl/index writes from latency-sensitive reads.
- Make dense retrieval an actual ranking signal after evaluation.
- Use a real reranker only on shortlisted results.
- Make evidence verification stronger than keyword presence.
- Keep answer generation optional and cache failures nonfatal.
- Publish consistent index generations and maintain explicit rollback capability.
- Apply resource budgets to each subsystem.

You do not yet need a large distributed architecture or a model trained specifically for this engine.

## 10. Implementation roadmap

Implementation checklist

0 / 21 complete

Phase 1 — Recover availability

Critical

Inspect storage usage and free safe, disposable build artifacts or obsolete caches. Aim for meaningful free-space headroom before restarting ingestion.

Change answer-cache writes to best-effort and test ENOSPC behavior.

Check the exact deployed binary, preserve a rollback copy, and reconcile uncommitted changes before deployment.

Run smoke tests against the serving API and index.

Phase 2 — Establish a baseline

High

Add complete stage-level search latency and resource measurements.

Build a realistic relevance and answer-quality benchmark.

Measure indexing throughput, bytes written per document and segment-merge costs.

Record the baseline before changing retrieval weights.

Phase 3 — Connect the corpus

High

Expose the four existing fleet shards through read-only lexical searchers.

Search shards in parallel and implement global result deduplication and fusion.

Test relevance and tail latency across 19K, 100K and 500K document configurations.

Add regression tests for generations, shard failures and document identity.

Phase 4 — Improve search quality

High

Evaluate BM25, source-first and dense retrieval combinations.

Enable semantic ranking only where it demonstrably improves results.

Integrate a lightweight shortlist reranker if CPU benchmarks justify it.

Improve typed-constraint evidence checks and citation verification.

Keep graph ranking optional until ablation tests support using it.

Phase 5 — Optimize ongoing operations

Medium

Reduce index write amplification and control segment merges.

Add index snapshots, storage alerts, cache quotas and bounded crawl queues.

Restart crawling under explicit disk, CPU, concurrency and per-origin budgets.

Reassess ANN, planner expansion and more advanced retrieval after baseline quality is established.

&#x20;Copy checklist

## 11. What I would not do yet

I would avoid introducing a new programming language, a fully distributed index, an independent query-planning model, large cross-encoders, sophisticated graph reasoning, or further crawler adapters before fixing the fundamentals.

Those features may eventually be useful, but the evidence does not yet show they are the limiting factors.

The system already has enough architectural components to produce a considerably better search engine.

## Final engineering judgment

Your project has three different levels of maturity:

Architecture: Relatively strong. The separation between crawling, indexing, retrieval, and optional model assistance is sound.

Implementation: Uneven. Important components exist but are disconnected, disabled, or reduced to placeholder behavior.

Production readiness: Weak in the supplied snapshot. Disk exhaustion, stale deployment, missing benchmarks, stopped crawling, and answer loss are substantial problems.

The most important changes are not exotic algorithms. They are making the existing 501K-document corpus searchable, eliminating avoidable failures, ensuring ranking actually uses its implemented signals, and measuring whether changes improve relevance.

The fundamental engineering goal should be a smaller number of fully functioning, measurable retrieval stages rather than a larger number of partially integrated advanced components.

This assessment is grounded in the supplied status report rather than a source-code audit or fresh benchmark run. Exact lock contention, CPU hotspots, memory allocations, crash-consistency guarantees, and security defects cannot be established from the report alone.



# Recovery and Indexing Plan for Hexxica Search

The next objective should be to finish processing everything already downloaded, make the existing 501,161 indexed documents searchable, and then resume crawling new pages.

Do not start by adding more crawlers, LLMs, or ranking algorithms. Your immediate problem is that the data pipeline is incomplete and the server has no free disk space.

## 1. What you have and what needs to happen

| Data                                                        | Reported count | Next action                             |
| ----------------------------------------------------------- | -------------- | --------------------------------------- |
| Downloaded resource records                                 | 1,090,184      | Audit their processing status           |
| Published documents in four shards                          | 501,161        | Connect to the serving search engine    |
| Fleet resources not represented by published document count | 589,023        | Classify; process recoverable documents |
| Structured pack records                                     | 45,786         | Resume structuring where needed         |
| Live searchable documents                                   | 19,255         | Extend search across the fleet          |
| Free disk space                                             | 0 GB           | Recover capacity first                  |

The 589,023 difference is not necessarily an indexing backlog. It includes records that may be tombstoned, duplicates, failed downloads, or otherwise ineligible. Count each category before deciding how many documents need processing.&#x20;

Pasted text(20261024-005618).txt



## 2. Correct order of execution

1. Recover disk capacity and fix answer delivery

   Do this first. No new indexing jobs yet.
2. Audit existing downloads, indexes, and unfinished processing

   Identify which resources are already indexed, which can be recovered, and which require refetching.
3. Activate the four existing search shards

   Make the 501K published documents searchable without recreating them.
4. Process and index the downloaded backlog

   Reuse locally stored content and avoid unnecessary HTTP requests.
5. Resume crawling and continuous indexing

   Use bounded workers, checkpoints, deduplication, and storage limits.
6. Improve semantic retrieval and answer quality

   Optimize the larger working search engine after measuring its performance.

## Phase 1 — Recover storage and service reliability

### Step 1. Inspect disk usage safely

Run these on the Ubuntu server:

```
df -h /
df -i /

sudo du -xhd1 /root/projects/search 2>/dev/null | sort -h
sudo du -xhd1 /var/lib 2>/dev/null | sort -h

sudo journalctl --disk-usage
sudo lsof +L1
```

Also find potentially large Rust compilation directories:

```
sudo find /root/projects/search \
  -type d -name target -prune -exec du -sh {} +
```

The report attributes roughly 217 GB to the project directory, including build outputs. Old build artifacts may offer considerable recoverable space.

Do not delete anything under `/var/lib/search-fleet` or `/var/lib/search` to make room. Those directories contain the downloaded/indexed data you want to retain.

Before removing any files, preserve the current source state and configuration, and preferably snapshot the data onto another volume. Then clean verified disposable build outputs, temporary files, or old caches.

I recommend an initial recovery target of at least 100 GB free, followed by storage estimates for bulk indexing and segment merging. The 100 GB is a provisional operational target, not a measured minimum.

### Step 2. Fix the answer-cache failure

Currently, a successful LLM completion can be discarded because writing its cache entry fails.

Change the answer-generation path so:

- A successful answer is returned even when cache writing fails.
- The cache failure is logged.
- Cache failure does not cause the entire answer stage to fail.
- An explicit ENOSPC test verifies this behavior.

This fix is independent of how much free storage you recover.

### Step 3. Deploy the newer indexing code safely

Your running binary was built on October 13, while the repository has indexing improvements through October 18.

The newer code includes durable generation allocation, two-phase publication, metadata-based document counts, and bulk indexing improvements.

Before rebuilding:

1. Preserve the 23 uncommitted files, especially the LLM-provider migration.
2. Record the current executable and configuration.
3. Run the relevant tests after recovering disk space.
4. Build and validate the new binary separately.
5. Switch the service only after a successful smoke test, retaining a rollback option.

Do not treat a Git pull or rebuild alone as a fix for the corpus indexing backlog.&#x20;

Pasted text(20261024-005618).txt



## Phase 2 — Audit and finish processing existing downloads

This should be your next development task after Phase 1.

The objective is to determine the exact processing state of every resource, without downloading anything again unnecessarily.

### Step 4. Build a resource-status audit

The crawler needs to distinguish the following conditions:

| State                      | Meaning                                                     | Required action                   |
| -------------------------- | ----------------------------------------------------------- | --------------------------------- |
| Discovered only            | URL known, content never fetched                            | Queue for crawling later          |
| Fetched, content available | Raw content exists locally                                  | Parse and index offline           |
| Parsed, not indexed        | Extracted text exists                                       | Add to index                      |
| Indexed, not published     | Index work exists but is not visible                        | Recover or finish publication     |
| Published                  | Document exists in a completed index generation             | Leave intact                      |
| Duplicate                  | Equivalent content already indexed                          | Associate with canonical document |
| Failed or unavailable      | Content missing, invalid, or fetch failed                   | Classify for retry or skip        |
| Tombstoned                 | Deliberately removed or invalidated                         | Keep excluded                     |
| Awaiting enrichment        | Searchable text exists, but no embedding or structured pack | Process optionally later          |

Currently, the report doesn't provide enough database-schema information to count these categories.

Begin with repository inspection:

```
cd /root/projects/search

git status --short
git log -5 --oneline

rg -n 'deploy-fleet-corpus|bulk.import|generation' \
  crates docs ops

rg -n 'CREATE TABLE|resource_status|fetch_status|indexed' \
  crates
```

Then inspect the fleet database schemas and existing index manifests. Use read-only inspection where possible.

The audit should produce per-shard counts, totals, and error categories.

Important: Do not infer from the number of database rows that corresponding HTML, PDF, or extracted text files still exist. Confirm their availability before scheduling offline processing.

### Step 5. Implement an offline backlog processor

The indexing pipeline should be capable of running without the web crawler.

Recommended processing flow:

Find locally available, unpublished documents

Read stored content or existing parsed text

Parse only when needed → Normalize → Deduplicate

Batch insert into Tantivy base index

Commit → Validate → Publish generation

Record completed progress → Continue

Use the existing bulk importer rather than feeding hundreds of thousands of historical documents through the mutable overlay.

Tantivy's documented behavior is that commits publish pending documents, while segment merging has its own costs. Frequent commits can produce more small segments and unnecessary merge work.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://docs.rs\&sz=32)

Rust

[image](https://www.google.com/s2/favicons?domain=https://docs.rs\&sz=32)

Rust



For a starting benchmark, test batches of approximately 500–2,000 documents and larger commit intervals, then tune based on document sizes, write amplification, memory use, and recovery time. These are trial settings, not known optimal values.

### Step 6. Make indexing resumable and idempotent

This is essential.

If a process crashes after publishing 40,000 documents, the next run must not rebuild them all or create duplicates.

Implement durable progress markers tied to successful index publication.

The important ordering is:

```
Prepare document batch
        ↓
Write index changes
        ↓
Commit index
        ↓
Publish valid generation
        ↓
Mark batch completed
        ↓
Continue next batch
```

A crash between steps must be recoverable. On restart, reconcile the checkpoint against the published index rather than assuming that every pending batch needs blind reinsertion.

Use a stable resource/document identity and an upsert or reconciliation strategy that avoids repeated indexing of unchanged content.

For SQLite, check journal behavior and checkpoint growth during heavy ingestion. WAL mode supports simultaneous readers and a writer, but still only permits one writer at a time.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://www.sqlite.org\&sz=32)

SQLite



## Phase 3 — Activate the existing 501K-document fleet

Do this before rebuilding the fleet or generating embeddings for everything.

You already have four published Tantivy shard indexes. The fastest route to better coverage is likely to search those existing indexes directly.

### Step 7. Implement a fleet-search adapter

Recommended behavior:

1. Open each published shard read-only.
2. Send a user query to all healthy shards in parallel.
3. Retrieve a bounded candidate list from each.
4. Convert shard-local IDs into stable global document IDs.
5. Deduplicate overlapping documents.
6. Fuse and rank the combined results.
7. Return them through the existing `/search` API.

Start with BM25 retrieval. The four shards have no dense sidecars or entity graphs, so those features should not block integration.

Do not combine raw BM25 scores from separately built indexes without evaluating score comparability. Use rank-based fusion initially.

Add a compatibility adapter for shard-specific metadata if necessary, rather than immediately converting all indexes.

The existing serving index can either remain as a separate search source or be retired after proving that the fleet contains its required documents.

### Acceptance conditions

| Test               | Pass condition                                                      |
| ------------------ | ------------------------------------------------------------------- |
| Shard reachability | All four shard indexes can be queried                               |
| Corpus access      | Published fleet documents are discoverable through the main API     |
| Deduplication      | The same document does not appear multiple times                    |
| Restart            | No committed index data disappears                                  |
| Failed shard       | A partial failure is reported and does not crash the entire service |
| Relevance          | Representative queries return expected documents                    |
| Resource usage     | Search latency and memory remain within measured budgets            |

Do not claim a total of 520,416 unique searchable documents by adding the live index and fleet counts. Their overlap has not been established.

## Phase 4 — Finish the downloaded backlog

After the existing fleet is serving, process the recoverable unfinished documents.

I recommend three separate jobs:

| Job                 | Input                                            | Output                                 |
| ------------------- | ------------------------------------------------ | -------------------------------------- |
| Text backfill       | Locally available content not yet published      | Searchable full-text documents         |
| Metadata repair     | Index rows with missing or inconsistent metadata | Valid document metadata and mappings   |
| Optional enrichment | Published searchable documents                   | Embeddings, entities, structured packs |

Text backfill should have the highest priority. An ordinary searchable document is already useful without an embedding or an LLM-generated structured pack.

Do not require all 1,090,184 resource records to have structured packs before considering indexing complete. Offline structuring involves hosted model calls and is a separate, potentially costly pipeline.

For the backfill, start with one shard, measure throughput and disk write cost, and then increase concurrency only if resources permit.

A sensible admission policy would stop ingestion automatically when free disk falls below a configured reserve. It should preserve progress, not continue until ENOSPC.

## Phase 5 — Restart crawling and keep the index growing

Once existing content is indexed and the fleet is searchable, restart crawling.

Use two independent queues:

Backlog queue

Processes saved content. No new HTTP requests.

Crawl queue

Downloads new or stale pages under site-specific rate limits.

Shared parser and indexing pipeline

Give the saved-content backlog priority until it is substantially cleared. Limit live crawling based on free space, indexing throughput, and HTTP error rates.

For the new crawler, the main improvements should be adaptive per-domain concurrency, `Retry-After` handling for HTTP 429 responses, exponential retry delays, content deduplication, and backpressure when the indexer falls behind.

The target is not maximum pages downloaded per minute. It is maximum useful documents published per minute, without exhausting storage.

## Phase 6 — Improve the complete index

Only after the above work should you activate the expensive retrieval features at scale.

The next sequence should be dense-embedding generation for the fleet, measured hybrid ranking, shortlist reranking, stronger citation verification, and then optional graph expansion and model-based planning.

Keep embedding generation incremental. New documents should not require rebuilding vectors for the entire corpus.

## Progress dashboard to implement

Every operation should report these counters:

| Metric                                   | Why it matters                        |
| ---------------------------------------- | ------------------------------------- |
| Total resource records                   | Overall discovery volume              |
| Locally recoverable, unindexed documents | Actual offline backlog                |
| Published searchable documents           | Real search coverage                  |
| Duplicate/tombstoned/skipped resources   | Explains resource-to-index difference |
| Documents indexed per minute             | Throughput                            |
| Index bytes written per document         | Write amplification                   |
| Failed or retrying documents             | Recoverability                        |
| Index publication lag                    | Freshness                             |
| Available disk and projected growth      | Capacity safety                       |
| Search p95 latency                       | User-facing performance               |

Without these measurements, you cannot reliably tell whether an indexing optimization is helping.

## The next coding task I would assign

Use the following as the first instruction to your coding agent. It deliberately requires repository inspection before modification, so it won't invent commands or assume the database schema.

Implementation brief

&#x20;Copy

This brief is ready to pass to your coding agent. It keeps the existing architecture while prioritizing data recovery, indexing completion, and serving integration.

The milestone I would aim for first: restore reliable answering, expose the existing 501K published documents through the live API, then clear the verified offline indexing backlog without unnecessary recrawling. That will provide far more immediate value than implementing another advanced retrieval component.


