from __future__ import annotations

import logging
import sqlite3
from collections import defaultdict
from pathlib import Path
from typing import NamedTuple

try:
    from .config import DEFAULT_DB_PATH
except ImportError:
    from config import DEFAULT_DB_PATH

logger = logging.getLogger("concepts.bridge")

class BridgePath(NamedTuple):
    source: str
    target: str
    path: list[str]
    score: float
    documents: list[str]

class ConceptBridger:
    def __init__(self, db_path: Path = DEFAULT_DB_PATH):
        self.db_path = Path(db_path)

    def find_bridging_concepts(
        self,
        concept_a: str,
        concept_b: str,
        max_hops: int = 3,
        top_k: int = 5,
    ) -> list[dict]:
        """Find intermediate concepts that bridge concept_a and concept_b."""
        con = sqlite3.connect(self.db_path)
        try:
            # Resolve aliases to concept_ids
            def resolve_id(c_name: str) -> str | None:
                norm = c_name.lower().strip()
                row = con.execute("SELECT concept_id FROM concept WHERE lower(name) = ? OR concept_id = ?", (norm, norm)).fetchone()
                if row:
                    return row[0]
                row = con.execute("SELECT concept_id FROM concept_alias WHERE lower(alias) = ?", (norm,)).fetchone()
                if row:
                    return row[0]
                # Fuzzy like match
                row = con.execute("SELECT concept_id FROM concept_alias WHERE lower(alias) LIKE ? LIMIT 1", (f"%{norm}%",)).fetchone()
                return row[0] if row else None

            id_a = resolve_id(concept_a)
            id_b = resolve_id(concept_b)

            if not id_a or not id_b:
                return [{"error": f"Could not resolve concepts: {concept_a} ({id_a}), {concept_b} ({id_b})"}]

            # 2-hop intersection: find concepts co-occurring with BOTH id_a and id_b across documents
            query = """
            WITH docs_a AS (
                SELECT resource_id FROM doc_concept WHERE concept_id = ?
            ),
            docs_b AS (
                SELECT resource_id FROM doc_concept WHERE concept_id = ?
            ),
            bridge_concepts AS (
                SELECT dc.concept_id, COUNT(DISTINCT dc.resource_id) as joint_docs, AVG(dc.weight) as avg_weight
                FROM doc_concept dc
                WHERE dc.concept_id NOT IN (?, ?)
                  AND dc.resource_id IN (
                      SELECT resource_id FROM doc_concept WHERE concept_id = ?
                      UNION
                      SELECT resource_id FROM doc_concept WHERE concept_id = ?
                  )
                GROUP BY dc.concept_id
                ORDER BY joint_docs DESC, avg_weight DESC
                LIMIT ?
            )
            SELECT bc.concept_id, c.name, c.kind, bc.joint_docs, bc.avg_weight
            FROM bridge_concepts bc
            JOIN concept c ON bc.concept_id = c.concept_id
            """

            rows = con.execute(query, (id_a, id_b, id_a, id_b, id_a, id_b, top_k)).fetchall()
            bridges = []
            for cid, name, kind, docs_cnt, weight in rows:
                bridges.append({
                    "concept_id": cid,
                    "name": name,
                    "kind": kind,
                    "co_occurring_docs": docs_cnt,
                    "avg_salience": round(weight, 3),
                })
            return bridges
        finally:
            con.close()
