#!/usr/bin/env python3
"""Run a reproducible paired Pareto gate for the C read-stress benchmark."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import math
import os
import platform
import shlex
import statistics
import subprocess
import sys
import time
from functools import partial
from pathlib import Path
from typing import Any, Callable


ENGINES = ("sqlite", "csgdb-plaintext", "csgdb-encrypted")
HIGHER_IS_BETTER = ("reads_per_second",)
LOWER_IS_BETTER = ("p50_us", "p95_us", "p99_us", "max_rss_kib")


def positive_integer(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("value must be a positive integer")
    return parsed


def non_negative_integer(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("value must be a non-negative integer")
    return parsed


def balanced_round_count(value: str) -> int:
    parsed = positive_integer(value)
    if parsed < 4 or parsed % 2 != 0:
        raise argparse.ArgumentTypeError(
            "rounds must be an even integer of at least four for balanced AB/BA order"
        )
    return parsed


def finite_non_negative(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed) or parsed < 0.0:
        raise argparse.ArgumentTypeError("value must be finite and non-negative")
    return parsed


def cpu_set(value: str) -> frozenset[int]:
    cpus: set[int] = set()
    try:
        for component in value.split(","):
            component = component.strip()
            if not component:
                raise ValueError
            if "-" in component:
                first_text, last_text = component.split("-", 1)
                first = int(first_text)
                last = int(last_text)
                if first < 0 or last < first:
                    raise ValueError
                cpus.update(range(first, last + 1))
            else:
                cpu = int(component)
                if cpu < 0:
                    raise ValueError
                cpus.add(cpu)
    except ValueError as error:
        raise argparse.ArgumentTypeError(
            "CPU affinity must look like 2,4-7 and contain no negative CPUs"
        ) from error
    if not cpus:
        raise argparse.ArgumentTypeError("CPU affinity must not be empty")
    return frozenset(cpus)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Compare two sqlite_comparison binaries with alternating AB/BA "
            "read-only stress runs against the same existing database."
        )
    )
    parser.add_argument(
        "--baseline-binary", type=Path, required=True, help="frozen baseline binary"
    )
    parser.add_argument(
        "--candidate-binary", type=Path, required=True, help="candidate binary"
    )
    parser.add_argument("--engine", choices=ENGINES, required=True)
    parser.add_argument(
        "--database",
        "--existing-db",
        dest="database",
        type=Path,
        required=True,
        help="one existing database used by both binaries",
    )
    parser.add_argument("--rows", type=positive_integer, required=True)
    parser.add_argument("--seconds", type=positive_integer, default=10)
    parser.add_argument("--rounds", type=balanced_round_count, default=8)
    parser.add_argument(
        "--cpu-affinity",
        type=cpu_set,
        help="optional Linux CPU list, for example 2,4-7",
    )
    parser.add_argument(
        "--warmup",
        "--warmup-runs",
        dest="warmup",
        type=non_negative_integer,
        nargs="?",
        const=1,
        default=1,
        help="number of unscored AB/BA warm-up pairs (default: 1)",
    )
    parser.add_argument(
        "--timeout-seconds",
        type=positive_integer,
        help="per-process timeout (default: max(120, 2 * seconds + 60))",
    )
    parser.add_argument("--output", type=Path, help="also write the full JSON report")
    parser.add_argument(
        "--report-only",
        action="store_true",
        help="do not fail on metric regressions; benchmark errors still fail",
    )
    parser.add_argument(
        "--tolerance",
        "--tolerance-pct",
        "--allowed-regression-pct",
        dest="tolerance_pct",
        type=finite_non_negative,
        default=0.0,
        help="allowed median regression percentage (default: strict 0%%)",
    )
    return parser.parse_args()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def artifact_identity(path: Path) -> dict[str, Any]:
    details = path.stat()
    return {
        "path": os.fspath(path),
        "sha256": sha256_file(path),
        "size_bytes": details.st_size,
        "mtime_ns": details.st_mtime_ns,
    }


def engine_library_identity(
    binary: Path, engine: str, role: str
) -> dict[str, Any] | None:
    """Identify the sibling library selected by the benchmark's $ORIGIN RUNPATH."""
    library = binary.parent / "libcsgdb.so"
    if library.is_file():
        identity = artifact_identity(library.resolve())
        identity["resolution"] = "sibling-libcsgdb.so"
        return identity
    if engine != "sqlite":
        raise RuntimeError(
            f"{role} csgdb benchmark requires a sibling libcsgdb.so: {library}"
        )
    return None


def dynamic_linker_identity(binary: Path) -> dict[str, Any]:
    command = ["ldd", os.fspath(binary)]
    completed = subprocess.run(
        command,
        check=False,
        capture_output=True,
        text=True,
        timeout=30,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"ldd failed for {binary} with status {completed.returncode}: "
            f"{completed.stderr.strip()}"
        )
    entries: list[dict[str, Any]] = []
    missing: list[str] = []
    for raw_line in completed.stdout.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        soname: str | None = None
        resolved_path: Path | None = None
        if "=>" in line:
            name_text, target_text = line.split("=>", 1)
            soname = name_text.strip()
            target_parts = target_text.strip().split()
            if target_parts[:2] == ["not", "found"]:
                missing.append(soname)
            elif target_parts and target_parts[0].startswith("/"):
                resolved_path = Path(target_parts[0]).resolve()
        else:
            parts = line.split()
            if parts:
                soname = parts[0]
                if parts[0].startswith("/"):
                    resolved_path = Path(parts[0]).resolve()
        entry: dict[str, Any] = {"raw_line": raw_line, "soname": soname}
        if resolved_path is not None and resolved_path.is_file():
            entry["artifact"] = artifact_identity(resolved_path)
        else:
            entry["artifact"] = None
        entries.append(entry)
    if missing:
        raise RuntimeError(
            f"ldd reported missing dependencies for {binary}: " + ", ".join(missing)
        )
    return {
        "command": command,
        "command_shell": shlex.join(command),
        "returncode": completed.returncode,
        "raw_stdout": completed.stdout,
        "raw_stderr": completed.stderr,
        "entries": entries,
    }


def loaded_dso_artifacts(
    linker_identity: dict[str, Any], soname: str
) -> list[dict[str, Any]]:
    return [
        entry["artifact"]
        for entry in linker_identity["entries"]
        if entry.get("soname") == soname and entry.get("artifact") is not None
    ]


def verify_loaded_csgdb_library(
    role: str,
    engine: str,
    sibling_identity: dict[str, Any] | None,
    linker_identity: dict[str, Any],
) -> None:
    if engine == "sqlite":
        return
    if sibling_identity is None:
        raise RuntimeError(f"{role} CSGDB sibling library identity is missing")
    loaded = loaded_dso_artifacts(linker_identity, "libcsgdb.so")
    if len(loaded) != 1:
        raise RuntimeError(
            f"{role} benchmark must resolve exactly one libcsgdb.so; "
            f"found {len(loaded)}"
        )
    if (
        loaded[0]["path"] != sibling_identity["path"]
        or loaded[0]["sha256"] != sibling_identity["sha256"]
    ):
        raise RuntimeError(
            f"{role} benchmark does not load its audited sibling libcsgdb.so"
        )


def runtime_linker_environment() -> dict[str, str | None]:
    values = {
        "LD_LIBRARY_PATH": os.environ.get("LD_LIBRARY_PATH"),
        "LD_PRELOAD": os.environ.get("LD_PRELOAD"),
    }
    configured = [name for name, value in values.items() if value]
    if configured:
        raise RuntimeError(
            "dynamic-linker override variables must be unset or empty: "
            + ", ".join(configured)
        )
    return values


def database_artifacts(database: Path) -> dict[str, dict[str, Any] | None]:
    artifacts: dict[str, dict[str, Any] | None] = {}
    for label, path in (
        ("database", database),
        ("wal", Path(os.fspath(database) + "-wal")),
        ("shm", Path(os.fspath(database) + "-shm")),
    ):
        artifacts[label] = artifact_identity(path) if path.is_file() else None
    return artifacts


def artifact_contents_equal(
    before: dict[str, Any] | None, after: dict[str, Any] | None
) -> bool:
    if before is None or after is None:
        return before is None and after is None
    return (
        before["sha256"] == after["sha256"]
        and before["size_bytes"] == after["size_bytes"]
    )


def host_identity() -> dict[str, Any]:
    uname = platform.uname()
    affinity: list[int] | None = None
    if hasattr(os, "sched_getaffinity"):
        affinity = sorted(os.sched_getaffinity(0))
    return {
        "node": uname.node,
        "system": uname.system,
        "release": uname.release,
        "version": uname.version,
        "machine": uname.machine,
        "processor": uname.processor,
        "platform": platform.platform(),
        "python": platform.python_version(),
        "logical_cpu_count": os.cpu_count(),
        "process_cpu_affinity": affinity,
    }


def is_number(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def validate_result(
    result: dict[str, Any], engine: str, rows: int, seconds: int
) -> list[str]:
    issues: list[str] = []
    expected = {
        "mode": "read-stress",
        "engine": engine,
        "rows": rows,
        "requested_seconds": seconds,
    }
    for field, expected_value in expected.items():
        if result.get(field) != expected_value:
            issues.append(
                f"{field} expected {expected_value!r}, got {result.get(field)!r}"
            )

    for field in ("elapsed_seconds", "p50_us", "p95_us", "p99_us"):
        value = result.get(field)
        if not is_number(value) or not math.isfinite(float(value)) or value <= 0:
            issues.append(f"{field} must be finite and positive")
    for field in ("reads", "checksum", "max_rss_kib"):
        value = result.get(field)
        if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
            issues.append(f"{field} must be a positive integer")
    errors = result.get("errors")
    if not isinstance(errors, int) or isinstance(errors, bool) or errors < 0:
        issues.append("errors must be a non-negative integer")
    elif errors != 0:
        issues.append(f"benchmark reported {errors} read errors")
    if result.get("integrity_ok") is not True:
        issues.append("integrity_ok must be true")

    p50 = result.get("p50_us")
    p95 = result.get("p95_us")
    p99 = result.get("p99_us")
    if all(is_number(value) for value in (p50, p95, p99)) and not (
        p50 <= p95 <= p99
    ):
        issues.append("latency percentiles must satisfy p50 <= p95 <= p99")
    return issues


def parse_stdout(stdout: str) -> tuple[dict[str, Any] | None, list[str]]:
    lines = [line for line in stdout.splitlines() if line.strip()]
    if len(lines) != 1:
        return None, [
            "benchmark must emit exactly one non-empty JSON line; "
            f"received {len(lines)}"
        ]
    try:
        parsed = json.loads(lines[0])
    except json.JSONDecodeError as error:
        return None, [f"benchmark emitted invalid JSON: {error}"]
    if not isinstance(parsed, dict):
        return None, ["benchmark JSON must be an object"]
    return parsed, []


def run_one(
    role: str,
    binary: Path,
    binary_sha256: str,
    engine_library_identity_value: dict[str, Any] | None,
    engine: str,
    database: Path,
    rows: int,
    seconds: int,
    timeout_seconds: int,
    affinity: frozenset[int] | None,
    phase: str,
    pair_index: int,
    order_in_pair: int,
    invocation_index: int,
) -> tuple[dict[str, Any], list[str]]:
    command = [
        os.fspath(binary),
        "read-stress",
        engine,
        os.fspath(database),
        str(rows),
        str(seconds),
    ]
    preexec_fn: Callable[[], None] | None = None
    if affinity is not None:
        preexec_fn = partial(os.sched_setaffinity, 0, affinity)

    started_ns = time.monotonic_ns()
    try:
        completed = subprocess.run(
            command,
            check=False,
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
            preexec_fn=preexec_fn,
        )
        wall_seconds = (time.monotonic_ns() - started_ns) / 1_000_000_000.0
        record: dict[str, Any] = {
            "phase": phase,
            "pair_index": pair_index,
            "order_in_pair": order_in_pair,
            "invocation_index": invocation_index,
            "comparison_role": role,
            "benchmark_binary": os.fspath(binary),
            "binary_sha256": binary_sha256,
            "engine_library": engine_library_identity_value,
            "command": command,
            "command_shell": shlex.join(command),
            "returncode": completed.returncode,
            "wall_seconds": wall_seconds,
            "raw_stdout": completed.stdout,
            "raw_stderr": completed.stderr,
        }
        result, issues = parse_stdout(completed.stdout)
        if result is not None:
            record.update(result)
            issues.extend(validate_result(result, engine, rows, seconds))
            elapsed = result.get("elapsed_seconds")
            reads = result.get("reads")
            if (
                is_number(elapsed)
                and math.isfinite(float(elapsed))
                and elapsed > 0
                and isinstance(reads, int)
                and not isinstance(reads, bool)
                and reads > 0
            ):
                record["reads_per_second"] = reads / elapsed
        if completed.returncode != 0:
            issues.append(f"benchmark exited with status {completed.returncode}")
    except subprocess.TimeoutExpired as error:
        wall_seconds = (time.monotonic_ns() - started_ns) / 1_000_000_000.0
        stdout = error.stdout.decode() if isinstance(error.stdout, bytes) else error.stdout
        stderr = error.stderr.decode() if isinstance(error.stderr, bytes) else error.stderr
        record = {
            "phase": phase,
            "pair_index": pair_index,
            "order_in_pair": order_in_pair,
            "invocation_index": invocation_index,
            "comparison_role": role,
            "benchmark_binary": os.fspath(binary),
            "binary_sha256": binary_sha256,
            "engine_library": engine_library_identity_value,
            "command": command,
            "command_shell": shlex.join(command),
            "returncode": None,
            "wall_seconds": wall_seconds,
            "raw_stdout": stdout or "",
            "raw_stderr": stderr or "",
            "timed_out": True,
        }
        issues = [f"benchmark exceeded the {timeout_seconds}s timeout"]
    record["hard_failure_messages"] = list(dict.fromkeys(issues))
    return record, record["hard_failure_messages"]


def metric_summary(
    baseline: list[dict[str, Any]],
    candidate: list[dict[str, Any]],
    field: str,
    higher_is_better: bool,
    tolerance_pct: float,
) -> dict[str, Any]:
    baseline_values = [float(record[field]) for record in baseline]
    candidate_values = [float(record[field]) for record in candidate]
    baseline_median = float(statistics.median(baseline_values))
    candidate_median = float(statistics.median(candidate_values))
    raw_delta_pct = (candidate_median / baseline_median - 1.0) * 100.0
    improvement_pct = raw_delta_pct if higher_is_better else -raw_delta_pct

    wins = 0
    equal = 0
    regressions = 0
    within_tolerance = 0
    pairs: list[dict[str, Any]] = []
    for round_index, (baseline_value, candidate_value) in enumerate(
        zip(baseline_values, candidate_values), start=1
    ):
        if candidate_value == baseline_value:
            disposition = "equal"
            equal += 1
        elif (candidate_value > baseline_value) == higher_is_better:
            disposition = "candidate-win"
            wins += 1
        else:
            disposition = "candidate-regression"
            regressions += 1
        pair_raw_delta = (candidate_value / baseline_value - 1.0) * 100.0
        pair_improvement = pair_raw_delta if higher_is_better else -pair_raw_delta
        pair_pass = pair_improvement >= -tolerance_pct
        within_tolerance += int(pair_pass)
        pairs.append(
            {
                "round": round_index,
                "baseline": baseline_value,
                "candidate": candidate_value,
                "candidate_delta_pct": pair_raw_delta,
                "candidate_improvement_pct": pair_improvement,
                "disposition": disposition,
                "within_tolerance": pair_pass,
            }
        )

    return {
        "direction": "higher-is-better" if higher_is_better else "lower-is-better",
        "baseline_median": baseline_median,
        "candidate_median": candidate_median,
        "candidate_delta_pct": raw_delta_pct,
        "candidate_improvement_pct": improvement_pct,
        "tolerance_pct": tolerance_pct,
        "median_pass": improvement_pct >= -tolerance_pct,
        "paired_candidate_wins": wins,
        "paired_equal": equal,
        "paired_candidate_regressions": regressions,
        "paired_within_tolerance": within_tolerance,
        "pairs": pairs,
    }


def write_report(report: dict[str, Any], output: Path | None) -> None:
    encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
    sys.stdout.write(encoded)
    if output is not None:
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(encoded, encoding="utf-8")


def main() -> int:
    arguments = parse_args()
    linker_environment = runtime_linker_environment()
    baseline_binary = arguments.baseline_binary.resolve()
    candidate_binary = arguments.candidate_binary.resolve()
    database = arguments.database.resolve()
    for label, path in (
        ("baseline binary", baseline_binary),
        ("candidate binary", candidate_binary),
        ("database", database),
    ):
        if not path.is_file():
            raise RuntimeError(f"{label} does not exist or is not a file: {path}")
    if not os.access(baseline_binary, os.X_OK):
        raise RuntimeError(f"baseline binary is not executable: {baseline_binary}")
    if not os.access(candidate_binary, os.X_OK):
        raise RuntimeError(f"candidate binary is not executable: {candidate_binary}")
    if arguments.cpu_affinity is not None:
        if not hasattr(os, "sched_setaffinity"):
            raise RuntimeError("--cpu-affinity is not supported on this platform")
        available = (
            set(os.sched_getaffinity(0))
            if hasattr(os, "sched_getaffinity")
            else set(range(os.cpu_count() or 0))
        )
        unavailable = set(arguments.cpu_affinity) - available
        if unavailable:
            raise RuntimeError(
                "requested CPUs are outside this process's allowed affinity: "
                + ",".join(str(cpu) for cpu in sorted(unavailable))
            )

    baseline_identity = artifact_identity(baseline_binary)
    candidate_identity = artifact_identity(candidate_binary)
    baseline_library_identity = engine_library_identity(
        baseline_binary, arguments.engine, "baseline"
    )
    candidate_library_identity = engine_library_identity(
        candidate_binary, arguments.engine, "candidate"
    )
    baseline_linker_identity = dynamic_linker_identity(baseline_binary)
    candidate_linker_identity = (
        baseline_linker_identity
        if candidate_binary == baseline_binary
        else dynamic_linker_identity(candidate_binary)
    )
    verify_loaded_csgdb_library(
        "baseline",
        arguments.engine,
        baseline_library_identity,
        baseline_linker_identity,
    )
    verify_loaded_csgdb_library(
        "candidate",
        arguments.engine,
        candidate_library_identity,
        candidate_linker_identity,
    )
    baseline_library_sha = (
        baseline_library_identity["sha256"]
        if baseline_library_identity is not None
        else None
    )
    candidate_library_sha = (
        candidate_library_identity["sha256"]
        if candidate_library_identity is not None
        else None
    )
    if (
        baseline_identity["sha256"] == candidate_identity["sha256"]
        and baseline_library_sha == candidate_library_sha
    ):
        raise RuntimeError(
            "read-stress A/B requires a different benchmark binary or "
            "sibling library hash"
        )
    database_before = database_artifacts(database)
    timeout_seconds = arguments.timeout_seconds or max(
        120, arguments.seconds * 2 + 60
    )
    selected_affinity = (
        sorted(arguments.cpu_affinity)
        if arguments.cpu_affinity is not None
        else None
    )
    base_report: dict[str, Any] = {
        "schema_version": 1,
        "gate": "c-read-stress-pareto",
        "generated_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "invocation": [sys.executable, *sys.argv],
        "invocation_shell": shlex.join([sys.executable, *sys.argv]),
        "host": host_identity(),
        "dynamic_linker_environment": linker_environment,
        "artifacts": {
            "baseline_binary": baseline_identity,
            "candidate_binary": candidate_identity,
            "baseline_engine_library": baseline_library_identity,
            "candidate_engine_library": candidate_library_identity,
            "baseline_dynamic_linker": baseline_linker_identity,
            "candidate_dynamic_linker": candidate_linker_identity,
            "database_before": database_before,
        },
        "configuration": {
            "engine": arguments.engine,
            "database": os.fspath(database),
            "rows": arguments.rows,
            "seconds": arguments.seconds,
            "rounds": arguments.rounds,
            "warmup_pairs": arguments.warmup,
            "timeout_seconds": timeout_seconds,
            "cpu_affinity": selected_affinity,
            "tolerance_pct": arguments.tolerance_pct,
            "median_gate": "strict Pareto no-regression"
            if arguments.tolerance_pct == 0.0
            else "Pareto with configured tolerance",
            "schedule": "alternating baseline/candidate AB then BA within each pair",
        },
    }

    warmup_runs: list[dict[str, Any]] = []
    measured_runs: list[dict[str, Any]] = []
    hard_failures: list[str] = []
    invocation_index = 0

    def run_pairs(phase: str, pair_count: int, destination: list[dict[str, Any]]) -> None:
        nonlocal invocation_index
        for pair_zero_index in range(pair_count):
            pair = [
                (
                    "baseline",
                    baseline_binary,
                    baseline_identity["sha256"],
                    baseline_library_identity,
                ),
                (
                    "candidate",
                    candidate_binary,
                    candidate_identity["sha256"],
                    candidate_library_identity,
                ),
            ]
            if pair_zero_index % 2 == 1:
                pair.reverse()
            for order_in_pair, (role, binary, digest, library_identity) in enumerate(
                pair, start=1
            ):
                record, issues = run_one(
                    role,
                    binary,
                    digest,
                    library_identity,
                    arguments.engine,
                    database,
                    arguments.rows,
                    arguments.seconds,
                    timeout_seconds,
                    arguments.cpu_affinity,
                    phase,
                    pair_zero_index + 1,
                    order_in_pair,
                    invocation_index,
                )
                invocation_index += 1
                destination.append(record)
                if issues:
                    hard_failures.extend(
                        f"{phase} pair {pair_zero_index + 1} {role}: {issue}"
                        for issue in issues
                    )
                    return
            print(
                f"{phase} pair {pair_zero_index + 1}/{pair_count} complete",
                file=sys.stderr,
                flush=True,
            )

    run_pairs("warmup", arguments.warmup, warmup_runs)
    if not hard_failures:
        run_pairs("measured", arguments.rounds, measured_runs)

    database_after = database_artifacts(database)
    base_report["artifacts"]["database_after"] = database_after
    unchanged = {
        label: artifact_contents_equal(database_before[label], database_after[label])
        for label in ("database", "wal", "shm")
    }
    base_report["artifacts"]["database_artifacts_unchanged"] = unchanged
    changed = [label for label, is_unchanged in unchanged.items() if not is_unchanged]
    if changed:
        hard_failures.append(
            "read-only stress modified database artifacts: " + ", ".join(changed)
        )

    summaries: dict[str, Any] = {}
    if not hard_failures:
        baseline_records = sorted(
            (
                record
                for record in measured_runs
                if record["comparison_role"] == "baseline"
            ),
            key=lambda record: record["pair_index"],
        )
        candidate_records = sorted(
            (
                record
                for record in measured_runs
                if record["comparison_role"] == "candidate"
            ),
            key=lambda record: record["pair_index"],
        )
        if (
            len(baseline_records) != arguments.rounds
            or len(candidate_records) != arguments.rounds
        ):
            hard_failures.append("measured paired results are incomplete")
        else:
            for field in HIGHER_IS_BETTER:
                summaries[field] = metric_summary(
                    baseline_records,
                    candidate_records,
                    field,
                    True,
                    arguments.tolerance_pct,
                )
            for field in LOWER_IS_BETTER:
                summaries[field] = metric_summary(
                    baseline_records,
                    candidate_records,
                    field,
                    False,
                    arguments.tolerance_pct,
                )

    metric_gate_pass = bool(summaries) and all(
        summary["median_pass"] for summary in summaries.values()
    )
    overall_pass = not hard_failures and metric_gate_pass
    report = {
        **base_report,
        "overall_pass": overall_pass,
        "hard_failure": bool(hard_failures),
        "hard_failure_messages": hard_failures,
        "metric_gate_pass": metric_gate_pass,
        "metrics": summaries,
        "warmup_runs": warmup_runs,
        "runs": measured_runs,
    }
    write_report(report, arguments.output)
    if hard_failures:
        return 2
    if overall_pass or arguments.report_only:
        return 0
    return 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, ValueError) as error:
        print(f"read-stress gate error: {error}", file=sys.stderr)
        raise SystemExit(2) from error
