#!/usr/bin/env python3
"""Regression checks for the embedded sampler: python3 -m unittest discover -s scripts/tests."""
import argparse
from contextlib import redirect_stderr
import io
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import types
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "bench-tsc-workers.sh"
BENCH = types.ModuleType("bench")
exec(compile(SCRIPT.read_text().split("<<'PY'\n", 1)[1].rsplit("\nPY", 1)[0], str(SCRIPT), "exec"),
     BENCH.__dict__)


class BenchmarkTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="bench-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.args = argparse.Namespace(repo=self.root, task="build:types", package=[],
                                       switch_env="LOCAL_TSC=tsrs", max_weight=None)

    def test_switch_is_unset_for_tsc_and_defaults_ignore_inherited_weight(self):
        with patch.dict(os.environ, {"LOCAL_TSC": "tsrs", "LUCHTA_MAX_WEIGHT": "5",
                                     "LUCHTA_NO_CACHE": "1", "LUCHTA_CACHE_DIR": "/wrong"}):
            env = BENCH.worker_environment(self.args, "tsc")
            for name in ("LOCAL_TSC", "LUCHTA_MAX_WEIGHT", "LUCHTA_NO_CACHE", "LUCHTA_CACHE_DIR"):
                self.assertNotIn(name, env)
            self.assertEqual(env["LUCHTA_SHARED_CACHE"], "off")
            self.args.max_weight = 7
            env = BENCH.worker_environment(self.args, "tsrs")
            self.assertEqual(env["LOCAL_TSC"], "tsrs")
            self.assertEqual(env["LUCHTA_MAX_WEIGHT"], "7")

    def test_summary_counts_local_shared_and_failed_not_periodic_progress(self):
        log = self.root / "run.log"
        log.write_text("✔ 99 ⏩ 99\n\x1b[32m✔ 6\x1b[0m ⏩ 2 × 1 (pkg#build) 📥 3 ⌚ 2s 🐏 10 MB 🌊 1 / 1\n")
        self.assertEqual(BENCH.parse_summary(log), dict(completed=6, cache_hits=5,
                                                      local_hits=2, shared_hits=3, failed=1))
        log.write_text("config failed\n")
        with self.assertRaisesRegex(ValueError, "missing luchta final summary"):
            BENCH.parse_summary(log)

    def test_rss_counts_descendants_only_and_max_single_worker_not_worker_sum(self):
        snapshot = {1: (0, 10, "luchta"), 2: (1, 20, "wrapper"),
                    3: (2, 100, "luchta-tsc-worker"), 4: (1, 200, "luchta-tsc-worker"),
                    5: (99, 9999, "luchta-tsc-worker")}
        with patch.object(BENCH, "process_snapshot", return_value=snapshot):
            self.assertEqual(BENCH.tree_rss(1, "luchta-tsc-worker"), (330, 200))
            self.assertEqual(BENCH.tree_rss(1, "luchta-tsrs-worker"), (330, None))

    def test_cache_removal_rejects_symlinks_and_leaves_other_files(self):
        cache = self.root / ".luchta" / "cache"
        cache.mkdir(parents=True)
        (cache / "old").write_text("old")
        keep = cache.parent / "keep"
        keep.write_text("keep")
        BENCH.remove_cache(self.root)
        self.assertFalse(cache.exists())
        self.assertEqual(keep.read_text(), "keep")
        cache.symlink_to(self.root, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "symlinked cache"):
            BENCH.remove_cache(self.root)

    def test_even_count_median_is_not_rounded_to_an_integer(self):
        self.assertEqual(BENCH.spread([{"failed": 0}, {"failed": 1}], "failed", 0), "0.5 / 0 / 1")

    def test_real_proc_sampler_returns_rss_and_cpu(self):
        result = BENCH.sample_run([sys.executable, "-c", "import time; time.sleep(0.35)"],
                                  self.root, os.environ, self.root / "sample.log", "tsc")
        self.assertEqual(result["exit_code"], 0)
        self.assertGreater(result["tree_mib"], 0)
        self.assertIsNone(result["worker_mib"])
        self.assertGreaterEqual(result["wall_s"], 0.35)
        self.assertGreaterEqual(result["tree_cpu_pct"], 0)

    def test_lifecycle_filters_worker_logs_and_warm_miss_detection(self):
        self.args.package = ["@formative/logger"]
        calls, records = [], []
        warm_hits = 2

        def fake_sample(command, repo, env, log, worker):
            calls.append(command)
            warm = log.stem.endswith("-warm")
            log.write_text(f"✔ 2 ⏩ {warm_hits if warm else 0} ⌚ 1s 🐏 1 MB 🌊 1 / 1\n")
            return dict(exit_code=0)

        class Unmeasured:
            pid = 99999

            def __init__(self, command, **kwargs):
                calls.append(command)
                kwargs["stdout"].write("✔ 2 ⌚ 1s 🐏 1 MB 🌊 1 / 1\n".encode())

            def wait(self):
                return 0

        with patch.object(BENCH, "sample_run", side_effect=fake_sample), \
             patch.object(BENCH.subprocess, "Popen", Unmeasured), \
             patch.object(BENCH, "remove_cache") as remove, redirect_stderr(io.StringIO()):
            for worker in ("tsc", "tsrs"):
                BENCH.benchmark_repetition(self.args, worker, {}, self.root, 1, records)
            self.assertEqual(remove.call_count, 2)
            self.assertEqual(len({record["log"] for record in records}), 4)
            self.assertTrue(all(record["verified"] for record in records if record["kind"] == "warm"))
            self.assertEqual(calls[0], ["luchta", "run", "clean", "--output", "summary",
                                        "--package", "@formative/logger", "--continue"])
            self.assertIn("--no-cache", calls[1])
            self.assertNotIn("--no-cache", calls[3])
            self.assertNotIn("--no-cache", calls[4])
            warm_hits = 1
            BENCH.benchmark_repetition(self.args, "tsc", {}, self.root, 2, records)
            self.assertFalse(records[-1]["verified"])
            self.assertIn("wasn't all cache hits", records[-1]["error"])

    def test_stop_process_cleans_workers_in_separate_process_groups(self):
        child_pid_file = self.root / "worker.pid"
        source = ("import os, subprocess, sys, time; "
                  "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'], "
                  "preexec_fn=os.setpgrp); "
                  "open(sys.argv[1], 'w').write(str(child.pid)); time.sleep(60)")
        process = subprocess.Popen([sys.executable, "-c", source, str(child_pid_file)], start_new_session=True)
        try:
            self.wait_until(lambda: child_pid_file.exists() and child_pid_file.read_text())
            status = Path(f"/proc/{int(child_pid_file.read_text())}/status")
            BENCH.stop_process(process)
            self.assertIsNotNone(process.returncode)
            self.wait_until(lambda: not status.exists() or "State:\tZ" in status.read_text())
        finally:
            BENCH.stop_process(process)

    def wait_until(self, predicate):
        for _ in range(100):
            if predicate():
                return
            time.sleep(0.01)
        self.fail("timed out waiting for subprocess state")


if __name__ == "__main__":
    unittest.main()
