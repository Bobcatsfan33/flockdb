//! The command surface. Six verbs, and no more than six.
//!
//! Every verb here earns its place by being *in the quickstart* or by being the thing a person asks
//! for immediately after finishing it. `flock gc`, `flock restore <snapshot>`, `flock export`,
//! `flock serve` are all reasonable and none of them is here, because the cost of a CLI is not the
//! code, it is the `--help` output that a newcomer has to read before they are allowed to be
//! productive.

use crate::error::Result;
use crate::workspace::Workspace;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "flock",
    version,
    about = "A DuckDB you can fork in a millisecond.",
    long_about = "A DuckDB you can fork in a millisecond, and snapshot for free.\n\n\
                  Import a file, query it, fork it. A fork copies no bytes and is a real, separate \
                  database: a write to the fork is never visible in its parent.\n\n\
                  State lives in ./.flock (override with --pool or $FLOCK_POOL)."
)]
pub struct Cli {
    /// The pool directory. Defaults to ./.flock, or $FLOCK_POOL if it is set.
    ///
    /// A pool is a security boundary, not a namespace: two databases in one pool share pages (which
    /// is what makes a fork free), and two databases in different pools never share a page even when
    /// their bytes are identical.
    #[arg(long, global = true, value_name = "DIR")]
    pool: Option<PathBuf>,

    /// Act on this branch instead of the checked-out one.
    #[arg(long, short, global = true, value_name = "NAME")]
    branch: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a database from a CSV or Parquet file.
    ///
    /// This is also how a pool comes into existence — there is no `flock init`, because an extra
    /// mandatory step at the very start of the quickstart is the most expensive step there is.
    Import {
        /// The file. .csv, .tsv, .csv.gz, .tsv.gz or .parquet.
        file: PathBuf,

        /// The table to create. Defaults to the file's name.
        #[arg(long, value_name = "NAME")]
        table: Option<String>,
    },

    /// Run SQL. Prints a table.
    ///
    /// Takes a snapshot when it returns, so a write survives the process exiting. See the crate
    /// docs for why that is not optional in a CLI.
    Sql {
        /// The query. Quote it.
        query: String,
    },

    /// Fork the current branch, and switch to the fork.
    ///
    /// No bytes are copied. The fork is a real, separate database that happens to share every page
    /// with its parent, and a write to it is never visible in the parent — not because we check,
    /// but because a manifest is an immutable value and the fork holds a different one.
    Branch {
        /// The name for the fork.
        name: String,

        /// Create the fork but stay where you are.
        #[arg(long)]
        no_checkout: bool,
    },

    /// List the branches in this pool.
    Branches,

    /// Switch the checked-out branch.
    Checkout {
        /// The branch to switch to.
        name: String,
    },

    /// Put a branch in object storage and release its compute.
    ///
    /// What is left of the database afterwards is a WakeToken: a pool, a 32-byte manifest id, and a
    /// page size. That is why a million sleeping databases fit in a registry on a laptop.
    Sleep {
        /// Where to put it. A **directory on this machine**.
        ///
        /// Defaults to <pool>/cold. The engine will speak to S3, GCS or Azure through
        /// `flock_core::RemoteTier`; this CLI ships the filesystem backend only, and says so rather
        /// than shipping an S3 path nobody has ever run. See the README.
        #[arg(long, value_name = "DIR")]
        tier: Option<PathBuf>,
    },

    /// Bring a sleeping branch back out of object storage.
    Wake {
        /// The branch to wake.
        name: String,
    },
}

impl Cli {
    pub fn run(self) -> Result<()> {
        let ws = Workspace::locate(self.pool);
        let branch = self.branch.as_deref();

        match self.command {
            Command::Import { file, table } => crate::cmd::import::run(&ws, branch, &file, table),
            Command::Sql { query } => crate::cmd::sql::run(&ws, branch, &query),
            Command::Branch { name, no_checkout } => {
                crate::cmd::branch::fork(&ws, branch, &name, no_checkout)
            }
            Command::Branches => crate::cmd::branch::list(&ws),
            Command::Checkout { name } => crate::cmd::branch::checkout(&ws, &name),
            Command::Sleep { tier } => crate::cmd::tier::sleep(&ws, branch, tier),
            Command::Wake { name } => crate::cmd::tier::wake(&ws, &name),
        }
    }
}
