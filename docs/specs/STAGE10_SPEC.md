# Stage 10 Spec — Distribution, operations & community

Documentation and scripts only — no new Rust crates. Deliverables:

- `docs/specs/DISTRIBUTION.md` — how a pack built by `crates/dataset` (Stage 6) is published and
  consumed; versioning/`PACK_SCHEMA`; what is server-only (`raw-dataset/`, `models/`, the
  `/var/lib/search` data root) vs shipped.
- `docs/OPERATIONS.md` — runbook: start/stop via the systemd units in `deploy/`, backup/restore
  of `/var/lib/search`, the refresh tick, log/trace locations, capacity + safety bounds
  (`max_response_bytes`, `max_redirects`, `max_parse_ms`, `max_pdf_pages`, `max_browser_ms`,
  `max_robots_bytes`), and what to do when the index is unpublished
  (`degradations=["index_unpublished"]`).
- `docs/specs/ROADMAP.md` — the 11-stage plan mapped to actual status, with the honest remaining gaps
  (no real BGE/SPLADE weights, no trained cross-encoder, single-box only, frontier single-writer
  exception, `doc_type`/`language` indexed-not-stored).
- `docs/specs/CONTRIBUTING.md` — build/test commands (`cargo build --workspace`,
  `cargo test --workspace`, `--features real-indexer`, the UI's `npm ci && npm test && npm run build`,
  `cargo run -p search-bench --release`), the per-stage spec→scaffold→parallel-agents→verify→commit
  workflow, and the "commit per discrete change" convention.
- `scripts/build-release.sh` — `cargo build --release` for the binaries (`agent0`, `search-mcp`,
  `search-api`/`bench`) into `dist/`, printing a manifest of artifacts.
- `scripts/publish-pack.sh` — copy a validated pack from `crates/dataset` output into
  `dist/packs/<name>/<version>/` and print its manifest + checksum.
- `scripts/backup.sh` — safe copy of `/var/lib/search` (SQLite `.backup`-style: stop writer, copy,
  restart; or `VACUUM INTO`) into a timestamped dir, with a warning that a raw `cp` of a live
  SQLite file is unsafe.
- `README.md` — update the top-level README to reflect the finished architecture: the crate map,
  the pipeline, how to ingest/search/serve, the MCP server, and where the specs live.

All scripts must be `set -euo pipefail`, `chmod +x`, and must not contain secrets. Every claim in
the docs must be checkable against the repo — do not document features that are not implemented.
