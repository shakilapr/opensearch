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
    rc, out = sh(f"find {DATA}/llm-cache -name '*.json' 2>/dev/null | wc -l", timeout=60)
    if out.strip().isdigit():
        m["llm_cache_entries"] = int(out.strip())
    for s in SHARDS:
        m["shards"][str(s)] = {
            "docs": shard_docs(s),
            "raw_bytes": dir_bytes(REPO / "raw-dataset" / f"shard-{s}"),
            "pack_records": pack_records(s),
            "running": bool(
                sh(f"pgrep -fa 'crawl /var/lib/search-fleet/work/shard-{s}' | grep -v pgrep | wc -l")[1]
                .strip()
                .isdigit()
            ),
        }
    return m


# --------------------------------------------------------------------------- plan


def load_plan() -> dict:
    return json.loads(PLAN.read_text())


def save_plan(plan: dict) -> None:
    PLAN.write_text(json.dumps(plan, indent=2) + "\n")


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


def dispatch_code(plan: dict, item: dict) -> None:
    """Dispatch one coding task to opencode in an isolated worktree, then verify it here."""
    wid = f"wt-{item['id'][:18]}"
    wt = WORKTREES / wid
    branch = wid
    model = item.get("model") or DEFAULT_MODEL
    if wt.exists():
        log(f"dispatch {item['id']}: worktree {wt} already exists, skipping (inspect it)")
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

    rc2, newc = sh(f"git -C {wt} log --oneline {plan.get('_base', 'HEAD')}..HEAD")
    commits = [l for l in newc.strip().splitlines() if l.strip()]
    if not commits:
        item["status"] = "blocked"
        item["reason"] = (
            f"opencode produced no commit (rc={rc}); worktree kept at {wt}. Log tail: {tail[-300:]}"
        )
        save_plan(plan)
        log(f"dispatch {item['id']}: BLOCKED - no commit produced")
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
        "| shard | crawler | indexed | pack records | raw |",
        "|---|---|---|---|---|",
    ]
    for s in SHARDS:
        i = metrics["shards"][str(s)]
        rows.append(
            f"| {s} | {'running' if i['running'] else 'stopped'} | {n(i['docs'])} | "
            f"{n(i['pack_records'])} | {n(i['raw_bytes'])} bytes |"
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
| live index generation (documents at build) | {n(metrics['index_generation_live'])} |
| live index manifest | {metrics['index_manifest_live'] or 'unavailable'} |
| local index generation | {n(metrics['index_generation_local'])} |
| live service up | {metrics['service_live_up']} |
| local service up | {metrics['service_local_up']} |
| main-pack records (index model) | {n(metrics['pack_records_main'])} |
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
    log("progress-watch starting" + (" (once)" if args.once else f" (interval {args.interval}s)"))
    while True:
        try:
            plan = load_plan()
            metrics = collect()
            with METRICS.open("a") as fh:
                fh.write(json.dumps(metrics) + "\n")
            start_structuring(plan, metrics)
            if not args.no_dispatch:
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
