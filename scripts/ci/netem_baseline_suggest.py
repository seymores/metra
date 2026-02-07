#!/usr/bin/env python3
"""Generate WAN netem baseline suggestions from tune-runtime and tune-lanes reports."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", required=True, help="Scenario key (latency/loss/jitter)")
    parser.add_argument("--runtime-report", required=True, help="Path to tune-runtime report")
    parser.add_argument("--lane-report", required=True, help="Path to tune-lanes report")
    parser.add_argument(
        "--throughput-floor-factor",
        type=float,
        default=0.80,
        help="Baseline floor factor applied to observed p50/p95 values",
    )
    parser.add_argument(
        "--min-success-rate",
        type=float,
        default=0.80,
        help="Suggested minimum success/completion rate",
    )
    parser.add_argument("--json-out", required=True, help="Output JSON path")
    parser.add_argument("--md-out", required=True, help="Output Markdown summary path")
    return parser.parse_args()


def load_json(path: Path) -> dict:
    return json.loads(path.read_text())


def build_runtime_suggestion(
    report: dict, floor_factor: float, min_success_rate: float
) -> dict:
    profiles = {}
    for profile in report.get("profiles", []):
        name = str(profile.get("runtime_profile", "")).strip().lower()
        if not name:
            continue
        throughput = profile.get("throughput_gbps", {})
        p50 = throughput.get("p50")
        p95 = throughput.get("p95")
        if not isinstance(p50, (int, float)) or not isinstance(p95, (int, float)):
            continue
        profiles[name] = {
            "baseline_p50_gbps": round(float(p50) * floor_factor, 3),
            "baseline_p95_gbps": round(float(p95) * floor_factor, 3),
            "min_success_rate": min_success_rate,
        }
    return {
        "default_max_regression_percent": 30.0,
        "default_min_success_rate": min_success_rate,
        "profiles": profiles,
    }


def build_lane_suggestion(report: dict, floor_factor: float, min_success_rate: float) -> dict:
    candidates = {}
    for candidate in report.get("candidates", []):
        lanes = candidate.get("lanes")
        if not isinstance(lanes, int) or lanes <= 0:
            continue
        aggregate = candidate.get("aggregate_gbps", {})
        p50 = aggregate.get("p50")
        p95 = aggregate.get("p95")
        if not isinstance(p50, (int, float)) or not isinstance(p95, (int, float)):
            continue
        candidates[str(lanes)] = {
            "baseline_p50_gbps": round(float(p50) * floor_factor, 3),
            "baseline_p95_gbps": round(float(p95) * floor_factor, 3),
            "min_success_rate": min_success_rate,
        }
    return {
        "default_max_regression_percent": 35.0,
        "default_min_success_rate": min_success_rate,
        "min_recommended_score": 0.05,
        "candidates": candidates,
    }


def markdown_summary(
    scenario: str, runtime_report_path: Path, lane_report_path: Path, output_json_path: Path
) -> str:
    return "\n".join(
        [
            "## Netem Baseline Suggestion",
            "",
            f"- Scenario: `{scenario}`",
            f"- Runtime report: `{runtime_report_path}`",
            f"- Lane report: `{lane_report_path}`",
            f"- Output JSON: `{output_json_path}`",
            "",
            "Use the generated JSON to update:",
            "- `ci/netem-runtime-baseline.json`",
            "- `ci/netem-lane-baseline.json`",
        ]
    )


def main() -> int:
    args = parse_args()
    scenario = args.scenario.strip().lower()
    runtime_report_path = Path(args.runtime_report)
    lane_report_path = Path(args.lane_report)
    json_out = Path(args.json_out)
    md_out = Path(args.md_out)

    floor_factor = max(0.1, min(1.0, float(args.throughput_floor_factor)))
    min_success_rate = max(0.0, min(1.0, float(args.min_success_rate)))

    runtime_report = load_json(runtime_report_path)
    lane_report = load_json(lane_report_path)

    suggestion = {
        "schema_version": 1,
        "scenario": scenario,
        "throughput_floor_factor": floor_factor,
        "runtime": build_runtime_suggestion(
            runtime_report, floor_factor, min_success_rate
        ),
        "lane": build_lane_suggestion(lane_report, floor_factor, min_success_rate),
    }

    if json_out.parent:
        json_out.parent.mkdir(parents=True, exist_ok=True)
    json_out.write_text(json.dumps(suggestion, indent=2) + "\n")

    summary = markdown_summary(scenario, runtime_report_path, lane_report_path, json_out)
    if md_out.parent:
        md_out.parent.mkdir(parents=True, exist_ok=True)
    md_out.write_text(summary + "\n")
    print(summary)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
