# Contributing

How to build, test, and land changes in this workspace. Every command below is run from
the repo root unless a `cd` says otherwise.

## Toolchain

Observed and supported: **Rust** (rustup, current stable; `crates.io` reachable),
**Node.js 26 / npm 11** (for `ui/`), **Python 3.14** (dataset importers). No GPU is
required. The workspace pins `edition = "2021"`, `resolver = "2"`.

## Build and test

```sh
# Rust: build + full hermetic test suite.
cargo build --workspace
cargo test  --workspace

# The real Tantivy indexer is wired in behind a per-package feature on
# `search-api` and `search-retrieval`. There is no workspace-level
# `real-indexer` feature, so target the package:
cargo test -p search-api --features real-indexer
# (crates/api/tests/real_indexer.rs is `#![cfg(feature = "real-indexer")]`,
#  so without the feature that integration test compiles out.)

# Stage-2 ablation harness (prints the metrics table, writes benchmarks/results.json).
cargo run -p search-bench --release
cargo test -p search-bench

# A single crate, e.g.:
cargo test -p search-dataset
cargo test -p search-mcp
```

UI (`ui/`):

```sh
cd ui
npm ci
npm test          # vitest run
npm run build     # tsc && vite build
npm run lint      # tsc --noEmit
npm run deploy    # build + copy dist/* to /var/www/search
```

Release staging and ops scripts (see `docs/specs/DISTRIBUTION.md`, `docs/OPERATIONS.md`):

```sh
bash -n scripts/*.sh                 # syntax check
scripts/build-release.sh             # -> dist/bin/{agent0,search-mcp,bench} + manifest
scripts/publish-pack.sh <pack-dir>   # -> dist/packs/<name>/<version>/
scripts/backup.sh                    # consistent backup of /var/lib/search
```

### Test conventions

* Tests are **hermetic**: no network. Fixture servers and in-process fakes stand in for
  live endpoints (e.g. `MockBrowser`, the MCP fake backend, metadata in a `tempfile`
  dir). Network-dependent code is behind a feature (`bge`, `chromium`) and must not be
  required to build or to run the default tests.
* Two committed fixture trees are deliberately **not** gitignored: `benchmarks/corpus/**`
  and `datasets/fixtures/**`. Keep additions small.
* Migrations are source, not data: `crates/metadata/migrations/*.sql` is un-ignored on
  purpose. Every migration must be idempotent (`IF NOT EXISTS`) so a replay is safe, and
  `SCHEMA_VERSION` (`crates/metadata/src/migrations.rs`) must be bumped with it.

## The per-stage workflow

The project is built one stage at a time from the stage specs in `docs/`:

1. **Spec** — freeze the stage contract in `docs/specs/STAGE<N>_SPEC.md` (interfaces, bounds,
   exit gate) before writing code. If code and spec disagree, the spec wins until a commit
   says otherwise.
2. **Scaffold** — create the crate(s) the spec names, with the frozen interfaces and a
   lib-level `//!` doc pointing at the spec.
3. **Parallel agents** — implement crates concurrently, **each agent owning only its own
   crate's files**. The dependency rule is one-way: stage crates depend on `search-core`
   (and, e.g., `retrieval` on `indexer`/`graph`); `search-core` never depends on them.
   `search-metadata` owns persistence with its own plain row structs and takes no
   dependency on the stage crates.
4. **Verify** — the stage's exit gate must be demonstrated with real output: run the
   in-crate tests and any documented command (e.g. the bench table). No stage proceeds on
   a plausible-looking stub.
5. **Commit** — commit the stage as discrete, reviewable changes.

## Commit convention

**One commit per discrete change.** Commit at natural boundaries (a crate's interfaces, its
tests, a spec freeze, a fix) rather than batching unrelated work, and use the existing
history as the model (`git log --oneline`): `feat(stage7): …`, `docs+scaffold(stages8-10):
…`, `fix(...)`. Keep a spec freeze in its own commit so the contract is auditable
separately from the code that follows it.

## Ground rules

* **Never commit secrets.** No API keys, tokens, or credentials anywhere — adapters take
  injected paths/clients, and provider policy adapters are injected a `dyn ModelClient`
  with no key. Scripts in `scripts/` must run without sudo and contain no secrets.
* **Never commit server-only data.** `raw-dataset/`, `models/`, `datasets/` output, and the
  `/var/lib/search` data root are gitignored and must stay out of the tree.
* **Don't document a feature that isn't implemented.** The docs are checked against the
  repo; `docs/specs/ROADMAP.md` tracks the honest gaps.
* **Crawled content is untrusted.** Treat it as data, never as instructions or tool grants.
* Keep the scope of a change to the crate(s) the stage spec assigns it; don't edit another
  crate's files in the same change.
