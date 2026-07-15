# flockdb (Python)

**A DuckDB you can fork in a millisecond and snapshot for free.**

```python
import flockdb

db = flockdb.open("./warehouse")                      # a pool; created if it isn't there
db.sql("CREATE TABLE trades AS SELECT * FROM 'trades.csv'")

what_if = db.branch("what-if")                        # a fork. no bytes copied.
what_if.sql("DELETE FROM trades WHERE symbol = 'AAPL'")

print(db.sql("SELECT count(*) FROM trades"))          # the parent never noticed
```

## `pip install flockdb` does **not** work, and we are not going to pretend it does

**There is no `flockdb` package on PyPI.** Nobody has published one, this project has no PyPI
credentials, and a README that says `pip install flockdb` when that command installs *someone
else's* package — or nothing — is worse than no README at all.

What works today, from a checkout:

```bash
git clone https://github.com/Bobcatsfan33/flockdb
cd flockdb/bindings/python
pip install maturin
maturin develop --release          # builds the extension into the active venv
```

or, equivalently, `pip install -e .` (which uses maturin as the build backend — same thing, and it
will fetch maturin itself).

**The first build compiles DuckDB from source and takes minutes, not seconds.** That is not a
figure of speech and it is not a thing we can hide: `libduckdb-sys` builds ~350,000 lines of C++.
It is cached afterwards.

When there is a wheel on PyPI, this section will say so, and it will say it *after* the wheel
exists.

## The API

| | |
| --- | --- |
| `flockdb.open(path, branch="main")` | Open or create a pool. Returns a `Db`. |
| `db.sql(query)` | Run SQL. Returns `Rows`. **Snapshots before it returns** — see below. |
| `db.branch(name)` | Fork. Returns a `Db` on the new branch. No bytes copied. |
| `db.checkout(name)` | A handle on another branch of the same pool. Changes nothing on disk. |
| `db.branches()` | The branches in this pool. |
| `db.snapshot()` | Take a snapshot; returns its id (hex). |
| `db.restore(id)` | Go back to one. Everything since is discarded. |
| `db.export_duckdb(path)` | Write a plain `.duckdb` file with nothing of ours in it. |
| `rows.columns`, `len(rows)`, `print(rows)` | No pyarrow needed. |
| `rows.to_arrow()` / `rows.df()` / `rows.to_ipc()` | pyarrow / pandas / raw Arrow IPC bytes. |

### `sql()` snapshots, every time

A write lives in DuckDB's scratch file until a snapshot folds it into the page store. A Python
process that exits without one loses it. So `sql()` takes the snapshot for you, and `snapshot()`
exists only for when you want the *id* back.

### What is not true yet

* **No wheel on PyPI.** See above.
* **`sql()` materialises the whole result** before returning it. A `SELECT *` over a database bigger
  than RAM will fail the way any such program fails.
* **Waking a sleeping database downloads all of it.** Substrate wakes lazily; DuckDB needs a whole
  file, so FlockDB does not get to. `sleep`/`wake` are on the CLI, not on this API, for exactly that
  reason — see the top-level README.
