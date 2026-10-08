# Distribution

What this project ships, how a dataset **pack** is published and consumed, and what
stays server-only. Everything here is checkable against the repo — the pack format is
implemented in `crates/dataset` (package `search-dataset`), frozen in
[`STAGE6_SPEC.md`](STAGE6_SPEC.md).

## Two kinds of artifact

| artifact | built by | format | shipped? |
| :--- | :--- | :--- | :--- |
| **Binaries** — `agent0`, `search-mcp`, `bench` | `cargo build --release` (`scripts/build-release.sh`) | ELF executables | yes, `dist/bin/` |
| **Dataset packs** — normalized records (entities, sources, URLs, titles, abstracts, edges) | the Stage-6 dataset builder in `crates/dataset` (`scripts/publish-pack.sh`) | JSONL shards + `manifest.json` | yes, `dist/packs/` |
| Raw web data, model weights, runtime state | the crawler / model downloaders | — | **no** (server-only) |

## Binaries

`scripts/build-release.sh` builds and stages exactly three binaries. There is **no
`search-api` binary** — the HTTP API is served by `agent0 serve`, and the `search-api`
package is a library that `agent0` links with its `real-indexer` feature enabled
(`crates/agent0/Cargo.toml`).

| binary | package | what it is |
| :--- | :--- | :--- |
| `agent0` | `search-agent0` | the driver: `agent0 ingest <url>` / `search <query> [limit]` / `serve` / `refresh` |
| `search-mcp` | `search-mcp` | the stdio MCP server (`docs/MCP.md`) |
| `bench` | `search-bench` | the Stage-2 ablation harness |

```sh
scripts/build-release.sh            # -> dist/bin/{agent0,search-mcp,bench} + dist/manifest.txt
scripts/build-release.sh --out /tmp/rel
```

`dist/manifest.txt` records the commit, toolchain versions, and each artifact's size and
SHA-256. `dist/` is gitignored.

## Dataset packs

A **pack** is a directory of JSONL shards plus a `manifest.json`, written by
`search_dataset::write_pack` / `build_from_adapter` and re-checked by
`search_dataset::validate_pack` (`crates/dataset/src/pack.rs`).

### Layout

```
<pack-dir>/
├── manifest.json
├── <name>-00000.jsonl        # one JSON *Record* per line, shards named {name}-{index:05}.jsonl
└── <name>-00001.jsonl
```

Each line is a `Record` (`crates/dataset/src/lib.rs`):

```json
{"id":"...","source":"openalex","url":"https://...","title":"...","text":"...","published_at":1690000000,"extras":{}}
```

### Manifest

```json
{
  "name": "openalex",
  "version": "2024-10",
  "records": 12345,
  "shards": [
    { "path": "openalex-00000.jsonl", "records": 10000, "checksum": "fnv1a64:9f2c...e1" }
  ],
  "created_at": 1699999999,
  "schema": 1
}
```

* `schema` is `PACK_SCHEMA` (`crates/dataset/src/lib.rs`) and is currently **1**. A
  consumer must reject a pack whose `schema` it does not understand.
* `checksum` is **FNV-1a 64-bit** of the shard's exact on-disk bytes, hex-encoded with a
  `fnv1a64:` tag (`crates/dataset/src/checksum.rs`). It is an **integrity / tamper**
  check, not a cryptographic one — that is why the checksum module is a hand-rolled hash
  with no `sha2` dependency.
* `validate_pack` fails if the manifest is missing/unreadable, any shard is missing, a
  shard checksum no longer matches, a shard's JSONL record count differs from the
  manifest, or the per-shard counts do not sum to `records`.

### Producing a pack

Adapters (`CommonCrawlAdapter`, `WikidataAdapter`, `OpenAlexAdapter`, `OaiPmhAdapter`)
read an **injected path** — a committed fixture in tests — and never perform network I/O
or read a credential. There is **no `search-dataset` CLI binary**: packs are produced
programmatically by calling `build_from_adapter(...)` / `write_pack(...)`. The offline
round-trip (parse fixture → write → validate → reject a tampered shard) is exercised by:

```sh
cargo test -p search-dataset
```

### Publishing

`scripts/publish-pack.sh` takes an already-built pack directory, independently re-checks
it (shard presence, FNV-1a checksums, JSONL line counts, total), then stages it:

```sh
scripts/publish-pack.sh path/to/pack                 # -> dist/packs/<name>/<version>/
scripts/publish-pack.sh path/to/pack --out /tmp/pub
```

It prints the manifest and writes `SHA256SUMS` (SHA-256 of every staged file) for
transfer integrity. The script re-computes the FNV-1a sums itself; it does not trust the
manifest. Full `validate_pack` semantics additionally live in the Rust crate and are
covered by `cargo test -p search-dataset`.

### Consuming

Fetch the pack, verify `SHA256SUMS` over the transfer, then load the shards and cross-
check each against its manifest entry (`records` + `fnv1a64:` checksum) before indexing.
Reject the pack if `schema` is unknown. The engine's ingestion path (`agent0 ingest`,
`crates/ingest`) is the downstream consumer of normalized records.

### Versioning

* `version` is the pack's own string (e.g. a snapshot date like `2024-10`).
* `schema` is the format version (`PACK_SCHEMA`). Bump `PACK_SCHEMA` only when the
  on-disk `Record`/manifest shape changes incompatibly; consumers must refuse a `schema`
  they don't implement.

## Server-only (never shipped, gitignored)

| path | contents | why server-only |
| :--- | :--- | :--- |
| `raw-dataset/` | raw HTML/PDF/JSON blobs, WARC/Parquet shard downloads, extracted text | bulk scraped data |
| `models/` | BGE/SPLADE weights, cross-encoder, checkpoints | large model artifacts |
| `/var/lib/search` | runtime state root (SQLite `meta/meta.sqlite`, Tantivy `tantivy/base` + `tantivy/overlay`, `blobs/`, `text/`) | a live mmap + WAL database; corrupt if copied mid-write |
| `datasets/` output | built packs (only committed `datasets/fixtures/**` are tracked) | generated data |

`.gitignore` and `.stignore` both exclude these; `crates/core` refuses to start if the
data root looks like it lives inside the synced repo (`Config::assert_not_synced`).

## What is NOT distributed yet

Honest gaps, so a pack consumer is not surprised:

* There is **no pack loader in the engine** that reads `manifest.json` directly — packs
  are validated (Rust `validate_pack`, or `scripts/publish-pack.sh`) and their records
  go through the normal ingestion path. No signed/verified registry exists.
* No remote registry, no signing keys, no update channel: distribution is by copying the
  `dist/` outputs.
