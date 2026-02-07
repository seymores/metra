#!/usr/bin/env python3
"""Lane tuning regression gate for tune-lanes aggregate throughput and stability."""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", required=True, help="Path to tune-lanes JSON report")
    parser.add_argument("--baseline", required=True, help="Path to lane baseline JSON config")
    parser.add_argument(
        "--default-max-regression-percent",
        type=float,
        default=None,
        help="Override max allowed regression percentage for lane p50 throughput",
    )
    parser.add_argument(
        "--default-min-success-rate",
        type=float,
        default=None,
        help="Override minimum accepted success rate per candidate (0..1)",
    )
    return parser.parse_args()


def load_json(path: Path) -> dict:
    return json.loads(path.read_text())


def lane_map(report: dict) -> dict[int, dict]:
    result: dict[int, dict] = {}
    for candidate in report.get("candidates", []):
        lanes = candidate.get("lanes")
        if isinstance(lanes, int) and lanes > 0:
            result[lanes] = candidate
    return result


def candidate_p50(candidate: dict) -> float:
    aggregate = candidate.get("aggregate_gbps", {})
    p50 = aggregate.get("p50")
    if isinstance(p50, (int, float)):
        return float(p50)
    return 0.0


def candidate_success_rate(candidate: dict) -> float:
    success_rate = candidate.get("success_rate")
    if isinstance(success_rate, (int, float)):
        return max(0.0, min(1.0, float(success_rate)))
    successful_runs = candidate.get("successful_runs")
    failed_runs = candidate.get("failed_runs")
    if isinstance(successful_runs, int) and isinstance(failed_runs, int):
        total = successful_runs + failed_runs
        if total > 0:
            return successful_runs / total
    return 0.0


def candidate_recommendation_score(candidate: dict) -> float:
    value = candidate.get("recommendation_score")
    if isinstance(value, (int, float)):
        return max(0.0, float(value))
    return 0.0


def render_summary(
    rows: list[dict],
    failures: list[str],
    report_path: Path,
    baseline_path: Path,
    recommended_lanes: int | None,
    recommended_score: float | None,
) -> str:
    header = [
        "## Lane Benchmark Gate",
        "",
        f"- Report: `{report_path}`",
        f"- Baseline: `{baseline_path}`",
        f"- Recommended lanes: `{recommended_lanes}`",
        f"- Recommended score: `{recommended_score if recommended_score is not None else 'n/a'}`",
        "",
        "| Lanes | Current p50 (Gbps) | Baseline p50 (Gbps) | Threshold (Gbps) | Success Rate | Min Success Rate | Result |",
        "|---:|---:|---:|---:|---:|---:|---|",
    ]
    table = [
        "| {lanes} | {current:.3f} | {baseline:.3f} | {threshold:.3f} | {success_rate:.2f} | {min_success_rate:.2f} | {result} |".format(
            **row
        )
        for row in rows
    ]
    footer = []
    if failures:
        footer.extend(["", "### Failures"])
        footer.extend([f"- {failure}" for failure in failures])
    else:
        footer.extend(["", "All lane benchmark checks passed."])
    return "\n".join(header + table + footer)


def main() -> int:
    args = parse_args()
    report_path = Path(args.report)
    baseline_path = Path(args.baseline)

    report = load_json(report_path)
    baseline = load_json(baseline_path)

    lanes = lane_map(report)
    baseline_candidates = baseline.get("candidates", {})
    if not isinstance(baseline_candidates, dict) or not baseline_candidates:
        print("lane baseline candidates are missing", file=sys.stderr)
        return 2

    default_max_regression = (
        float(args.default_max_regression_percent)
        if args.default_max_regression_percent is not None
        else float(baseline.get("default_max_regression_percent", 40.0))
    )
    default_min_success_rate = (
        float(args.default_min_success_rate)
        if args.default_min_success_rate is not None
        else float(baseline.get("default_min_success_rate", 0.5))
    )
    min_recommended_score = float(baseline.get("min_recommended_score", 0.0))

    rows: list[dict] = []
    failures: list[str] = []
    for lane_key, lane_cfg in baseline_candidates.items():
        if not isinstance(lane_cfg, dict):
            failures.append(f"lane '{lane_key}' baseline config is not an object")
            continue
        try:
            lane_count = int(lane_key)
        except ValueError:
            failures.append(f"lane '{lane_key}' is not an integer")
            continue

        baseline_p50 = lane_cfg.get("baseline_p50_gbps")
        if not isinstance(baseline_p50, (int, float)) or float(baseline_p50) <= 0:
            failures.append(f"lane '{lane_key}' has invalid baseline_p50_gbps")
            continue
        allowed = float(lane_cfg.get("max_regression_percent", default_max_regression))
        threshold = float(baseline_p50) * max(0.0, (1.0 - allowed / 100.0))
        min_success_rate = float(
            lane_cfg.get("min_success_rate", default_min_success_rate)
        )

        candidate = lanes.get(lane_count)
        if candidate is None:
            failures.append(f"lane '{lane_key}' missing from tune-lanes report")
            rows.append(
                {
                    "lanes": lane_count,
                    "current": 0.0,
                    "baseline": float(baseline_p50),
                    "threshold": threshold,
                    "success_rate": 0.0,
                    "min_success_rate": min_success_rate,
                    "result": "FAIL",
                }
            )
            continue

        current = candidate_p50(candidate)
        success_rate = candidate_success_rate(candidate)
        result = "PASS"
        if current < threshold:
            failures.append(
                f"lane {lane_count} regressed: current={current:.3f} Gbps "
                f"< threshold={threshold:.3f} Gbps "
                f"(baseline={float(baseline_p50):.3f}, allowed={allowed:.1f}%)"
            )
            result = "FAIL"
        if success_rate < min_success_rate:
            failures.append(
                f"lane {lane_count} success rate too low: {success_rate:.2f} < {min_success_rate:.2f}"
            )
            result = "FAIL"

        rows.append(
            {
                "lanes": lane_count,
                "current": current,
                "baseline": float(baseline_p50),
                "threshold": threshold,
                "success_rate": success_rate,
                "min_success_rate": min_success_rate,
                "result": result,
            }
        )

    recommended_lanes = report.get("recommended_lanes")
    if not isinstance(recommended_lanes, int) or recommended_lanes <= 0:
        failures.append("recommended_lanes missing or invalid in tune-lanes report")
        recommended_lanes = None
    elif recommended_lanes not in lanes:
        failures.append(f"recommended_lanes={recommended_lanes} not found in candidates")

    recommended_score_value = report.get("recommendation_score")
    recommended_score = (
        float(recommended_score_value)
        if isinstance(recommended_score_value, (int, float))
        else None
    )
    if recommended_score is None:
        if recommended_lanes is not None:
            recommended_score = candidate_recommendation_score(lanes[recommended_lanes])
    if recommended_score is not None and recommended_score < min_recommended_score:
        failures.append(
            f"recommended lane score too low: {recommended_score:.3f} < {min_recommended_score:.3f}"
        )

    summary = render_summary(
        rows,
        failures,
        report_path,
        baseline_path,
        recommended_lanes,
        recommended_score,
    )
    print(summary)

    step_summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if step_summary:
        Path(step_summary).write_text(summary + "\n")

    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
