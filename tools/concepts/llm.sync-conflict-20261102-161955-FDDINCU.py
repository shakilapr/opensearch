from __future__ import annotations

import hashlib
import json
import logging
import urllib.request
import urllib.error
from pathlib import Path
from typing import Any

from .config import DEFAULT_BASE_URL, DEFAULT_MODEL, DEFAULT_TIMEOUT_S, DEFAULT_LLM_CACHE_DIR, get_api_key

logger = logging.getLogger("concepts.llm")

class LlmClient:
    def __init__(
        self,
        base_url: str = DEFAULT_BASE_URL,
        model: str = DEFAULT_MODEL,
        api_key: str | None = None,
        cache_dir: Path = DEFAULT_LLM_CACHE_DIR,
        timeout: int = DEFAULT_TIMEOUT_S,
    ):
        self.base_url = base_url.rstrip("/")
        self.model = model
        self.api_key = api_key or get_api_key()
        self.cache_dir = Path(cache_dir)
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        self.timeout = timeout

    def _cache_key(self, prompt: str) -> Path:
        h = hashlib.sha256(f"{self.model}:{prompt}".encode("utf-8")).hexdigest()
        sub = self.cache_dir / h[:2]
        sub.mkdir(parents=True, exist_ok=True)
        return sub / f"{h}.json"

    def complete_json(self, system_prompt: str, user_prompt: str, max_tokens: int = 4096) -> dict[str, Any] | list[Any]:
        """Send chat completion request requiring JSON format, with disk caching."""
        full_prompt = f"SYSTEM: {system_prompt}\nUSER: {user_prompt}"
        cache_file = self._cache_key(full_prompt)
        if cache_file.exists():
            try:
                cached_data = json.loads(cache_file.read_text(encoding="utf-8"))
                return cached_data
            except Exception:
                pass

        req_body = {
            "model": self.model,
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_prompt},
            ],
            "response_format": {"type": "json_object"},
            "temperature": 0.2,
            "max_tokens": max_tokens,
        }

        req = urllib.request.Request(
            f"{self.base_url}/chat/completions",
            data=json.dumps(req_body).encode("utf-8"),
            headers={
                "Authorization": f"Bearer {self.api_key}",
                "Content-Type": "application/json",
                "User-Agent": "search-concept-pipeline/0.1",
            },
        )

        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                data = json.loads(resp.read().decode("utf-8"))
                choice = data["choices"][0]["message"]
                content = choice.get("content", "").strip()

                # Clean markdown backticks if present
                if content.startswith("```json"):
                    content = content[7:]
                if content.startswith("```"):
                    content = content[3:]
                if content.endswith("```"):
                    content = content[:-3]
                content = content.strip()

                parsed = json.loads(content)
                # Cache response
                cache_file.write_text(json.dumps(parsed, indent=2), encoding="utf-8")
                return parsed
        except urllib.error.HTTPError as e:
            err_body = e.read().decode("utf-8", errors="replace")
            raise RuntimeError(f"LLM API HTTP {e.code}: {err_body}") from e
        except Exception as e:
            raise RuntimeError(f"LLM completion error: {e}") from e
