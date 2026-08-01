#!/usr/bin/env python3
"""Auditable Pareto gate for the complete sqlite_comparison workload."""

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
import tempfile
import time
from functools import partial
from pathlib import Path
from typing import Any, Callable


ENGINES = ("sqlite", "csgdb-plaintext", "csgdb-encrypted")
STRESS_ROWS_PER_COMMIT = 32
SQLITE_DATABASE_HEADER = b"SQLite format 3\x00"
RESULT_FIELDS = (
    "engine",
    "engine_sqlite_version",
    "system_sqlite_version",
    "csgdb_version",
    "cache_bytes_per_connection",
    "rows",
    "point_operations",
    "range_operations",
    "update_operations",
    "durable_operations",
    "open_ms",
    "bulk_insert_ms",
    "durable_autocommit_ms",
    "point_read_ms",
    "range_scan_ms",
    "range_rows",
    "update_ms",
    "stress_seconds",
    "stress_reads",
    "stress_writes",
    "stress_commits",
    "stress_errors",
    "read_p50_us",
    "read_p95_us",
    "read_p99_us",
    "commit_p50_us",
    "commit_p95_us",
    "commit_p99_us",
    "database_bytes",
    "max_rss_kib",
    "integrity_ok",
    "deterministic_checksum",
    "checksum",
)
CONFIGURATION_FIELDS = (
    "rows",
    "point_operations",
    "range_operations",
    "update_operations",
    "durable_operations",
)
CONSISTENCY_FIELDS = (
    "engine_sqlite_version",
    "system_sqlite_version",
    "csgdb_version",
    "cache_bytes_per_connection",
    *CONFIGURATION_FIELDS,
    "range_rows",
    "deterministic_checksum",
)
HIGHER_IS_BETTER = (
    "stress_reads_per_second",
    "stress_writes_per_second",
)
LOWER_IS_BETTER = (
    "open_ms",
    "bulk_insert_ms",
    "durable_autocommit_ms",
    "point_read_ms",
    "range_scan_ms",
    "update_ms",
    "read_p50_us",
    "read_p95_us",
    "read_p99_us",
    "commit_p50_us",
    "commit_p95_us",
    "commit_p99_us",
    "database_bytes",
    "max_rss_kib",
)


def positive_integer(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("value must be a positive integer")
    return parsed


def row_count(value: str) -> int:
    parsed = positive_integer(value)
    if parsed < 1_000 or parsed > 10_000_000:
        raise argparse.ArgumentTypeError("rows must be between 1000 and 10000000")
    return parsed


def measured_round_count(value: str) -> int:
    parsed = positive_integer(value)
    if parsed < 4 or parsed % 2 != 0:
        raise argparse.ArgumentTypeError(
            "measured rounds must be an even integer of at least four"
        )
    return parsed


def non_negative_integer(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("value must be a non-negative integer")
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
            "Run the complete sqlite_comparison workload on fresh databases in "
            "alternating baseline/candidate order, then enforce a median Pareto "
            "no-regression gate."
        )
    )
    parser.add_argument(
        "--binary",
        type=Path,
        default=Path("target/sqlite-comparison"),
        help=(
            "candidate sqlite_comparison executable "
            "(default: target/sqlite-comparison)"
        ),
    )
    parser.add_argument(
        "--baseline-binary",
        type=Path,
        help=(
            "optional frozen baseline executable; by default --binary is used "
            "for both engines"
        ),
    )
    parser.add_argument(
        "--baseline-engine",
        choices=ENGINES,
        default="sqlite",
        help="baseline engine (default: sqlite)",
    )
    parser.add_argument("--candidate-engine", choices=ENGINES, required=True)
    parser.add_argument("--rows", type=row_count, default=100_000)
    parser.add_argument(
        "--point",
        "--point-reads",
        dest="point_operations",
        type=positive_integer,
        default=200_000,
    )
    parser.add_argument(
        "--range",
        "--range-scans",
        dest="range_operations",
        type=positive_integer,
        default=5_000,
    )
    parser.add_argument("--updates", type=positive_integer, default=100_000)
    parser.add_argument(
        "--durable",
        "--durable-writes",
        dest="durable_operations",
        type=positive_integer,
        default=1_000,
    )
    parser.add_argument("--stress-seconds", type=positive_integer, default=10)
    parser.add_argument("--rounds", type=measured_round_count, default=8)
    parser.add_argument(
        "--warmup",
        "--warmup-pairs",
        dest="warmup",
        type=non_negative_integer,
        nargs="?",
        const=1,
        default=1,
        help="number of unscored AB/BA warm-up pairs (default: 1)",
    )
    parser.add_argument(
        "--cpu-affinity",
        type=cpu_set,
        help="optional Linux CPU list inherited by every benchmark process",
    )
    parser.add_argument(
        "--timeout-seconds",
        "--timeout",
        dest="timeout_seconds",
        type=positive_integer,
        help="per-process timeout (default: max(300, 3 * stress seconds + 120))",
    )
    parser.add_argument("--output", type=Path, help="also write the full JSON report")
    parser.add_argument(
        "--tolerance",
        "--tolerance-pct",
        "--allowed-regression-pct",
        dest="tolerance_pct",
        type=finite_non_negative,
        default=0.0,
        help="allowed median regression percentage (default: strict 0%%)",
    )
    parser.add_argument(
        "--report-only",
        action="store_true",
        help="do not fail on metric regressions; benchmark errors still fail",
    )
    arguments = parser.parse_args()
    if (
        arguments.baseline_binary is None
        and arguments.baseline_engine == arguments.candidate_engine
    ):
        parser.error(
            "same-engine comparisons require a distinct --baseline-binary"
        )
    return arguments


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
    library = binary.parent / "libcsgdb.so"
    if library.is_file():
        identity = artifact_identity(library.resolve())
        identity["resolution"] = "sibling-libcsgdb.so"
        return identity
    if engine != "sqlite":
        raise RuntimeError(
            f"the {role} CSGDB engine requires libcsgdb.so beside its "
            f"benchmark binary: {library}"
        )
    return None


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


def is_number(value: Any) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def is_positive_number(value: Any) -> bool:
    return is_number(value) and math.isfinite(float(value)) and value > 0


def is_positive_integer(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


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


def validate_result(
    result: dict[str, Any], engine: str, configuration: dict[str, int]
) -> list[str]:
    issues: list[str] = []
    missing_fields = sorted(set(RESULT_FIELDS) - set(result))
    unexpected_fields = sorted(set(result) - set(RESULT_FIELDS))
    if missing_fields:
        issues.append("benchmark JSON is missing fields: " + ", ".join(missing_fields))
    if unexpected_fields:
        issues.append(
            "benchmark JSON has unexpected fields: " + ", ".join(unexpected_fields)
        )
    if result.get("engine") != engine:
        issues.append(f"engine expected {engine!r}, got {result.get('engine')!r}")
    for field in CONFIGURATION_FIELDS:
        expected = configuration[field]
        if result.get(field) != expected:
            issues.append(f"{field} expected {expected}, got {result.get(field)!r}")

    for field in (
        "open_ms",
        "bulk_insert_ms",
        "durable_autocommit_ms",
        "point_read_ms",
        "range_scan_ms",
        "update_ms",
        "stress_seconds",
        "read_p50_us",
        "read_p95_us",
        "read_p99_us",
        "commit_p50_us",
        "commit_p95_us",
        "commit_p99_us",
    ):
        if not is_positive_number(result.get(field)):
            issues.append(f"{field} must be finite and positive")

    for field in (
        "cache_bytes_per_connection",
        "range_rows",
        "stress_reads",
        "stress_writes",
        "stress_commits",
        "database_bytes",
        "max_rss_kib",
    ):
        if not is_positive_integer(result.get(field)):
            issues.append(f"{field} must be a positive integer")

    checksum = result.get("checksum")
    if not isinstance(checksum, int) or isinstance(checksum, bool) or checksum < 0:
        issues.append("checksum must be a non-negative integer")
    if not is_positive_integer(result.get("deterministic_checksum")):
        issues.append("deterministic_checksum must be a positive integer")
    stress_errors = result.get("stress_errors")
    if not isinstance(stress_errors, int) or isinstance(stress_errors, bool):
        issues.append("stress_errors must be an integer")
    elif stress_errors != 0:
        issues.append(f"benchmark reported {stress_errors} stress errors")
    if result.get("integrity_ok") is not True:
        issues.append("integrity_ok must be true")

    stress_commits = result.get("stress_commits")
    stress_writes = result.get("stress_writes")
    if is_positive_integer(stress_commits) and is_positive_integer(stress_writes):
        if stress_writes != stress_commits * STRESS_ROWS_PER_COMMIT:
            issues.append(
                "stress_writes must equal stress_commits times the fixed batch size"
            )

    for field in (
        "engine_sqlite_version",
        "system_sqlite_version",
        "csgdb_version",
    ):
        value = result.get(field)
        if not isinstance(value, str) or not value.strip():
            issues.append(f"{field} must be a non-empty string")

    for prefix in ("read", "commit"):
        percentile_values = tuple(
            result.get(f"{prefix}_{percentile}_us")
            for percentile in ("p50", "p95", "p99")
        )
        if all(is_number(value) for value in percentile_values) and not (
            percentile_values[0] <= percentile_values[1] <= percentile_values[2]
        ):
            issues.append(
                f"{prefix} latency percentiles must satisfy p50 <= p95 <= p99"
            )

    stress_seconds = result.get("stress_seconds")
    stress_reads = result.get("stress_reads")
    stress_writes = result.get("stress_writes")
    if is_positive_number(stress_seconds):
        requested_seconds = configuration["stress_seconds_requested"]
        if float(stress_seconds) + 0.001 < requested_seconds:
            issues.append(
                "reported stress_seconds is shorter than the requested duration"
            )
        if is_positive_integer(stress_reads):
            result["stress_reads_per_second"] = stress_reads / stress_seconds
        if is_positive_integer(stress_writes):
            result["stress_writes_per_second"] = stress_writes / stress_seconds
    return issues


def database_artifacts(database: Path) -> dict[str, Any]:
    artifacts: dict[str, Any] = {}
    for label, path in (
        ("database", database),
        ("wal", Path(os.fspath(database) + "-wal")),
        ("shm", Path(os.fspath(database) + "-shm")),
    ):
        if path.is_file():
            artifacts[label] = artifact_identity(path)
        else:
            artifacts[label] = None
    return artifacts


def run_one(
    binary: Path,
    binary_identity: dict[str, Any],
    library_identity: dict[str, Any] | None,
    role: str,
    engine: str,
    database: Path,
    configuration: dict[str, int],
    timeout_seconds: int,
    affinity: frozenset[int] | None,
    phase: str,
    pair_index: int,
    order_in_pair: int,
    invocation_index: int,
) -> tuple[dict[str, Any], list[str]]:
    database_paths = (
        database,
        Path(os.fspath(database) + "-wal"),
        Path(os.fspath(database) + "-shm"),
    )
    preexisting = [os.fspath(path) for path in database_paths if path.exists()]
    command = [
        os.fspath(binary),
        engine,
        os.fspath(database),
        str(configuration["rows"]),
        str(configuration["point_operations"]),
        str(configuration["range_operations"]),
        str(configuration["update_operations"]),
        str(configuration["durable_operations"]),
        str(configuration["stress_seconds_requested"]),
    ]
    record: dict[str, Any] = {
        "phase": phase,
        "pair_index": pair_index,
        "order_in_pair": order_in_pair,
        "invocation_index": invocation_index,
        "comparison_role": role,
        "engine_requested": engine,
        "benchmark_binary": os.fspath(binary),
        "binary_sha256": binary_identity["sha256"],
        "engine_library": library_identity,
        "database_path": os.fspath(database),
        "database_fresh_before_run": not preexisting,
        "preexisting_database_files": preexisting,
        "command": command,
        "command_shell": shlex.join(command),
    }
    if preexisting:
        issues = ["database path or a sidecar existed before the run"]
        record["hard_failure_messages"] = issues
        return record, issues

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
        record.update(
            {
                "returncode": completed.returncode,
                "wall_seconds": (
                    time.monotonic_ns() - started_ns
                ) / 1_000_000_000.0,
                "raw_stdout": completed.stdout,
                "raw_stderr": completed.stderr,
            }
        )
        result, issues = parse_stdout(completed.stdout)
        if result is not None:
            issues.extend(validate_result(result, engine, configuration))
            record["benchmark_result"] = dict(result)
            record.update(
                {
                    field: result[field]
                    for field in (
                        *RESULT_FIELDS,
                        "stress_reads_per_second",
                        "stress_writes_per_second",
                    )
                    if field in result
                }
            )
        if completed.returncode != 0:
            issues.append(f"benchmark exited with status {completed.returncode}")
    except subprocess.TimeoutExpired as error:
        stdout = (
            error.stdout.decode() if isinstance(error.stdout, bytes) else error.stdout
        )
        stderr = (
            error.stderr.decode() if isinstance(error.stderr, bytes) else error.stderr
        )
        record.update(
            {
                "returncode": None,
                "wall_seconds": (
                    time.monotonic_ns() - started_ns
                ) / 1_000_000_000.0,
                "raw_stdout": stdout or "",
                "raw_stderr": stderr or "",
                "timed_out": True,
            }
        )
        issues = [f"benchmark exceeded the {timeout_seconds}s timeout"]

    artifacts = database_artifacts(database)
    record["database_artifacts_after_run"] = artifacts
    main_artifact = artifacts["database"]
    if main_artifact is None or main_artifact["size_bytes"] <= 0:
        issues.append("benchmark did not leave a positive-size database file")
    else:
        with database.open("rb") as source:
            header = source.read(len(SQLITE_DATABASE_HEADER))
        plaintext_header = header == SQLITE_DATABASE_HEADER
        record["database_header_hex"] = header.hex()
        record["database_has_sqlite_plaintext_header"] = plaintext_header
        if len(header) != len(SQLITE_DATABASE_HEADER):
            issues.append("database file is too short to contain a complete header")
        elif engine == "csgdb-encrypted" and plaintext_header:
            issues.append("encrypted database exposes a plaintext SQLite header")
        elif engine != "csgdb-encrypted" and not plaintext_header:
            issues.append("plaintext engine did not produce a SQLite database header")
        if is_positive_integer(record.get("database_bytes")) and (
            record["database_bytes"] != main_artifact["size_bytes"]
        ):
            issues.append(
                "reported database_bytes does not match the resulting "
                "database file size"
            )
    record["hard_failure_messages"] = list(dict.fromkeys(issues))
    return record, record["hard_failure_messages"]


def consistency_report(
    records: list[dict[str, Any]], role_engines: tuple[tuple[str, str], ...]
) -> tuple[dict[str, Any], list[str]]:
    report: dict[str, Any] = {
        "cross_engine_checksum_equality_required": False,
        "deterministic_checksum_equality_required": True,
        "checksum_policy": (
            "audited but not compared because it includes concurrent stress reads"
        ),
        "run_groups": {},
    }
    issues: list[str] = []
    for role, engine in role_engines:
        group_name = f"{role}:{engine}"
        engine_records = [
            record
            for record in records
            if record.get("comparison_role") == role
            and record.get("engine") == engine
        ]
        if not engine_records:
            report["run_groups"][group_name] = {
                "consistent": False,
                "record_count": 0,
                "reference": None,
            }
            issues.append(f"no valid workload output was recorded for {group_name}")
            continue
        reference = {
            field: engine_records[0].get(field) for field in CONSISTENCY_FIELDS
        }
        mismatches: list[dict[str, Any]] = []
        for record in engine_records[1:]:
            for field in CONSISTENCY_FIELDS:
                if record.get(field) != reference[field]:
                    mismatches.append(
                        {
                            "phase": record.get("phase"),
                            "pair_index": record.get("pair_index"),
                            "comparison_role": record.get("comparison_role"),
                            "field": field,
                            "expected": reference[field],
                            "actual": record.get(field),
                        }
                    )
        engine_report = {
            "consistent": not mismatches,
            "record_count": len(engine_records),
            "reference": reference,
            "checksums": [record.get("checksum") for record in engine_records],
            "mismatches": mismatches,
        }
        report["run_groups"][group_name] = engine_report
        if mismatches:
            issues.append(
                f"{group_name} emitted inconsistent deterministic workload results"
            )
    range_row_values = {
        details["reference"]["range_rows"]
        for details in report["run_groups"].values()
        if details["reference"] is not None
    }
    report["cross_role_range_rows_equal"] = len(range_row_values) <= 1
    report["cross_engine_range_rows_equal"] = len(range_row_values) <= 1
    if len(range_row_values) > 1:
        issues.append(
            "comparison roles returned different deterministic range row counts"
        )
    deterministic_checksums = {
        details["reference"]["deterministic_checksum"]
        for details in report["run_groups"].values()
        if details["reference"] is not None
    }
    report["cross_role_deterministic_checksum_equal"] = (
        len(deterministic_checksums) <= 1
    )
    if len(deterministic_checksums) > 1:
        issues.append("comparison roles returned different deterministic checksums")
    return report, issues


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
        pairs.append(
            {
                "round": round_index,
                "baseline": baseline_value,
                "candidate": candidate_value,
                "candidate_delta_pct": pair_raw_delta,
                "candidate_improvement_pct": pair_improvement,
                "disposition": disposition,
                "within_tolerance": pair_improvement >= -tolerance_pct,
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
    candidate_binary = arguments.binary.resolve()
    baseline_binary = (
        arguments.baseline_binary.resolve()
        if arguments.baseline_binary is not None
        else candidate_binary
    )
    for role, binary in (
        ("baseline", baseline_binary),
        ("candidate", candidate_binary),
    ):
        if not binary.is_file():
            raise RuntimeError(
                f"{role} benchmark binary does not exist or is not a file: {binary}"
            )
        if not os.access(binary, os.X_OK):
            raise RuntimeError(f"{role} benchmark binary is not executable: {binary}")
    if (
        arguments.baseline_engine == arguments.candidate_engine
        and baseline_binary == candidate_binary
    ):
        raise RuntimeError(
            "same-engine comparisons require distinct benchmark artifacts"
        )
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

    baseline_binary_identity = artifact_identity(baseline_binary)
    candidate_binary_identity = artifact_identity(candidate_binary)
    baseline_library_identity = engine_library_identity(
        baseline_binary, arguments.baseline_engine, "baseline"
    )
    candidate_library_identity = engine_library_identity(
        candidate_binary, arguments.candidate_engine, "candidate"
    )
    baseline_linker_identity = dynamic_linker_identity(baseline_binary)
    candidate_linker_identity = (
        baseline_linker_identity
        if candidate_binary == baseline_binary
        else dynamic_linker_identity(candidate_binary)
    )
    verify_loaded_csgdb_library(
        "baseline",
        arguments.baseline_engine,
        baseline_library_identity,
        baseline_linker_identity,
    )
    verify_loaded_csgdb_library(
        "candidate",
        arguments.candidate_engine,
        candidate_library_identity,
        candidate_linker_identity,
    )
    if arguments.baseline_engine == arguments.candidate_engine:
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
            baseline_binary_identity["sha256"]
            == candidate_binary_identity["sha256"]
            and baseline_library_sha == candidate_library_sha
        ):
            raise RuntimeError(
                "same-engine A/B requires a different benchmark binary or "
                "sibling library hash"
            )
    timeout_seconds = arguments.timeout_seconds or max(
        300, arguments.stress_seconds * 3 + 120
    )
    selected_affinity = (
        sorted(arguments.cpu_affinity)
        if arguments.cpu_affinity is not None
        else None
    )
    configuration = {
        "rows": arguments.rows,
        "point_operations": arguments.point_operations,
        "range_operations": arguments.range_operations,
        "update_operations": arguments.updates,
        "durable_operations": arguments.durable_operations,
        "stress_seconds_requested": arguments.stress_seconds,
    }
    base_report: dict[str, Any] = {
        "schema_version": 1,
        "gate": "c-engine-comparison-pareto",
        "generated_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "invocation": [sys.executable, *sys.argv],
        "invocation_shell": shlex.join([sys.executable, *sys.argv]),
        "host": host_identity(),
        "dynamic_linker_environment": linker_environment,
        "artifacts": {
            "baseline_binary": baseline_binary_identity,
            "candidate_binary": candidate_binary_identity,
            "baseline_engine_library": baseline_library_identity,
            "candidate_engine_library": candidate_library_identity,
            "baseline_dynamic_linker": baseline_linker_identity,
            "candidate_dynamic_linker": candidate_linker_identity,
        },
        "configuration": {
            "baseline_engine": arguments.baseline_engine,
            "candidate_engine": arguments.candidate_engine,
            "separate_baseline_binary": arguments.baseline_binary is not None,
            **configuration,
            "rounds": arguments.rounds,
            "warmup_pairs": arguments.warmup,
            "timeout_seconds": timeout_seconds,
            "cpu_affinity": selected_affinity,
            "tolerance_pct": arguments.tolerance_pct,
            "median_gate": "strict Pareto no-regression"
            if arguments.tolerance_pct == 0.0
            else "Pareto with configured tolerance",
            "schedule": (
                "fresh database per invocation; alternating baseline/candidate "
                "AB then BA within each phase"
            ),
        },
    }

    warmup_runs: list[dict[str, Any]] = []
    measured_runs: list[dict[str, Any]] = []
    hard_failures: list[str] = []
    invocation_index = 0

    with tempfile.TemporaryDirectory(prefix="csgdb-engine-gate-") as temporary:
        temporary_root = Path(temporary)
        base_report["temporary_database_root"] = os.fspath(temporary_root)

        def run_pairs(
            phase: str, pair_count: int, destination: list[dict[str, Any]]
        ) -> None:
            nonlocal invocation_index
            for pair_zero_index in range(pair_count):
                pair = [
                    (
                        "baseline",
                        arguments.baseline_engine,
                        baseline_binary,
                        baseline_binary_identity,
                        baseline_library_identity,
                    ),
                    (
                        "candidate",
                        arguments.candidate_engine,
                        candidate_binary,
                        candidate_binary_identity,
                        candidate_library_identity,
                    ),
                ]
                if pair_zero_index % 2 == 1:
                    pair.reverse()
                for order_in_pair, (
                    role,
                    engine,
                    role_binary,
                    role_binary_identity,
                    role_library_identity,
                ) in enumerate(pair, start=1):
                    database = temporary_root / (
                        f"{phase}-pair-{pair_zero_index + 1:03d}-"
                        f"order-{order_in_pair}-{role}-{engine}.db"
                    )
                    record, issues = run_one(
                        role_binary,
                        role_binary_identity,
                        role_library_identity,
                        role,
                        engine,
                        database,
                        configuration,
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

        all_runs = [*warmup_runs, *measured_runs]
        workload_consistency, consistency_issues = consistency_report(
            all_runs,
            (
                ("baseline", arguments.baseline_engine),
                ("candidate", arguments.candidate_engine),
            ),
        )
        hard_failures.extend(consistency_issues)

        summaries: dict[str, Any] = {}
        if not hard_failures:
            baseline_records = sorted(
                (
                    record
                    for record in measured_runs
                    if record.get("comparison_role") == "baseline"
                ),
                key=lambda record: record["pair_index"],
            )
            candidate_records = sorted(
                (
                    record
                    for record in measured_runs
                    if record.get("comparison_role") == "candidate"
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
            "hard_failure_messages": list(dict.fromkeys(hard_failures)),
            "metric_gate_pass": metric_gate_pass,
            "metrics": summaries,
            "workload_output_consistency": workload_consistency,
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
        print(f"engine comparison gate error: {error}", file=sys.stderr)
        raise SystemExit(2) from error
