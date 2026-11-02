//! `agent0 index build` — index documents that are already downloaded.
//!
//! The crawler writes three things per document: a `resource` row (the URL), a
//! `document_version` row (title + hash + the `content_id` it points at), and the extracted text
//! under `content.text_path`. Tantivy is written at the end of that same path — so when a crawl is
//! interrupted, or when a shard is resumed into a fresh index, the *documents* are on disk but the
//! index is behind.
//!
//! Measured 2026-10-28 on the fleet: **1,163,766 document versions with extracted text, 569,000 in
//! Tantivy, 255,069 published.** There is no crawl to run — the corpus is already there and only
//! the index is missing. Before this verb the only way to close that gap was to re-crawl, which
//! would re-fetch every page to recover bytes that are already on local disk.
//!
//! What it does: stream `document_version ⨝ resource ⨝ content`, read each `text_path`, and index
//! in batches through `Pipeline::index_batch` — the same code path the crawler uses, so the schema,
//! the dedupe and the manifest publishing cannot drift. Documents whose text file is missing are
//! counted and skipped, never invented.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use search_core::config::Config;
use search_core::AppResult;
use search_indexer::IndexDoc;

use crate::pipeline::Pipeline;

/// Documents indexed per Tantivy commit. 512 amortises the ~2 s per-commit cost (measured: a
/// per-document commit made the crawl commit-bound) while keeping a kill's loss bounded.
const DEFAULT_BATCH: usize = 512;

use search_metadata::MetadataReader;

/// One document ready to index.
struct Pending {
    doc_id: String,
    resource_id: String,
    content_id: String,
    url: String,
    title: String,
    text_path: String,
    published_at: Option<i64>,
    fetched_at: i64,
}

fn flag_usize(args: &[String], flag: &str) -> Option<usize> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1)?.parse().ok()
}

fn flag_str(args: &[String], flag: &str) -> Option<String> {
    let idx = args.iter().position(|a| a == flag)?;
    args.get(idx + 1).cloned()
}

/// One page of downloaded documents that have extracted text.
///
/// Paged on `docver_id` rather than materialised: 1.16M rows of metadata is small, but each row
/// carries a text path and the caller then reads megabytes of body, so holding "all pending docs"
/// in memory is what let a 386k-document shard reach 3.5 GB and be OOM-killed (measured
/// 2026-10-28, `CONSTRAINT_MEMCG` kill on the index build).
fn pending_docs_page(reader: &MetadataReader, after: &str, limit: usize) -> AppResult<Vec<Pending>> {
    let conn = reader.conn();
    let mut stmt = conn
        .prepare(
            // Newest version per resource only. A resource can have many `document_version` rows
            // (the crawl re-ingests a changed page), and indexing all of them puts the same URL in
            // the index several times: measured 2026-10-28, one Google AdMob page appeared five
            // times in a rebuilt index. Search dedupes by `doc_id`, so those were five *different*
            // ids for one page — duplicate results, not versions of one result.
            "SELECT dv.docver_id, dv.resource_id, dv.content_id, r.canonical_url, dv.title,
                    c.text_path, dv.published_at, dv.fetched_at
               FROM document_version dv
               JOIN resource r ON r.resource_id = dv.resource_id
               JOIN content  c ON c.content_id = dv.content_id
              WHERE c.text_path IS NOT NULL
                AND dv.docver_id > ?1
                AND dv.version_no = (
                      SELECT MAX(d2.version_no) FROM document_version d2
                       WHERE d2.resource_id = dv.resource_id)
              ORDER BY dv.docver_id
              LIMIT ?2",
        )
        .map_err(search_metadata::db_err)?;
    let rows = stmt
        .query_map(rusqlite::params![after, limit as i64], |r| {
            Ok(Pending {
                doc_id: r.get(0)?,
                resource_id: r.get(1)?,
                content_id: r.get(2)?,
                url: r.get(3)?,
                title: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                text_path: r.get(5)?,
                published_at: r.get(6)?,
                fetched_at: r.get(7)?,
            })
        })
        .map_err(search_metadata::db_err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(search_metadata::db_err)?;
    Ok(rows)
}

/// Report of one shard's rebuild.
struct Report {
    indexed: usize,
    missing_text: usize,
    empty_text: usize,
    elapsed_ms: u64,
}

/// Build the index for one data root.
async fn build_root(cfg: &Config, batch: usize, limit: usize) -> AppResult<Report> {
    let started = Instant::now();
    // A writer is required: this is the indexing pass. Reading and writing the same root is
    // exactly what `Pipeline::open` does for a crawl.
    let pipeline = Pipeline::open(cfg)?;
    pipeline.enable_bulk_ingest(batch);

    let reader = MetadataReader::from_path(&cfg.meta_db())?;

    let mut report = Report {
        indexed: 0,
        missing_text: 0,
        empty_text: 0,
        elapsed_ms: 0,
    };
    let mut batch_docs: Vec<IndexDoc> = Vec::with_capacity(batch);
    // Page size is independent of the index batch: the page bounds peak memory (one text at a
    // time is still the real cost), the batch bounds the commit count.
    let page = 2_000usize;
    let mut after = String::new();
    let mut seen = 0usize;

    'pages: loop {
        let rows = pending_docs_page(&reader, &after, page.min(limit.saturating_sub(seen)).max(1))?;
        if rows.is_empty() {
            break;
        }
        for p in rows {
            seen += 1;
            after = p.doc_id.clone();
            if seen > limit {
                break 'pages;
            }
            let body = match std::fs::read_to_string(&p.text_path) {
                Ok(b) => b,
                Err(_) => {
                    // The row exists but the text is gone. Counting it is honest; indexing a title
                    // with an empty body would put a document in the index that cannot be read.
                    report.missing_text += 1;
                    continue;
                }
            };
            if body.trim().is_empty() {
                report.empty_text += 1;
                continue;
            }
            batch_docs.push(IndexDoc {
                doc_id: p.doc_id,
                version: 1,
                resource_id: p.resource_id,
                work_id: None,
                content_id: Some(p.content_id),
                url: p.url,
                title: p.title,
                headings: String::new(),
                // Derived from `body` by `to_tantivy`; the index keeps the excerpt, not the text.
                excerpt: String::new(),
                body,
                entities: Vec::new(),
                source_id: None,
                doc_type: "html".to_string(),
                language: "en".to_string(),
                published_at: p.published_at,
                modified_at: None,
                fetched_at: p.fetched_at,
                // A rebuilt document is visible immediately: it is being indexed *now*, and the
                // manifest this pass publishes is what gates it.
                visible_generation: 0,
                tombstone: false,
            });

            if batch_docs.len() >= batch {
                let n_docs = batch_docs.len();
                pipeline.index_batch(std::mem::take(&mut batch_docs))?;
                report.indexed += n_docs;
                if report.indexed % (batch * 10) == 0 {
                    tracing::info!(
                        indexed = report.indexed,
                        rate = format!(
                            "{:.0}/s",
                            report.indexed as f64 / started.elapsed().as_secs_f64()
                        ),
                        "index build: progress"
                    );
                }
            }
        }
        if seen >= limit {
            break;
        }
    }
    // Publish: every buffered document becomes visible under one manifest.
    pipeline.flush_overlay_batch()?;
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(report)
}

/// `agent0 index build [--dir PATH]... [--batch N] [--limit N]`
pub async fn run(args: &[String]) -> ExitCode {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" => match args.get(i + 1) {
                Some(v) => {
                    dirs.push(PathBuf::from(v));
                    i += 2;
                }
                None => return usage("--dir needs a path"),
            },
            _ => break,
        }
    }
    if dirs.is_empty() {
        dirs.push(Config::from_env().data_dir);
    }
    let batch = flag_usize(args, "--batch").unwrap_or(DEFAULT_BATCH).max(1);
    let limit = flag_usize(args, "--limit").unwrap_or(usize::MAX);

    let mut failed = false;
    for dir in &dirs {
        let mut cfg = Config::from_env();
        cfg.data_dir = dir.clone();
        println!("{}:", dir.display());
        match build_root(&cfg, batch, limit).await {
            Ok(r) => {
                println!(
                    "  indexed {} document(s) in {:.1}s ({} missing text, {} empty)",
                    r.indexed,
                    r.elapsed_ms as f64 / 1000.0,
                    r.missing_text,
                    r.empty_text
                );
            }
            Err(e) => {
                failed = true;
                eprintln!("  error: {e}");
            }
        }
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn usage(why: &str) -> ExitCode {
    eprintln!("error: {why}");
    eprintln!("usage: agent0 index build [--dir PATH]... [--batch N] [--limit N]");
    ExitCode::from(2)
}
