import urllib.request
import json

url = "https://openrouter.ai/api/v1/models"
req = urllib.request.Request(url, headers={"Authorization": "Bearer sk-or-v1-REDACTED"})
with urllib.request.urlopen(req, timeout=10) as r:
    data = json.loads(r.read().decode("utf-8"))

free_models = []
for m in data.get("data", []):
    pricing = m.get("pricing", {})
    p_prompt = str(pricing.get("prompt", "1"))
    p_compl = str(pricing.get("completion", "1"))
    if (p_prompt == "0" or p_prompt == "0.0") and (p_compl == "0" or p_compl == "0.0"):
        free_models.append((m["id"], m.get("context_length", 0)))

print(f"Total free models found: {len(free_models)}")
for mid, ctx in sorted(free_models, key=lambda x: x[0]):
    print(f"  {mid:50} | ctx: {ctx}")
