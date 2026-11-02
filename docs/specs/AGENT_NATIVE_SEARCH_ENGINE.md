# Agent-Native Local Web Search Engine

## 1. Project Mission

Build an open-source search engine designed primarily for AI agents such as OpenClaw, Hermes and custom autonomous systems.

The project should:

- run locally or on an ordinary VPS;
- require no commercial search API;
- understand long natural-language questions;
- distinguish ambiguous entities using context;
- know which websites and source families are likely to contain an answer;
- search its local index first;
- selectively check the live web when freshness is necessary;
- crawl, read and structure webpages, PDFs, research papers, feeds and dynamic sites;
- optionally let an LLM guide difficult searches;
- expose normal web-search UI as well as agent/MCP APIs;
- continuously improve from crawling, user searches and evaluation data.

The system is **not primarily a web archive**.

Its core asset is:

> A compact semantic map of where useful information exists.

---

# 2. Fundamental Architecture

The project has three large planes.

```text
                  ┌──────────────────────┐
                  │     DATA BUILDER     │
                  │                      │
                  │ open datasets        │
                  │ crawling             │
                  │ normalization        │
                  │ graph construction   │
                  └──────────┬───────────┘
                             │
                             ▼
                  ┌──────────────────────┐
                  │      WEB MEMORY      │
                  │                      │
                  │ entities             │
                  │ concepts             │
                  │ sources              │
                  │ URLs                 │
                  │ works                │
                  │ lexical index        │
                  │ semantic index       │
                  │ graphs               │
                  └──────────┬───────────┘
                             │
                             ▼
                  ┌──────────────────────┐
                  │    SEARCH RUNTIME    │
                  │                      │
                  │ query understanding  │
                  │ hybrid retrieval     │
                  │ graph traversal      │
                  │ freshness checking   │
                  │ live web retrieval   │
                  │ agent policy         │
                  └──────────┬───────────┘
                             │
                  ┌──────────┴───────────┐
                  ▼                      ▼
             SEARCH UI              AGENT API
                                    MCP / HTTP
```

The **Data Builder** can be heavy.

The **runtime** must remain small and efficient.

This lets project maintainers process large public datasets and release compact index packs without forcing every user to reproduce the entire crawl.

---

# 3. Core Data Model

Everything in the project should resolve to a small number of object classes.

## Entity

A real or conceptual thing.

```text
Apple Inc.
apple fruit
NVIDIA
PyTorch
Bitcoin
KV cache
```

An entity contains:

```text
EntityID
canonical_name
aliases[]
types[]
concepts[]
relationships[]
source_ids[]
popularity
```

This solves ambiguity such as:

```text
Apple
├ Apple Inc.
├ apple fruit
├ Apple Records
└ other
```

---

## Concept

A semantic subject or relationship that may not correspond to one concrete entity.

Examples:

```text
GPU memory optimization
local LLM inference
plant disease
financial earnings
crypto security incident
API pricing
```

Concepts connect entities and sources.

---

## Domain

```text
python.org
arxiv.org
github.com
example.com
```

Domain records contain:

```text
DomainID
hostname
quality
authority
topics[]
robots
sitemaps[]
feeds[]
crawl statistics
```

---

## Source

A source is more useful than a domain.

Examples:

```text
docs.python.org / asyncio
pytorch.org / documentation
Apple / investor relations
arXiv / cs.AI
GitHub / llama.cpp repository
crypto site / research section
```

A source contains:

```text
SourceID
DomainID
root/path
entities[]
concepts[]
source_type
authority
freshness_model
discovery_methods
known_url_patterns[]
historical_query_success
```

This is the primary unit that semantic retrieval should target.

---

## Resource

Anything that can be fetched.

```text
HTML URL
PDF URL
RSS feed
XML sitemap
JSON endpoint
image
repository object
```

A resource record is intentionally cheap.

---

## Work

A logical intellectual object.

Examples:

```text
research paper
book
software release
news article
report
```

One work may have several resources.

```text
Research paper
├ arXiv PDF
├ publisher page
├ repository copy
└ author manuscript
```

For papers, DOI/arXiv/PMID mappings resolve to one internal `WorkID`.

---

## Content

Unique material after deduplication.

```text
URL A ─┐
URL B ─┼─→ ContentID 18272
URL C ─┘
```

Exact hashes and near-duplicate detection prevent multiple versions of effectively identical pages from inflating the index.

---

# 4. Storage Architecture

The runtime should not depend on one giant database.

Use specialized stores.

```text
metadata/
    domains
    sources
    resources
    works
    crawl state

lexical/
    Tantivy inverted index

graph/
    entities
    concepts
    entity→source
    source→source
    citations
    hyperlinks

semantic/
    source embeddings
    selected document embeddings

frontier/
    crawl queue
    refresh queue

blob-cache/
    temporary HTML
    PDFs
    rendered output

text/
    compressed title/abstract/body blobs
```

Tantivy is a good lexical foundation because it provides BM25 and mmap-backed indexes, so large indexes can rely heavily on the operating system page cache instead of being fully loaded into application memory.

---

# 5. The Data Builder

The builder constructs downloadable search packs.

Inputs can include:

```text
Common Crawl
OpenAlex
Crossref
arXiv
Wikidata/Wikipedia
Open Library
public repositories
library catalogs
existing crawler output
community source definitions
```

Common Crawl's URL Index is already stored as Parquet and designed for large analytical queries using systems including DuckDB, Spark, Arrow and Polars.

OpenAlex provides a downloadable snapshot in JSONL and Parquet, partitioned by update date, which is valuable for scholarly bootstrapping and incremental synchronization.

The builder pipeline is:

```text
RAW DATASETS
     │
     ▼
STREAM IMPORT
     │
     ▼
NORMALIZE IDs
     │
     ▼
DEDUPLICATE
     │
     ▼
ENTITY / WORK RESOLUTION
     │
     ▼
SOURCE DISCOVERY
     │
     ▼
GRAPH CONSTRUCTION
     │
     ▼
INDEX GENERATION
     │
     ▼
COMPRESSION
     │
     ▼
RELEASE PACKS
```

Raw upstream records should be discarded after normalization unless needed for debugging.

---

# 6. Crawl and Discovery Plane

The crawler is a permanent component, not a one-time indexing script.

Its job is to:

```text
discover
prioritize
fetch
classify
normalize
deduplicate
index
refresh
```

The important distinction is:

> Discovery and fetching are separate operations.

The engine may know millions of URLs while having fetched only a fraction.

---

# 7. Source Discovery

A source can enter the system through:

```text
open datasets
known seeds
Common Crawl
hyperlinks
sitemaps
RSS/Atom
DOIs/references
repository catalogs
agent searches
community submissions
```

For a new domain:

```text
DOMAIN
  ↓
DNS/HTTP probe
  ↓
robots.txt
  ↓
sitemaps
  ↓
feeds
  ↓
homepage
  ↓
site classification
  ↓
URL patterns
  ↓
SourceProfile
```

RFC 9309 should be implemented centrally for robots handling rather than separately inside individual crawler workers.

---

# 8. Crawl Frontier

Everything potentially fetchable enters a priority frontier.

```text
FrontierEntry {
    ResourceID
    SourceID
    DomainID

    URL
    type_guess

    priority

    discovered_from
    discovery_method

    earliest_fetch_at

    retries
    expected_value
    expected_cost
}
```

Priority should approximately model:

```text
expected search value
──────────────────────
 expected crawl cost
```

Signals include:

```text
source authority
query demand
novelty
freshness
topic importance
entity importance
URL family quality
change probability
```

Penalties include:

```text
duplicates
crawl traps
very large resources
repeated failures
low-value path families
```

---

# 9. Per-Origin Scheduling

Never treat crawling as one global FIFO queue.

```text
GLOBAL FRONTIER
     │
     ├─ python.org queue
     ├─ arxiv.org queue
     ├─ site-A queue
     ├─ site-B queue
     └─ ...
```

Every origin maintains:

```text
concurrency
next_allowed_request
error rate
latency
429/503 history
robots policy
adaptive crawl delay
```

This allows enormous aggregate crawling throughput while remaining conservative toward individual sites.

At scale, bandwidth comes from:

```text
many independent hosts
```

not:

```text
thousands of simultaneous requests to one host.
```

---

# 10. Fetch Layer

Use a hierarchy.

```text
Resource
   │
   ▼
ordinary HTTP
   │
   ├ successful static content → process
   │
   └ insufficient JS shell
            │
            ▼
         Browser
```

Additional specialized routes:

```text
PDF
XML
JSON
RSS/Atom
OAI-PMH
archive files
images
repository resources
```

The browser is an escalation path, never the default.

Playwright browser contexts are lightweight isolated sessions within one browser process, making them suitable for a small pool of JS-rendering workers.

---

# 11. Freshness System

Every source gets a learned change model.

```text
FreshnessModel {
    historical_change_interval

    feed_available
    sitemap_lastmod_accuracy

    ETag support
    Last-Modified support

    query_demand
    importance

    last_checked
    last_changed
}
```

Sources fall approximately into:

```text
HOT
WARM
COLD
```

Examples:

```text
crypto/news feed          HOT
software releases         HOT/WARM
documentation             WARM
paper metadata            COLD
old books                 COLD
```

Refresh scheduling is adaptive.

---

# 12. Query-Triggered Crawling

This is one of the project's defining features.

Suppose:

```text
"What happened with Ethereum today?"
```

The query engine determines:

```text
Ethereum
+ crypto
+ news/current
```

The source graph activates:

```text
Ethereum official sources
crypto news
exchange announcements
security sources
development feeds
```

Then:

```text
local results
      +
freshness check
      ↓
selected sources stale?
      ↓ yes
urgent refresh
      ↓
new documents
      ↓
merge into active result set
```

This is how the engine achieves “real-time” behavior without continuously recrawling the whole web.

Later, the engine could support publisher notifications such as IndexNow; the protocol allows publishers to signal that URLs have been added, modified or deleted.

---

# 13. Indexing Plane

Every normalized document updates several structures.

```text
NORMALIZED DOCUMENT
        │
   ┌────┼─────────────┐
   ▼    ▼             ▼
lexical graph       semantic
index   edges        representations
```

The lexical index contains:

```text
title
abstract
body
headings
URL
entities
topics
source
type
dates
```

The graph receives:

```text
entity → concept
entity → source
source → document
source → source
work → citation
document → external source
```

Dense embeddings should initially focus on:

```text
sources
important entities
concept clusters
selected high-value documents
```

not every URL.

---

# 14. Search Query Pipeline

Input:

```text
query
conversation context
freshness requirement
mode
caller capabilities
```

Analysis produces:

```text
entities
entity candidates
concepts
phrases
intent
document type preferences
constraints
temporal requirement
query complexity
```

Do not collapse an ambiguous entity immediately.

For:

```text
Apple
```

retain something like:

```text
Apple Inc.       .46
apple fruit      .40
Apple Records    .08
...
```

until context resolves it.

---

# 15. Query Complexity Routing

Use several search modes internally.

```text
C0 direct
C1 semantic
C2 ambiguous
C3 deep / multi-hop
```

Examples:

```text
"Python datetime docs"
→ C0

"posts about LLMs on old GPUs"
→ C1

"Apple health problems"
→ C2

"Find developers who moved from API models
to local models because of cost and identify
the hardware they selected"
→ C3
```

Adaptive-RAG provides strong precedent for routing queries to retrieval strategies of different computational costs rather than using the most expensive strategy universally.

---

# 16. Parallel Retrieval

The first retrieval pass should run concurrently.

```text
                         QUERY

              ┌────────────┼────────────┐
              ▼            ▼            ▼
            BM25      ENTITY/GRAPH    DENSE
              │            │            │
              └────┐       │       ┌────┘
                   ▼       ▼       ▼
                      FUSION
                        │
                        ▼
                SOURCE CANDIDATES
```

Eventually add a learned sparse lane:

```text
BM25
+
SPLADE-like retrieval
```

SPLADE-v3 demonstrates that learned sparse retrieval can significantly outperform BM25 while retaining inverted-index-style search.

---

# 17. Source-First Retrieval

Do not immediately search every individual page semantically.

First:

```text
hundreds of thousands / millions
of source clusters
      ↓
retrieve
      ↓
top 20–100 sources
```

Then:

```text
search URLs/documents
inside selected sources
```

This dramatically reduces the expensive search space.

---

# 18. Search Frontier and Graph Navigation

Candidate interpretations become paths.

```text
ENTITY
  ↓
CONCEPT
  ↓
SOURCE
  ↓
URL FAMILY
  ↓
DOCUMENT
```

Each path has:

```text
relevance
context coherence
authority
freshness
expected information gain
expected cost
```

For simple searches use best-first traversal.

For ambiguity use beam search.

```text
beam width = 2–8
```

Keep several interpretations alive until evidence separates them.

Think-on-Graph 2.0 validates the broader pattern of iteratively alternating graph retrieval and document retrieval rather than making only one retrieval decision.

---

# 19. Agent Policy

An LLM is optional.

The engine always generates valid actions.

For example:

```text
Current state:

1. apple fruit → plant disease
2. Apple Inc. → health software
3. apple fruit → nutrition

Actions:

FOLLOW(1)
FOLLOW(2)
KEEP(1,2)
WIDEN
STOP
```

The agent chooses.

It does not invent random search infrastructure.

Policy implementations:

```text
AlgorithmPolicy
OpenAIPolicy
ClaudePolicy
DeepSeekPolicy
GLMPolicy
CallerAgentPolicy
LocalPolicy
```

Interface:

```text
SearchPolicy.choose(state, actions)
```

The agent is therefore:

> judgment

rather than:

> the entire search engine.

---

# 20. Live Retrieval Loop

When an active source needs new information:

```text
source
 ↓
known recent URLs
 ↓
RSS
 ↓
sitemap
 ↓
site sections
 ↓
focused discovery
```

New documents are scored immediately against the active query.

They do **not** need to wait for a global reindex job.

```text
live document
 ↓
temporary candidate
 ↓
reranking
 ↓
search results

and asynchronously:
 ↓
persistent index
```

---

# 21. Reranking

MVP:

```text
BM25/hybrid candidates
       ↓
top 100
       ↓
small CPU cross encoder
       ↓
top 10–20
```

Small embedding and reranking models are already available in CPU-oriented runtimes; FastEmbed currently lists models such as BGE-small at roughly 67 MB.

Later test:

```text
MUVERA
WARP
```

MUVERA turns multi-vector similarity into fixed-dimensional encodings so ordinary MIPS infrastructure can retrieve initial candidates; its reported experiments achieved around 90% lower latency than prior multi-vector implementations while improving average recall.

WARP reports a 41× improvement versus the reference XTR implementation and around a 3× speedup versus PLAID while preserving retrieval quality.

---

# 22. Ranking

Conceptually:

```text
FinalScore =

lexical relevance
+ semantic relevance
+ entity match
+ concept match
+ graph relevance
+ source authority
+ freshness
+ document-type match
+ query constraint coverage
+ historical source success

- duplication
- low-quality penalties
- stale penalties
```

MVP weights can be manually tuned.

Later replace or augment them with learning-to-rank.

---

# 23. Diversity

Results should not be ten versions of the same story.

Use:

```text
domain diversity
content near-duplicate penalty
semantic diversity
source-type diversity
```

depending on query.

For research questions:

```text
official source
paper
technical explanation
community evidence
```

may provide a stronger result set than four near-identical articles.

---

# 24. Search Stopping

Stop when:

```text
enough results
AND
high confidence
AND
query constraints sufficiently covered
```

or when:

```text
next unexplored path cannot beat current results
```

or:

```text
budget exhausted
deadline reached
```

Every search receives a budget:

```text
graph expansions
source expansions
HTTP requests
browser renders
agent calls
reranking candidates
wall-clock deadline
```

This prevents agent searches from becoming uncontrolled browsing loops.

---

# 25. Search Modes

## Fast

```text
local index only
no HTTP
no agent
```

Target:

```text
tens of milliseconds
```

## Auto

```text
local retrieval
+
freshness check
+
small number of live fetches if required
+
one agent call if ambiguous
```

This should be the default.

## Deep

```text
query decomposition
beam search
multi-source traversal
agent navigation
more live fetches
```

Used for research-style tasks.

---

# 26. Search UI

The project needs a normal search interface from the beginning.

```text
┌─────────────────────────────────────────────┐
│ Search                                      │
│ [ latest improvements to KV cache memory ] │
└─────────────────────────────────────────────┘

All   Web   Papers   Code   News

1. Result
   title
   source · date
   relevant snippet

2. Result
   ...
```

Important debugging toggle:

```text
Why this result?

lexical            .81
semantic           .90
entity             .74
source authority   .96
freshness          .80

source path:
LLM
→ inference
→ memory optimization
→ research papers
```

This UI is essential while building ranking.

---

# 27. Live UI

For current queries:

```text
Local search complete
Checking 8 fresh sources...
2 new items discovered
Updating results...
```

The user gets instant local results while live retrieval continues.

This makes the engine feel responsive even when query-time web access takes longer.

---

# 28. Agent API

Expose both simple and interactive interfaces.

Simple:

```text
POST /search

{
  query,
  mode,
  max_results,
  freshness
}
```

Tools:

```text
search
search_deep

source_lookup
source_refresh

read
find_in_document

follow_path
backtrack
```

Then expose them over:

```text
HTTP
MCP
provider compatibility adapters
```

OpenClaw/Hermes should see essentially:

```text
localhost search
```

rather than understanding the internal architecture.

---

# 29. Evaluation System

AI models are very valuable here as judges.

Maintain a benchmark containing:

```text
short factual queries
long semantic queries
ambiguous entities
current queries
papers
documentation
GitHub
community posts
multi-hop questions
```

For each result:

```text
relevance
constraint satisfaction
freshness
source suitability
```

Use:

```text
LLM judges
+
manual spot checking
```

to build relevance labels.

Metrics:

```text
Recall@K
MRR
nDCG

correct source @ K
correct entity rate

p50 latency
p95 latency

HTTP requests/query
LLM calls/query

index size
RAM/query
```

The product-level metric is:

> Percentage of agent searches successfully completed without a commercial search engine.

---

# 30. Learning System

Every search can generate a retrieval trace:

```text
query
context

candidate sources
chosen paths

documents retrieved

results clicked/used
agent decisions

latency
success/failure
```

From this update:

```text
entity→source weights
concept→source weights
source-success priors
freshness priorities
ranking weights
```

Later these traces train the dedicated small navigation model.

---

# 31. Dedicated Future Search Model

Do not build it initially.

Eventually train:

```text
query/context
+
candidate graph paths
→
path ranking
```

Likely target:

```text
20M–150M parameters
```

It does not need general language generation.

Its responsibilities are:

```text
query semantics
entity disambiguation
concept activation
source routing
branch ranking
```

External Claude/OpenAI/DeepSeek/GLM policies can initially generate teacher data.

---

# 32. Security Architecture

Crawler network access needs strong isolation.

Block by default:

```text
localhost
private IPv4 ranges
private IPv6 ranges
link-local
cloud metadata endpoints
file://
internal service DNS
```

All downloaded content is:

```text
UNTRUSTED_WEB_DATA
```

never agent instructions.

Web content cannot directly initiate tools.

Also enforce:

```text
max download size
max decompression size
redirect limit
parse time
browser time
PDF page limit
archive nesting
```

---

# 33. Deployment on Current Server

Current environment:

```text
16 vCPU
64 GB RAM
~670 GB free SSD
no GPU
```

is suitable for the complete prototype.

Logical deployment:

```text
search-api
crawler-scheduler
HTTP workers
parser workers
PDF workers
browser worker
indexer
metadata DB
Tantivy
vector service
UI
```

For the early stages impose an artificial storage limit:

```text
100 GB
```

even though more is available.

The purpose is to prove that useful search doesn't require hundreds of gigabytes.

---

# 34. Repository Architecture

A clean repository might eventually look like:

```text
/webbrain

  /crates
      core
      models
      metadata
      graph
      frontier
      crawler
      fetcher
      indexer
      retrieval
      ranking
      freshness
      agents
      api

  /workers
      browser
      pdf
      embedding

  /builder
      commoncrawl
      openalex
      arxiv
      wikidata
      openlibrary

  /ui
      web-search

  /adapters
      generic
      sitemap
      feeds
      oai
      arxiv
      github
      libraries

  /benchmarks
      queries
      judgments

  /packs
      schemas
      manifests

  /deploy
      docker
      systemd
```

The core implementation should preferably be Rust.

Browser automation can remain an isolated worker rather than contaminating the core dependency tree.

---

# 35. BUILD STAGES

## Stage 0 — Core Infrastructure

Goal:

Make the skeleton reliable before collecting data.

Build:

```text
internal IDs
metadata schema

SourceRegistry
Resource model
Work model

Tantivy index

crawler frontier
HTTP fetcher
robots cache

sitemap/RSS discovery

canonical URL handling
exact deduplication

basic search HTTP API
```

Corpus:

```text
~100 manually selected pages
```

Exit condition:

```text
URL
→ crawl
→ index
→ search
```

works end-to-end.

Do not add agents yet.

---

## Stage 1 — 10K LLM Search MVP

This is the major first milestone.

Target approximately:

```text
10,000 indexed documents
```

with much larger discovery memory:

```text
100K+ known URLs
hundreds/thousands of sources
```

Corpus intentionally contains:

```text
research
docs
GitHub
blogs/news
model information
community posts
benchmarks
PDFs
```

Build:

```text
BM25
source registry
entity aliases
concept graph
source graph
freshness metadata
basic source-first retrieval
```

Add a normal search UI.

Exit condition:

> Someone can search complicated LLM-related questions and consistently receive useful results.

---

## Stage 2 — Search Quality

Now stop adding lots of documents temporarily.

Build the benchmark.

Target:

```text
500–2,000 queries
```

covering all major search behaviors.

Add:

```text
small embeddings
semantic source retrieval
cross-encoder reranking
entity disambiguation
result diversification
```

Test:

```text
BM25
vs

BM25 + graph
vs

BM25 + graph + semantic
```

Use multiple LLMs as relevance judges plus manual verification.

Exit condition:

> Hybrid retrieval measurably beats plain BM25.

---

## Stage 3 — Live Freshness

Build:

```text
HOT/WARM/COLD sources
RSS monitoring
sitemap refresh
conditional HTTP
change detection
query-driven refresh
```

Test queries:

```text
"latest..."
"today..."
"new release..."
"current price..."
```

The UI should first show local results and then merge newly discovered live results.

Exit condition:

> Fresh-query results become current without relying on another search engine.

---

## Stage 4 — Agent Navigation

Add SearchState and path traversal.

```text
entity
→ concept
→ source
→ URL
```

Implement:

```text
best-first
beam search
backtracking
confidence thresholds
```

Then add provider adapters:

```text
OpenAI
Claude
DeepSeek
GLM
caller agent
```

The agent receives only structured state and allowed actions.

Exit condition:

> Difficult ambiguous searches improve compared with one-shot retrieval while simple queries remain fast.

---

## Stage 5 — 100K to 1M Documents

Only now increase corpus size substantially.

Scale:

```text
10K
→ 100K
→ 1M
```

Measure after every step:

```text
index bytes/document
crawl CPU/document
search latency
RAM
duplicate ratio
relevance
```

Introduce:

```text
URL template learning
near-duplicate detection
adaptive crawl scheduling
compact graph encoding
better compressed storage
```

Exit condition:

> p95 search latency and result quality remain stable as the corpus grows.

---

## Stage 6 — Dataset Bootstrapping

Introduce large public indexes.

Use:

```text
Common Crawl
OpenAlex
Wikidata
Open Library
scholarly sources
```

but process them **streamingly**.

Example:

```text
download shard
→ normalize
→ retain useful records
→ delete shard
```

Do not copy upstream datasets wholesale into the runtime.

Common Crawl becomes the large-scale web discovery mechanism. OpenAlex becomes a scholarly identity/citation/location source.

Exit condition:

> The source map grows orders of magnitude faster than direct crawling alone without exploding disk use.

---

## Stage 7 — Multi-Million Search

Scale toward:

```text
10M+
known/indexed resources
```

At this stage optimize:

```text
compressed IDs
bitmaps
memory maps
source partitioning
vector quantization
distributed frontier
```

Qdrant currently supports scalar, binary and product-style compression schemes, including 4× scalar compression and more aggressive representations; these provide useful options if a dedicated vector engine remains part of the architecture.

Evaluate whether Qdrant remains necessary or whether a more integrated ANN implementation belongs in the Rust runtime.

---

## Stage 8 — Advanced Retrieval

Only after real benchmark evidence indicates it is necessary:

Test:

```text
SPLADE
MUVERA
WARP
late interaction
learning-to-rank
```

Do not adopt algorithms merely because they are cutting-edge.

They must demonstrate improvement in:

```text
quality / RAM / latency
```

on *our* query set.

---

## Stage 9 — Search-Policy Distillation

By now there should be many agent traces.

Dataset:

```text
SearchState
CandidateActions
ChosenAction
Outcome
```

Train small local policy.

Initially:

```text
20M–150M
```

target.

Compare:

```text
local policy
vs
Claude
vs
OpenAI
vs
DeepSeek
```

External models then become optional fallback/teacher systems.

Exit condition:

> Most search navigation no longer requires paid inference.

---

## Stage 10 — Distributed Crawl

Only once indexing quality is proven does large-scale bandwidth become worthwhile.

Architecture becomes:

```text
scheduler cluster

       ↓

thousands of origin queues

       ↓

crawler workers

       ↓

parsers/indexers

       ↓

distributed compact index
```

Bandwidth is used across many independent origins while respecting per-origin policies and RFC 9309.

At this stage:

```text
crawl bandwidth
```

should be determined by **search value**, not by maximum available bandwidth.

---

## Stage 11 — Public Index Packs

Release:

```text
core.pack
developer.pack
research.pack
AI.pack
science.pack
books.pack
news-source.pack
```

Users install only what they need.

Each pack contains primarily:

```text
entities
concepts
sources
important URLs
titles
abstracts where distributable
graph edges
compact search structures
```

not a copy of every website.

---

## Stage 12 — Community Search Network

The open-source community can contribute:

```text
new sources
domain definitions
sitemaps
URL patterns
quality corrections
entity relationships
search judgments
crawler adapters
```

Optional privacy-preserving telemetry can contribute:

```text
query class
successful source
failed source
ranking outcome
```

without requiring submission of private agent conversations.

Over time:

```text
community usage
      ↓
better source map
      ↓
better ranking
      ↓
better search
```

---

# 36. What NOT to Do Early

Do not begin with:

```text
100M pages

a giant transformer

every PDF

a distributed cluster

MCTS

RL

full browser crawling

terabytes of embeddings
```

Those solve scaling problems before we've proven the information architecture.

The central hypothesis is much simpler:

> Can a compact entity/concept/source map tell an agent where useful information lives better than blindly searching a giant collection?

The 10K MVP should answer that first.

---

# 37. MVP Definition

The MVP is finished when all of these work together:

```text
crawler
source registry
URL frontier

normal webpage fetching
feeds
sitemaps
PDF handling

entity/concept/source graph

BM25 search
semantic source retrieval
reranking

freshness check
query-time refresh

normal search UI

agent API

benchmark/evaluation system
```

Not every component must be sophisticated.

But the **full architecture must be present**.

Then scaling is iterative improvement rather than a rewrite.

---

# 38. Final Mental Model

The project consists of five forms of intelligence.

```text
             ┌────────────────────┐
             │       AGENT        │
             │     judgment       │
             └─────────┬──────────┘
                       │
             ┌─────────▼──────────┐
             │    SEARCH ENGINE   │
             │ retrieval/reflexes │
             └─────────┬──────────┘
                       │
             ┌─────────▼──────────┐
             │     WEB MEMORY     │
             │ entities + sources │
             └─────────┬──────────┘
                       │
             ┌─────────▼──────────┐
             │      CRAWLER       │
             │ discovery/freshness│
             └─────────┬──────────┘
                       │
             ┌─────────▼──────────┐
             │      LIVE WEB      │
             │      reality       │
             └────────────────────┘
```

**Crawler = senses.**

**Index/graph = memory.**

**Search algorithm = reflexes.**

**Agent = judgment.**

**Internet = external reality.**

That is the architecture I would use as the project's baseline.
