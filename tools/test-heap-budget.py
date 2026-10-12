#!/usr/bin/env python3
"""Check that the diagnostic budget driver rejects missing instrumentation and failed probes."""

import importlib.util
import json
from pathlib import Path
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


if __name__ == "__main__":
    unittest.main()
