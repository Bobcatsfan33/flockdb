//! The primary side of shipping: turn a primary's fsync'd WAL into [`Shipment`]s.
//!
//! # What this reads, and the one coupling it accepts
//!
//! A [`WalSource`] reads the WAL substrate already wrote — the segments under the database's `wal/`
//! directory — and folds them back into per-transaction shipments, exactly the way substrate's own
//! recovery folds `Write` records up to a `Commit`. It fetches the referenced page bytes from the
//! primary's CAS. It re-derives nothing: the manifest in each shipment is the one substrate fsync'd.
//!
//! Substrate does not expose a public "iterate the log" API (its `Wal` only *applies* the log, via
//! `recover`). So this module knows one thing substrate's frozen API does not promise: that the log
//! lives in `wal/NN…N.wal` segment files framed as `len | crc32c | bincode(Record)`, with an
//! optional `wal/CHECKPOINT` marker. That layout is **not** part of substrate's compatibility
//! promise — but the dependency is pinned to a substrate *tag* (`substrate-v1.2.1`), so it cannot
//! change underneath us without a deliberate version bump, which is the whole reason FlockDB pins to
//! a tag and never a branch (flockdb `CLAUDE.md` rule 1). The frame decoding itself uses substrate's
//! public, frozen [`Record::decode`]. This is the single point of coupling, and it is confined to
//! this file so that the day substrate ships a public log reader, only this file changes.

use crate::error::{Result, SyncError};
use crate::shipment::{PageBlob, PageWrite, Shipment};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use substrate_pager::{Cas, LogicalPageNo, Manifest, ManifestId, PageId, Vfs};
use substrate_wal::{Lsn, ReadOutcome, Record, RecordKind};

/// A read-only view of a primary database's committed history, as shippable transactions.
///
/// Construct one over a primary that is opened by [`Flock::open`](../flock_core) (a *local* database
/// with a real WAL). A woken, object-storage-backed database has no WAL to ship — see the note on
/// [`Db::wal_source`](../flock_core) — and there is nothing here for it.
pub struct WalSource {
    vfs: Arc<dyn Vfs>,
    wal_dir: PathBuf,
    cas: Arc<dyn Cas>,
    page_size: usize,
}

impl WalSource {
    /// Open a source over the WAL and CAS of a primary database directory.
    ///
    /// `store_dir` is the database's own directory — the one [`DurableStore::open`] was given, which
    /// holds `wal/`. `cas` is that database's content-addressed store (`pager.cas()`), where the
    /// page bytes live. `page_size` is the store's page size, needed only to name the canonical
    /// empty root manifest for a database that has never checkpointed.
    pub fn open(
        vfs: Arc<dyn Vfs>,
        store_dir: impl AsRef<Path>,
        cas: Arc<dyn Cas>,
        page_size: usize,
    ) -> Self {
        WalSource {
            vfs,
            wal_dir: store_dir.as_ref().join("wal"),
            cas,
            page_size,
        }
    }

    /// The manifest a follower would be at with zero commits applied.
    ///
    /// It is the checkpoint manifest if the primary has checkpointed (older segments are then gone,
    /// and replay starts there), or the canonical empty root otherwise. This is the `base` of the
    /// first shippable commit.
    pub fn start_head(&self) -> Result<ManifestId> {
        match self.read_checkpoint()? {
            Some((manifest, _lsn)) => Ok(manifest),
            None => Manifest::empty(self.page_size)
                .id()
                .map_err(SyncError::pager("name the empty root manifest")),
        }
    }

    /// Every shippable commit with LSN `>= from`, in commit order.
    ///
    /// Folding always starts from the true beginning of the available log so that each shipment's
    /// `base` is correct even when earlier commits are filtered out; `from` only gates what is
    /// *emitted*. The result is materialised into a `Vec` — a whole database's history in memory is
    /// the price of not depending on a substrate streaming API it does not offer. For a long-running
    /// primary, checkpoint to bound it, exactly as substrate bounds its own recovery.
    pub fn shipments_from(&self, from: Lsn) -> Result<Vec<Shipment>> {
        let checkpoint_lsn = self.read_checkpoint()?.map(|(_, lsn)| lsn);
        let mut base = self.start_head()?;
        let mut pending: BTreeMap<LogicalPageNo, Option<PageId>> = BTreeMap::new();
        let mut out = Vec::new();

        for record in self.records()? {
            // Records at or below a checkpoint are already folded into the checkpoint manifest; the
            // bytes linger on disk (a checkpoint deletes only whole sealed segments) but the
            // transactions have already happened as far as `base` is concerned.
            if checkpoint_lsn.is_some_and(|cp| record.lsn <= cp) {
                continue;
            }
            match record.kind {
                RecordKind::Write { page_no, page } => {
                    pending.insert(page_no, page);
                }
                RecordKind::Commit {
                    manifest,
                    created_at_ms,
                } => {
                    let commit_lsn = record.lsn;
                    let writes = std::mem::take(&mut pending);
                    if commit_lsn >= from {
                        out.push(self.build_shipment(
                            base,
                            manifest,
                            commit_lsn,
                            created_at_ms,
                            &writes,
                        )?);
                    }
                    base = manifest;
                }
                RecordKind::Checkpoint { manifest } => {
                    // An inline checkpoint record: history up to here is captured by `manifest`.
                    base = manifest;
                    pending.clear();
                }
            }
        }
        Ok(out)
    }

    /// The LSN a follower at `head` should stream from next.
    ///
    /// The follower's manifest *is* its position: the primary finds it among its committed manifests
    /// and returns the LSN just after. A follower that has applied nothing (its head is
    /// [`start_head`](Self::start_head)) streams from `0`. A follower whose head is nowhere in this
    /// log has forked from this primary — [`SyncError::FollowerUnknown`].
    pub fn resume_after(&self, head: ManifestId) -> Result<Lsn> {
        if head == self.start_head()? {
            return Ok(0);
        }
        for shipment in self.shipments_from(0)? {
            if shipment.manifest == head {
                return Ok(shipment.commit_lsn + 1);
            }
        }
        Err(SyncError::FollowerUnknown {
            head: head.to_hex(),
        })
    }

    /// The manifest the primary was at as of `lsn` — i.e. the state a point-in-time restore to `lsn`
    /// must land on. `None` if no commit has LSN `<= lsn` in the available log.
    pub fn manifest_at(&self, lsn: Lsn) -> Result<Option<ManifestId>> {
        let mut at = None;
        for shipment in self.shipments_from(0)? {
            if shipment.commit_lsn <= lsn {
                at = Some(shipment.manifest);
            } else {
                break;
            }
        }
        Ok(at)
    }

    /// Assemble one shipment: the commit's writes plus the bytes for each page it set.
    fn build_shipment(
        &self,
        base: ManifestId,
        manifest: ManifestId,
        commit_lsn: Lsn,
        created_at_ms: u64,
        writes: &BTreeMap<LogicalPageNo, Option<PageId>>,
    ) -> Result<Shipment> {
        let mut page_writes = Vec::with_capacity(writes.len());
        let mut blobs = Vec::new();
        for (&page_no, &page) in writes {
            page_writes.push(PageWrite { page_no, page });
            if let Some(id) = page {
                // Only fetch a blob once even if several logical pages point at the same content.
                if !blobs.iter().any(|b: &PageBlob| b.id == id) {
                    let bytes = self
                        .cas
                        .get(id)
                        .map_err(|_| SyncError::MissingPage {
                            lsn: commit_lsn,
                            page_no,
                            page_id: id.to_hex(),
                        })?
                        .into_bytes();
                    blobs.push(PageBlob { id, bytes });
                }
            }
        }
        Ok(Shipment {
            base,
            manifest,
            commit_lsn,
            created_at_ms,
            writes: page_writes,
            pages: blobs,
        })
    }

    /// Every complete record in the log, in LSN order, stopping at the first torn tail.
    ///
    /// A torn record is the live write head or a crash-in-progress commit — never something to ship.
    /// Reading stops there, exactly as substrate's recovery does.
    ///
    /// The subtlety is that decoding an *empty* trailing slice also reads as `Torn`, so the clean end
    /// of a sealed segment and a genuine torn tail look the same to [`Record::decode`]. They are told
    /// apart by whether any bytes are left unread: a clean boundary (`offset == bytes.len()`) means
    /// "on to the next segment"; a torn tail with bytes still after it (`offset < bytes.len()`) is
    /// the live/crashed write head and the true end of the log. Getting this wrong silently truncates
    /// a multi-segment log to its first segment — which is exactly the bug the segment-rollover test
    /// exists to catch.
    fn records(&self) -> Result<Vec<Record>> {
        let mut out = Vec::new();
        for segment in self.segments()? {
            let path = Self::segment_path(&self.wal_dir, segment);
            let bytes = self.vfs.read(&path).map_err(|source| SyncError::Io {
                path: path.clone(),
                source,
            })?;
            let mut offset = 0usize;
            loop {
                match Record::decode(&bytes[offset..]) {
                    ReadOutcome::Record { record, consumed } => {
                        offset += consumed;
                        out.push(record);
                    }
                    ReadOutcome::Torn if offset < bytes.len() => {
                        // A torn tail with unread bytes after it: the crashed/live write head, and the
                        // true end of the log. Stop entirely — nothing past this happened.
                        return Ok(out);
                    }
                    // A clean end of this segment. Move on to the next sealed segment, if any.
                    ReadOutcome::Torn => break,
                }
            }
        }
        Ok(out)
    }

    /// The checkpoint marker's manifest and LSN, if one is durable.
    fn read_checkpoint(&self) -> Result<Option<(ManifestId, Lsn)>> {
        let path = self.wal_dir.join("CHECKPOINT");
        if !self.vfs.exists(&path) {
            return Ok(None);
        }
        let bytes = self.vfs.read(&path).map_err(|source| SyncError::Io {
            path: path.clone(),
            source,
        })?;
        match Record::decode(&bytes) {
            ReadOutcome::Record { record, .. } => match record.kind {
                RecordKind::Checkpoint { manifest } => Ok(Some((manifest, record.lsn))),
                // A checkpoint file holding a non-checkpoint is nonsense; ignore it and fold from the
                // start, which is slower and certainly correct.
                _ => Ok(None),
            },
            // A torn marker means a crash mid-write; the log behind it is whole, so fold from the
            // start and arrive at the same place.
            ReadOutcome::Torn => Ok(None),
        }
    }

    /// The segment numbers present, in order.
    fn segments(&self) -> Result<Vec<u64>> {
        // A missing `wal/` directory means "no log yet", not an error — and the two VFS backends
        // disagree on how they report it (`StdVfs` errors NotFound, `MemVfs` returns nothing and
        // does not track directories as files), so we go by `read_dir` and treat NotFound as empty
        // rather than probing `exists`, which is file-only on the in-memory backend.
        let entries = match self.vfs.read_dir(&self.wal_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(SyncError::Io {
                    path: self.wal_dir.clone(),
                    source,
                })
            }
        };
        let mut out: Vec<u64> = entries
            .iter()
            .filter_map(|p| {
                let name = p.file_name()?.to_str()?;
                name.strip_suffix(".wal")?.parse::<u64>().ok()
            })
            .collect();
        out.sort_unstable();
        Ok(out)
    }

    fn segment_path(dir: &Path, n: u64) -> PathBuf {
        dir.join(format!("{n:012}.wal"))
    }
}
