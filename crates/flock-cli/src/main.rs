//! `flock` — the command line over [`flock_core`].
//!
//! # The one thing this binary is for
//!
//! A stranger, on a machine that has never heard of us, forking a database **within ninety seconds
//! of opening the README**. Not "could in principle". Actually doing it. Every design decision in
//! this crate is downstream of that sentence, including the ones that look like laziness:
//!
//! * **There is no `flock init`.** `flock import` creates the pool. A ceremony step is a step
//!   someone skips and then gets an error they cannot read.
//! * **There is no `flock commit`.** Every command that can change data takes a snapshot before it
//!   returns, because a CLI is a sequence of *processes*, and an uncommitted write in a dead
//!   process's scratch file is a write that never happened. See [`cmd::sql`].
//! * **`flock branch NAME` also checks out `NAME`.** `git branch` does not, and `git branch` is
//!   right for git — but the next thing a person wants after forking a database is to *write to the
//!   fork*, and making them type a second command to get there is one more place to lose them.
//! * **Errors say what to type next.** A `Debug` dump of an error enum is a message written for the
//!   person who caused the bug, not the person who hit it.
//!
//! # State on disk
//!
//! ```text
//!   .flock/                 the pool — override with --pool or $FLOCK_POOL
//!     HEAD                  the branch `flock sql` talks to, as a bare name
//!     cas/                  substrate's content-addressed store — SHARED by every branch
//!     dbs/<branch>/         one branch: a private WAL, and symlinks into cas/
//!     asleep/<branch>.json  a branch that is in object storage: the WakeToken and where it is
//! ```
//!
//! `HEAD` is a file with a branch name in it and nothing else. It is not a lock, it is not a
//! transaction, and two `flock` processes racing on it will interleave — the CLI is a *single-user*
//! tool, and the concurrency story belongs to the library, which has one.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::todo)]
#![warn(rust_2018_idioms)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

mod cli;
mod cmd;
mod error;
mod render;
mod workspace;

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    // `main` returns an ExitCode rather than a Result, because `Result`'s own Termination impl
    // prints the error with `Debug` — i.e. `Pager { op: "restore", source: ... }`. Every error in
    // this program has a Display that names the next thing to type, and this is what makes sure the
    // user sees *that* and not the enum.
    match cli::Cli::parse().run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
