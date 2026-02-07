#!/usr/bin/env python3
"""Benchmark regression gate for tune-runtime profile throughput."""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", required=True, help="Path to tune-runtime JSON report")
    parser.add_argument("--baseline", required=True, help="Path to baseline JSON config")
    parser.add_argument(
        "--default-max-regression-percent",
        type=float,
        default=None,
        help="Override max allowed regression percentage for all profiles",
    )
    parser.add_argument(
        "--scenario",
        default=None,
        help="Optional baseline scenario key (for example: latency, loss, jitter)",
    )
    return parser.parse_args()


def load_json(path: Path) -> dict:
    return json.loads(path.read_text())


def report_profile_map(report: dict) -> dict[str, dict[str, float]]:
    profiles = report.get("profiles", [])
    result: dict[str, dict[str, float]] = {}
    for profile in profiles:
        name = str(profile.get("runtime_profile", "")).strip().lower()
        if not name:
            continue
        throughput = profile.get("throughput_gbps", {})
        p50 = throughput.get("p50")
        p95 = throughput.get("p95")
        successful_runs = profile.get("successful_runs")
        failed_runs = profile.get("failed_runs")
        success_rate = 1.0
        if isinstance(successful_runs, int) and isinstance(failed_runs, int):
            total = successful_runs + failed_runs
            if total > 0:
                success_rate = successful_runs / total
            else:
                success_rate = 0.0
        if isinstance(p50, (int, float)) and isinstance(p95, (int, float)):
            result[name] = {
                "p50": float(p50),
                "p95": float(p95),
                "success_rate": max(0.0, min(1.0, float(success_rate))),
            }
    return result


def fmt_float(value: float | None) -> str:
    if value is None:
        return "n/a"
    return f"{value:.3f}"


def resolve_baseline_profiles(
    baseline: dict, scenario: str | None
) -> tuple[dict, float, float]:
    if scenario:
        scenarios = baseline.get("scenarios", {})
        if not isinstance(scenarios, dict) or scenario not in scenarios:
            raise ValueError(f"baseline scenario '{scenario}' not found")
        scenario_cfg = scenarios[scenario]
        if not isinstance(scenario_cfg, dict):
            raise ValueError(f"baseline scenario '{scenario}' is not an object")
        profiles = scenario_cfg.get("profiles", {})
        if not isinstance(profiles, dict) or not profiles:
            raise ValueError(f"baseline scenario '{scenario}' profiles are missing")
        default_allowed = float(
            scenario_cfg.get(
                "default_max_regression_percent",
                baseline.get("default_max_regression_percent", 35.0),
            )
        )
        default_min_success_rate = float(
            scenario_cfg.get(
                "default_min_success_rate",
                baseline.get("default_min_success_rate", 0.0),
            )
        )
        return profiles, default_allowed, default_min_success_rate

    profiles = baseline.get("profiles", {})
    if not isinstance(profiles, dict) or not profiles:
        raise ValueError("baseline profiles are missing")
    default_allowed = float(baseline.get("default_max_regression_percent", 35.0))
    default_min_success_rate = float(baseline.get("default_min_success_rate", 0.0))
    return profiles, default_allowed, default_min_success_rate


def render_summary(
    rows: list[dict],
    failures: list[str],
    report_path: Path,
    baseline_path: Path,
    scenario: str | None,
) -> str:
    header = [
        "## Benchmark Gate",
        "",
        f"- Report: `{report_path}`",
        f"- Baseline: `{baseline_path}`",
        f"- Scenario: `{scenario if scenario else 'default'}`",
        "",
        "| Profile | Current p50 | Baseline p50 | Threshold p50 | Current p95 | Baseline p95 | Threshold p95 | Success Rate | Min Success Rate | Allowed Regression | Result |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|",
    ]
    table = []
    for row in rows:
        table.append(
            "| {profile} | {current_p50} | {baseline_p50} | {threshold_p50} | {current_p95} | {baseline_p95} | {threshold_p95} | {success_rate:.2f} | {min_success_rate:.2f} | {allowed:.1f}% | {result} |".format(
                profile=row["profile"],
                current_p50=fmt_float(row["current_p50"]),
                baseline_p50=fmt_float(row["baseline_p50"]),
                threshold_p50=fmt_float(row["threshold_p50"]),
                current_p95=fmt_float(row["current_p95"]),
                baseline_p95=fmt_float(row["baseline_p95"]),
                threshold_p95=fmt_float(row["threshold_p95"]),
                success_rate=row["success_rate"],
                min_success_rate=row["min_success_rate"],
                allowed=row["allowed"],
                result=row["result"],
            )
        )
    footer = []
    if failures:
        footer.extend(["", "### Failures"])
        footer.extend([f"- {failure}" for failure in failures])
    else:
        footer.extend(["", "All benchmark profile checks passed."])
    return "\n".join(header + table + footer)


def main() -> int:
    args = parse_args()
    report_path = Path(args.report)
    baseline_path = Path(args.baseline)

    report = load_json(report_path)
    baseline = load_json(baseline_path)

    profile_stats = report_profile_map(report)
    try:
        (
            baseline_profiles,
            baseline_default_allowed,
            baseline_default_min_success_rate,
        ) = resolve_baseline_profiles(baseline, args.scenario)
    except ValueError as err:
        print(str(err), file=sys.stderr)
        return 2

    default_allowed = (
        float(args.default_max_regression_percent)
        if args.default_max_regression_percent is not None
        else baseline_default_allowed
    )
    default_min_success_rate = baseline_default_min_success_rate

    rows: list[dict] = []
    failures: list[str] = []
    for profile, profile_cfg in baseline_profiles.items():
        if not isinstance(profile_cfg, dict):
            failures.append(f"profile '{profile}' baseline config is not an object")
            continue
        baseline_p50 = profile_cfg.get("baseline_p50_gbps")
        if not isinstance(baseline_p50, (int, float)) or float(baseline_p50) <= 0:
            failures.append(f"profile '{profile}' has invalid baseline_p50_gbps")
            continue
        baseline_p95_value = profile_cfg.get("baseline_p95_gbps")
        baseline_p95 = (
            float(baseline_p95_value)
            if isinstance(baseline_p95_value, (int, float))
            and float(baseline_p95_value) > 0
            else None
        )
        allowed = float(profile_cfg.get("max_regression_percent", default_allowed))
        min_success_rate = float(
            profile_cfg.get("min_success_rate", default_min_success_rate)
        )
        threshold_p50 = float(baseline_p50) * max(0.0, (1.0 - allowed / 100.0))
        threshold_p95 = (
            baseline_p95 * max(0.0, (1.0 - allowed / 100.0))
            if baseline_p95 is not None
            else None
        )

        current = profile_stats.get(profile.lower())
        result = "PASS"
        if current is None:
            failures.append(f"profile '{profile}' missing from benchmark report")
            current_p50 = 0.0
            current_p95 = 0.0
            current_success_rate = 0.0
            result = "FAIL"
        else:
            current_p50 = current["p50"]
            current_p95 = current["p95"]
            current_success_rate = current["success_rate"]

        if current_p50 < threshold_p50:
            failures.append(
                f"profile '{profile}' p50 regressed: current={current_p50:.3f} Gbps "
                f"< threshold={threshold_p50:.3f} Gbps "
                f"(baseline={float(baseline_p50):.3f}, allowed={allowed:.1f}%)"
            )
            result = "FAIL"
        if threshold_p95 is not None and current_p95 < threshold_p95:
            failures.append(
                f"profile '{profile}' p95 regressed: current={current_p95:.3f} Gbps "
                f"< threshold={threshold_p95:.3f} Gbps "
                f"(baseline={baseline_p95:.3f}, allowed={allowed:.1f}%)"
            )
            result = "FAIL"
        if current_success_rate < min_success_rate:
            failures.append(
                f"profile '{profile}' completion rate too low: "
                f"{current_success_rate:.2f} < {min_success_rate:.2f}"
            )
            result = "FAIL"

        rows.append(
            {
                "profile": profile,
                "current_p50": current_p50,
                "baseline_p50": float(baseline_p50),
                "threshold_p50": threshold_p50,
                "current_p95": current_p95,
                "baseline_p95": baseline_p95,
                "threshold_p95": threshold_p95,
                "success_rate": current_success_rate,
                "min_success_rate": min_success_rate,
                "allowed": allowed,
                "result": result,
            }
        )

    summary = render_summary(rows, failures, report_path, baseline_path, args.scenario)
    print(summary)

    step_summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if step_summary:
        Path(step_summary).write_text(summary + "\n")

    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
