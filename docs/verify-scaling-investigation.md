# Verify-mode scaling investigation (LIVING DOC)

> **Status:** ACTIVE. **This is a living document — updated after each stage.**
> Goal: explain the flattened verify-mode scaling curve and determine whether it is
> intrinsic or a fixable design artifact; back every claim with evidence; propose
> design tweaks. Branch `perf-verify-speedup`, worktree
> `…/rs-stellar-archivist-verifyperf`.
>
> Companions: `docs/max-concurrent-progress-report.md` (the existence-scan result +
> concurrency mental model §2.1–2.4), `docs/max-concurrent-local-http-sim.md` (the
> local archive server we'll use), `docs/max-concurrent-sweep-plan.md` (overall plan).
>
> **Update log**
> - 2026-06-17 — Part 0 (design audit) written. Decode-path baseline decision (async).
>   Improvement-ideas backlog seeded.
> - 2026-06-17 — Part 1 done: async baseline binary built (`bin/sa-verify-async`);
>   local-sim up (`miniserve` 0.35 serving `/data/pubnet-mirror` on `127.0.0.1:8088`,
>   `--hidden` for `.well-known`). Correctness gate passed (239 ok / 0 fail) **once
>   `--skip-optional` was used** — the mirror was built with `--skip-optional` so it
>   has no SCP files (see §8 caveat). Part 2 sweeps running.
> - 2026-06-18/22 — Part 2 done (§9, rewritten twice; see §9 supersede note). Large
>   multi-L10 range pins at ~3/32 cores for **both** async and sync decode. Definitive
>   evidence via in-runtime **tokio metrics** (`bin/sa-verify-rtm`): **~800–1000 live
>   decode tasks but run-queue ≈ 0 and ~3 busy cores** (one 29-core burst) → decoders
>   are *created in abundance but parked/starved*, not unscheduled. **Root cause: the
>   single `for_each_concurrent` orchestration task runs feed + XDR parse + tx/result
>   hashing + cross-file verify for every file (only gzip is spawned); the ~800 decoders
>   starve behind it.** `--max-concurrent` disproven as the cap (800 ≫ mc=128). Retracted:
>   draft-1 "lumpy discovery/H4" and draft-2 "Recv-Q backed up" (loopback artifact). Fix
>   = design options A–E (§9.6); prototype A (spawn-per-checkpoint) next. Instrumentation
>   committed `05d316c`.

---

## 1. Scope & question

Verify mode (`scan --verify`, and the verify-on-write in `mirror`) downloads each
referenced bucket, gzip-decompresses it, and checks its SHA-256. Earlier work found
verify **throughput stops scaling early** (local `file://`: a `-c≈4` plateau).

The question here is **not** "what's the best `--max-concurrent`." It is:

> **Why does verify scaling flatten, is it intrinsic or a design artifact, and what
> (if anything) should change in the design?** Confirm the effect is real (not a
> measurement or wrong-implementation artifact), verify each concurrency component
> behaves as intended, then propose evidence-backed tweaks.

---

## 2. Concurrency design map (verify path)

```
for_each_concurrent(-c)  over checkpoints                          [pipeline.rs:327]
  └─ process_checkpoint: join3(cats, scp, history)                [pipeline.rs:378]
       └─ process_history_and_buckets → process_buckets           [pipeline.rs:483]
            ├─ LOCK bucket_lru (Mutex<LruCache>); claim unseen buckets; UNLOCK
            └─ join_all(per-bucket download+verify futures)
                 └─ process_file → verify_bucket_*                 [verify.rs]
                      ├─ OpenDAL Operator.read (permit: ConcurrentLimitLayer)
                      └─ gzip-decode + SHA-256
```

- **`-c` / `--concurrency`** — number of checkpoint futures in flight
  (`for_each_concurrent`). Async tasks on tokio's default runtime (32 workers = nproc).
- **`--max-concurrent`** — OpenDAL `ConcurrentLimitLayer` semaphore; permit **held for
  the whole transfer** (≈ a live TCP connection). HTTP backend = `services::Http`
  over reqwest (`storage.rs`).
- **Bucket dedup** — `Mutex<LruCache<String,()>>`, 1M entries (`pipeline.rs:234`).
  See full model in `max-concurrent-progress-report.md` §2.

---

## 3. Part 0 — component audit (findings)

### 3.1 Bucket dedup LRU — ✅ correct, no bug
`process_buckets` (`pipeline.rs:483`) claims buckets **under the mutex** via
`cache.put(hash, ()).is_none()` (`None` ⇒ key was new ⇒ first claimant downloads it;
otherwise skip). Because the claim is inside the lock, two concurrent checkpoints
cannot both download the same bucket — **no thundering-herd re-download**, each bucket
is fetched exactly once. The lock is held only for the CPU-only claim, released before
`join_all` awaits the downloads.

**Consequence (not a bug, but the crux):** a bucket referenced by K checkpoints is
claimed by **one** checkpoint → **one download → one single-threaded gzip decode**.
So a shared large bucket is a serial long-pole *by construction*. Exercising decode
parallelism therefore requires a **large** range spanning many bucket-eras, so that
many *distinct* large buckets exist (one bucket-era offers no parallelism to find).

### 3.2 Decode path — ⚠️ branch confound, controlled by baseline decision (§4)
This branch's scan-verify uses the **experimental sync decode** (`verify_bucket_stream`,
`verify.rs:151`: `spawn_blocking` + `SyncIoBridge` + `flate2::read::GzDecoder`), which
commit `4a2c6f3` labels *"Experiment-only; not for merge as-is."* The **production**
decode is the **async** path (`verify_bucket_maybe_write`, `verify.rs:29`: an mpsc
channel + per-chunk `.await` feeding `async_compression::GzipDecoder`, all on the
worker pool). **Measuring scaling on the branch as-is would measure a non-shipped
path → an artifact.** Baseline decision in §4.

### 3.3 Semaphore coupling (download ⇄ decode) — ❌ investigated, NOT the bottleneck
Initial suspicion: the `ConcurrentLimitLayer` (`--max-concurrent`) permit couples
download and decode concurrency. **Disproven in Part 2:** we measured ~800 concurrent
live decode tasks at `mc=128` (§9.2), i.e. `active_decodes ≫ mc`, so the semaphore does
**not** cap concurrent streaming/decode here. The real limiter is the serial
orchestration task (§9.3), not this knob.

---

## 4. Decode path baseline: async (now) vs sync (parked)

**Decision (2026-06-17): baseline on the production ASYNC path** — it is what ships,
so it answers "why does the *current design* flatten." For the experiment binary we
point `verify_bucket_stream` at the async `verify_bucket_maybe_write` (keeping the
`perf-metrics` feature), rather than building from `main`.

**Async vs sync is PARKED, not dropped.** Once the larger suspects (long-pole,
semaphore coupling, scheduling) are crossed out, revisit a clean async-vs-sync
comparison on the quiet box + large range — it directly tests the per-stream
overhead hypothesis (H3) and finishes the Phase-2 question left inconclusive under
contamination (commit `4a2c6f3`; `docs/HANDOFF.md`). Trade-offs noted:
- **async** (mpsc + per-chunk await, worker pool): more coordination overhead per
  chunk; decode shares the worker pool with everything else.
- **sync** (`spawn_blocking`, blocking pool): no channel; decode on the (≤512) blocking
  pool; halved single-stream RSS in earlier tests but regressed c=16 RSS; ties up a
  blocking thread for the whole download+decode.

---

## 5. Hypotheses for the flat curve (and the discriminating test)

| # | Hypothesis | Kind | How we'll test |
|---|---|---|---|
| **H1** | **Intrinsic long-pole** — a shared big bucket = one serial gzip stream | intrinsic | large range with many distinct big buckets: does aggregate decode scale across cores? |
| **H2** | **Semaphore couples decode⇄download** — one permit gates both, so neither cores nor link saturate | design | per-core util + RX-bandwidth at the knee; test idea I1 (separate limits) |
| **H3** | **Per-stream async overhead** — mpsc + per-chunk await caps single-stream throughput / wastes worker CPU | design | async-vs-sync compare (parked, §4); CPU-per-MB decoded |
| **H4** | **Work imbalance** — a checkpoint blocks on its claimed long-pole while holding a `-c` slot; tail underutilizes cores | design | per-core utilization over time (tail), bucket-claim distribution |

**Resolution (Part 2, §9) — all four were wrong or not the cause:**
- **H1 (intrinsic long-pole):** ❌ not the cause — decode is parallel-capable (~800
  spawned decoders; 29-core burst); large ranges have abundant distinct buckets.
- **H2 (semaphore coupling):** ❌ disproven — `active_decodes ≈ 800 ≫ mc=128` (§3.3, §9.4).
- **H3 (async per-stream overhead):** ❌ ruled out — sync ≈ async, same ~3-core ceiling.
- **H4 (boundary-aligned work imbalance):** ❌ wrong — superseded by §9.
- **Actual cause:** the single `for_each_concurrent` **orchestration task** runs feed +
  XDR parse + tx/result hashing + cross-file verify for every file; the spawned gzip
  decoders starve behind it (§9.3).

---

## 6. Improvement ideas backlog (living — add freely)

> Tags: **[lever]** decode-CPU / network / scheduling / memory / observability ·
> **[effort]** S/M/L · **[risk]** low/med/high. Priority = cross out big suspects first.

### Tier 0 — THE root-cause fix (confirmed diagnosis, §9)
- **I0 — Take feed + parse + hash + verify OFF the single orchestration task.**
  [scheduling][M] Root cause (§9.3): `for_each_concurrent`/`join_all` give concurrency
  not parallelism, so the **feeding, XDR parsing, tx/result hashing, and cross-file
  verify run serially on one task** (only gzip is `tokio::spawn`ed); the ~800 spawned
  decoders starve ⇒ ~3/32 cores. The fix space is the **design options in §9.6**:
  **A** spawn-per-checkpoint (recommended first), **B** spawn-per-file, **C/D** split
  I/O vs a CPU pool (`rayon`), **E** move parse into the spawned decode task. Watch the
  `XdrVerificationManager` `Mutex` as the next bottleneck. **Prototype A in Part 3 =
  decisive confirmation.** (I1/I2/I3 below are complementary; H2-targeting I1 is now
  lower priority since the semaphore was disproven as the cap.)

### Tier 1 — complementary levers
- **I1 — Separate download-concurrency from decode-concurrency.** [scheduling/decode][M]
  Today one semaphore (`max-concurrent`) gates both (§3.3). Idea: release the OpenDAL
  permit when the bytes are in hand and run decode under a *separate* limit sized to
  cores (a decode semaphore or a `rayon`/blocking pool of ~nproc). Lets the link stay
  full while decode parallelism = cores. Directly targets H2. Cost: may buffer a
  bucket's compressed bytes (memory) if download fully precedes decode — or keep
  streaming but with an independent decode-permit.
- **I2 — Size-aware (largest-first / LPT) bucket scheduling.** [scheduling][M]
  Buckets vary enormously (one 2.36 GB vs p50 0.5 MB). Scheduling biggest-first
  minimizes makespan (classic LPT) so the long-pole starts early and overlaps with all
  the small work instead of finishing alone at the tail. Targets H4. Needs a size hint
  (bucket level / HAS ordering) before claim.
- **I3 — Global bucket work-queue, decoupled from per-checkpoint `join_all`.**
  [scheduling][L] Currently a checkpoint future waits for *its* claimed buckets and
  holds a `-c` slot meanwhile; a checkpoint that claims the long-pole is stuck on it.
  A single work-stealing queue over *all* discovered buckets balances load across
  workers and removes the checkpoint↔bucket coupling. Targets H4 (and amplifies I1/I2).

### Tier 2 — strong improvements (after suspects narrowed)
- **I4 — Parallel ranged download of a single large bucket (HTTP Range).** [network][L]
  Breaks the *download* half of the long-pole: fetch byte ranges of the 2.36 GB bucket
  over N connections, reassemble in order, decode-stream as ranges arrive. Decode stays
  serial (gzip) but download time drops ~N× and overlaps decode. Big if remote verify
  is download-bound at the tail. Needs server Range support (CDN: yes).
- **I5 — HTTP/2 multiplexing.** [network][M] HTTP/1.1 = one request per connection
  (we saw TIME-WAIT/keep-alive churn). h2 multiplexes many requests over few
  connections → less FD/port pressure and connection overhead. reqwest supports h2;
  test whether the archive/CDN negotiates it. Benefit largest for the many-small-bucket
  case; modest for big transfers.
- **I6 — Faster single-stream decoder (zlib-rs).** [decode][S] ~10–11% single-stream
  win, pure-Rust (resolved in the verify CPU experiment). Lowers the long-pole *floor*
  but does not parallelize it. Cheap, safe; productionize as default.
- **I7 — Read-ahead / buffer tuning.** [decode/network][S] Ensure decode never starves
  on network and the link never idles on decode: tune `CHANNEL_CAPACITY`, `BufReader`
  sizes, and prefetch depth on the streaming path.
- **I8 — Connection keep-alive / pool tuning.** [network][S] Reduce TIME-WAIT churn and
  reconnection cost observed in the existence sweep; size the idle pool to the knee.

### Tier 3 — observability & smaller wins
- **I9 — Expose dedup/transfer metrics in `--report`.** [observability][S] distinct vs
  downloaded buckets, bytes downloaded vs bytes decoded, per-phase wall. Makes dedup
  regressions and download/decode balance visible in production (and in these
  experiments without bespoke samplers).
- **I10 — (Archive-format, out of our control) parallelizable bucket compression.**
  [decode][L][note-only] If buckets were stored as framed zstd / bgzip (independent
  blocks), a *single* bucket could be decoded across cores — eliminating the decode
  long-pole entirely. Out of scope (server-side format) but worth recording.

---

## 7. Experiment plan (Parts 1–3)

- **Part 1 — Local-sim archive.** Stand up a concurrent HTTP server over
  `/data/pubnet-mirror` per `docs/max-concurrent-local-http-sim.md` (server =
  `miniserve` via cargo; bare loopback first, `tc netem` later). Removes politeness
  limits so large ranges are free. Build the experiment binary on the **async** decode
  path (§4) with `perf-metrics`.
- **Part 2 — Measure on large multi-L10 ranges.** Sweep `-c` (and `mc`), capturing per-core
  utilization (all 32?), throughput, RSS, bytes-downloaded-vs-distinct, thread states /
  `futex` (lock contention), and the `bottleneck.sh` verdict. Large range = many
  distinct big buckets (the discriminator, §5).
- **Part 3 — Attribute & propose.** Map results to H1–H4; if a design bottleneck is
  real, prototype the relevant idea(s) from §6 and re-measure; present evidence + a
  recommended tweak. Per-phase approval before any code change/commit.

**Periodic code review:** after each stage, re-scan the relevant code for missed
optimization/improvement opportunities and add them to §6.

---

## 8. Environment & caveats

- **Local-sim archive:** `miniserve 0.35.0` (installed via `cargo install`, no root)
  serving `/data/pubnet-mirror` on `127.0.0.1:8088` with `--hidden` (required so the
  `.well-known/stellar-history.json` root marker is served). Loopback connect ~0.14 ms.
  Bare loopback first (≈ infinite bandwidth → isolates decode/CPU); `tc netem` later.
- **⚠️ Mirror lacks optional SCP files (all checkpoints).** `/data/pubnet-mirror` was
  built by the Stage 2.2 mirror with `--skip-optional`, so it has **no `scp/`
  directory at all** — zero SCP for every checkpoint. All verify runs against the
  local-sim must pass `--skip-optional`, else every checkpoint reports a missing scp
  file (first gate: 17/256 failed, all `scp`; with `--skip-optional`: 0 failed).
  Buckets — what we measure — are present and verify clean. Driver defaults
  `SA_SKIP_OPTIONAL=1`.
  - Distinct from the **public** archive, where SCP presence *varies by checkpoint*:
    recent checkpoints HAVE scp (e.g. `63045055` → HTTP 200), early/initial ones do
    NOT (ledger `63` and `0x0003ffff` → HTTP 404) — SCP archiving began partway
    through pubnet history. Without `--skip-optional` the scanner treats a missing
    optional file as a failure, so a full-history *public* scan would flag early
    checkpoints' missing scp (by design). Our public runs used the recent range where
    scp exists, so they were clean without the flag.
- **Async baseline binary:** `bin/sa-verify-async` = `--features perf-metrics` with
  `verify_bucket_stream` delegating to the production async `verify_bucket_maybe_write`
  (the experiment's `src/verify.rs` edit; kept uncommitted as experiment scaffolding —
  the shipped sync experiment remains at commit `4a2c6f3`).
- **Measurement:** `nethealth.sh`/`bottleneck.sh` with `SA_NIC=lo` (loopback). Disk
  reads from the `/data` RAID0 NVMe (`nvme1n1 nvme2n1`) are a possible secondary
  limiter for large ranges (mirror is 7.2 TB, won't fit in page cache) — watch [5]–[7].

---

## 9. Part 2 — why verify doesn't scale (root cause: serial orchestration task)

> **This section supersedes two earlier, discarded drafts.** Draft 1 blamed "lumpy,
> boundary-aligned bucket discovery" — wrong. Draft 2 blamed a "single feeder task"
> and leaned on a "sockets hold unread data (Recv-Q)" reading — incomplete *and* that
> Recv-Q reading was a loopback artifact (retracted, §9.4). The account below is
> grounded in **in-runtime tokio metrics** + a **work-inventory audit** of the
> orchestration task, and is the current evidence-backed conclusion.

### 9.0 Method
All runs on the local-sim (miniserve over loopback ⇒ network bandwidth ≈ infinite;
any flat curve with idle cores/disk is therefore a **client design limit**, not I/O).
Binary `bin/sa-verify-async` (production async decode) unless noted; `--skip-optional`.
**We test large multi-L10 ranges**, which span many bucket-eras and therefore contain
many *distinct* large buckets — the condition required for decode parallelism to be
possible at all. Main range: recent **3×L10 =
196,608 cp, ledgers 50.46M–63.05M** (spill schedule: level `i` spans `4^(i+1)`
ledgers; L10 = 4,194,304 ledgers = 65,536 cp ⇒ this range crosses ~3 L10 + ~12 L9 +
~48 L8 boundaries ⇒ many *distinct* large buckets; the mirror holds many 2.1–2.6 GB
buckets, `bucket/` = 4.3 TB).

### 9.1 The observation: verify pins at ~3 cores regardless of knobs
Sustained live sampling on the large range (loopback):

| run | `-c` | `mc` | client cores (typ/peak) | throughput (typ/peak) | notes |
|---|---:|---:|---:|---:|---|
| async | 32 | 128 | 3.0 / 3.3 | 130 / 156 MB/s | sustained low |
| async | 256 | 256 | ~3 / 9.3 | 120 / 530 MB/s | bursty, not sustained |
| async | 128 | 128 | ~3 (2–5) | ~130 (40–246) MB/s | diag below |
| **sync** | 128 | 128 | ~3.7 (1.4–6.7) | ~160 (27–336) MB/s | same ceiling |

Raising `-c` 32→256 only makes the curve *burstier* (occasional 9-core spikes when
several big buckets briefly overlap); it does **not** lift the sustained ~3-core
average. The box has 32 cores.

### 9.2 The decisive in-runtime evidence (tokio runtime metrics, `-c=128`, async)
Instrumented build `bin/sa-verify-rtm` (`--cfg tokio_unstable` + an `ACTIVE_DECODES`
gauge counting live spawned decode tasks). Logged every 500 ms (raw:
`perf-results/maxconc/verifysim/rtmetrics/`):

| signal | value |
|---|---|
| **alive decode tasks** (`active_decodes`) | **700–1014** (grows over the run) |
| **run-queue depth** (inject + per-worker local) | **≈ 0** almost every sample |
| **busy cores** | **~0.4–3 typical**, one burst to **29.4 / 32** |

`-c` scaling control: `-c=32` → `active_decodes ≈ 85`, busy `~2–3`; `-c=128` →
`active_decodes ≈ 850`, busy `~3`. The decode-task count scales with `-c`
(≈ `-c × in-flight-files-per-cp`), **but cores stay ~3 either way.**

**Reading:**
1. Decode tasks are created in **abundance (~800–1000 alive)** — *not* under-created.
2. `runnable_q ≈ 0` with ~30 idle cores ⇒ those ~800 tasks are **parked `await`ing
   input** (empty decode channels), **not** unscheduled in a run queue. So it's not a
   scheduler problem and not lock contention (CPU would be busy, not idle).
3. The runtime parallelizes decode fine — the **29-core burst** (when many decoders
   momentarily become runnable) proves the cores are available.
4. 10× more decode tasks (`-c` 32→128) buys **zero** extra cores ⇒ the limit is *not*
   the number/scheduling of decoders, but a **single serial producer** feeding them.

### 9.3 What the orchestration task actually does (the real root cause)
`for_each_concurrent`/`join_all`/`join3` give **concurrency, not parallelism** — they
poll all child futures **within the one task** that awaits them (they don't spawn). And
that task does far more than orchestration — for *every* file, on one worker thread:

| work on the orchestration task | where | cost |
|---|---|---|
| **feed** — `stream.next()` + per-chunk `tx.send()` | `verify.rs`, `xdr_verify.rs:1039` | I/O + memcpy, every file |
| **XDR parse + tx-set/result hashing** — `read_xdr_iter` + `compute_v0/v1_tx_set_hash` | `parse_*_stream` → `parse_transaction_entries_for_checkpoint` (sync, after `decompress_to_buffer().await`) | **heavy CPU**, every category file |
| **cross-file verify** — completeness, tx/result hash compare, internal chain | `verify_and_release` (`pipeline.rs:404`) | CPU, under manager `Mutex` |
| cross-checkpoint chain verify | `verify_checkpoint_chain` (end) | CPU, once |
| orchestration — poll/join/spawn | `run_checkpoints` | — |

The **only** work `tokio::spawn`ed onto the worker pool is the **gzip decompress**
(`decompress_task`, `xdr_verify.rs:1015`) and the **bucket gzip+SHA** (`hash_task`,
`verify.rs:40`). **The feeding, the XDR parsing, the tx-set/result hash computation,
and all cross-file verification run serially on the one orchestration task.** That task
is the single serial producer (~1 core of mixed I/O+parse+hash+verify); the ~800
spawned gzip decoders starve behind it ⇒ ~3 cores total. Adding `-c`, `mc`, cores, or
blocking threads can't help — none of them parallelize that task.

### 9.4 Controls — ruled out, and explicitly retracted
- **Decode style (H3, async mpsc vs sync):** the sync binary hits the **same ~3-core
  ceiling** (blocking pool grew to 545 threads, almost all parked). Not the cause.
- **`--max-concurrent` / semaphore coupling (H2):** `active_decodes ≈ 800 ≫ mc=128`, so
  the OpenDAL `ConcurrentLimitLayer` does **not** cap concurrent streaming/decode — the
  earlier "semaphore couples download⇄decode" suspicion (§3.3 / H2) is **not** the
  bottleneck.
- **Instrumentation (`perf-metrics`):** lock-free atomics — not an artifact.
- **Locks (manager / stats / LRU):** CPU is *idle*, not spinning — symptom is
  "starved," not "lock-contended." (The manager `Mutex` is a candidate *next*
  bottleneck once decode parallelizes — §9.6 — but it isn't today's cap.)
- **Server / disk:** miniserve ~0.15 cores, disk sub-ms await — idle.
- **RETRACTED:** (a) the "lumpy boundary-aligned discovery / H4" story (draft 1) — wrong;
  (b) the "~80 sockets hold unread data (Recv-Q backed up)" reading (draft 2) —
  socket-buffer occupancy is unreliable on loopback (transfers finish too fast to catch
  buffered; a time series showed Recv-Q ≈ 0). The runtime metrics in §9.2 are the clean
  evidence, not Recv-Q.

### 9.5 Why this explains everything
- **Existence scaled (Phase A):** no per-byte CPU (`exists().await`) — one task juggling
  thousands of pure-I/O awaits is fine. ✅
- **Verify pins ~3 cores:** feed + parse + hash + verify for every file on one task. ✅
- **sync ≈ async:** same single-producer path; only the already-spawned decode differs. ✅
- **~800 decoders, `runnable_q≈0`, 29-core burst:** decoders abundant but starved; cores
  available. ✅
- **More `-c` ⇒ more decoders, same cores:** confirms the producer, not the decoders. ✅

### 9.6 Design options (to prototype + measure — NOT yet decided)
Goal: keep the main task **purely orchestration**; push feed + decode + parse + hash +
verify onto distributed work.

- **A. Spawn per *checkpoint*** — `tokio::spawn(process_checkpoint)`, bounded by
  `Semaphore(-c)`. Smallest refactor; moves *all* per-cp work off main; the
  `XdrVerificationManager` is already `Mutex`-protected for concurrent access; ordering
  preserved (chain-verify at end). **Recommended first.**
- **B. Spawn per *file*** — finest granularity/load-balance; more tasks, needs a global
  file-level concurrency bound; checkpoint must join its file tasks before
  `verify_and_release`.
- **C/D. Split I/O vs CPU pools** — async runtime for network+feed; a `rayon`/blocking
  CPU pool (≈ncores) for decode+parse+hash+verify; bytes cross via channels. Cleanest
  isolation + best ceiling; biggest refactor.
- **E. Targeted — move XDR parse+hash into the existing spawned `decompress_task`** —
  small change; takes the heavy parse off main, but leaves feed + `verify_and_release`
  on main (may just shift the bottleneck).
- **Next-bottleneck caveat:** once decode parallelizes, watch the
  `XdrVerificationManager` global `Mutex` (`record_*` + `verify_and_release`) — brief
  per call but may contend at 32-way; shard / make lock-free if so. Don't pre-optimize.

**Plan:** prototype **A**, measure cores + manager-lock contention; escalate to **D**
(CPU pool) and/or shard the manager if A is limited by CPU-on-async-workers or lock
contention. Decisive confirmation = cores climb toward saturation (the 29-core burst is
the ceiling we expect to unlock).

### 9.7 Caveats
- aarch64 + miniserve loopback; absolute MB/s is box-specific, but the **structural
  conclusion** (serial orchestration task does feed+parse+hash+verify while spawned
  decoders starve) is code-level and portable. Disk cold (4.3 TB > RAM) but provably not
  the limiter (sub-ms await, idle).
