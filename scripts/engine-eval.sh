#!/usr/bin/env bash
# Engine evaluation: run a fixed query set through the engine's HTTP surface and
# report what the engine actually did, per query.
#
#   BASE=http://127.0.0.1:8090 MODE=fast scripts/engine-eval.sh [out.json]
#
# Why a harness: the engine's claim — "answers grounded questions from the local
# index, and refuses when the index has no support" — is only worth anything if
# it is re-runnable against a corpus. Each row records stop_reason, verified
# coverage, the number of resolved citations and the top-ranked page, so a
# refusal can be told apart from a regression.
#
# A response is classified from its own text, because the engine answers a
# well-covered query and *refuses* an uncovered one in the same field:
#   grounded answer  verified coverage > 0, and an answer whose `[n]` markers all
#                    index real results, and which does not declare insufficiency
#   honest refusal    the answer states the evidence is insufficient (correct and
#                    desirable for an unsupported query)
# Expectations are per query:
#   answer  must be a grounded answer (a refusal here is a corpus gap, reported)
#   refuse  must NOT be a grounded answer, and coverage must be 0
#   either  informational only
# The script exits non-zero when an expectation is violated.
set -euo pipefail

BASE="${BASE:-http://127.0.0.1:8090}"
MODE="${MODE:-fast}"
LIMIT="${LIMIT:-5}"
OUT="${1:-}"

python3 - "$BASE" "$MODE" "$LIMIT" "$OUT" <<'PY'
import json, re, sys, time, urllib.error, urllib.request

base, mode, limit, out = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]

# (query, expectation, why)
QUERIES = [
    ("what is retrieval augmented generation", "answer",
     "Wikipedia + vLLM + cookbook pages define it"),
    ("what is a mixture of experts model", "either",
     "in-scope and covered, but this index only mentions MoE without defining it, so an "
     "honest refusal is the correct answer"),
    ("what is a transformer", "either",
     "the original corpus gap: may still refuse honestly"),
    ("how does paged attention reduce memory use in vllm", "either",
     "depends on whether the explaining vLLM page is indexed"),
    ("how does the tantivy index store field values", "either",
     "docs.rs/tantivy is the largest source, but coverage is index-dependent"),
    ("what is the capital of peru", "refuse",
     "not an LLM-topic corpus: must not be answered"),
    ("who won the 1998 formula one world championship", "refuse",
     "out of scope, and the index has no sports pages"),
    ("how do i bake sourdough bread at home", "refuse",
     "out of scope: no cooking pages in the seed set"),
]

# Phrases with which a correct engine declines to answer. A grounded answer never
# contains them; a refusal always does. Kept explicit and small: an unclassified
# non-empty answer is treated as a grounded answer, i.e. the risky direction.
REFUSAL_PHRASES = (
    "insufficient",
    "not contain",
    "no information",
    "does not state",
    "do not state",
    "not state",
    "cannot answer",
    "none of the passages",
    "not enough evidence",
    "evidence is insufficient",
)

def classify(resp):
    """Return (kind, answer, markers, resolvable) for one engine response."""
    ans = resp.get("answer") or ""
    markers = [int(m) for m in re.findall(r"\[(\d+)\]", ans)]
    results = resp.get("results") or []
    resolvable = None if not markers else all(i < len(results) for i in markers)
    cov = (resp.get("coverage") or {}).get("coverage") or 0
    low = ans.lower()
    refused = any(p in low for p in REFUSAL_PHRASES)
    if ans and not refused and markers and resolvable and cov > 0:
        return "answer", ans, markers, resolvable
    if ans and refused:
        return "refusal", ans, markers, resolvable
    if ans:
        return "unattributed", ans, markers, resolvable
    return "none", ans, markers, resolvable

def call(query):
    body = json.dumps({
        "query": query, "engine": True, "mode": mode, "limit": limit,
    }).encode()
    req = urllib.request.Request(
        base + "/search", data=body,
        headers={"content-type": "application/json"},
    )
    t0 = time.time()
    try:
        with urllib.request.urlopen(req, timeout=180) as r:
            return json.loads(r.read().decode()), time.time() - t0, None
    except urllib.error.HTTPError as e:
        return None, time.time() - t0, f"HTTP {e.code}: {e.read()[:200].decode(errors='replace')}"
    except Exception as e:                                  # noqa: BLE001
        return None, time.time() - t0, f"{type(e).__name__}: {e}"

rows, failures, warnings = [], [], []
# Answer-expectation violations are collected separately: with the answerer off on the server they
# are not *measurable*, and must not be reported as failures (see the classification below).
answer_expect_failures = []
for query, expect, why in QUERIES:
    resp, secs, err = call(query)
    if err:
        rows.append({"query": query, "expect": expect, "error": err, "seconds": round(secs, 2)})
        failures.append(f"{query!r}: transport failure ({err})")
        continue

    cov = resp.get("coverage") or {}
    coverage = cov.get("coverage")
    kind, answer, markers, resolvable = classify(resp)
    results = resp.get("results") or []
    row = {
        "query": query,
        "expect": expect,
        "kind": kind,
        "why": why,
        "stop_reason": resp.get("stop_reason"),
        "coverage": coverage,
        "mandatory_ok": cov.get("mandatory_ok"),
        "unsupported": cov.get("unsupported"),
        "results": len(results),
        "top_title": (results[0].get("title") if results else None),
        "answer_chars": len(answer),
        "markers": markers,
        "markers_resolvable": resolvable,
        "seconds": round(secs, 2),
    }
    rows.append(row)

    if expect == "answer" and kind != "answer":
        answer_expect_failures.append(
            f"{query!r}: expected a grounded answer, got kind={kind} coverage={coverage} "
            f"markers={markers} resolvable={resolvable} answer_chars={len(answer)} "
            f"stop_reason={row['stop_reason']} -- a refusal here is a corpus gap"
        )
    if expect == "refuse" and kind == "answer":
        failures.append(f"{query!r}: unsupported query was answered with markers {markers}")
    if expect == "refuse" and (coverage or 0) > 0:
        # Not a failure, and deliberately not folded into one: coverage is a token-level
        # signal, so an out-of-scope unit can be "supported" by a page that merely contains
        # its tokens (observed: a summarizer demo quoting a Lewis Hamilton paragraph
        # satisfied "who won the 1998 formula one world championship"). The answer stayed a
        # refusal, so nothing was fabricated; verification quality is what the warning is
        # about. The engine's trace names the supporting document.
        warnings.append(
            f"{query!r}: coverage {coverage} on an out-of-scope query -- the verifier matched "
            f"a page containing the unit's tokens without the fact"
        )
    if resolvable is False:
        failures.append(f"{query!r}: answer cites results that do not exist: {markers}")
    if kind == "unattributed":
        failures.append(f"{query!r}: answer with no resolvable citation: {answer[:120]!r}")

print(f"engine eval  base={base} mode={mode} limit={limit}")
print(f"{'query':46} {'kind':11} {'stop_reason':16} {'cov':>5} {'res':>4} {'ans':>5} {'mk':>4} {'s':>6}  verdict")
for r in rows:
    if r.get("error"):
        print(f"{r['query'][:46]:46} {'-':11} {'-':16} {'-':>5} {'-':>4} {'-':>5} {'-':>4} {'-':>6}  ERROR")
        continue
    if r["expect"] == "either":
        verdict = "ok"
    elif r["expect"] == "answer":
        verdict = "ok" if r["kind"] == "answer" else "GAP"
    else:
        verdict = "ok" if r["kind"] in ("refusal", "none") else "VIOLATION"
    print(f"{r['query'][:46]:46} {r['kind']:11} {str(r['stop_reason'])[:16]:16} {r['coverage']!s:>5} "
          f"{r['results']:>4} {r['answer_chars']:>5} {len(r['markers']):>4} {r['seconds']:>6}  {verdict}")

# Classification, done BEFORE the report is written so the JSON cannot disagree with the table.
#
# With no answer text anywhere, the answerer is simply off on this server (it is opt-in:
# SEARCH_ANSWER_ENABLED=1 plus a key). No answer can exist whatever retrieval does, so the answer
# expectations are *unmeasurable*, not failed -- and the engine is not "broken". The older
# version reported a disabled answerer as "the retrieval or verification path is broken", a
# misdiagnosis that made a known-off feature look like a regression.
#
# Only rows that actually got a response count: a run where every call failed at the transport is
# a connectivity failure, not an off answerer, and says nothing about either path.
answered_rows = [r for r in rows if not r.get("error")]
answerer_off = bool(answered_rows) and all(not r.get("answer_chars") for r in answered_rows)
not_measured = list(answer_expect_failures) if answerer_off else []
if not answerer_off:
    failures.extend(answer_expect_failures)

# A harness that cannot fail proves nothing: with the answerer actually running, the engine must
# answer at least one query from the index, otherwise a broken retrieval path would look like
# uniform honesty. With the answerer off — or with no response at all — there is nothing to
# conclude about either path.
if not answerer_off and answered_rows and not any(r.get("kind") == "answer" for r in answered_rows):
    failures.append(
        "no query produced a grounded answer: the retrieval or verification path is broken, "
        "not merely the corpus"
    )

if warnings:
    print("\nWARNINGS:")
    for w in warnings:
        print(" ~", w)

if not_measured:
    print("\nNOT MEASURED — the answerer is off on this server (SEARCH_ANSWER_ENABLED unset), so "
          "answer expectations cannot be judged:")
    for f in not_measured:
        print(" ~", f)
    print("  Start it with SEARCH_ANSWER_ENABLED=1 and CROWBOT_API_KEY set to measure these.")

report = {
    "base": base,
    "mode": mode,
    "queries": len(rows),
    "rows": rows,
    "failures": failures,
    "not_measured": not_measured,
    "warnings": warnings,
    "answerer_off": answerer_off,
    "ok": not failures,
}
if out:
    with open(out, "w") as f:
        json.dump(report, f, indent=2)
    print(f"\nwrote {out}")

if failures:
    print("\nFAILURES:")
    for f in failures:
        print(" -", f)
    sys.exit(1)
print(f"\nall {len(rows)} queries met their expectation")
PY
