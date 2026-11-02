import urllib.request
import json
import time

url = "https://openrouter.ai/api/v1/chat/completions"
api_key = "sk-or-v1-REDACTED"

models = [
    "poolside/laguna-s-2.1:free",
    "google/gemma-4-26b-a4b-it:free",
    "nvidia/nemotron-3-ultra-550b-a55b:free",
    "nvidia/nemotron-3.5-lightning:free",
    "inclusionai/ling-3.1-flash",
]

for model in models:
    t0 = time.time()
    data = json.dumps({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a JSON assistant. Always respond strictly with valid JSON."},
            {"role": "user", "content": "Extract concepts from: 'Rust is a systems programming language focusing on safety and concurrency.' Return JSON with key 'concepts' containing a list of {name, kind}."},
        ],
        "response_format": {"type": "json_object"},
        "temperature": 0.1,
        "max_tokens": 1024,
    }).encode("utf-8")

    req = urllib.request.Request(
        url,
        data=data,
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
            "HTTP-Referer": "https://search.hexxica.space",
            "X-Title": "Hexxica Search",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            res = json.loads(r.read().decode("utf-8"))
            msg = res["choices"][0]["message"]
            content = msg.get("content", "") or ""
            dt = round(time.time() - t0, 2)
            print(f"[{model}] {dt}s -> {len(content)} chars: {content[:100]}...")
    except Exception as e:
        print(f"[{model}] failed: {e}")
