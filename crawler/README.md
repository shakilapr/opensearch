# Where the crawler is implemented

This directory holds the **specification** (`ARCHITECTURE.md` = `UNIVERSAL_CRAWLER_ARCHITECTURE.md`)
and the seed lists. The implementation lives in `../crates`, driven by the `agent0` binary. This
file maps each spec section to code so the spec can be audited, and states what is missing.

## Section → code

| Spec | Implementation |
| --- | --- |
| §2 Core objects (Domain, Source, Resource, Work, Content) | `crates/core/src/ids.rs` (the 5-id model), `crates/metadata` (rows) |
| §3 Source registry, §4 Seed layer, §5 Domain discovery | `crates/discovery`, `crawler/seeds/*.txt` |
| §6 robots.txt | `crates/discovery` (parser + cache), checked in `crates/agent0/src/crawl.rs` |
| §7 Sitemap discovery, §8 Feed discovery | `crates/discovery` (sitemap index walking, RSS/Atom) |
| §9–§11 Frontier, priority, per-origin queues | `crates/frontier`, queueing and the seed-by-seed drain in `crates/agent0/src/crawl.rs` |
| §12 Adaptive politeness | per-origin interval and `origin_cap()` in `crates/agent0/src/crawl.rs` (`SEARCH_CRAWL_ORIGIN_INTERVAL_MS`, `--origin-cap`) |
| §13 URL canonicalization | `crates/fetcher/src/canon.rs` (`canonicalize`), the identity source for `ResourceId` |
| §14 URL pattern engine | `crates/patterns` (partial) |
| §15 Seen-resource layer | the crawl `seen` set persisted via `crawl-log.jsonl` + `frontier.json` |
| §16–§17 Fetchers, HTTP | `crates/fetcher` (SSRF guard, timeouts, limits) |
| §18 Conditional refresh | `crates/frontier` validators + `MemoryValidators` in `crates/agent0/src/pipeline.rs` |
| §19 Response router | `crates/router` (`search_router::classify`) |
| §20 HTML route | `crates/parser` (normalisation) + `crates/agent0/src/pipeline.rs` |
| §21–§22 Browser queue, network interception | `crates/browser` (persistent Chromium, per-fetch context; `SEARCH_BROWSER=1`, `--features chromium`) |
| §23 PDFs | `crates/pdf` |
| §36 Content deduplication | `crates/dedup`, `crates/ingest` |
| §37 Link extraction | `article_links` in `crates/agent0/src/crawl.rs` (`MAX_LINKS_PER_PAGE`) |
| §39–§40 Freshness scheduler, query-driven freshness | `crates/scheduler`, `Pipeline::run_refresh_tick`, `due_sources` |
| §41 Crawler budget model, §42–§43 failure handling, retry | attempt/origin caps and the retry policy in `crates/agent0/src/crawl.rs`, `crates/fetcher` |
| §44 Deletion detection | `Pipeline::mark_deleted` + the 404/410 path in `crates/agent0/src/crawl.rs` (mark DELETED, keep the bytes) |
| §45–§47 Security boundary, resource limits, archive-bomb protection | `crates/fetcher` (SSRF), `crates/core/src/config.rs` limits, response-size caps |
| §48–§49 Storage tiers, cache types | `crates/tiers`, `crates/cache`, `crates/core/src/config.rs` |
| §51 MVP single-server layout, §52 database responsibilities | `deploy/`, `crates/metadata` (SQLite), `docs/OPERATIONS.md` |
| §60 Data lifecycle, §61 crawl algorithm | `crates/agent0/src/crawl.rs` (the drain loop, seeds-first, per-origin scheduling) |

## Verified gaps

- **§24–§35 content routes are absent**: research-paper pipeline, OAI-PMH repositories, books and
  libraries, archives, git/software repositories, forums, news and blogs, documentation sites,
  APIs/JSON/XML, S3/object storage/public datasets, images, video/audio. `crates/sources` is the
  adapter surface; nothing implements those routes.
- **§50 Distributed architecture** is not built. §51's single-server layout is what runs (deliberate).
- **§54–§58 learning**: specialized adapters, source learning, URL-pattern learning and query-demand
  learning are absent.
- **§61 `update_graph` is not a crawl step.** Entities reach the query lane only after a separate
  `agent0 graph build`; a freshly crawled page is visible to lexical search immediately but to graph
  expansion only after that rebuild.
- **§14 URL pattern engine** and **§46 archive-bomb protection** are partial: present as code, not
  as the full behaviour the spec describes.
