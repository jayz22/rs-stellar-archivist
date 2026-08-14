//! Tests for writable object-store (S3/GCS/Azure/B2/Swift) destinations.
//!
//! Most tests use `mock_object_store` (see `tests::utils`) — a local-fs-backed
//! store that presents exactly what a cloud object store presents to the rest
//! of the code: writable, atomic commit-on-close, and NO filesystem base path.
//! The backing directory doubles as an inspection window: after operations
//! complete, the "bucket" contents can be checked as plain files.

use super::utils::{file_url_from_path, mock_object_store, testnet_small_archive_path};
use crate::storage::{OpendalStore, Storage, StorageRef};
use crate::test_helpers::{run_mirror, test_storage_config, MirrorConfig};
use crate::verify::verify_and_write_bucket;
use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
async fn mock_object_store_has_object_store_semantics() {
    let dir = TempDir::new().unwrap();
    let store = mock_object_store(dir.path());
    assert!(store.supports_writes());
    assert!(store.uses_atomic_writes());
    assert!(store.get_base_path().is_none());
}

/// Gzip `content` into a valid bucket file: the returned archive path embeds
/// the sha256 of the *decompressed* content, as real bucket files do.
fn make_bucket(content: &[u8]) -> (String, Vec<u8>) {
    let hash = hex::encode(Sha256::digest(content));
    let path = crate::history_format::bucket_path(&hash).unwrap();
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(content).unwrap();
    (path, enc.finish().unwrap())
}

/// Put `bytes` at `path` inside `dir` so a filesystem source store can serve it.
fn plant_file(dir: &Path, path: &str, bytes: &[u8]) {
    let full = dir.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(&full, bytes).unwrap();
}

fn fs_store(root: &Path) -> StorageRef {
    Arc::new(OpendalStore::filesystem(root, &test_storage_config()).expect("create fs store"))
}

#[tokio::test]
async fn verified_write_commits_good_bucket_at_final_path_on_object_store() {
    let src_dir = TempDir::new().unwrap();
    let dst_dir = TempDir::new().unwrap();
    let (path, gz) = make_bucket(b"verified bucket content");
    plant_file(src_dir.path(), &path, &gz);

    let src = fs_store(src_dir.path());
    let dst = mock_object_store(dst_dir.path());
    let reader = src.open_reader(&path).await.unwrap();

    verify_and_write_bucket(&path, reader, &dst)
        .await
        .expect("verified write should succeed");

    assert!(dst.exists(&path).await.unwrap(), "object at final path");
    assert!(
        !dst_dir.path().join(format!("{path}.tmp")).exists(),
        "no stranded sibling .tmp object"
    );
}

#[tokio::test]
async fn verified_write_of_corrupt_bucket_leaves_nothing_at_final_path() {
    let src_dir = TempDir::new().unwrap();
    let dst_dir = TempDir::new().unwrap();

    // Path claims the hash of one content, bytes decompress to another:
    // valid gzip, wrong hash — must fail verification.
    let (path, _) = make_bucket(b"content the path claims");
    let (_, wrong_gz) = make_bucket(b"content actually delivered");
    plant_file(src_dir.path(), &path, &wrong_gz);

    let src = fs_store(src_dir.path());
    let dst = mock_object_store(dst_dir.path());
    let reader = src.open_reader(&path).await.unwrap();

    let err = verify_and_write_bucket(&path, reader, &dst)
        .await
        .expect_err("hash mismatch must fail");
    assert!(err.to_string().contains("Hash mismatch"), "got: {err}");

    assert!(
        !dst.exists(&path).await.unwrap(),
        "corrupt data must not be visible at the final path"
    );
    assert!(
        !dst_dir.path().join(format!("{path}.tmp")).exists(),
        "no stranded sibling .tmp object"
    );
}

#[cfg(feature = "opendal-s3")]
#[tokio::test]
async fn s3_store_is_writable_and_atomic() {
    let store = OpendalStore::s3(
        "test-bucket",
        Some("us-east-1"),
        Some("http://127.0.0.1:9"), // never contacted; construction only
        Some("test-access-key"),
        Some("test-secret-key"),
        "some/prefix",
        &test_storage_config(),
    )
    .expect("construct S3 store");
    assert!(store.supports_writes());
    assert!(store.uses_atomic_writes());
    assert!(store.get_base_path().is_none());
}

#[tokio::test]
async fn http_destination_is_still_rejected() {
    let src_url = file_url_from_path(&testnet_small_archive_path());
    let err = run_mirror(MirrorConfig::new(&src_url, "https://example.org/archive"))
        .await
        .expect_err("HTTP destinations must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("does not support writes"), "got: {msg}");
}
