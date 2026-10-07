# Universal Crawling & Scraping Architecture

## 1. Design Goal

The crawler must support:

```text
normal websites
news/blogs
documentation
forums
Git repositories
research papers
PDFs
books
library catalogs
archives
RSS / Atom
XML sitemaps
OAI-PMH repositories
JSON/XML APIs
JavaScript applications
images
datasets/files
historical archives
```

without building a unique crawler from scratch for every source.

The central design is:

```text
                     SEEDS
                       │
                       ▼
                SOURCE DISCOVERY
                       │
                       ▼
                GLOBAL FRONTIER
                       │
                       ▼
               POLICY + SCHEDULER
                       │
                       ▼
                    FETCHER
                       │
                 RESPONSE ROUTER
          ┌────────────┼──────────────┐
          ▼            ▼              ▼
        HTML          PDF         STRUCTURED
          │            │              │
          ▼            ▼              ▼
       parser        document       XML/JSON/
                    processor      feeds/etc.
          │            │              │
          └────────────┼──────────────┘
                       ▼
                 NORMALIZED OBJECT
                       │
             ┌─────────┼─────────┐
             ▼         ▼         ▼
           INDEX      GRAPH     LINKS
                                   │
                                   ▼
                              NEW FRONTIER
```

The crawler continuously loops.

---

# 2. Core Objects

The entire crawler operates around five IDs.

```text
DomainID
SourceID
ResourceID
WorkID
ContentID
```

### Domain

A hostname:

```text
arxiv.org
docs.python.org
github.com
example.com
```

### Source

A meaningful section or logical collection:

```text
arxiv.org / cs.AI
docs.python.org / asyncio
github.com / llama.cpp
example.com / news
```

### Resource

A fetchable address:

```text
URL
feed URL
sitemap URL
PDF URL
API endpoint
repository object
```

### Work

A logical intellectual object that may exist at multiple URLs:

```text
research paper
book
article
software release
```

### Content

The unique material itself.

Several URLs may point at one ContentID.

```text
URL A ─┐
URL B ─┼── ContentID 91828
URL C ─┘
```

This is important for deduplication.

---

# 3. Source Registry

Before crawling URLs, maintain a database about websites.

```text
SourceProfile {
    source_id
    domain_id

    root_url
    type

    topics[]
    entities[]

    robots_url
    sitemap_urls[]
    feed_urls[]

    known_path_patterns[]

    discovery_methods[]

    authority
    quality

    crawl_priority

    expected_change_rate

    requires_js_probability

    successful_fetch_rate

    average_response_size
    average_latency

    last_crawled
    next_discovery_at
}
```

Examples of `type`:

```text
documentation
news
blog
forum
academic_repository
publisher
software_repository
library
archive
government
company
dataset
market_data
```

The registry is effectively the crawler's memory.

---

# 4. Seed Layer

The crawler can accept many kinds of seeds.

```text
SEED
├ domain
├ URL
├ sitemap
├ RSS/Atom feed
├ DOI
├ arXiv ID
├ Git repository
├ OAI-PMH endpoint
├ ISBN
├ archive identifier
├ imported dataset
└ community source pack
```

Seeds can come from:

```text
manual lists
Common Crawl
Wikidata
OpenAlex
Crossref
Open Library
existing source packs
links discovered by crawling
agent searches
user submissions
```

Common Crawl's URL Index is particularly useful for large-scale bootstrap discovery because it exposes its crawl URL index in Parquet and can be queried locally with systems such as DuckDB, Spark, Arrow, or Polars.

---

# 5. Domain Discovery

A new domain first enters:

```text
DOMAIN_DISCOVERED
```

not immediately:

```text
CRAWL_EVERYTHING
```

Run cheap checks first.

```text
DNS valid?
        │
        ▼
HTTPS reachable?
        │
        ▼
robots.txt
        │
        ▼
homepage headers
        │
        ▼
site type prediction
        │
        ▼
sitemap/feed discovery
        │
        ▼
quality score
```

Possible domain states:

```text
UNKNOWN
PROBING
ACTIVE
LOW_VALUE
BLOCKED
DEAD
TEMP_FAILURE
BLACKLISTED
```

---

# 6. robots.txt Layer

Every origin has a cached `RobotsPolicy`.

RFC 9309 standardizes `robots.txt`, including user-agent groups, allow/disallow rules, fetching and caching behavior.

```text
RobotsPolicy {
    origin
    fetched_at
    expires_at

    rules[]
    sitemaps[]

    status
}
```

Before fetching:

```text
ALLOW(resource)?
```

If false:

```text
DROP / LINK_ONLY
```

Don't make every worker download robots independently.

There should be one centralized policy cache.

---

# 7. Sitemap Discovery

Try known locations:

```text
robots.txt declarations

/sitemap.xml
/sitemap_index.xml
/sitemap.xml.gz
/sitemap-index.xml
```

Then follow sitemap indexes.

A standard sitemap can contain up to 50,000 URLs or 50 MB uncompressed; larger collections can be represented using sitemap indexes.

Create records without necessarily fetching each page:

```text
SitemapURLRecord {
    url
    lastmod
    sitemap_id
    source_id
    discovered_at
    fetched = false
}
```

This means:

```text
1 sitemap fetch
→ 40,000 known URLs
```

without:

```text
40,000 page downloads
```

---

# 8. Feed Discovery

Detect:

```text
RSS
Atom
JSON Feed
```

through:

```text
<link rel="alternate">
known paths
source configuration
HTML links
```

Feeds belong to the **freshness subsystem**.

Google's own sitemap guidance notes that RSS and Atom feeds are useful for exposing recent URLs, whereas full sitemaps are better suited for broader site coverage.

The crawler therefore treats:

```text
SITEMAP
→ coverage

FEED
→ freshness
```

differently.

---

# 9. Frontier

Every candidate resource enters a central distributed priority queue.

```text
FrontierEntry {
    resource_id
    url

    domain_id
    source_id

    resource_type_guess

    discovered_from
    discovery_method

    priority

    earliest_fetch_at

    retry_count

    expected_cost
    expected_value

    status
}
```

Possible statuses:

```text
NEW
QUEUED
FETCHING
FETCHED
UNCHANGED
FAILED
RETRY
SKIPPED
BLOCKED
```

---

# 10. Frontier Priority

Do not use FIFO.

Calculate approximately:

```text
priority =

source_quality
× expected_information_value
× freshness_need
× query_demand
× discovery_confidence
× uniqueness_probability

÷ expected_fetch_cost
```

Then add modifiers for:

```text
important entity
important topic
new source
recent feed entry
sitemap lastmod
historical success
document rarity
```

And penalties for:

```text
duplicate patterns
faceted URLs
infinite calendars
search pages
session URLs
tracking URLs
very large resources
repeated failures
```

---

# 11. Per-Origin Queues

The global queue should not directly dispatch URLs.

Use:

```text
GLOBAL SCHEDULER

example.com queue
python.org queue
arxiv.org queue
site-a queue
site-b queue
...
```

Each origin has:

```text
OriginState {
    concurrency_limit

    next_allowed_time

    observed_latency

    error_rate

    rate_limit_state

    robots_policy

    adaptive_delay
}
```

This lets the crawler use enormous aggregate bandwidth while remaining gentle to individual websites.

---

# 12. Adaptive Politeness

Start conservatively.

For an unknown site:

```text
1 request concurrently
```

Observe:

```text
latency
429
503
timeouts
Retry-After
server errors
```

Healthy:

```text
possibly increase slowly
```

Errors:

```text
decrease immediately
```

For example:

```text
site A    1 concurrent
site B    2 concurrent
site C    1 concurrent
GitHub    API-specific limits
```

At high scale, concurrency comes from crawling **many hosts**, not hammering one host.

---

# 13. URL Canonicalization

Before placing a URL on the frontier:

```text
normalize scheme
normalize hostname
remove fragments
normalize default ports
resolve relative URLs
normalize safe path segments
remove known tracking parameters
sort safe query arguments
```

Examples removed:

```text
utm_source
utm_medium
utm_campaign
fbclid
gclid
ref
```

when semantics allow it.

Maintain both:

```text
original_url
canonical_url
```

---

# 14. URL Pattern Engine

Group URLs into templates.

Example:

```text
/blog/{slug}
/article/{id}
/docs/{version}/{page}
/users/{name}
/tag/{tag}?page={n}
```

Create:

```text
URLPattern {
    source_id
    template
    document_type
    value_score
    crawl_probability
}
```

After enough evidence:

```text
/article/{slug}
→ FETCH

/tag/{tag}?page=N
→ DISCOVERY_ONLY

/login
→ IGNORE

/share
→ IGNORE
```

This prevents crawl traps.

---

# 15. Seen-Resource Layer

Every discovered normalized URL is hashed.

```text
url_hash = HASH(canonical_url)
```

Check:

```text
Bloom / XOR filter
        ↓ maybe present
persistent URL index
```

Only unseen or refresh-due URLs reach the frontier.

The in-memory approximate filter avoids expensive database lookups for obvious duplicates.

---

# 16. Fetcher Architecture

Use several fetch classes.

```text
Fetcher
├ HTTPFetcher
├ BrowserFetcher
├ FileFetcher
├ StructuredProtocolFetcher
├ RepositoryFetcher
└ ArchiveFetcher
```

The dispatcher chooses based on resource metadata.

---

# 17. HTTP Fetcher

This handles most resources.

Requirements:

```text
HTTP/1.1
HTTP/2
connection pooling
compression
redirects
timeouts
range requests
conditional requests
content limits
DNS caching
TLS reuse
```

Keep persistent clients.

Do not establish a fresh TLS connection for every page.

---

# 18. Conditional Refresh

Store:

```text
ETag
Last-Modified
```

On refresh:

```text
If-None-Match
If-Modified-Since
```

HTTP conditional requests let the crawler validate cached resources without retransmitting unchanged content.

Response:

```text
304
→ unchanged
→ update timestamp
→ no parsing/indexing required
```

This becomes extremely important for hot sources.

---

# 19. Response Router

After receiving headers, route by MIME type before reading/processing unnecessarily.

```text
text/html
→ HTML

application/pdf
→ PDF

application/json
→ JSON

application/xml
text/xml
→ XML

application/rss+xml
→ FEED

application/atom+xml
→ FEED

image/*
→ IMAGE

text/plain
→ TEXT

application/zip
→ ARCHIVE policy

other
→ FILE policy
```

Large unknown files should be rejected or range-probed before full download.

---

# 20. HTML Route

Normal path:

```text
HTTP
 ↓
HTML
 ↓
metadata/layout analysis
 ↓
normalized document
 ↓
links
 ↓
new candidates
```

If meaningful content appears in static HTML:

```text
DONE
```

If the response is clearly an empty JavaScript shell:

```text
SEND_TO_BROWSER_QUEUE
```

---

# 21. Browser Queue

Browser rendering is an escalation path, not the default.

```text
STATIC_FETCH
 ↓
insufficient?
 ↓ yes
BROWSER_QUEUE
```

Run:

```text
persistent Chromium
```

with short-lived isolated browser contexts.

Playwright BrowserContexts are isolated sessions and can be created within one running browser without requiring one browser process per page.

Set limits:

```text
max browser time
max network requests
block ads/analytics
block fonts where safe
block video
optional block images
max DOM size
```

Once DOM/data is obtained:

```text
close context
```

Never keep browser state unless a source adapter specifically requires it.

---

# 22. Network Interception for JS Sites

Sometimes the HTML is useless but the page itself calls:

```text
/api/articles
/graphql
/api/search
```

During browser rendering, observe network responses.

If a clean data endpoint is discovered:

```text
Browser page
     ↓ discovers
JSON endpoint
     ↓
register endpoint pattern
```

Future fetches can potentially bypass rendering:

```text
HTTP → JSON
```

This is an important optimization.

---

# 23. PDFs

A PDF resource goes through:

```text
PDF discovered
 ↓
size check
 ↓
hash/check duplicate
 ↓
text-layer test
 ↓
document type classifier
```

For scientific documents:

```text
GROBID
```

GROBID is explicitly designed to transform scholarly PDFs into structured TEI and extracts bibliographic information including title, abstract, authors, affiliations, keywords, references, and document structure.

Generic PDFs can use:

```text
PDF text parser
```

Scanned PDFs:

```text
OCR queue
```

but OCR only when document value warrants it.

---

# 24. Research Paper Pipeline

Discovery methods:

```text
DOI
arXiv ID
OAI-PMH
sitemap
citation
repository
OpenAlex
links
Common Crawl
```

Identity:

```text
DOI
arXiv
PMID/PMCID
repository IDs
title+author fallback
```

A paper can have several locations:

```text
publisher
preprint
repository
author copy
```

OpenAlex uses essentially this location→work model and resolves many copies into one scholarly work.

Our crawler should do:

```text
LOCATION
  ↓
WORK RESOLUTION
  ↓
WorkID
```

not index every copy as a separate paper.

---

# 25. OAI-PMH Repositories

Support OAI-PMH as a first-class adapter.

OAI-PMH is specifically designed for metadata harvesting between repositories and service providers and exposes six standardized HTTP operations.

Adapter:

```text
Identify
 ↓
ListMetadataFormats
 ↓
ListSets
 ↓
ListRecords
 ↓
follow resumptionToken
```

Store checkpoints:

```text
repository
metadataPrefix
set
last_datestamp
resumption_token
```

This allows incremental scholarly/library harvesting.

---

# 26. Books and Libraries

Book discovery:

```text
ISBN
library dumps
OAI
catalog pages
Open Library IDs
archive IDs
publisher catalogs
```

Use:

```text
Work
  ├ editions
  ├ authors
  └ locations
```

instead of treating every edition as unrelated.

Open Library publishes monthly works, editions and authors dumps specifically for bulk processing; current public dumps are multi-gigabyte but manageable for builder infrastructure.

Crawler behavior:

```text
catalog metadata
→ index

public/open full text
→ optionally process

restricted full text
→ location only
```

---

# 27. Archives

Archives need a two-level architecture.

```text
ARCHIVE ITEM
     │
     ├ metadata
     └ files[]
```

Do not immediately fetch all files.

Create resource children:

```text
item
├ PDF
├ EPUB
├ TXT
├ images
└ metadata XML
```

Prioritize preferred representations:

```text
structured text
> EPUB
> searchable PDF
> scanned PDF
> images
```

depending on the task.

---

# 28. Git and Software Repositories

Treat repositories specially.

Logical resources:

```text
repository
README
releases
tags
issues
discussions
pull requests
documentation
package metadata
```

Do not clone all Git history during ordinary crawling.

Use repository pages/APIs/dumps where allowed.

GitHub's REST infrastructure has explicit rate limits, including separate treatment for search operations, so adapter scheduling must understand provider-specific quotas instead of treating it like an ordinary website.

For known repositories:

```text
repo activity observed
 ↓
release/tag changed?
 ↓
fetch new relevant objects
```

---

# 29. Forums

Forum crawler detects structures:

```text
forum
  ↓
category
  ↓
thread
  ↓
pages/replies
```

Frontier policy differs from news:

```text
new/high-quality thread
→ high

page 87 of old low-value thread
→ low
```

Watch:

```text
thread updated timestamp
reply count
last post date
```

so active technical discussions are refreshed while dead threads remain cold.

---

# 30. News and Blogs

Discovery priority:

```text
RSS/Atom
 ↓
news sitemap
 ↓
normal sitemap
 ↓
section pages
 ↓
internal links
```

Hot news sources should be feed-driven rather than continuously recrawling entire sites.

Store a `publication cadence` model:

```text
articles/hour
articles/day
usual publishing hours
feed delay
sitemap delay
```

Use it to estimate the next check.

---

# 31. Documentation Sites

Docs sites usually contain:

```text
versioned trees
navigation hierarchy
sitemaps
internal TOCs
```

Crawler learns:

```text
/docs/v1/
/docs/v2/
/docs/latest/
```

and creates source clusters by version.

Avoid indexing duplicate:

```text
/latest/
```

and:

```text
/v3.5/
```

as independent content when they are identical.

Canonical/version relationships should be recorded.

---

# 32. APIs / JSON / XML

If a public data endpoint is discovered and permitted:

```text
endpoint
 ↓
schema fingerprint
 ↓
pagination strategy
 ↓
incremental strategy
```

Possible pagination:

```text
page=
offset=
cursor=
next URL
continuation token
```

Store checkpoint state.

The generic crawler should know:

```text
collection endpoint
```

versus:

```text
single-object endpoint
```

to prevent infinite enumeration.

---

# 33. S3/Object Storage/Public Datasets

If a source exposes directory manifests or index files:

```text
manifest
 ↓
objects
```

do not list/download blindly.

Create lightweight resource records first.

Use:

```text
size
last modified
content type
checksum
```

to decide what is useful.

Large dataset files can remain pointers unless a specialized importer exists.

---

# 34. Images

Images are normally secondary objects.

Store:

```text
URL
mime
dimensions
alt text
caption
parent document
hash
```

Do not download every full-resolution image.

Fetch image bytes when:

```text
image search requested
vision analysis requested
diagram/chart important
OCR required
```

Use thumbnails or image hashing where useful.

---

# 35. Video/Audio

MVP should mostly store:

```text
metadata
title
description
captions/transcripts if legitimately available
duration
source
```

not giant media files.

Full media downloading is outside the normal search-crawl path.

---

# 36. Canonical Content Deduplication

After processing:

```text
exact_hash
```

checks exact duplicates.

Then:

```text
SimHash / MinHash / shingles
```

detect near duplicates.

Example:

```text
publisher article
syndicated article
printer view
AMP page
tracking URL
```

may collapse into:

```text
ContentID 99127
```

while retaining all source URLs.

---

# 37. Link Extraction

Links are not merely crawl candidates.

Classify them.

```text
INTERNAL_NAVIGATION
RELATED_CONTENT
CITATION
DOWNLOAD
AUTHOR
CANONICAL
NEXT_PAGE
PREVIOUS_PAGE
TAG
CATEGORY
EXTERNAL_REFERENCE
SOCIAL
ADVERTISEMENT
LOGIN
```

Only useful link types enter the frontier.

Citation edges enter the knowledge graph even when their URL is already known.

---

# 38. Discovery Feedback

Every successful fetch can produce:

```text
new domains
new sources
new feeds
new sitemaps
new works
new entities
new URL patterns
```

Therefore:

```text
FETCH
 ↓
DISCOVER
 ↓
FRONTIER
 ↓
FETCH
```

is self-expanding.

---

# 39. Freshness Scheduler

Every SourceID learns a change model.

```text
FreshnessModel {
    mean_change_interval
    variance

    recent_change_rate

    feed_available
    sitemap_lastmod_quality

    conditional_request_support

    demand_score

    importance
}
```

Then estimate:

```text
next_check =
f(change_rate,
  importance,
  demand,
  freshness_requirements)
```

Examples:

```text
breaking news       minutes
crypto news         minutes
GitHub releases     hours
API pricing         hours/days
documentation       days
old research PDF    months
book metadata       months
```

`Last-Modified` and ETags allow inexpensive validation of known resources.

---

# 40. Query-Driven Freshness

The background crawler cannot make everything fresh.

Search queries can create crawl jobs.

Example:

```text
query:
"Ethereum news today"

 ↓

search engine activates:
crypto / Ethereum source cluster

 ↓

FreshnessScheduler sees:
sources stale for query requirement

 ↓

URGENT_REFRESH jobs
```

These jobs outrank background crawling.

So the scheduler has queues:

```text
P0 QUERY CRITICAL
P1 HOT UPDATES
P2 NORMAL REFRESH
P3 DISCOVERY
P4 ARCHIVAL/BACKFILL
```

---

# 41. Crawler Budget Model

Every fetch carries an estimated cost:

```text
network bytes
CPU parse time
browser cost
OCR cost
storage cost
```

and value:

```text
query demand
source quality
freshness
novelty
graph importance
```

Calculate:

```text
crawl_value =
expected_value / expected_cost
```

This prevents:

```text
100 MB PDF with low value
```

from outranking:

```text
30 KB high-value release page
```

without good reason.

---

# 42. Failure Handling

Classify errors.

```text
DNS
connection
TLS
timeout
429
403
404
410
500
503
parsing
browser
oversized
unsupported
robots
```

Each has different behavior.

Example:

```text
404
→ don't retry soon

410
→ mark deleted

429
→ obey Retry-After / slow origin

500
→ exponential retry

timeout
→ limited retries

403
→ don't continuously hammer

robots denied
→ blocked
```

---

# 43. Retry Policy

Use exponential backoff with jitter.

Conceptually:

```text
retry =
base × 2^attempt + jitter
```

but respect:

```text
Retry-After
```

when present.

Put retry scheduling into the frontier, not sleeping worker threads.

---

# 44. Deletion Detection

Resources disappear.

If previously indexed URL now gives:

```text
404
410
```

record:

```text
deleted_at
```

Do not immediately erase search history.

Mark:

```text
ACTIVE
STALE
DELETED
ARCHIVED
```

Historical results may still matter.

---

# 45. Security Boundary

Fetched web content is untrusted.

Crawler workers must not have unrestricted network access.

Block:

```text
127.0.0.0/8
private RFC1918 networks
link local
cloud metadata IPs
Unix sockets
file://
internal service names
```

unless explicitly required.

This prevents SSRF.

---

# 46. Resource Limits

Every job has:

```text
max_response_bytes
max_decompressed_bytes
max_redirects
max_parse_time
max_browser_time
max_dom_nodes
max_pdf_pages
max_archive_expansion
```

Never trust remote `Content-Length` alone.

---

# 47. Archive Bomb Protection

For:

```text
ZIP
gzip
tar
```

check:

```text
compressed size
declared expanded size
file count
compression ratio
nested depth
```

before expanding.

Large archives go to specialized dataset workers, not generic extraction.

---

# 48. Storage Tiers

### Tier 0 — Discovery

```text
URL
source
type guess
priority
lastmod
```

### Tier 1 — Indexed metadata

```text
title
description/abstract
date
entities
topics
links
```

### Tier 2 — Extracted content

```text
normalized content blocks
```

### Tier 3 — Raw cache

```text
HTML
PDF
JSON
etc.
```

Raw cache should have short TTL unless particularly valuable.

---

# 49. Cache Types

Maintain:

```text
DNS cache
robots cache
HTTP body cache
parsed document cache
browser result cache
sitemap cache
feed cache
entity/source cache
```

Each has its own TTL.

---

# 50. Distributed Architecture

Even if the MVP runs on one machine, design job ownership for later distribution.

```text
                    COORDINATOR
                         │
             ┌───────────┼───────────┐
             ▼           ▼           ▼
         scheduler    registry     frontier
                         │
      ┌──────────────────┼──────────────────┐
      ▼                  ▼                  ▼
 HTTP workers        PDF workers       Browser workers
      │                  │                  │
      └──────────────────┼──────────────────┘
                         ▼
                    result queue
                         │
                         ▼
                      indexer
```

Workers should be stateless where possible.

---

# 51. MVP Single-Server Layout

On the current 16-vCPU / 64-GB server:

```text
crawler-api
scheduler
frontier-db
source-registry

HTTP workers          32–64 async tasks
parser workers        4–8
PDF workers           2–4
browser contexts      2
index workers         2–4
```

Those are starting values, not fixed production values.

The important distinction is:

```text
HTTP concurrency
≠
CPU worker count
```

Async fetches can greatly outnumber CPUs.

---

# 52. Database Responsibilities

Use separate logical stores.

```text
Metadata DB
───────────
domains
sources
resources
crawl state
freshness

Frontier
────────
priority jobs

Search index
────────────
documents
titles
abstracts

Graph
─────
links
entities
concepts
works

Blob cache
──────────
temporary/raw content
```

Avoid forcing all workloads through one database.

---

# 53. Event Architecture

Every component communicates through events.

```text
DOMAIN_DISCOVERED

RESOURCE_DISCOVERED

RESOURCE_FETCHED

RESOURCE_UNCHANGED

RESOURCE_FAILED

DOCUMENT_PARSED

WORK_RESOLVED

SOURCE_DISCOVERED

LINK_DISCOVERED

CONTENT_CHANGED

RESOURCE_DELETED
```

For example:

```text
RESOURCE_FETCHED
    │
    ├→ parser
    ├→ cache metrics
    └→ freshness model
```

This makes later distributed scaling much easier.

---

# 54. Specialized Adapters

The core remains generic, but adapters improve high-value sources.

```text
GenericWebAdapter
SitemapAdapter
FeedAdapter
OAIAdapter
ArxivAdapter
GitHubAdapter
OpenLibraryAdapter
ArchiveAdapter
ScholarlyPDFAdapter
JavaScriptSiteAdapter
```

An adapter does **not** replace the crawler.

It supplies things like:

```text
discovery rules
identity rules
pagination
freshness hints
preferred representations
```

---

# 55. Adapter Interface

Conceptually:

```text
SourceAdapter {
    matches(url/source)

    discover(source)

    canonicalize(resource)

    fetch_strategy(resource)

    identify(resource)

    child_resources(document)

    refresh_strategy(source)
}
```

If no adapter matches:

```text
GenericWebAdapter
```

handles it.

---

# 56. Source Learning

The crawler should gradually learn source behavior.

For every source record:

```text
static_success_rate
browser_success_rate

useful_document_ratio

duplicate_ratio

average_document_value

changes_per_day

feed_delay

sitemap_accuracy

query_success_rate
```

Example:

```text
Site A:
95% static HTML success
→ stop trying browser

Site B:
90% empty HTML but browser succeeds
→ route straight to browser

Site C:
RSS finds updates faster than sitemap
→ prioritize RSS
```

This removes wasted work.

---

# 57. URL Pattern Learning

Likewise:

```text
/docs/*
→ useful 98%

/tag/*
→ useful 12%

/author/*
→ useful 10%

/article/*
→ useful 95%

/calendar/*
→ crawl trap
```

The frontier automatically learns priorities by template.

---

# 58. Query-Demand Learning

If users frequently search:

```text
crypto
LLM releases
PyTorch
CUDA
```

increase:

```text
source crawl budget
refresh frequency
content cache duration
```

for related sources.

Therefore crawling evolves according to actual agent demand.

---

# 59. Bootstrap Strategy

The crawler should start in stages.

```text
PHASE 1
manual/high-quality seeds

PHASE 2
sitemaps + feeds

PHASE 3
outgoing links

PHASE 4
open dataset imports

PHASE 5
Common Crawl discovery

PHASE 6
query-driven expansion
```

Do not immediately dump millions of random domains into the frontier.

---

# 60. Data Lifecycle

A resource progresses through:

```text
DISCOVERED
    ↓
SCORED
    ↓
QUEUED
    ↓
FETCHED
    ↓
CLASSIFIED
    ↓
INDEXED
    ↓
MONITORED
    ↓
REFRESHED
    ↓
STALE / DELETED
```

Or:

```text
DISCOVERED
    ↓
LOW VALUE
    ↓
METADATA ONLY
```

Not everything must become a fetched/indexed document.

---

# 61. Complete Crawl Algorithm

```text
while running:

    process discovery events

    update frontier priorities

    for each available origin:

        if origin.next_allowed > now:
            continue

        job = frontier.best_eligible(origin)

        if no job:
            continue

        if !robots.allow(job.url):
            mark_blocked(job)
            continue

        dispatch(job)

    for each fetch result:

        update_origin_statistics()

        if unchanged:
            schedule_next_refresh()
            continue

        if failed:
            apply_failure_policy()
            continue

        route_by_content_type()

        normalize_document()

        resolve_identity()

        deduplicate()

        send_to_index()

        update_graph()

        discover_links()

        discover_feeds()

        discover_sitemaps()

        add_new_resources_to_frontier()

        update_source_statistics()

        calculate_next_refresh()
```

---

# 62. Full Architecture

```text
                             SEEDS
                               │
     ┌─────────────────────────┼──────────────────────────┐
     ▼                         ▼                          ▼
 OPEN DATASETS              LINKS                   USER/AGENT
     │                         │                          │
     └─────────────────────────┼──────────────────────────┘
                               ▼
                       DISCOVERY ENGINE
                               │
                               ▼
                         SOURCE REGISTRY
                               │
                               ▼
                       RESOURCE FRONTIER
                               │
                    ┌──────────┴──────────┐
                    │                     │
                  robots              scheduler
                    │                     │
                    └──────────┬──────────┘
                               ▼
                          FETCH ROUTER
                               │
       ┌───────────────┬───────┼────────┬───────────────┐
       ▼               ▼       ▼        ▼               ▼
      HTTP           Browser   PDF    Structured      Archive
       │               │       │        │               │
       └───────────────┴───────┼────────┴───────────────┘
                               ▼
                        CONTENT ROUTER
                               │
                               ▼
                       NORMALIZED OBJECT
                               │
              ┌────────────────┼───────────────────┐
              ▼                ▼                   ▼
            INDEX             GRAPH            CACHE/BLOBS
              │                │
              │                ▼
              │           NEW DISCOVERY
              │                │
              │                └──────► FRONTIER
              │
              ▼
                         SEARCH ENGINE
                              │
                              ▼
                          USER QUERY
                              │
                     freshness needed?
                        /           \
                      no             yes
                      │               │
                      │        PRIORITY CRAWL JOB
                      │               │
                      └───────┬───────┘
                              ▼
                           RESULTS
```

---

# 63. Guiding Rule

The crawler should not ask:

> **“Can I download this?”**

It should ask:

> **“How much will knowing or refreshing this resource improve search?”**

That distinction determines whether the project becomes a useful search engine or merely a huge web archive.

For ordinary pages we may store the extracted content. For papers we may store title + abstract + locations. For books we may keep metadata. For a dynamic price page we may refresh frequently. For an old PDF we may only keep the URL. For a JavaScript dashboard we may discover the underlying data endpoint and stop using Chromium.

The crawler therefore becomes a **resource-allocation engine**, not simply a downloader.
