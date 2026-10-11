#!/usr/bin/env python3
"""Progress watcher: measure, improve the index model, keep the plan honest, drive the next task.

Design rules, learned the hard way:

  * MEASURE BEFORE ACTING. Every claim this tool makes carries the number behind it, and a
    metric it could not read is reported as None/unavailable - never as a plausible value.
  * ONE DELIVERABLE PER SESSION. A coding task is dispatched to an isolated git worktree
    with an explicit read budget and a leash. A 90-minute session handed a broad brief
    once produced zero lines and had to be re-scoped.
  * VERIFY BEFORE MARKING ANYTHING DONE. A dispatched task is only `done` if a new commit
    exists on its branch AND its tests pass when re-run here. Otherwise it is `blocked`
    with the reason, and the worktree is kept for inspection.
  * NEVER DEPLOY UNGATED. The ops step (importing the fleet into the serving index) runs
    only after the same benchmark on a staging data dir meets the deployed quality.
  * NO SECRETS. Nothing here reads, prints or commits a credential; the LLM clients take
    theirs from the environment.

Usage:  python3 tools/progress_watch.py [--interval 300] [--once] [--no-dispatch]
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OPS = REPO / "ops"
PLAN = OPS / "plan.json"
LOGFILE = OPS / "watch.log"
METRICS = OPS / "progress.jsonl"
PROGRESS_MD = REPO / "docs" / "PROGRESS.md"
WORKTREES = Path("/root/.hermes/cache/scratch")
DATA = Path("/var/lib/search")
FLEET = Path("/var/lib/search-fleet")
SHARDS = [0, 1, 2, 3]
LOCAL_HEALTH = "http://127.0.0.1:8090/health"
LIVE_HEALTH = "https://search.hexxica.space/health"
CARGO = os.path.expanduser("~/.cargo/bin/cargo")
DEFAULT_MODEL = "crowbot/deepseek-v4.1-flash"  # key is in the opencode config, so the service needs no env var
RESEARCH_MODEL = "apizio/t/sonar"
DISPATCH_LEASH_S = 2700
REFERENCES = REPO / "references"
REF_SPEC = OPS / "references.json"
STATE = OPS / "state.json"
LOCK = OPS / "watch.lock"
RESEARCH_LEASH_S = 1800
CLONES_PER_CYCLE = 2
# The crawl fleet's own settings, so a diagnosis can name the ceiling it is working against.
CRAWL_CONCURRENCY = 32
CRAWL_ORIGIN_INTERVAL_MS = 250
CRAWL_ORIGIN_CAP = 250
SIDECAR_LOG = DATA / "sidecar-build.log"


def log(msg: str) -> None:
    line = f"{time.strftime('%Y-%m-%dT%H:%M:%S')} {msg}"
    print(line, flush=True)
    try:
        with LOGFILE.open("a") as fh:
            fh.write(line + "\n")
    except OSError:
        pass


def sh(cmd: str, timeout: int = 120, cwd: Path | None = None) -> tuple[int, str]:
    """Run a shell command. Never raises: a failure is a result, not an exception."""
    try:
        p = subprocess.run(
            cmd, shell=True, cwd=str(cwd or REPO), capture_output=True, text=True, timeout=timeout
        )
        return p.returncode, (p.stdout or "") + (p.stderr or "")
    except subprocess.TimeoutExpired:
        return 124, f"timed out after {timeout}s"
    except OSError as exc:
        return 127, str(exc)


def curl_json(url: str, timeout: int = 10) -> dict | None:
    rc, out = sh(f"curl -sS --max-time {timeout} {url}", timeout=timeout + 5)
    if rc != 0 or not out.strip():
        return None
    try:
        return json.loads(out)
    except json.JSONDecodeError:
        return None


# --------------------------------------------------------------------------- state


def load_state() -> dict:
    if STATE.exists():
        try:
            return json.loads(STATE.read_text())
        except (OSError, json.JSONDecodeError):
            return {}
    return {}


def save_state(state: dict) -> None:
    merged = load_state()
    merged.update(state)
    STATE.write_text(json.dumps(merged, indent=2) + "\n")


# --------------------------------------------------------------------------- metrics


def shard_docs(shard: int) -> int | None:
    logf = FLEET / f"shard-{shard}.log"
    if not logf.exists():
        return None
    rc, out = sh(f"grep -c 'ingested new document' {logf}")
    try:
        return int(out.strip().splitlines()[0])
    except (ValueError, IndexError):
        return None


def dir_bytes(path: Path) -> int | None:
    if not path.exists():
        return None
    rc, out = sh(f"du -sb {path} 2>/dev/null | cut -f1")
    try:
        return int(out.strip().splitlines()[0])
    except (ValueError, IndexError):
        return None


def pack_records(shard: int | None) -> int | None:
    if shard is None:
        man = DATA / "pack-structured" / "manifest.json"
    else:
        man = FLEET / f"pack-shard-{shard}" / "manifest.json"
    if not man.exists():
        return None
    try:
        data = json.loads(man.read_text())
    except (OSError, json.JSONDecodeError):
        return None
    for key in ("records", "record_count", "docs", "count"):
        if isinstance(data.get(key), int):
            return data[key]
    return None


def export_running() -> bool:
    rc, out = sh("pgrep -fa 'agent0 export' | grep -v pgrep | wc -l")
    try:
        return int(out.strip()) > 0
    except ValueError:
        return False


def collect() -> dict:
    local = curl_json(LOCAL_HEALTH)
    live = curl_json(LIVE_HEALTH, timeout=15)
    rc, commit = sh("git rev-parse --short HEAD")
    rc2, dirty = sh("git status --porcelain | wc -l")
    disk = shutil.disk_usage("/")
    m: dict = {
        "ts": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "commit": commit.strip() or None,
        "dirty_files": int(dirty.strip() or 0) if dirty.strip().isdigit() else None,
        "disk_free_gb": round(disk.free / 1e9, 1),
        # /health reports a `generation` and an `index_manifest_id`, not a live document count.
        # The generation equals the document count at build time (19254 for a 19255-document index),
        # so it is labelled a generation here rather than dressed up as a current doc count.
        "index_generation_local": (local or {}).get("generation"),
        "index_generation_live": (live or {}).get("generation"),
        "index_manifest_live": (live or {}).get("index_manifest_id"),
        "service_local_up": local is not None,
        "service_live_up": live is not None,
        "pack_records_main": pack_records(None),
        "llm_cache_entries": None,
        "shards": {},
        "export_running": export_running(),
    }
    prev = load_state()
    prev_shards = prev.get("shards") or {}
    prev_ts = prev.get("ts_epoch")
    now = time.time()
    try:
        m["cores"] = os.cpu_count()
        m["load1"] = round(os.getloadavg()[0], 2)
    except OSError:
        m["cores"], m["load1"] = None, None
    # Count the crawler BINARY only: pgrep -f also matches the timeout/bash wrappers around it,
    # which reported 13 "crawlers" for 4 shards.
    rc0, procs = sh("ps -eo comm,args --no-headers | awk '$1==\"agent0\" && / crawl /' | wc -l")
    m["crawler_procs"] = int(procs.strip()) if procs.strip().isdigit() else None
    rc, out = sh(f"find {DATA}/llm-cache -name '*.json' 2>/dev/null | wc -l", timeout=60)
    if out.strip().isdigit():
        m["llm_cache_entries"] = int(out.strip())
    total = 0
    for s in SHARDS:
        docs = shard_docs(s)
        entry = {
            "docs": docs,
            "raw_bytes": dir_bytes(REPO / "raw-dataset" / f"shard-{s}"),
            "pack_records": pack_records(s),
            "running": bool(
                sh(f"pgrep -fa 'crawl /var/lib/search-fleet/work/shard-{s}' | grep -v pgrep | wc -l")[1]
                .strip()
                .isdigit()
            ),
        }
        prev_docs = (prev_shards.get(str(s)) or {}).get("docs")
        if docs is not None and prev_docs is not None and prev_ts and now > prev_ts:
            entry["docs_per_min"] = round((docs - prev_docs) * 60.0 / (now - prev_ts), 1)
        if docs:
            total += docs
        m["shards"][str(s)] = entry
    m["crawl_concurrency"] = CRAWL_CONCURRENCY
    m["crawl_origin_interval_ms"] = CRAWL_ORIGIN_INTERVAL_MS
    # NOTE: computed AFTER total_docs_per_min below, or it reads None on every cycle.
    rate = None
    if rate and rate > 0:
        # With C concurrent fetches sustaining R per second, the average request occupies a slot
        # for C/R seconds. That is the measurement that says whether the box is idle because each
        # fetch is slow (timeouts, blocked hosts, politeness) or because nothing is concurrent.
        m["implied_fetch_s"] = round(CRAWL_CONCURRENCY / (rate / 60.0), 2)
    errs = 0
    seen_any = False
    for s_ in SHARDS:
        logf = FLEET / f"shard-{s_}.log"
        if not logf.exists():
            continue
        seen_any = True
        rc_e, out_e = sh(f"grep -ciE 'timed out|timeout|connection refused|too many requests|429|403' {logf}")
        if out_e.strip().isdigit():
            errs += int(out_e.strip())
    m["crawl_error_lines"] = errs if seen_any else None
    m["total_docs"] = total or None
    prev_total = prev.get("total_docs")
    if total and prev_total and prev_ts and now > prev_ts:
        m["total_docs_per_min"] = round((total - prev_total) * 60.0 / (now - prev_ts), 1)
    rate = m.get("total_docs_per_min")
    if rate and rate > 0:
        m["implied_fetch_s"] = round(CRAWL_CONCURRENCY / (rate / 60.0), 2)
    save_state({"ts_epoch": now, "shards": {k: {"docs": v.get("docs")} for k, v in m["shards"].items()},
                "total_docs": total or None})
    return m


# --------------------------------------------------------------------------- research / references


def clone_references(plan: dict, metrics: dict) -> None:
    """Clone the reference repos, a couple per cycle. Gitignored; never served, never committed."""
    if not REF_SPEC.exists():
        return
    try:
        repos = json.loads(REF_SPEC.read_text()).get("repos", [])
    except (OSError, json.JSONDecodeError) as exc:
        log(f"references: spec unreadable: {exc}")
        return
    base = REFERENCES / "repos"
    base.mkdir(parents=True, exist_ok=True)
    started = 0
    failures = []
    for r in repos:
        dest = base / r["name"]
        if dest.exists():
            continue
        if started >= CLONES_PER_CYCLE:
            break
        rc, out = sh(f"git clone --depth 1 -q {r['url']} {dest}", timeout=600, cwd=REFERENCES)
        if rc == 0:
            log(f"references: cloned {r['name']}")
        else:
            failures.append(f"{r['name']}: {out.strip()[-120:]}")
            log(f"references: clone failed for {r['name']}: {out.strip()[-160:]}")
            shutil.rmtree(dest, ignore_errors=True)
        started += 1
    it = item_by_id(plan, "references-repos")
    if not it:
        return
    present = [r["name"] for r in repos if (base / r["name"]).exists()]
    if len(present) == len(repos):
        it["status"] = "done"
        it["evidence"] = f"cloned {len(present)}/{len(repos)}: " + ", ".join(present)
    elif failures:
        it["status"] = "in_progress"
        it["reason"] = "failures so far: " + "; ".join(failures[:3])
    else:
        it["status"] = "in_progress"
        it["evidence"] = f"cloned {len(present)}/{len(repos)}"
    save_plan(plan)


def dispatch_research(plan: dict, item: dict) -> None:
    """Dispatch a research question to the browsing model, and require a written answer."""
    out_rel = item.get("output")
    if not out_rel:
        item["status"] = "blocked"
        item["reason"] = "research item has no `output` path"
        save_plan(plan)
        return
    out = REPO / out_rel
    out.parent.mkdir(parents=True, exist_ok=True)
    model = item.get("model") or RESEARCH_MODEL
    brief = f"""You are doing RESEARCH ONLY: do not modify any code in this repository. Your entire
deliverable is a written report at {out_rel} (relative to the working directory). Create that file
with your findings, then stop.

QUESTION
{item['detail']}

REQUIREMENTS
- Cite sources with links for every non-obvious claim. If you cannot verify something, say so.
- Be concrete: name techniques, parameters and tradeoffs rather than describing areas.
- End with a ranked, actionable list (what to do first and what it buys).
- No hedging generalities, no filler. Length is not a virtue; specificity is.

Write the report to {out_rel} now."""
    logf = OPS / f"research-{item['id']}.log"
    cmd = (
        f"timeout {RESEARCH_LEASH_S} opencode run --model {model} {json.dumps(brief)} "
        f"> {logf} 2>&1"
    )
    log(f"research {item['id']}: {model}, leash {RESEARCH_LEASH_S}s -> {out_rel}")
    rc, _ = sh(cmd, timeout=RESEARCH_LEASH_S + 120)
    try:
        body = out.read_text()
    except OSError:
        body = ""
    if len(body) < 1500:
        # opencode sometimes answers in the session transcript instead of using its write tool,
        # and losing a real report to that is not acceptable. Capture the reply, label where it
        # came from, and keep the content unedited.
        try:
            raw = logf.read_text()
        except OSError:
            raw = ""
        cleaned = "\n".join(
            line
            for line in raw.splitlines()
            if line.strip() and not line.lstrip().startswith(("\u2192", "\u2717", "!", ">", "```"))
        )
        if len(cleaned) >= 1500:
            header = (
                f"<!-- Captured from the opencode session transcript ({logf.name}): the model "
                f"answered inline instead of writing this file. Content is unedited. -->\n\n"
            )
            out.write_text(header + cleaned + "\n")
            body = out.read_text()
            log(f"research {item['id']}: captured an inline answer into {out_rel}")
    if len(body) >= 1500:
        first = next((l.strip() for l in body.splitlines() if l.strip().startswith("#")), "")
        item["status"] = "done"
        item["evidence"] = f"{out_rel} written ({len(body)} chars){'; ' + first if first else ''}"
        log(f"research {item['id']}: DONE ({len(body)} chars)")
    else:
        item["status"] = "blocked"
        item["reason"] = (
            f"no usable report at {out_rel} (rc={rc}, {len(body)} chars); log: {logf}"
        )
        log(f"research {item['id']}: BLOCKED - report missing or too short ({len(body)} chars)")


# --------------------------------------------------------------------------- sidecars


def refresh_sidecars(plan: dict, metrics: dict) -> None:
    """Rebuild the graph and dense sidecars when the serving index generation changes.

    Both are rebuildable from the index alone (the pack carries the text), so this is additive and
    idempotent; it is also slow (a graph build over 19k docs measured 495 s), so it runs detached and
    is collected on a later cycle by "artifact newer than the generation it was started for".
    """
    gen = metrics.get("index_generation_local") or metrics.get("index_generation_live")
    item = item_by_id(plan, "sidecar-refresh")
    if gen is None or item is None:
        return
    st = load_state()
    if st.get("sidecar_generation") == gen and st.get("sidecar_done_generation") == gen:
        if item["status"] != "done":
            item["status"] = "done"
            item["evidence"] = f"sidecars rebuilt for generation {gen}"
            save_plan(plan)
        return
    binary = REPO / "target" / "release" / "agent0"
    running = sh("pgrep -f 'agent0 (graph|embed) build' | wc -l")[1].strip()
    if running not in ("", "0"):
        return
    started = st.get("sidecar_started_ts")
    if st.get("sidecar_generation") == gen and started:
        # A build was started for this generation: does its output exist and postdate it?
        graph = DATA / "graph" / "entities.bin"
        vectors = DATA / "vectors.bin"
        try:
            fresh = graph.exists() and vectors.exists() and min(
                graph.stat().st_mtime, vectors.stat().st_mtime
            ) > started
        except OSError:
            fresh = False
        if fresh:
            save_state({"sidecar_done_generation": gen})
            item["status"] = "done"
            item["evidence"] = f"graph + dense sidecars rebuilt for generation {gen}"
            save_plan(plan)
            log(f"sidecars: rebuilt for generation {gen}")
        return
    if not binary.exists():
        return
    cmd = (
        f"cd {REPO} && SEARCH_DATA_DIR=/var/lib/search SEARCH_READ_ONLY=auto "
        f"timeout 21600 {binary} embed build >> {SIDECAR_LOG} 2>&1; "
        f"SEARCH_DATA_DIR=/var/lib/search timeout 21600 {binary} graph build >> {SIDECAR_LOG} 2>&1"
    )
    DATA.mkdir(parents=True, exist_ok=True)
    subprocess.Popen(cmd, shell=True, cwd=str(REPO), start_new_session=True)
    save_state({"sidecar_generation": gen, "sidecar_started_ts": time.time()})
    item["status"] = "in_progress"
    item["evidence"] = f"rebuilding sidecars for generation {gen} (started {metrics['ts']})"
    save_plan(plan)
    log(f"sidecars: rebuild started for generation {gen} (previous generation changed)")


# --------------------------------------------------------------------------- throughput


def throughput_guard(plan: dict, metrics: dict) -> None:
    """Watch observed throughput against the resources actually available, and act on underuse."""
    cores = metrics.get("cores")
    load = metrics.get("load1")
    procs = metrics.get("crawler_procs")
    rate = metrics.get("total_docs_per_min")  # None on the first cycle: reported, not invented
    if not cores or load is None:
        return
    idle = load < cores * 0.5
    stopped = [s for s in SHARDS if not metrics["shards"][str(s)]["running"]]
    seeded = [s for s in stopped if (FLEET / "work" / f"shard-{s}.txt").exists()]
    if idle and seeded:
        s = seeded[0]
        env = (
            f"SEARCH_DATA_DIR=/var/lib/search-fleet/shard-{s} SEARCH_CRAWL_ORIGIN_INTERVAL_MS=250 "
            f"SEARCH_CRAWL_ORIGIN_CAP=250 SEARCH_CRAWL_COMPRESS=1"
        )
        cmd = (
            f"cd {REPO} && mkdir -p /var/lib/search-fleet/shard-{s} raw-dataset/shard-{s} && "
            f"{env} timeout 86400 {REPO}/target/release/agent0 crawl "
            f"/var/lib/search-fleet/work/shard-{s}.txt --out raw-dataset/shard-{s} "
            f"--max-docs 250000 --max-urls 1000000 --concurrency 32 --compress "
            f"> /var/lib/search-fleet/shard-{s}.log 2>&1"
        )
        subprocess.Popen(cmd, shell=True, cwd=str(REPO), start_new_session=True)
        log(
            f"throughput: host idle (load1 {load} of {cores} cores, {procs} crawler processes, "
            f"rate {rate} docs/min) - restarted shard {s}"
        )
    implied = metrics.get("implied_fetch_s")
    if implied and implied > 10:
        log(
            f"throughput DIAGNOSIS: at concurrency {metrics.get('crawl_concurrency')} the observed "
            f"rate implies a mean fetch latency of {implied} s per request - the box is idle because "
            f"each fetch holds a slot for {implied} s (slow hosts, timeouts or politeness delays), "
            f"not because too few requests are in flight. Raising concurrency is the wrong fix: "
            f"measure where the time goes first ({metrics.get('crawl_error_lines')} error/timeout lines "
            f"in the shard logs)."
        )
    elif idle:
        log(
            f"throughput: host underused (load1 {load} of {cores} cores, {procs} crawler "
            f"processes, {rate} docs/min) but no stopped shard has a seed file to restart"
        )
    it = item_by_id(plan, "crawl-throughput")
    if it:
        it["status"] = "in_progress"
        it["evidence"] = (
            f"{metrics['ts']}: {rate} docs/min, load1 {load}/{cores} cores, "
            f"{procs} crawler process(es), {len(stopped)} shard(s) stopped"
        )
        save_plan(plan)


# --------------------------------------------------------------------------- plan


def load_plan() -> dict:
    return json.loads(PLAN.read_text())


def save_plan(plan: dict) -> None:
    PLAN.write_text(json.dumps(plan, indent=2) + "\n")


def unblock_deps(plan: dict) -> None:
    """A dependent item becomes actionable only when every dependency is genuinely done."""
    changed = False
    for it in plan["items"]:
        deps = it.get("deps") or []
        if it["status"] == "blocked" and deps and all(
            (item_by_id(plan, d) or {}).get("status") == "done" for d in deps
        ):
            it["status"] = "todo"
            it.pop("reason", None)
            log(f"unblocked {it['id']}: deps {', '.join(deps)} are done")
            changed = True
    if changed:
        save_plan(plan)


def item_by_id(plan: dict, item_id: str) -> dict | None:
    for it in plan["items"]:
        if it["id"] == item_id:
            return it
    return None


# --------------------------------------------------------------------------- actions


def start_structuring(plan: dict, metrics: dict) -> None:
    """Structure the fleet: export the neediest shard's index into a pack, one at a time."""
    if metrics["export_running"]:
        return
    if not (REPO / "target" / "release" / "agent0").exists():
        log("skip structuring: no release binary")
        return
    for s in SHARDS:
        info = metrics["shards"][str(s)]
        docs, recs = info.get("docs"), info.get("pack_records")
        if docs is None or docs == 0:
            continue
        if recs is not None and recs >= docs:
            continue  # this shard is already structured
        out = FLEET / f"pack-shard-{s}"
        out.mkdir(parents=True, exist_ok=True)
        env = (
            f"SEARCH_DATA_DIR=/var/lib/search-fleet/shard-{s} "
            f"SEARCH_LLM_CACHE=/var/lib/search/llm-cache SEARCH_LLM_CONCURRENCY=32 "
        )
        cmd = (
            f"{env}timeout 21600 {REPO}/target/release/agent0 export {out} "
            f"> {FLEET}/pack-shard-{s}.log 2>&1"
        )
        subprocess.Popen(cmd, shell=True, cwd=str(REPO), start_new_session=True)
        log(f"structuring: started export for shard {s} (indexed {docs}, pack records {recs})")
        it = item_by_id(plan, "fleet-structuring")
        if it:
            it["status"] = "in_progress"
            it["evidence"] = f"{metrics['ts']}: export started for shard {s} (indexed {docs})"
        save_plan(plan)
        return
    it = item_by_id(plan, "fleet-structuring")
    if it and it["status"] != "done":
        done = [s for s in SHARDS if (metrics["shards"][str(s)].get("pack_records") or 0) > 0]
        if len(done) == len(SHARDS):
            it["status"] = "done"
            it["evidence"] = "all shards structured: " + ", ".join(
                f"shard {s} {metrics['shards'][str(s)]['pack_records']} records" for s in SHARDS
            )
            save_plan(plan)
            log("structuring: all shards structured")


def verify_and_merge(plan: dict, item: dict, wt: Path, branch: str, rc: int) -> None:
    """Re-run the crate tests HERE and merge. A child's success is a self-report until then."""
    rc2, newc = sh(f"git -C {wt} log --oneline {plan.get('_base', 'HEAD')}..HEAD")
    commits = [l for l in newc.strip().splitlines() if l.strip()]
    if not commits:
        item["status"] = "blocked"
        item["reason"] = f"no commit in {wt} (rc={rc}); worktree kept for inspection"
        save_plan(plan)
        log(f"verify {item['id']}: BLOCKED - no commit produced")
        return
    crates = item.get("test_crates", [])
    if not crates:
        rc3, out3 = sh(
            f"git -C {wt} diff --name-only {plan.get('_base', 'HEAD')}..HEAD | "
            f"sed -n 's#^crates/\\([^/]*\\)/.*#\\1#p' | sort -u"
        )
        crates = [c.strip() for c in out3.splitlines() if c.strip()]
    pkgs = " ".join(f"-p {c}" if c.startswith("search-") else f"-p search-{c}" for c in crates)
    rc4, out4 = sh(
        f"cd {wt} && PATH={os.path.dirname(CARGO)}:$PATH timeout 1500 {CARGO} test {pkgs} 2>&1 | tail -40",
        timeout=1600,
    )
    summary = "; ".join(l.strip() for l in out4.splitlines() if "test result:" in l) or out4[-200:]
    if "test result: FAILED" in out4 or rc4 != 0:
        item["status"] = "blocked"
        item["reason"] = f"tests failed on {wt}: {summary[:400]}"
        save_plan(plan)
        log(f"verify {item['id']}: BLOCKED - tests failed: {summary[:200]}")
        return
    rc5, out5 = sh(f"cd {REPO} && git merge --no-edit {branch} 2>&1 | tail -3")
    if rc5 != 0:
        item["status"] = "blocked"
        item["reason"] = f"merge failed (worktree {wt} kept): {out5.strip()[:300]}"
        save_plan(plan)
        log(f"verify {item['id']}: BLOCKED - merge failed")
        return
    rc6, head = sh("git rev-parse --short HEAD")
    item["status"] = "done"
    item["evidence"] = f"{commits[-1].split()[0]} merged as {head.strip()}; tests: {summary[:200]}"
    save_plan(plan)
    log(f"verify {item['id']}: DONE - {item['evidence']}")


def dispatch_code(plan: dict, item: dict) -> None:
    """Dispatch one coding task to opencode in an isolated worktree, then verify it here."""
    wid = f"wt-{item['id'][:18]}"
    wt = WORKTREES / wid
    branch = wid
    model = item.get("model") or DEFAULT_MODEL
    if wt.exists():
        # A worktree left by an interrupted cycle must not strand the item forever: if it holds
        # commits, verify and merge them; if it holds nothing, remove it so the item can retry.
        rc2, newc = sh(f"git -C {wt} log --oneline {plan.get('_base', 'HEAD')}..HEAD")
        if [l for l in newc.strip().splitlines() if l.strip()]:
            log(f"dispatch {item['id']}: recovering commits already in {wt}")
            verify_and_merge(plan, item, wt, branch, 0)
        elif sh(f"pgrep -fa 'opencode run' | grep -v pgrep | wc -l")[1].strip() not in ("", "0"):
            log(f"dispatch {item['id']}: {wt} exists and a session is running; waiting")
        else:
            log(f"dispatch {item['id']}: stale empty worktree; removing so the item can retry")
            sh(f"git worktree remove --force {wt}; git branch -D {branch}")
        return
    rc, out = sh(f"git worktree add -b {branch} {wt} {plan.get('_base', 'HEAD')}")
    if rc != 0:
        log(f"dispatch {item['id']}: worktree add failed: {out.strip()[:200]}")
        return
    brief = f"""Work ONLY inside this worktree (the current directory). Never read any path outside it: not /root/projects/search, not /var/lib/*, not ~/.cargo. If a command would read outside it, skip that command. cargo and rustc are already on PATH.

Work fast: read at most 4 files, then WRITE CODE. Do not produce a plan document.

DELIVERABLE (exactly one): {item['title']}

DETAIL
{item['detail']}

DONE MEANS
{item['verify']}

RULES
- Run the tests for every crate you touch; they must pass.
- Commit on the current branch with a clear message.
- Do NOT commit any key or token. The LLM clients read credentials from the environment.
- Do NOT write to /var/lib/search, /var/lib/search-fleet or raw-dataset/ (read-only is fine).
- Prefer a small, well-tested change over a refactor.

REPORT: files changed, the exact test command, its result, and anything you could not do."""
    logf = OPS / f"dispatch-{item['id']}.log"
    cmd = (
        f"timeout {DISPATCH_LEASH_S} opencode run --model {model} {json.dumps(brief)} "
        f"> {logf} 2>&1"
    )
    log(f"dispatch {item['id']}: opencode on {model}, leash {DISPATCH_LEASH_S}s, worktree {wt}")
    rc, out = sh(cmd, timeout=DISPATCH_LEASH_S + 120)
    tail = ""
    try:
        tail = logf.read_text()[-600:]
    except OSError:
        pass

    verify_and_merge(plan, item, wt, branch, rc)
    return

    crates = [c for c in item.get("test_crates", [])]
    if not crates:
        rc3, out3 = sh(
            f"git -C {wt} diff --name-only {plan.get('_base', 'HEAD')}..HEAD | sed -n 's#^crates/\\([^/]*\\)/.*#\\1#p' | sort -u"
        )
        crates = [c.strip() for c in out3.splitlines() if c.strip()]
    pkg_flags = " ".join(f"-p search-{c}" if not c.startswith("search-") else f"-p {c}" for c in crates)
    rc4, out4 = sh(
        f"cd {wt} && PATH={os.path.dirname(CARGO)}:$PATH timeout 1500 {CARGO} test {pkg_flags} 2>&1 | tail -40",
        timeout=1600,
    )
    passed = "test result: FAILED" not in out4 and rc4 == 0
    summary = "; ".join(l.strip() for l in out4.splitlines() if "test result:" in l) or out4[-200:]
    if not passed:
        item["status"] = "blocked"
        item["reason"] = f"tests failed on {wt}: {summary[:400]}"
        save_plan(plan)
        log(f"dispatch {item['id']}: BLOCKED - tests failed: {summary[:200]}")
        return

    rc5, out5 = sh(f"cd {REPO} && git merge --no-edit {branch} 2>&1 | tail -3")
    if rc5 != 0:
        item["status"] = "blocked"
        item["reason"] = f"merge failed (worktree {wt} kept): {out5.strip()[:300]}"
        save_plan(plan)
        log(f"dispatch {item['id']}: BLOCKED - merge failed")
        return
    rc6, head = sh("git rev-parse --short HEAD")
    item["status"] = "done"
    item["evidence"] = f"{commits[-1].split()[0]} merged as {head.strip()}; tests: {summary[:200]}"
    save_plan(plan)
    log(f"dispatch {item['id']}: DONE - {item['evidence']}")


def gate_ops(plan: dict, item: dict) -> None:
    """An ops item runs only if its gate command passes. Otherwise it stays blocked."""
    gate = item.get("gate")
    if not gate:
        return
    if item["status"] == "blocked" and "packs exist" in item.get("detail", ""):
        needs = ["fleet-structuring"]
        if any((item_by_id(plan, n) or {}).get("status") != "done" for n in needs):
            return  # still blocked: the packs do not exist yet
        item["status"] = "todo"
        save_plan(plan)
        log(f"ops {item['id']}: unblocked (packs exist); waiting for a gate run")


def render(metrics: dict, plan: dict) -> None:
    def n(v, unit=""):
        return "unavailable" if v is None else f"{v:,}{unit}"

    rows = [
        "| shard | crawler | indexed | docs/min | pack records | raw |",
        "|---|---|---|---|---|---|",
    ]
    for s in SHARDS:
        i = metrics["shards"][str(s)]
        rows.append(
            f"| {s} | {'running' if i['running'] else 'stopped'} | {n(i['docs'])} | "
            f"{n(i.get('docs_per_min'))} | {n(i['pack_records'])} | {n(i['raw_bytes'])} bytes |"
        )
    items = "\n".join(
        f"| `{it['id']}` | {it['kind']} | **{it['status']}** | "
        f"{(it.get('evidence') or it.get('reason') or it.get('detail', '')[:70]).replace('|', '/')} |"
        for it in plan["items"]
    )
    body = f"""# Progress

Generated by `tools/progress_watch.py` at {metrics['ts']}. Every line here is measured; a value the
watcher could not read is written `unavailable`, never guessed.

## Serving

| what | value |
|---|---|
| live index generation (documents at build) | {n(metrics.get('index_generation_live'))} |
| live index manifest | {metrics['index_manifest_live'] or 'unavailable'} |
| local index generation | {n(metrics.get('index_generation_local'))} |
| live service up | {metrics.get('service_live_up')} |
| local service up | {metrics.get('service_local_up')} |
| main-pack records (index model) | {n(metrics['pack_records_main'])} |
| crawl rate (all shards) | {n(metrics.get('total_docs_per_min'), ' docs/min')} |
| crawler processes / cores | {n(metrics.get('crawler_procs'))} / {n(metrics.get('cores'))} |
| load average (1 min) | {n(metrics.get('load1'))} |
| crawl concurrency / origin interval | {n(metrics.get('crawl_concurrency'))} / {n(metrics.get('crawl_origin_interval_ms'), ' ms')} |
| implied mean fetch latency | {n(metrics.get('implied_fetch_s'), ' s')} |
| crawl error/timeout lines | {n(metrics.get('crawl_error_lines'))} |
| LLM cache entries (structuring is free when cached) | {n(metrics['llm_cache_entries'])} |
| disk free | {metrics['disk_free_gb']} GB |
| HEAD | `{metrics['commit']}` ({n(metrics['dirty_files'])} dirty files) |

## Fleet

{chr(10).join(rows)}

## Work plan

| item | kind | status | evidence / reason |
|---|---|---|---|
{items}
"""
    PROGRESS_MD.write_text(body)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--interval", type=int, default=300)
    ap.add_argument("--once", action="store_true")
    ap.add_argument("--no-dispatch", action="store_true")
    args = ap.parse_args()
    OPS.mkdir(exist_ok=True)
    # One cycle at a time. A manual run racing the service let the last writer win, which
    # silently reverted a freshly-blocked item back to todo.
    lock_fh = LOCK.open("w")
    try:
        # flock(), not fcntl(): fcntl(fd, F_SETLK) needs a struct-flock argument and raises
        # OSError without one, which made this guard fire on EVERY run and exit immediately -
        # the service flapped in an auto-restart loop and never completed a cycle.
        fcntl.flock(lock_fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        log("another progress-watch is running; exiting")
        return 1
    log("progress-watch starting" + (" (once)" if args.once else f" (interval {args.interval}s)"))
    while True:
        try:
            plan = load_plan()
            metrics = collect()
            with METRICS.open("a") as fh:
                fh.write(json.dumps(metrics) + "\n")
            start_structuring(plan, metrics)
            unblock_deps(plan)
            clone_references(plan, metrics)
            refresh_sidecars(plan, metrics)
            throughput_guard(plan, metrics)
            if not args.no_dispatch:
                # Research first: it is cheap, it runs on the browsing model, and the index-model
                # work depends on what it finds.
                for it in plan["items"]:
                    if it["kind"] == "research" and it["status"] == "todo":
                        dispatch_research(plan, it)
                        break
                else:
                    for it in plan["items"]:
                        if it["kind"] == "code" and it["status"] == "todo":
                            dispatch_code(plan, it)
                            break  # one session at a time: verification must stay tractable
            for it in plan["items"]:
                if it["kind"] == "ops":
                    gate_ops(plan, it)
            render(metrics, plan)
            sh("git add ops/plan.json docs/PROGRESS.md && "
               "git diff --cached --quiet || "
               "git commit -q -m 'chore(ops): progress-watch checkpoint'")
            log(
                f"cycle done: live_gen={metrics['index_generation_live']} local_gen={metrics['index_generation_local']} "
                f"cache={metrics['llm_cache_entries']} export_running={metrics['export_running']}"
            )
        except Exception as exc:  # noqa: BLE001 - a watcher must survive its own bugs
            log(f"cycle error: {type(exc).__name__}: {exc}")
        if args.once:
            return 0
        time.sleep(args.interval)


if __name__ == "__main__":
    sys.exit(main())
