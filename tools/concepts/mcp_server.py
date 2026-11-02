#!/usr/bin/env python3
"""MCP Server exposing the Concept Hypergraph and Associative Bridging tools over stdio."""

from __future__ import annotations

import json
import logging
import sqlite3
import sys
from pathlib import Path
from typing import Any

# Ensure both package and direct script execution work
_current_dir = Path(__file__).resolve().parent
sys.path.insert(0, str(_current_dir.parent.parent))
sys.path.insert(0, str(_current_dir))

try:
    from tools.concepts.config import DEFAULT_DB_PATH
    from tools.concepts.bridge import ConceptBridger
    from tools.concepts.latent import LatentSpaceEngine
except ImportError:
    from config import DEFAULT_DB_PATH
    from bridge import ConceptBridger
    from latent import LatentSpaceEngine

# Setup quiet logging to stderr so stdout is purely JSON-RPC
logging.basicConfig(level=logging.INFO, stream=sys.stderr, format="[MCP] %(message)s")
logger = logging.getLogger("concepts.mcp")

PROTOCOL_VERSION = "2024-11-05"

def get_tools_def() -> list[dict[str, Any]]:
    return [
        {
            "name": "probe_concepts",
            "description": "Probe the concept hypergraph using continuous latent space vector similarity (BGE-small 384-d) and/or lexical matching. Returns canonical concepts, cosine similarity, salience weights, and document counts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Concept name, mechanism, or topic to search for (e.g. 'oxytocin', 'database', 'diffusion', 'biochemical basis of attraction').",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of concepts to return (default 10).",
                        "default": 10,
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["hybrid", "latent", "exact"],
                        "description": "'hybrid' (default: blends surface matches + latent cosine similarity), 'latent' (pure dense vector nearest-neighbors), 'exact' (strict substring).",
                        "default": "hybrid",
                    },
                },
                "required": ["query"],
            },
        },
        {
            "name": "bridge_concepts",
            "description": "Find intermediate bridging concepts connecting two topics or domains via continuous geodesic spherical interpolation (SLERP) in 384-d latent space and/or discrete document co-occurrence.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "concept_a": {
                        "type": "string",
                        "description": "First anchor topic or concept (e.g. 'chemistry', 'diffusion').",
                    },
                    "concept_b": {
                        "type": "string",
                        "description": "Second anchor topic or concept (e.g. 'romance', 'contrastive').",
                    },
                    "top_k": {
                        "type": "integer",
                        "description": "Number of intermediate bridging waypoints or concepts to return (default 5).",
                        "default": 5,
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["hybrid", "latent", "graph"],
                        "description": "'hybrid' (default: returns both latent geodesic trajectory and corpus graph bridges), 'latent' (continuous SLERP trajectory), 'graph' (discrete document co-occurrence).",
                        "default": "hybrid",
                    },
                },
                "required": ["concept_a", "concept_b"],
            },
        },
        {
            "name": "analogize_concepts",
            "description": "Execute vector arithmetic in the continuous concept latent space (e.g. positive=['pair bonding', 'neurotransmitter'], negative=['psychology'] -> returns 'oxytocin').",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "positive": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Concepts or phrases to add together in latent space.",
                    },
                    "negative": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Concepts or phrases to subtract from the vector direction.",
                        "default": [],
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Number of closest concepts to return (default 5).",
                        "default": 5,
                    },
                },
                "required": ["positive"],
            },
        },
        {
            "name": "find_documents_by_latent",
            "description": "Retrieve documents whose concept-barycenter embedding in 384-d latent space is closest to a query concept or phrase.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Query concept, phrase, or topic to match against document barycenters.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum documents to return (default 5).",
                        "default": 5,
                    },
                },
                "required": ["query"],
            },
        },
        {
            "name": "find_documents_by_concepts",
            "description": "Find documents that intersect across a list of concept IDs or names.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "concepts": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "List of concept IDs (e.g. 'c-influxdb-224c22') or concept names.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum documents to return (default 5).",
                        "default": 5,
                    },
                },
                "required": ["concepts"],
            },
        },
        {
            "name": "read_passages",
            "description": "Read exact contextual passages and evidence quotes from a document, optionally focused on specific concepts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "resource_id": {
                        "type": "string",
                        "description": "The resource_id of the document.",
                    },
                    "concept_id": {
                        "type": "string",
                        "description": "Optional concept_id to focus on.",
                    },
                },
                "required": ["resource_id"],
            },
        },
    ]

class McpServer:
    def __init__(self, db_path: Path = DEFAULT_DB_PATH):
        self.db_path = Path(db_path)
        self.latent = LatentSpaceEngine(db_path=self.db_path)
        self.bridger = ConceptBridger(db_path=self.db_path, latent_engine=self.latent)

    def probe_concepts(self, query: str, limit: int = 10, mode: str = "hybrid") -> list[dict[str, Any]]:
        if mode == "latent":
            return self.latent.probe_latent(query, top_k=limit)
        elif mode == "hybrid":
            return self.latent.probe_hybrid(query, top_k=limit)
        else:
            # Exact substring
            con = sqlite3.connect(self.db_path)
            try:
                q = f"%{query.lower().strip()}%"
                sql = """
                SELECT c.concept_id, c.name, c.kind, COUNT(dc.resource_id) as doc_count, AVG(dc.weight) as avg_weight
                FROM concept c
                LEFT JOIN doc_concept dc ON c.concept_id = dc.concept_id
                WHERE lower(c.name) LIKE ?
                   OR c.concept_id IN (SELECT concept_id FROM concept_alias WHERE lower(alias) LIKE ?)
                GROUP BY c.concept_id
                ORDER BY doc_count DESC, avg_weight DESC
                LIMIT ?
                """
                rows = con.execute(sql, (q, q, limit)).fetchall()
                results = []
                for cid, name, kind, count, avg_w in rows:
                    aliases = [
                        r[0] for r in con.execute(
                            "SELECT alias FROM concept_alias WHERE concept_id = ? LIMIT 5", (cid,)
                        ).fetchall()
                    ]
                    results.append({
                        "concept_id": cid,
                        "name": name,
                        "kind": kind,
                        "document_count": count,
                        "avg_weight": round(avg_w or 0.0, 3),
                        "aliases": aliases,
                    })
                return results
            finally:
                con.close()

    def bridge_concepts(self, concept_a: str, concept_b: str, top_k: int = 5, mode: str = "hybrid") -> list[dict[str, Any]]:
        return self.bridger.find_bridging_concepts(concept_a, concept_b, top_k=top_k, mode=mode)

    def analogize_concepts(self, positive: list[str], negative: list[str] | None = None, limit: int = 5) -> list[dict[str, Any]]:
        return self.latent.analogize(positive, negative, top_k=limit)

    def find_documents_by_latent(self, query: str, limit: int = 5) -> list[dict[str, Any]]:
        return self.latent.find_documents_by_latent(query, top_k=limit)

    def find_documents_by_concepts(self, concepts: list[str], limit: int = 5) -> list[dict[str, Any]]:
        con = sqlite3.connect(self.db_path)
        try:
            # Resolve names to IDs
            resolved_ids = []
            for c in concepts:
                norm = c.lower().strip()
                row = con.execute("SELECT concept_id FROM concept WHERE concept_id = ? OR lower(name) = ?", (c, norm)).fetchone()
                if row:
                    resolved_ids.append(row[0])
                else:
                    alias_row = con.execute("SELECT concept_id FROM concept_alias WHERE lower(alias) = ?", (norm,)).fetchone()
                    if alias_row:
                        resolved_ids.append(alias_row[0])

            if not resolved_ids:
                return []

            placeholders = ",".join("?" for _ in resolved_ids)
            sql = f"""
            SELECT dc.resource_id, d.title, COUNT(DISTINCT dc.concept_id) as matched_concepts, SUM(dc.weight) as total_weight
            FROM doc_concept dc
            JOIN document_version d ON dc.resource_id = d.resource_id
            WHERE dc.concept_id IN ({placeholders})
            GROUP BY dc.resource_id
            ORDER BY matched_concepts DESC, total_weight DESC
            LIMIT ?
            """
            rows = con.execute(sql, (*resolved_ids, limit)).fetchall()
            docs = []
            for res_id, title, matches, weight in rows:
                docs.append({
                    "resource_id": res_id,
                    "title": title,
                    "matched_concepts": matches,
                    "total_weight": round(weight, 3),
                })
            return docs
        finally:
            con.close()

    def read_passages(self, resource_id: str, concept_id: str | None = None) -> dict[str, Any]:
        con = sqlite3.connect(self.db_path)
        try:
            # 1. Fetch document metadata
            doc_row = con.execute(
                """
                SELECT d.title, c.text_path
                FROM document_version d
                JOIN content c ON d.content_id = c.content_id
                WHERE d.resource_id = ?
                LIMIT 1
                """,
                (resource_id,),
            ).fetchone()

            if not doc_row:
                return {"error": f"Document not found: {resource_id}"}

            title, text_path = doc_row

            # 2. Fetch concept associations and evidence quotes
            sql = "SELECT c.name, dc.weight, dc.relation_kind, dc.evidence FROM doc_concept dc JOIN concept c ON dc.concept_id = c.concept_id WHERE dc.resource_id = ?"
            params = [resource_id]
            if concept_id:
                sql += " AND dc.concept_id = ?"
                params.append(concept_id)
            sql += " ORDER BY dc.weight DESC LIMIT 15"

            assocs = con.execute(sql, params).fetchall()

            evidence_items = []
            for name, weight, r_kind, ev in assocs:
                evidence_items.append({
                    "concept": name,
                    "weight": round(weight, 3),
                    "kind": r_kind,
                    "evidence": ev,
                })

            # 3. Read snippet from text_path if available
            text_snippet = None
            if text_path and Path(text_path).exists():
                try:
                    with open(text_path, "r", encoding="utf-8", errors="replace") as f:
                        text_snippet = f.read(2500)
                except Exception as e:
                    text_snippet = f"Error reading text: {e}"

            return {
                "resource_id": resource_id,
                "title": title,
                "text_path": text_path,
                "text_snippet": text_snippet,
                "evidence": evidence_items,
            }
        finally:
            con.close()

def main():
    server = McpServer()

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except Exception as e:
            logger.error("Malformed JSON-RPC: %s", e)
            continue

        method = req.get("method")
        msg_id = req.get("id")

        if method == "initialize":
            res = {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "concepts-mcp", "version": "0.1.0"},
                },
            }
            sys.stdout.write(json.dumps(res) + "\n")
            sys.stdout.flush()

        elif method == "notifications/initialized":
            # Client ack, no response
            pass

        elif method == "tools/list":
            res = {
                "jsonrpc": "2.0",
                "id": msg_id,
                "result": {"tools": get_tools_def()},
            }
            sys.stdout.write(json.dumps(res) + "\n")
            sys.stdout.flush()

        elif method == "tools/call":
            params = req.get("params", {})
            tool_name = params.get("name")
            args = params.get("arguments", {})

            try:
                if tool_name == "probe_concepts":
                    out = server.probe_concepts(
                        args.get("query", ""),
                        limit=int(args.get("limit", 10)),
                        mode=str(args.get("mode", "hybrid")),
                    )
                elif tool_name == "bridge_concepts":
                    out = server.bridge_concepts(
                        args.get("concept_a", ""),
                        args.get("concept_b", ""),
                        top_k=int(args.get("top_k", 5)),
                        mode=str(args.get("mode", "hybrid")),
                    )
                elif tool_name == "analogize_concepts":
                    out = server.analogize_concepts(
                        positive=args.get("positive", []),
                        negative=args.get("negative", []),
                        limit=int(args.get("limit", 5)),
                    )
                elif tool_name == "find_documents_by_latent":
                    out = server.find_documents_by_latent(
                        args.get("query", ""),
                        limit=int(args.get("limit", 5)),
                    )
                elif tool_name == "find_documents_by_concepts":
                    out = server.find_documents_by_concepts(args.get("concepts", []), limit=int(args.get("limit", 5)))
                elif tool_name == "read_passages":
                    out = server.read_passages(args.get("resource_id", ""), concept_id=args.get("concept_id"))
                else:
                    out = {"error": f"Unknown tool: {tool_name}"}

                text_result = json.dumps(out, indent=2)
                res = {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": {
                        "content": [{"type": "text", "text": text_result}],
                        "isError": "error" in out if isinstance(out, dict) else False,
                    },
                }
            except Exception as e:
                logger.error("Tool execution failed: %s", e)
                res = {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "result": {
                        "content": [{"type": "text", "text": f"Error: {e}"}],
                        "isError": True,
                    },
                }

            sys.stdout.write(json.dumps(res) + "\n")
            sys.stdout.flush()

        else:
            if msg_id is not None:
                res = {
                    "jsonrpc": "2.0",
                    "id": msg_id,
                    "error": {"code": -32601, "message": f"Method not found: {method}"},
                }
                sys.stdout.write(json.dumps(res) + "\n")
                sys.stdout.flush()

if __name__ == "__main__":
    main()
