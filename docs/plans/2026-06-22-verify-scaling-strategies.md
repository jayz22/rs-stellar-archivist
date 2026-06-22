# Verify-Scaling Strategies — Implementation & Benchmark Plan

> **For agentic workers:** REQUIRED SUB-SKILL: use superpowers:subagent-driven-development
> or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Implement five **mutually-exclusive** candidate fixes (A–E) for the verify-mode
scaling bottleneck, **each on its own branch off a common base**, then run one benchmark
suite that compares them on correctness, core usage, scan-verify (multi-L10), and
scan-no-verify.

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

## The five strategies (mutually exclusive; each its own branch)

| branch | idea | moves off the orchestration task | leaves on it | full fix? |
|---|---|---|---|---|
| `vs/a-spawn-checkpoint` | spawn each checkpoint | *everything* per-cp incl. `verify_and_release` | iteration only | **yes** |
| `vs/b-spawn-file` | spawn each file | feed + decode + parse + hash | iteration, HAS discovery, `verify_and_release` | mostly |
| `vs/c-spawn-blocking` | decode on tokio blocking pool | feed + decode + parse (bucket & xdr) | `verify_and_release` | partial |
| `vs/d-rayon` | decode on a rayon pool | decode + parse (bucket & xdr) | feed, `verify_and_release` | partial |
| `vs/e-parse-in-task` | parse+hash inside the spawned decode task | XDR parse + hash | feed, `verify_and_release` | partial |

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

- [ ] **Step 2: Create the five branches off the base (use worktrees for parallel builds)**

```bash
BASE=d0266b2
for b in a-spawn-checkpoint b-spawn-file c-spawn-blocking d-rayon e-parse-in-task; do
  git worktree add -b "vs/$b" "../vs-$b" "$BASE"
done
git worktree list
```
Expected: five worktrees `../vs-a-spawn-checkpoint` … `../vs-e-parse-in-task`, each at `vs/<b>`.

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
                    if let Err(e) =
                        tokio::spawn(async move { me.process_checkpoint(ck).await }).await
                    {
                        error!("checkpoint {ck} task panicked: {e}");
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
`process_checkpoint`, add `process_file_owned`); `src/repair_operation.rs`.

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

- [ ] **Step 3: Convert `process_checkpoint` + `process_file` to spawn per file**

`process_checkpoint` becomes `self: Arc<Self>` and spawns each file via a `JoinSet`.
Replace `process_checkpoint` (`pipeline.rs:378`) with:

```rust
    pub async fn process_checkpoint(self: Arc<Self>, checkpoint: u32) {
        use tokio::task::JoinSet;
        let mut set: JoinSet<()> = JoinSet::new();

        for cat in ["ledger", "transactions", "results"] {
            let me = Arc::clone(&self);
            let path = checkpoint_path(cat, checkpoint);
            set.spawn(async move { me.process_file(checkpoint, path).await });
        }
        if !self.config.skip_optional {
            let me = Arc::clone(&self);
            let path = checkpoint_path("scp", checkpoint);
            set.spawn(async move { me.process_file(checkpoint, path).await });
        }
        if !self.config.skip_history_and_buckets {
            if let Some((state, buffer)) = self.fetch_history_file_state(checkpoint).await {
                let history_path = checkpoint_path("history", checkpoint);
                self.process_history_file(checkpoint, &history_path, buffer).await;
                let bucket_paths: Vec<String> = {
                    let mut cache = self.bucket_lru.lock().unwrap();
                    state
                        .buckets()
                        .iter()
                        .filter_map(|b| {
                            (cache.put(b.clone(), ()).is_none())
                                .then(|| bucket_path(b).ok())
                                .flatten()
                        })
                        .collect()
                };
                for path in bucket_paths {
                    let me = Arc::clone(&self);
                    set.spawn(async move { me.process_file(checkpoint, path).await });
                }
            }
        }

        while set.join_next().await.is_some() {}

        if let Some(manager) = &self.verification_manager {
            manager.verify_and_release(checkpoint);
        }
    }
```

`process_file` becomes `self: Arc<Self>` and acquires a permit. Replace `process_file`
(`pipeline.rs:540`)'s signature/body wrapper:

```rust
    pub(crate) async fn process_file(self: Arc<Self>, checkpoint: u32, path: String) {
        let _permit = self.file_semaphore.clone().acquire_owned().await;
        // ... unchanged body: with_retries(process_object) + record stats ...
    }
```

> The old per-checkpoint helpers `process_checkpoint`/`process_history_and_buckets` used
> `join_all`; this branch inlines bucket discovery into `process_checkpoint` and removes
> `process_buckets`/`process_history_and_buckets` if now unused (delete dead code — clean
> implementation). `process_history_file` stays `&self` (called inline before the spawns).

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

**Idea:** Run feed + gzip + parse/hash on tokio's blocking pool via `spawn_blocking`, for
both bucket and XDR files (feed via `SyncIoBridge` inside the blocking task). No pipeline
changes — modify the decode functions directly.

**Files:** Modify `src/verify.rs` (`verify_bucket_stream`), `src/xdr_verify.rs`
(`decompress_to_buffer` or the decompress path), `Cargo.toml` (tokio-util `io-util`).

- [ ] **Step 1: Replace the async bucket decode with the sync blocking version — `src/verify.rs`**

Replace `verify_bucket_stream`'s body (it currently delegates to the async
`verify_bucket_maybe_write`) with a `spawn_blocking` decode, and delete the now-unused
async `verify_bucket_maybe_write` *for the scan path* (keep it only if the mirror path
still needs it — check callers with `grep -n verify_bucket_maybe_write src`):

```rust
use tokio_util::io::SyncIoBridge;
use tokio_util::compat::FuturesAsyncReadCompatExt;
use std::io::Read as _;
const SYNC_READ_BUF: usize = 256 * 1024;

pub async fn verify_bucket_stream(path: &str, reader: Reader) -> Result<(), StorageError> {
    let _g = crate::phase!(crate::metrics::Phase::BucketStream);
    let expected = bucket_hash_from_path(path)
        .ok_or_else(|| StorageError::fatal(format!("Invalid bucket path: {}", path)))?;
    let async_read = reader
        .into_futures_async_read(..)
        .await
        .map_err(|e| from_opendal_error(e, &format!("Failed to read {}", path)))?
        .compat();
    let bridge = SyncIoBridge::new(async_read);
    let path_owned = path.to_string();
    let (actual, n) = tokio::task::spawn_blocking(move || {
        let _dg = crate::metrics::DecodeGuard::enter();
        let mut dec =
            flate2::read::GzDecoder::new(std::io::BufReader::with_capacity(SYNC_READ_BUF, bridge));
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; HASH_BUFFER_SIZE];
        let mut n: u64 = 0;
        loop {
            let k = dec.read(&mut buf)?;
            if k == 0 { break; }
            hasher.update(&buf[..k]);
            n += k as u64;
        }
        Ok::<_, std::io::Error>((hex::encode(hasher.finalize()), n))
    })
    .await
    .map_err(|e| StorageError::fatal(format!("hash task panicked {}: {}", path_owned, e)))?
    .map_err(|e| StorageError::retry(format!("decompress failed {}: {}", path_owned, e)))?;
    if actual != expected {
        return Err(StorageError::fatal(format!(
            "Hash mismatch for {}: expected {}, got {}", path, expected, actual
        )));
    }
    crate::metrics::add_bytes(crate::metrics::Phase::BucketStream, n);
    crate::metrics::record_file(n);
    Ok(())
}
```

- [ ] **Step 2: Replace the async XDR decode with `spawn_blocking` — `src/xdr_verify.rs`**

Replace `decompress_to_buffer`'s body (the function `parse_*_stream` already calls) with a
`spawn_blocking` gzip-to-`Vec`, and delete the now-unused async `decompress_and_write_internal`
*if no other caller* (the mirror write path may still use it — check
`grep -n decompress_and_write_internal src`):

```rust
async fn decompress_to_buffer(path: &str, reader: Reader) -> Result<Vec<u8>, StorageError> {
    use tokio_util::io::SyncIoBridge;
    use tokio_util::compat::FuturesAsyncReadCompatExt;
    use std::io::Read as _;
    let async_read = reader
        .into_futures_async_read(..)
        .await
        .map_err(|e| from_opendal_error(e, &format!("read {}", path)))?
        .compat();
    let bridge = SyncIoBridge::new(async_read);
    let p = path.to_string();
    tokio::task::spawn_blocking(move || {
        let _dg = crate::metrics::DecodeGuard::enter();
        let mut dec = flate2::read::GzDecoder::new(std::io::BufReader::with_capacity(256 * 1024, bridge));
        let mut out = Vec::new();
        dec.read_to_end(&mut out)
            .map_err(|e| StorageError::retry(format!("decompress {}: {}", p, e)))?;
        Ok::<_, StorageError>(out)
    })
    .await
    .map_err(|e| StorageError::fatal(format!("decompress task panicked {}: {}", path, e)))?
}
```

(The XDR *parse* stays on the caller after `.await` — this branch isolates the *decode*
offload. The parse is light vs gzip; if C still pins, that's a finding favoring A/B/E.)

- [ ] **Step 3: Cargo.toml — ensure `tokio-util` has `io-util`**

```toml
tokio-util = { version = "0.7", features = ["io", "io-util", "compat"] }
```

- [ ] **Step 4: Build + correctness gate** (same commands as Task A Step 4; expect `239/0`).

- [ ] **Step 5: Commit**

```bash
git add src/verify.rs src/xdr_verify.rs Cargo.toml
git commit -m "perf(verify): decode on tokio blocking pool (Strategy C)"
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

**Idea:** Smallest change. Move the XDR parse+hash *into* the already-spawned decode task
so the orchestration task only feeds. (Buckets already hash in their spawned task.)

**Files:** Modify `src/xdr_verify.rs` (`parse_ledger_header_stream`,
`parse_transactions_stream`, `parse_results_stream`).

- [ ] **Step 1: `parse_transactions_stream` — fuse parse into the spawned task**

```rust
pub async fn parse_transactions_stream(path: &str, reader: Reader) -> Result<BTreeMap<u32, Hash>, StorageError> {
    let cp = history_format::checkpoint_from_path(path);
    let stream = reader.into_stream(..).await
        .map_err(|e| from_opendal_error(e, &format!("stream {}", path)))?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(CHANNEL_CAPACITY);
    let path_owned = path.to_string();
    let task = tokio::spawn(async move {
        let _dg = crate::metrics::DecodeGuard::enter();
        let sr = StreamReader::new(
            tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, std::io::Error>));
        let mut dec = GzipDecoder::new(BufReader::new(sr));
        let mut decompressed = Vec::new();
        dec.read_to_end(&mut decompressed).await
            .map_err(|e| StorageError::retry(format!("decompress {}: {}", path_owned, e)))?;
        parse_transaction_entries_for_checkpoint(&decompressed, cp)   // CPU parse, now in-task
    });
    futures_util::pin_mut!(stream);
    while let Some(chunk) = stream.next().await {
        let buf = chunk.map_err(|e| from_opendal_error(e, &format!("read {}", path)))?;
        for c in buf { if tx.send(c).await.is_err() { break; } }
    }
    drop(tx);
    task.await.map_err(|e| StorageError::fatal(format!("parse task panicked {}: {}", path, e)))?
}
```

- [ ] **Step 2: `parse_ledger_header_stream` — same pattern**

```rust
pub async fn parse_ledger_header_stream(path: &str, reader: Reader) -> Result<LedgerHeaderData, StorageError> {
    let cp = history_format::checkpoint_from_path(path);
    let stream = reader.into_stream(..).await
        .map_err(|e| from_opendal_error(e, &format!("stream {}", path)))?;
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(CHANNEL_CAPACITY);
    let path_owned = path.to_string();
    let task = tokio::spawn(async move {
        let _dg = crate::metrics::DecodeGuard::enter();
        let sr = StreamReader::new(
            tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<_, std::io::Error>));
        let mut dec = GzipDecoder::new(BufReader::new(sr));
        let mut decompressed = Vec::new();
        dec.read_to_end(&mut decompressed).await
            .map_err(|e| StorageError::retry(format!("decompress {}: {}", path_owned, e)))?;
        parse_ledger_header_entries_for_checkpoint(&decompressed, cp)
    });
    futures_util::pin_mut!(stream);
    while let Some(chunk) = stream.next().await {
        let buf = chunk.map_err(|e| from_opendal_error(e, &format!("read {}", path)))?;
        for c in buf { if tx.send(c).await.is_err() { break; } }
    }
    drop(tx);
    task.await.map_err(|e| StorageError::fatal(format!("parse task panicked {}: {}", path, e)))?
}
```

> Use the actual return type of `parse_ledger_header_stream` (check its current signature;
> shown here as `LedgerHeaderData` — substitute the real type).

- [ ] **Step 3: `parse_results_stream` — same pattern**

Identical structure, calling `parse_result_entries_for_checkpoint(&decompressed, cp)` and
returning `BTreeMap<u32, Hash>` (match the current signature). Write the full body — no
"same as above".

- [ ] **Step 4: Build + correctness gate** (expect `239/0`).

- [ ] **Step 5: Commit**

```bash
git add src/xdr_verify.rs
git commit -m "perf(verify): parse+hash inside the spawned decode task (Strategy E)"
```

---

## Task F: Cross-branch benchmark suite

**Files:** Create `scripts/perf/bench_strategies.sh` (on the base branch / each worktree —
keep one canonical copy). Build each branch into a distinctly-named binary, then sweep.

**Test matrix.** For each binary `B` in `{base, a, b, c, d, e}`:
1. **Full test suite** (correctness floor): `cargo test --release` on the branch — must pass.
2. **Correctness vs base:** completing `scan --verify` on a bounded range
   (`SA_LOW_C..SA_HIGH_C`, default ~1000 cp), `--report`; must match base's
   `(succeeded, failed, retries)` **and** the sorted broken-file set (`broken_sig`).
3. **Core usage (verify, multi-L10):** `scan --verify` on `SA_LOW..SA_HIGH`
   (default 3×L10 `50463103..63046015`) for `SA_WINDOW` s; capture RTM `busy_cores`
   mean/max + `active_decodes`. Plus the bounded-range wall (throughput number).
4. **No-verify regression (same multi-L10 range):** `scan` (no `--verify`) for `SA_WINDOW`
   s; capture cores + behavior. Must not regress vs base.

- [ ] **Step 1: Build every branch's instrumented binary**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
mkdir -p bin
# base (the shared starting point = no-fix reference)
( cd /home/jay/Projects/rs-stellar-archivist-verifyperf && \
  RUSTFLAGS="--cfg tokio_unstable" cargo build --release --features perf-metrics --quiet && \
  cp target/release/stellar-archivist bin/sa-base )
for b in a-spawn-checkpoint b-spawn-file c-spawn-blocking d-rayon e-parse-in-task; do
  ( cd "../vs-$b" && RUSTFLAGS="--cfg tokio_unstable" cargo build --release --features perf-metrics --quiet \
    && cp target/release/stellar-archivist "/home/jay/Projects/rs-stellar-archivist-verifyperf/bin/sa-${b%%-*}" )
done   # → bin/sa-base, sa-a, sa-b, sa-c, sa-d, sa-e
```

- [ ] **Step 2: Create `scripts/perf/bench_strategies.sh`**

```bash
#!/usr/bin/env bash
# Compare verify-scaling strategy binaries on correctness, core usage, verify (multi-L10),
# and no-verify. Run with the miniserve local-sim up on :8088.
set -o pipefail
export PATH="$HOME/.cargo/bin:$PATH"
ARCHIVE="${SA_ARCHIVE:-http://127.0.0.1:8088}"
BINS="${SA_BINS:-base a b c d e}"; PREFIX="${SA_PREFIX:-bin/sa-}"
LOW="${SA_LOW:-50463103}"; HIGH="${SA_HIGH:-63046015}"         # multi-L10
LOWC="${SA_LOW_C:-62982015}"; HIGHC="${SA_HIGH_C:-63046015}"   # bounded (correctness/wall)
WINDOW="${SA_WINDOW:-60}"
OUT="${SA_OUT:-perf-results/maxconc/verifysim/strategies}"; mkdir -p "$OUT"
cores_of(){ grep '^RTM' "$1" 2>/dev/null | python3 -c "import sys,re;b=[float(re.search(r'busy_cores=([\d.]+)',l).group(1)) for l in sys.stdin if 'busy_cores' in l];print(f'{sum(b)/len(b):.1f}/{max(b):.1f}' if b else 'n/a')"; }
summ(){ python3 -c "import json,sys;d=json.load(open(sys.argv[1]))['summary'];print(d['succeeded'],d['failed'],d['retries'])" "$1" 2>/dev/null; }
sig(){ python3 -c "import json,sys,hashlib;d=json.load(open(sys.argv[1]));print(hashlib.sha256(repr(sorted((d.get('files') or {}).items())).encode()).hexdigest()[:16])" "$1" 2>/dev/null; }
printf '%-6s %-18s %-12s %-16s %-12s %-14s\n' bin correctness broken_sig verify_cores verify_wall noverify_cores
for k in $BINS; do
  B="${PREFIX}${k}"
  rep="$OUT/${k}.json"; t0=$(date +%s.%N)
  "$B" scan "$ARCHIVE" -c 128 --max-concurrent 128 --verify --skip-optional --low "$LOWC" --high "$HIGHC" --report "$rep" >/dev/null 2>"$OUT/${k}_c.err"
  vw=$(awk -v a=$t0 -v b=$(date +%s.%N) 'BEGIN{printf "%.1f",b-a}')
  SA_RT_METRICS=1 "$B" scan "$ARCHIVE" -c 128 --max-concurrent 128 --verify --skip-optional --low "$LOW" --high "$HIGH" >/dev/null 2>"$OUT/${k}_v.rtm" & p=$!; sleep "$WINDOW"; kill -9 $p 2>/dev/null
  SA_RT_METRICS=1 "$B" scan "$ARCHIVE" -c 128 --max-concurrent 128 --skip-optional --low "$LOW" --high "$HIGH" >/dev/null 2>"$OUT/${k}_n.rtm" & p=$!; sleep "$WINDOW"; kill -9 $p 2>/dev/null
  printf '%-6s %-18s %-12s %-16s %-12s %-14s\n' "$k" "$(summ "$rep")" "$(sig "$rep")" "$(cores_of "$OUT/${k}_v.rtm")" "$vw" "$(cores_of "$OUT/${k}_n.rtm")"
done
```

- [ ] **Step 3: Run + record**

```bash
chmod +x scripts/perf/bench_strategies.sh
scripts/perf/bench_strategies.sh | tee perf-results/maxconc/verifysim/strategies/summary.txt
```

- [ ] **Step 4: Acceptance criteria**

- **Correctness (hard gate):** every binary's `cargo test` passes, and `correctness` +
  `broken_sig` match `base`. Any mismatch → reject that strategy.
- **Verify scaling:** rank by `verify_cores` mean + bounded `verify_wall`. A/B expected to
  saturate; C/D/E reveal whether residual on-main work (feed / `verify_and_release`) caps
  them.
- **No-verify:** `noverify_cores` must match `base` within noise (no regression).
- **Tie-breakers:** code cleanliness (lines changed), memory (D buffers compressed bytes),
  and `XdrVerificationManager` `Mutex` contention (a plateau < 32 cores with `runnable_q>0`
  → lock contention → follow-up sharding task).

- [ ] **Step 5: Commit the harness + results (on the base branch)**

```bash
git add scripts/perf/bench_strategies.sh
git add -f perf-results/maxconc/verifysim/strategies/
git commit -m "perf(verify): cross-branch benchmark suite for strategies A–E + results"
```

---

## Self-review notes (gaps the implementer must close)

- **Repair caller (A, B):** `repair_operation.rs` calls `run_checkpoints` — wrap its
  `Pipeline` in `Arc` to match the new `self: Arc<Self>` signature; verify by compiling.
- **Dead code (B, C):** after restructuring, delete now-unused helpers
  (`process_buckets`/`process_history_and_buckets` in B; async `verify_bucket_maybe_write`
  / `decompress_and_write_internal` in C **only if** the mirror path doesn't still use
  them — `grep` callers first). Clean implementation = remove what the branch no longer uses.
- **`Reader::read(..)` (D):** confirm in opendal 0.55; else collect `into_stream`.
- **E return types:** match each `parse_*_stream`'s real signature (ledger header type).
- **Manager lock:** if a full-fix branch plateaus < 32 cores with `runnable_q>0`, add a
  follow-up task to shard `XdrVerificationManager`'s `Mutex<HashMap>`.
- **Mirror parity:** these branches change the *scan-verify* decode; if a branch also
  affects the mirror verify-on-write path (shared functions), run a mirror smoke test too.
