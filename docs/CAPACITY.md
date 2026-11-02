# Capacity — what the crawl and index actually do, and what 1000 sites/s would need

Question: can this system crawl 1,000 websites per second? Answer: **not on this host and this IP**,
and the first wall is neither CPU nor bandwidth. Numbers below are measured on 2026-11-02, mostly
between 09:50 and 10:00 while the fleet was healthy.

## 1. What it does now

| Metric | Measured | Window |
| --- | --- | --- |
| attempts | **540 / min** (9 / s) | 10 min, 4 shards |
| documents ingested | **372 / min** (6.2 / s, **22,308 / h**) | same |
| yield | 68.8% ingested | same |
| distinct hosts touched | 1,260 (~2.1 / s) | same |
| frontier supply | 464,408 READY, ~10.4M NEW | 09:30 |

Per shard: 0 → 1,266 ingested/10 min, 1 → 1,176, 2 → 143, 3 → 1,133. The spread is the frontier's
composition, not the binary: one shard was draining a rate-limited, spam-heavy slice at the time.

Two things had to be fixed before these numbers existed at all: **195,648 expired `FETCHING` leases**
(55,158 + 52,108 + 56,786 + 31,596) were reclaimed with `agent0 frontier recover`, and the crawlers
were dying silently (see `docs/OPERATIONS.md`). Before both, the fleet sat at ~110 docs/min.

## 2. What 1,000 sites/s means

1,000 sites/s = **60,000 attempts/min**, 111× the current attempt rate, and — at the measured 68.8%
yield — about 690 documents/s.

| Constraint | Measured here | Needed at 1,000/s | Verdict |
| --- | --- | --- | --- |
| Ephemeral ports (`ip_local_port_range`) | 28,231 (32768–60999), 60 s `TIME_WAIT` per connection | ~60,000 ports/s in rotation → **≥3 egress IPs** | **first wall** — one IP caps at ~470 new connections/s |
| Concurrent sockets | `ulimit -n` 524,288; 1,000/s × 0.8 s latency ≈ 800 in flight | 800 | fine |
| Distinct origins in flight | 1,260 hosts per 10 min | 1,000 distinct origins **per second**, 60k/min | needs a frontier of millions of *ready* hosts, not 464k |
| Parse + index CPU | rebuild measured ~1,870 docs/s; parse+index ~1,300–1,500/s on 16 cores | 690 docs/s at 1000 attempts/s | fits, but leaves little for fetching |
| Disk (extracted text) | 11.6 KB/doc measured | 690 × 11.6 KB ≈ **8 MB/s ≈ 690 GB/day** | **second wall** — ~100 GB free is ~4 hours |
| Disk (index) | 1.10 KB/doc | ~0.8 MB/s, ~66 GB/day | grows faster than deletion |
| Network | NIC sustains 650 Mbps, crawl uses ~2 Mbps today | 1,000 pages/s × ~60–100 KB HTML ≈ 500–800 Mbps | **third wall** — at or above the NIC |
| Politeness | 250 ms per origin | legal only if every request is to a *different* host | requires 1,000 distinct hosts/s; re-hitting a host at 1/s is 4× the configured floor |

### What would have to change, in order

1. **More egress IPs** (≥3, ideally 8) — the port table is arithmetic, not tuning. `tcp_tw_reuse`
   does not help real internet connections.
2. **A disk strategy**: do not keep 690 GB/day of text. Either keep excerpt + vectors only (the
   architecture's target model) or crawl in short campaigns and delete sources after the derived
   layers are verified.
3. **A separate indexer**: 690 docs/s consumes half the parse/index budget on this box, and the
   crawler needs the other half.
4. **A frontier that can supply 60k distinct origins per minute** — today it has 464k ready URLs
   total, which at 1,000/s is under eight minutes of work before it must discover more.

None of that is a code change to the crawl loop: concurrency is already not the limit (measured flat
from `--concurrency` 256 to 2048, 5.8k attempts/min each, because the rate is set by how many
distinct origins are in flight times the per-origin interval).

## 3. What is worth doing at the current scale

At 22k docs/h the corpus grows ~530k/day, which is already faster than the extracted-text disk
budget allows to keep forever (6 GB/day of text, ~26 GB/day of index at the current 1.10 KB/doc).

The levers that actually raise throughput here, in measured order:

1. **Frontier composition** — retire hosts that 429 repeatedly and are spam-adjacent; a handful of
   hostile origins absorb slots (measured: shard-2 at 143 docs/10 min while three others did 1,100+).
2. **Reclaim leases** on a schedule (`frontier recover`), not by hand.
3. **Breadth**: more distinct origins in the seed/frontier, not more concurrency.

Every one of those is about *which* URLs are attempted, not how fast one URL is fetched — the same
conclusion the crawl measurements have reached every time.