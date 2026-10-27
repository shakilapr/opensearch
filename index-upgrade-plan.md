# Improve the index, then delete the raw — what that costs and the order it must happen in

Answer to: *can we improve the index model and then delete the raw HTML?*

**Yes. And the deletion is where most of the win is: 284 GB of raw blobs buy 6 GB of index.**
But there is exactly one ordering constraint, and it is not negotiable: **anything you want from
the raw HTML must be extracted before the raw HTML is deleted.** After that the index is the only
copy, and no future parser improvement can recover what was never captured.

All numbers measured 2026-10-24 on this host. Disk currently: **596 GB used / 78 GB free** (89%).

---

## 1. What the index already holds

The Tantivy schema (`crates/indexer/src/schema.rs`) STOREs `url`, `title`, **`body`**,
`entities`, `resource_id`, `work_id`, `content_id`, `source_id` and the three timestamps.

The body is the full extracted text, not a fragment — verified end to end:

```
GET /read?doc_id=res_ae3585e8…   → 6,352 chars
text/d5/d5af6f7a….txt            → 6,352 chars   identical
```

So for the **serving** index (6.0 GB total across the fleet, 315 MB locally) the text is already
inside it, and `/read` serves from the index (`Indexer::doc_text` reads the STORED body,
`crates/indexer/src/store.rs:655`).

What the index does **not** hold:

| Field | State | Consequence |
| --- | --- | --- |
| `headings` | INDEXED, not STORED | cannot be read back; export loses it |
| `doc_type` | INDEXED, not STORED | not a usable facet on read-back |
| `language` | INDEXED, not STORED | same |
| `outlinks` / links | **not persisted at all** | parsed per fetch, then discarded; there is no link table in the schema |

Those four are the only things that would be lost forever if the blobs went away today.

## 2. What the upgrade would cost (measured by sampling 60 raw pages)

| What | Per page | Fleet projection (1,090,184 pages) |
| --- | --- | --- |
| anchors | 125 | **~136 M link edges** (53% internal) |
| H1/H2 text | 133 chars | **~138 MB** |
| doc_type + language | ~20 bytes | ~20 MB |

So enriching the index costs **roughly 1 GB of index fields plus 5–10 GB of SQLite** for the link
edges, against **284 GB** of raw HTML. The trade is about 25:1 in favour of enriching and deleting.

## 3. The order

**Step 0 — free what is already duplicated (no information at risk).**
`raw-dataset/shard-*/blobs` is 47 GB of gzipped HTML whose content hashes are **100% already in the
fleet's own `content` table** (174,216/174,219 on shard 0; 333,477/333,480 on shard 2). Keep
`crawl-log.jsonl` (7 MB) and `frontier.json`, delete the blobs. `target-browser` (4 GB) is idle.
`target` (87 GB) is rebuildable but costs a full rebuild — the deployed binary at
`/opt/search/bin/agent0` is unaffected either way.

**Step 1 — publish the 589,572 documents that already have text.** This needs **no blobs at all** —
the text files exist for 100% of `document_version` rows. It is the largest coverage win available
(19,255 → up to ~590K served) and it is independent of the schema work. Do it first so a failed
upgrade cannot cost coverage.

**Step 2 — land the schema upgrade.** Add `STORED` to `headings`/`doc_type`/`language` (this also
retires decision D001 by making it free, since a re-index is happening anyway) and add a
`link_edge(src_resource, dst_url, anchor, rel)` table. New migration, no format break for readers.

**Step 3 — one offline re-parse pass over the blobs.** Read `blobs/*`, extract headings + links +
doc_type/language, write edges, re-index. **No network.** Estimated hours, not days: ~284 GB of
local reads for 1.09M pages. There is no `agent0 reparse` verb today — that is the new code this
plan needs.

**Step 4 — verify before deleting.** Gates, all on real data:
- pages re-parsed == resources in the shard (count match, not a sample);
- 200 random pages: index body byte-identical to the blob's extracted text;
- headings present and non-empty for the same sample;
- link edges resolve to known `resource_id`s at a stated rate;
- `/read` and a 20-query smoke set behave identically before and after.

**Step 5 — delete the blobs, one shard at a time, after its gate passes.** Before deleting, keep a
1,000-page random sample per shard as a permanent replay fixture (~300 MB) so parser regressions
stay testable without keeping the corpus.

## 4. What the fleet becomes

| | Today | After |
| --- | --- | --- |
| raw HTML blobs | 284 GB | **0** |
| extracted text | 13 GB | 13 GB |
| Tantivy index | 6.0 GB | ~7 GB |
| metadata + link edges | 4.8 GB | ~12–15 GB |
| **total** | **~307 GB** | **~32–35 GB** |

That is a 9× reduction with more capability than today (headings readable, links queryable,
`doc_type`/`language` facetable).

## 5. The one decision this needs

**Do you want the link graph?** It is the only irreversibly-lost item, and it is also the input the
source-first design in `next-move.md` depends on. Capturing it makes Step 3 a full re-parse (hours,
CPU only). Skipping it makes Step 3 much shorter — but the 284 GB goes away and the links can then
only be rebuilt by re-crawling 1.09M pages.

Everything else in the plan is reversible.
