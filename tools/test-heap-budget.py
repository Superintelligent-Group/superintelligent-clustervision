#!/usr/bin/env python3
"""Check that the diagnostic budget driver rejects missing instrumentation and failed probes."""

import importlib.util
import json
from pathlib import Path
import sys
import subprocess
import sqlite3
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("heap_budget", Path(__file__).with_name("check-heap-budget.py"))
budget = importlib.util.module_from_spec(spec)
spec.loader.exec_module(budget)


class DriverTests(unittest.TestCase):
    def result(self, features, heap):
        return SimpleNamespace(returncode=0, stdout=json.dumps({"build_features": features, "requested_rust_heap": heap}), stderr="")

    def test_clean_timing_binary_is_rejected(self):
        with patch.object(budget.subprocess, "run", return_value=self.result({"mmap": True, "probe_heap": False}, None)):
            with self.assertRaisesRegex(AssertionError, "instrumentation"):
                budget.probe("unused", [], {}, True)

    def test_missing_counters_are_rejected(self):
        with patch.object(budget.subprocess, "run", return_value=self.result({"mmap": True, "probe_heap": True}, None)):
            with self.assertRaisesRegex(AssertionError, "counters missing"):
                budget.probe("unused", [], {}, True)

    def test_failed_probe_is_rejected(self):
        with patch.object(budget.subprocess, "run", return_value=SimpleNamespace(returncode=1, stdout="", stderr="bad source")):
            with self.assertRaisesRegex(AssertionError, "probe failed.*bad source"):
                budget.probe("unused", [], {}, True)

    def test_size_dependent_growth_is_rejected(self):
        with self.assertRaisesRegex(AssertionError, "grows with source size"):
            budget.bound([budget.MIB, 4 * budget.MIB], 8 * budget.MIB, 2 * budget.MIB, "source")

    def test_facts_gate_rejects_changed_full_report(self):
        manifest = {"harness": "claude", "id": "fixture", "path": "source", "root": "root",
                    "expected_messages": 20, "expected_compactions": 4, "decoded_metadata_payload_bytes": 16,
                    "physical_input_bytes": 32}
        report = {"compactions": 4, "conversational": {"tool_results": 2}}
        facts = {"messages": 20, "detail": report}
        full = {"messages": 20, "detail": dict(report, compactions=0)}
        with patch.object(budget.subprocess, "check_output", return_value=json.dumps(manifest)), \
             patch.object(budget, "probe", side_effect=[(facts, 1024), (full, 4096)]):
            with self.assertRaisesRegex(AssertionError, "report changed"):
                budget.metadata_facts("unused", Path("unused"), {})

    def test_metadata_fixture_payload_budgets_and_native_cli_references(self):
        generator = Path(__file__).with_name("perf-fixture.py")
        with tempfile.TemporaryDirectory(prefix="cv-test-facts-") as directory:
            for harness in ("claude", "cursor"):
                for escaped in (False, True):
                    command = [sys.executable, str(generator), harness,
                               str(Path(directory) / f"{harness}-{escaped}"), "--shape", "metadata",
                               "--records", "3", "--payload-bytes", "257"]
                    if escaped:
                        command.append("--escaped")
                    manifest = json.loads(subprocess.check_output(command, text=True))
                    self.assertEqual(manifest["decoded_metadata_payload_bytes"], 257)
                    if harness == "claude":
                        rows = [json.loads(line) for line in Path(manifest["path"]).read_text().splitlines()]
                        self.assertEqual(len(rows), manifest["expected_messages"])
                        payload = sum(len(row.get("toolUseResult", {}).get("stdout", ""))
                                      + len(row.get("compactMetadata", {}).get("unusedPayload", ""))
                                      + len(row.get("attachment", {}).get("unusedPayload", "")) for row in rows)
                    else:
                        with sqlite3.connect(manifest["path"]) as db:
                            metadata = json.loads(bytes.fromhex(db.execute("SELECT value FROM meta WHERE key='0'").fetchone()[0]))
                            native_root = db.execute("SELECT data FROM blobs WHERE id=?", (metadata["latestRootBlobId"],)).fetchone()[0]
                            rows = []
                            for raw, in db.execute("SELECT data FROM blobs WHERE id!=?", (metadata["latestRootBlobId"],)):
                                rows.append(json.loads(raw))
                            self.assertEqual(len(rows), manifest["expected_messages"])
                            self.assertTrue(native_root.endswith(bytes([80, 1])))
                            payload = sum(len(row.get("providerOptions", {}).get("cursor", {}).get("ignoredPayload", ""))
                                          + len(row.get("providerOptions", {}).get("cursor", {}).get("highLevelToolCallResult", {}).get("output", ""))
                                          for row in rows)
                    self.assertEqual(payload, 257)

    def test_selected_gate_rejects_changed_serialized_record(self):
        manifest = {"harness": "cursor", "id": "fixture", "path": "source", "root": "root",
                    "expected_messages": 5}
        doctor = {"messages": 5, "detail": {"messages": 5}}
        full = {"detail": {"record_written": True, "serialized_record_bytes": 42, "record_sha256": "full"}}
        changed = {"detail": dict(full["detail"], record_sha256="changed")}
        with patch.object(budget.subprocess, "check_output", return_value=json.dumps(manifest)), \
             patch.object(budget, "probe", side_effect=[(doctor, 0), (full, 0), (doctor, 0), (doctor, 0), (changed, 0)]):
            with self.assertRaisesRegex(AssertionError, "dataset differs"):
                budget.selected_payloads("unused", Path("unused"), {}, ["cursor"])

    def test_selected_fixture_native_shapes_and_late_sentinel(self):
        generator = Path(__file__).with_name("perf-fixture.py")
        with tempfile.TemporaryDirectory(prefix="cv-test-selected-") as directory:
            for harness, shape in budget.SELECTED_PAYLOAD_CASES:
                command = [sys.executable, str(generator), "cursor" if harness == "cursor-ide" else harness,
                           str(Path(directory) / f"{harness}-{shape}"), "--shape", shape,
                           "--records", "2", "--payload-bytes", "257", "--escaped"]
                manifest = json.loads(subprocess.check_output(command, text=True))
                self.assertEqual(manifest["decoded_payload_bytes"], 257)
                self.assertEqual(manifest["selected_payload_bytes"], 257)
                self.assertGreater(manifest["physical_input_bytes"], 257)
                self.assertGreaterEqual(manifest["expected_messages"], 5)

    def sqlite_driver(self):
        # Import the actual command under Python -I without relying on ambient module paths.
        tools = Path(__file__).parent
        schema_spec = importlib.util.spec_from_file_location("perf_fixture_sqlite", tools / "perf_fixture_sqlite.py")
        schema = importlib.util.module_from_spec(schema_spec)
        schema_spec.loader.exec_module(schema)
        driver_spec = importlib.util.spec_from_file_location("sqlite_budget", tools / "perf-sqlite-check.py")
        driver = importlib.util.module_from_spec(driver_spec)
        with patch.dict(sys.modules, {"perf_fixture_sqlite": schema}):
            driver_spec.loader.exec_module(driver)
        return driver

    def test_sqlite_clean_binary_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "probe-heap"):
            self.sqlite_driver().checked_peak({"build_features": {"mmap": True, "probe_heap": False}})

    def test_sqlite_missing_counter_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "heap counters"):
            self.sqlite_driver().checked_peak({"build_features": {"mmap": True, "probe_heap": True}})


if __name__ == "__main__":
    unittest.main()
