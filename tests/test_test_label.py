"""Sample 4×6 Road Runner label + LCD test-print submit path."""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import test_label


class Code128Tests(unittest.TestCase):
    def test_checksum_and_stop(self):
        vals = test_label.code128_values("A")
        # Start B (104), 'A' (33), checksum, stop (106)
        self.assertEqual(vals[0], 104)
        self.assertEqual(vals[1], 33)
        self.assertEqual(vals[-1], 106)
        self.assertEqual(vals[-2], (104 + 1 * 33) % 103)

    def test_tracking_encodes(self):
        vals = test_label.code128_values(test_label.TRACKING)
        self.assertGreater(len(vals), 4)
        self.assertEqual(vals[-1], 106)


class WriteLabelTests(unittest.TestCase):
    def test_write_pdf_and_zpl(self):
        with tempfile.TemporaryDirectory() as td:
            pdf = Path(td) / "l.pdf"
            zpl = Path(td) / "l.zpl"
            test_label.write_pdf(pdf)
            test_label.write_zpl(zpl)
            self.assertGreater(pdf.stat().st_size, 1000)
            text = zpl.read_text(encoding="ascii")
            self.assertTrue(text.startswith("^XA"))
            self.assertIn("^XZ", text)
            self.assertIn(test_label.RECIPIENT, text)
            self.assertIn(test_label.TRACKING, text)
            self.assertIn("^BCN", text)
            self.assertIn("^GFA,", text)
            self.assertIn("^PW812", text)
            self.assertIn("^LL1218", text)

    def test_render_is_4x6_at_203dpi(self):
        img = test_label.render_pdf_image()
        self.assertEqual(img.size, (812, 1218))


class SubmitTestLabelTests(unittest.TestCase):
    def test_pdf_not_raw_zpl_is_raw(self):
        seen: list[tuple[str, bool]] = []

        def lp(cups, path, *, title=None, copies=1, raw=False):
            seen.append((Path(path).suffix.lower(), raw))
            return "Q-1"

        with mock.patch.object(test_label, "ensure_test_labels") as ens:
            with mock.patch("zpl.should_convert_to_zpl", return_value=False):
                with tempfile.TemporaryDirectory() as td:
                    pdf = Path(td) / "vesyl-roadrunner-4x6.pdf"
                    zpl = Path(td) / "vesyl-roadrunner-4x6.zpl"
                    pdf.write_bytes(b"%PDF-1.4 test")
                    zpl.write_text("^XA^FO0,0^FDX^FS^XZ\n", encoding="ascii")
                    ens.return_value = (pdf, zpl)
                    test_label.submit_test_label(
                        "Office", "pdf", lp=lp, wait_cups=False
                    )
                    test_label.submit_test_label(
                        "Zebra", "zpl", lp=lp, wait_cups=False
                    )

        self.assertEqual(seen[0][0], ".pdf")
        self.assertFalse(seen[0][1])
        self.assertEqual(seen[1][0], ".zpl")
        self.assertTrue(seen[1][1])

    def test_rejects_unknown_format(self):
        from jobs import JobError

        with self.assertRaises(JobError):
            test_label.submit_test_label("Q", "epl")


if __name__ == "__main__":
    unittest.main()
