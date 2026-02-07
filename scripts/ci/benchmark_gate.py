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
    return parser.parse_args()


def load_json(path: Path) -> dict:
    return json.loads(path.read_text())


def report_profile_p50_map(report: dict) -> dict[str, float]:
    profiles = report.get("profiles", [])
    result: dict[str, float] = {}
    for profile in profiles:
        name = str(profile.get("runtime_profile", "")).strip().lower()
        if not name:
            continue
        throughput = profile.get("throughput_gbps", {})
        p50 = throughput.get("p50")
        if isinstance(p50, (int, float)):
            result[name] = float(p50)
    return result


def render_summary(rows: list[dict], failures: list[str], report_path: Path, baseline_path: Path) -> str:
    header = [
        "## Benchmark Gate",
        "",
        f"- Report: `{report_path}`",
        f"- Baseline: `{baseline_path}`",
        "",
        "| Profile | Current p50 (Gbps) | Baseline p50 (Gbps) | Threshold (Gbps) | Allowed Regression | Result |",
        "|---|---:|---:|---:|---:|---|",
    ]
    table = [
        "| {profile} | {current:.3f} | {baseline:.3f} | {threshold:.3f} | {allowed:.1f}% | {result} |".format(
            **row
        )
        for row in rows
    ]
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

    profile_p50 = report_profile_p50_map(report)
    baseline_profiles = baseline.get("profiles", {})
    if not isinstance(baseline_profiles, dict) or not baseline_profiles:
        print("baseline profiles are missing", file=sys.stderr)
        return 2

    default_allowed = (
        float(args.default_max_regression_percent)
        if args.default_max_regression_percent is not None
        else float(baseline.get("default_max_regression_percent", 35.0))
    )

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
        allowed = float(profile_cfg.get("max_regression_percent", default_allowed))
        threshold = float(baseline_p50) * max(0.0, (1.0 - allowed / 100.0))

        current = profile_p50.get(profile.lower())
        if current is None:
            failures.append(f"profile '{profile}' missing from benchmark report")
            current = 0.0
            result = "FAIL"
        elif current < threshold:
            failures.append(
                f"profile '{profile}' regressed: current={current:.3f} Gbps "
                f"< threshold={threshold:.3f} Gbps "
                f"(baseline={float(baseline_p50):.3f}, allowed={allowed:.1f}%)"
            )
            result = "FAIL"
        else:
            result = "PASS"

        rows.append(
            {
                "profile": profile,
                "current": current,
                "baseline": float(baseline_p50),
                "threshold": threshold,
                "allowed": allowed,
                "result": result,
            }
        )

    summary = render_summary(rows, failures, report_path, baseline_path)
    print(summary)

    step_summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if step_summary:
        Path(step_summary).write_text(summary + "\n")

    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
