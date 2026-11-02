from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import Any

from .llm import LlmClient

logger = logging.getLogger("concepts.decompose")

@dataclass
class ExtractedConcept:
    name: str
    weight: float
    kind: str  # 'mechanism', 'phenomenon', 'entity', 'relation', 'context'
    evidence: str

DECOMPOSE_SYSTEM_PROMPT = """You are an expert ontological knowledge graph engineer.
Your task is to exhaustively extract all concepts, mechanisms, entities, phenomena, molecules, states, and contextual themes present in the provided article.

Guidelines:
1. Be as granular and exhaustive as possible. Extract fine-grained concepts (e.g. "oxytocin receptor downregulation", "dopaminergic reward pathway", "ventral tegmental area activation", "limerence", "pair bonding") rather than just broad categories.
2. For each concept, assign:
   - "name": Concise canonicalized English phrase (2-5 words preferred).
   - "weight": Salience from 0.10 (minor passing mention) to 1.00 (central core thesis of the document).
   - "kind": One of ["mechanism", "phenomenon", "entity", "relation", "context"].
   - "evidence": A short direct excerpt (1-2 sentences) from the text providing evidence.
3. Output MUST be valid JSON with the top-level key "concepts" holding an array of concept objects:
{
  "concepts": [
    {
      "name": "oxytocin receptor",
      "weight": 0.90,
      "kind": "entity",
      "evidence": "Oxytocin receptors in the nucleus accumbens mediate attachment behaviors."
    }
  ]
}
"""

class ConceptDecomposer:
    def __init__(self, llm_client: LlmClient | None = None):
        self.client = llm_client or LlmClient()

    def decompose(self, title: str, text: str, max_chars: int = 16000) -> list[ExtractedConcept]:
        """Extract fine-grained concepts from an article text."""
        # Clean text
        clean_text = " ".join(text.split())
        if not clean_text:
            return []

        # Truncate or chunk if excessively long
        sample_text = clean_text[:max_chars]

        user_prompt = f"TITLE: {title}\n\nTEXT:\n{sample_text}\n\nExtract all granular concepts, mechanisms, and entities from this text in the required JSON format."

        try:
            res = self.client.complete_json(DECOMPOSE_SYSTEM_PROMPT, user_prompt, max_tokens=4096)
        except Exception as e:
            logger.error("Decomposition failed: %s", e)
            return []

        raw_concepts = []
        if isinstance(res, dict):
            raw_concepts = res.get("concepts", [])
        elif isinstance(res, list):
            raw_concepts = res

        results: list[ExtractedConcept] = []
        seen_names = set()

        for c in raw_concepts:
            if not isinstance(c, dict):
                continue
            name = c.get("name", "").strip()
            if not name or len(name) < 2 or len(name) > 120:
                continue

            name_norm = name.lower()
            if name_norm in seen_names:
                continue
            seen_names.add(name_norm)

            try:
                weight = float(c.get("weight", 0.5))
            except (ValueError, TypeError):
                weight = 0.5
            weight = max(0.05, min(1.0, weight))

            kind = str(c.get("kind", "concept")).lower()
            if kind not in {"mechanism", "phenomenon", "entity", "relation", "context"}:
                kind = "concept"

            evidence = str(c.get("evidence", "")).strip()

            results.append(ExtractedConcept(
                name=name,
                weight=round(weight, 3),
                kind=kind,
                evidence=evidence[:400]
            ))

        return results
