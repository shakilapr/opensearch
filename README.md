# Agent-Native Local Web Search Engine

An open-source, autonomous search engine designed natively for **AI agents** (such as OpenClaw, Nous Hermes, and custom autonomous agents) and human users. 

Built to run entirely self-hosted on standard VPS hardware (e.g., 16 vCPUs, 64 GB RAM, zero GPU dependency) with **no commercial search API subscriptions**.

---

## 1. Core Philosophy

Traditional search engines attempt to archive the entire internet. This project takes the opposite approach:

> **A compact semantic map of where useful information exists.**

The engine acts as a **resource-allocation engine** rather than a blunt downloader. It does not ask *"Can I download this?"*—it asks *"How much will knowing or refreshing this resource improve search quality?"*

```text
┌────────────────────┐
│       AGENT        │  ──►  Judgment (interprets ambiguity, navigates difficult paths)
└─────────┬──────────┘
          │
┌─────────▼──────────┐
│    SEARCH ENGINE   │  ──►  Reflexes (query planner, dual-lane retrieval, candidate fusion)
└─────────┬──────────┘
          │
┌─────────▼──────────┐
│     WEB MEMORY     │  ──►  Memory (Tantivy lexical index, entity-concept graph, source profiles)
└─────────┬──────────┘
          │
┌─────────▼──────────┐
│      CRAWLER       │  ──►  Senses (adaptive politeness, discovery frontier, freshness monitoring)
└─────────┬──────────┘
          │
┌─────────▼──────────┐
│      LIVE WEB      │  ──►  External Reality
└────────────────────┘
```

---

## 2. System Architecture

The project is structured across three functional planes:

```text
                  ┌──────────────────────┐
                  │     DATA BUILDER     │
                  │  (Server-Side Heavy) │
                  │ Common Crawl/Parquet │
                  │ OpenAlex, arXiv, OAI │
                  └──────────┬───────────┘
                             │
                             ▼
                  ┌──────────────────────┐
                  │      WEB MEMORY      │
                  │ Entities & Concepts  │
                  │ Sources & Work IDs   │
                  │ Tantivy BM25 + Graph │
                  └──────────┬───────────┘
                             │
                             ▼
                  ┌──────────────────────┐
                  │    SEARCH RUNTIME    │
                  │ Query Decomposition  │
                  │ Parallel Retrieval   │
                  │ Live Freshness Check │
                  └──────────┬───────────┘
                             │
                  ┌──────────┴───────────┐
                  ▼                      ▼
             SEARCH UI              AGENT API
         (Vite/React/TS)           (HTTP & MCP)
```

### Key Subsystems:

* **[crawler/](crawler/) — Universal Crawling & Scraping Subsystem:**
  * **RFC 9309 Compliant:** Centralized policy cache for robots.txt rules.
  * **Adaptive Politeness:** Per-origin queues with exponential backoff and dynamic concurrency. Concurrency comes from crawling *many origins*, not hammering one.
  * **Content Routing:** Direct pipeline handling for HTML, PDFs (GROBID/text), feeds (RSS/Atom), sitemaps, JSON/XML APIs, and git repositories.
  * **Escalation Path:** Lightweight HTTP fetch by default; isolates Chromium/Playwright contexts strictly as an escalation path for JS-rendered shells.
  * **Deduplication:** Content hashing, SimHash/MinHash near-duplicate detection.

* **[engine/](engine/) — Search Algorithm & Retrieval Core:**
  * **Parallel Dual-Lane Retrieval:** Executes global document retrieval and source-routing in parallel so a mistaken source route never drops viable answers.
  * **Hybrid Indexing:** Fast mmap-backed Tantivy lexical search (BM25) combined with dense embeddings and an entity-concept-source knowledge graph.
  * **Query-Triggered Freshness:** Active queries (e.g. *"latest PyTorch release"*) dynamically spawn high-priority refresh jobs for stale sources.
  * **Calibrated Agent Policy:** Structured actions (`FOLLOW`, `KEEP`, `WIDEN`, `STOP`) with beam search for ambiguous entities (e.g., distinguishing Apple Inc. vs. apple fruit).

* **[ui/](ui/) — Modern Web Search Frontend:**
  * Interactive React 18 + Vite + TypeScript interface.
  * Real-time search telemetry, source authority metrics, and category filtering (All, Web, Papers, Code, News).
  * Direct inspection of retrieval provenance and ranking signals.

* **[docs/](docs/) — Architecture & Specifications:**
  * Complete reference specifications for the crawler, engine, and storage models.

---

## 3. Core Data Model (The 5 IDs)

Everything in the engine resolves to 5 foundational identities:

1. **`DomainID`:** Hostname metadata (e.g. `arxiv.org`, `github.com`), crawl statistics, and robots policy.
2. **`SourceID`:** Logical collection unit targeted by semantic routing (e.g. `docs.python.org/asyncio`, `github.com/ggerganov/llama.cpp`).
3. **`ResourceID`:** Lightweight fetchable address (URL, sitemap, feed, PDF link).
4. **`WorkID`:** Logical intellectual object (e.g. a research paper across preprint, publisher, and author repository).
5. **`ContentID`:** Deduplicated payload identity across syndications, mirrors, and print views.

---

## 4. Search Complexity Modes

| Mode | Target Latency | Workflow |
| :--- | :--- | :--- |
| **`fast`** | < 50 ms | Pure local Tantivy & graph retrieval. No network I/O, no LLM inference. |
| **`auto`** | 100–300 ms | Hybrid local retrieval + source freshness verification + bounded live fetch if stale. *(Default)* |
| **`deep`** | Multi-step | Query decomposition, beam search over candidate graph paths, agent navigation, and evidence verification. |

---

## 5. Repository & Workspace Topology

To ensure fast local development while handling massive datasets, the project enforces a clear split between local code and server-side storage:

```text
projects/search/
├── crawler/                # [LOCAL & SERVER] Universal crawler implementation
│   ├── ARCHITECTURE.md
│   └── UNIVERSAL_CRAWLER_ARCHITECTURE.md
├── docs/                   # [LOCAL & SERVER] Design specs & algorithm RFCs
│   ├── AGENT_NATIVE_SEARCH_ENGINE.md
│   ├── ARCHITECTURE.md
│   └── UNIVERSAL_CRAWLER_ARCHITECTURE.md
├── engine/                 # [LOCAL & SERVER] Retrieval, ranking & query engine
│   ├── ARCHITECTURE.md
│   └── engine-architecture.md
├── ui/                     # [LOCAL & SERVER] Search web application (Vite / React)
│   ├── src/
│   ├── package.json
│   └── ...
├── models/                 # [SERVER-ONLY] Model weights, GGUF/ONNX checkpoints
├── raw-dataset/            # [SERVER-ONLY] Bulk Common Crawl, Parquet dumps & WARCs
├── .gitignore              # Ignores large datasets, caches, models & media
└── README.md               # Project overview & guide
```

> **Note:** Heavy datasets (`raw-dataset/`), embeddings, model weights (`models/`), and large media files are strictly ignored by Git and reside exclusively on the production server.

---

## 6. Synchronization & Deployment Workflows

To keep local development in sync with the remote VPS (`85.137.250.157`), use one of the following methods:

### Method 1: Remote Git (GitHub / GitLab) — Recommended
This is the standard, cleanest workflow for continuous synchronization:
1. **Push from Local PC:**
   ```bash
   git remote add origin https://github.com/<your-username>/search.git
   git branch -M main
   git push -u origin main
   ```
2. **Pull on the Server (`/root/projects/search`):**
   ```bash
   git clone https://github.com/<your-username>/search.git /root/projects/search
   # To sync ongoing updates:
   cd /root/projects/search && git pull origin main
   ```

### Method 2: Direct Sync via SSH / Rsync
For direct machine-to-machine synchronization:
1. Ensure the SSH daemon is active on the VPS:
   ```bash
   systemctl restart ssh && ufw allow 22/tcp
   ```
2. Sync directly from local terminal:
   ```bash
   rsync -avz --exclude-from=.gitignore ./ vpsgo:/root/projects/search/
   # Or using scp:
   scp -r * vpsgo:/root/projects/search/
   ```

### Method 3: Standalone Git Bundle (Instant via Web VS Code)
Transfer the entire repository history in a single portable file:
1. **Create bundle locally:**
   ```bash
   git bundle create search.bundle --all
   ```
2. Drag and drop `search.bundle` into [Web VS Code](https://code.hexxica.space).
3. **Unpack in server terminal:**
   ```bash
   git clone search.bundle /root/projects/search
   rm search.bundle
   ```

---

## 7. Quick Start

### Frontend (UI)
```bash
cd ui
npm install
npm run dev
```

### Running Tests
```bash
cd ui
npm test
```

---

## 8. Roadmap & Build Stages

* [x] **Stage 0: Core Architecture & Specifications** (Crawler & Engine specifications drafted).
* [ ] **Stage 1: 10K LLM Search MVP** (Tantivy index, source registry, manual seeds, BM25 + graph).
* [ ] **Stage 2: Search Quality & Benchmarks** (Hybrid search evaluation against a 500+ query testbed).
* [ ] **Stage 3: Live Freshness Loop** (Query-triggered background refresh and live results overlay).
* [ ] **Stage 4: Agent Navigation** (Beam search path traversal with Hermes/OpenClaw policy adapters).
* [ ] **Stage 5: Bootstrap Scaling (100K–1M Docs)** (Parquet-based streaming imports from Common Crawl & OpenAlex).

