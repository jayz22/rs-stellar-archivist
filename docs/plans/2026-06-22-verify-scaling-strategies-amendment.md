# Amendment — Verify-Scaling Strategies Plan (review notes)

> **Status:** PROPOSAL for the original author's consideration. Companion to
> `docs/plans/2026-06-22-verify-scaling-strategies.md` (the "plan") and
> `docs/verify-scaling-investigation.md` (the "investigation"). Nothing here is
> applied — each item is a self-contained suggestion with a rationale and a
> concrete change, tagged with severity and an author-decision checkbox. The
> plan and investigation are otherwise sound; this only fills gaps and adds two
> options.

## How this was reviewed

Validated against the **actual code on this branch** (base `d0266b2`), not a
sibling branch. The plan's line numbers and APIs match: `Pipeline::run`
(`pipeline.rs:285`), `run_checkpoints` (`:310`), `process_checkpoint` (`:378`),
`process_file` (`:540`), the pipeline owns `src_store`/`dst_store` (`:229-230`),
and the `fetch_history_buffer`/`process_buffer` history model. Every call site
the strategies touch was traced with grep. The investigation's root-cause
evidence (§9.2: `~800–1014` live decode tasks, run-queue `≈0`, `~3/32` cores)
is decisive and correctly attributed.

## Summary of proposed amendments

| # | Amendment | Kind | Severity |
|---|---|---|---|
| A1 | Correct the strategy table: C and D offload **decode only**, not XDR parse | correction | **high** (misleading) |
| A2 | Fix Strategy B's `Arc` ripple + the contradictory "delete `process_history_and_buckets`" | correction | **high** (won't compile / breaks repair) |
| A3 | Restructure Strategy C off the `SyncIoBridge`-in-`spawn_blocking` anti-pattern | improvement | medium |
| A4 | Deduplicate Strategy E via one generic helper | improvement | low |
| A5 | Add **Strategy F** — `spawn_blocking` the *sync parse* (missing discriminator) | new branch | medium |
| A6 | Add **Task G** — compose the winners + shard the manager `Mutex` | new task | medium |
| A7 | Benchmark: capture peak RSS; drop the no-op `io-util` step; label partial-run sampling | improvement | low |
| A8 | Strategy A/B: panic in a spawned task is swallowed (parity with base) | correction | low |

---

## A1 — Correct the strategy table: C and D offload decode only

**Where:** the strategy table (plan lines 34–40), rows C and D, columns "moves
off the orchestration task" / "full fix?".

**The gap.** The table claims C and D move *"feed + decode + parse (bucket &
xdr)"* and *"decode + parse"* off the orchestration task. The implementations do
**not** move the XDR parse off:

- Task C Step 2 says verbatim: *"The XDR parse stays on the caller after
  `.await` — this branch isolates the decode offload"* (plan line 438).
- Task D rewrites only `decompress_to_buffer` (`xdr_verify.rs:842`) to return the
  decompressed `Vec`; `parse_ledger_header_stream` / `parse_transactions_stream`
  / `parse_results_stream` (`xdr_verify.rs:851,950,864`) still call
  `parse_*_entries_for_checkpoint` **on the caller** after the `.await`.

So for XDR files C and D offload only **gzip decode** (+ for buckets, the hash,
which the base already spawns at `verify.rs:40`). The heavy work the
investigation identified —
`parse_transaction_entries_for_checkpoint` →
`compute_v0/v1_tx_set_hash` over every transaction (`xdr_verify.rs:880,914-917`)
and the ledger-header SHA-256 — **stays on the orchestration task**.

**Why it matters.** Investigation §9.3 names "XDR parse + tx/result hashing" as
the heavy per-file CPU on the orchestration task, and §9.4 reports that a bucket
**sync-decode already hit the same ~3-core ceiling**. So C and D are predicted to
*plateau on the XDR parse*, not to scale. The "partial" tag is right; the "moves
parse off" claim is wrong and will mislead whoever reads the results.

**Proposed change.** Rewrite the C and D rows to read "moves off: **decode
(bucket & xdr) + bucket hash**; **leaves on: feed-coordination, XDR parse+hash,
`verify_and_release`**", and add a one-line predicted outcome: *"expected to
plateau where the base does if XDR parse+hash dominates — serves as the
decode-vs-parse discriminator."* (See A5 for the complementary parse-only
discriminator.)

- [ ] Accept  - [ ] Reject  - [ ] Modify

---

## A2 — Strategy B: real `Arc` ripple + a contradictory deletion

**Where:** Task B Step 1–3 (plan lines 203–323) and the self-review note (plan
lines 753–756).

**The gap (two parts).**

1. **`process_file` → `self: Arc<Self>` ripples further than documented.** Task B
   Step 3 changes `process_file`'s signature to `self: Arc<Self>` to hold the
   semaphore permit. But `process_file` is called from outside the pipeline:
   - `repair_operation.rs:447` — `pipeline.process_file(cp, path).await`
   - and `process_history_and_buckets` (`pipeline.rs:414`) calls `process_file`
     internally, so it too would have to become `Arc<Self>`.
   The self-review only mentions wrapping the **`run_checkpoints`** caller; it
   misses these.

2. **"Delete `process_history_and_buckets`" contradicts repair.** Task B Step 3
   (plan line 320) and the self-review (line 753) say to delete the now-unused
   `process_buckets`/`process_history_and_buckets`. They are **not** unused —
   repair calls `process_history_and_buckets` at `repair_operation.rs:445`.
   Deleting it breaks the repair retry path; the build fails.

**Why it happened.** B borrowed A's "spawn + own an `Arc`" shape but also changed
the *callee* signatures, which is unnecessary and is what creates the ripple.

**Proposed change (also makes B cleaner and smaller).** Keep `process_file` (and
`process_history_and_buckets`) as `&self`, and acquire the permit **in the spawn
wrapper** inside `process_checkpoint`, mirroring A's pattern:

```rust
let permit = self.file_semaphore.clone().acquire_owned().await.unwrap();
let me = Arc::clone(&self);
set.spawn(async move {
    let _permit = permit;            // held for the task's lifetime
    me.process_file(checkpoint, path).await
});
```

Consequences: `process_file` and `process_history_and_buckets` keep `&self`, so
**both repair call sites (`:445`, `:447`) are untouched**; only
`run`/`run_checkpoints`/`process_checkpoint` become `Arc`-based, and only the
`run_checkpoints` caller (`repair_operation.rs:494`) needs the `Arc` wrap the plan
already calls for. Do **not** delete `process_history_and_buckets`. If B's
inlined bucket-discovery duplicates it, either leave the duplication (experiment
branch) or have both share a small helper — do not remove the method repair
depends on.

- [ ] Accept  - [ ] Reject  - [ ] Modify

---

## A3 — Strategy C: avoid blocking a pool thread on async I/O

**Where:** Task C Steps 1–2 (plan lines 353–439).

**The gap.** C wraps `SyncIoBridge::new(async_read)` inside `spawn_blocking` and
reads the source through it. `SyncIoBridge` turns each synchronous `read()` into a
`Handle::block_on` of the async reader — so the blocking-pool thread is parked on
**network I/O** for the entire download, not just CPU. With ~800 concurrent files
this is the 512-thread pressure the investigation already observed (§9.4:
"blocking pool grew to 545 threads, almost all parked"). Using the blocking pool
for I/O-bound waits defeats its purpose (it exists for CPU/FS-blocking work) and
caps useful parallelism at the pool size while burning threads.

**Why it matters.** Besides the thread waste, it confounds the experiment: C-as-
written measures "decode on blocking pool **with feed also block_on'd there**,"
which is not cleanly comparable to D ("read async, decode on rayon"). And §9.4
already showed a sync bucket decode reproduces the ceiling.

**Proposed change.** Make C symmetric with D: read the compressed bytes **async**
(off the blocking pool), then `spawn_blocking` only the **pure-CPU decode+hash**
over the owned buffer, bounded by a `Semaphore(~nproc)`:

```rust
let compressed = reader.read(..).await?.to_vec();          // async I/O
let _permit = DECODE_SEM.acquire().await;                  // bound to ~nproc
let (actual, n) = tokio::task::spawn_blocking(move || {
    let mut dec = flate2::read::GzDecoder::new(std::io::Cursor::new(compressed));
    // ... hash loop ...
}).await??;
```

Now C and D differ only in the pool (tokio blocking vs rayon), giving a clean
comparison, and neither thread-floods. (Keep `verify_bucket_maybe_write` and
`decompress_and_write_internal` — both are still used by the mirror
verify-on-write path via `verify_and_write_bucket` / `verify_and_write_xdr`,
`xdr_verify.rs:1172,1192`; the plan's "delete if unused" hedges are correct, just
confirm with grep before removing anything.)

- [ ] Accept  - [ ] Reject  - [ ] Modify

---

## A4 — Strategy E: one generic helper instead of three copies

**Where:** Task E Steps 1–3 (plan lines 582–647).

**The gap (minor).** E rewrites `parse_transactions_stream`,
`parse_ledger_header_stream`, and `parse_results_stream` with near-identical
stream + channel + spawn boilerplate, and the plan instructs "Write the full body
— no 'same as above'" (line 646). Three copies of the streaming machinery is
avoidable churn and a future maintenance hazard.

**Proposed change.** Factor a single generic helper that decodes in the spawned
task and runs a caller-supplied parse closure **inside** that task:

```rust
async fn decompress_then<T: Send + 'static>(
    path: &str,
    reader: Reader,
    parse: impl FnOnce(&[u8]) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, StorageError> {
    // stream → mpsc(64) → spawn { GzipDecoder.read_to_end(&mut buf); parse(&buf) }
    // feed loop on the caller; task.await yields the parsed T
}
```

Then each `parse_*_stream` is one line, e.g.
`decompress_then(path, reader, move |b| parse_transaction_entries_for_checkpoint(b, cp)).await`.
Same behavior, no duplication, and it keeps E the smallest *correct* change.

- [ ] Accept  - [ ] Reject  - [ ] Modify

---

## A5 — New Strategy F: `spawn_blocking` the synchronous parse

**Where:** new branch `vs/f-parse-spawn-blocking`, sibling to A–E.

**The gap it fills.** The matrix has spawn-the-decode (C/D) and fuse-parse-into-
the-decode-task (E), but **not** "keep the async decode, offload only the sync
parse." That is the smallest possible change that moves the §9.3 hotspot off the
orchestration task, and — paired with C — it is the **clean discriminator** the
plan currently lacks:

- **C** offloads decode, leaves parse → isolates the decode cost.
- **F** offloads parse, leaves the (already-spawned) decode → isolates the parse cost.
- If F lifts cores and C doesn't, XDR parse+hash is the cap (as §9.3 predicts).
- If neither alone suffices, it's the *combination* (which E/A/B deliver).

**The change.** Leave `decompress_to_buffer` (`xdr_verify.rs:842`) untouched; wrap
only the synchronous `parse_*_entries_for_checkpoint` in `spawn_blocking`, bounded
by a `Semaphore(~nproc)` since it is CPU on the (512-thread) blocking pool:

```rust
pub async fn parse_transactions_stream(path: &str, reader: Reader)
    -> Result<BTreeMap<u32, Hash>, StorageError>
{
    let decompressed = decompress_to_buffer(path, reader).await?;   // async decode, unchanged
    let cp = history_format::checkpoint_from_path(path);
    let _permit = PARSE_SEM.acquire().await;
    tokio::task::spawn_blocking(move || parse_transaction_entries_for_checkpoint(&decompressed, cp))
        .await
        .map_err(|e| StorageError::fatal(format!("parse task panicked {path}: {e}")))?
}
```
(Same for ledger/results; SCP has no hashing, leave it.) **Files:**
`src/xdr_verify.rs` only. Smallest diff of any full-ish fix; no streaming rewrite,
no `SyncIoBridge`. Add row F to the strategy table and to the benchmark matrix.

- [ ] Accept  - [ ] Reject  - [ ] Modify

---

## A6 — New Task G: compose the winners + shard the manager `Mutex`

**Where:** new task after Task F (benchmark), before "self-review notes."

**The gap.** The plan's "five **mutually-exclusive** branches" framing is exactly
right for *attribution*, but it structurally cannot surface the **production
answer**, which is almost certainly a composition:

- A/B distribute feed+parse+hash per cp/file but keep the async decode spawn;
- E/F move parse off but leave the single-task **feed** (`stream.next` + per-chunk
  `tx.send` for ~800 files) on the orchestration task;
- once decode parallelizes, the next bottleneck the investigation itself flags
  (§9.6, plan self-review line 758) is the global `XdrVerificationManager`
  `Mutex` — `record_*` + `verify_and_release` (`pipeline.rs:404`) at 32-way.

A single mutually-exclusive branch can show *which lever helps most* but not
*whether two together saturate*.

**Proposed Task G.**
1. Take the best *distribution* winner (A or B) and re-measure with the manager
   sharded: split `pending: Mutex<HashMap<u32,…>>` into `[Mutex<HashMap>; N]`
   keyed by `cp % N` (or `DashMap`), and confirm `verify_and_release` does
   remove-under-lock then compute-outside-lock. Gate this on the observed signal
   the plan names: a plateau `< 32` cores with `runnable_q > 0` (CPU spinning on
   the lock, not starved).
2. If the distribution winner *still* leaves feed-bound headroom, layer E/F's
   parse-offload onto it and re-measure.
3. Record the composed result against the single-strategy baselines.

This is the step that turns "which branch wins" into "what should we ship."

- [ ] Accept  - [ ] Reject  - [ ] Modify

---

## A7 — Benchmark (Task F) improvements

**Where:** Task F (plan lines 660–745).

- **Capture peak RSS.** D's central tradeoff is memory — `reader.read(..).to_vec()`
  buffers the whole compressed file (the mirror has 2.1–2.6 GB buckets;
  investigation §9.0), times in-flight count. The harness reports cores + wall but
  **not memory**, so D's cost is invisible in the results. The infrastructure
  already exists: `metrics.rs` `peak_rss_bytes()` (`:120`) and the `SA_PERF_OUT`
  `timeseries.csv` sampler (`start_sampler`, `:138`). Add a `peak_rss_mb` column
  sourced from the existing `headline.csv`/`timeseries.csv`.
- **Drop the no-op step.** Task C Step 3 adds `io-util` to `tokio-util`, but
  `Cargo.toml:69` already has `features = ["compat", "io", "io-util"]`. Remove the
  step (or note it's already satisfied).
- **Label the partial-run sampling.** `verify_cores` is sampled by running for
  `SA_WINDOW=60s` then `kill -9` — a *partial-run* sample, fine for relative
  ranking but not a completion metric. Note this next to the column so results
  aren't read as full-run averages. The bounded-range `verify_wall` is the
  completion/throughput number; keep both.
- **Optional:** a small `-c` sweep for the *winning* strategy only, to show the
  scaling curve rather than a single `-c=128` point.

- [ ] Accept  - [ ] Reject  - [ ] Modify

---

## A8 — Strategy A/B: spawned-task panics are swallowed

**Where:** Task A Step 2 (plan lines 138–142); the same applies to B's `JoinSet`
drain (plan line 302).

**The gap.** A logs `tokio::spawn(...).await`'s `Err(JoinError)` and continues; B's
`while set.join_next().await.is_some() {}` ignores the `Result` entirely. In the
**base**, a panic inside `process_checkpoint` propagates through
`for_each_concurrent` and aborts the run. Under A/B a panicked checkpoint/file is
silently dropped and **its files are never recorded as failures** — so the
correctness gate (which compares `succeeded`/`failed`/`retries` and `broken_sig`
to base, plan Step 4) could read "success" for a run that actually panicked.

**Why it matters here specifically.** A hash mismatch is *not* a panic (it's a
recorded failure inside `process_object`), so any `JoinError` is a real bug — and
the one place it must not be hidden is the benchmark's correctness gate.

**Proposed change.** On `JoinError`, either abort the run or record the
checkpoint/file as a failure (e.g. mark the cp's files via
`stats.record_failure`), so the correctness gate cannot pass over a panic. At
minimum, make the harness fail if any "task panicked" line appears in stderr.

- [ ] Accept  - [ ] Reject  - [ ] Modify

---

## Cross-reference: ideas already in the backlog (not re-proposed here)

Several complementary levers are already captured in investigation §6 and need no
amendment — flagged only so they aren't re-invented: **I1** separate
download/decode concurrency limits (composes with C/D/F), **I2** largest-first
(LPT) bucket scheduling, **I3** global bucket work-queue, **I6** faster
single-stream decoder (zlib-rs). The amendments above are deliberately scoped to
the A–E plan mechanics plus the two additions (F, G).

## Predicted ranking (a hypothesis for the benchmark to confirm or refute)

Stated so results can be checked against an explicit prior, per the
investigation's "back every claim with evidence" stance:

```
A ≈ B   (full: feed+parse+hash distributed)         → expect cores → saturation*
  >  E ≈ F   (parse offloaded; single-task feed remains)  → near-full unless feed-bound
  >  C ≈ D   (decode offloaded; XDR parse+hash remains)   → expect base-like plateau
      (* modulo the manager Mutex — see Task G)
```

If C/D do **not** plateau, that refutes the §9.3 "parse is heavy" model and is
itself a valuable finding.
