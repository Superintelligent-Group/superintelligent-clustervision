#!/usr/bin/env python3
"""Offline WAL/source-isolation controls, also loaded by test_tools.py."""
import importlib.util
import json
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("schema_snapshot", HERE / "schema_snapshot.py")
SNAPSHOT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SNAPSHOT)


class SchemaSnapshotCase(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="cv-schema-test.")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def pending_wal(self):
        writer = sqlite3.connect(self.root / "writer.db")
        try:
            writer.execute("PRAGMA journal_mode=WAL")
            writer.execute("PRAGMA wal_autocheckpoint=0")
            writer.execute("CREATE TABLE baseline(id INTEGER)")
            writer.commit()
            writer.execute("PRAGMA wal_checkpoint(TRUNCATE)")
            writer.execute("CREATE TABLE recent(data TEXT, updated_at TEXT)")
            writer.execute("PRAGMA user_version=7")
            writer.execute("INSERT INTO recent VALUES(?, ?)",
                           (json.dumps({"role": "assistant", "content": "private-value"}), "2026-10-14T01:02:03Z"))
            writer.commit()
            # Freeze actual SQLite-written uncheckpointed state with no SHM. SQLite must
            # only rebuild auxiliary state in its owned scratch directory during inspection.
            source = self.root / "source ?#.db"
            shutil.copyfile(self.root / "writer.db", source)
            shutil.copyfile(self.root / "writer.db-wal", Path(str(source) + "-wal"))
        finally:
            writer.close()
        return source

    def test_wal_schema_rows_and_metadata_without_original_sidecars(self):
        source = self.pending_wal()
        files = SNAPSHOT.sqlite_files(source)
        before = [SNAPSHOT.fingerprint(path) for path in files]
        contents = [path.read_bytes() if path.exists() else None for path in files]
        result = subprocess.run([sys.executable, "-I", str(HERE / "schema_snapshot.py"),
                                 "sqlite", str(source), "--json-column", "recent.data"],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("PRAGMA user_version = 7", result.stdout)
        self.assertIn("CREATE TABLE recent", result.stdout)
        self.assertIn("merged over 1 rows", result.stdout)
        self.assertIn("$.role: str = assistant", result.stdout)
        self.assertNotIn("private-value", result.stdout)
        with SNAPSHOT.sqlite_snapshot(source) as database:
            self.assertEqual(database.execute("SELECT max(updated_at) FROM recent").fetchone()[0],
                             "2026-10-14T01:02:03Z")
        self.assertEqual([SNAPSHOT.fingerprint(path) for path in files], before)
        self.assertEqual([path.read_bytes() if path.exists() else None for path in files], contents)
        self.assertFalse(Path(str(source) + "-shm").exists())

    def test_changes_after_copy_are_rejected_before_successful_completion(self):
        source = self.pending_wal()
        with self.assertRaisesRegex(RuntimeError, "SQLite source changed"):
            with SNAPSHOT.sqlite_snapshot(source) as database:
                self.assertEqual(database.execute("SELECT count(*) FROM recent").fetchone()[0], 1)
                with Path(str(source) + "-wal").open("ab") as wal:
                    wal.write(b"changed after snapshot")
        self.assertFalse(Path(str(source) + "-shm").exists())

    def test_changes_during_copy_are_rejected_before_queries(self):
        source = self.pending_wal()
        copy = shutil.copyfile

        def changing_copy(original, target):
            result = copy(original, target)
            if str(original).endswith("-wal"):
                with source.open("ab") as database:
                    database.write(b"changed during copy")
            return result

        with mock.patch.object(SNAPSHOT.shutil, "copyfile", changing_copy):
            with self.assertRaisesRegex(RuntimeError, "SQLite source changed"):
                with SNAPSHOT.sqlite_snapshot(source):
                    self.fail("mixed copy accepted")
        self.assertFalse(Path(str(source) + "-shm").exists())

    def test_plain_database_and_missing_source(self):
        source = self.root / "plain.db"
        with sqlite3.connect(source) as database:
            database.execute("CREATE TABLE plain(id INTEGER)")
        with SNAPSHOT.sqlite_snapshot(source) as database:
            self.assertEqual(database.execute("SELECT count(*) FROM plain").fetchone()[0], 0)
        with self.assertRaises(FileNotFoundError):
            with SNAPSHOT.sqlite_snapshot(self.root / "missing.db"):
                self.fail("missing source accepted")
        self.assertFalse((self.root / "missing.db").exists())


if __name__ == "__main__":
    unittest.main()
