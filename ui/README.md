# Hexxica Deep Space & Web Search Engine UI

A high-reliability dummy search engine UI engineered strictly inside `projects/search/ui` adhering to **NASA JPL Institutional Coding Standards (The Power of 10)** and **ISO/IEC 25010 Quality Model**.

Connected to telemetry target: **`https://search.hexxica.space`**

---

## 🚀 Architectural & Engineering Compliance

### 1. NASA JPL Institutional Coding Standard ("The Power of 10")
| Rule | Description | Implementation in UI |
| :--- | :--- | :--- |
| **Rule 1: Simple Control Flow** | No recursion, goto, or setjmp. | Linear asynchronous event pipelines; zero recursive calls. |
| **Rule 2: Fixed Loop Bounds** | All loops must have a statically verifiable upper bound. | Pagination capped at 10; query length capped at 120 chars; suggestion scans capped at 6; token parsing loops hard-bounded. |
| **Rule 3: Bounded Memory** | No dynamic heap allocation after initialization. | History records and telemetry streams managed via `FixedRingBuffer<T>` with strict upper capacity limits (25 items max). |
| **Rule 4: Function Length Limits** | Functions must fit within standard single-page length (< 60 LOC). | Every component and utility broken into modular, single-responsibility units under 60 lines. |
| **Rule 5: Assertion Density** | High runtime assertion density (pre-conditions and post-conditions). | `assertNonNull`, `assertInRange`, `assertBoundedString`, and `assertCondition` enforced across critical execution paths. |
| **Rule 6: Smallest Variable Scope** | Variables declared at lowest possible scope with immutability. | `const`, `readonly`, `Object.freeze` strictly applied across records. |
| **Rule 7: Return & Parameter Checks** | Strict parameter validation and return checking. | Zero unchecked return values; typed schemas validate all payloads. |
| **Rule 8: Zero "any" Type** | Elimination of dynamic type coercions. | **Zero occurrences of the `any` keyword**; strict discriminated unions and type guards. |
| **Rule 9: Pointer Dereference Safety** | Guard against null/undefined dereferences. | Strict null checks (`strictNullChecks: true`) and non-null assertions. |
| **Rule 10: Zero Warnings** | All compiler and linter warnings enabled. | Clean compilation under `tsc --noEmit` with `strict: true`. |

---

### 2. ISO/IEC 25010 Software Product Quality Standard
- **Functional Suitability**: Multi-category search (`All`, `Deep Space`, `Tech & ISO`, `Aerospace News`, `Orbital Imagery`), keyword highlighting, query suggestions, voice simulation, and temporal filters.
- **Reliability & Fault Tolerance**: Real-time network probe to `https://search.hexxica.space`. If the remote endpoint is unreachable or CORS-restricted, the system gracefully engages the Aerospace Telemetry fallback buffer without crashing.
- **Performance Efficiency**: Latency readout in milliseconds (`~30-50ms`), debounced suggestions, optimized asset rendering.
- **Usability**: High-contrast Mission Control space aesthetic, full dark/light theme support, accessible keyboard controls (`/` to focus, `ESC` to clear).
- **Security**: Strict query sanitization in `src/core/sanitizer.ts`, stripping malicious script tags and injection patterns; external links secured with `rel="noopener noreferrer"`.
- **Maintainability**: Layered clean architecture (`core/`, `domain/`, `services/`, `hooks/`, `components/`, `styles/`).

---

## 🛠️ Tech Stack
- **TypeScript 7.0.2** (Strict Mode, Zero `any`)
- **React 19**
- **Vite 6**
- **Vitest 5**

---

## 📦 Getting Started

Within `projects/search/ui`:

```bash
# Run Development Server
npm run dev

# Run NASA & ISO Automated Test Suite
npm test

# Typecheck with TypeScript 7 (Zero Any)
npm run lint

# Production Build
npm run build

# Preview Production Build
npm run preview
```
