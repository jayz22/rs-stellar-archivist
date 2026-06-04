#[tokio::test]
async fn each_corruption_is_detected() {
    use rand::SeedableRng;
    for kind in crate::corruption::ALL_KINDS {
        let dir = tempfile::TempDir::new().unwrap();
        crate::tests::utils::copy_testnet_small_archive(dir.path()).unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        crate::corruption::apply(dir.path(), kind, &mut rng)
            .unwrap_or_else(|| panic!("apply {kind}"));
        let cfg = crate::test_helpers::ScanConfig::new(crate::tests::utils::file_url_from_path(
            dir.path(),
        ))
        .verify();
        let res = crate::test_helpers::run_scan(cfg).await;
        assert!(
            res.is_err(),
            "scan --verify must DETECT corruption kind `{kind}`"
        );
    }
}
