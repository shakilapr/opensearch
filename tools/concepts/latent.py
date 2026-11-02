from __future__ import annotations

import logging
import sqlite3
import time
from pathlib import Path
from typing import Any

import numpy as np

from .config import DEFAULT_DB_PATH, DEFAULT_EMBED_CACHE_DIR

logger = logging.getLogger("concepts.latent")

EMBED_MODEL_NAME = "BAAI/bge-small-en-v1.5"
EMBED_DIM = 384


def slerp(v0: np.ndarray, v1: np.ndarray, t: float) -> np.ndarray:
    """Spherical linear interpolation (SLERP) between two normalized vectors."""
    dot = np.clip(np.dot(v0, v1), -1.0, 1.0)
    theta = np.arccos(dot)
    sin_theta = np.sin(theta)

    if sin_theta < 1e-5:
        # Collinear or almost collinear: fall back to linear interpolation
        res = (1.0 - t) * v0 + t * v1
    else:
        res = (np.sin((1.0 - t) * theta) / sin_theta) * v0 + (np.sin(t * theta) / sin_theta) * v1

    norm = np.linalg.norm(res)
    return res / (norm if norm > 1e-6 else 1.0)


class LatentSpaceEngine:
    """Manages the continuous 384-dimensional latent manifold for concepts and documents.

    Features:
    1. Persistent SQLite vector storage (`concept_embedding`, `doc_embedding`).
    2. In-memory contiguous NumPy matrices for sub-millisecond dot-product retrieval.
    3. FastEmbed local CPU inference for query and novel concept embeddings.
    4. Latent Concept Probing (Semantic KNN).
    5. Hybrid Probing (Lexical exact boost + Latent Cosine similarity).
    6. Latent Geodesic Pathing (SLERP trajectory between concepts).
    7. Concept Vector Arithmetic & Analogies (Positive/Negative vectors).
    8. Document Barycentric Projection & Latent Neighborhood search.
    """

    def __init__(
        self,
        db_path: Path = DEFAULT_DB_PATH,
        embed_cache_dir: Path = DEFAULT_EMBED_CACHE_DIR,
    ):
        self.db_path = Path(db_path)
        self.embed_cache_dir = Path(embed_cache_dir)
        self._embedder = None

        # In-memory index
        self.concept_ids: list[str] = []
        self.concept_names: dict[str, str] = {}
        self.concept_kinds: dict[str, str] = {}
        self.alias_to_id: dict[str, str] = {}
        self.embeddings_matrix: np.ndarray = np.zeros((0, EMBED_DIM), dtype=np.float32)
        self.cid_to_row_idx: dict[str, int] = {}

        self._init_db_tables()
        self.sync_and_load()

    @property
    def embedder(self):
        """Lazy-loaded FastEmbed model."""
        if self._embedder is None:
            from fastembed import TextEmbedding

            logger.info("Loading FastEmbed %s from %s...", EMBED_MODEL_NAME, self.embed_cache_dir)
            self._embedder = TextEmbedding(EMBED_MODEL_NAME, cache_dir=str(self.embed_cache_dir))
        return self._embedder

    def _init_db_tables(self):
        """Ensure vector embedding tables exist in SQLite."""
        con = sqlite3.connect(self.db_path)
        try:
            con.execute(
                """
                CREATE TABLE IF NOT EXISTS concept_embedding (
                    concept_id TEXT PRIMARY KEY,
                    vector BLOB NOT NULL,
                    dim INTEGER NOT NULL,
                    model TEXT NOT NULL,
                    updated_at INTEGER NOT NULL
                )
                """
            )
            con.execute(
                """
                CREATE TABLE IF NOT EXISTS doc_embedding (
                    resource_id TEXT PRIMARY KEY,
                    vector BLOB NOT NULL,
                    dim INTEGER NOT NULL,
                    model TEXT NOT NULL,
                    concept_count INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                )
                """
            )
            con.commit()
        finally:
            con.close()

    def sync_and_load(self) -> int:
        """Sync persistent embeddings with concepts in SQLite, embedding any missing concepts, and load into memory."""
        con = sqlite3.connect(self.db_path)
        try:
            # 1. Load concept metadata
            rows = con.execute("SELECT concept_id, name, kind FROM concept WHERE status = 'active'").fetchall()
            for cid, name, kind in rows:
                self.concept_names[cid] = name
                self.concept_kinds[cid] = kind
                self.alias_to_id[name.lower().strip()] = cid

            # Load aliases
            alias_rows = con.execute("SELECT concept_id, alias FROM concept_alias").fetchall()
            for cid, alias in alias_rows:
                self.alias_to_id[alias.lower().strip()] = cid

            # 2. Check which concepts already have persistent embeddings
            cached_rows = con.execute("SELECT concept_id, vector FROM concept_embedding WHERE model = ?", (EMBED_MODEL_NAME,)).fetchall()
            cached_vectors: dict[str, np.ndarray] = {}
            for cid, blob in cached_rows:
                if len(blob) == EMBED_DIM * 4:
                    vec = np.frombuffer(blob, dtype=np.float32).copy()
                    # Defensive re-normalization
                    n = np.linalg.norm(vec)
                    cached_vectors[cid] = vec / (n if n > 1e-6 else 1.0)

            # 3. Identify missing concepts that need embedding
            missing_cids = [cid for cid in self.concept_names if cid not in cached_vectors]
            if missing_cids:
                logger.info("Computing embeddings for %d new concepts...", len(missing_cids))
                missing_names = [self.concept_names[cid] for cid in missing_cids]
                raw_embeds = list(self.embedder.embed(missing_names))
                now = int(time.time())
                inserts = []
                for cid, emb in zip(missing_cids, raw_embeds):
                    vec = np.array(emb, dtype=np.float32)
                    n = np.linalg.norm(vec)
                    norm_vec = vec / (n if n > 1e-6 else 1.0)
                    cached_vectors[cid] = norm_vec
                    inserts.append((cid, norm_vec.tobytes(), EMBED_DIM, EMBED_MODEL_NAME, now))

                con.executemany(
                    "INSERT OR REPLACE INTO concept_embedding (concept_id, vector, dim, model, updated_at) VALUES (?, ?, ?, ?, ?)",
                    inserts,
                )
                con.commit()
                logger.info("Persisted %d new concept embeddings to SQLite.", len(inserts))

            # 4. Build in-memory contiguous matrix
            self.concept_ids = [cid for cid in self.concept_names if cid in cached_vectors]
            if self.concept_ids:
                mat = np.vstack([cached_vectors[cid] for cid in self.concept_ids])
                self.embeddings_matrix = mat.astype(np.float32)
                self.cid_to_row_idx = {cid: idx for idx, cid in enumerate(self.concept_ids)}
            else:
                self.embeddings_matrix = np.zeros((0, EMBED_DIM), dtype=np.float32)
                self.cid_to_row_idx = {}

            logger.info("LatentSpaceEngine ready: %d concept vectors loaded in memory.", len(self.concept_ids))
            return len(self.concept_ids)
        finally:
            con.close()

    def embed_text(self, text: str) -> np.ndarray:
        """Embed arbitrary query text into 384-d normalized vector."""
        emb = list(self.embedder.embed([text]))[0]
        v = np.array(emb, dtype=np.float32)
        n = np.linalg.norm(v)
        return v / (n if n > 1e-6 else 1.0)

    def resolve_concept_id(self, query_or_name: str) -> str | None:
        """Resolve concept ID from exact ID, name, or alias."""
        norm = query_or_name.lower().strip()
        if query_or_name in self.concept_names:
            return query_or_name
        if norm in self.alias_to_id:
            return self.alias_to_id[norm]
        # Partial match
        for alias, cid in self.alias_to_id.items():
            if norm == alias or norm in alias:
                return cid
        return None

    def get_concept_vector(self, concept_id: str) -> np.ndarray | None:
        """Retrieve the normalized vector for a concept ID."""
        row_idx = self.cid_to_row_idx.get(concept_id)
        if row_idx is not None and row_idx < len(self.embeddings_matrix):
            return self.embeddings_matrix[row_idx]
        return None

    def probe_latent(self, query: str, top_k: int = 10, threshold: float = 0.30) -> list[dict[str, Any]]:
        """Semantic KNN search across the concept manifold."""
        if len(self.embeddings_matrix) == 0:
            return []

        q_vec = self.embed_text(query)
        sims = self.embeddings_matrix @ q_vec

        # Top K indices
        top_indices = np.argsort(sims)[::-1][:top_k]

        con = sqlite3.connect(self.db_path)
        try:
            results = []
            for idx in top_indices:
                score = float(sims[idx])
                if score < threshold:
                    continue
                cid = self.concept_ids[idx]
                name = self.concept_names[cid]
                kind = self.concept_kinds.get(cid, "concept")

                # Doc count and average weight
                row = con.execute(
                    "SELECT COUNT(resource_id), AVG(weight) FROM doc_concept WHERE concept_id = ?",
                    (cid,),
                ).fetchone()
                doc_count = row[0] if row else 0
                avg_weight = round(row[1] or 0.0, 3) if row else 0.0

                # Aliases
                aliases = [
                    r[0] for r in con.execute(
                        "SELECT alias FROM concept_alias WHERE concept_id = ? LIMIT 5",
                        (cid,),
                    ).fetchall()
                ]

                results.append({
                    "concept_id": cid,
                    "name": name,
                    "kind": kind,
                    "similarity": round(score, 4),
                    "document_count": doc_count,
                    "avg_weight": avg_weight,
                    "aliases": aliases,
                })
            return results
        finally:
            con.close()

    def probe_hybrid(self, query: str, top_k: int = 10) -> list[dict[str, Any]]:
        """Hybrid probe: combines exact/alias surface matching with latent semantic cosine similarity."""
        # 1. Latent probe
        latent_hits = self.probe_latent(query, top_k=top_k * 2, threshold=0.25)
        latent_map = {h["concept_id"]: h for h in latent_hits}

        # 2. Lexical probe
        norm_q = query.lower().strip()
        con = sqlite3.connect(self.db_path)
        try:
            sql = """
            SELECT c.concept_id, c.name, c.kind, COUNT(dc.resource_id) as doc_count, AVG(dc.weight) as avg_weight
            FROM concept c
            LEFT JOIN doc_concept dc ON c.concept_id = dc.concept_id
            WHERE lower(c.name) LIKE ?
               OR c.concept_id IN (SELECT concept_id FROM concept_alias WHERE lower(alias) LIKE ?)
            GROUP BY c.concept_id
            LIMIT ?
            """
            lex_rows = con.execute(sql, (f"%{norm_q}%", f"%{norm_q}%", top_k)).fetchall()
            lex_cids = set()

            scored_results = []
            for cid, name, kind, count, avg_w in lex_rows:
                lex_cids.add(cid)
                aliases = [r[0] for r in con.execute("SELECT alias FROM concept_alias WHERE concept_id = ? LIMIT 5", (cid,)).fetchall()]
                # If also in latent hits, blend score
                latent_sim = latent_map.get(cid, {}).get("similarity", 0.6)
                # Exact match receives 1.0 lexical score
                lex_score = 1.0 if norm_q == name.lower() or any(norm_q == a.lower() for a in aliases) else 0.8
                blended_score = 0.5 * lex_score + 0.5 * latent_sim
                scored_results.append({
                    "concept_id": cid,
                    "name": name,
                    "kind": kind,
                    "score": round(blended_score, 4),
                    "similarity": round(latent_sim, 4),
                    "document_count": count,
                    "avg_weight": round(avg_w or 0.0, 3),
                    "aliases": aliases,
                    "match_type": "lexical+latent" if cid in latent_map else "lexical",
                })

            # Add pure latent hits that were not caught by SQL LIKE
            for cid, h in latent_map.items():
                if cid not in lex_cids:
                    h_copy = dict(h)
                    h_copy["score"] = round(h["similarity"] * 0.9, 4)
                    h_copy["match_type"] = "latent"
                    scored_results.append(h_copy)

            # Sort by blended score
            scored_results.sort(key=lambda x: x["score"], reverse=True)
            return scored_results[:top_k]
        finally:
            con.close()

    def bridge_geodesic(
        self,
        concept_a: str,
        concept_b: str,
        steps: int = 4,
        top_k_per_step: int = 2,
    ) -> list[dict[str, Any]]:
        """Compute the continuous geodesic trajectory (SLERP) in latent space between concept_a and concept_b.

        Finds intermediate concepts along the arc that connect the two semantic spaces.
        """
        cid_a = self.resolve_concept_id(concept_a)
        cid_b = self.resolve_concept_id(concept_b)

        if not cid_a or not cid_b:
            # If not in concept graph, embed query texts directly
            v_a = self.get_concept_vector(cid_a) if cid_a else self.embed_text(concept_a)
            v_b = self.get_concept_vector(cid_b) if cid_b else self.embed_text(concept_b)
            name_a = self.concept_names.get(cid_a, concept_a)
            name_b = self.concept_names.get(cid_b, concept_b)
        else:
            v_a = self.get_concept_vector(cid_a)
            v_b = self.get_concept_vector(cid_b)
            name_a = self.concept_names[cid_a]
            name_b = self.concept_names[cid_b]

        if v_a is None or v_b is None or len(self.embeddings_matrix) == 0:
            return []

        # Cosine distance between endpoints
        direct_similarity = float(np.dot(v_a, v_b))

        waypoints = []
        # Exclude endpoints: step 1 to steps
        t_values = np.linspace(0.0, 1.0, steps + 2)[1:-1]

        seen_cids = {cid_a, cid_b} if cid_a and cid_b else set()

        for t in t_values:
            v_t = slerp(v_a, v_b, float(t))
            sims = self.embeddings_matrix @ v_t

            # Find closest concepts to this waypoint
            top_indices = np.argsort(sims)[::-1]
            step_hits = []
            for idx in top_indices:
                cid = self.concept_ids[idx]
                if cid in seen_cids:
                    continue
                score = float(sims[idx])
                seen_cids.add(cid)
                step_hits.append({
                    "concept_id": cid,
                    "name": self.concept_names[cid],
                    "kind": self.concept_kinds.get(cid, "concept"),
                    "similarity_to_waypoint": round(score, 4),
                })
                if len(step_hits) >= top_k_per_step:
                    break

            waypoints.append({
                "trajectory_progress": round(float(t), 2),
                "nearest_concepts": step_hits,
            })

        return [{
            "source_concept": {"concept_id": cid_a, "name": name_a},
            "target_concept": {"concept_id": cid_b, "name": name_b},
            "endpoint_cosine_similarity": round(direct_similarity, 4),
            "geodesic_waypoints": waypoints,
        }]

    def analogize(
        self,
        positive: list[str],
        negative: list[str] | None = None,
        top_k: int = 5,
    ) -> list[dict[str, Any]]:
        """Vector arithmetic: positive_vectors - negative_vectors -> nearest neighbor concepts.

        Example: ['pair bonding', 'neurotransmitter'] - ['psychology'] -> 'oxytocin'
        """
        if len(self.embeddings_matrix) == 0:
            return []

        neg_list = negative or []
        target_vec = np.zeros(EMBED_DIM, dtype=np.float32)
        input_cids = set()

        for term in positive:
            cid = self.resolve_concept_id(term)
            if cid:
                input_cids.add(cid)
                v = self.get_concept_vector(cid)
            else:
                v = self.embed_text(term)
            if v is not None:
                target_vec += v

        for term in neg_list:
            cid = self.resolve_concept_id(term)
            if cid:
                input_cids.add(cid)
                v = self.get_concept_vector(cid)
            else:
                v = self.embed_text(term)
            if v is not None:
                target_vec -= v

        norm = np.linalg.norm(target_vec)
        if norm < 1e-6:
            return []
        target_vec = target_vec / norm

        sims = self.embeddings_matrix @ target_vec
        top_indices = np.argsort(sims)[::-1]

        results = []
        for idx in top_indices:
            cid = self.concept_ids[idx]
            if cid in input_cids:
                continue
            score = float(sims[idx])
            results.append({
                "concept_id": cid,
                "name": self.concept_names[cid],
                "kind": self.concept_kinds.get(cid, "concept"),
                "similarity": round(score, 4),
            })
            if len(results) >= top_k:
                break
        return results

    def compute_and_save_doc_embeddings(self, limit: int | None = None) -> int:
        """Compute document barycenters in latent space from concept associations and persist to doc_embedding."""
        con = sqlite3.connect(self.db_path)
        try:
            # Query documents with concepts
            sql = """
            SELECT dc.resource_id, dc.concept_id, dc.weight
            FROM doc_concept dc
            JOIN concept c ON dc.concept_id = c.concept_id
            WHERE c.status = 'active'
            ORDER BY dc.resource_id
            """
            rows = con.execute(sql).fetchall()
            doc_concepts: dict[str, list[tuple[str, float]]] = {}
            for res_id, cid, weight in rows:
                if res_id not in doc_concepts:
                    doc_concepts[res_id] = []
                doc_concepts[res_id].append((cid, float(weight)))

            now = int(time.time())
            saved = 0
            inserts = []

            for res_id, c_list in doc_concepts.items():
                if limit and saved >= limit:
                    break
                doc_vec = np.zeros(EMBED_DIM, dtype=np.float32)
                total_w = 0.0
                valid_count = 0

                for cid, weight in c_list:
                    v = self.get_concept_vector(cid)
                    if v is not None:
                        doc_vec += weight * v
                        total_w += weight
                        valid_count += 1

                if valid_count > 0 and total_w > 0:
                    norm = np.linalg.norm(doc_vec)
                    if norm > 1e-6:
                        doc_norm_vec = doc_vec / norm
                        inserts.append((
                            res_id,
                            doc_norm_vec.tobytes(),
                            EMBED_DIM,
                            EMBED_MODEL_NAME,
                            valid_count,
                            now,
                        ))
                        saved += 1

            if inserts:
                con.executemany(
                    "INSERT OR REPLACE INTO doc_embedding (resource_id, vector, dim, model, concept_count, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
                    inserts,
                )
                con.commit()
                logger.info("Computed and saved %d document latent barycenters to SQLite.", len(inserts))
            return saved
        finally:
            con.close()

    def find_documents_by_latent(
        self,
        query_or_concept: str,
        top_k: int = 5,
    ) -> list[dict[str, Any]]:
        """Retrieve documents whose concept barycenter is closest to the query in latent space."""
        cid = self.resolve_concept_id(query_or_concept)
        if cid and self.get_concept_vector(cid) is not None:
            q_vec = self.get_concept_vector(cid)
        else:
            q_vec = self.embed_text(query_or_concept)

        con = sqlite3.connect(self.db_path)
        try:
            rows = con.execute(
                "SELECT d.resource_id, dv.title, d.vector, d.concept_count FROM doc_embedding d JOIN document_version dv ON d.resource_id = dv.resource_id WHERE d.model = ?",
                (EMBED_MODEL_NAME,),
            ).fetchall()

            if not rows:
                return []

            res_ids = []
            titles = []
            concept_counts = []
            vec_list = []

            for r_id, title, blob, c_count in rows:
                if len(blob) == EMBED_DIM * 4:
                    vec = np.frombuffer(blob, dtype=np.float32)
                    res_ids.append(r_id)
                    titles.append(title)
                    concept_counts.append(c_count)
                    vec_list.append(vec)

            if not vec_list:
                return []

            doc_mat = np.vstack(vec_list)
            sims = doc_mat @ q_vec
            top_indices = np.argsort(sims)[::-1][:top_k]

            results = []
            for idx in top_indices:
                score = float(sims[idx])
                results.append({
                    "resource_id": res_ids[idx],
                    "title": titles[idx],
                    "cosine_similarity": round(score, 4),
                    "concept_count": concept_counts[idx],
                })
            return results
        finally:
            con.close()
