//! Behaviour tests: one per capability F3 claims, plus the failure modes that make the claims safe.
//!
//! Each test names the behaviour it turns green. This crate is greenfield, so the honest "failing
//! before" is *the capability did not exist* — there was no replica, no shipping, no restore. Where
//! a test instead documents *why a design choice is load-bearing* (the replay clock, the
//! before-apply base check), it does so by showing the wrong alternative failing, which is a real
//! red/green and is called out in the test's own comment.

use std::path::Path;
use std::sync::Arc;
use substrate_pager::testing::MemVfs;
use substrate_pager::{std_vfs, ManifestId, StoreConfig, Vfs};
use substrate_wal::DurableStore;

use flock_sync::{Applied, Replica, Shipment, SyncError, WalSource};

fn primary(dir: &std::path::Path) -> Arc<DurableStore> {
    let s = Arc::new(DurableStore::open(std_vfs(), dir, StoreConfig::default()).unwrap());
    s.recover().unwrap();
    s
}

/// Commit `writes` (page_no → bytes) and return the new head.
fn commit(store: &DurableStore, writes: &[(u64, &[u8])]) -> ManifestId {
    let mut txn = store.begin().unwrap();
    for (page_no, bytes) in writes {
        store.write(&mut txn, *page_no, bytes.to_vec()).unwrap();
    }
    store.commit(txn).unwrap()
}

fn source(dir: &std::path::Path, primary: &DurableStore) -> WalSource {
    WalSource::open(
        std_vfs(),
        dir,
        primary.pager().cas(),
        substrate_pager::PageStore::page_size(primary.pager().as_ref()),
    )
}

// ── The log spans more than one WAL segment, and the follower still converges ────────────────────
//
// Segments seal at `SEGMENT_TARGET_BYTES` (4 MiB). A source that read only the first segment would
// pass every other test in this file — they all fit in one segment — and silently drop a real
// primary's history. So this one deliberately writes past a segment boundary (many pages per commit,
// so the WAL accumulates records fast) and checks the follower catches up across the rollover.
#[test]
fn a_follower_catches_up_across_a_wal_segment_rollover() {
    // In-memory: this writes several MiB of WAL to cross a 4 MiB segment boundary, which on a real
    // disk is tens of thousands of fsyncs (minutes). `MemVfs` still rolls segments — that logic is
    // byte-counted, not fsync-driven — so the multi-segment path is exercised in a couple of seconds.
    let vfs = MemVfs::new();
    let pdir = Path::new("/primary");
    let primary = Arc::new(
        DurableStore::open(
            Arc::clone(&vfs) as Arc<dyn Vfs>,
            pdir,
            StoreConfig::default(),
        )
        .unwrap(),
    );
    primary.recover().unwrap();

    let wal_dir = pdir.join("wal");
    let seg_count = |vfs: &Arc<MemVfs>| -> usize {
        vfs.read_dir(&wal_dir)
            .unwrap_or_default()
            .iter()
            .filter(|p| p.extension().is_some_and(|x| x == "wal"))
            .count()
    };

    // Commit many-page transactions (so the WAL accumulates records fast) until it spans at least
    // two segments — bounded, so a change in record sizing can never hang the test.
    let mut last = primary.head();
    let mut round = 0u64;
    while seg_count(&vfs) < 2 && round < 4_000 {
        let mut txn = primary.begin().unwrap();
        for p in 0u64..128 {
            primary
                .write(&mut txn, p, format!("r{round}-p{p}").into_bytes())
                .unwrap();
        }
        last = primary.commit(txn).unwrap();
        round += 1;
    }
    assert!(
        seg_count(&vfs) >= 2,
        "the WAL did not roll over after {round} commits"
    );
    let final_round = round - 1;

    let source = WalSource::open(
        Arc::clone(&vfs) as Arc<dyn Vfs>,
        pdir,
        primary.pager().cas(),
        substrate_pager::PageStore::page_size(primary.pager().as_ref()),
    );
    let rvfs = MemVfs::new();
    let mut replica = Replica::open(
        rvfs as Arc<dyn Vfs>,
        Path::new("/replica"),
        StoreConfig::default(),
    )
    .unwrap();
    replica.catch_up(&source).unwrap();
    assert_eq!(
        replica.head(),
        last,
        "a follower must converge across a segment rollover, not stop at the first segment"
    );
    assert_eq!(
        replica.read(&replica.head(), 127).unwrap().as_bytes(),
        format!("r{final_round}-p127").as_bytes()
    );
}

// ── Capability 1: WAL shipping — a follower applies the primary's committed records and converges ─
#[test]
fn a_follower_catches_up_to_the_primary_by_applying_shipped_commits() {
    let pdir = tempfile::tempdir().unwrap();
    let primary = primary(pdir.path());
    commit(&primary, &[(0, b"alpha"), (1, b"one")]);
    commit(&primary, &[(0, b"beta")]);
    let head = commit(&primary, &[(2, b"three")]);

    let rdir = tempfile::tempdir().unwrap();
    let mut replica = Replica::open(std_vfs(), rdir.path(), StoreConfig::default()).unwrap();

    let advanced = replica.catch_up(&source(pdir.path(), &primary)).unwrap();
    assert_eq!(advanced, 3, "three commits should have shipped");
    assert_eq!(
        replica.head(),
        head,
        "the replica must be the primary, exactly"
    );
    assert_eq!(
        replica.read(&replica.head(), 0).unwrap().as_bytes(),
        b"beta"
    );
    assert_eq!(
        replica.read(&replica.head(), 2).unwrap().as_bytes(),
        b"three"
    );
}

// ── Capability 2: read replica — a pinned read is a whole snapshot, never half-applied ───────────
#[test]
fn a_read_pinned_to_a_manifest_does_not_see_a_later_commit() {
    let pdir = tempfile::tempdir().unwrap();
    let primary = primary(pdir.path());
    commit(&primary, &[(0, b"v1")]);
    commit(&primary, &[(0, b"v2")]);
    let src = source(pdir.path(), &primary);
    let shipments = src.shipments_from(0).unwrap();

    let rdir = tempfile::tempdir().unwrap();
    let mut replica = Replica::open(std_vfs(), rdir.path(), StoreConfig::default()).unwrap();

    replica.apply(&shipments[0]).unwrap();
    let pinned = replica.head(); // a reader pins this whole, committed snapshot

    // The stream keeps flowing…
    replica.apply(&shipments[1]).unwrap();

    // …but a read against the pinned manifest still sees v1, in full. The advancing head does not
    // reach back and mutate a snapshot a reader is holding.
    assert_eq!(replica.read(&pinned, 0).unwrap().as_bytes(), b"v1");
    assert_eq!(replica.read(&replica.head(), 0).unwrap().as_bytes(), b"v2");
}

// ── Capability 3: point-in-time restore — replay to a chosen LSN and stop, exactly ───────────────
#[test]
fn restore_to_an_lsn_lands_on_the_primarys_state_at_that_lsn() {
    let pdir = tempfile::tempdir().unwrap();
    let primary = primary(pdir.path());
    let h0 = commit(&primary, &[(0, b"at-t0")]);
    let _h1 = commit(&primary, &[(0, b"at-t1")]);
    let _h2 = commit(&primary, &[(0, b"at-t2")]);
    let src = source(pdir.path(), &primary);
    let target = src.shipments_from(0).unwrap()[0].commit_lsn; // the first commit's LSN

    let rdir = tempfile::tempdir().unwrap();
    let mut replica = Replica::open(std_vfs(), rdir.path(), StoreConfig::default()).unwrap();
    replica.restore_to(&src, target).unwrap();

    assert_eq!(
        replica.head(),
        h0,
        "PITR must land on the t0 manifest exactly"
    );
    assert_eq!(
        replica.read(&replica.head(), 0).unwrap().as_bytes(),
        b"at-t0"
    );
}

// ── A shipment is a wire message: it round-trips through an ordinary serializer ──────────────────
#[test]
fn a_shipment_survives_a_json_round_trip_and_still_applies() {
    let pdir = tempfile::tempdir().unwrap();
    let primary = primary(pdir.path());
    let head = commit(&primary, &[(0, b"over-the-wire"), (3, b"page-three")]);
    let src = source(pdir.path(), &primary);
    let shipment = &src.shipments_from(0).unwrap()[0];

    // Serialize on the "primary", deserialize on the "follower" — a stand-in for a socket.
    let json = serde_json::to_string(shipment).unwrap();
    let received: Shipment = serde_json::from_str(&json).unwrap();

    let rdir = tempfile::tempdir().unwrap();
    let mut replica = Replica::open(std_vfs(), rdir.path(), StoreConfig::default()).unwrap();
    assert_eq!(replica.apply(&received).unwrap(), Applied::Advanced);
    assert_eq!(replica.head(), head);
}

// ── Re-delivering the same shipment is harmless (idempotence) ────────────────────────────────────
#[test]
fn applying_the_same_shipment_twice_is_idempotent() {
    let pdir = tempfile::tempdir().unwrap();
    let primary = primary(pdir.path());
    commit(&primary, &[(0, b"once")]);
    let shipment = &source(pdir.path(), &primary).shipments_from(0).unwrap()[0];

    let rdir = tempfile::tempdir().unwrap();
    let mut replica = Replica::open(std_vfs(), rdir.path(), StoreConfig::default()).unwrap();
    assert_eq!(replica.apply(shipment).unwrap(), Applied::Advanced);
    assert_eq!(
        replica.apply(shipment).unwrap(),
        Applied::AlreadyPresent,
        "a re-sent shipment must be a no-op, not an error or a double-apply"
    );
}

// ── A gap in the stream is refused BEFORE anything is applied ────────────────────────────────────
#[test]
fn applying_a_commit_out_of_order_is_refused() {
    let pdir = tempfile::tempdir().unwrap();
    let primary = primary(pdir.path());
    commit(&primary, &[(0, b"first")]);
    commit(&primary, &[(0, b"second")]);
    let shipments = source(pdir.path(), &primary).shipments_from(0).unwrap();

    let rdir = tempfile::tempdir().unwrap();
    let mut replica = Replica::open(std_vfs(), rdir.path(), StoreConfig::default()).unwrap();

    // Skip the first commit and try to apply the second: its base is not the replica's head.
    let err = replica.apply(&shipments[1]).unwrap_err();
    assert!(
        matches!(err, SyncError::OutOfOrder { .. }),
        "a gap must be refused, not silently applied: {err}"
    );
    // And nothing was applied — the replica is untouched.
    assert_eq!(
        replica.head(),
        source(pdir.path(), &primary).start_head().unwrap()
    );
}

// ── A corrupted page blob is caught by content addressing ────────────────────────────────────────
#[test]
fn a_shipment_with_tampered_page_bytes_is_rejected() {
    let pdir = tempfile::tempdir().unwrap();
    let primary = primary(pdir.path());
    commit(&primary, &[(0, b"honest-bytes")]);
    let mut shipment = source(pdir.path(), &primary).shipments_from(0).unwrap()[0].clone();

    // Flip the bytes but leave the claimed content hash: exactly what a corrupted transfer looks
    // like. The follower must not file these under a name they do not hash to.
    shipment.pages[0].bytes = b"tampered!!!!".to_vec();

    let rdir = tempfile::tempdir().unwrap();
    let mut replica = Replica::open(std_vfs(), rdir.path(), StoreConfig::default()).unwrap();
    let err = replica.apply(&shipment).unwrap_err();
    assert!(
        matches!(err, SyncError::PageHashMismatch { .. }),
        "tampered bytes must be rejected: {err}"
    );
}

// ── Why the replay clock is load-bearing: the timestamp is part of a manifest's identity ─────────
//
// This is the red/green for the `ReplayClock` design. If a follower re-derived a commit with its own
// wall-clock time instead of the primary's, it would produce a DIFFERENT manifest id — a silent
// divergence. Here we mutate only the shipped timestamp and watch the follower refuse to advance,
// which is precisely the failure the replay clock exists to avoid by carrying the primary's time.
#[test]
fn changing_the_shipped_timestamp_makes_replay_diverge() {
    let pdir = tempfile::tempdir().unwrap();
    let primary = primary(pdir.path());
    commit(&primary, &[(0, b"time-matters")]);
    let mut shipment = source(pdir.path(), &primary).shipments_from(0).unwrap()[0].clone();

    // A different timestamp than the one baked into `shipment.manifest`.
    shipment.created_at_ms = shipment.created_at_ms.wrapping_add(1);

    let rdir = tempfile::tempdir().unwrap();
    let mut replica = Replica::open(std_vfs(), rdir.path(), StoreConfig::default()).unwrap();
    let err = replica.apply(&shipment).unwrap_err();
    assert!(
        matches!(err, SyncError::Diverged { .. }),
        "a manifest is identified by its bytes, timestamp included; a different time is a different \
         database and must be caught: {err}"
    );
}

// ── A follower from a different primary cannot follow this one ───────────────────────────────────
#[test]
fn a_foreign_follower_is_not_told_it_is_caught_up() {
    let adir = tempfile::tempdir().unwrap();
    let a = primary(adir.path());
    commit(&a, &[(0, b"pool-a")]);

    // A second, unrelated database with its own history.
    let bdir = tempfile::tempdir().unwrap();
    let b = primary(bdir.path());
    commit(&b, &[(0, b"pool-b")]);

    // Make a replica that has followed B, then point it at A's source.
    let rdir = tempfile::tempdir().unwrap();
    let mut replica = Replica::open(std_vfs(), rdir.path(), StoreConfig::default()).unwrap();
    replica.catch_up(&source(bdir.path(), &b)).unwrap();

    let err = replica.catch_up(&source(adir.path(), &a)).unwrap_err();
    assert!(
        matches!(err, SyncError::FollowerUnknown { .. }),
        "a follower whose head is not in the primary's log has forked and must be told so: {err}"
    );
}
