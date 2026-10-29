# Answer generation (`search-answer`)

Query-time answer generation is an **opt-in** stage that runs after search, on the evidence the
retrieval loop already selected. It is implemented by the `search-answer` crate
(`crates/answer`), which implements `search_engine::Answerer` and is wired into `agent0 serve`.
When it is off, or when it fails, search behaves exactly as before.

## Enable it

Answer generation is disabled unless **both** conditions hold:

1. `SEARCH_ANSWER_ENABLED` is truthy (`1`, `true`, `on`, `yes`); and
2. `CROWBOT_API_KEY` is set and non-empty.

`search_answer::from_env()` then returns `Some(LlmAnswerer)`; otherwise it returns `None` and the
caller injects no answerer. `agent0 serve` logs exactly one info line naming the reason
(feature disabled vs. missing key).

## Environment variables

| Variable | Default | Meaning |
| --- | --- | --- |
| `SEARCH_ANSWER_ENABLED` | unset (off) | Opt-in switch. Unset/`0` = disabled. |
| `CROWBOT_API_KEY` | — | Credential. Read **only** from the environment; never logged or written to disk. |
| `SEARCH_ANSWER_MODEL` | `deepseek-v4.1-flash` | Model id. |
| `SEARCH_ANSWER_BASE_URL` | `https://api.crowbot.sh/v1` | OpenAI-chat-compatible endpoint. |
| `SEARCH_ANSWER_TIMEOUT_MS` | `20000` | Per-request HTTP timeout on the search hot path. |
| `SEARCH_ANSWER_MAX_TOKENS` | `1024` | Completion token budget. |
| `SEARCH_ANSWER_CONCURRENCY` | `4` | Bound on simultaneous in-flight requests. |
| `SEARCH_LLM_CACHE` | `$SEARCH_DATA_DIR/llm-cache` (else `/var/lib/search/llm-cache`) | Cache directory (same convention as `search-organize`). |
| `SEARCH_LLM_FALLBACK_BASE_URL` | `https://api.crowbot.sh/v1` | **Fallback** provider, used only when a primary call fails. |
| `SEARCH_LLM_FALLBACK_MODEL` | `deepseek-v4.1-flash` | Model id asked of the fallback provider. |
| `SEARCH_LLM_FALLBACK_API_KEY` / `CROWBOT_API_KEY` | — | Fallback credential. **Unset = no fallback** (primary only). |
| `SEARCH_LLM_USER_AGENT` | `search-engine/0.1 (+https://github.com/agent-native-search)` | Sent with every request. Cosmetic for the providers in use — crowbot and the apizio relay both accept a request with or without one (measured 2026-10-13) — but kept because a Cloudflare-fronted relay can reject a request carrying none: the former cun.ai fallback answered HTTP 403 (Cloudflare 1010) until one was sent. |

## Model chain and fallback provider

A request walks an ordered **chain** of models: those listed in `SEARCH_LLM_MODELS` first, then the
fallback provider. The deployment runs Sonar (Perplexity's search model, through the apizio relay)
first and crowbot's `deepseek-v4.1-flash` — **the default model for this project** — as the last
resort. When an attempt **fails** — connection refused, timeout, a non-2xx response, or an
unparseable body — the request moves to the next model with its **own** model id, and a `WARN`
names the provider, the model and the cause. Every attempt is recorded as a `LlmAttempt`, so the
response (and the UI's status strip) can say which models were tried, which failed and why, and
which one answered — "no answer" is never indistinguishable from "every model was down". A
successful call never touches the rest of the chain, so the later models cost nothing in the normal
case. The policy lives in `search_answer::ChainTransport` and is exercised by hermetic fakes.
`search-organize` implements the same policy for the organizer and the query planner, so
structuring and planning fall over identically. With no chain and no fallback credential
configured, the single configured provider is the only one tried, exactly as before.

A reasoning model (id containing `thinking`/`reason`/`r1`/`o1`)
spends part of its budget on a hidden trace, so
`max_tokens` is raised to at least **8192** and the timeout to at least **120000 ms** — an
explicit opt-in to a slower call. Such a model also gets at most one attempt; there are no
retries (a retry would only double hot-path latency).

## Prompt budget

The single prompt is assembled from the query, the evidence records and the passages, under hard
caps (constants in `crates/answer/src/prompt.rs`):

* **at most 8 passages** (`MAX_PASSAGES`);
* **at most 1000 characters per passage** (`MAX_CHARS_PER_PASSAGE`), truncated not dropped;
* **at most 24 evidence records** (`MAX_EVIDENCE_RECORDS`);
* **at most 8000 characters total** (`MAX_PROMPT_CHARS`) — never exceeded on the wire.

The model is instructed to answer **only** from the supplied evidence, to say so plainly when the
evidence is insufficient, and to cite the passage indices it used. The prompt text carries a
version (`PROMPT_VERSION = "answer-v1"`) that is part of the cache key.

## Cost and latency

One completion per uncached `(query, evidence, passages)` request, bounded by
`SEARCH_ANSWER_TIMEOUT_MS` and `SEARCH_ANSWER_MAX_TOKENS`, with at most
`SEARCH_ANSWER_CONCURRENCY` calls in flight. A cached request costs nothing and performs no
network call.

## Cache

One file per `(model, prompt version, prompt)`, keyed by `sha256(model \0 version \0 prompt)`, at
`<cache>/<first two hex>/<hash>.json`, holding the answer. Writes are atomic (temp file + rename),
so a partial write can never poison the cache, and a corrupt entry degrades to a fresh call
rather than a hard error. `LlmAnswerer::cache_stats()` reports hits, misses and entry count.

## Failure is never fatal

On a transport error, timeout, empty/whitespace content, or `finish_reason=length` with null
content, `answer` returns a short, non-secret `Err(String)`. The engine ignores that error, so a
search can never fail because answer generation failed.
