#!/usr/bin/env python3
"""Negative controls for the release upgrade gate; never opens a harness store."""
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("upgrade", Path(__file__).with_name("check-upgrade.py"))
upgrade = importlib.util.module_from_spec(spec)
spec.loader.exec_module(upgrade)


class UpgradeControls(unittest.TestCase):
    def test_unchanged_pass_requires_zero_and_no_unreadable(self):
        stable = "index: 0 changed/new (0 read from where the last pass stopped), 0 removed, 5 unchanged, 0 errors"
        for result, diagnostic, passes in [({"indexed": 5, "unreadable": []}, stable, True),
                ({"indexed": 5, "unreadable": []}, stable.replace("0 changed/new", "1 changed/new"), False),
                ({"indexed": 5, "unreadable": ["lost"]}, stable, False),
                ({"unreadable": []}, stable, False),
                ({"indexed": 5, "unreadable": []}, "missing work diagnostic", False)]:
            with self.subTest(result=result), patch.object(upgrade, "run", return_value=(json.dumps(result), diagnostic)):
                if passes:
                    self.assertEqual(upgrade.index(Path("cv"), {}, unchanged=True), 0)
                else:
                    with self.assertRaises(RuntimeError):
                        upgrade.index(Path("cv"), {}, unchanged=True)

    def test_new_store_is_explicit_but_candidate_cannot_omit_it(self):
        expected = {("cursor", "cli:one")}
        with patch.object(upgrade, "run", return_value="[]"):
            self.assertEqual(upgrade.rows(Path("old"), {}, expected, baseline=True), [])
            with self.assertRaises(RuntimeError):
                upgrade.rows(Path("new"), {}, expected)
        row = {"harness": "cursor", "id": "cli:one"}
        with patch.object(upgrade, "run", return_value=json.dumps([row, row])):
            with self.assertRaises(RuntimeError):
                upgrade.rows(Path("new"), {}, expected)

    def test_events_compare_meaning_not_allocated_row_ids(self):
        with tempfile.TemporaryDirectory(prefix="cv-upgrade-control-") as scratch:
            catalogs = [Path(scratch) / name for name in ("old", "new")]
            for catalog, key in zip(catalogs, [1, 99]):
                catalog.mkdir()
                with sqlite3.connect(catalog / "catalog.db") as connection:
                    connection.executescript("""
                        CREATE TABLE event_sessions(id,harness,session_id,parent_id,agent_id,workflow);
                        CREATE TABLE event_targets(id,target);
                        CREATE TABLE events(session,msg_idx,ts,kind,tool,target_id,detail);
                    """)
                    connection.execute("INSERT INTO event_sessions VALUES(?,'codex','s',NULL,NULL,NULL)", [key])
                    connection.execute("INSERT INTO event_targets VALUES(?,'/work/file')", [key])
                    connection.execute("INSERT INTO events VALUES(?,3,NULL,'file_edit','apply_patch',?,NULL)", [key, key])
            self.assertEqual(upgrade.event_digest(catalogs[0]), upgrade.event_digest(catalogs[1]))
            with sqlite3.connect(catalogs[1] / "catalog.db") as connection:
                connection.execute("UPDATE event_targets SET target='/different/file'")
            self.assertNotEqual(upgrade.event_digest(catalogs[0]), upgrade.event_digest(catalogs[1]))


if __name__ == "__main__":
    unittest.main()
