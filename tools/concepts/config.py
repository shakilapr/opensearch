from __future__ import annotations

import os
from pathlib import Path

# Paths
DEFAULT_DB_PATH = Path("/var/lib/search/meta/meta.sqlite")
DEFAULT_TEXT_DIR = Path("/var/lib/search/text")
DEFAULT_EMBED_CACHE_DIR = Path("/var/lib/search/fastembed")
DEFAULT_LLM_CACHE_DIR = Path("/var/lib/search/llm-cache")
SEARCH_LLM_ENV_FILE = Path("/etc/search/search-llm.env")

def load_env_file(path: Path) -> dict[str, str]:
    """Parse key=value pairs from an env file, ignoring comments."""
    env = {}
    if path.exists():
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            line = line.strip()
            if line and not line.startswith("#") and "=" in line:
                k, _, v = line.partition("=")
                env[k.strip()] = v.strip().strip("\"'")
    return env

# Check if search-llm.env exists (OpenRouter verified free tier)
_search_env = load_env_file(SEARCH_LLM_ENV_FILE)

# LLM Configuration: prioritizes environment, then /etc/search/search-llm.env (OpenRouter Free), then RelayCloud
DEFAULT_BASE_URL = os.environ.get(
    "SEARCH_LLM_BASE_URL",
    _search_env.get("SEARCH_LLM_BASE_URL", "https://openrouter.ai/api/v1")
)
DEFAULT_MODEL = os.environ.get(
    "SEARCH_LLM_MODEL",
    _search_env.get("SEARCH_LLM_MODEL", "nvidia/nemotron-3-ultra-550b-a55b:free")
)
DEFAULT_TIMEOUT_S = int(os.environ.get("SEARCH_LLM_TIMEOUT", "60"))

# Free model fallback chain (all pricing.prompt == 0, pricing.completion == 0)
DEFAULT_FALLBACK_MODELS = [
    "nvidia/nemotron-3-ultra-550b-a55b:free",
    "openrouter/free",
    "thinkingmachines/inkling:free",
    "nvidia/nemotron-3.5-lightning:free",
]

# Threshold for semantic deduplication / merging (cosine similarity)
MERGE_SIMILARITY_THRESHOLD = float(os.environ.get("CONCEPT_MERGE_THRESHOLD", "0.88"))

def get_api_key() -> str:
    """Retrieve API key: env var -> /etc/search/search-llm.env -> Hermes config."""
    # 1. Direct environment
    key = os.environ.get("SEARCH_LLM_API_KEY")
    if key and key.strip():
        return key.strip()

    # 2. Search engine free models file (/etc/search/search-llm.env)
    if _search_env.get("SEARCH_LLM_API_KEY"):
        return _search_env["SEARCH_LLM_API_KEY"]

    # 3. Hermes config fallback
    hermes_cfg = Path("/root/.hermes/config.yaml")
    if hermes_cfg.exists():
        try:
            import yaml
            cfg = yaml.safe_load(hermes_cfg.read_text(encoding="utf-8"))
            providers = cfg.get("providers", {})
            for p_name in ["relaycloud", "crowbot", "apizio"]:
                p_cfg = providers.get(p_name, {})
                if p_cfg.get("api_key"):
                    return p_cfg["api_key"]
        except Exception:
            pass

    raise RuntimeError("No API key found. Set SEARCH_LLM_API_KEY or configure /etc/search/search-llm.env.")
