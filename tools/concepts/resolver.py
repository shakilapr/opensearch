from __future__ import annotations

import logging
import re
import sqlite3
import time
import uuid
from pathlib import Path
from typing import NamedTuple

import numpy as np

from .config import DEFAULT_DB_PATH, DEFAULT_EMBED_CACHE_DIR, MERGE_SIMILARITY_THRESHOLD
from .decompose import ExtractedConcept

logger = logging.getLogger("concepts.resolver")

def normalize_surface(text: str) -> str:
    """Normalize surface form: lowercase, trim, strip punctuation."""
    t = text.lower().strip()
    t = re.sub(r"[^\w\s-]", "", t)
    return " ".join(t.split())

def slugify(text: str) -> str:
    """Create a URL-safe slug for a concept ID."""
    s = text.lower().strip()
    s = re.sub(r"[^\w\s-]", "", s)
    s = re.sub(r"[\s_]+", "-", s)
    return s[:40].strip("-")

class ResolveResult(NamedTuple):
    concept_id: str
    name: str
    weight: float
    is_new_concept: bool
    is_new_alias: bool
    similarity: float

class ConceptResolver:
    def __init__(
        self,
        db_path: Path = DEFAULT_DB_PATH,
        embed_cache_dir: Path = DEFAULT_EMBED_CACHE_DIR,
        threshold: float = MERGE_SIMILARITY_THRESHOLD,
    ):
        self.db_path = Path(db_path)
        self.threshold = threshold
        self.embed_cache_dir = Path(embed_cache_dir)

        # In-memory indices
        self.alias_to_id: dict[str, str] = {}
        self.concept_names: dict[str, str] = {}  # concept_id -> canonical name
        self.concept_ids: list[str] = []
        self.embeddings_matrix: np.ndarray | None = None

        # FastEmbed model
        from fastembed import TextEmbedding
        logger.info("Loading FastEmbed BGE-small model from %s...", self.embed_cache_dir)
        self.embedder = TextEmbedding("BAAI/bge-small-en-v1.5", cache_dir=str(self.embed_cache_dir))

        self._load_existing_concepts()

    def _load_existing_concepts(self):
        """Load all concepts and aliases from SQLite into in-memory lookup and embedding matrix."""
        con = sqlite3.connect(self.db_path)
        try:
            # 1. Load concepts
            rows = con.execute("SELECT concept_id, name FROM concept WHERE status = 'active'").fetchall()
            for cid, name in rows:
                self.concept_names[cid] = name
                norm = normalize_surface(name)
                if norm:
                    self.alias_to_id[norm] = cid

            # 2. Load aliases
            alias_rows = con.execute("SELECT concept_id, alias FROM concept_alias").fetchall()
            for cid, alias in alias_rows:
                norm = normalize_surface(alias)
                if norm:
                    self.alias_to_id[norm] = cid

            self.concept_ids = list(self.concept_names.keys())
            names = [self.concept_names[cid] for cid in self.concept_ids]

            if names:
                logger.info("Computing embeddings for %d existing concepts...", len(names))
                raw_embeds = list(self.embedder.embed(names))
                vecs = [np.array(e, dtype=np.float32) for e in raw_embeds]
                # Normalize L2
                norms = [np.linalg.norm(v) for v in vecs]
                vecs = [v / (norm if norm > 1e-6 else 1.0) for v, norm in zip(vecs, norms)]
                self.embeddings_matrix = np.vstack(vecs)
            else:
                self.embeddings_matrix = np.zeros((0, 384), dtype=np.float32)

            logger.info("ConceptResolver initialized with %d concepts and %d aliases.", len(self.concept_ids), len(self.alias_to_id))
        finally:
            con.close()

    def resolve_and_merge(
        self,
        extracted: list[ExtractedConcept],
        resource_id: str,
        dry_run: bool = False,
    ) -> list[ResolveResult]:
        """Deduplicate and merge concepts, then write associations to SQLite."""
        if not extracted:
            return []

        # Step 1: Batch embed candidates that need semantic resolution
        candidates_to_embed = []
        candidate_indices = []

        now = int(time.time())
        resolved_items: list[ResolveResult] = []
        pending_novel: list[tuple[str, str, str, np.ndarray, ExtractedConcept]] = [] # cid, name, kind, vec, concept
        pending_alias: list[tuple[str, str]] = []  # cid, alias_name
        doc_associations: list[tuple[str, str, float, str, str, int, int]] = []

        # Quick alias check pass
        for idx, item in enumerate(extracted):
            norm = normalize_surface(item.name)
            if not norm:
                continue

            if norm in self.alias_to_id:
                cid = self.alias_to_id[norm]
                cname = self.concept_names.get(cid, item.name)
                resolved_items.append(ResolveResult(cid, cname, item.weight, False, False, 1.0))
                doc_associations.append((resource_id, cid, item.weight, "direct", item.evidence, now, now))
            else:
                candidates_to_embed.append(item.name)
                candidate_indices.append(idx)

        # Step 2: Vector resolution for non-exact matches
        if candidates_to_embed:
            cand_embeds = list(self.embedder.embed(candidates_to_embed))
            cand_vecs = [np.array(e, dtype=np.float32) for e in cand_embeds]
            # Normalize
            for i, v in enumerate(cand_vecs):
                n = np.linalg.norm(v)
                cand_vecs[i] = v / (n if n > 1e-6 else 1.0)

            for item_idx, vec in zip(candidate_indices, cand_vecs):
                item = extracted[item_idx]
                norm = normalize_surface(item.name)

                best_sim = 0.0
                best_cid = None
                best_name = None

                if self.embeddings_matrix is not None and len(self.embeddings_matrix) > 0:
                    sims = self.embeddings_matrix @ vec
                    best_pos = int(np.argmax(sims))
                    best_sim = float(sims[best_pos])
                    if best_sim >= self.threshold:
                        best_cid = self.concept_ids[best_pos]
                        best_name = self.concept_names[best_cid]

                if best_cid is not None and best_sim >= self.threshold:
                    # Semantic merge! Add as alias to existing concept
                    pending_alias.append((best_cid, item.name))
                    self.alias_to_id[norm] = best_cid
                    resolved_items.append(ResolveResult(best_cid, best_name or item.name, item.weight, False, True, best_sim))
                    doc_associations.append((resource_id, best_cid, item.weight, "direct", item.evidence, now, now))
                else:
                    # Novel concept!
                    slug = slugify(item.name)
                    cid = f"c-{slug[:28]}-{uuid.uuid4().hex[:6]}" if slug else f"c-{uuid.uuid4().hex[:12]}"
                    pending_novel.append((cid, item.name, item.kind, vec, item))

                    self.concept_names[cid] = item.name
                    self.alias_to_id[norm] = cid
                    self.concept_ids.append(cid)

                    # Append to embedding matrix
                    if self.embeddings_matrix is None or len(self.embeddings_matrix) == 0:
                        self.embeddings_matrix = vec.reshape(1, -1)
                    else:
                        self.embeddings_matrix = np.vstack([self.embeddings_matrix, vec])

                    resolved_items.append(ResolveResult(cid, item.name, item.weight, True, False, best_sim))
                    doc_associations.append((resource_id, cid, item.weight, "direct", item.evidence, now, now))

        # Step 3: Write to SQLite in a single transaction
        if not dry_run and (pending_novel or pending_alias or doc_associations):
            con = sqlite3.connect(self.db_path)
            try:
                con.execute("BEGIN TRANSACTION")

                # Insert novel concepts
                for cid, name, kind, _, _ in pending_novel:
                    con.execute(
                        """
                        INSERT OR IGNORE INTO concept (concept_id, name, description, kind, status, created_at, updated_at)
                        VALUES (?, ?, ?, ?, 'active', ?, ?)
                        """,
                        (cid, name, f"Discovered concept: {name}", kind, now, now)
                    )
                    con.execute(
                        "INSERT OR IGNORE INTO concept_alias (concept_id, alias, lang, created_at) VALUES (?, ?, 'en', ?)",
                        (cid, name, now)
                    )

                # Insert merged aliases
                for cid, alias in pending_alias:
                    con.execute(
                        "INSERT OR IGNORE INTO concept_alias (concept_id, alias, lang, created_at) VALUES (?, ?, 'en', ?)",
                        (cid, alias, now)
                    )

                # Insert document associations (upserting max weight)
                con.executemany(
                    """
                    INSERT INTO doc_concept (resource_id, concept_id, weight, relation_kind, evidence, model_version, created_at, updated_at)
                    VALUES (?, ?, ?, ?, ?, 'v2-decompose', ?, ?)
                    ON CONFLICT(resource_id, concept_id, relation_kind) DO UPDATE SET
                        weight = MAX(doc_concept.weight, excluded.weight),
                        evidence = COALESCE(excluded.evidence, doc_concept.evidence),
                        updated_at = excluded.updated_at
                    """,
                    [(r[0], r[1], r[2], r[3], r[4], r[5], r[6]) for r in doc_associations]
                )

                con.commit()
            except Exception as e:
                con.rollback()
                logger.error("Failed to commit resolved concepts to SQLite: %s", e)
                raise
            finally:
                con.close()

        return resolved_items
