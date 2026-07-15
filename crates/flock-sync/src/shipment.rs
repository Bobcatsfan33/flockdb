//! The unit of replication: one committed transaction, ready to send to a follower.
//!
//! # Why a shipment carries page bytes, and the WAL does not
//!
//! Substrate's WAL is deliberately tiny: it records *ordering* — which content hash became which
//! logical page, and when — and never the page bytes themselves, because the bytes are already
//! durable in the CAS before any record referencing them is written (substrate `docs/02 §3.1`).
//! That is what keeps the fsync on the commit path fast.
//!
//! A follower on another machine does not share that CAS. So a shipment is the primary's fsync'd
//! commit record **plus** the page bytes it references — the content the WAL is entitled to omit
//! because it is sitting on the same disk, and which a follower has no other way to obtain. Shipping
//! the record without the bytes would send a follower a set of instructions referring to pages it
//! has never seen.
//!
//! We ship the *durable commit record*, not a re-derived diff (the F3 mandate): the `manifest` and
//! `created_at_ms` below are lifted verbatim from the `Commit` record substrate fsync'd, and the
//! follower re-derives that exact manifest or refuses to advance.

use serde::{Deserialize, Serialize};
use substrate_pager::{LogicalPageNo, ManifestId, PageId};
use substrate_wal::Lsn;

/// One logical-page change within a committed transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageWrite {
    /// The logical page that changed.
    pub page_no: LogicalPageNo,
    /// The content now at that page, or `None` if the page was removed.
    pub page: Option<PageId>,
}

/// The bytes of one page, filed under the content hash they must reproduce.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageBlob {
    /// The content hash the bytes must hash to on the follower.
    pub id: PageId,
    /// The page bytes.
    pub bytes: Vec<u8>,
}

/// One committed transaction, everything a follower needs to reproduce it, and nothing else.
///
/// A `Shipment` is [`Serialize`]/[`Deserialize`], so it is a wire message as much as an in-process
/// value: a primary can send it over a socket and a follower can apply it, byte-for-byte identical
/// to the primary at that commit. `tests/shipping.rs` proves the round trip through JSON.
///
/// The `base` and `manifest` fields are the two ends of one commit: applying `writes` to `base`
/// must produce `manifest`, and the follower checks exactly that. `base` lets a divergence be caught
/// *before* anything is applied; `manifest` catches one *after*, against substrate's own derivation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shipment {
    /// The manifest this commit builds on — the follower's head must equal this to apply it.
    pub base: ManifestId,
    /// The manifest this commit produces. The follower re-derives it and refuses to differ.
    pub manifest: ManifestId,
    /// The commit record's LSN on the primary. Monotonic; the coordinate for streaming and PITR.
    pub commit_lsn: Lsn,
    /// The wall-clock timestamp baked into `manifest`. Replayed verbatim so the hash matches.
    pub created_at_ms: u64,
    /// The transaction's page changes, in canonical (page-number) order.
    pub writes: Vec<PageWrite>,
    /// The bytes for every write that set a page (not for removals), to seed the follower's CAS.
    pub pages: Vec<PageBlob>,
}

impl Shipment {
    /// The bytes for a page id referenced by this shipment, if it carries them.
    pub fn bytes_for(&self, id: PageId) -> Option<&[u8]> {
        self.pages
            .iter()
            .find(|blob| blob.id == id)
            .map(|blob| blob.bytes.as_slice())
    }
}
