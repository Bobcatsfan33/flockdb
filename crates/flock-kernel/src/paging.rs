//! The file ↔ pages marshalling. This is the fallback, and this is where it costs.
//!
//! # The layout, which is deliberately as dumb as it can be
//!
//! Logical page `i` holds bytes `[i * page_size, (i + 1) * page_size)` of the DuckDB file. The
//! last page is short if the file is not a whole multiple of the page size. That is the entire
//! format. There is no header, no length field, no index.
//!
//! **Why no header.** A length field would be a second source of truth about how long the file is,
//! and the manifest already knows: concatenating the pages in logical order reconstructs the file
//! byte-for-byte and its length falls out. A header would let those two disagree, and a database
//! that disagrees with itself about its own length is a database that hands DuckDB a truncated
//! file and calls it corruption. (Substrate's rule 9 in miniature: liveness — and here, length —
//! comes from the manifest, never from a counter.)
//!
//! # Why we byte-compare instead of hashing
//!
//! [`persist`] decides which pages changed by reading the old page and comparing bytes, rather
//! than by hashing the new chunk and comparing `PageId`s. Hashing looks obviously faster — one
//! BLAKE3 pass instead of a CAS read — and we do not do it, for a reason worth stating:
//!
//! **the `PageStore` trait does not expose which hasher the store was built with.** Under
//! `--features keyed-hash` (docs/02 §9.1, mandatory for CUI pools) page identity is
//! `BLAKE3_keyed(pool_key, plaintext)`, and a caller who hashes unkeyed would compute a `PageId`
//! that matches nothing, conclude that every page changed, and rewrite the entire database on
//! every checkpoint. It would still be *correct*. It would just be quietly, catastrophically slow,
//! and only in the deployment that can least afford to be surprised.
//!
//! Byte comparison cannot get this wrong, because it never forms an opinion about identity. It
//! costs a read of every page from the CAS — which we were already paying, because [`persist`]
//! reads the whole file anyway.

use crate::error::{KernelError, Result};
use std::fs;
use std::path::Path;
use substrate_pager::{LogicalPageNo, ManifestId, PageStore};

/// Write the database described by `manifest` out to `path` as a plain file.
///
/// This is the "pages → file" direction, and it runs whenever a kernel opens: on `Flock::open`, on
/// every `Db::fork`, and on every `Db::restore`. It copies the whole database. See the crate docs
/// for why, and for what it would take to stop.
pub(crate) fn hydrate(store: &dyn PageStore, manifest: &ManifestId, path: &Path) -> Result<()> {
    let page_size = store.page_size();

    // `resolve` walks the overlay chain and hands back the complete page map. It is O(pages), and
    // that is fine here — we are about to read every one of those pages anyway.
    let pages = store
        .resolve(manifest)
        .map_err(KernelError::store("hydrate"))?;

    let mut file = Vec::with_capacity(pages.len() * page_size);

    // A BTreeMap iterates in key order, which is the whole reason the format needs no index: the
    // pages arrive in the order they belong in the file.
    for &page_no in pages.keys() {
        let expected = page_no as usize * page_size;
        if file.len() != expected {
            // A hole in the logical page numbering. This cannot happen — `persist` writes 0..n
            // contiguously and removes everything beyond — so if it *does* happen, the manifest
            // was not written by us, and concatenating the pages anyway would hand DuckDB a file
            // with a block of bytes silently missing from the middle. Refuse, loudly.
            return Err(KernelError::CorruptLayout {
                manifest: *manifest,
                page_no,
                expected_offset: expected,
                actual_offset: file.len(),
            });
        }
        let page = store
            .read(manifest, page_no)
            .map_err(KernelError::store("hydrate"))?;
        file.extend_from_slice(page.as_bytes());
    }

    if file.is_empty() {
        // A brand-new database must be handed NO FILE AT ALL — not an empty one.
        //
        // This is not a nicety. DuckDB refuses to adopt a zero-byte file that already exists:
        //
        //     IO Error: The file "…/flock.duckdb" exists, but it is not a valid DuckDB database
        //     file!
        //
        // Handed *nothing*, it creates a database. Handed an empty file, it fails at the door —
        // so every `Flock::open` of a name that does not exist yet would have failed, which is to
        // say every first use of FlockDB by every new user. It is worth removing the file we may
        // have hydrated on a previous pass, rather than assuming there is none.
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(KernelError::scratch("clear scratch file", path)(e)),
        }
        return Ok(());
    }

    fs::write(path, &file).map_err(KernelError::scratch("write hydrated database", path))?;
    Ok(())
}

/// Read `path` back into pages and commit them. Returns the new head.
///
/// This is the "file → pages" direction: the sync at a transaction boundary that the whole
/// fallback rests on. It is the expensive half, and the cost is honest — see the table in the
/// crate docs.
///
/// Only *changed* pages enter the transaction. That matters far more than it looks: substrate's
/// manifests are overlays, and an overlay whose change-set is the entire database is not an
/// overlay, it is a copy. Committing every page each time would make each commit O(database) in
/// manifest size *as well as* in I/O, would defeat deduplication across forks, and would turn a
/// 10,000-database fleet's manifest budget from 1.2 GiB into whatever it liked.
pub(crate) fn persist(store: &dyn PageStore, path: &Path) -> Result<ManifestId> {
    let page_size = store.page_size();
    let head = store.head();

    let bytes =
        fs::read(path).map_err(KernelError::scratch("read database for checkpoint", path))?;
    let old = store
        .resolve(&head)
        .map_err(KernelError::store("checkpoint"))?;

    let mut txn = store.begin().map_err(KernelError::store("checkpoint"))?;

    let chunks = bytes.chunks(page_size);
    let new_page_count = chunks.len() as LogicalPageNo;

    for (i, chunk) in chunks.enumerate() {
        let page_no = i as LogicalPageNo;

        // Unchanged? Then it must not enter the transaction — see the doc comment above.
        if old.contains_key(&page_no) {
            let existing = store
                .read(&head, page_no)
                .map_err(KernelError::store("checkpoint"))?;
            if existing.as_bytes() == chunk {
                continue;
            }
        }

        store
            .write(&mut txn, page_no, chunk.to_vec())
            .map_err(KernelError::store("checkpoint"))?;
    }

    // The file shrank — a `DROP TABLE`, or a DuckDB checkpoint compacting free blocks. Every
    // logical page past the new end must be *removed*, not merely ignored. Left in place, they
    // would be reconstructed by `hydrate` as trailing bytes DuckDB never wrote, and the file would
    // grow back with garbage welded to the end of it.
    for &page_no in old.keys().filter(|&&n| n >= new_page_count) {
        store
            .remove(&mut txn, page_no)
            .map_err(KernelError::store("checkpoint"))?;
    }

    // Substrate's commit: page bytes are already in the CAS and fsync'd; this derives the manifest
    // and installs it. An empty transaction returns `head` unchanged, which is what makes
    // `checkpoint()` on an unmodified database free rather than a source of duplicate manifests.
    //
    // NOTE on the commit point: at the pager alone, installing the manifest *is* the commit. The
    // durable "which manifest is the head of this database" record is written by `flock-core`,
    // into substrate's WAL, immediately after this returns — and until that record is fsync'd,
    // everything here is orphan pages and an unreferenced manifest, which is to say harmless
    // garbage that GC sweeps. See `flock_core::Db::snapshot`, and docs/02 §3.1.
    store.commit(txn).map_err(KernelError::store("checkpoint"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use substrate_pager::{Pager, StoreConfig, MIN_PAGE_SIZE};

    /// The smallest page substrate will accept — 4 KiB, and it must be a power of two. Using the
    /// *minimum* rather than the 64 KiB default means the multi-page and short-last-page cases are
    /// reachable from a unit test without writing megabytes, while still exercising a page size the
    /// store will actually accept. The format itself does not care what the page size is.
    const PS: usize = MIN_PAGE_SIZE;

    fn store(page_size: usize) -> Pager {
        Pager::in_memory(StoreConfig {
            page_size,
            ..StoreConfig::default()
        })
        .unwrap()
    }

    fn roundtrip(page_size: usize, content: &[u8]) -> Vec<u8> {
        let store = store(page_size);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");

        fs::write(&path, content).unwrap();
        let head = persist(&store, &path).unwrap();

        let back = dir.path().join("back");
        hydrate(&store, &head, &back).unwrap();
        // `unwrap_or_default` and not `unwrap`, because hydrating an *empty* database deliberately
        // leaves no file — see `an_empty_database_is_hydrated_as_no_file_at_all`.
        fs::read(&back).unwrap_or_default()
    }

    #[test]
    fn a_file_survives_a_round_trip_through_pages_byte_for_byte() {
        // 10,000 bytes over 4 KiB pages: two full pages and a short one — the layout that a
        // naive concatenation gets wrong at exactly one boundary.
        let content: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(roundtrip(PS, &content), content);
    }

    #[test]
    fn a_file_shorter_than_one_page_survives_a_round_trip() {
        assert_eq!(roundtrip(PS, b"tiny"), b"tiny");
    }

    #[test]
    fn a_file_that_is_an_exact_multiple_of_the_page_size_survives_a_round_trip() {
        // The case a length header would exist to disambiguate, and which needs no disambiguating:
        // 4 full pages concatenate to exactly 4 pages of bytes, and the length falls out.
        let content = vec![7u8; 4 * PS];
        assert_eq!(roundtrip(PS, &content), content);
    }

    #[test]
    fn an_empty_file_survives_a_round_trip() {
        assert_eq!(roundtrip(PS, b""), b"");
    }

    #[test]
    fn an_empty_database_is_hydrated_as_no_file_at_all() {
        // DuckDB refuses a zero-byte file that already exists — "exists, but it is not a valid
        // DuckDB database file" — and creates a database only when handed nothing. So an empty
        // manifest must produce an ABSENT file, not an empty one.
        //
        // Every first use of FlockDB by every new user goes through this path. It is one line of
        // code and it is the difference between the product working and the product not starting.
        let store = store(PS);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");

        // Even if a file is already sitting there, hydration of an empty database must clear it.
        fs::write(&path, b"leftovers from a previous life").unwrap();

        hydrate(&store, &store.head(), &path).unwrap();
        assert!(
            !path.exists(),
            "an empty database must leave NO scratch file — DuckDB will not adopt an empty one"
        );
    }

    #[test]
    fn an_unchanged_file_commits_to_the_same_manifest_it_started_from() {
        // The property that keeps `checkpoint()` from littering the DAG: if nothing moved, no new
        // manifest exists. It is also what makes the fork-isolation test meaningful — a fork that
        // silently re-committed its parent's state would look identical to one that shared it.
        let store = store(PS);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        fs::write(&path, vec![3u8; 200]).unwrap();

        let first = persist(&store, &path).unwrap();
        let second = persist(&store, &path).unwrap();
        assert_eq!(
            first, second,
            "an unchanged file must not produce a new manifest"
        );
    }

    #[test]
    fn a_file_that_shrinks_does_not_leave_its_old_tail_behind() {
        // The bug this test exists to prevent: `DROP TABLE` shrinks the file, we write the pages
        // that are still there and forget to remove the ones that are not, and `hydrate` welds the
        // dead tail back on. The database grows back from the dead, holding data the user deleted.
        let store = store(PS);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");

        // Three pages, then one. Pages 1 and 2 must be REMOVED, not merely left unreferenced.
        fs::write(&path, vec![1u8; 3 * PS]).unwrap();
        persist(&store, &path).unwrap();

        fs::write(&path, vec![2u8; PS]).unwrap();
        let head = persist(&store, &path).unwrap();

        let back = dir.path().join("back");
        hydrate(&store, &head, &back).unwrap();
        assert_eq!(fs::read(&back).unwrap(), vec![2u8; PS]);
    }

    #[test]
    fn only_the_pages_that_changed_are_written() {
        // If this regresses, every commit becomes O(database) in manifest size and forks stop
        // sharing pages. Nothing would *break*; the product's central economic claim would just
        // quietly stop being true. So we assert it.
        let store = store(PS);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");

        let mut content = vec![0u8; 10 * PS];
        fs::write(&path, &content).unwrap();
        let base = persist(&store, &path).unwrap();

        content[PS + 10] = 9; // one byte, in page 1, and nowhere else
        fs::write(&path, &content).unwrap();
        let next = persist(&store, &path).unwrap();

        let diff = store.diff(&base, &next).unwrap();
        assert_eq!(
            diff.len(),
            1,
            "one changed byte must touch exactly one page, not ten"
        );
    }
}
