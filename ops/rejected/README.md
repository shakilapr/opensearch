# Rejected measurements

Kept only so nobody re-derives them. Do not cite.

- `2026-11-01-bm25-docverid-bug.json` — nDCG 0.0 on all 600 cases. The index had been rebuilt with
  `docver_id` as `doc_id` while the graded labels are `resource_id`, so nothing could match. The
  same binary on the same corpus scored 0.2112 once the identity was fixed (see
  `ops/bench-2026-11-02/bm25.json`). A result of exactly 0.0 on a corpus that answers by hand is a
  wiring bug, not a model result.
