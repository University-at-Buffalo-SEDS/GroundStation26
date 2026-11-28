#!/usr/bin/env python3
"""Over-range handling in plots.py.

Runs with `python3 -m unittest tools/loadcell_investigation/test_plots.py` from the repo root.
"""
import os
import sys
import unittest

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import plots  # noqa: E402


class OverRangeTests(unittest.TestCase):
    def test_code_limits(self):
        fs = plots.CODE_FS
        codes = [0, fs - 1, -(fs - 1), fs, -fs, 0xB40000, 0x0FFFFFF, -0x1000000, np.inf, -np.inf, np.nan]
        self.assertEqual(plots.over_range(codes).tolist(),
                         [False, False, False, True, True, True, True, True, True, True, False])

    def test_flag_marks_in_range_code(self):
        self.assertEqual(plots.over_range([1.0, 2.0, np.nan], flag=[0, 1, np.nan]).tolist(), [False, True, False])

    def test_legacy_plateau_is_0xb40000(self):
        raw = 0xB40000 * plots.LEGACY_PER_CODE
        self.assertAlmostEqual(float(plots.kg1000_code(raw)), 0xB40000, places=3)
        self.assertAlmostEqual(raw * plots.K, 3.375, places=9)
        self.assertTrue(plots.over_range(plots.kg1000_code([raw]))[0])

    def test_runs_and_spans(self):
        m = np.array([True, True, False, False, True, False, True])
        self.assertEqual(plots.runs(m), [(0, 2), (4, 5), (6, 7)])
        x = np.array([0.0, 1.0, 2.0, 3.0, 4.0, 100.0, 101.0])
        # The run before the 4 -> 100 gap stops one sample interval later; the last run likewise.
        self.assertEqual(plots.spans(x, m), [(0.0, 2.0), (4.0, 5.0), (101.0, 102.0)])

    def test_mcp_vref_rescales_volts_only(self):
        self.assertEqual(plots.mcp_v(2.4).item(), 2.4)  # default: as recorded
        old = plots.MCP_VREF
        try:
            plots.MCP_VREF = 2.2
            self.assertAlmostEqual(plots.mcp_v(3.375).item(), 3.09375)
        finally:
            plots.MCP_VREF = old

    def test_without_breaks_line(self):
        y = plots.without(np.array([1.0, 2.0, 3.0]), np.array([False, True, False]))
        self.assertTrue(np.isnan(y[1]))
        self.assertEqual(y[[0, 2]].tolist(), [1.0, 3.0])


if __name__ == '__main__':
    unittest.main()
