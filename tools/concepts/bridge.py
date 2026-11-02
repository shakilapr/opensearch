from __future__ import annotations

import logging
import sqlite3
from collections import defaultdict
from pathlib import Path
from typing import NamedTuple

try:
    from .config import DEFAULT_DB_PATH
    from .latent import LatentSpaceEngine
except ImportError:
    from config import DEFAULT_DB_PATH
    from latent import LatentSpaceEngine

logger = logging.getLogger("concepts.bridge")

class BridgePath(NamedTuple):
    source: str
    target: str
    path: list[str]
    score: float
    documents: list[str]

class ConceptBridger:
    def __init__(self, db_path: Path = DEFAULT_DB_PATH, latent_engine: LatentSpaceEngine | None = None):
        self.db_path = Path(db_path)
        self._latent = latent_engine

    @property
    def latent(self) -> LatentSpaceEngine:
        if self._latent is None:
            self._latent = LatentSpaceEngine(db_path=self.db_path)
        return self._latent

    def find_bridging_concepts(
        self,
        concept_a: str,
        concept_b: str,
        max_hops: int = 3,
        top_k: int = 5,
        mode: str = "hybrid",
    ) -> list[dict]:
        """Find intermediate concepts connecting concept_a and concept_b.

        mode:
          - 'latent': Continuous geodesic (SLERP) path in 384-d latent space.
          - 'graph': Discrete corpus document co-occurrence.
          - 'hybrid': Both latent trajectory and graph co-occurrence.
        """
        # Latent geodesic trajectory
        latent_bridges = []
        if mode in ("latent", "hybrid"):
            latent_bridges = self.latent.bridge_geodesic(concept_a, concept_b, steps=top_k)

        if mode == "latent":
            return latent_bridges

        # Graph co-occurrence bridging
        graph_bridges = []
        con = sqlite3.connect(self.db_path)
        try:
            id_a = self.latent.resolve_concept_id(concept_a)
            id_b = self.latent.resolve_concept_id(concept_b)

            if id_a and id_b:
                query = """
                WITH bridge_concepts AS (
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
                rows = con.execute(query, (id_a, id_b, id_a, id_b, top_k)).fetchall()
                for cid, name, kind, docs_cnt, weight in rows:
                    graph_bridges.append({
                        "concept_id": cid,
                        "name": name,
                        "kind": kind,
                        "co_occurring_docs": docs_cnt,
                        "avg_salience": round(weight, 3),
                    })
        finally:
            con.close()

        if mode == "graph":
            return graph_bridges

        # Hybrid result combining geodesic trajectory and graph connections
        return [{
            "geodesic_latent_trajectory": latent_bridges[0] if latent_bridges else None,
            "topological_graph_bridges": graph_bridges,
        }]
