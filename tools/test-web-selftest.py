#!/usr/bin/env python3
"""Refute empty/malformed green browser reports using a task-local Playwright stub."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

RUNNER = Path(__file__).with_name("web-selftest.mjs").resolve()


class BrowserReportTests(unittest.TestCase):
    def test_browser_report_requires_real_consistent_checks(self):
        with tempfile.TemporaryDirectory(prefix="cv-browser-report-") as directory:
            module = Path(directory) / "playwright"
            module.mkdir()
            (module / "index.js").write_text("""
module.exports = { chromium: { launch: async () => ({
  newPage: async () => ({ on() {}, async goto() {}, async waitForFunction() {},
    async evaluate() { return JSON.parse(process.env.CV_TEST_REPORT); } }),
  async close() {}
}) } };
""")
            for report, expected in [({"total": 2, "failed": 0, "failures": []}, 0),
                                     ({"total": 2, "failed": 1, "failures": ["broken"]}, 1),
                                     ({"total": 0, "failed": 0, "failures": []}, 1),
                                     ({"total": 2, "failed": "0", "failures": []}, 1),
                                     ({"total": 2, "failed": 0, "failures": ["broken"]}, 1),
                                     ({"total": 2, "failed": -1, "failures": []}, 1), ({}, 1)]:
                with self.subTest(report=report):
                    result = subprocess.run(["node", str(RUNNER)], capture_output=True, text=True, timeout=10,
                                            env=dict(os.environ, NODE_PATH=directory, CV_TEST_REPORT=json.dumps(report)))
                    self.assertEqual(result.returncode, expected, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
