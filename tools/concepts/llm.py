from __future__ import annotations

import hashlib
import json
import logging
import re
import urllib.request
import urllib.error
from pathlib import Path
from typing import Any

from .config import DEFAULT_BASE_URL, DEFAULT_MODEL, DEFAULT_TIMEOUT_S, DEFAULT_LLM_CACHE_DIR, DEFAULT_FALLBACK_MODELS, get_api_key

logger = logging.getLogger("concepts.llm")

def repair_json(raw: str) -> dict[str, Any] | list[Any] | None:
    """Attempt standard parse, or repair truncated JSON by closing open brackets or recovering items."""
    s = raw.strip()
    if not s:
        return None

    # Strip markdown codeblocks
    if s.startswith("```json"):
        s = s[7:]
    elif s.startswith("```"):
        s = s[3:]
    if s.endswith("```"):
        s = s[:-3]
    s = s.strip()

    try:
        return json.loads(s)
    except json.JSONDecodeError:
        pass

    # Method 2: If truncated inside a JSON object/array, find last valid closing brace
    last_brace = s.rfind("}")
    if last_brace != -1:
        sub = s[:last_brace + 1].strip()
        for suffix in ["]}", "]", "}", "]}"]:
            try:
                return json.loads(sub + suffix)
            except json.JSONDecodeError:
                pass

    # Method 3: Extract individual JSON objects matching {"name": ...}
    concept_pattern = re.compile(r'\{[^{}]*?"name"\s*:\s*"[^"]+?"[^{}]*?\}', re.DOTALL)
    recovered = []
    for m in concept_pattern.finditer(s):
        try:
            item = json.loads(m.group(0))
            if "name" in item:
                recovered.append(item)
        except Exception:
            continue

    if recovered:
        return {"concepts": recovered}

    return None

class LlmClient:
    def __init__(
        self,
        base_url: str = DEFAULT_BASE_URL,
        model: str = DEFAULT_MODEL,
        fallback_models: list[str] | None = None,
        api_key: str | None = None,
        cache_dir: Path = DEFAULT_LLM_CACHE_DIR,
        timeout: int = DEFAULT_TIMEOUT_S,
    ):
        self.base_url = base_url.rstrip("/")
        self.model = model
        self.fallback_models = fallback_models or DEFAULT_FALLBACK_MODELS
        self.api_key = api_key or get_api_key()
        self.cache_dir = Path(cache_dir)
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        self.timeout = timeout

    def _cache_key(self, prompt: str) -> Path:
        h = hashlib.sha256(prompt.encode("utf-8")).hexdigest()
        sub = self.cache_dir / h[:2]
        sub.mkdir(parents=True, exist_ok=True)
        return sub / f"{h}.json"

    def complete_json(self, system_prompt: str, user_prompt: str, max_tokens: int = 8192) -> dict[str, Any] | list[Any]:
        """Send chat completion request requiring JSON format, with disk caching, auto-repair, and fallback models."""
        full_prompt = f"SYSTEM: {system_prompt}\nUSER: {user_prompt}"
        cache_file = self._cache_key(full_prompt)
        if cache_file.exists():
            try:
                cached_data = json.loads(cache_file.read_text(encoding="utf-8"))
                return cached_data
            except Exception:
                pass

        # Try primary model, followed by fallback models
        models_to_try = [self.model] + [m for m in self.fallback_models if m != self.model]
        last_error = None

        for candidate_model in models_to_try:
            req_body = {
                "model": candidate_model,
                "messages": [
                    {"role": "system", "content": system_prompt + "\nIMPORTANT: Keep thinking brief. Directly produce the valid JSON output."},
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
                    "HTTP-Referer": "https://search.hexxica.space",
                    "X-Title": "Hexxica Search",
                },
            )

            try:
                with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                    data = json.loads(resp.read().decode("utf-8"))
                    choice = data["choices"][0]["message"]
                    content = (
                        choice.get("content")
                        or choice.get("reasoning_content")
                        or choice.get("reasoning")
                        or ""
                    )
                    if isinstance(content, list):
                        content = "".join(part.get("text", "") for part in content if isinstance(part, dict))
                    content = content.strip()

                    parsed = repair_json(content)
                    if parsed is None:
                        raise RuntimeError(f"Could not parse or repair JSON response ({len(content)} chars): {content[:100]}...")

                    # Cache response
                    cache_file.write_text(json.dumps(parsed, indent=2), encoding="utf-8")
                    return parsed
            except urllib.error.HTTPError as e:
                err_body = e.read().decode("utf-8", errors="replace")
                last_error = f"HTTP {e.code} ({candidate_model}): {err_body[:150]}"
                logger.warning("LLM model %s failed: %s; trying next fallback...", candidate_model, last_error)
            except Exception as e:
                last_error = f"Error ({candidate_model}): {e}"
                logger.warning("LLM model %s error: %s; trying next fallback...", candidate_model, last_error)

        raise RuntimeError(f"All free LLM models failed. Last error: {last_error}")
