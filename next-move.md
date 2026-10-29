# My assessment: Your idea is stronger than the architecture currently implementing it

I would keep the project, but significantly redesign its priorities. Your intended system is not really a miniature version of Google Search. It is a lightweight web discovery and navigation engine built specifically for AI agents.

That distinction matters because it changes what should be indexed, what should be stored, and where the computational work should happen.

Your fundamental idea is sound:

> Maintain enough knowledge about the web to locate the right information quickly, then retrieve detailed content from the original sources only when an AI agent needs it.

This could be much more storage-efficient than a conventional search engine. It also makes sense for research agents, coding agents, technical documentation search, and deep research workflows.

But the current architecture devotes too much effort to building a large traditional document index, while its most valuable AI-specific capability—intelligent source discovery followed by targeted navigation—is not yet operational.

## 1. Where the current architecture is going wrong

The original crawler specification already contains many good ideas: source profiles, sitemap discovery, URL pattern learning, value-based crawl scheduling, adaptive freshness, and multiple storage tiers. Those are particularly well suited to your goal.&#x20;

Pasted markdown(20261024-010519).md

Pasted markdown(20261024-010519).md



The mismatch is what the implementation emphasizes.

| Area        | Current emphasis                           | What your goal actually needs                                    |
| ----------- | ------------------------------------------ | ---------------------------------------------------------------- |
| Crawling    | Collect and process large numbers of pages | Discover valuable sites and understand their structure           |
| Indexing    | Full-text document indexing                | Lightweight source and URL discovery indexes                     |
| Storage     | Hundreds of GB of crawl data               | Mostly metadata, with selectively cached content                 |
| Ranking     | Ranking individual documents               | Ranking sources first, then relevant pages                       |
| Embeddings  | Document-level vectors                     | Source/section-level vectors first, page-level only when useful  |
| Graph       | Entity co-occurrence relationships         | Site navigation, cross-site citations, and topical relationships |
| LLM         | Generate an answer after search            | Help the agent navigate and collect evidence                     |
| Freshness   | Maintain a large local corpus              | Fetch current information on demand                              |
| Performance | Search index throughput                    | Time, cost, and requests needed to find useful evidence          |

Your current system resembles a small-scale conventional web search engine with an LLM attached.

The better design is a web navigation engine with a small search index attached.

That would make the project more distinctive and much better suited to the hardware you have.

## 2. The architecture I would build

I would call the design a Source-First Retrieval Engine.

It would have two main abilities:

1. Know where information is likely to exist.
2. Navigate those sources efficiently to retrieve the information when requested.

The engine would perform discovery and navigation. The AI model would perform reasoning and synthesis.

Proposed AI-native architecture

AI agent sends a query

Topic, constraints, freshness, and resource budget

Source Discovery Index

Find promising websites, repositories, archives, and sections

Source Ranking + Site Navigator

Consult sitemaps, URL patterns, internal links, feeds, and source profiles

Targeted Page Retrieval

Fetch selected pages, read documents, follow useful links

Evidence Extraction and Coverage Check

Extract passages, identify gaps, verify provenance

Sufficient evidence

Return content and citations to the AI

More needed

Expand to related pages or sources

Persistent source knowledge + lightweight URL index + temporary content cache

The important difference is that the engine does not need to know everything on every website in advance. It needs to know enough to make intelligent decisions about where to look next.

## 3. Four storage layers instead of one giant corpus

Your existing design already proposes storage tiers, but I would make those tiers the central architecture, not an optional storage optimization.

| Layer                  | What is stored                                                            | Retention          |
| ---------------------- | ------------------------------------------------------------------------- | ------------------ |
| L0 — Source directory  | Websites, topics, site reputation, sections, sitemap locations            | Long-term          |
| L1 — URL catalog       | Canonical URLs, paths, titles if known, link context, last-modified hints | Long-term, compact |
| L2 — Selected content  | Useful extracted passages, abstracts, documentation summaries             | Demand-based       |
| L3 — Temporary content | Raw HTML, full document text, browser output                              | Short-term cache   |

Most of the global web knowledge should be in L0 and L1.

For example, an illustrative 10-million-URL catalog averaging 250 bytes of compact metadata per URL would occupy about 2.5 GB of raw metadata. Actual database indexes, link relationships, strings, and storage overhead would increase that figure, potentially substantially.

But it illustrates the opportunity: you do not need to retain full HTML for every URL to know where that URL exists.

There is an important limitation, though. Sitemaps commonly provide URLs and limited hints, not detailed descriptions of what each page discusses. Google also explicitly describes sitemaps as discovery aids rather than guarantees that every listed page will be useful or indexed.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://developers.google.com\&sz=32)

Google for Developers

+1



That is why L1 should combine sitemap entries with link text, URL paths, previously observed headings, and lightweight samples. A pure sitemap index will not be sufficient for reliable topical search.

## 4. The biggest improvement: hierarchical search

This is where I think your engine could differentiate itself.

Rather than searching a single massive document index for every query, search in a hierarchy:

Level 1 — Find relevant sites

Which sources are knowledgeable about this topic?

Level 2 — Find relevant site sections

Documentation, articles, releases, forums, research papers

Level 3 — Find candidate pages

URL structure, title hints, internal links, page summaries

Level 4 — Read relevant content

Actual passages and related evidence

Consider a query such as:

Does the STM32G431 support CAN FD, and what are its limitations?

Instead of searching half a million locally stored documents equally, the system could recognize that STMicroelectronics documentation, reference manuals, device datasheets, and technical discussion forums are relevant source categories.

Then it could inspect the source map, locate relevant manuals or pages, retrieve precise content, and return evidence to the AI.

The system would need to inspect only a relatively small number of promising pages for many such questions.

However, do not enforce a rigid source-first filter for every query. Exact error messages, obscure product identifiers, newly published information, and cross-domain discoveries may be missed if the correct source is not already known.

Keep a global URL/document recall path as a fallback.

## 5. Source knowledge is more useful than endlessly collecting pages

Your existing `SourceProfile` design is one of its strongest elements.

But it should become the primary unit of intelligence.

I would extend it to include a compact topical representation and navigational information:

```
SourceProfile
  identity
  domain
  source_sections
  topic_signatures
  authority_signals
  sitemap_tree
  feed_locations
  url_patterns
  internal_navigation_graph
  citation_relationships
  content_types
  language
  freshness_statistics
  last_verified
  retrieval_success_statistics
```

A domain is not always a single meaningful source. A large website could contain several unrelated communities, documentation products, or subprojects.

The engine should therefore be able to rank the relevant section independently from the broader domain.

I would also prioritize link relationships over an expensive global entity co-occurrence graph at this stage.

For source navigation, knowing that page A links to documentation page B as a related reference is often more immediately useful than knowing that two pages mention the same named entity.

Entity extraction can remain an optional enrichment layer.

## 6. The AI should control exploration, not micromanage every fetch

This is another improvement I would make.

Your current application mainly exposes search and reading operations. The model-driven navigation policy described in the earlier status report does not have an operational transport.

I would replace the complicated planned navigation policy with a small set of deterministic, structured operations.

| Agent operation  | Purpose                                                  |
| ---------------- | -------------------------------------------------------- |
| `search_sources` | Find knowledgeable sources about a topic                 |
| `map_source`     | Get the site's useful sections and navigation structure  |
| `find_pages`     | Locate relevant pages within selected sources            |
| `fetch_pages`    | Retrieve and extract selected documents                  |
| `follow_related` | Explore citations, internal links, and neighboring pages |
| `expand_search`  | Search for evidence still missing from the task          |

Each operation should return compact, typed data. The agent can request full text explicitly when required.

This is better than repeatedly asking an LLM to make low-level decisions such as which URL to open next.

The engine should automatically handle ordinary navigation, redirects, sitemap parsing, link filtering, and retries. The LLM should intervene when the decision requires semantic reasoning.

### The AI can read many articles, but should not always read all of them

You mentioned that AI models can read all the supplied articles, unlike most human users.

That is partly true, but model input has costs and limits. Sending hundreds of irrelevant or duplicate articles wastes tokens and can even make answers worse.

A better strategy is adaptive exploration.

For example, the engine might first select ten promising sources, inspect navigation information from five, retrieve fifteen likely relevant pages, and then decide whether another search round is necessary.

These numbers are illustrative, not optimized limits.

The system should stop when it has sufficient evidence for the requested claims, or when additional retrieval is unlikely to justify its time and cost.

Your current token-based coverage check is not strong enough for that decision. It needs query-constraint and evidence-support checks.

## 7. I would make indexing demand-driven

Right now, the crawler's behavior has largely been driven by collecting and publishing documents.

For your goal, indexing policy should be based on expected future usefulness.

High-value documentation

Retain selected text

Keep useful sections, headings, links, and stable reference passages.

Ordinary websites

Metadata first

Store source profiles and page-location hints; fetch body text when useful.

Frequently changing pages

Refresh on demand

Store URL, update history, and short-lived content. Prefer conditional requests and feeds.

Large research papers and books

Metadata + location

Retain identifying metadata, abstracts, and access locations. Extract full content when permitted and needed.

This also means not every downloaded resource needs to become a permanent, fully indexed document.

Your crawler specification already recognizes this principle, but the actual system needs to enforce it through storage quotas and ingestion policy.&#x20;

Pasted markdown(20261024-010519).md



The right retention decision should also account for whether content can be retrieved again. Rare or disappearing sources may justify persistent snapshots, while stable public documentation may be inexpensive to re-fetch.

## 8. The biggest unsolved challenge is global source discovery

This is the part that prevents a small local system from realistically matching Google's general web coverage.

You can navigate a website efficiently once you know it exists. But how do you discover the best website for an unfamiliar topic?

A crawler starting with a few seeds can miss entire communities, specialized publications, and recently created sites.

Your system should combine independent discovery channels instead of relying exclusively on its own crawl graph.

Useful foundations include public web URL indexes, host-level link graphs, topic directories, scholarly datasets, user-submitted seeds, and optionally external search APIs for cold-start discovery.

For example, Common Crawl publishes both URL indexes and host/domain web graphs. Those can help bootstrap a source directory without downloading every page yourself.&#x20;

[image](https://www.google.com/s2/favicons?domain=https://commoncrawl.org\&sz=32)

Common Crawl

+2



The important design principle is to keep those providers interchangeable. The open-source core should not require a proprietary search service to function, even if external services can improve coverage.

Also, source quality must be judged by more than domain popularity. Official documentation is useful for product specifications, while specialized forums may contain workarounds or field observations unavailable in official material.

A good AI retrieval engine must know when to include both.

## 9. What I would remove, retain, and redesign

| Component                     | Decision               | Why                                                               |
| ----------------------------- | ---------------------- | ----------------------------------------------------------------- |
| Rust core                     | Keep                   | Appropriate for efficient crawling and indexing                   |
| SQLite metadata               | Keep                   | Reasonable at this scale with controlled writes                   |
| Tantivy                       | Keep                   | Useful for source, title, URL, and selective document retrieval   |
| Source registry               | Expand substantially   | Central to the new goal                                           |
| Sitemaps and feeds            | Prioritize             | Cheap source and URL discovery                                    |
| URL pattern learning          | Keep                   | Especially useful for navigation                                  |
| Per-origin scheduler          | Keep                   | Necessary for responsible live retrieval                          |
| Global full-text indexing     | Make selective         | Not every page deserves permanent indexing                        |
| Per-document embeddings       | Make optional          | Prefer compact source/section representations initially           |
| Entity graph                  | Deprioritize           | Link navigation is more immediately useful                        |
| Cross-encoder reranking       | Optional               | Useful for shortlists, but measure CPU cost                       |
| LLM structuring of every page | Reduce                 | Expensive and unnecessary for broad discovery                     |
| LLM planner                   | Redesign               | Use bounded navigation with deterministic operations              |
| LLM answer generator          | Optional               | External AI agents should be able to do their own synthesis       |
| Large raw crawl cache         | Reduce after migration | Conflicts with minimal-storage goal                               |
| 36-crate structure            | Review                 | Keep useful separation, remove unnecessary integration complexity |

The 36-crate count itself is not a performance defect. However, for a miniature engine, the operational complexity must be justified by real capabilities rather than architectural ambition.

## 10. How I would transition your existing implementation

I would change the previous indexing plan slightly given this clearer objective.

You should still recover the disk, repair answer delivery, and make the four existing shards accessible. But I would not blindly attempt to retain full text for all 1.09 million resource records.

Instead, use your existing data as the foundation of the new architecture.

### Recommended development sequence

1. Recover storage and preserve the fleet. Do not throw away downloaded content until its metadata and useful relationships have been extracted and validated.
2. Create the compact source directory and URL catalog. Build site profiles, source sections, navigation links, sitemap references, and topical signatures from the existing corpus.
3. Build hierarchical retrieval. Implement source ranking, within-source navigation, and a global lexical fallback.
4. Implement the agent retrieval interface. Add structured source discovery, site mapping, targeted fetching, and related-page exploration through the API and MCP.
5. Add a demand-driven content cache. Retain raw text temporarily and selectively persist high-value material.
6. Benchmark against the old architecture. Compare retrieval quality, storage usage, response times, network requests, and evidence completeness before retiring any large index.

The existing 501K-document index remains useful as a transition asset and as a benchmark against the lightweight approach.

## 11. How to measure success for this engine

For your goal, total document count is a poor primary measure.

I would measure:

| Metric                    | What it tells you                                                      |
| ------------------------- | ---------------------------------------------------------------------- |
| Source Recall\@10         | Did the engine find the sources needed for a query?                    |
| Page Recall\@20           | Did it locate the relevant pages?                                      |
| Evidence coverage         | Did it retrieve enough supporting material?                            |
| Citation accuracy         | Are returned statements supported by those sources?                    |
| Time to useful evidence   | How quickly can an agent begin its task?                               |
| Network requests per task | How efficiently does it navigate?                                      |
| Storage per source / URL  | How compact is the index?                                              |
| Task success rate         | Can the external AI complete the task using the retrieved information? |

Compare these against a conventional search-based agent under the same query set, token budget, and time budget.

You may find that your system performs much better on documentation and specialized research while performing worse on broad, obscure, or breaking-news queries. That would still be a valuable outcome, provided the scope is explicit.

## 12. Can it be as powerful as Google?

For focused, source-based research: potentially very competitive.

An AI-native navigator can explore documentation trees, follow references, combine multiple sources, and provide richer evidence than a conventional ranked list of ten blue links.

For discovering and instantly ranking arbitrary pages across the entire web: not realistically Google-equivalent on one small server.

That requires immense discovery coverage, historical link knowledge, ranking infrastructure, freshness monitoring, and resistance to web spam.

The practical target: Google-like usefulness for AI research tasks, with a fraction of the stored data.

Achieve that by concentrating on source discovery and efficient on-demand exploration, not replicating Google's index.

## Final recommendation

I think your project should evolve from an agent-native search engine into an agent-native web discovery and retrieval infrastructure.

The durable knowledge should primarily describe the web: which sites exist, what they cover, how their pages are organized, how they relate to other sources, and when their content should be revisited.

The expensive work—opening pages, parsing large documents, selecting passages, and assembling evidence—should happen mainly when an actual AI task requires it.

That gives the project a coherent architectural rule:

Index where knowledge lives. Retrieve the knowledge when needed. Cache what proves useful.

It is a stronger, more realistic, and more distinctive objective for an open-source miniature search engine than trying to reproduce Google's traditional crawl-and-index architecture at a smaller scale.


The following is a proposed project specification and migration roadmap for Hexxica. It is intended to guide development agents and future contributors while keeping the existing Rust codebase and downloaded data useful.

It is based on your current implementation and the original universal crawler specification. The reported baseline includes 501,161 published fleet documents, 19,255 documents in the live index, and unresolved storage and integration problems.&#x20;

Pasted text(20261024-005618).txt

Pasted markdown(20261024-010519).md



# Hexxica: AI-Native Web Discovery and Retrieval Engine

## 1. Project Vision

Hexxica is an open-source, lightweight web discovery, navigation, and retrieval engine designed primarily for artificial intelligence (AI) agents.

The goal is to provide AI systems with powerful web search and research capabilities without requiring the infrastructure or storage capacity of a conventional large-scale search engine.

Unlike traditional search engines, Hexxica is not primarily designed to maintain a complete local copy of web content or present ranked links to human users.

Instead, it maintains a compact understanding of where information exists, how websites are organized, and how to retrieve useful information efficiently.

When an AI agent needs information, Hexxica identifies promising sources, explores their structure, retrieves relevant material, and returns organized evidence for the AI model to analyze.

**The fundamental principle is:**

**Index where information exists, retrieve the information when needed, and retain only what provides lasting value.**

The long-term ambition is to approach the research usefulness of much larger search engines for AI workflows while using significantly less storage and computational power.

This is a goal to evaluate experimentally, not an assumption that a small engine can match Google's complete web coverage.

---

## 2. Problem Statement

Traditional search engines are designed to crawl enormous quantities of web content, maintain extensive indexes, and rank individual pages for human users.

This approach requires substantial storage, computing resources, continuous crawling, and sophisticated ranking infrastructure.

AI agents have different requirements.

An AI model can process the contents of multiple documents, follow structured references, examine technical documentation, and combine information from several sources. It does not necessarily need a conventional search engine to preselect every relevant passage.

However, AI models still need an efficient way to discover reliable information sources and navigate the web.

Existing approaches present several problems.

### 2.1 Excessive local storage

Conventional crawling architectures frequently retain extracted page content, document indexes, embeddings, link data, and temporary files.

Much of this information may never be requested again.

For a small independent search engine, continuously collecting full documents can cause storage usage to grow faster than the practical value of the collected information.

### 2.2 Incomplete information discovery

AI agents often depend on a small collection of search results returned by an external service.

Relevant information may exist deeper within a website, including documentation sections, sitemap-listed pages, research references, archived articles, and related resources.

A conventional ranked-results interface does not always expose this structure.

### 2.3 Repeated and inefficient browsing

AI agents may repeatedly discover the same websites, examine the same navigation pages, download duplicate content, and perform unnecessary search operations.

Without persistent knowledge of source structures, previous exploration provides little benefit to future requests.

### 2.4 Dependence on external search providers

Many AI workflows rely on commercial search APIs for web discovery.

This creates dependency on external pricing, rate limits, availability, and returned search coverage.

An open-source alternative should be capable of independent discovery while optionally using external providers to improve coverage.

### 2.5 Inefficient separation between search and reasoning

Traditional search systems perform substantial document-level indexing and ranking before the AI model begins its work.

For AI research, a more efficient approach may be to identify promising information sources first and perform deeper retrieval only where necessary.

The challenge is to build a compact, reliable web knowledge system that can locate relevant information without having to permanently store the contents of the entire web.

---

## 3. Main Objective

To develop an open-source, resource-efficient search infrastructure that enables AI agents to discover relevant websites, understand their organization, retrieve information on demand, and conduct multi-source research using minimal persistent storage.

## 4. Specific Objectives

### Objective 1 — Build a compact global source directory

Maintain information about websites, repositories, publications, documentation collections, archives, and other valuable information sources.

Record their topics, structure, discovery methods, quality indicators, and update patterns.

### Objective 2 — Build a lightweight URL discovery index

Store useful page-location information without requiring complete page content for every URL.

Support searching by URL structure, titles, link text, topics, source sections, and available metadata.

### Objective 3 — Enable hierarchical source-first retrieval

Identify relevant sources before exploring individual pages when this is advantageous.

Retain global lexical and other discovery methods to avoid missing relevant pages from unknown or unexpected sources.

### Objective 4 — Develop intelligent website navigation

Use sitemaps, feeds, internal links, navigation menus, references, and URL patterns to locate useful pages efficiently.

Avoid blindly downloading entire websites.

### Objective 5 — Implement on-demand content retrieval

Fetch full page content only when requested or when it provides sufficient expected value.

Provide clean extracted text, document structure, references, and provenance to AI agents.

### Objective 6 — Minimize storage and repeated processing

Implement tiered storage, content deduplication, selective indexing, temporary caching, and incremental refresh.

### Objective 7 — Support agent-driven iterative research

Allow AI agents to request additional sources, follow relevant references, inspect site sections, and retrieve more evidence when needed.

### Objective 8 — Maintain independent operation

Keep the core open-source system functional without mandatory dependence on commercial search services, hosted LLMs, or GPU hardware.

### Objective 9 — Deliver measurable search quality

Evaluate source discovery, page retrieval, evidence coverage, accuracy, response time, network usage, and storage efficiency.

### Objective 10 — Keep the architecture extensible

Support new content formats, source adapters, ranking techniques, and distributed deployment without redesigning the core engine.

---

## 5. Project Scope

### Core capabilities

The system should support:

- Website and source discovery.
- Sitemap and RSS/Atom processing.
- URL and section indexing.
- Link extraction and navigation.
- Source and document metadata.
- Technical documentation and Git repository discovery.
- Research paper and PDF retrieval.
- Query-driven web exploration.
- Content extraction and deduplication.
- Structured agent APIs and Model Context Protocol (MCP) integration.
- Selective persistent caching.
- Freshness monitoring.
- Search quality and performance evaluation.

### Non-goals

The initial project should not attempt to:

- Permanently archive the entire public web.
- Duplicate Google's complete search infrastructure.
- Index every discovered URL as a full-text document.
- Generate embeddings for every web page automatically.
- Train a proprietary large language model.
- Require a GPU for ordinary operation.
- Reproduce every feature of a commercial human-oriented search engine.
- Guarantee that every site can be crawled or accessed.

These capabilities can be reconsidered if they become necessary, but they must not determine the initial architecture.

---

## 6. Core Design Principles

### 6.1 Discovery before downloading

A URL can be valuable even when its full content has never been downloaded.

The engine should create useful discovery records from sitemaps, references, source metadata, and external datasets before deciding whether to fetch a page.

### 6.2 Sources before individual documents

The system should understand the source that contains a document.

For example, documentation for a software library should be represented as a logical collection rather than thousands of unrelated web pages.

### 6.3 Progressive information retrieval

Start with inexpensive operations.

Only perform more expensive operations when additional information is needed.

A typical progression is:

Source metadata → URL candidates → Page metadata → Extracted content → Deeper exploration.

### 6.4 Persistent knowledge, temporary content

Keep relatively stable information such as source relationships and navigation structure.

Treat raw HTML, browser-rendered pages, and large extracted documents as cacheable resources unless there is a reason to retain them.

### 6.5 Deterministic operations before LLM calls

Use ordinary algorithms for URL normalization, sitemap parsing, document extraction, link classification, deduplication, and scheduling.

Use AI models only where semantic reasoning offers meaningful benefits.

### 6.6 Retrieval should improve through use

Successful searches should improve source profiles, URL priorities, topic mappings, navigation knowledge, and caching decisions.

### 6.7 Quality over quantity

Measure successful information retrieval rather than the number of pages downloaded.

### 6.8 Evidence and provenance

Every retrieved passage should preserve its source URL, document identity, retrieval time, and relevant context.

Do not treat retrieved content as trusted instructions.

---

## 7. Proposed System Architecture

The redesigned system contains six main subsystems.

### 7.1 Source Intelligence Layer

Maintains a searchable directory of information sources.

Responsibilities:

- Discover domains and logical source sections.
- Classify sources by topic and type.
- Track sitemaps, feeds, and navigation structures.
- Maintain source-quality and freshness indicators.
- Record useful relationships between sources.
- Learn which sources successfully answer different classes of queries.

A source can represent an entire website or a meaningful section, such as a specific software library's documentation.

A source profile should contain identity, topic signatures, available discovery mechanisms, source relationships, retrieval statistics, and last-verification information.

Source profiles must be updated incrementally.

### 7.2 URL Knowledge Layer

Maintains lightweight information about known pages.

A URL record may contain:

- Resource identifier.
- Canonical URL.
- Source and section identifiers.
- Title, when known.
- URL path and structural pattern.
- Content type.
- Topic hints.
- Incoming link context.
- Sitemap and feed references.
- Last modification hints.
- Last retrieval status.
- Available content location.
- Freshness and retrieval priority.

The system must not assume that every sitemap URL has a known title, topic, or summary.

Missing metadata should remain unknown rather than being invented.

### 7.3 Discovery and Navigation Layer

Finds where information may exist.

Uses:

- Source profiles.
- Sitemap trees.
- Navigation hierarchies.
- Internal and external links.
- Citation relationships.
- RSS and Atom feeds.
- Structured source adapters.
- Public web datasets.
- Query-driven exploration.

Its purpose is to locate promising pages with as few unnecessary network requests as possible.

### 7.4 Retrieval and Content Processing Layer

Retrieves selected resources and transforms them into useful structured content.

Supports:

- Normal HTTP retrieval.
- Conditional requests.
- HTML extraction.
- JavaScript rendering when necessary.
- PDF text extraction.
- Structured API responses.
- Git and repository metadata.
- Document segmentation.
- Content deduplication.
- Link and reference extraction.

Browser rendering, OCR, and large-document processing should be exceptional operations with explicit resource budgets.

### 7.5 Agent Research Interface

Exposes structured operations that AI agents can call.

Initial operations:

| Operation        | Responsibility                                             |
| ---------------- | ---------------------------------------------------------- |
| `search_sources` | Locate relevant sources for a topic                        |
| `map_source`     | Return a source's useful sections and navigation structure |
| `find_pages`     | Locate relevant page candidates                            |
| `fetch_pages`    | Fetch and extract selected documents                       |
| `follow_related` | Locate related pages, references, or citations             |
| `expand_search`  | Search for missing information beyond current sources      |

The interface should return structured results rather than requiring the AI agent to parse ordinary web search pages.

The engine should return evidence and navigation information by default. Answer generation may remain an optional feature.

### 7.6 Storage and Learning Layer

Controls what information is retained.

It should maintain:

- Source registry.
- URL and section catalog.
- Link relationships.
- Selected full-text index.
- Temporary content cache.
- Query and retrieval statistics.
- Freshness information.
- Optional source-level embeddings.

All retained data should have a clear purpose.

---

## 8. Hierarchical Retrieval Workflow

For a query such as:

"Compare the communication capabilities of ESP32-S3 and STM32G431."

The system should:

1. Analyze the requested topics and constraints using inexpensive parsing first.
2. Search the source directory for relevant manufacturers, documentation collections, and technical references.
3. Identify sections likely to contain communication interface specifications.
4. Search their URL catalogs, sitemaps, and navigation indexes.
5. Retrieve promising pages or documents.
6. Extract relevant specifications and supporting passages.
7. Identify missing details or contradictions.
8. Follow additional references where necessary.
9. Return structured evidence with citations to the requesting AI agent.

The system should not automatically download the entire manufacturer's documentation website.

If the source-first approach finds insufficient candidates, the engine should expand to a broader global document/URL search and other independent discovery methods.

The AI agent may request more evidence, but routine crawling and navigation must remain automated inside the engine.

---

## 9. Storage Strategy

Implement four logical storage tiers.

| Tier                   | Data                                                                | Default policy     |
| ---------------------- | ------------------------------------------------------------------- | ------------------ |
| L0 — Source knowledge  | Sites, topics, sections, discovery methods, source relationships    | Retain             |
| L1 — URL knowledge     | URL metadata, link context, paths, discovered pages                 | Retain compactly   |
| L2 — Selected content  | Valuable passages, documentation summaries, selected extracted text | Retain selectively |
| L3 — Raw content cache | HTML, PDFs, rendered content, temporary extraction output           | Expire or evict    |

Retention must be based on expected value, retrieval cost, freshness requirements, and available storage.

### Special retention considerations

Frequently requested documents may be retained longer.

Rare or disappearing sources may warrant snapshots.

Large public research papers may be represented by metadata and access locations until full content is requested.

Dynamic pages should primarily rely on freshness tracking rather than permanent storage.

Original source attribution and applicable access or licensing restrictions must be preserved.

A cache entry must never be removed while it is the only recoverable copy required by an unfinished indexing or publication operation.

---

# 10. Migration Plan From the Existing Architecture

The current engine already contains important reusable components.

The migration should preserve those components rather than start a new repository.

The existing full-text corpus should also remain available until the replacement system has been evaluated.

## Phase 0 — Stabilize and Preserve the Current System

**Objective:** Restore reliable operation and prevent data loss.

Tasks:

1. Audit available disk capacity and large directories.
2. Free verified disposable build outputs and obsolete caches.
3. Preserve existing crawl data, published index generations, and metadata.
4. Fix the answer cache so cache-write failure cannot discard a generated answer.
5. Review uncommitted source changes.
6. Test and deploy newer indexing changes safely.
7. Record baseline indexing throughput, search latency, resource usage, and corpus counts.
8. Add disk-space safeguards to ingestion jobs.

Deliverables:

- Healthy running service.
- Recoverable storage capacity.
- Preserved fleet data.
- Reliable answer delivery.
- Baseline performance report.

**Exit criteria:** The service can search reliably, has enough free space for controlled development, and has a tested recovery and rollback process.

## Phase 1 — Audit and Recover the Existing Corpus

**Objective:** Understand and reuse already collected information.

Current reported data includes approximately 1.09 million fleet resource records and 501,161 published fleet documents.

Not all resource records necessarily contain downloaded content or represent documents that should be indexed.

Tasks:

1. Identify the exact resource states in each shard.
2. Count discovered-only, fetched, parsed, indexed, unpublished, failed, duplicate, and tombstoned resources.
3. Verify which raw documents or extracted text remain locally available.
4. Find missing document metadata and broken links between records.
5. Complete recoverable indexing operations using locally available content.
6. Make processing resumable and idempotent.
7. Avoid downloading already available content again.
8. Preserve the existing fleet indexes for search integration.

Deliverables:

- Corpus status report.
- Verified offline processing backlog.
- Resumable backlog processor.
- Consistent published index generations.

**Exit criteria:** Every existing resource has a known recoverable or terminal state, and eligible locally stored content can be processed without recrawling.

## Phase 2 — Make the Existing Fleet Searchable

**Objective:** Use the 501K published document corpus immediately.

Tasks:

1. Open the existing Tantivy shards through read-only search adapters.
2. Search multiple shards concurrently.
3. Establish globally stable document identities.
4. Merge and deduplicate results.
5. Apply cross-shard rank fusion.
6. Connect fleet results to the existing search API.
7. Validate metadata and passage retrieval.
8. Measure memory usage and search latency.

Do not rebuild every index merely to achieve integration.

Deliverables:

- Federated shard search.
- Global document identification.
- Shared result representation.
- Fleet-aware search API.

**Exit criteria:** Existing published fleet documents are discoverable through the main API with correct attribution and acceptable measured latency.

## Phase 3 — Build the Compact Source Directory

**Objective:** Convert existing crawl knowledge into the new core index.

Tasks:

1. Extract domains and logical source sections from existing metadata.
2. Build or complete the source registry.
3. Associate resources with their sources.
4. Import known sitemap and feed locations.
5. Group resources by useful URL patterns.
6. Collect navigation and citation relationships.
7. Create topical representations for sources and sections.
8. Add source-level search.
9. Identify and consolidate redundant source records.

Use deterministic metadata and text processing first.

Generate embeddings only where they improve source discovery.

Deliverables:

- Searchable source directory.
- Logical source-section records.
- Topic-to-source lookup.
- Source relationship index.

**Exit criteria:** A topic query can produce a ranked set of relevant sources without requiring a full-document scan.

## Phase 4 — Build the Lightweight URL Catalog

**Objective:** Find useful pages without requiring their full content to be stored.

Tasks:

1. Develop a compact canonical URL record.
2. Import URL identities from the fleet.
3. Add available titles, paths, link text, and content-type metadata.
4. Store sitemap membership and navigation relationships.
5. Index URL paths, titles, and available descriptions.
6. Add section-level page filtering.
7. Support sitemap-only URL records with incomplete metadata.
8. Build source-specific page lookup.
9. Keep a global lexical fallback across known pages.

Deliverables:

- Compact URL catalog.
- Source-scoped page search.
- Global page discovery.
- Sitemap-aware page selection.

**Exit criteria:** Relevant page candidates can be found using compact metadata, without requiring full content for every URL.

## Phase 5 — Implement Query-Driven Navigation

**Objective:** Retrieve useful information using minimal unnecessary network requests.

Tasks:

1. Build hierarchical source-to-page search.
2. Implement source navigation maps.
3. Support selective retrieval from sitemaps and internal links.
4. Use existing URL pattern learning to prioritize useful paths.
5. Add related-document and citation traversal.
6. Integrate the existing HTTP and browser fetchers.
7. Add document extraction and passage selection.
8. Keep robots, security, timeout, and resource limits enforced.
9. Provide bounded exploration and stop conditions.

The engine should choose inexpensive retrieval actions before expensive ones.

Deliverables:

- Source navigator.
- On-demand page retrieval.
- Related-page exploration.
- Evidence extraction pipeline.

**Exit criteria:** For representative research queries, the system can navigate from known sources to useful supporting documents without requiring a permanent full-text corpus.

## Phase 6 — Implement the AI Agent Interface

**Objective:** Make Hexxica useful as an independent web research tool.

Tasks:

1. Implement `search_sources`.
2. Implement `map_source`.
3. Implement `find_pages`.
4. Implement `fetch_pages`.
5. Implement `follow_related`.
6. Implement `expand_search`.
7. Return structured documents and evidence references.
8. Provide explicit freshness and retrieval status.
9. Support request-level limits for time, network use, and returned content.
10. Expose these capabilities through HTTP and MCP.
11. Retain compatibility with existing `/search` and `/read` clients where possible.

The returned data should distinguish:

- Known metadata.
- Previously cached content.
- Freshly retrieved content.
- Unverified hints.
- Failed or inaccessible resources.

Deliverables:

- Stable agent-facing API.
- MCP tools.
- Structured evidence format.
- Example AI research workflows.

**Exit criteria:** An external AI agent can discover sources, inspect navigation, fetch evidence, and continue research without requiring a separate web search system for routine supported tasks.

## Phase 7 — Implement Demand-Driven Storage and Learning

**Objective:** Make the engine smaller and more efficient as it operates.

Tasks:

1. Track repeated source and page usage.
2. Improve source priorities from successful retrieval.
3. Track content changes and freshness.
4. Add content-cache expiration and eviction.
5. Deduplicate repeated downloaded content.
6. Retain selected high-value documents.
7. Reduce unnecessary document embeddings and LLM structuring.
8. Add storage quotas and free-space protection.
9. Schedule refreshes based on source behavior and query demand.
10. Preserve rare content when regeneration or refetching would be difficult.

Deliverables:

- Adaptive cache.
- Demand-based refresh scheduler.
- Source-learning statistics.
- Controlled storage growth.

**Exit criteria:** The system stays within configured storage limits under sustained crawling and querying, without unacceptable retrieval-quality loss.

## Phase 8 — Evaluate and Optimize

**Objective:** Establish whether the new approach is more efficient than the existing search engine.

Create a representative query benchmark covering:

- Technical documentation.
- Research papers.
- Software and API references.
- News and changing information.
- Obscure topics.
- Exact identifiers.
- Multi-source comparison.
- Questions requiring deeper site navigation.

Compare:

1. Existing full-document retrieval.
2. Source-first retrieval.
3. Source-first retrieval with global fallback.
4. Source-first retrieval with on-demand exploration.
5. Optional hybrid semantic retrieval.

Measure source recall, page recall, evidence completeness, citation accuracy, response latency, number of network requests, AI token usage, and persistent storage.

Optimize components only after measurements identify their actual limitations.

**Exit criteria:** The lightweight architecture demonstrates useful retrieval quality and significant resource savings against the existing baseline, without depending on unverifiable assumptions.

---

## 11. Suggested Query Modes

Expose three basic research modes.

| Mode     | Behavior                                                                                         | Use case             |
| -------- | ------------------------------------------------------------------------------------------------ | -------------------- |
| Fast     | Search local source/URL knowledge and cached evidence                                            | Simple agent lookups |
| Standard | Source discovery plus limited live fetching and related-page exploration                         | Normal research      |
| Deep     | Multi-source expansion, additional document retrieval, coverage checks, and repeated exploration | Complex research     |

Each mode should have configurable budgets for time, fetched pages, network bytes, and returned evidence.

Budget exhaustion should produce a useful partial result rather than a misleading claim of complete coverage.

The same engine should support both completely local lookups and fresh online research.

---

## 12. Evaluation and Success Criteria

The following targets are initial development goals and must be validated experimentally.

### Functional success

- Relevant sources can be discovered for representative queries.
- Known source structures can be explored efficiently.
- Sitemaps can produce URL candidates without downloading all listed pages.
- Related-page traversal works.
- AI agents receive structured evidence with usable provenance.
- Unavailable and insufficient evidence is reported explicitly.

### Storage success

- Most known URLs require metadata storage rather than permanent full-text storage.
- Raw content is evicted safely.
- Duplicate resources do not cause unnecessary repeated indexing.
- Total storage growth is bounded by configurable policy.
- The compact index provides a measurable reduction against the existing full-content corpus.

### Retrieval success

- Source and page recall remain competitive with the current full-text baseline.
- On-demand exploration improves evidence coverage for complex queries.
- Retrieval cost decreases for repeated queries.
- Cold-start discovery does not depend exclusively on already known domains.

### Reliability success

- Index publication remains safe across restarts.
- Failed downloads do not corrupt source records.
- Cache failures do not cause otherwise successful retrievals to fail.
- Search remains available during controlled background ingestion.
- Crawling respects resource limits and applicable site access policies.

### Open-source success

- The default setup runs on ordinary CPU-based hardware.
- Dependencies are documented.
- Proprietary services remain optional.
- Local deployment and reproducible tests are supported.
- APIs and storage schemas have documented compatibility rules.

---

## 13. Architecture and Development Constraints

### Preserve existing working functionality

The redesign must not destroy the current crawler, index, search API, or stored corpus.

New subsystems should initially operate alongside existing ones.

### Migrate incrementally

Use adapters and compatibility layers rather than rewriting all crates at once.

Each phase must produce a working and testable improvement.

### Prevent unnecessary complexity

Do not introduce distributed coordination, model-driven orchestration, or additional storage systems unless simpler alternatives fail measured requirements.

### Keep the engine modular

The source directory, URL catalog, retrieval engine, crawler, and agent API must remain independently testable.

### Maintain clear data identities

Domain, source, resource, work, and content identities must remain consistent across shards and storage tiers.

### Avoid expensive mandatory model inference

The core engine should function without hosted LLM calls.

Optional models may help with source classification, query decomposition, reranking, and evidence evaluation.

### Preserve security boundaries

Fetched web pages, document content, and model-produced navigation suggestions are untrusted.

All live fetching must enforce network restrictions, robots policy, timeouts, and resource limits.

---

## 14. Immediate Development Backlog

The first development cycle should concentrate on the following work, in order:

1. Recover disk capacity and preserve all existing data.
2. Fix the answer-cache error propagation problem.
3. Reconcile and validate the current Rust repository state.
4. Audit the four fleet shards and their resource-processing states.
5. Complete recoverable offline indexing jobs.
6. Expose the four existing published indexes through the main search API.
7. Build a compact source and section catalog from the fleet.
8. Build the lightweight URL catalog.
9. Implement source-first searching with global fallback.
10. Implement site mapping and targeted content retrieval.
11. Expose the first structured agent operations.
12. Benchmark the new approach against the existing document index.

Do not begin large-scale recrawling until storage safeguards, indexing recovery, and published-data access are operational.

Do not delete the old document index until the new retrieval architecture has demonstrated acceptable coverage and correctness.

---

## 15. Definition of the First Successful Release

The first AI-native release is complete when an external AI agent can:

1. Submit a topic or research question.
2. Discover a useful set of information sources.
3. Inspect the structure of a selected source.
4. Find promising pages through metadata, sitemaps, and links.
5. Retrieve selected pages on demand.
6. Follow additional references when necessary.
7. Receive structured, attributed evidence.
8. Continue researching without repeating unnecessary discovery.
9. Operate without mandatory proprietary search APIs or hosted LLMs.
10. Run within a configurable storage, CPU, network, and time budget.

The release must include a benchmark demonstrating its behavior on real research tasks.

A successful first release does not require millions of permanently indexed full-text documents.

It requires reliable discovery, navigation, retrieval, and evidence delivery.

---

## 16. Long-Term Direction

Once the source-first retrieval engine is stable, future development may include:

- Source-level semantic embeddings.
- Better cross-site citation graphs.
- Adaptive selection of navigation strategies.
- Distributed crawling and indexing.
- Community-contributed source directories.
- Importable source packs.
- Improved scholarly and software repository adapters.
- Multilingual discovery.
- Lightweight learned source ranking.
- Specialized navigation for documentation, academic repositories, and forums.
- Retrieval-quality learning from verified task outcomes.

These features must remain secondary to the project's central purpose.

## Final Design Principle

Hexxica should not attempt to become a small copy of a conventional web search engine.

It should become a compact map of the web that enables AI systems to find the right places, explore them intelligently, and retrieve the information required for reasoning.

**The engine discovers and navigates. The AI analyzes and reasons. Persistent storage supports efficient discovery, rather than attempting to contain the web itself.**

The most important architectural decision is to retain the existing Tantivy document index as a fallback while introducing the compact source and URL indexes. That makes the migration reversible and lets you measure whether the new design genuinely achieves better efficiency.

I would treat Phase 3 (Source Directory), Phase 4 (URL Catalog), and Phase 5 (Query-Driven Navigation) as the heart of the redesign. The earlier phases recover and connect what you already built; these three phases transform it into the system you actually want.