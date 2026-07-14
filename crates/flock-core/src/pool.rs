//! The pool: the directory a set of databases share, and the names they are allowed to have.
//!
//! # Layout
//!
//! ```text
//!   <root>/
//!     pages/            one substrate CAS + manifest store, SHARED by every database here
//!     wal/<name>/       one write-ahead log per database — the durable "which manifest am I"
//! ```
//!
//! **The CAS is shared and that is the entire economic argument.** Ten thousand databases forked
//! from one template store one copy of the template's pages (docs/02 §3.1). If each database had
//! its own CAS, a fork would have to copy, and FlockDB would be a worse way to run ten thousand
//! DuckDBs rather than a better one.
//!
//! **The WAL is per-database and that is the isolation.** A database's head — which manifest it
//! currently *is* — is private to it, which is why a write to a fork cannot be seen by its parent
//! no matter what either of them does.

use crate::error::{FlockError, Result};
use std::path::{Path, PathBuf};

/// Where substrate's pages and manifests live, under the pool root.
pub(crate) fn pages_dir(root: &Path) -> PathBuf {
    root.join("pages")
}

/// Where one database's write-ahead log lives, under the pool root.
pub(crate) fn wal_dir(root: &Path, name: &str) -> PathBuf {
    root.join("wal").join(name)
}

/// Check that a database name can safely be a directory component.
///
/// # Why this is not a style rule
///
/// The name becomes a directory under `<root>/wal/`. A name of `../../../etc` puts a database's
/// durable head *outside its pool*, and a pool is a **security boundary**: docs/02 §9.1 promises
/// that two stores in different pools never share a page, precisely so that data cannot cross a
/// classification boundary through the storage layer. A name that escapes the pool directory is a
/// hole straight through that promise, and it would be opened by a caller who thought they were
/// naming a database.
///
/// We reject rather than sanitise. Sanitising means `../../etc` quietly becomes `etc` and the
/// caller gets a database they did not ask for, under a name they did not choose — which is how
/// you end up with two tenants writing to one database and finding out from a customer.
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
        assert_eq!(wal_dir(root, "sales"), Path::new("/pool/wal/sales"));
        assert_eq!(pages_dir(root), Path::new("/pool/pages"));
    }
}
