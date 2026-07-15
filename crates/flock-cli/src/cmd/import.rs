//! `flock import <file>` — the first command a stranger types, and therefore the important one.
//!
//! It creates the pool, creates the branch, creates the table, and snapshots. Four things, no
//! flags, no `init`. Every one of those steps is a step someone could have been asked to type, and
//! each one asked for is a chance to lose them before they have seen a single row.

use crate::error::{CliError, Result};
use crate::workspace::Workspace;
use flock_core::Flock;
use std::path::Path;

/// The branch a brand-new pool starts on.
const DEFAULT_BRANCH: &str = "main";

pub fn run(ws: &Workspace, branch: Option<&str>, file: &Path, table: Option<String>) -> Result<()> {
    let fresh_pool = !ws.exists();

    // Unlike every other command, `import` may be run against a pool that does not exist yet — that
    // is how pools come into existence — so it cannot use `Workspace::resolve`, which exists to
    // refuse unknown branches.
    let branch = match branch {
        Some(b) => b.to_string(),
        None if fresh_pool => DEFAULT_BRANCH.to_string(),
        None => ws.head()?,
    };
    if ws.is_asleep(&branch) {
        return Err(CliError::BranchAsleep { name: branch });
    }

    if !file.is_file() {
        return Err(CliError::NoSuchFile {
            path: file.display().to_string(),
        });
    }
    let reader = reader_for(file)?;
    let table = match table {
        Some(t) => t,
        None => table_name_from(file)?,
    };
    check_identifier(&table)?;

    let mut db = Flock::open(ws.root(), &branch)?;

    // Ask before writing. `CREATE TABLE` would fail on its own, but with DuckDB's message — which
    // is correct, and which does not mention `--table`, or branching, or any of the three things
    // this user might actually want to do next.
    if table_exists(&mut db, &table)? {
        return Err(CliError::TableExists {
            table,
            branch: branch.clone(),
        });
    }

    // The path goes into SQL as a string literal, so a single quote in a filename is doubled. It is
    // not a placeholder because DuckDB's readers take a *literal* here, not a bound parameter.
    let path = file.display().to_string().replace('\'', "''");
    db.execute(&format!(
        "CREATE TABLE \"{table}\" AS SELECT * FROM {reader}('{path}')"
    ))?;

    let rows = db.query(&format!("SELECT count(*) FROM \"{table}\""))?;
    let n = crate::cmd::sql::first_i64(&rows).unwrap_or(0);

    // **The commit.** Without it, the process exits, DuckDB's scratch file is deleted with its
    // TempDir, and the next `flock sql` opens an empty database — having reported a successful
    // import. There is no more important line in this file.
    db.snapshot()?;

    if fresh_pool {
        ws.set_head(&branch)?;
    }

    println!("imported {n} rows into table \"{table}\" on branch \"{branch}\"");
    Ok(())
}

/// Which DuckDB reader to point at the file.
///
/// Not `SELECT * FROM 'file'`, even though DuckDB would infer it: the inference is DuckDB's, the
/// error when it fails is DuckDB's, and it says nothing about what FlockDB does support. Naming the
/// reader means we can name the alternatives when we cannot.
fn reader_for(file: &Path) -> Result<&'static str> {
    let name = file
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();

    // `.csv.gz` first: `Path::extension` on `trades.csv.gz` is `gz`, which is not a format.
    if name.ends_with(".csv")
        || name.ends_with(".tsv")
        || name.ends_with(".csv.gz")
        || name.ends_with(".tsv.gz")
    {
        return Ok("read_csv_auto");
    }
    if name.ends_with(".parquet") {
        return Ok("read_parquet");
    }

    Err(CliError::UnknownFormat {
        path: file.display().to_string(),
        ext: name
            .rsplit_once('.')
            .map(|(_, e)| format!(".{e}"))
            .unwrap_or_else(|| "(none)".to_string()),
    })
}

/// `path/to/daily-trades.csv.gz` → `daily_trades`.
fn table_name_from(file: &Path) -> Result<String> {
    let stem = file
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .split('.')
        .next()
        .unwrap_or_default()
        .to_string();

    let cleaned: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();

    // A table whose name starts with a digit needs quoting forever afterwards, and the user did not
    // choose the name — we derived it. So make it usable.
    let cleaned = if cleaned.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        format!("t_{cleaned}")
    } else {
        cleaned
    };

    check_identifier(&cleaned)?;
    Ok(cleaned)
}

/// A table name we are willing to interpolate into SQL.
///
/// The name is quoted at the call site (`"name"`), so this is belt and braces — but the belt is
/// cheap and the alternative is a table name that closes the quote. There is no user input in
/// FlockDB's SQL that is not either a bound parameter or checked here.
fn check_identifier(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');

    if ok {
        Ok(())
    } else {
        Err(CliError::BadTableName {
            name: name.to_string(),
        })
    }
}

fn table_exists(db: &mut flock_core::Db, table: &str) -> Result<bool> {
    let escaped = table.replace('\'', "''");
    let rows = db.query(&format!(
        "SELECT count(*) FROM duckdb_tables() WHERE table_name = '{escaped}'"
    ))?;
    Ok(crate::cmd::sql::first_i64(&rows).unwrap_or(0) > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_name_is_derived_from_the_file_name() {
        assert_eq!(table_name_from(Path::new("trades.csv")).unwrap(), "trades");
        assert_eq!(
            table_name_from(Path::new("/tmp/daily-trades.csv.gz")).unwrap(),
            "daily_trades"
        );
        assert_eq!(
            table_name_from(Path::new("2024.parquet")).unwrap(),
            "t_2024"
        );
    }

    #[test]
    fn a_file_name_cannot_smuggle_sql_through_the_table_name() {
        // The derived name is interpolated into `CREATE TABLE "..."`. If a file called
        // `a";DROP TABLE t;--.csv` produced a name with a quote in it, the quoting would close and
        // the rest would run. It cannot: every non-alphanumeric becomes `_`.
        let name = table_name_from(Path::new("a\";DROP TABLE t;--.csv")).unwrap();
        assert!(!name.contains('"'), "{name} still contains a quote");
        assert!(check_identifier(&name).is_ok());
    }

    #[test]
    fn an_unknown_extension_is_refused_by_name() {
        let err = reader_for(Path::new("data.xlsx")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(".xlsx"), "the message must name the extension");
        assert!(msg.contains("read_json_auto"), "and offer the way out");
    }

    #[test]
    fn the_readers_we_claim_to_support_are_the_ones_we_route() {
        assert_eq!(reader_for(Path::new("a.csv")).unwrap(), "read_csv_auto");
        assert_eq!(reader_for(Path::new("a.TSV")).unwrap(), "read_csv_auto");
        assert_eq!(reader_for(Path::new("a.csv.gz")).unwrap(), "read_csv_auto");
        assert_eq!(reader_for(Path::new("a.parquet")).unwrap(), "read_parquet");
    }
}
