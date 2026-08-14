//! Unit tests for the StagedWriter commit/abort contract, per backend flavor.
//!
//! Three flavors are exercised through their public constructors:
//! - mock object store (atomic commit-on-close, no base path)
//! - plain filesystem (fs-staged: `.tmp` sibling + rename)
//! - filesystem with atomic_file_writes (OpenDAL atomic_write_dir)

use super::utils::mock_object_store;
use crate::storage::{OpendalStore, StorageConfig, StorageRef};
use crate::test_helpers::test_storage_config;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

// mock_object_store (tests/utils.rs) already returns a StorageRef.

fn plain_fs_store(root: &Path) -> StorageRef {
    // test_storage_config() has atomic_file_writes = false
    Arc::new(OpendalStore::filesystem(root, &test_storage_config()).unwrap())
}

fn atomic_fs_store(root: &Path) -> StorageRef {
    let config = StorageConfig::new(
        3,
        Duration::from_millis(100),
        Duration::from_secs(30),
        64,
        Duration::from_secs(30),
        Duration::from_secs(300),
        0,
        true, // atomic_file_writes
    );
    Arc::new(OpendalStore::filesystem(root, &config).unwrap())
}

const OBJ: &str = "bucket/ab/cd/ef/bucket-test.xdr.gz";

/// Every regular file under `root`, whatever it is named. Used to catch staged
/// data an abort failed to clean up, without depending on how a backend names
/// its temp files.
fn files_under(root: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .collect()
}

async fn write_and_commit(store: &StorageRef) -> u64 {
    let mut w = store.open_staged_writer(OBJ).await.unwrap();
    w.write(b"hello ".to_vec().into()).await.unwrap();
    w.write(b"world".to_vec().into()).await.unwrap();
    w.commit().await.unwrap()
}

#[tokio::test]
async fn commit_makes_data_visible_on_all_writable_flavors() {
    for store_fn in [mock_object_store, plain_fs_store, atomic_fs_store] {
        let dir = TempDir::new().unwrap();
        let store = store_fn(dir.path());
        let bytes = write_and_commit(&store).await;
        assert_eq!(bytes, 11, "commit returns bytes written");
        let got = crate::storage::download_buffer(&store, OBJ)
            .await
            .unwrap()
            .to_vec();
        assert_eq!(got, b"hello world");
        assert!(
            !dir.path().join(format!("{OBJ}.tmp")).exists(),
            "no stranded .tmp sibling after commit"
        );
    }
}

#[tokio::test]
async fn abort_leaves_nothing_at_final_path_on_all_writable_flavors() {
    for store_fn in [mock_object_store, plain_fs_store, atomic_fs_store] {
        let dir = TempDir::new().unwrap();
        let store = store_fn(dir.path());
        let mut w = store.open_staged_writer(OBJ).await.unwrap();
        w.write(b"partial garbage".to_vec().into()).await.unwrap();
        w.abort().await;
        assert!(
            !store.exists(OBJ).await.unwrap(),
            "nothing visible after abort"
        );
        assert!(
            !dir.path().join(format!("{OBJ}.tmp")).exists(),
            "fs-staged abort removes the .tmp sibling"
        );
        assert_eq!(
            files_under(dir.path()),
            Vec::<PathBuf>::new(),
            "abort leaves no staged data behind anywhere under the root"
        );
    }
}

#[tokio::test]
async fn drop_without_commit_leaves_nothing_at_final_path() {
    for store_fn in [mock_object_store, plain_fs_store, atomic_fs_store] {
        let dir = TempDir::new().unwrap();
        let store = store_fn(dir.path());
        {
            let mut w = store.open_staged_writer(OBJ).await.unwrap();
            w.write(b"never committed".to_vec().into()).await.unwrap();
            // dropped here without commit()
        }
        assert!(!store.exists(OBJ).await.unwrap());
    }
}

#[tokio::test]
async fn read_only_backend_rejects_staged_writer() {
    let store =
        crate::storage::from_url_with_config("https://example.org/archive", &test_storage_config())
            .unwrap();
    // StagedWriter is not Debug (it wraps an OpenDAL Writer), so match instead
    // of expect_err().
    let Err(err) = store.open_staged_writer(OBJ).await else {
        panic!("HTTP must reject writes");
    };
    assert!(err.to_string().contains("not supported"), "got: {err}");
}
