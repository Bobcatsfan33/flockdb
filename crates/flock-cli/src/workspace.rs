//! The pool on disk, and which branch `HEAD` points at.
//!
//! # Why a `HEAD` file and not a flag
//!
//! Because the quickstart has to fit in five commands, and `--branch` on every one of them is not
//! five commands, it is five commands plus a thing to remember. `flock branch what-if` switches;
//! everything after it talks to `what-if`; `--branch main` is the escape hatch for the one command
//! that wants the other side. That is the shape of `git`, and it is the shape people already have.
//!
//! # What is in the pool, and what is ours
//!
//! `cas/` and `dbs/` belong to `flock-core` (see its `pool` module). `HEAD` and `asleep/` are the
//! CLI's — the library has no concept of a "current" database, and should not: a *library* whose
//! behaviour depends on a file in a directory is a library that surprises its callers.

use crate::error::{CliError, Result};
use std::path::{Path, PathBuf};

/// The default pool, relative to the working directory. Deliberately hidden, deliberately not in
/// `$HOME`: a database made in a directory belongs to that directory, and `rm -rf` on the directory
/// should take it with it. A `~/.flock` that quietly accumulates every experiment anyone ever ran
/// is how you end up with 40 GB you are afraid to delete.
const DEFAULT_POOL: &str = ".flock";

pub struct Workspace {
    root: PathBuf,
}

/// Everything `flock sleep` has to remember for `flock wake` to work.
///
/// The `WakeToken` is substrate's (a pool, a 32-byte manifest id, a page size) and is stored as the
/// JSON substrate itself produces — we do not re-model it, because a second model of someone else's
/// wire format is a second thing to get wrong.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SleepRecord {
    /// The directory the pages went to. Absolute: the pool may be opened from another cwd.
    pub tier: PathBuf,
    /// The dedup pool the tier is bound to. Substrate refuses to wake a database into a pool that
    /// is not the one it slept in — that is the classification boundary, working.
    pub pool: String,
    /// `WakeToken::to_json`, verbatim.
    pub token: String,
}

impl Workspace {
    /// `--pool`, else `$FLOCK_POOL`, else `./.flock`.
    pub fn locate(explicit: Option<PathBuf>) -> Workspace {
        let root = explicit
            .or_else(|| std::env::var_os("FLOCK_POOL").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_POOL));
        Workspace { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Does a pool exist here at all? `HEAD` is the marker, because `HEAD` is the thing only
    /// `flock` writes — an empty `.flock/` directory someone made by hand is not a pool.
    pub fn exists(&self) -> bool {
        self.head_path().is_file()
    }

    fn head_path(&self) -> PathBuf {
        self.root.join("HEAD")
    }

    fn dbs_dir(&self) -> PathBuf {
        self.root.join("dbs")
    }

    fn asleep_dir(&self) -> PathBuf {
        self.root.join("asleep")
    }

    pub fn sleep_record_path(&self, branch: &str) -> PathBuf {
        self.asleep_dir().join(format!("{branch}.json"))
    }

    /// The branch commands act on when `--branch` is absent.
    pub fn head(&self) -> Result<String> {
        if !self.exists() {
            return Err(CliError::NoPool {
                pool: self.root.clone(),
            });
        }
        let name = std::fs::read_to_string(self.head_path()).map_err(CliError::io(
            "read HEAD",
            self.head_path(),
            "The pool's HEAD file says which branch is checked out. Recreate it with `flock checkout <branch>`.",
        ))?;
        Ok(name.trim().to_string())
    }

    pub fn set_head(&self, branch: &str) -> Result<()> {
        std::fs::create_dir_all(&self.root).map_err(CliError::io(
            "create the pool",
            &self.root,
            "Check that the parent directory exists and is writable.",
        ))?;
        std::fs::write(self.head_path(), format!("{branch}\n")).map_err(CliError::io(
            "write HEAD",
            self.head_path(),
            "Check that the pool directory is writable.",
        ))
    }

    /// Every branch in the pool, sorted, awake or asleep.
    pub fn branches(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for dir in [self.dbs_dir(), self.asleep_dir()] {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                // A pool with no `asleep/` has simply never slept anything. Not an error.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(CliError::io(
                        "list branches",
                        &dir,
                        "Check that the pool directory is readable.",
                    )(e))
                }
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                // `asleep/` holds `<branch>.json`; `dbs/` holds `<branch>/`.
                let name = name.strip_suffix(".json").unwrap_or(&name).to_string();
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        names.sort();
        Ok(names)
    }

    pub fn is_asleep(&self, branch: &str) -> bool {
        self.sleep_record_path(branch).is_file()
    }

    /// Resolve the branch a command should act on, and refuse — with a list — if it is not there.
    ///
    /// This is the check that turns "thread 'main' panicked" and "No such file or directory (os
    /// error 2)" into a sentence. It is called by every command that touches a branch.
    pub fn resolve(&self, explicit: Option<&str>) -> Result<String> {
        let name = match explicit {
            Some(n) => n.to_string(),
            None => self.head()?,
        };
        if !self.exists() {
            return Err(CliError::NoPool {
                pool: self.root.clone(),
            });
        }
        let known = self.branches()?;
        if !known.contains(&name) {
            return Err(CliError::UnknownBranch { name, known });
        }
        if self.is_asleep(&name) {
            return Err(CliError::BranchAsleep { name });
        }
        Ok(name)
    }

    pub fn read_sleep_record(&self, branch: &str) -> Result<SleepRecord> {
        let path = self.sleep_record_path(branch);
        let text = std::fs::read_to_string(&path).map_err(CliError::io(
            "read the sleep record",
            &path,
            "`flock sleep` writes it; `flock wake` reads it. If it is missing, the branch is not asleep.",
        ))?;
        serde_json::from_str(&text).map_err(|source| CliError::BadSleepRecord {
            name: branch.to_string(),
            path,
            source,
        })
    }

    pub fn write_sleep_record(&self, branch: &str, record: &SleepRecord) -> Result<()> {
        let dir = self.asleep_dir();
        std::fs::create_dir_all(&dir).map_err(CliError::io(
            "create the sleep-record directory",
            &dir,
            "Check that the pool directory is writable.",
        ))?;
        let path = self.sleep_record_path(branch);
        let json =
            serde_json::to_string_pretty(record).map_err(|source| CliError::BadSleepRecord {
                name: branch.to_string(),
                path: path.clone(),
                source,
            })?;
        std::fs::write(&path, json).map_err(CliError::io(
            "write the sleep record",
            &path,
            "Check that the pool directory is writable. Nothing was dropped: the database is still \
             in the tier, and still local.",
        ))
    }

    pub fn remove_sleep_record(&self, branch: &str) -> Result<()> {
        let path = self.sleep_record_path(branch);
        std::fs::remove_file(&path).map_err(CliError::io(
            "remove the sleep record",
            &path,
            "The branch is awake; this file is what marks it asleep. Delete it by hand.",
        ))
    }

    /// Delete a branch's own directory — its private WAL and its symlinks into the shared CAS.
    ///
    /// **This does not delete any page.** Pages live in `cas/`, which every branch shares; removing
    /// one branch's directory removes its *head*, not its data. That is the difference between
    /// `flock sleep` (which frees compute) and a garbage collection (which frees disk, and which is
    /// a fleet-level decision, not something a `sleep` should do behind your back).
    pub fn remove_branch_dir(&self, branch: &str) -> Result<()> {
        let dir = self.dbs_dir().join(branch);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CliError::io(
                "remove the branch directory",
                &dir,
                "Check permissions on the pool. The database is safe: this directory holds a \
                 write-ahead log and two symlinks, and the pages are in the pool's shared CAS.",
            )(e)),
        }
    }

    /// The pool's shared content-addressed store — where `flock wake` puts the pages it fetched.
    pub fn cas_dir(&self) -> PathBuf {
        self.root.join("cas")
    }
}
