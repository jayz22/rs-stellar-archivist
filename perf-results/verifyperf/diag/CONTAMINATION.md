# Measurement contamination (discovered 2026-06-16 ~18:40Z)

bottleneck.sh during a c=16 sync verify showed system-wide network RX ~50-112
MB/s and disk-write ~60-125 MB/s on a read-only file:// scan that should do
neither, plus ~110-170 threads on futex_wait_queue.

Root cause (via `ps`): a Stage 2.2 full-pubnet mirror was running on the same
box the whole session:
  PID 1368832  sa-perf mirror https://history.stellar.org/prd/core-live/core_live_001 \
               file:///data/pubnet-mirror -c 32 --skip-optional
  elapsed 13h24m, ~1.6 TB written to /data, started ~05:17Z (another session).

Impact: all Phase 0/1/2 verify numbers are contaminated; re-measure on a quiet
box. See ../../../docs/HANDOFF.md. Mirror left running per user decision.
