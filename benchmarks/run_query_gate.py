#!/usr/bin/env python3
"""Interleaved Pareto gate for public Rust scalar-query API paths."""

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
from functools import partial
from pathlib import Path
from typing import Any, Callable


WORKLOADS = ("fixed", "hot16", "overflow17", "overflow64", "unique10000")
SECURITIES = ("plaintext", "encrypted")
API_CANDIDATE_MODES = ("public", "cached")
IMPLEMENTATIONS = {
    "reference": "direct-reference",
    "public": "public-query_i64",
    "cached": "public-query_i64_cached",
}
HIGHER_IS_BETTER = ("throughput_ops_s",)
LOWER_IS_BETTER = ("p50_ns", "p95_ns", "p99_ns", "vm_hwm_kib")


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
            "rounds must be an even integer of at least four for balanced order"
        )
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
            "Run an interleaved, multi-round Pareto gate for a selected public "
            "scalar-query API versus its direct-prepare reference, or compare "
            "candidate and baseline binaries on query_i64."
        )
    )
    parser.add_argument(
        "--binary",
        type=Path,
        default=Path("target/release/examples/query_gate"),
        help="candidate query_gate example binary",
    )
    parser.add_argument(
        "--baseline-binary",
        type=Path,
        help=(
            "optional baseline query_gate binary; when supplied, both binaries "
            "run the public query_i64 implementation"
        ),
    )
    parser.add_argument(
        "--api-candidate-mode",
        choices=API_CANDIDATE_MODES,
        default="public",
        help=(
            "public API compared with the direct reference when no baseline "
            "binary is supplied (default: public)"
        ),
    )
    parser.add_argument(
        "--build",
        action="store_true",
        help="build the release example with Cargo before benchmarking",
    )
    parser.add_argument("--rounds", type=balanced_round_count, default=8)
    parser.add_argument("--iterations", type=positive_integer, default=50_000)
    parser.add_argument(
        "--warmup-iterations", type=non_negative_integer, default=10_000
    )
    parser.add_argument(
        "--workloads", nargs="+", choices=WORKLOADS, default=list(WORKLOADS)
    )
    parser.add_argument(
        "--securities", nargs="+", choices=SECURITIES, default=list(SECURITIES)
    )
    parser.add_argument("--timeout-seconds", type=positive_integer, default=180)
    parser.add_argument(
        "--cpu-affinity",
        type=cpu_set,
        help="optional Linux CPU list inherited by each benchmark process",
    )
    parser.add_argument(
        "--allowed-regression-pct",
        type=float,
        default=0.0,
        help="non-negative tolerance; the default is the strict no-regression gate",
    )
    parser.add_argument(
        "--report-only",
        action="store_true",
        help="always exit successfully after emitting the summary",
    )
    parser.add_argument("--output", type=Path, help="also write the JSON summary here")
    arguments = parser.parse_args()
    if not math.isfinite(arguments.allowed_regression_pct):
        parser.error("--allowed-regression-pct must be finite")
    if arguments.allowed_regression_pct < 0.0:
        parser.error("--allowed-regression-pct must be non-negative")
    if (
        arguments.baseline_binary is not None
        and arguments.api_candidate_mode != "public"
    ):
        parser.error(
            "--api-candidate-mode must be public when --baseline-binary is supplied"
        )
    return arguments


def build_example() -> None:
    subprocess.run(
        [
            "cargo",
            "build",
            "-p",
            "csgdb",
            "--example",
            "query_gate",
            "--release",
            "--locked",
        ],
        check=True,
    )


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


def host_identity() -> dict[str, Any]:
    uname = platform.uname()
    process_affinity: list[int] | None = None
    if hasattr(os, "sched_getaffinity"):
        process_affinity = sorted(os.sched_getaffinity(0))
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
        "process_cpu_affinity": process_affinity,
    }


def run_one(
    binary: Path,
    binary_sha256: str,
    mode: str,
    security: str,
    workload: str,
    database_path: Path,
    iterations: int,
    warmup_iterations: int,
    timeout_seconds: int,
    affinity: frozenset[int] | None,
) -> dict[str, Any]:
    command = [
        os.fspath(binary),
        mode,
        security,
        workload,
        os.fspath(database_path),
        str(iterations),
        str(warmup_iterations),
    ]
    preexec_fn: Callable[[], None] | None = None
    if affinity is not None:
        preexec_fn = partial(os.sched_setaffinity, 0, affinity)
    completed = subprocess.run(
        command,
        check=False,
        capture_output=True,
        text=True,
        timeout=timeout_seconds,
        preexec_fn=preexec_fn,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"query gate failed ({' '.join(command)}):\n{completed.stderr.strip()}"
        )
    lines = [line for line in completed.stdout.splitlines() if line.strip()]
    if len(lines) != 1:
        raise RuntimeError(
            f"query gate must emit exactly one JSON line; received {len(lines)} lines"
        )
    try:
        result = json.loads(lines[0])
    except json.JSONDecodeError as error:
        raise RuntimeError(f"query gate emitted invalid JSON: {error}") from error
    validate_result(result, mode, security, workload, iterations, warmup_iterations)
    result["benchmark_binary"] = os.fspath(binary)
    result["binary_sha256"] = binary_sha256
    result["command"] = command
    result["command_shell"] = shlex.join(command)
    result["raw_stderr"] = completed.stderr
    return result


def validate_result(
    result: dict[str, Any],
    mode: str,
    security: str,
    workload: str,
    iterations: int,
    warmup_iterations: int,
) -> None:
    expected_implementation = IMPLEMENTATIONS[mode]
    expected = {
        "schema_version": 1,
        "implementation": expected_implementation,
        "security": security,
        "workload": workload,
        "iterations": iterations,
        "warmup_iterations": warmup_iterations,
    }
    for field, expected_value in expected.items():
        if result.get(field) != expected_value:
            raise RuntimeError(
                f"unexpected {field}: expected {expected_value!r}, "
                f"received {result.get(field)!r}"
            )

    for field in (
        "elapsed_ns",
        "throughput_ops_s",
        "p50_ns",
        "p95_ns",
        "p99_ns",
        "checksum",
    ):
        value = result.get(field)
        if not isinstance(value, (int, float)) or isinstance(value, bool):
            raise RuntimeError(f"{field} is not numeric")
        if not math.isfinite(float(value)) or value <= 0:
            raise RuntimeError(f"{field} must be finite and positive")
    vm_hwm = result.get("vm_hwm_kib")
    if vm_hwm is not None and (
        not isinstance(vm_hwm, int) or isinstance(vm_hwm, bool) or vm_hwm <= 0
    ):
        raise RuntimeError("vm_hwm_kib must be a positive integer or null")
    p50 = result.get("p50_ns")
    p95 = result.get("p95_ns")
    p99 = result.get("p99_ns")
    if not (p50 <= p95 <= p99):
        raise RuntimeError("latency percentiles must satisfy p50 <= p95 <= p99")


def median(records: list[dict[str, Any]], field: str) -> float | None:
    values = [record[field] for record in records if record.get(field) is not None]
    if len(values) != len(records):
        return None
    return float(statistics.median(values))


def metric_statistics(
    baseline: list[dict[str, Any]],
    candidate: list[dict[str, Any]],
    field: str,
    higher_is_better: bool,
    allowed_regression_pct: float,
) -> dict[str, Any]:
    baseline_median = median(baseline, field)
    candidate_median = median(candidate, field)
    if baseline_median is None or candidate_median is None:
        return {
            "baseline_median": baseline_median,
            "candidate_median": candidate_median,
            "available": False,
            "median_pass": False,
            "paired_candidate_wins": 0,
            "paired_equal": 0,
            "paired_candidate_regressions": 0,
        }

    candidate_delta_pct = (candidate_median / baseline_median - 1.0) * 100.0
    if higher_is_better:
        median_pass = candidate_delta_pct >= -allowed_regression_pct
    else:
        median_pass = candidate_delta_pct <= allowed_regression_pct

    wins = 0
    equal = 0
    regressions = 0
    for baseline_run, candidate_run in zip(baseline, candidate):
        baseline_value = baseline_run.get(field)
        candidate_value = candidate_run.get(field)
        if baseline_value is None or candidate_value is None:
            continue
        if candidate_value == baseline_value:
            equal += 1
        elif (candidate_value > baseline_value) == higher_is_better:
            wins += 1
        else:
            regressions += 1

    return {
        "baseline_median": baseline_median,
        "candidate_median": candidate_median,
        "candidate_delta_pct": candidate_delta_pct,
        "available": True,
        "median_pass": median_pass,
        "paired_candidate_wins": wins,
        "paired_equal": equal,
        "paired_candidate_regressions": regressions,
    }


def compare_api_metric(
    reference: list[dict[str, Any]],
    candidate: list[dict[str, Any]],
    field: str,
    higher_is_better: bool,
    allowed_regression_pct: float,
    candidate_mode: str,
) -> dict[str, Any]:
    metric = metric_statistics(
        reference, candidate, field, higher_is_better, allowed_regression_pct
    )
    return {
        "reference_median": metric["baseline_median"],
        f"{candidate_mode}_median": metric["candidate_median"],
        **(
            {f"{candidate_mode}_delta_pct": metric["candidate_delta_pct"]}
            if metric["available"]
            else {}
        ),
        "available": metric["available"],
        "median_pass": metric["median_pass"],
        f"paired_{candidate_mode}_wins": metric["paired_candidate_wins"],
        "paired_equal": metric["paired_equal"],
        "paired_regressions": metric["paired_candidate_regressions"],
    }


def summarize_api_scenario(
    records: list[dict[str, Any]],
    security: str,
    workload: str,
    allowed_regression_pct: float,
    candidate_mode: str,
) -> dict[str, Any]:
    candidate_implementation = IMPLEMENTATIONS[candidate_mode]
    scenario_records = [
        record
        for record in records
        if record["security"] == security and record["workload"] == workload
    ]
    reference = sorted(
        (
            record
            for record in scenario_records
            if record["implementation"] == "direct-reference"
        ),
        key=lambda record: record["round"],
    )
    candidate = sorted(
        (
            record
            for record in scenario_records
            if record["implementation"] == candidate_implementation
        ),
        key=lambda record: record["round"],
    )
    if len(reference) != len(candidate) or not reference:
        raise RuntimeError(f"incomplete paired results for {security}/{workload}")

    metrics: dict[str, Any] = {}
    for field in HIGHER_IS_BETTER:
        metrics[field] = compare_api_metric(
            reference,
            candidate,
            field,
            True,
            allowed_regression_pct,
            candidate_mode,
        )
    for field in LOWER_IS_BETTER:
        metrics[field] = compare_api_metric(
            reference,
            candidate,
            field,
            False,
            allowed_regression_pct,
            candidate_mode,
        )

    reference_checksums = {record["checksum"] for record in reference}
    candidate_checksums = {record["checksum"] for record in candidate}
    checksum_pass = (
        len(reference_checksums) == 1
        and len(candidate_checksums) == 1
        and reference_checksums == candidate_checksums
    )
    median_pareto_pass = checksum_pass and all(
        metric["median_pass"] for metric in metrics.values()
    )
    strictly_better_metrics = [
        field
        for field, metric in metrics.items()
        if metric["available"]
        and (
            metric[f"{candidate_mode}_median"] > metric["reference_median"]
            if field in HIGHER_IS_BETTER
            else metric[f"{candidate_mode}_median"] < metric["reference_median"]
        )
    ]
    return {
        "security": security,
        "workload": workload,
        "rounds": len(reference),
        "candidate_implementation": candidate_implementation,
        "checksum_pass": checksum_pass,
        "median_pareto_pass": median_pareto_pass,
        "strictly_better_metrics": strictly_better_metrics,
        "metrics": metrics,
    }


def summarize_binary_scenario(
    records: list[dict[str, Any]],
    security: str,
    workload: str,
    allowed_regression_pct: float,
) -> dict[str, Any]:
    scenario_records = [
        record
        for record in records
        if record["security"] == security and record["workload"] == workload
    ]
    baseline = sorted(
        (
            record
            for record in scenario_records
            if record["comparison_role"] == "baseline"
        ),
        key=lambda record: record["round"],
    )
    candidate = sorted(
        (
            record
            for record in scenario_records
            if record["comparison_role"] == "candidate"
        ),
        key=lambda record: record["round"],
    )
    if len(baseline) != len(candidate) or not baseline:
        raise RuntimeError(f"incomplete binary pairs for {security}/{workload}")

    metrics: dict[str, Any] = {}
    for field in HIGHER_IS_BETTER:
        metrics[field] = metric_statistics(
            baseline, candidate, field, True, allowed_regression_pct
        )
    for field in LOWER_IS_BETTER:
        metrics[field] = metric_statistics(
            baseline, candidate, field, False, allowed_regression_pct
        )

    baseline_checksums = {record["checksum"] for record in baseline}
    candidate_checksums = {record["checksum"] for record in candidate}
    checksum_pass = (
        len(baseline_checksums) == 1
        and len(candidate_checksums) == 1
        and baseline_checksums == candidate_checksums
    )
    median_pareto_pass = checksum_pass and all(
        metric["median_pass"] for metric in metrics.values()
    )
    strictly_better_metrics = [
        field
        for field, metric in metrics.items()
        if metric["available"]
        and (
            metric["candidate_median"] > metric["baseline_median"]
            if field in HIGHER_IS_BETTER
            else metric["candidate_median"] < metric["baseline_median"]
        )
    ]
    return {
        "security": security,
        "workload": workload,
        "rounds": len(baseline),
        "checksum_pass": checksum_pass,
        "median_pareto_pass": median_pareto_pass,
        "strictly_better_metrics": strictly_better_metrics,
        "metrics": metrics,
    }


def main() -> int:
    arguments = parse_args()
    if arguments.build:
        build_example()
    binary = arguments.binary.resolve()
    if not binary.is_file():
        raise RuntimeError(f"candidate query gate binary does not exist: {binary}")
    if not os.access(binary, os.X_OK):
        raise RuntimeError(f"candidate query gate binary is not executable: {binary}")
    baseline_binary = (
        arguments.baseline_binary.resolve()
        if arguments.baseline_binary is not None
        else None
    )
    if baseline_binary is not None and not baseline_binary.is_file():
        raise RuntimeError(
            f"baseline query gate binary does not exist: {baseline_binary}"
        )
    if baseline_binary is not None and not os.access(baseline_binary, os.X_OK):
        raise RuntimeError(
            f"baseline query gate binary is not executable: {baseline_binary}"
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
    binary_comparison = baseline_binary is not None
    binary_identity = artifact_identity(binary)
    baseline_identity = (
        artifact_identity(baseline_binary) if baseline_binary is not None else None
    )
    if (
        baseline_identity is not None
        and baseline_identity["sha256"] == binary_identity["sha256"]
    ):
        raise RuntimeError(
            "binary A/B requires different baseline and candidate artifact hashes"
        )

    scenarios = [
        (security, workload)
        for security in arguments.securities
        for workload in arguments.workloads
    ]
    records: list[dict[str, Any]] = []
    with tempfile.TemporaryDirectory(prefix="csgdb-query-gate-") as temporary:
        temporary_path = Path(temporary)
        invocation_index = 0
        for round_index in range(arguments.rounds):
            rotation = round_index % len(scenarios)
            scheduled = scenarios[rotation:] + scenarios[:rotation]
            if round_index % 2 == 1:
                scheduled.reverse()
            for security, workload in scheduled:
                if binary_comparison:
                    assert baseline_binary is not None
                    pair = [
                        ("baseline", baseline_binary, "public"),
                        ("candidate", binary, "public"),
                    ]
                else:
                    pair = [
                        ("reference", binary, "reference"),
                        (
                            arguments.api_candidate_mode,
                            binary,
                            arguments.api_candidate_mode,
                        ),
                    ]
                if round_index % 2 == 1:
                    pair.reverse()
                for comparison_role, run_binary, mode in pair:
                    database_path = temporary_path / (
                        f"round-{round_index + 1}-{invocation_index}-{security}-"
                        f"{workload}-{comparison_role}.db"
                    )
                    result = run_one(
                        run_binary,
                        (
                            baseline_identity["sha256"]
                            if comparison_role == "baseline"
                            and baseline_identity is not None
                            else binary_identity["sha256"]
                        ),
                        mode,
                        security,
                        workload,
                        database_path,
                        arguments.iterations,
                        arguments.warmup_iterations,
                        arguments.timeout_seconds,
                        arguments.cpu_affinity,
                    )
                    result["round"] = round_index + 1
                    result["invocation_index"] = invocation_index
                    if binary_comparison:
                        result["comparison_role"] = comparison_role
                        result["benchmark_binary"] = os.fspath(run_binary)
                    records.append(result)
                    invocation_index += 1
                print(
                    f"round {round_index + 1}/{arguments.rounds}: "
                    f"{security}/{workload} complete",
                    file=sys.stderr,
                    flush=True,
                )

    if binary_comparison:
        summaries = [
            summarize_binary_scenario(
                records, security, workload, arguments.allowed_regression_pct
            )
            for security, workload in scenarios
        ]
        comparison = {
            "kind": "binary-public-query_i64",
            "baseline": {
                "binary": os.fspath(baseline_binary),
                "implementation": "public-query_i64",
            },
            "candidate": {
                "binary": os.fspath(binary),
                "implementation": "public-query_i64",
            },
        }
        schedule = "rotated scenarios; alternating paired binary order"
    else:
        summaries = [
            summarize_api_scenario(
                records,
                security,
                workload,
                arguments.allowed_regression_pct,
                arguments.api_candidate_mode,
            )
            for security, workload in scenarios
        ]
        comparison = {
            "kind": "api-path",
            "binary": os.fspath(binary),
            "reference": "direct-reference",
            (
                "public"
                if arguments.api_candidate_mode == "public"
                else "candidate"
            ): IMPLEMENTATIONS[arguments.api_candidate_mode],
        }
        schedule = "rotated scenarios; alternating paired implementation order"
    overall_pass = all(summary["median_pareto_pass"] for summary in summaries)
    output = {
        "schema_version": 1,
        "gate": "rust-query-i64-pareto",
        "generated_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "invocation": [sys.executable, *sys.argv],
        "invocation_shell": shlex.join([sys.executable, *sys.argv]),
        "comparison": comparison,
        "host": host_identity(),
        "artifacts": {
            "candidate_binary": binary_identity,
            "baseline_binary": baseline_identity,
        },
        "configuration": {
            "binary": os.fspath(binary),
            "candidate_binary": os.fspath(binary),
            "baseline_binary": (
                os.fspath(baseline_binary) if baseline_binary is not None else None
            ),
            "api_candidate_mode": arguments.api_candidate_mode,
            "rounds": arguments.rounds,
            "iterations": arguments.iterations,
            "warmup_iterations": arguments.warmup_iterations,
            "workloads": arguments.workloads,
            "securities": arguments.securities,
            "allowed_regression_pct": arguments.allowed_regression_pct,
            "cpu_affinity": (
                sorted(arguments.cpu_affinity)
                if arguments.cpu_affinity is not None
                else None
            ),
            "schedule": schedule,
        },
        "overall_pass": overall_pass,
        "scenarios": summaries,
        "runs": records,
    }
    encoded = json.dumps(output, separators=(",", ":"), sort_keys=True)
    print(encoded)
    if arguments.output is not None:
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        arguments.output.write_text(encoded + "\n", encoding="utf-8")
    if arguments.report_only or overall_pass:
        return 0
    return 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"query gate error: {error}", file=sys.stderr)
        raise SystemExit(2) from error
