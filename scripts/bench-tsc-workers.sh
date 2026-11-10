#!/usr/bin/env bash
# Linux-only benchmark. Python 3 handles /proc sampling and summary statistics.
set -euo pipefail
exec python3 - "$@" <<'PY'
import argparse
import json
import os
from pathlib import Path
import re
import resource
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time


def positive(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return number


def arguments():
    parser = argparse.ArgumentParser(prog="bench-tsc-workers.sh", description="""Linux-only tsc/tsrs benchmark (requires Python 3.9+ and luchta on PATH).
Runs clean + uncached compile, then clean + cache removal + unmeasured populate,
then measured cache-hit compile, independently for each worker and repetition.
Only the switch environment variable selects the worker; config files aren't edited.
WARNING: clean removes build outputs and populate deletes REPO/.luchta/cache.
Use a disposable worktree with no other builds running. Tasks themselves may write
files; tracked changes are checked before and after, never silently restored.
Inherited cache overrides and max weight are cleared; shared cache is disabled.
Omitting --max-weight uses luchta's config/default concurrency.
""", formatter_class=argparse.RawDescriptionHelpFormatter, epilog="""Examples:
  scripts/bench-tsc-workers.sh --repo /path/to/worktree --runs 3 --switch-env LOCAL_TSC=tsrs
  scripts/bench-tsc-workers.sh --repo /path/to/worktree --runs 1 --workers tsc --package @formative/logger

--package applies to clean and compile; dependency tasks still run. 'cold' means
luchta --no-cache, not a cold OS page cache. Warm verification requires ALL runnable
tasks (including dependencies) to hit cache. Logs and raw JSON stay outside repo.
RSS peaks are sampled, so short-lived processes/spikes can be missed (N/A if no
worker was observed). Tree RSS sums resident pages, including shared pages per
process. Tree CPU uses child user+system time / wall time (100% = one CPU); host
CPU and 1-minute load include other workloads. Sampling target: 100 ms.
""")
    parser.add_argument("--repo", required=True, type=Path)
    parser.add_argument("--task", default="build:types")
    parser.add_argument("--runs", default=3, type=positive)
    parser.add_argument("--max-weight", type=positive)
    parser.add_argument("--switch-env", default="LOCAL_TSC=tsrs", metavar="NAME=VALUE")
    parser.add_argument("--workers", choices=("both", "tsc", "tsrs"), default="both")
    parser.add_argument("--package", action="append", default=[], help="package name/glob, repeatable")
    args = parser.parse_args()
    if not re.fullmatch(r"[A-Za-z_][A-Za-z_0-9]*=.+", args.switch_env):
        parser.error("--switch-env must be NAME=VALUE with a nonempty value")
    if args.task.startswith("-") or not args.task:
        parser.error("--task must be a task name")
    args.repo = args.repo.resolve(strict=True)
    return args


def tracked_status(repo):
    return subprocess.check_output(
        ["git", "-C", str(repo), "status", "--porcelain", "--untracked-files=no"]
    ).decode()


def worker_environment(args, worker):
    env = os.environ.copy()
    # Force one known local cache and prevent an inherited --no-cache equivalent.
    for name in ("LUCHTA_CACHE_DIR", "LUCHTA_NO_CACHE", "LUCHTA_SHARED_CACHE_DIR", "LUCHTA_MAX_WEIGHT"):
        env.pop(name, None)
    env.update(NO_COLOR="1", FORCE_COLOR="0", LUCHTA_SHARED_CACHE="off")
    name, value = args.switch_env.split("=", 1)
    env.pop(name, None)
    if worker == "tsrs":
        env[name] = value
    if args.max_weight is not None:
        env["LUCHTA_MAX_WEIGHT"] = str(args.max_weight)
    return env


def host_cpu():
    values = [int(value) for value in Path("/proc/stat").read_text().splitlines()[0].split()[1:]]
    # guest and guest_nice are already included in user and nice.
    return sum(values[:8]), sum(values[3:5])


def process_snapshot():
    processes = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            status = dict(line.split(":", 1) for line in (entry / "status").read_text().splitlines())
            rss = int(status.get("VmRSS", "0 kB").split()[0])
            try:
                executable = Path(os.readlink(entry / "exe")).name.removesuffix(" (deleted)")
            except OSError:
                executable = ""
            processes[int(entry.name)] = (int(status["PPid"]), rss, executable)
        except (OSError, ValueError, KeyError):
            # Processes can exit between directory enumeration and status reads.
            continue
    return processes


def tree_rss(root, executable):
    processes = process_snapshot()
    children = {}
    for pid, (parent, _, _) in processes.items():
        children.setdefault(parent, []).append(pid)
    pending, seen, total, worker_peak = [root], set(), 0, None
    while pending:
        pid = pending.pop()
        if pid in seen:
            continue
        seen.add(pid)
        _, rss, name = processes.get(pid, (0, 0, ""))
        total += rss
        if name == executable:
            worker_peak = max(worker_peak or 0, rss)
        pending.extend(children.get(pid, []))
    return total, worker_peak


def stop_process(process):
    # Workers create their own process groups, but inherit luchta's session.
    try:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=5)
    except (subprocess.TimeoutExpired, ProcessLookupError):
        pass
    finally:
        for entry in Path("/proc").iterdir():
            if entry.name.isdigit():
                try:
                    if os.getsid(int(entry.name)) == process.pid:
                        os.kill(int(entry.name), signal.SIGKILL)
                except ProcessLookupError:
                    pass
        process.wait()


def sample_run(command, repo, env, log, worker):
    before_cpu = host_cpu()
    before_usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    started = time.monotonic()
    samples, tree_peak, worker_peak, gap = [], 0, None, 0.0
    previous = started
    with log.open("wb") as output:
        process = subprocess.Popen(command, cwd=repo, env=env, stdout=output,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        try:
            while True:
                now = time.monotonic()
                gap = max(gap, now - previous)
                previous = now
                total, single = tree_rss(process.pid, f"luchta-{worker}-worker")
                tree_peak = max(tree_peak, total)
                if single is not None:
                    worker_peak = max(worker_peak or 0, single)
                samples.append(os.getloadavg()[0])
                try:
                    code = process.wait(timeout=max(0.001, 0.1 - (time.monotonic() - now)))
                    break
                except subprocess.TimeoutExpired:
                    pass
        except BaseException:
            stop_process(process)
            raise
    wall = time.monotonic() - started
    after_usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    cpu_seconds = ((after_usage.ru_utime + after_usage.ru_stime)
                   - (before_usage.ru_utime + before_usage.ru_stime))
    after_cpu = host_cpu()
    ticks = after_cpu[0] - before_cpu[0]
    idle = after_cpu[1] - before_cpu[1]
    return dict(wall_s=wall, worker_mib=None if worker_peak is None else worker_peak / 1024,
                tree_mib=tree_peak / 1024, tree_cpu_pct=100 * cpu_seconds / wall,
                host_cpu_pct=100 * (ticks - idle) / ticks if ticks else 0,
                load1_mean=statistics.mean(samples), load1_max=max(samples),
                sample_gap_ms=gap * 1000, exit_code=code)


def parse_summary(log):
    text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", log.read_text(errors="replace"))
    # progress.rs::render_summary: ✔ includes ran + local + shared hits;
    # ⏩ counts only local cache skips, 📥 shared restores, × failed tasks.
    lines = [line for line in text.splitlines() if re.search(r"✔\s+\d+.*⌚.*🌊", line)]
    if not lines:
        raise ValueError(f"missing luchta final summary in {log}")
    summary = lines[-1]
    def count(symbol):
        match = re.search(re.escape(symbol) + r"\s+(\d+)", summary)
        return int(match[1]) if match else 0
    return dict(completed=count("✔"), cache_hits=count("⏩") + count("📥"),
                local_hits=count("⏩"), shared_hits=count("📥"), failed=count("×"))


def run_phase(args, worker, env, directory, label, task, measured=False, extra=()):
    log = directory / f"{label}.log"
    filters = [value for package in args.package for value in ("--package", package)]
    command = ["luchta", "run", task, "--output", "summary", *filters, *extra]
    print(f"{worker}: {label} -> {log}", file=sys.stderr, flush=True)
    if measured:
        result = sample_run(command, args.repo, env, log, worker)
    else:
        with log.open("wb") as output:
            process = subprocess.Popen(command, cwd=args.repo, env=env, stdout=output,
                                       stderr=subprocess.STDOUT, start_new_session=True)
            try:
                result = {"exit_code": process.wait()}
            except BaseException:
                stop_process(process)
                raise
    result.update(log=str(log), worker=worker, phase=label)
    try:
        result.update(parse_summary(log))
    except ValueError as error:
        result.update(failed=None, completed=None, cache_hits=None, error=str(error))
    return result


def remove_cache(repo):
    parent = repo / ".luchta"
    cache = parent / "cache"
    if parent.is_symlink() or cache.is_symlink():
        raise ValueError(f"refusing to delete symlinked cache: {cache}")
    if cache.exists():
        shutil.rmtree(cache)


def successful(result):
    return result["exit_code"] == 0 and result["failed"] == 0


def benchmark_repetition(args, worker, env, directory, repetition, records):
    prefix = f"{worker}-{repetition}"
    for phase in ("cold", "warm"):
        clean = run_phase(args, worker, env, directory, f"{prefix}-{phase}-clean", "clean",
                          extra=("--continue",))
        if not successful(clean):
            raise ValueError(f"clean failed; see {clean['log']}")
        if phase == "warm":
            remove_cache(args.repo)
            populate = run_phase(args, worker, env, directory, f"{prefix}-populate", args.task)
            if not successful(populate):
                raise ValueError(f"cache populate failed; see {populate['log']}")
        result = run_phase(args, worker, env, directory, f"{prefix}-{phase}", args.task,
                           measured=True, extra=("--no-cache",) if phase == "cold" else ())
        result["kind"] = phase
        result["repetition"] = repetition
        if phase == "warm":
            result["verified"] = (successful(result) and result["completed"] > 0
                                  and result["completed"] == populate["completed"]
                                  and result["cache_hits"] == result["completed"])
            if not result["verified"]:
                result["error"] = "warm run wasn't all cache hits; inspect logs for unstable inputs or non-cacheable dependencies"
        records.append(result)


def spread(records, field, digits=2):
    values = [record[field] for record in records if record.get(field) is not None]
    if not values:
        return "N/A"
    summary = (statistics.median(values), min(values), max(values))
    return " / ".join(f"{value:g}" if digits == 0 else f"{value:.{digits}f}" for value in summary)


def print_table(records):
    print("\nValues: median / min / max. CPU 100% = one CPU. Cache hits include dependencies.")
    print("| Worker | Phase | n | Wall s | Worker RSS MiB | Tree RSS MiB | Tree CPU % | Host CPU % | Load1 mean | Load1 peak | Exit | Failed tasks | Cache hits | Completed | Warm verified |")
    print("|---|---|---:|---|---|---|---|---|---|---|---|---|---|---|---|")
    for worker in ("tsc", "tsrs"):
        for phase in ("cold", "warm"):
            group = [r for r in records if r["worker"] == worker and r["kind"] == phase]
            if not group:
                continue
            fields = ("wall_s", "worker_mib", "tree_mib", "tree_cpu_pct", "host_cpu_pct",
                      "load1_mean", "load1_max", "exit_code", "failed", "cache_hits", "completed")
            cells = [spread(group, field, 0 if field in fields[7:] else 2) for field in fields]
            verified = f"{sum(r.get('verified', False) for r in group)}/{len(group)}" if phase == "warm" else "—"
            print(f"| {worker} | {phase} | {len(group)} | " + " | ".join(cells) + f" | {verified} |")


def main():
    args = arguments()
    if sys.platform != "linux" or not Path("/proc/self/status").is_file():
        raise ValueError("Linux with mounted /proc is required")
    if not shutil.which("luchta"):
        raise ValueError("luchta not found on PATH")
    if tracked_status(args.repo):
        raise ValueError("target repo has tracked changes; use a clean disposable worktree")
    directory = Path(tempfile.mkdtemp(prefix="bench-tsc-workers-"))
    records, errors = [], []
    metadata = dict(repo=str(args.repo), task=args.task, runs=args.runs, packages=args.package,
                    switch_env=args.switch_env, max_weight=args.max_weight,
                    inherited_max_weight=os.environ.get("LUCHTA_MAX_WEIGHT"),
                    luchta=shutil.which("luchta"), cpu_count=os.cpu_count(),
                    affinity_cpus=len(os.sched_getaffinity(0)),
                    mem_total=Path("/proc/meminfo").read_text().splitlines()[0])
    print(f"Linux benchmark; logs: {directory}\nMachine: {metadata}", flush=True)
    workers = ("tsc", "tsrs") if args.workers == "both" else (args.workers,)
    try:
        for worker in workers:
            env = worker_environment(args, worker)
            for repetition in range(1, args.runs + 1):
                try:
                    benchmark_repetition(args, worker, env, directory, repetition, records)
                except (OSError, ValueError) as error:
                    errors.append(str(error))
    finally:
        status = tracked_status(args.repo)
        if status:
            errors.append(f"target repo has tracked changes after benchmark (not restored):\n{status}")
        errors.extend(r["error"] for r in records if "error" in r)
        if any(r["sample_gap_ms"] > 250 for r in records):
            errors.append("sampler fell behind 250 ms; RSS peaks may be undersampled")
        (directory / "results.json").write_text(json.dumps(
            dict(metadata=metadata, records=records, errors=errors), indent=2) + "\n")
        print_table(records)
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        print(f"\nRaw results: {directory / 'results.json'}", flush=True)
    return int(bool(errors) or any(not successful(r) for r in records))


def interrupted(signum, frame):
    raise KeyboardInterrupt


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGHUP, interrupted)
    try:
        sys.exit(main())
    except (OSError, ValueError) as error:
        sys.exit(f"ERROR: {error}")
    except KeyboardInterrupt:
        sys.exit(130)
PY
