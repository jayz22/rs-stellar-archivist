# Verify-Scaling Strategies — Implementation & Benchmark Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use superpowers:subagent-driven-development
> or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Implement six **mutually-exclusive** candidate fixes (A–F) for the verify-mode
scaling bottleneck, **each on its own branch off a common base**, then run one benchmark
suite that compares them on correctness, core usage, scan-verify (multi-L10), and
scan-no-verify — followed by a composition/productionization step (Task G) and a
**winner capability graph** (Task H: throughput vs cores over the 196,608-cp multi-L10
range).

**Architecture:** Five sibling branches share base commit **`d0266b2`** (HEAD of
`perf-verify-speedup` *excluding* the uncommitted Task-A prototype). Each branch is a
**clean, direct** implementation of one strategy — modify the existing functions in the
simplest way; **do not** add runtime switches, `_spawned` variants, or `cfg`/env dispatch
(maintainability/coexistence is explicitly out of scope). The benchmark builds each
branch's binary and sweeps `binary × {verify, no-verify}` over a fixed multi-L10 range.

**Tech stack:** Rust, tokio 1.35 (`tokio_unstable` runtime metrics — already in base),
OpenDAL+reqwest, `async-compression`/`flate2`, `rayon` (new dep, branch D only),
`miniserve` local-sim, bash/python harness.

**Root cause** (`docs/verify-scaling-investigation.md` §9): in `scan --verify`,
`for_each_concurrent`/`join_all` give concurrency not parallelism — **feed + XDR-parse +
tx/result-hash + cross-file-verify run serially on one orchestration task**; only gzip is
spawned. → ~800 decode tasks starve, ~3/32 cores. The base already contains the
instrumentation needed to measure the fix: the `ACTIVE_DECODES` gauge + `DecodeGuard`
(`metrics.rs`), the `#[cfg(tokio_unstable)]` `SA_RT_METRICS` logger (`main.rs`), and the
production async-decode baseline (`verify.rs`).

---

## The six strategies (mutually exclusive; each its own branch)

| branch | idea | moves off the orchestration task | leaves on it | predicted |
|---|---|---|---|---|
| `vs/a-spawn-checkpoint` | spawn each checkpoint | *everything* per-cp incl. `verify_and_release` | iteration only | **full** (→ saturation, modulo manager `Mutex`) |
| `vs/b-spawn-file` | spawn each file | feed + decode + XDR-parse + hash (per file) | iteration, HAS discovery, `verify_and_release` | **near-full** |
| `vs/c-spawn-blocking` | **decode** on tokio blocking pool | gzip decode + bucket hash | feed-coord, **XDR parse+hash**, `verify_and_release` | **plateau** if parse dominates (decode discriminator) |
| `vs/d-rayon` | **decode** on a rayon pool | gzip decode + bucket hash | feed-coord, **XDR parse+hash**, `verify_and_release` | **plateau** like C (decode discriminator) |
| `vs/e-parse-in-task` | fuse parse+hash into the spawned decode task | gzip decode + XDR parse + hash | feed-coord, `verify_and_release` | near-full unless feed-bound |
| `vs/f-parse-spawn-blocking` | offload only the **sync parse** (async decode unchanged) | XDR parse + hash | feed-coord, `verify_and_release` | near-full (parse discriminator) |

**C/D vs F are the decode-vs-parse discriminators** (A1/A5): C/D move only the *decode*
(the XDR parse+hash — the §9.3 hotspot — stays on the orchestration task), so they're
predicted to plateau where the base does; F moves only the *parse* (decode stays async-
spawned). If F lifts cores and C/D don't, XDR parse+hash is the cap, as §9.3 predicts.
The benchmark measures whether each moves enough off the serial task to saturate cores.

---

## Task 0: Establish the shared base + branches

**Files:** none (git only).

- [ ] **Step 1: Clean the working tree to the base (discard the uncommitted Task-A code)**

The uncommitted `src/pipeline.rs` holds the Task-A prototype; A is reimplemented cleanly
on its own branch, so discard it. (Keep `docs/plans/` — the plan.)

```bash
cd /home/jay/Projects/rs-stellar-archivist-verifyperf
git stash push -- src/pipeline.rs        # or: git restore src/pipeline.rs
git rev-parse --short HEAD                # expect d0266b2 (the shared base)
```

- [ ] **Step 2: Create the six branches off the base (use worktrees for parallel builds)**

```bash
BASE=d0266b2
for b in a-spawn-checkpoint b-spawn-file c-spawn-blocking d-rayon e-parse-in-task f-parse-spawn-blocking; do
  git worktree add -b "vs/$b" "../vs-$b" "$BASE"
done
git worktree list
```
Expected: six worktrees `../vs-a-spawn-checkpoint` … `../vs-f-parse-spawn-blocking`, each at `vs/<b>`.

> Each task below is performed **in its own worktree/branch**. The base already has the
> instrumentation, so `DecodeGuard` / `SA_RT_METRICS` work everywhere. All verify runs use
> `--skip-optional` (mirror has no SCP — `…investigation.md` §8).

---

## Task A: branch `vs/a-spawn-checkpoint` — spawn each checkpoint

**Idea:** `run_checkpoints` spawns each `process_checkpoint` onto the worker pool, bounded
by `for_each_concurrent(-c)`. The orchestration task only spawns + awaits.

**Files:** Modify `src/pipeline.rs` (`run`, `run_checkpoints`); `src/repair_operation.rs`
(the other `run_checkpoints` caller).

- [ ] **Step 1: Make the pipeline shareable — `run` wraps `self` in `Arc`**

Replace `Pipeline::run` (`pipeline.rs:285`) with:

```rust
    pub async fn run(self) -> Result<(), Error> {
        let (lower_bound, upper_bound) =
            self.operation.get_checkpoint_bounds(&self.src_store).await?;
        let total_count = history_format::count_checkpoints_in_range(lower_bound, upper_bound);
        let this = Arc::new(self);
        if total_count != 0 {
            let checkpoints =
                (lower_bound..=upper_bound).step_by(history_format::CHECKPOINT_FREQUENCY as usize);
            Arc::clone(&this).run_checkpoints(checkpoints).await?;
        } else {
            info!("No checkpoints to process");
        }
        Arc::try_unwrap(this)
            .unwrap_or_else(|_| unreachable!("pipeline still shared after run_checkpoints"))
            .finish(upper_bound)
            .await
    }
```

Add `use std::sync::Arc;` (the base imports only `std::sync::Mutex`):

```rust
use std::sync::{Arc, Mutex};
```

- [ ] **Step 2: Change `run_checkpoints` to spawn each checkpoint**

Replace the `&self` signature and `for_each_concurrent` body (`pipeline.rs:310`) with:

```rust
    pub async fn run_checkpoints<I>(self: Arc<Self>, cps: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = u32>,
    {
        let cps = cps.into_iter();
        let total = cps.size_hint().1;
        if total == Some(0) {
            return Ok(());
        }
        let num_completed = std::sync::atomic::AtomicUsize::new(0);
        let completed_ref = &num_completed;
        let concurrency = self.config.concurrency;

        stream::iter(cps)
            .for_each_concurrent(concurrency, |ck| {
                let me = Arc::clone(&self);
                async move {
                    // A8: do NOT swallow a panic. In the base, a panic in
                    // process_checkpoint propagates through for_each_concurrent and
                    // aborts the run; preserve that so the correctness gate can't pass
                    // over a panicked checkpoint. (A hash mismatch is a recorded
                    // failure, not a panic — so a JoinError here is a real bug.)
                    if let Err(e) =
                        tokio::spawn(async move { me.process_checkpoint(ck).await }).await
                    {
                        if e.is_panic() {
                            std::panic::resume_unwind(e.into_panic());
                        }
                        error!("checkpoint {ck} task cancelled: {e}");
                    }
                    let done =
                        completed_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if done.is_multiple_of(PROGRESS_REPORTING_FREQUENCY) || total == Some(done) {
                        if let Some(total) = total {
                            info!("Progress: {done}/{total} checkpoints processed");
                        } else {
                            info!("Progress: {done} checkpoints processed");
                        }
                    }
                }
            })
            .await;

        if let Some(manager) = &self.verification_manager {
            manager.verify_checkpoint_chain();
            manager.drain_all_errors(&mut *self.stats.failures.lock().await);
        }
        Ok(())
    }
```

- [ ] **Step 3: Fix the repair caller**

`repair_operation.rs` calls `run_checkpoints` on a `&Pipeline`; wrap in `Arc`. Find the
call (`grep -n run_checkpoints src/repair_operation.rs`) and change e.g.
`pipeline.run_checkpoints(paths_iter)` → `std::sync::Arc::new(pipeline).run_checkpoints(paths_iter)`
(the local `pipeline` is owned there; if it's borrowed, build the `Arc` at construction).

- [ ] **Step 4: Build + correctness gate (must equal base)**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release --features perf-metrics --quiet && echo OK
cargo build --release --features perf-metrics --quiet   # (run full test suite in Task F gate)
bin=target/release/stellar-archivist
$bin scan http://127.0.0.1:8088 -c 128 --max-concurrent 128 --verify --skip-optional \
  --low 63045000 --high 63046015 --report /tmp/a.json >/dev/null 2>&1
python3 -c "import json;print(json.load(open('/tmp/a.json'))['summary'])"
```
Expected: `OK`, then `{'succeeded': 239, 'skipped': 0, 'failed': 0, 'retries': 0}`.

- [ ] **Step 5: Commit**

```bash
git add src/pipeline.rs src/repair_operation.rs
git commit -m "perf(verify): spawn per checkpoint (Strategy A)"
```

---

## Task B: branch `vs/b-spawn-file` — spawn each file

**Idea:** Keep the baseline checkpoint loop, but inside `process_checkpoint` spawn every
file (category + bucket) as its own task, bounded by a pipeline-wide `Semaphore`. Feed +
decode + parse run per-file in parallel; the checkpoint task does HAS discovery + joins +
`verify_and_release`.

**Files:** Modify `src/pipeline.rs` (`run`, `run_checkpoints`, `Pipeline` struct + `new`,
`process_checkpoint`); `src/repair_operation.rs` (wrap the `run_checkpoints` caller in
`Arc` only — `process_file`/`process_history_and_buckets` keep `&self`, so their repair
call sites are untouched).

- [ ] **Step 1: Arc plumbing (same as A Steps 1–3, but `run_checkpoints` does NOT spawn the checkpoint)**

Add `use std::sync::{Arc, Mutex};` and `use tokio::sync::Semaphore;`. `run` wraps `self`
in `Arc` (identical to Task A Step 1). `run_checkpoints` takes `self: Arc<Self>` but
**awaits `process_checkpoint` inline** (spawning happens per-file inside it):

```rust
    pub async fn run_checkpoints<I>(self: Arc<Self>, cps: I) -> Result<(), Error>
    where I: IntoIterator<Item = u32>,
    {
        let cps = cps.into_iter();
        let total = cps.size_hint().1;
        if total == Some(0) { return Ok(()); }
        let num_completed = std::sync::atomic::AtomicUsize::new(0);
        let completed_ref = &num_completed;
        let concurrency = self.config.concurrency;
        stream::iter(cps)
            .for_each_concurrent(concurrency, |ck| {
                let me = Arc::clone(&self);
                async move {
                    me.process_checkpoint(ck).await;     // inline; files are spawned inside
                    let done = completed_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    if done.is_multiple_of(PROGRESS_REPORTING_FREQUENCY) || total == Some(done) {
                        if let Some(total) = total { info!("Progress: {done}/{total} checkpoints processed"); }
                        else { info!("Progress: {done} checkpoints processed"); }
                    }
                }
            })
            .await;
        if let Some(manager) = &self.verification_manager {
            manager.verify_checkpoint_chain();
            manager.drain_all_errors(&mut *self.stats.failures.lock().await);
        }
        Ok(())
    }
```

Fix the repair caller to pass an `Arc` (same as Task A Step 3).

- [ ] **Step 2: Add a file-level semaphore to `Pipeline`**

Add field to `struct Pipeline<Op>` (`pipeline.rs:226`):

```rust
    /// Bounds total in-flight per-file tasks (≈ concurrency × fan-out).
    file_semaphore: Arc<Semaphore>,
```

In `Pipeline::new`, before the `Self { … }` literal:

```rust
        let file_semaphore =
            Arc::new(Semaphore::new(config.concurrency.saturating_mul(8).max(64)));
```
and add `file_semaphore,` to the literal.

- [ ] **Step 3: Convert only `process_checkpoint` to `Arc<Self>` + spawn each file**

**A2 (verified):** `process_file` is called by repair (`repair_operation.rs:447`) and
internally (`pipeline.rs:382,387,493`); `process_history_and_buckets` is called by repair
(`repair_operation.rs:445`). So **do NOT change `process_file`'s signature** and **do NOT
delete `process_history_and_buckets`** — that would break the build and the repair path.
Instead, keep both as `&self` and **acquire the file-semaphore permit in the spawn
wrapper** (mirroring A's "spawn + own an Arc" shape). Only `process_checkpoint` becomes
`Arc<Self>`. Replace `process_checkpoint` (`pipeline.rs:378`) with:

```rust
    pub async fn process_checkpoint(self: Arc<Self>, checkpoint: u32) {
        use tokio::task::JoinSet;
        let mut set: JoinSet<()> = JoinSet::new();

        // Helper closure shape: acquire a permit, then spawn process_file (which stays &self,
        // called via the Arc deref). The permit is held for the task's lifetime.
        let mut spawn_file = |me: &Arc<Self>, set: &mut JoinSet<()>, path: String| {
            let me = Arc::clone(me);
            let sem = self.file_semaphore.clone();
            set.spawn(async move {
                let _permit = sem.acquire_owned().await.unwrap();
                me.process_file(checkpoint, path).await;
            });
        };

        for cat in ["ledger", "transactions", "results"] {
            spawn_file(&self, &mut set, checkpoint_path(cat, checkpoint));
        }
        if !self.config.skip_optional {
            spawn_file(&self, &mut set, checkpoint_path("scp", checkpoint));
        }
        if !self.config.skip_history_and_buckets {
            // HAS fetch + history write + bucket discovery stay inline (cheap; gate the spawns).
            if let Some((state, buffer)) = self.fetch_history_file_state(checkpoint).await {
                let history_path = checkpoint_path("history", checkpoint);
                self.process_history_file(checkpoint, &history_path, buffer).await;
                let bucket_paths: Vec<String> = {
                    let mut cache = self.bucket_lru.lock().unwrap();
                    state.buckets().iter()
                        .filter_map(|b| (cache.put(b.clone(), ()).is_none())
                            .then(|| bucket_path(b).ok()).flatten())
                        .collect()
                };
                for path in bucket_paths {
                    spawn_file(&self, &mut set, path);
                }
            }
        }

        // A8: propagate a panicked file task (don't swallow); match base abort semantics.
        while let Some(res) = set.join_next().await {
            if let Err(e) = res {
                if e.is_panic() { std::panic::resume_unwind(e.into_panic()); }
                error!("file task cancelled in cp {checkpoint}: {e}");
            }
        }

        if let Some(manager) = &self.verification_manager {
            manager.verify_and_release(checkpoint);
        }
    }
```

`process_file` and `process_history_file` keep their existing `&self` signatures
(unchanged bodies). **`fetch_history_file_state`** is the existing private helper used by
`process_history_and_buckets`; reuse it (don't duplicate). Do **not** delete
`process_buckets`/`process_history_and_buckets` — repair depends on the latter. (If B's
inlined discovery duplicates `process_history_and_buckets`'s logic, leave the duplication
on this experiment branch, or share a tiny helper — but keep the method repair calls.)

- [ ] **Step 4: Build + correctness gate**

```bash
cargo build --release --features perf-metrics --quiet && echo OK
target/release/stellar-archivist scan http://127.0.0.1:8088 -c 128 --max-concurrent 128 \
  --verify --skip-optional --low 63045000 --high 63046015 --report /tmp/b.json >/dev/null 2>&1
python3 -c "import json;print(json.load(open('/tmp/b.json'))['summary'])"
```
Expected: `OK`, then `succeeded=239 failed=0`.

- [ ] **Step 5: Commit**

```bash
git add src/pipeline.rs src/repair_operation.rs
git commit -m "perf(verify): spawn per file (Strategy B)"
```

---

## Task C: branch `vs/c-spawn-blocking` — decode on the blocking pool

**Idea (A3):** Read the compressed bytes **async** (off the blocking pool), then
`spawn_blocking` only the **pure-CPU gzip decode + hash** over the owned buffer, bounded by
a `Semaphore(~nproc)`. Do **not** use `SyncIoBridge` inside `spawn_blocking` — that
`block_on`s network I/O on a blocking-pool thread, the exact 545-parked-threads
anti-pattern the investigation observed (§9.4). This makes C symmetric with D (both:
async read-all → CPU pool), so they differ *only* in the pool (tokio-blocking vs rayon) —
a clean comparison. No pipeline changes; modify the decode functions directly.

**Files:** Modify `src/verify.rs` (`verify_bucket_stream`), `src/xdr_verify.rs`
(`decompress_to_buffer`). **Keep** `verify_bucket_maybe_write` and
`decompress_and_write_internal` — the mirror verify-on-write path uses them
(`verify.rs:166` `verify_and_write_bucket`; `xdr_verify.rs:1192` `verify_and_write_xdr`).
`tokio-util` already has `io-util` (`Cargo.toml:69`) — no Cargo change needed. Add a
module-level decode semaphore.

- [ ] **Step 1: Add a bounded decode gate (both files share the cap)**

In `src/verify.rs` (and reference it from `xdr_verify.rs`):

```rust
use std::sync::OnceLock;
use tokio::sync::Semaphore;
/// Bound concurrent CPU decode jobs to ~cores so the blocking pool isn't oversubscribed.
pub(crate) fn decode_sem() -> &'static Semaphore {
    static S: OnceLock<Semaphore> = OnceLock::new();
    S.get_or_init(|| Semaphore::new(num_cpus::get().max(1)))   // or std::thread::available_parallelism
}
```

- [ ] **Step 2: Bucket — async read, then `spawn_blocking` CPU decode+hash — `src/verify.rs`**

Replace `verify_bucket_stream`'s body:

```rust
pub async fn verify_bucket_stream(path: &str, reader: Reader) -> Result<(), StorageError> {
    let _g = crate::phase!(crate::metrics::Phase::BucketStream);
    let expected = bucket_hash_from_path(path)
        .ok_or_else(|| StorageError::fatal(format!("Invalid bucket path: {}", path)))?;
    let compressed = reader.read(..).await                       // ASYNC I/O (not on the pool)
        .map_err(|e| from_opendal_error(e, &format!("Failed to read {}", path)))?
        .to_vec();
    let path_owned = path.to_string();
    let _permit = decode_sem().acquire().await.unwrap();         // bound to ~nproc
    let (actual, n) = tokio::task::spawn_blocking(move || {
        let _dg = crate::metrics::DecodeGuard::enter();
        use std::io::Read as _;
        let mut dec = flate2::read::GzDecoder::new(std::io::Cursor::new(compressed));
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; HASH_BUFFER_SIZE];
        let mut n: u64 = 0;
        loop { let k = dec.read(&mut buf)?; if k == 0 { break; } hasher.update(&buf[..k]); n += k as u64; }
        Ok::<_, std::io::Error>((hex::encode(hasher.finalize()), n))
    })
    .await
    .map_err(|e| StorageError::fatal(format!("hash task panicked {}: {}", path_owned, e)))?
    .map_err(|e| StorageError::retry(format!("decompress failed {}: {}", path_owned, e)))?;
    if actual != expected {
        return Err(StorageError::fatal(format!(
            "Hash mismatch for {}: expected {}, got {}", path, expected, actual)));
    }
    crate::metrics::add_bytes(crate::metrics::Phase::BucketStream, n);
    crate::metrics::record_file(n);
    Ok(())
}
```

- [ ] **Step 3: XDR — async read, then `spawn_blocking` CPU decode — `src/xdr_verify.rs`**

Replace `decompress_to_buffer`'s body (the parse stays on the caller after `.await` — C
isolates the *decode* offload; if C still plateaus, that favors the parse hypothesis, see
Strategy F):

```rust
async fn decompress_to_buffer(path: &str, reader: Reader) -> Result<Vec<u8>, StorageError> {
    let compressed = reader.read(..).await
        .map_err(|e| from_opendal_error(e, &format!("read {}", path)))?
        .to_vec();
    let p = path.to_string();
    let _permit = crate::verify::decode_sem().acquire().await.unwrap();
    tokio::task::spawn_blocking(move || {
        let _dg = crate::metrics::DecodeGuard::enter();
        use std::io::Read as _;
        let mut dec = flate2::read::GzDecoder::new(std::io::Cursor::new(compressed));
        let mut out = Vec::new();
        dec.read_to_end(&mut out).map(|_| out)
    })
    .await
    .map_err(|e| StorageError::fatal(format!("decompress task panicked {}: {}", path, e)))?
    .map_err(|e| StorageError::retry(format!("decompress {}: {}", p, e)))
}
```

> `Reader::read(..)` returns `opendal::Buffer`; `.to_vec()` → `Vec<u8>` (confirm in opendal
> 0.55, else collect `into_stream`). This buffers the whole compressed file — same memory
> tradeoff as D, captured by the benchmark's `peak_rss_mb` column (Task F / A7).

- [ ] **Step 4: Build + correctness gate** (same commands as Task A Step 4; expect `239/0`).

- [ ] **Step 5: Commit**

```bash
git add src/verify.rs src/xdr_verify.rs
git commit -m "perf(verify): decode on tokio blocking pool, async feed (Strategy C)"
```

---

## Task D: branch `vs/d-rayon` — decode on a rayon pool

**Idea:** Read the whole compressed body async (feed), then decode+hash on a dedicated
rayon pool sized to cores, off the async I/O workers.

**Files:** `Cargo.toml` (+rayon); create `src/decode_pool.rs`; `src/lib.rs` (`mod`);
modify `src/verify.rs`, `src/xdr_verify.rs`.

- [ ] **Step 1: Cargo.toml**

```toml
rayon = "1"
```

- [ ] **Step 2: Create `src/decode_pool.rs`**

```rust
//! Strategy D: a process-global rayon pool for CPU-bound decode/hash, kept off the async
//! I/O workers. Async reads bytes; rayon decodes; result returns via a tokio oneshot.
use std::sync::OnceLock;
static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
fn pool() -> &'static rayon::ThreadPool {
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .thread_name(|i| format!("decode-{i}"))
            .build()
            .expect("build rayon decode pool")
    })
}
pub async fn run<T, F>(f: F) -> T
where F: FnOnce() -> T + Send + 'static, T: Send + 'static {
    let (tx, rx) = tokio::sync::oneshot::channel();
    pool().spawn(move || {
        let _dg = crate::metrics::DecodeGuard::enter();
        let _ = tx.send(f());
    });
    rx.await.expect("decode pool task dropped")
}
```

- [ ] **Step 3: `src/lib.rs` — register the module**

```rust
mod decode_pool;
```

- [ ] **Step 4: `src/verify.rs` — bucket decode on rayon**

```rust
pub async fn verify_bucket_stream(path: &str, reader: Reader) -> Result<(), StorageError> {
    let _g = crate::phase!(crate::metrics::Phase::BucketStream);
    let expected = bucket_hash_from_path(path)
        .ok_or_else(|| StorageError::fatal(format!("Invalid bucket path: {}", path)))?;
    let compressed = reader
        .read(..)
        .await
        .map_err(|e| from_opendal_error(e, &format!("read {}", path)))?
        .to_vec();
    let path_owned = path.to_string();
    let (actual, n) = crate::decode_pool::run(move || {
        use std::io::Read as _;
        let mut dec = flate2::read::GzDecoder::new(std::io::Cursor::new(compressed));
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; HASH_BUFFER_SIZE];
        let mut n: u64 = 0;
        loop { let k = dec.read(&mut buf)?; if k == 0 { break; } hasher.update(&buf[..k]); n += k as u64; }
        Ok::<_, std::io::Error>((hex::encode(hasher.finalize()), n))
    })
    .await
    .map_err(|e| StorageError::retry(format!("decompress {}: {}", path_owned, e)))?;
    if actual != expected {
        return Err(StorageError::fatal(format!(
            "Hash mismatch for {}: expected {}, got {}", path, expected, actual)));
    }
    crate::metrics::add_bytes(crate::metrics::Phase::BucketStream, n);
    crate::metrics::record_file(n);
    Ok(())
}
```

- [ ] **Step 5: `src/xdr_verify.rs` — XDR decode on rayon**

```rust
async fn decompress_to_buffer(path: &str, reader: Reader) -> Result<Vec<u8>, StorageError> {
    let compressed = reader
        .read(..)
        .await
        .map_err(|e| from_opendal_error(e, &format!("read {}", path)))?
        .to_vec();
    let p = path.to_string();
    crate::decode_pool::run(move || {
        use std::io::Read as _;
        let mut dec = flate2::read::GzDecoder::new(std::io::Cursor::new(compressed));
        let mut out = Vec::new();
        dec.read_to_end(&mut out).map(|_| out)
    })
    .await
    .map_err(|e| StorageError::retry(format!("decompress {}: {}", p, e)))
}
```

> Confirm `opendal::Reader::read(..) -> Buffer` exists in the pinned opendal (0.55); if
> not, collect `reader.into_stream(..)` into a `Vec`. Note: this buffers the whole
> compressed file in memory (e.g. ~2.4 GB for the largest bucket) — a known tradeoff vs C.

- [ ] **Step 6: Build + correctness gate** (expect `239/0`).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml src/decode_pool.rs src/lib.rs src/verify.rs src/xdr_verify.rs
git commit -m "perf(verify): rayon decode pool (Strategy D)"
```

---

## Task E: branch `vs/e-parse-in-task` — parse+hash inside the spawned decode task

**Idea:** Move the XDR parse+hash *into* the already-spawned decode task so the
orchestration task only feeds. (Buckets already hash in their spawned task.) **A4:** use
**one generic helper** instead of three copies of the stream+channel+spawn boilerplate.

**Files:** Modify `src/xdr_verify.rs` (add `decompress_then`; rewrite
`parse_ledger_header_stream`, `parse_transactions_stream`, `parse_results_stream` as
one-liners over it).

- [ ] **Step 1: Add the generic `decompress_then` helper — `src/xdr_verify.rs`**

Decodes in the spawned task and runs the caller's parse closure **inside** that task, so
both gzip and parse+hash leave the orchestration task; the caller only feeds:

```rust
/// Stream `reader` → mpsc → a spawned task that gzip-decodes to a buffer and runs `parse`
/// on it. The feed loop runs on the caller; the spawned task does decode + parse+hash.
async fn decompress_then<T, F>(path: &str, reader: Reader, parse: F) -> Result<T, StorageError>
where
    T: Send + 'static,
    F: FnOnce(&[u8]) -> Result<T, StorageError> + Send + 'static,
{
    let stream = reader
        .into_stream(..)
        .await
        .map_err(|e| from_opendal_error(e, &format!("stream {}", path)))?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(CHANNEL_CAPACITY);
    let path_owned = path.to_string();
    let task = tokio::spawn(async move {
        let _dg = crate::metrics::DecodeGuard::enter();
        let sr = StreamReader::new(
            tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, std::io::Error>));
        let mut dec = GzipDecoder::new(BufReader::new(sr));
        let mut decompressed = Vec::new();
        dec.read_to_end(&mut decompressed)
            .await
            .map_err(|e| StorageError::retry(format!("decompress {}: {}", path_owned, e)))?;
        parse(&decompressed)
    });
    futures_util::pin_mut!(stream);
    while let Some(chunk) = stream.next().await {
        let buf = chunk.map_err(|e| from_opendal_error(e, &format!("read {}", path)))?;
        for c in buf {
            if tx.send(c).await.is_err() {
                break;
            }
        }
    }
    drop(tx);
    task.await
        .map_err(|e| StorageError::fatal(format!("parse task panicked {}: {}", path, e)))?
}
```

- [ ] **Step 2: Rewrite the three `parse_*_stream` as one-liners over the helper**

Each becomes a single `decompress_then(...)` call with its parse closure (match each
function's existing return type — e.g. `parse_ledger_header_stream`'s real type; do not
guess):

```rust
pub async fn parse_transactions_stream(path: &str, reader: Reader) -> Result<BTreeMap<u32, Hash>, StorageError> {
    let cp = history_format::checkpoint_from_path(path);
    decompress_then(path, reader, move |b| parse_transaction_entries_for_checkpoint(b, cp)).await
}

pub async fn parse_ledger_header_stream(path: &str, reader: Reader) -> Result</* real type */, StorageError> {
    let cp = history_format::checkpoint_from_path(path);
    decompress_then(path, reader, move |b| parse_ledger_header_entries_for_checkpoint(b, cp)).await
}

pub async fn parse_results_stream(path: &str, reader: Reader) -> Result<BTreeMap<u32, Hash>, StorageError> {
    let cp = history_format::checkpoint_from_path(path);
    decompress_then(path, reader, move |b| parse_result_entries_for_checkpoint(b, cp)).await
}
```

(`parse_scp_stream` has no hashing — leave it as is. Confirm each `parse_*_stream`'s
current return type and substitute it in the helper's `T`.)

- [ ] **Step 3: Build + correctness gate** (expect `239/0`).

- [ ] **Step 4: Commit**

```bash
git add src/xdr_verify.rs
git commit -m "perf(verify): parse+hash inside the spawned decode task (Strategy E)"
```

---

## Task F: branch `vs/f-parse-spawn-blocking` — offload only the sync parse

**Idea (A5):** Keep the async decode unchanged; offload **only** the synchronous
`parse_*_entries_for_checkpoint` to `spawn_blocking`, bounded by a `Semaphore(~nproc)`
(it's CPU on the 512-thread blocking pool). Smallest change that moves the §9.3 hotspot
(XDR parse + tx/result hash) off the orchestration task — and the clean **parse
discriminator** complementing C/D's **decode discriminator**: if F lifts cores and C/D
don't, XDR parse+hash is the cap.

**Files:** Modify `src/xdr_verify.rs` only.

- [ ] **Step 1: Add a bounded parse gate + wrap the sync parse**

```rust
use std::sync::OnceLock;
use tokio::sync::Semaphore;
fn parse_sem() -> &'static Semaphore {
    static S: OnceLock<Semaphore> = OnceLock::new();
    S.get_or_init(|| Semaphore::new(std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8)))
}

pub async fn parse_transactions_stream(path: &str, reader: Reader) -> Result<BTreeMap<u32, Hash>, StorageError> {
    let decompressed = decompress_to_buffer(path, reader).await?;     // async decode, UNCHANGED
    let cp = history_format::checkpoint_from_path(path);
    let _permit = parse_sem().acquire().await.unwrap();
    let p = path.to_string();
    tokio::task::spawn_blocking(move || parse_transaction_entries_for_checkpoint(&decompressed, cp))
        .await
        .map_err(|e| StorageError::fatal(format!("parse task panicked {}: {}", p, e)))?
}
```

- [ ] **Step 2: Same for `parse_ledger_header_stream` and `parse_results_stream`**

Identical wrapper around `parse_ledger_header_entries_for_checkpoint` /
`parse_result_entries_for_checkpoint` (match each real return type). Leave
`parse_scp_stream` and `decompress_to_buffer` untouched.

- [ ] **Step 3: Build + correctness gate** (expect `239/0`).

- [ ] **Step 4: Commit**

```bash
git add src/xdr_verify.rs
git commit -m "perf(verify): offload sync parse to spawn_blocking (Strategy F)"
```

---

## Task BENCH: Cross-branch benchmark suite

**Files:** Create `scripts/perf/bench_strategies.sh` (on the base branch / each worktree —
keep one canonical copy). Build each branch into a distinctly-named binary, then sweep.

**Test matrix.** For each binary `B` in `{base, a, b, c, d, e, f}`:
1. **Full test suite** (correctness floor): `cargo test --release` on the branch — must pass.
2. **Correctness vs base:** completing `scan --verify` on a bounded range
   (`SA_LOW_C..SA_HIGH_C`, default ~1000 cp), `--report`; must match base's
   `(succeeded, failed, retries)` **and** the sorted broken-file set (`broken_sig`).
3. **Core usage (verify, multi-L10):** `scan --verify` on `SA_LOW..SA_HIGH`
   (default 3×L10 `50463103..63046015`) for `SA_WINDOW` s; capture RTM `busy_cores`
   mean/max. **Partial-run sample** (we `kill -9` after the window) — good for *relative*
   ranking, not a completion average; the bounded-range `verify_wall` is the completion
   metric. Also capture **peak RSS** (A7: C and D buffer whole compressed files; their
   memory cost is otherwise invisible) from the `perf-metrics` `timeseries.csv`.
4. **No-verify regression (same multi-L10 range):** `scan` (no `--verify`) for `SA_WINDOW`
   s; capture cores. Must not regress vs base.
5. **Panic gate (A8):** any `task panicked` / `task cancelled` line in a run's stderr
   fails that binary — a `JoinError` is a real bug, never a recorded hash mismatch.

- [ ] **Step 1: Build every branch's instrumented binary**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
mkdir -p bin
# base (the shared starting point = no-fix reference)
( cd /home/jay/Projects/rs-stellar-archivist-verifyperf && \
  RUSTFLAGS="--cfg tokio_unstable" cargo build --release --features perf-metrics --quiet && \
  cp target/release/stellar-archivist bin/sa-base )
for b in a-spawn-checkpoint b-spawn-file c-spawn-blocking d-rayon e-parse-in-task f-parse-spawn-blocking; do
  ( cd "../vs-$b" && RUSTFLAGS="--cfg tokio_unstable" cargo build --release --features perf-metrics --quiet \
    && cp target/release/stellar-archivist "/home/jay/Projects/rs-stellar-archivist-verifyperf/bin/sa-${b%%-*}" )
done   # → bin/sa-base, sa-a … sa-f
```

- [ ] **Step 2: Create `scripts/perf/bench_strategies.sh`**

```bash
#!/usr/bin/env bash
# Compare verify-scaling strategy binaries on correctness, core usage, verify (multi-L10),
# and no-verify. Run with the miniserve local-sim up on :8088.
set -o pipefail
export PATH="$HOME/.cargo/bin:$PATH"
ARCHIVE="${SA_ARCHIVE:-http://127.0.0.1:8088}"
BINS="${SA_BINS:-base a b c d e f}"; PREFIX="${SA_PREFIX:-bin/sa-}"
LOW="${SA_LOW:-50463103}"; HIGH="${SA_HIGH:-63046015}"         # multi-L10
LOWC="${SA_LOW_C:-62982015}"; HIGHC="${SA_HIGH_C:-63046015}"   # bounded (correctness/wall)
WINDOW="${SA_WINDOW:-60}"
OUT="${SA_OUT:-perf-results/maxconc/verifysim/strategies}"; mkdir -p "$OUT"
cores_of(){ grep '^RTM' "$1" 2>/dev/null | python3 -c "import sys,re;b=[float(re.search(r'busy_cores=([\d.]+)',l).group(1)) for l in sys.stdin if 'busy_cores' in l];print(f'{sum(b)/len(b):.1f}/{max(b):.1f}' if b else 'n/a')"; }
# A7: peak RSS from the perf-metrics timeseries.csv (cols: t_s,files_done,bytes_done,peak_rss_mb)
rss_of(){ awk -F, 'NR>1 && $4>m{m=$4} END{printf "%.0f", m+0}' "$1/timeseries.csv" 2>/dev/null; }
summ(){ python3 -c "import json,sys;d=json.load(open(sys.argv[1]))['summary'];print(d['succeeded'],d['failed'],d['retries'])" "$1" 2>/dev/null; }
sig(){ python3 -c "import json,sys,hashlib;d=json.load(open(sys.argv[1]));print(hashlib.sha256(repr(sorted((d.get('files') or {}).items())).encode()).hexdigest()[:16])" "$1" 2>/dev/null; }
panicked(){ grep -qE 'task (panicked|cancelled)' "$@" 2>/dev/null && echo PANIC || echo ok; }
printf '%-6s %-18s %-12s %-16s %-10s %-8s %-14s %-6s\n' bin correctness broken_sig verify_cores* verify_wall rss_mb noverify_cores panic
for k in $BINS; do
  B="${PREFIX}${k}"
  rep="$OUT/${k}.json"; t0=$(date +%s.%N)
  "$B" scan "$ARCHIVE" -c 128 --max-concurrent 128 --verify --skip-optional --low "$LOWC" --high "$HIGHC" --report "$rep" >/dev/null 2>"$OUT/${k}_c.err"
  vw=$(awk -v a=$t0 -v b=$(date +%s.%N) 'BEGIN{printf "%.1f",b-a}')
  # verify multi-L10 window; SA_PERF_OUT captures timeseries.csv (peak RSS) even when killed
  rm -rf "$OUT/${k}_v"; SA_PERF_OUT="$OUT/${k}_v" SA_RT_METRICS=1 "$B" scan "$ARCHIVE" -c 128 --max-concurrent 128 --verify --skip-optional --low "$LOW" --high "$HIGH" >/dev/null 2>"$OUT/${k}_v.rtm" & p=$!; sleep "$WINDOW"; kill -9 $p 2>/dev/null
  SA_RT_METRICS=1 "$B" scan "$ARCHIVE" -c 128 --max-concurrent 128 --skip-optional --low "$LOW" --high "$HIGH" >/dev/null 2>"$OUT/${k}_n.rtm" & p=$!; sleep "$WINDOW"; kill -9 $p 2>/dev/null
  printf '%-6s %-18s %-12s %-16s %-10s %-8s %-14s %-6s\n' "$k" "$(summ "$rep")" "$(sig "$rep")" "$(cores_of "$OUT/${k}_v.rtm")" "$vw" "$(rss_of "$OUT/${k}_v")" "$(cores_of "$OUT/${k}_n.rtm")" "$(panicked "$OUT/${k}_c.err" "$OUT/${k}_v.rtm" "$OUT/${k}_n.rtm")"
done
# * verify_cores is a partial-run (kill -9 after $WINDOW) sample for ranking, not a completion average.
```

- [ ] **Step 3: Run + record**

```bash
chmod +x scripts/perf/bench_strategies.sh
scripts/perf/bench_strategies.sh | tee perf-results/maxconc/verifysim/strategies/summary.txt
```

- [ ] **Step 4: Acceptance criteria**

- **Correctness (hard gate):** every binary's `cargo test` passes; `correctness` +
  `broken_sig` match `base`; and `panic == ok`. Any mismatch/PANIC → reject that strategy.
- **Verify scaling:** rank by `verify_cores` mean + bounded `verify_wall`. Predicted
  (§"six strategies" + amendment ranking): **A ≈ B** (full) saturate; **E ≈ F** (parse
  offloaded, single-task feed remains) near-full unless feed-bound; **C ≈ D** (decode
  offloaded, XDR parse+hash remains) plateau ~base. If C/D do *not* plateau, that refutes
  the §9.3 "parse is heavy" model — a valuable finding.
- **No-verify:** `noverify_cores` must match `base` within noise (no regression).
- **Memory:** compare `rss_mb` — C and D buffer whole compressed files (up to ~2.4 GB ×
  in-flight); A/B/E/F stream. A large `rss_mb` for C/D is the cost to weigh against their
  (predicted small) scaling benefit.
- **Tie-breakers:** code cleanliness (lines changed), memory, and the
  `XdrVerificationManager` `Mutex` (a plateau < 32 cores with `runnable_q>0` → lock
  contention → Task G).

- [ ] **Step 5: Commit the harness + results (on the base branch)**

```bash
git add scripts/perf/bench_strategies.sh
git add -f perf-results/maxconc/verifysim/strategies/
git commit -m "perf(verify): cross-branch benchmark suite for strategies A–F + results"
```

---

## Task G: compose the winner + shard the manager `Mutex` (productionization)

**Why (A6):** the mutually-exclusive branches answer *attribution* ("which lever helps
most") but not the *production* question, which is likely a **composition** — e.g. the
best distribution winner (A/B) *plus* a parse offload (E/F) *plus* removing the next
bottleneck. The investigation itself flags that next bottleneck: once decode parallelizes,
the global `XdrVerificationManager` `Mutex` (`record_*` + `verify_and_release`,
`pipeline.rs:404`) may contend at 32-way (§9.6).

**Files:** branch `vs/g-compose` off the **benchmark winner's** branch; modify
`src/xdr_verify.rs` (`XdrVerificationManager`).

- [ ] **Step 1: Gate on the observed signal**

Only do this if the winning branch plateaus **< 32 cores with `runnable_q > 0`** in the
benchmark (CPU spinning on the lock, not starved). If it already saturates, record that
and stop — no sharding needed.

- [ ] **Step 2: Shard `pending` (and friends) by `cp % N`**

Replace `pending: Mutex<HashMap<u32, PendingCheckpoint>>` (`xdr_verify.rs:171`) with
`pending: [Mutex<HashMap<u32, PendingCheckpoint>>; N]` (N = e.g. 16), keyed by `cp % N`,
or use `DashMap`. Ensure `verify_and_release` does **remove-under-lock then
compute-outside-lock** (don't hold the shard lock across the CPU verify). Keep `boundaries`
/ `errors` correctness (those are touched once per cp / on error — lower contention; shard
only if measured).

- [ ] **Step 3: If still feed-bound, layer E/F's parse offload onto the winner**

If the distribution winner still shows single-task **feed** headroom (`stream.next` +
per-chunk `tx.send` for ~800 files on the orchestration task), compose E or F's
parse-offload on top and re-measure.

- [ ] **Step 4: Re-run the benchmark for `g` vs the single-strategy winners; record**

```bash
# build vs/g-compose → bin/sa-g, then:
SA_BINS="base a b g" scripts/perf/bench_strategies.sh | tee -a perf-results/maxconc/verifysim/strategies/summary.txt
git add -f perf-results/maxconc/verifysim/strategies/ ; git commit -m "perf(verify): composed winner + sharded manager (Task G)"
```

---

## Task H: winner capability graph — throughput vs cores

**Why:** the benchmark (Task BENCH) ranks strategies at a fixed `-c=128` on 32 cores. The
*capability* question is whether the winner actually **scales with cores** — the baseline
was flat at ~3 cores regardless. Produce a strong-scaling curve for the winner (and the
**base** as the flat reference) over the **same multi-L10 range** (196,608 cp), varying
the number of physical cores.

**Files:** Create `scripts/perf/capability_graph.sh`; outputs CSV + a plot via the
existing `scripts/perf/plot.py`.

- [ ] **Step 1: Sweep cores with `taskset` (winner + base)**

Pin the process to N physical cores (the OS then caps decode parallelism at N, regardless
of tokio's worker count); measure throughput over a fixed window. `SA_PERF_OUT`'s
`timeseries.csv` gives decompressed `bytes_done` even on a killed run.

```bash
#!/usr/bin/env bash
# capability_graph.sh — throughput (+ cores) vs N physical cores, for the winner & base.
set -o pipefail; export PATH="$HOME/.cargo/bin:$PATH"
ARCHIVE="${SA_ARCHIVE:-http://127.0.0.1:8088}"
LOW="${SA_LOW:-50463103}"; HIGH="${SA_HIGH:-63046015}"   # 196,608 cp = 3×L10
WINDOW="${SA_WINDOW:-60}"; C="${SA_C:-128}"
NCORES="${SA_NCORES:-1 2 4 8 16 24 32}"
OUT="${SA_OUT:-perf-results/maxconc/verifysim/capability}"; mkdir -p "$OUT"
echo "bin,cores,decompressed_mb_per_s,busy_cores_mean" > "$OUT/capability.csv"
for BIN in "${SA_WINNER:-bin/sa-a}" bin/sa-base; do
  for n in $NCORES; do
    d="$OUT/$(basename "$BIN")_n${n}"; rm -rf "$d"
    taskset -c "0-$((n-1))" env SA_PERF_OUT="$d" SA_RT_METRICS=1 "$BIN" scan "$ARCHIVE" \
      -c "$C" --max-concurrent "$C" --verify --skip-optional --low "$LOW" --high "$HIGH" \
      >/dev/null 2>"$d.rtm" & p=$!; sleep "$WINDOW"; kill -9 $p 2>/dev/null
    mbps=$(awk -F, 'NR>1{b=$3} END{printf "%.0f", (b+0)/'"$WINDOW"'/1e6}' "$d/timeseries.csv" 2>/dev/null)
    cores=$(grep '^RTM' "$d.rtm" 2>/dev/null | python3 -c "import sys,re;v=[float(re.search(r'busy_cores=([\d.]+)',l).group(1)) for l in sys.stdin if 'busy_cores' in l];print(f'{sum(v)/len(v):.1f}' if v else '0')")
    echo "$(basename "$BIN"),$n,${mbps:-0},${cores:-0}" | tee -a "$OUT/capability.csv"
  done
done
```

- [ ] **Step 2: Plot throughput vs cores (winner vs base)**

```bash
chmod +x scripts/perf/capability_graph.sh
SA_WINNER=bin/sa-<winner> scripts/perf/capability_graph.sh
python3 scripts/perf/plot.py "$OUT/capability.csv"   # cores (x) vs decompressed_mb_per_s (y), one line per bin
```
Expected shape: **winner ≈ near-linear** up to where the workload's parallelism or the next
limit (manager `Mutex` / the single-stream long-pole on a shared big bucket) caps it;
**base ≈ flat ~3 cores' worth** at every N (the bottleneck this whole effort removes).

- [ ] **Step 3: Commit**

```bash
git add scripts/perf/capability_graph.sh
git add -f perf-results/maxconc/verifysim/capability/
git commit -m "perf(verify): winner capability graph (throughput vs cores)"
```

> If "different cores" should instead mean a **`-c` sweep** (concurrency, not physical
> cores), swap the `taskset` loop for a `-c ∈ {4,8,16,32,64,128,256}` loop at fixed 32
> cores — but the physical-core sweep above is the canonical "does it scale with cores"
> capability graph.

---

## Self-review notes (gaps the implementer must close)

- **Repair caller (A, B):** only the **`run_checkpoints`** call
  (`repair_operation.rs:494`) needs the `Arc` wrap — `run_checkpoints` is the one becoming
  `self: Arc<Self>`. `process_file` (`:447`) and `process_history_and_buckets` (`:445`)
  **keep `&self`** (A2), so those repair call sites are untouched. **Do not delete
  `process_history_and_buckets`** — repair depends on it (the original plan's deletion
  note was wrong; corrected in Task B).
- **Keep the mirror decode helpers (C):** `verify_bucket_maybe_write` (`verify.rs`) and
  `decompress_and_write_internal` (`xdr_verify.rs`) are used by the mirror verify-on-write
  path (`verify_and_write_bucket` `verify.rs:166`, `verify_and_write_xdr`
  `xdr_verify.rs:1192`) — verified; **do not delete them**. C only rewrites the scan-path
  `verify_bucket_stream` / `decompress_to_buffer`.
- **`Reader::read(..)` (C, D):** returns `opendal::Buffer`; `.to_vec()` → `Vec<u8>`.
  Confirm in opendal 0.55; else collect `into_stream`.
- **E/F return types:** match each `parse_*_stream`'s real signature (the ledger-header
  type in particular — don't guess).
- **Manager lock:** Task G handles it; only act on the measured signal (plateau < 32 cores
  with `runnable_q > 0`).
- **Mirror parity:** these branches change the *scan-verify* decode; if a branch also
  affects the mirror verify-on-write path (shared functions), run a mirror smoke test too.
- **`tokio-util` `io-util`** already present (`Cargo.toml:69`) — no Cargo change for C (A7).
