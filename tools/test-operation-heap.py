#!/usr/bin/env python3
"""Budget enforcement controls; missing measurements must never pass CI."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("operation_heap", Path(__file__).with_name("check-operation-heap.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class BudgetTests(unittest.TestCase):
    def test_missing_or_invalid_counters_fail(self):
        for value in (None, {}, {"peak_above_baseline_bytes": None},
                      {"peak_above_baseline_bytes": True}, {"peak_above_baseline_bytes": -1}):
            with self.subTest(value=value), self.assertRaises(AssertionError):
                gate.checked_peak(value)

    def test_old_whole_payload_ownership_exceeds_cap(self):
        with self.assertRaisesRegex(AssertionError, "exceeds"):
            gate.checked_peak({"peak_above_baseline_bytes": 201466250})

    def test_measured_peak_with_platform_slack_passes(self):
        self.assertEqual(gate.checked_peak({"peak_above_baseline_bytes": 463595}), 463595)


if __name__ == "__main__":
    unittest.main()
