//! The model oracle for replication (flockdb `CLAUDE.md` rule 8, substrate rule 4).
//!
//! The property under test is the one the whole crate exists to provide, and it is subtle in exactly
//! the way that gets shipped broken:
//!
//! > A follower that has applied a prefix of the primary's WAL **equals** the primary at that same
//! > prefix — the same manifest id (so, byte-for-byte the same pages) — and a point-in-time restore
//! > to LSN *N* equals the primary as of *N*.
//!
//! "Almost equal" is a wrong answer served with confidence, so we do not test one hand-built
//! scenario; we generate thousands. Each run builds a primary from a random sequence of commits,
//! ships its WAL, and checks a follower against it at **every** applied prefix and at **randomized**
//! catch-up and restore points — including a crash-and-resume of the follower itself.
//!
//! There is no separate "model" struct here because substrate already ships one: the primary *is*
//! the oracle. The follower is the implementation under test, and the assertion is that the two
//! agree, read for read, at every point they should.

use std::path::Path;
use std::sync::Arc;
use substrate_pager::testing::MemVfs;
use substrate_pager::{ManifestId, PageStore, StoreConfig, Vfs};
use substrate_wal::DurableStore;

use flock_sync::{Replica, WalSource};

// Every store lives at the same synthetic path inside its OWN in-memory disk, so paths never
// collide across stores and no real filesystem is touched.
const DB: &str = "/db";

// ── A tiny deterministic RNG, so a failing run reproduces from its seed ──────────────────────────
// splitmix64: boring, well-known, and it means "the oracle failed on seed 91237" is an instruction,
// not a shrug. No external crate for six lines of arithmetic.
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

const MAX_PAGE: u64 = 6; // logical pages 0..MAX_PAGE
const MAX_COMMITS: u64 = 8;

/// The primary is a plain substrate durable store — the same thing flock-core opens for a local
/// database. It is also the oracle: the follower must reproduce exactly what it committed.
type Primary = DurableStore;

/// Build a random transaction on the primary, commit it, and report the new head **iff it changed**.
///
/// A transaction that rewrites identical bytes or removes an absent page is a no-op: substrate's
/// `commit` writes no WAL record and returns the unchanged head, and therefore produces no shipment.
/// Reporting only real commits keeps the expected-heads list aligned one-to-one with the shipments.
fn random_commit(store: &Primary, rng: &mut Rng, tick: &mut u64) -> Option<ManifestId> {
    let before = store.head();
    let mut txn = store.begin().unwrap();
    let ops = 1 + rng.below(3);
    for _ in 0..ops {
        let page_no = rng.below(MAX_PAGE);
        if rng.below(5) == 0 {
            store.remove(&mut txn, page_no).unwrap();
        } else {
            // Fresh bytes each time (a monotonic tick) so a write reliably changes something; the
            // length varies too, exercising short pages.
            *tick += 1;
            let len = 1 + (rng.below(15) as usize);
            let mut bytes = vec![(*tick & 0xFF) as u8; len];
            if !bytes.is_empty() {
                bytes[0] = (*tick >> 8) as u8;
            }
            store.write(&mut txn, page_no, bytes).unwrap();
        }
    }
    let after = store.commit(txn).unwrap();
    (after != before).then_some(after)
}

/// Every page 0..MAX_PAGE at `manifest`, as `Option<bytes>` (None = the page is absent there).
fn snapshot_reads(store: &dyn ReadAt, manifest: &ManifestId) -> Vec<Option<Vec<u8>>> {
    (0..MAX_PAGE)
        .map(|p| store.read_page(manifest, p))
        .collect()
}

/// The one thing a primary and a replica must both be able to do for the oracle: read a page as of a
/// manifest. Both back onto substrate; this trait just lets the assertion take either.
trait ReadAt {
    fn read_page(&self, manifest: &ManifestId, page_no: u64) -> Option<Vec<u8>>;
}
impl ReadAt for Primary {
    fn read_page(&self, manifest: &ManifestId, page_no: u64) -> Option<Vec<u8>> {
        self.read(manifest, page_no)
            .ok()
            .map(|p| p.as_bytes().to_vec())
    }
}
impl ReadAt for Replica {
    fn read_page(&self, manifest: &ManifestId, page_no: u64) -> Option<Vec<u8>> {
        self.read(manifest, page_no)
            .ok()
            .map(|p| p.as_bytes().to_vec())
    }
}

fn open_primary(vfs: Arc<dyn Vfs>) -> Arc<Primary> {
    let store = Arc::new(DurableStore::open(vfs, Path::new(DB), StoreConfig::default()).unwrap());
    store.recover().unwrap();
    store
}

fn source_for(vfs: Arc<dyn Vfs>, primary: &Primary) -> WalSource {
    WalSource::open(
        vfs,
        Path::new(DB),
        primary.pager().cas(),
        primary.pager().page_size(),
    )
}

/// One randomized run. Returns nothing; panics (fails the test) the instant primary and replica
/// disagree, naming the seed so the run reproduces.
fn run_once(seed: u64) {
    let mut rng = Rng(seed);
    let mut tick = seed;

    // ── Build a primary from a random commit sequence, remembering its head after each commit ────
    let primary_vfs = MemVfs::new();
    let primary = open_primary(Arc::clone(&primary_vfs) as Arc<dyn Vfs>);

    let n_commits = 1 + rng.below(MAX_COMMITS);
    let mut heads: Vec<ManifestId> = Vec::new();
    while (heads.len() as u64) < n_commits {
        if let Some(head) = random_commit(&primary, &mut rng, &mut tick) {
            heads.push(head);
        }
    }
    let final_head = *heads.last().unwrap();

    // ── Ship, and sanity-check the shipping itself against the primary's own heads ───────────────
    let source = source_for(Arc::clone(&primary_vfs) as Arc<dyn Vfs>, &primary);
    let shipments = source.shipments_from(0).unwrap();
    assert_eq!(
        shipments.len(),
        heads.len(),
        "seed {seed}: one shipment per committed transaction"
    );
    for (i, s) in shipments.iter().enumerate() {
        assert_eq!(s.manifest, heads[i], "seed {seed}: shipment {i} manifest");
        let expected_base = if i == 0 {
            source.start_head().unwrap()
        } else {
            heads[i - 1]
        };
        assert_eq!(
            s.base, expected_base,
            "seed {seed}: shipment {i} base chains"
        );
    }

    // ── Oracle 1: a follower applying the stream equals the primary at EVERY prefix ──────────────
    let replica_vfs = MemVfs::new();
    let mut replica = Replica::open(
        Arc::clone(&replica_vfs) as Arc<dyn Vfs>,
        Path::new(DB),
        StoreConfig::default(),
    )
    .unwrap();
    for (i, s) in shipments.iter().enumerate() {
        replica.apply(s).unwrap();
        assert_eq!(
            replica.head(),
            heads[i],
            "seed {seed}: replica head diverged at prefix {i}"
        );
        // Byte-for-byte, not merely id-equal: read every page on both sides and compare.
        assert_eq!(
            snapshot_reads(&replica, &replica.head()),
            snapshot_reads(primary.as_ref(), &heads[i]),
            "seed {seed}: replica pages differ from primary at prefix {i}"
        );
    }
    assert_eq!(replica.head(), final_head, "seed {seed}: final head");

    // ── Oracle 2: point-in-time restore to a RANDOM LSN lands exactly on the primary at that LSN ─
    {
        let target_idx = rng.below(shipments.len() as u64) as usize;
        let target_lsn = shipments[target_idx].commit_lsn;
        let pit_vfs = MemVfs::new();
        let mut pit = Replica::open(
            pit_vfs as Arc<dyn Vfs>,
            Path::new(DB),
            StoreConfig::default(),
        )
        .unwrap();
        pit.restore_to(&source, target_lsn).unwrap();
        let expected = source.manifest_at(target_lsn).unwrap().unwrap();
        assert_eq!(
            expected, heads[target_idx],
            "seed {seed}: manifest_at should be the primary's head at that commit"
        );
        assert_eq!(
            pit.head(),
            heads[target_idx],
            "seed {seed}: PITR to lsn {target_lsn} landed on the wrong state"
        );
        assert_eq!(
            snapshot_reads(&pit, &pit.head()),
            snapshot_reads(primary.as_ref(), &heads[target_idx]),
            "seed {seed}: PITR pages differ from primary"
        );
        // Reproducible: a second fresh restore to the same lsn lands on the identical head.
        let pit2_vfs = MemVfs::new();
        let mut pit2 = Replica::open(
            pit2_vfs as Arc<dyn Vfs>,
            Path::new(DB),
            StoreConfig::default(),
        )
        .unwrap();
        pit2.restore_to(&source, target_lsn).unwrap();
        assert_eq!(
            pit2.head(),
            pit.head(),
            "seed {seed}: PITR is not reproducible"
        );
    }

    // ── Oracle 3: a follower that crashes mid-catch-up resumes from its durable head and converges ─
    {
        // The in-memory disk survives the "crash"; only the process state (the Replica) is dropped.
        let resume_vfs = MemVfs::new();
        let prefix = rng.below(shipments.len() as u64 + 1) as usize; // 0..=all
        {
            let mut r = Replica::open(
                Arc::clone(&resume_vfs) as Arc<dyn Vfs>,
                Path::new(DB),
                StoreConfig::default(),
            )
            .unwrap();
            for s in shipments.iter().take(prefix) {
                r.apply(s).unwrap();
            }
            // r (and its DurableStore) drops here — the "crash": nothing closed cleanly.
        }
        // Reopen on the same disk. Recovery restores the durable head; catch_up resumes there.
        let mut resumed = Replica::open(
            Arc::clone(&resume_vfs) as Arc<dyn Vfs>,
            Path::new(DB),
            StoreConfig::default(),
        )
        .unwrap();
        let expected_after_recover = if prefix == 0 {
            source.start_head().unwrap()
        } else {
            heads[prefix - 1]
        };
        assert_eq!(
            resumed.head(),
            expected_after_recover,
            "seed {seed}: follower did not recover to its durable head at prefix {prefix}"
        );
        let advanced = resumed.catch_up(&source).unwrap();
        assert_eq!(
            advanced as usize,
            shipments.len() - prefix,
            "seed {seed}: catch_up advanced the wrong number of commits"
        );
        assert_eq!(
            resumed.head(),
            final_head,
            "seed {seed}: resumed follower did not converge to the primary"
        );
        assert_eq!(
            snapshot_reads(&resumed, &resumed.head()),
            snapshot_reads(primary.as_ref(), &final_head),
            "seed {seed}: resumed follower pages differ from primary"
        );
    }
}

#[test]
fn replica_equals_primary_across_randomized_runs() {
    // Two thousand seeds run as part of the ordinary gate; each seed exercises every-prefix
    // equality, random PITR, and crash-resume — so this is ~10k+ distinct primary/follower checks.
    // In-memory (`MemVfs`) keeps it to seconds. The even heavier sweep is `#[ignore]`d below.
    for seed in 0..2_000u64 {
        run_once(seed.wrapping_mul(0x100_0001).wrapping_add(1));
    }
}

#[test]
#[ignore = "the heavy differential sweep — run explicitly with `--ignored`"]
fn replica_equals_primary_ten_thousand_runs() {
    for seed in 0..10_000u64 {
        run_once(seed.wrapping_mul(0x9E37_79B1).wrapping_add(7));
    }
}
