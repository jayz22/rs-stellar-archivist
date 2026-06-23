//! Strategy D: a process-global rayon pool for CPU-bound decode/hash, kept off the async
//! I/O worker threads. The async side reads bytes; rayon does the CPU; the result returns
//! via a tokio oneshot. Pool size defaults to the number of logical CPUs.
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

/// Run a CPU-bound closure on the rayon pool, awaitable from async code.
pub async fn run<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    pool().spawn(move || {
        let _dg = crate::metrics::DecodeGuard::enter();
        let _ = tx.send(f());
    });
    rx.await.expect("decode pool task dropped")
}
