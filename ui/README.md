# Hexxica Web Search Engine UI

A high-reliability search UI for the local agent-native search engine, engineered
inside `projects/search/ui` and adhering to **NASA JPL Institutional Coding
Standards (The Power of 10)** and the **ISO/IEC 25010 Quality Model**.

The UI is a **real client of the local search API**. It contains **no demo data
and no fabricated results**: every result shown comes from the API response, and
when the API cannot answer the UI shows an honest, actionable state instead of
inventing rows.

---

## ⚙️ The local search API is required

The UI talks to the search API over HTTP (`POST /search`, `GET /health`). Start
the server first:

```bash
# From the repository root — serve the local search API on 127.0.0.1:8080
agent0 serve
```

The client (`src/services/hexxicaClient.ts`) resolves its base URL from the Vite
env var `VITE_API_BASE`, falling back to `http://127.0.0.1:8080`. Vite inlines
`import.meta.env.*` at **build time** (it is not read at runtime), so changing it
requires a rebuild. See `.env.example`.

When the API is unreachable, or returns a non-2xx status, the UI renders
*"The local search server is not reachable at the configured base URL."* with the
configured base URL and a **Retry** action. A successful response with zero
results renders an honest *"No results for this query"* state.

---

## 🧠 Agentic engine

Search requests send `{ query, engine: true, mode }`. The UI exposes a small
**Fast / Auto / Deep** mode selector that maps to the API's `mode` field (default
`auto`). The response may include these additive, optional fields — the UI handles
every one of them being absent (older server) without crashing:

| Field | UI usage |
| :--- | :--- |
| `engine` | Shows `engine: on` in the coverage strip. |
| `coverage` (`supported`/`unsupported`, `mandatory_ok`, `coverage`) | Coverage / mandatory-predicate strip. |
| `stop_reason` | `stopped: <reason>` in the coverage strip. |
| `trace` | Mapped onto the response (string list). |
| `answer` | Rendered in a labelled panel with a model-generated disclaimer (only when present). |
| per-result `lane_ranks` | `bm25 #3`, `source_first #12` rows in the *Why this result?* expander. |
| per-result `unit_support` | Supporting evidence-unit chips in the expander. |

The `answer` text is produced by the server; the UI only displays it and shows a
visible disclaimer that it is model-generated and may be incomplete.

---

## 🚀 Architectural & Engineering Compliance

### 1. NASA JPL Institutional Coding Standard ("The Power of 10")
| Rule | Description | Implementation in UI |
| :--- | :--- | :--- |
| **Rule 1: Simple Control Flow** | No recursion, goto, or setjmp. | Linear asynchronous event pipelines; zero recursive calls. |
| **Rule 2: Fixed Loop Bounds** | All loops must have a statically verifiable upper bound. | Pagination bounded; query length capped at 120 chars; suggestion scans capped; token parsing loops hard-bounded. |
| **Rule 3: Bounded Memory** | No dynamic heap allocation after initialization. | History records and telemetry streams managed via `FixedRingBuffer<T>` with strict upper capacity limits. |
| **Rule 4: Function Length Limits** | Functions must fit within standard single-page length (< 60 LOC). | Every component and utility broken into modular, single-responsibility units. |
| **Rule 5: Assertion Density** | High runtime assertion density (pre- and post-conditions). | `assertNonNull`, `assertInRange`, `assertBoundedString`, and `assertCondition` enforced on critical paths. |
| **Rule 6: Smallest Variable Scope** | Variables declared at lowest possible scope with immutability. | `const`, `readonly`, `Object.freeze` strictly applied across records. |
| **Rule 7: Return & Parameter Checks** | Strict parameter validation and return checking. | Zero unchecked return values; typed schemas validate all payloads. |
| **Rule 8: Zero "any" Type** | Elimination of dynamic type coercions. | **Zero occurrences of the `any` keyword**; strict discriminated unions and type guards. |
| **Rule 9: Pointer Dereference Safety** | Guard against null/undefined dereferences. | Strict null checks (`strictNullChecks: true`) and non-null assertions. |
| **Rule 10: Zero Warnings** | All compiler and linter warnings enabled. | Clean compilation under `tsc --noEmit` with `strict: true`. |

### 2. ISO/IEC 25010 Software Product Quality Standard
- **Functional Suitability**: Category filters (`All`, `Space`, `Tech`, `News`,
  `Images`), keyword highlighting, mode selector, and engine provenance views.
- **Reliability & Fault Tolerance**: A real network call to the local API. On
  failure the UI surfaces a typed `SearchUnavailableError` and an actionable retry
  state — it never substitutes fabricated data.
- **Performance Efficiency**: Latency readout from the API response, debounced
  suggestions, optimized asset rendering.
- **Usability**: High-contrast mission-control aesthetic, dark/light theme,
  keyboard controls (`/` to focus, `ESC` to clear).
- **Security**: Strict query sanitization in `src/core/sanitizer.ts`; external
  links secured with `rel="noopener noreferrer"`. The UI holds **no credentials**.
- **Maintainability**: Layered clean architecture (`core/`, `domain/`, `services/`,
  `hooks/`, `components/`, `styles/`).

---

## 🛠️ Tech Stack
- **TypeScript 7** (Strict Mode, Zero `any`)
- **React 19**
- **Vite 6**
- **Vitest 5** (+ jsdom)

---

## 📦 Getting Started

Within `projects/search/ui`:

```bash
npm ci   # or: npm install

# Run the development server (API must be running: `agent0 serve`)
npm run dev

# Automated test suite
npm test

# Typecheck (Zero Any)
npm run lint

# Production build
npm run build

# Preview the production build
npm run preview
```

---

## 🔌 API base (`VITE_API_BASE`) — build & serve

```bash
# Local development: talk to a locally running search-api.
VITE_API_BASE=http://127.0.0.1:8080 npm run dev

# Production: build same-origin (empty value) and let nginx proxy /search and
# /health to the search-api — see deploy/nginx.conf.
VITE_API_BASE= npm run build && npm run deploy   # copies dist/* to /var/www/search
```

When deployed behind nginx, `/search` and `/health` are reverse-proxied to
`127.0.0.1:8080`, so an empty `VITE_API_BASE` (same-origin) is the recommended
production setting. If the UI and API live on different hosts, set
`VITE_API_BASE` to the API origin, e.g. `https://search-api.example.com`.

---
