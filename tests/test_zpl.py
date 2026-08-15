"""ZPL graphic encoding + conversion decision tests."""

from __future__ import annotations

import io
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import zpl as zpl_mod


def _tiny_png() -> Path:
    from PIL import Image

    td = Path(tempfile.mkdtemp(prefix="vesyl-zpl-t-"))
    p = td / "dot.png"
    img = Image.new("L", (8, 2), 255)
    # black first pixel
    img.putpixel((0, 0), 0)
    img.save(p)
    return p


class TestGfaEncode(unittest.TestCase):
    def test_mono_hex_black_msb(self):
        from PIL import Image

        img = Image.new("1", (8, 1), 1)  # white
        img.putpixel((0, 0), 0)  # black leftmost
        hex_data, total, row_bytes, height = zpl_mod.mono_image_to_gfa_hex(img)
        self.assertEqual(row_bytes, 1)
        self.assertEqual(total, 1)
        self.assertEqual(height, 1)
        self.assertEqual(hex_data, "80")  # MSB set

    def test_build_label_contains_gfa(self):
        zpl = zpl_mod.build_zpl_label("80", total_bytes=1, bytes_per_row=1)
        self.assertTrue(zpl.startswith("^XA"))
        self.assertIn("^GFA,1,1,1,80", zpl)
        self.assertTrue(zpl.strip().endswith("^XZ"))

    def test_png_to_zpl_file(self):
        png = _tiny_png()
        try:
            dest = png.parent
            out = zpl_mod.write_zpl_file(png, dest, "job1")
            text = out.read_text(encoding="ascii")
            self.assertIn("^GFA,", text)
            self.assertIn("^XA", text)
        finally:
            for p in png.parent.iterdir():
                p.unlink(missing_ok=True)
            png.parent.rmdir()


class TestShouldConvert(unittest.TestCase):
    def test_force_raw_graphic(self):
        self.assertTrue(
            zpl_mod.should_convert_to_zpl(
                Path("label.pdf"),
                cups_name="OfficeJet",
                force_raw=True,
            )
        )

    def test_opt_out(self):
        self.assertFalse(
            zpl_mod.should_convert_to_zpl(
                Path("label.pdf"),
                cups_name="Zebra_ZD220",
                job_options={"no_zpl_convert": True},
            )
        )

    def test_zpl_name_heuristic(self):
        with mock.patch(
            "printers.queue_supports_raw", side_effect=RuntimeError("no cups")
        ):
            self.assertTrue(
                zpl_mod.should_convert_to_zpl(
                    Path("x.png"), cups_name="Zebra_ZD220-203dpi_ZPL"
                )
            )
            self.assertFalse(
                zpl_mod.should_convert_to_zpl(Path("x.png"), cups_name="Brother")
            )

    def test_non_graphic(self):
        self.assertFalse(
            zpl_mod.should_convert_to_zpl(
                Path("label.zpl"), cups_name="Zebra_ZD220", force_raw=True
            )
        )


class TestProcessJobConverts(unittest.TestCase):
    def test_pdf_like_png_to_zebra_queue(self):
        import jobs
        from jobs import JobStore, PrintJob

        png = _tiny_png()
        try:
            with tempfile.TemporaryDirectory() as td:
                store = JobStore(
                    queue_dir=Path(td) / "q", processed_dir=Path(td) / "p"
                )
                submitted: list[tuple] = []

                def fake_lp(queue, path, **kw):
                    submitted.append((queue, Path(path).read_text(), kw))
                    return "Zebra_ZD220-1"

                job = PrintJob(
                    id="j1",
                    cups_name="Zebra_ZD220-203dpi_ZPL",
                    content_type="local_path",
                    content=str(png),
                    options={"copies": 1},
                )
                with mock.patch("jobs.wait_cups_job", return_value="printed"), mock.patch(
                    "printers.queue_supports_raw", return_value=True
                ):
                    result = jobs.process_job(job, store, lp=fake_lp)
                self.assertEqual(result, "printed")
                self.assertEqual(len(submitted), 1)
                self.assertIn("^GFA,", submitted[0][1])
                self.assertTrue(submitted[0][2].get("raw"))
        finally:
            for p in png.parent.iterdir():
                p.unlink(missing_ok=True)
            png.parent.rmdir()


if __name__ == "__main__":
    unittest.main()
