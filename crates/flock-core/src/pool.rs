//! The pool: the directory a set of databases share, and the names they are allowed to have.
//!
//! # Layout
//!
//! ```text
//!   <root>/
//!     cas/
//!       pages/           the substrate CAS      — SHARED by every database in the pool
//!       manifests/       the manifest store     — SHARED by every database in the pool
//!     dbs/<name>/
//!       pages      →     symlink to ../../cas/pages
//!       manifests  →     symlink to ../../cas/manifests
//!       wal/             this database's write-ahead log — PRIVATE
//! ```
//!
//! **The CAS is shared and that is the entire economic argument.** Ten thousand databases forked
//! from one template store one copy of the template's pages (docs/02 §3.1). If each database had its
//! own CAS, a fork would have to copy, and FlockDB would be a worse way to run ten thousand DuckDBs
//! rather than a better one.
//!
//! **The WAL is per-database and that is the isolation.** A database's head — which manifest it
//! currently *is* — is private to it, which is why a write to a fork cannot be seen by its parent no
//! matter what either of them does.
//!
//! # Why there are symlinks in a storage engine
//!
//! Because [`substrate_wal::DurableStore`] — which owns substrate's commit protocol, and which we
//! are not going to reimplement — takes **one directory** and puts the CAS *and* the WAL inside it:
//!
//! ```text
//!   DurableStore::open(vfs, dir, cfg)  →  dir/pages   (CAS)
//!                                         dir/manifests
//!                                         dir/wal
//! ```
//!
//! One store, one directory, one CAS, one WAL. That cannot express *"many databases, one CAS, a WAL
//! each"*, which is the only shape FlockDB has. Point two `DurableStore`s at one directory and they
//! share the CAS **and the WAL** — two writers interleaving records in one log, and a recovery that
//! restores whichever database committed last. Give each its own directory and they get their own
//! CAS, a fork copies every page, and the product is gone.
//!
//! So each database gets its own directory — its own real, private `wal/` — and its `pages/` and
//! `manifests/` are **symlinks into the pool's one CAS**. `FsCas` opens `<db>/pages`, the kernel
//! follows the link, and every database in the pool reads and writes the same content-addressed
//! store. Pages and manifests are immutable and named by their own hash, so concurrent writers
//! cannot conflict: two processes writing identical bytes write the same filename with the same
//! content.
//!
//! The clever alternative was a custom [`substrate_pager::Vfs`] that rewrites `<db>/pages/*` to
//! `<pool>/cas/pages/*` in flight. It is one indirection layer, in the read path, in the storage
//! engine, and CLAUDE.md rule 7 says take the obvious one. A symlink is POSIX, is visible in `ls`,
//! and is understood by every tool a person will reach for at 3 a.m.
//!
//! **The right fix is upstream**: a `DurableStore::from_parts(pager, wal)` that lets the caller
//! supply a CAS and a log that live in different places. Until substrate has it, this is the honest
//! way to use the tested commit protocol instead of hand-rolling a second one.

use crate::error::{FlockError, Result};
use std::path::{Path, PathBuf};

/// The pool's one content-addressed store. Every database in the pool shares it.
pub(crate) fn cas_dir(root: &Path) -> PathBuf {
    root.join("cas")
}

/// One database's own directory — its private WAL, and symlinks to the shared CAS.
pub(crate) fn db_dir(root: &Path, name: &str) -> PathBuf {
    root.join("dbs").join(name)
}

/// Build a database's directory: a real `wal/`, and `pages`/`manifests` linked to the shared CAS.
///
/// Idempotent. Opening an existing database re-runs this and changes nothing, which is what makes it
/// safe to call from `open` rather than from a separate `create` that someone would forget.
pub(crate) fn link_db_to_shared_cas(root: &Path, name: &str) -> Result<PathBuf> {
    let cas = cas_dir(root);
    let dir = db_dir(root, name);

    // The shared CAS. `DurableStore` will create `<db>/pages` and `<db>/manifests` through the
    // symlinks, so the targets must exist first — a symlink to a missing directory is a dangling
    // link, and `create_dir_all` on one fails with a bewildering ENOENT on a path that plainly
    // exists.
    for sub in ["pages", "manifests"] {
        let target = cas.join(sub);
        std::fs::create_dir_all(&target).map_err(FlockError::pool("create shared CAS", &target))?;
    }
    std::fs::create_dir_all(&dir).map_err(FlockError::pool("create database directory", &dir))?;

    for sub in ["pages", "manifests"] {
        let link = dir.join(sub);
        match std::fs::symlink_metadata(&link) {
            // Already linked (or already a directory, if someone pre-made one). Leave it.
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(FlockError::pool("inspect database directory", &link)(e)),
        }

        // Relative, not absolute: a pool that is moved, copied, or mounted at a different path in a
        // container must still work. An absolute symlink would point at the old machine's layout,
        // and the failure would look like an empty database rather than a broken link.
        let target = Path::new("..").join("..").join("cas").join(sub);

        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link)
            .map_err(FlockError::pool("link database to the shared CAS", &link))?;

        #[cfg(not(unix))]
        {
            // Windows can make directory symlinks, but only with a privilege that is off by default,
            // so a plain `symlink_dir` here would fail for most users with an access-denied error
            // that has nothing to do with what they did wrong. FlockDB F1 targets Linux and macOS,
            // and we would rather say that than ship something that half-works.
            let _ = target;
            return Err(FlockError::Unsupported {
                what: "sharing one CAS between databases on this platform",
                why: "FlockDB links each database's `pages/` and `manifests/` into the pool's shared \
                      content-addressed store with a symlink, and creating one on Windows needs a \
                      privilege that is not granted by default. F1 supports Linux and macOS.",
            });
        }
    }

    Ok(dir)
}

/// Check that a database name can safely be a directory component.
///
/// # Why this is not a style rule
///
/// The name becomes a directory under `<root>/dbs/`. A name of `../../../etc` puts a database's
/// durable head *outside its pool*, and a pool is a **security boundary**: docs/02 §9.1 promises
/// that two stores in different pools never share a page, precisely so that data cannot cross a
/// classification boundary through the storage layer. A name that escapes the pool directory is a
/// hole straight through that promise, and it would be opened by a caller who thought they were
/// naming a database.
///
/// We reject rather than sanitise. Sanitising means `../../etc` quietly becomes `etc` and the caller
/// gets a database they did not ask for, under a name they did not choose — which is how you end up
/// with two tenants writing to one database and finding out from a customer.
pub(crate) fn validate_name(name: &str) -> Result<()> {
    let bad = |reason| {
        Err(FlockError::BadName {
            name: name.to_string(),
            reason,
        })
    };

    if name.is_empty() {
        return bad("it is empty");
    }
    if name == "." || name == ".." {
        return bad("'.' and '..' are directories that already mean something else");
    }
    if name.contains(['/', '\\']) {
        return bad(
            "it contains a path separator, which would place the database outside its pool",
        );
    }
    if name.contains('\0') {
        return bad("it contains a NUL byte");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return bad("it contains characters outside [A-Za-z0-9._-]");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_names_are_accepted() {
        for name in ["sales", "acme-prod", "tenant_42", "v1.2.3", "A"] {
            assert!(
                validate_name(name).is_ok(),
                "{name} should be a legal database name"
            );
        }
    }

    #[test]
    fn a_name_that_would_escape_the_pool_is_refused_and_not_sanitised() {
        // The whole reason this function exists. A pool is a security boundary (docs/02 §9.1), and
        // a name that walks out of it is a way to write a "database" anywhere on the filesystem.
        for name in ["../escape", "..", "/etc/passwd", "a/b", "a\\b", "."] {
            let err = validate_name(name).unwrap_err();
            assert!(
                matches!(err, FlockError::BadName { .. }),
                "{name} must be refused outright, not cleaned up into something else"
            );
        }
    }

    #[test]
    fn an_empty_name_is_refused() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn a_name_with_a_nul_byte_is_refused() {
        assert!(validate_name("sales\0evil").is_err());
    }

    #[test]
    fn a_valid_name_lands_inside_the_pool() {
        let root = Path::new("/pool");
        assert_eq!(db_dir(root, "sales"), Path::new("/pool/dbs/sales"));
        assert_eq!(cas_dir(root), Path::new("/pool/cas"));
    }

    #[test]
    fn two_databases_in_a_pool_end_up_sharing_one_cas() {
        // The economic argument, asserted. If these two ever resolve to different directories, a
        // fork stops sharing pages and starts copying them, and nothing else in the test suite
        // would notice — the data would all still be correct, just enormous.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let a = link_db_to_shared_cas(root, "base").unwrap();
        let b = link_db_to_shared_cas(root, "fork").unwrap();

        let a_pages = std::fs::canonicalize(a.join("pages")).unwrap();
        let b_pages = std::fs::canonicalize(b.join("pages")).unwrap();
        let shared = std::fs::canonicalize(cas_dir(root).join("pages")).unwrap();

        assert_eq!(a_pages, shared, "database `base` is not on the pool's CAS");
        assert_eq!(b_pages, shared, "database `fork` is not on the pool's CAS");
        assert_eq!(
            a_pages, b_pages,
            "two databases in one pool must share pages"
        );
    }

    #[test]
    fn linking_a_database_twice_is_harmless() {
        // `open` calls this every time, including on an existing database. If it were not
        // idempotent, the second open of every database would fail with EEXIST.
        let dir = tempfile::tempdir().unwrap();
        link_db_to_shared_cas(dir.path(), "sales").unwrap();
        link_db_to_shared_cas(dir.path(), "sales").unwrap();
    }
}
