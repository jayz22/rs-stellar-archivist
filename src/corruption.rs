//! Archive corruption tool logic (feature-gated; perf testing only).
use rand::seq::SliceRandom;
use rand::Rng;
use serde::Serialize;
use std::path::{Path, PathBuf};
use stellar_xdr::curr::Hash;
use walkdir::WalkDir;

//=============================================================================
// Ledger-header file corruption helpers
//
// Read/recompute/write a gzip'd ledger-header file at the XDR-entry level.
// Ported verbatim from src/tests/utils.rs (visibility widened to `pub`).
//=============================================================================

pub fn read_and_parse_ledger_file(path: &Path) -> Vec<stellar_xdr::curr::LedgerHeaderHistoryEntry> {
    use flate2::read::GzDecoder;
    use std::io::Read as _;
    use stellar_xdr::curr::{Frame, LedgerHeaderHistoryEntry, Limited, Limits, ReadXdr};

    let data = std::fs::read(path).expect("Failed to read ledger file");
    let mut decoder = GzDecoder::new(&data[..]);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .expect("Failed to decompress");

    let cursor = std::io::Cursor::new(&decompressed);
    let mut limited = Limited::new(cursor, Limits::none());

    Frame::<LedgerHeaderHistoryEntry>::read_xdr_iter(&mut limited)
        .map(|r| r.expect("Failed to parse ledger-header entry").0)
        .collect()
}

pub fn recompute_entry_hash(entry: &mut stellar_xdr::curr::LedgerHeaderHistoryEntry) {
    use sha2::{Digest, Sha256};
    use stellar_xdr::curr::{Limits, WriteXdr};

    let header_xdr = entry
        .header
        .to_xdr(Limits::none())
        .expect("Failed to serialize header");
    entry.hash = Hash(Sha256::digest(&header_xdr).into());
}

pub fn write_ledger_header_entries_to_file(
    path: &Path,
    entries: &[stellar_xdr::curr::LedgerHeaderHistoryEntry],
) {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write as _;
    use stellar_xdr::curr::{Limits, WriteXdr};

    let mut data = Vec::new();
    for entry in entries {
        let entry_xdr = entry
            .to_xdr(Limits::none())
            .expect("Failed to serialize entry");
        let frame_len = entry_xdr.len() as u32 | 0x8000_0000;
        data.extend_from_slice(&frame_len.to_be_bytes());
        data.extend_from_slice(&entry_xdr);
    }

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&data)
        .expect("Failed to write compressed data");
    let compressed = encoder.finish().expect("Failed to finish compression");
    std::fs::write(path, compressed).expect("Failed to write file");
}

/// Corrupt a single ledger-header file in place so that the named cross-file
/// hash field (`"tx_set"` or `"result"`) is wrong on every entry, while each
/// entry's own hash and the intra-checkpoint prev-hash chain are recomputed to
/// stay valid. The result parses cleanly per-entry; the mismatch surfaces only
/// during cross-file verification against the transactions/results files.
pub fn corrupt_ledger_cross_file_hash(ledger_file: &Path, field: &str) {
    let mut entries = read_and_parse_ledger_file(ledger_file);
    for i in 0..entries.len() {
        match field {
            "tx_set" => entries[i].header.scp_value.tx_set_hash = Hash([0xDE; 32]),
            "result" => entries[i].header.tx_set_result_hash = Hash([0xDE; 32]),
            other => panic!("unknown cross-file hash field: {other}"),
        }
        if i > 0 {
            entries[i].header.previous_ledger_hash = entries[i - 1].hash.clone();
        }
        recompute_entry_hash(&mut entries[i]);
    }
    write_ledger_header_entries_to_file(ledger_file, &entries);
}

//=============================================================================
// Corruption menu
//=============================================================================

#[derive(Serialize, Clone)]
pub struct Damage {
    pub kind: String,
    pub key: String,
} // key = archive-relative, '/'-separated
#[derive(Serialize, Default)]
pub struct Manifest {
    pub items: Vec<Damage>,
}

pub const ALL_KINDS: &[&str] = &[
    "delete-file",
    "truncate",
    "byte-flip",
    "invalid-gzip",
    "bucket-hash",
    "ledger-header-hash",
    "drop-ledger",
    "txset-hash",
    "result-hash",
    "intra-chain",
    "cross-chain",
    "well-known",
];

fn rel_key(abs: &Path, base: &Path) -> String {
    abs.strip_prefix(base)
        .unwrap()
        .to_string_lossy()
        .replace('\\', "/")
}
fn files_matching(base: &Path, pat: &str) -> Vec<PathBuf> {
    WalkDir::new(base)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.to_string_lossy().replace('\\', "/").contains(pat))
        .collect()
}

/// Apply one corruption of `kind` to `archive`; returns what was broken.
pub fn apply(archive: &Path, kind: &str, rng: &mut impl Rng) -> Option<Damage> {
    let mk = |abs: &Path, k: &str| Damage {
        kind: k.to_string(),
        key: rel_key(abs, archive),
    };
    let ledger_files = || -> Vec<PathBuf> { files_matching(archive, "/ledger-") };
    match kind {
        "delete-file" => {
            let files: Vec<_> = files_matching(archive, "/")
                .into_iter()
                .filter(|p| !p.to_string_lossy().contains(".well-known"))
                .collect();
            let p = files.choose(rng)?.clone();
            let d = mk(&p, kind);
            std::fs::remove_file(&p).ok()?;
            Some(d)
        }
        "truncate" => {
            let f = files_matching(archive, ".xdr.gz");
            let p = f.choose(rng)?.clone();
            std::fs::write(&p, b"").ok()?;
            Some(mk(&p, kind))
        }
        "byte-flip" => {
            let f = files_matching(archive, ".xdr.gz");
            let p = f.choose(rng)?.clone();
            let mut b = std::fs::read(&p).ok()?;
            for x in &mut b {
                *x ^= 0xff;
            }
            std::fs::write(&p, b).ok()?;
            Some(mk(&p, kind))
        }
        "invalid-gzip" => {
            let f = files_matching(archive, "/bucket-");
            let p = f.choose(rng)?.clone();
            std::fs::write(&p, b"not gzip at all").ok()?;
            Some(mk(&p, kind))
        }
        "bucket-hash" => {
            let f = files_matching(archive, "/bucket-");
            let p = f.choose(rng)?.clone();
            use flate2::{write::GzEncoder, Compression};
            use std::io::Write as _;
            let mut e = GzEncoder::new(Vec::new(), Compression::default());
            e.write_all(b"valid gzip wrong content").ok()?;
            std::fs::write(&p, e.finish().ok()?).ok()?;
            Some(mk(&p, kind))
        }
        "ledger-header-hash" => {
            let p = ledger_files().choose(rng)?.clone();
            let mut es = read_and_parse_ledger_file(&p);
            if let Some(e) = es.get_mut(0) {
                e.hash = Hash([0xDE; 32]);
            } // hash no longer matches header
            write_ledger_header_entries_to_file(&p, &es);
            Some(mk(&p, kind))
        }
        "drop-ledger" => {
            // pick a multi-entry ledger file so completeness genuinely breaks
            let candidates: Vec<_> = ledger_files()
                .into_iter()
                .filter(|p| read_and_parse_ledger_file(p).len() > 1)
                .collect();
            let p = candidates.choose(rng)?.clone();
            let mut es = read_and_parse_ledger_file(&p);
            es.remove(es.len() / 2);
            write_ledger_header_entries_to_file(&p, &es);
            Some(mk(&p, kind))
        }
        "txset-hash" => {
            let p = ledger_files().choose(rng)?.clone();
            corrupt_ledger_cross_file_hash(&p, "tx_set");
            Some(mk(&p, kind))
        }
        "result-hash" => {
            let p = ledger_files().choose(rng)?.clone();
            corrupt_ledger_cross_file_hash(&p, "result");
            Some(mk(&p, kind))
        }
        "intra-chain" => {
            let candidates: Vec<_> = ledger_files()
                .into_iter()
                .filter(|p| read_and_parse_ledger_file(p).len() > 1)
                .collect();
            let p = candidates.choose(rng)?.clone();
            let mut es = read_and_parse_ledger_file(&p);
            es[1].header.previous_ledger_hash = Hash([0xAB; 32]);
            recompute_entry_hash(&mut es[1]);
            write_ledger_header_entries_to_file(&p, &es);
            Some(mk(&p, kind))
        }
        "cross-chain" => {
            // must NOT be the genesis checkpoint (it has no predecessor boundary).
            // Pick the ledger file with the HIGHEST checkpoint to be safe.
            let mut f = ledger_files();
            f.sort_by_key(|p| p.to_string_lossy().to_string());
            let p = f.last()?.clone();
            let mut es = read_and_parse_ledger_file(&p);
            if let Some(e) = es.get_mut(0) {
                e.header.previous_ledger_hash = Hash([0xCD; 32]);
                recompute_entry_hash(e);
            }
            write_ledger_header_entries_to_file(&p, &es);
            Some(mk(&p, kind))
        }
        "well-known" => {
            let p = archive.join(".well-known/stellar-history.json");
            std::fs::write(&p, b"{ invalid json }}}").ok()?;
            Some(mk(&p, kind))
        }
        _ => None,
    }
}

pub fn run(archive: &Path, kinds: &[String], count: usize, seed: u64) -> Manifest {
    use rand::SeedableRng;
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut m = Manifest::default();
    for _ in 0..count {
        let k = kinds.choose(&mut rng).cloned().unwrap_or_default();
        if let Some(d) = apply(archive, &k, &mut rng) {
            m.items.push(d);
        }
    }
    m
}
