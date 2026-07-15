//! `flock branch`, `flock branches`, `flock checkout`.
//!
//! # `flock branch NAME` checks out NAME, and `git branch NAME` does not
//!
//! This is a deliberate divergence from the tool everyone's fingers already know, so it needs a
//! reason. It is this: after `git branch`, the next thing you do is usually *not* work on it. After
//! forking a database, the next thing you do is **always** work on it — that is what forking a
//! database is *for*. `--no-checkout` is there for the other case, and the message says which one
//! happened, every time, so nobody has to remember.

use crate::error::{CliError, Result};
use crate::workspace::Workspace;
use flock_core::Flock;

pub fn fork(ws: &Workspace, from: Option<&str>, name: &str, no_checkout: bool) -> Result<()> {
    let parent = ws.resolve(from)?;

    let mut db = Flock::open(ws.root(), &parent)?;
    // `Db::fork` snapshots the parent first (otherwise the fork would branch from the parent's last
    // snapshot and silently drop everything since), refuses a name that is taken, and copies no
    // pages. All three are the library's, and all three are tested there.
    let _forked = db.fork(name)?;

    println!("forked \"{parent}\" → \"{name}\" (no pages copied)");

    if no_checkout {
        println!("still on \"{parent}\" — switch with: flock checkout {name}");
    } else {
        ws.set_head(name)?;
        println!("switched to branch \"{name}\"");
    }
    Ok(())
}

pub fn list(ws: &Workspace) -> Result<()> {
    if !ws.exists() {
        return Err(CliError::NoPool {
            pool: ws.root().to_path_buf(),
        });
    }
    let head = ws.head()?;
    for name in ws.branches()? {
        let mark = if name == head { "*" } else { " " };
        let state = if ws.is_asleep(&name) {
            "  (asleep — flock wake it)"
        } else {
            ""
        };
        println!("{mark} {name}{state}");
    }
    Ok(())
}

pub fn checkout(ws: &Workspace, name: &str) -> Result<()> {
    if !ws.exists() {
        return Err(CliError::NoPool {
            pool: ws.root().to_path_buf(),
        });
    }
    let known = ws.branches()?;
    if !known.contains(&name.to_string()) {
        return Err(CliError::UnknownBranch {
            name: name.to_string(),
            known,
        });
    }

    ws.set_head(name)?;
    println!("switched to branch \"{name}\"");

    // Checking out a sleeping branch is allowed — you may well be about to wake it — but it must
    // not be a surprise when the next `flock sql` refuses.
    if ws.is_asleep(name) {
        println!("\"{name}\" is asleep in object storage. Bring it back with: flock wake {name}");
    }
    Ok(())
}
