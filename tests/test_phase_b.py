"""Phase B unit tests — durable queue ordering, content, mocked lp."""

from __future__ import annotations

import base64
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import jobs
from jobs import JobError, JobStore, PrintJob, drain_queue, process_job


def _store(td: str) -> JobStore:
    s = JobStore(queue_dir=Path(td) / "queue", processed_dir=Path(td) / "processed")
    s.ensure()
    return s


def _png_job(job_id: str = "job-1", cups: str = "TestPrinter") -> PrintJob:
    # 1x1 PNG
    png = base64.b64decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
    )
    return PrintJob(
        id=job_id,
        cups_name=cups,
        content_type="png_base64",
        content=base64.b64encode(png).decode("ascii"),
        title="unit test",
        options={"copies": 1},
    )


class TestDurableQueueOrdering(unittest.TestCase):
    def test_write_queue_before_ack_before_print(self):
        """Critical order: durable write → ack → lp → processed → delete queue."""
        with tempfile.TemporaryDirectory() as td:
            store = _store(td)
            job = _png_job()
            events: list[str] = []

            def ack(j: PrintJob) -> None:
                # Queue file must already exist when ack runs.
                self.assertTrue(store.has_queue_file(j.id))
                events.append("ack")

            def lp(cups, path, *, title=None, copies=1, raw=False):
                self.assertTrue(store.has_queue_file(job.id))
                self.assertIn("ack", events)
                self.assertFalse(store.is_processed(job.id))
                events.append("lp")
                self.assertTrue(Path(path).is_file())
                self.assertFalse(raw)

            def state(j, st, detail=None):
                events.append(f"state:{st}")

            result = process_job(job, store, lp=lp, ack=ack, report_state=state)
            self.assertIn(result, ("delivered", "printed"))
            self.assertEqual(events[0], "ack")
            self.assertIn("lp", events)
            self.assertTrue(store.is_processed(job.id))
            self.assertFalse(store.has_queue_file(job.id))
            # ack before lp
            self.assertLess(events.index("ack"), events.index("lp"))

    def test_already_processed_skips_print(self):
        with tempfile.TemporaryDirectory() as td:
            store = _store(td)
            job = _png_job()
            store.mark_processed(job.id)
            lp = mock.Mock()
            ack = mock.Mock()
            result = process_job(job, store, lp=lp, ack=ack)
            self.assertIn(result, ("delivered", "printed"))
            lp.assert_not_called()
            ack.assert_not_called()

    def test_failed_print_keeps_queue_file(self):
        """On lp failure, queue file remains for retry (no processed marker)."""
        with tempfile.TemporaryDirectory() as td:
            store = _store(td)
            job = _png_job()

            def lp_fail(*a, **k):
                raise JobError("printer offline", code="lp_error")

            with self.assertRaises(JobError):
                process_job(job, store, lp=lp_fail)
            self.assertTrue(store.has_queue_file(job.id))
            self.assertFalse(store.is_processed(job.id))

    def test_queue_fsync_write_readable(self):
        with tempfile.TemporaryDirectory() as td:
            store = _store(td)
            job = _png_job("job-fsync")
            path = store.write_queue(job)
            data = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(data["id"], "job-fsync")
            self.assertEqual(data["cups_name"], "TestPrinter")
            loaded = store.load_queued("job-fsync")
            self.assertEqual(loaded.content_type, "png_base64")


class TestDrain(unittest.TestCase):
    def test_drain_recovers_queued_jobs(self):
        with tempfile.TemporaryDirectory() as td:
            store = _store(td)
            j1 = _png_job("q1")
            j2 = _png_job("q2")
            store.write_queue(j1)
            store.write_queue(j2)
            printed: list[str] = []

            def lp(cups, path, *, title=None, copies=1, raw=False):
                printed.append(path.name)

            results = drain_queue(store, lp=lp)
            self.assertEqual(len(results), 2)
            self.assertTrue(all(r[1] in ("delivered", "printed") for r in results))
            self.assertEqual(len(printed), 2)
            self.assertEqual(store.list_queued_ids(), [])
            self.assertTrue(store.is_processed("q1"))
            self.assertTrue(store.is_processed("q2"))

    def test_drain_skips_already_processed(self):
        with tempfile.TemporaryDirectory() as td:
            store = _store(td)
            job = _png_job("done-already")
            store.write_queue(job)
            store.mark_processed(job.id)
            lp = mock.Mock()
            results = drain_queue(store, lp=lp)
            self.assertEqual(results, [("done-already", "printed")])
            lp.assert_not_called()
            self.assertFalse(store.has_queue_file(job.id))


class TestContent(unittest.TestCase):
    def test_pdf_base64(self):
        # minimal PDF
        pdf = b"%PDF-1.1\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF\n"
        job = PrintJob(
            id="pdf1",
            cups_name="P",
            content_type="pdf_base64",
            content=base64.b64encode(pdf).decode(),
        )
        with tempfile.TemporaryDirectory() as td:
            path, is_temp = jobs.materialize_content(job, work_dir=Path(td))
            self.assertTrue(is_temp)
            self.assertEqual(path.read_bytes(), pdf)
            path.unlink()

    def test_png_uri_fetch(self):
        png = b"\x89PNG\r\n\x1a\nfake"
        job = PrintJob(
            id="uri1",
            cups_name="P",
            content_type="png_uri",
            content="https://example.test/label.png",
        )
        with tempfile.TemporaryDirectory() as td:
            path, is_temp = jobs.materialize_content(
                job, work_dir=Path(td), fetch_url=lambda u: png
            )
            self.assertTrue(is_temp)
            self.assertEqual(path.read_bytes(), png)

    def test_local_path(self):
        with tempfile.TemporaryDirectory() as td:
            f = Path(td) / "x.jpg"
            f.write_bytes(b"jpeg-bytes")
            job = jobs.job_from_local_file(f, "Q1")
            path, is_temp = jobs.materialize_content(job)
            self.assertFalse(is_temp)
            self.assertEqual(path, f.resolve())

    def test_raw_base64_writes_zpl_without_pdf_sniff(self):
        zpl = b"^XA^FO50,50^FDHello^FS^XZ"
        job = PrintJob(
            id="raw1",
            cups_name="P",
            content_type="raw_base64",
            content=base64.b64encode(zpl).decode(),
        )
        with tempfile.TemporaryDirectory() as td:
            path, is_temp = jobs.materialize_content(job, work_dir=Path(td))
            self.assertTrue(is_temp)
            self.assertEqual(path.suffix, ".zpl")
            self.assertEqual(path.read_bytes(), zpl)

    def test_raw_uri_writes_raw_suffix_for_non_zpl(self):
        payload = b"\x1b@EPL2-not-zpl"
        job = PrintJob(
            id="raw2",
            cups_name="P",
            content_type="raw_uri",
            content="https://example.test/label.bin",
        )
        with tempfile.TemporaryDirectory() as td:
            path, is_temp = jobs.materialize_content(
                job, work_dir=Path(td), fetch_url=lambda u: payload
            )
            self.assertTrue(is_temp)
            self.assertEqual(path.suffix, ".raw")
            self.assertEqual(path.read_bytes(), payload)

    def test_raw_does_not_sniff_as_pdf(self):
        """A raw payload that happens to start like PDF must stay raw."""
        weird = b"%PDF-lookalike-but-raw"
        job = PrintJob(
            id="raw-pdfish",
            cups_name="P",
            content_type="raw_base64",
            content=base64.b64encode(weird).decode(),
        )
        with tempfile.TemporaryDirectory() as td:
            path, is_temp = jobs.materialize_content(job, work_dir=Path(td))
            self.assertEqual(path.suffix, ".raw")
            self.assertEqual(path.read_bytes(), weird)

    def test_sniff_overrides_wrong_pdf_label_for_png(self):
        """Server labeled pdf_* but payload is PNG — CUPS needs .png extension."""
        png = base64.b64decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
        )
        job = PrintJob(
            id="mislabel",
            cups_name="P",
            content_type="pdf_base64",
            content=base64.b64encode(png).decode(),
        )
        with tempfile.TemporaryDirectory() as td:
            path, is_temp = jobs.materialize_content(job, work_dir=Path(td))
            self.assertTrue(is_temp)
            self.assertEqual(path.suffix, ".png")
            self.assertEqual(path.read_bytes(), png)


class TestRawLpPath(unittest.TestCase):
    def test_process_job_passes_raw_to_lp(self):
        zpl = b"^XA^FO50,50^A0N,30,30^FDTest^FS^XZ"
        job = PrintJob(
            id="raw-lp",
            cups_name="Zebra_Raw",
            content_type="raw_base64",
            content=base64.b64encode(zpl).decode(),
            options={"copies": 1},
        )
        with tempfile.TemporaryDirectory() as td:
            store = _store(td)
            seen: dict = {}

            def lp(cups, path, *, title=None, copies=1, raw=False):
                seen["cups"] = cups
                seen["path"] = Path(path)
                seen["raw"] = raw
                seen["bytes"] = Path(path).read_bytes()
                return "Zebra_Raw-9"

            with mock.patch("jobs.wait_cups_job", return_value="printed"):
                result = process_job(job, store, lp=lp)
            self.assertEqual(result, "printed")
            self.assertTrue(seen["raw"])
            self.assertEqual(seen["cups"], "Zebra_Raw")
            self.assertEqual(seen["bytes"], zpl)
            self.assertEqual(seen["path"].suffix, ".zpl")

    def test_default_lp_includes_o_raw(self):
        with tempfile.TemporaryDirectory() as td:
            f = Path(td) / "label.zpl"
            f.write_bytes(b"^XA^XZ")
            with mock.patch("jobs.subprocess.run") as run:
                run.return_value = mock.Mock(
                    returncode=0,
                    stdout="request id is Zebra-1 (1 file(s))\n",
                    stderr="",
                )
                rid = jobs.default_lp("Zebra", f, title="t", copies=1, raw=True)
            self.assertEqual(rid, "Zebra-1")
            cmd = run.call_args[0][0]
            self.assertEqual(cmd[:4], ["lp", "-d", "Zebra", "-t"])
            self.assertIn("-o", cmd)
            self.assertEqual(cmd[cmd.index("-o") + 1], "raw")
            self.assertEqual(cmd[-1], str(f))

    def test_local_path_options_raw(self):
        with tempfile.TemporaryDirectory() as td:
            f = Path(td) / "x.zpl"
            f.write_bytes(b"^XA^XZ")
            job = jobs.job_from_local_file(f, "Q", raw=True)
            self.assertTrue(jobs.is_raw_job(job))
            store = _store(td)
            flags: list[bool] = []

            def lp(cups, path, *, title=None, copies=1, raw=False):
                flags.append(raw)

            process_job(job, store, lp=lp)
            self.assertEqual(flags, [True])


class TestReceiveIdempotent(unittest.TestCase):
    def test_double_receive_prints_once(self):
        with tempfile.TemporaryDirectory() as td:
            store = _store(td)
            job = _png_job("once")
            lp = mock.Mock()
            process_job(job, store, lp=lp)
            process_job(job, store, lp=lp)
            self.assertEqual(lp.call_count, 1)


if __name__ == "__main__":
    unittest.main()
