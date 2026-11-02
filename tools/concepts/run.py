from __future__ import annotations

import argparse
import logging
import sqlite3
import sys
from pathlib import Path

from .config import DEFAULT_DB_PATH
from .decompose import ConceptDecomposer
from .resolver import ConceptResolver

logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(message)s")
logger = logging.getLogger("concepts.runner")

def fetch_sample_documents(db_path: Path, limit: int = 5, query: str | None = None) -> list[tuple[str, str, str]]:
    """Fetch (resource_id, title, text) tuples from the database and disk text store."""
    con = sqlite3.connect(db_path)
    try:
        sql = """
        SELECT d.resource_id, d.title, c.text_path
        FROM document_version d
        JOIN content c ON d.content_id = c.content_id
        WHERE c.byte_len > 400
        """
        params = []
        if query:
            sql += " AND d.title LIKE ?"
            params.append(f"%{query}%")
        sql += " ORDER BY c.byte_len DESC LIMIT ?"
        params.append(limit)

        rows = con.execute(sql, params).fetchall()
        docs = []
        for res_id, title, text_path in rows:
            p = Path(text_path)
            if p.exists():
                try:
                    text = p.read_text(encoding="utf-8", errors="replace")
                    docs.append((res_id, title or "Untitled", text))
                except Exception as e:
                    logger.warning("Could not read text for %s: %s", text_path, e)
        return docs
    finally:
        con.close()

def main():
    parser = argparse.ArgumentParser(description="Decompose articles into concepts with free LLMs and merge into concept universe.")
    parser.add_argument("--limit", type=int, default=3, help="Number of documents to process.")
    parser.add_argument("--query", type=str, default=None, help="Filter documents by title query (e.g. 'chemistry', 'biology', 'quantum').")
    parser.add_argument("--dry-run", action="store_true", help="Perform extraction and resolution without writing to database.")
    parser.add_argument("--db", type=str, default=str(DEFAULT_DB_PATH), help="Path to meta.sqlite.")
    args = parser.parse_args()

    db_path = Path(args.db)
    if not db_path.exists():
        logger.error("Database not found at %s", db_path)
        sys.exit(1)

    logger.info("Initializing Concept Decomposer and Resolver...")
    decomposer = ConceptDecomposer()
    resolver = ConceptResolver(db_path=db_path)

    docs = fetch_sample_documents(db_path, limit=args.limit, query=args.query)
    if not docs:
        logger.warning("No documents matched the criteria.")
        return

    logger.info("Fetched %d documents for decomposition.", len(docs))

    total_extracted = 0
    total_novel = 0
    total_merged = 0
    total_exact = 0

    for i, (res_id, title, text) in enumerate(docs, 1):
        print("\n" + "=" * 70, flush=True)
        print(f"[{i}/{len(docs)}] Decomposing: {title}", flush=True)
        print(f"Resource ID: {res_id} | Text length: {len(text)} chars", flush=True)
        print("=" * 70, flush=True)

        extracted = decomposer.decompose(title, text)
        print(f"-> Extracted {len(extracted)} granular concepts.", flush=True)
        total_extracted += len(extracted)

        results = resolver.resolve_and_merge(extracted, resource_id=res_id, dry_run=args.dry_run)

        novel = [r for r in results if r.is_new_concept]
        merged = [r for r in results if r.is_new_alias]
        exact = [r for r in results if not r.is_new_concept and not r.is_new_alias]

        total_novel += len(novel)
        total_merged += len(merged)
        total_exact += len(exact)

        print(f"Resolution summary: {len(novel)} novel, {len(merged)} semantically merged, {len(exact)} exact alias matches.", flush=True)

        if novel:
            print("  Top Novel Concepts Added:", flush=True)
            for n in novel[:6]:
                print(f"    + [{n.concept_id}] \"{n.name}\" (weight: {n.weight})", flush=True)

        if merged:
            print("  Top Semantic Merges:", flush=True)
            for m in merged[:6]:
                print(f"    ~ Merged \"{m.name}\" -> [{m.concept_id}] (similarity: {m.similarity:.3f}, weight: {m.weight})", flush=True)

    print("\n" + "#" * 70, flush=True)
    print("PIPELINE EXECUTION SUMMARY", flush=True)
    print(f"Documents Processed: {len(docs)}", flush=True)
    print(f"Total Concepts Extracted: {total_extracted}", flush=True)
    print(f"Novel Concepts Created:   {total_novel}", flush=True)
    print(f"Semantic Merges:          {total_merged}", flush=True)
    print(f"Exact Matches:            {total_exact}", flush=True)
    print("#" * 70, flush=True)

if __name__ == "__main__":
    main()
