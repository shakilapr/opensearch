# Decision log

Product and scope calls that are not code, recorded so they stop resurfacing as open items.
Each entry states the question, the evidence, the decision, and the cost.

---

## D001 — Stored fields: keep `doc_type` and `language` indexed-not-stored

**Question** (`ops/plan.json` item `stored-fields-decision`). Read-back through
`Indexer::all_docs` is lossy: `headings`, `doc_type`, `language` and
`visible_generation` are INDEXED but not STORED in the frozen Tantivy schema
(`crates/indexer/src/schema.rs:12–31`), so a document rebuilt from the index comes back with
those fields empty. Options were (a) a schema version bump plus a full re-index, or (b) a
sidecar. Should we do either?

> **Correction.** `entities` **is** STORED (schema line 23) and was nevertheless being dropped by
> `Candidate::to_index_doc`, which set it to an empty vector — so every `export --raw` lost the
> organizing model's key entities even though the index was paying to store them. That is fixed:
> the field is carried through, and it is now written comma-joined so a multi-word entity survives
> the round trip. This document previously (and self-contradictorily) listed `entities` as
> un-stored in its first paragraph.

**Evidence.**

- The STORED set already covers the raw content: `url`, `title`, `body`, `resource_id`,
  `work_id`, `content_id`, `source_id`, `entities`, and the three timestamps
  (`published_at`/`modified_at`/`fetched_at`). Only `headings`, `doc_type`, `language` and
  `visible_generation` are missing.
- `headings` **is** recoverable: `portable.rs` rebuilds pack pages from `title` + `body`, and
  `graphbuild` re-derives entities from the same fields, so the pack path does not depend on
  the un-stored `headings`.
- `doc_type` and `language` are set on every document at ingest by the MIME router and the
  HTML parser respectively (`crates/router`, `crates/parser/src/html.rs`). They are cheap to
  re-derive deterministically from the stored `url` + `body`, so storing them adds nothing
  that cannot be recomputed.
- The one genuinely derived field is `visible_generation`, which is an index-internal
  visibility watermark, not document content. It must never be exported.

**Decision — (c): neither a schema bump nor a sidecar.** The schema stays frozen, and the
lossiness is documented rather than "fixed", because:

1. Nothing in the product depends on recovering `headings`/`doc_type`/`language` from the
   index: the pack path rebuilds pages from `title`+`body`, and the router/parser re-derive
   the two categorical fields deterministically.
2. A schema version bump forces a full re-index of ~310K documents across four shards to
   recover fields that are already recomputable — paying a large, measured cost to remove a
   lossiness that has no consumer.
3. A sidecar adds a second artifact that must be kept consistent with the index for the same
   non-benefit.

**Consequences.**

- `all_docs()` keeps its documented "lossy by design" contract
  (`crates/indexer/src/store.rs:645–651`) and its test
  `all_docs_returns_every_live_doc_with_stored_fields` keeps asserting the STORED fields only.
- If a future consumer needs `doc_type`/`language` on read-back, it must re-derive them from
  `url`+`body` (same router/parser code, no new state) — or this decision must be revisited
  with that consumer named.
- Revisit trigger: an export/import path that must round-trip a document byte-identically
  *without* access to the original response body.
