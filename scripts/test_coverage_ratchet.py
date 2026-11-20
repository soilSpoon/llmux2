import unittest

import coverage_ratchet


class RatchetTest(unittest.TestCase):
    def test_measured_at_or_above_floor_passes(self):
        self.assertEqual(coverage_ratchet.check(base_floor=90.0, floor=90.0, measured=90.0), [])

    def test_measured_below_floor_fails(self):
        errors = coverage_ratchet.check(base_floor=90.0, floor=90.0, measured=89.99)
        self.assertEqual(len(errors), 1)
        self.assertIn("89.99", errors[0])

    def test_lowering_the_floor_fails(self):
        errors = coverage_ratchet.check(base_floor=90.0, floor=89.0, measured=95.0)
        self.assertEqual(len(errors), 1)
        self.assertIn("lowered", errors[0])

    def test_raising_the_floor_passes(self):
        self.assertEqual(coverage_ratchet.check(base_floor=90.0, floor=91.0, measured=91.5), [])

    def test_new_floor_without_base_passes(self):
        self.assertEqual(coverage_ratchet.check(base_floor=None, floor=90.0, measured=90.5), [])

    def test_line_percent_is_read_from_llvm_cov_json(self):
        report = {"data": [{"totals": {"lines": {"count": 200, "covered": 187, "percent": 93.5}}}]}
        self.assertEqual(coverage_ratchet.line_percent(report), 93.5)

    def test_floor_file_parses_a_single_number(self):
        self.assertEqual(coverage_ratchet.parse_floor("93.5\n"), 93.5)
        with self.assertRaises(ValueError):
            coverage_ratchet.parse_floor("")


if __name__ == "__main__":
    unittest.main()
