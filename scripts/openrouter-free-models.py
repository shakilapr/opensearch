#!/usr/bin/env python3
"""Check which OpenRouter models actually answer, and print the working ones.

Why this exists: the free-model list is not a guess. OpenRouter's catalogue says which models are
free (`pricing.prompt == pricing.completion == 0`), but a listed free model can still refuse a
request — no healthy endpoint behind it, a rate limit, or a model that answers with an unparseable
body. Configuring a model that cannot answer would make the answer path fail over on every query.

So this asks each candidate for one real completion, with the same request shape the search engine
uses (`stream: false`, a small `max_tokens`), and reports per model: ok / HTTP status / the reason.
The ids that answer are printed last, comma-separated, ready for `SEARCH_LLM_MODELS`.

Usage:
  SEARCH_LLM_API_KEY=... scripts/openrouter-free-models.py            # check the built-in candidates
  SEARCH_LLM_API_KEY=... scripts/openrouter-free-models.py id1 id2    # check specific ids
  scripts/openrouter-free-models.py --list                            # catalogue only, no key needed

Exit status is 0 when at least one model answered, 1 when none did (so a caller can gate on it).
"""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.request

BASE_URL = os.environ.get("SEARCH_LLM_BASE_URL", "https://openrouter.ai/api/v1").rstrip("/")
UA = os.environ.get("SEARCH_LLM_USER_AGENT", "search-engine/0.1 (+https://search.hexxica.space)")

# Ordered candidates, largest context first: the answer path feeds retrieved passages, so a large
# context matters more than raw speed here, and every one of these is free by the catalogue's own
# pricing (verified against /models, which is public).
CANDIDATES = [
    "nvidia/nemotron-3-ultra-550b-a55b:free",
    "thinkingmachines/inkling:free",
    "google/gemma-4-31b-it:free",
    "nvidia/nemotron-3-super-120b-a12b:free",
    "google/gemma-4-26b-a4b-it:free",
    "openrouter/free",
    "liquid/lfm-2.5-2.6b:free",
]


def free_catalogue() -> list[tuple[int, str]]:
    """Every model OpenRouter prices at zero, largest context first."""
    req = urllib.request.Request(
        f"{BASE_URL}/models", headers={"User-Agent": UA, "Accept": "application/json"}
    )
    with urllib.request.urlopen(req, timeout=60) as resp:
        payload = json.loads(resp.read())
    out: list[tuple[int, str]] = []
    for model in payload.get("data", []):
        pricing = model.get("pricing") or {}
        try:
            if float(pricing.get("prompt", 1)) != 0.0 or float(pricing.get("completion", 1)) != 0.0:
                continue
        except (TypeError, ValueError):
            continue
        out.append((int(model.get("context_length") or 0), str(model.get("id"))))
    return sorted(out, reverse=True)


def answers(model: str, key: str) -> tuple[bool, str]:
    """Ask `model` for one short completion. Returns (answered, human reason)."""
    body = json.dumps(
        {
            "model": model,
            "messages": [{"role": "user", "content": "Reply with the single word: OK"}],
            "max_tokens": 16,
            "stream": False,
        }
    ).encode()
    req = urllib.request.Request(
        f"{BASE_URL}/chat/completions",
        data=body,
        headers={
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
            "User-Agent": UA,
        },
    )
    started = time.time()
    try:
        with urllib.request.urlopen(req, timeout=90) as resp:
            payload = json.loads(resp.read())
    except urllib.error.HTTPError as e:
        detail = e.read()[:160].decode("utf-8", "replace").replace("\n", " ")
        return False, f"HTTP {e.code} in {time.time() - started:.1f}s: {detail}"
    except Exception as e:  # noqa: BLE001 - any transport failure means "not usable"
        return False, f"{type(e).__name__} in {time.time() - started:.1f}s: {str(e)[:120]}"

    choices = payload.get("choices") or []
    content = ((choices[0] if choices else {}).get("message") or {}).get("content")
    if not content or not str(content).strip():
        return False, f"empty content in {time.time() - started:.1f}s"
    usage = payload.get("usage") or {}
    return True, (
        f"ok in {time.time() - started:.1f}s "
        f"(in {usage.get('prompt_tokens')}, out {usage.get('completion_tokens')})"
    )


def main(argv: list[str]) -> int:
    if "--list" in argv:
        for ctx, model in free_catalogue():
            print(f"{ctx:>9}  {model}")
        return 0

    candidates = [a for a in argv[1:] if not a.startswith("-")] or CANDIDATES
    key = (os.environ.get("SEARCH_LLM_API_KEY") or os.environ.get("OPENROUTER_API_KEY") or "").strip()
    if not key:
        print(
            "No credential: set SEARCH_LLM_API_KEY (or OPENROUTER_API_KEY) to a working "
            "OpenRouter key. Nothing can be checked without one.",
            file=sys.stderr,
        )
        return 1

    working: list[str] = []
    for model in candidates:
        ok, reason = answers(model, key)
        print(f"{'OK     ' if ok else 'FAILED '} {model}  {reason}")
        if ok:
            working.append(model)

    print()
    if working:
        print(f"{len(working)}/{len(candidates)} answered. Use these, in this order:")
        print(",".join(working))
        return 0
    print(f"0/{len(candidates)} answered — none of these can serve the answer path.")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
