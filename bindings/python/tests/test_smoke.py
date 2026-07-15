"""The Python binding, exercised the way a person would.

# Why this test exists

The binding is a `cdylib`: `cargo test` proves it *links*, not that `import flockdb` works, that the
methods are wired to the right Rust, or that fork isolation survives the FFI boundary. A binding that
compiles and then raises `AttributeError` on the first call is a compiling binding and a broken product.

So this drives the whole surface from Python — open, query, fork, isolation, snapshot/restore, and the
escape hatch — against a real on-disk pool. It is what CLAUDE.md rule 8 means by "do not skip a test":
the Python half is a deliverable, so it is tested as one.

Run it after `maturin develop`:

    maturin develop --release
    pytest bindings/python/tests
"""

import os
import tempfile

import flockdb


def _open(tmp):
    return flockdb.open(os.path.join(tmp, "pool"), "main")


def test_open_query_roundtrip():
    with tempfile.TemporaryDirectory() as tmp:
        db = _open(tmp)
        db.sql("CREATE TABLE t (id INTEGER, region TEXT)")
        db.sql("INSERT INTO t VALUES (1, 'EMEA'), (2, 'AMER'), (3, 'EMEA')")

        rows = db.sql("SELECT * FROM t ORDER BY id")
        assert len(rows) == 3
        assert rows.columns == ["id", "region"]  # a property, not a method (see #[getter])


def test_a_fork_is_isolated_from_its_parent():
    """The whole product in five lines: a fork diverges and the parent does not move.

    If this fails, nothing else about FlockDB matters, because this is the thing it is for.
    """
    with tempfile.TemporaryDirectory() as tmp:
        db = _open(tmp)
        db.sql("CREATE TABLE t (id INTEGER, region TEXT)")
        db.sql("INSERT INTO t VALUES (1, 'EMEA'), (2, 'AMER'), (3, 'EMEA')")

        fork = db.branch("what-if")
        fork.sql("DELETE FROM t WHERE region = 'EMEA'")

        assert len(fork.sql("SELECT * FROM t")) == 1, "the fork should have dropped the EMEA rows"
        assert len(db.sql("SELECT * FROM t")) == 3, "the parent must be untouched — separate databases"
        assert sorted(db.branches()) == ["main", "what-if"]


def test_snapshot_restore_round_trips():
    with tempfile.TemporaryDirectory() as tmp:
        db = _open(tmp)
        db.sql("CREATE TABLE t (id INTEGER)")
        db.sql("INSERT INTO t VALUES (1), (2)")

        snap = db.snapshot()
        assert isinstance(snap, str) and len(snap) == 64, "a snapshot id is a 32-byte hash in hex"

        db.sql("INSERT INTO t VALUES (3), (4)")
        assert len(db.sql("SELECT * FROM t")) == 4

        db.restore(snap)
        assert len(db.sql("SELECT * FROM t")) == 2, "restore must return to exactly the snapshot"


def test_export_duckdb_writes_a_file():
    """The escape hatch is a product promise; a Python user reaches it through this method."""
    with tempfile.TemporaryDirectory() as tmp:
        db = _open(tmp)
        db.sql("CREATE TABLE t (id INTEGER)")
        db.sql("INSERT INTO t VALUES (1), (2), (3)")

        out = os.path.join(tmp, "mine.duckdb")
        db.export_duckdb(out)
        assert os.path.getsize(out) > 0, "export_duckdb must write a non-empty standard .duckdb file"
