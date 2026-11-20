"""Fail CI when workspace line coverage drops below the committed floor, or a PR lowers the floor."""

import json
import os
import subprocess
import sys

FLOOR_FILE = ".github/coverage-floor"
RAISE_HINT_MARGIN = 1.0


def parse_floor(text):
    return float(text.strip())


def line_percent(report):
    return float(report["data"][0]["totals"]["lines"]["percent"])


def check(base_floor, floor, measured):
    errors = []
    if base_floor is not None and floor < base_floor:
        errors.append(
            f"{FLOOR_FILE} lowered from {base_floor} to {floor}; the floor may only rise."
        )
    if measured < floor:
        errors.append(
            f"Line coverage {measured:.2f}% is below the {floor}% floor in {FLOOR_FILE}; "
            "add tests for the uncovered code."
        )
    return errors


def base_floor_at(sha):
    if not sha:
        return None
    shown = subprocess.run(
        ["git", "show", f"{sha}:{FLOOR_FILE}"], capture_output=True, text=True
    )
    return parse_floor(shown.stdout) if shown.returncode == 0 else None


def main(report_path):
    with open(report_path) as f:
        measured = line_percent(json.load(f))
    with open(FLOOR_FILE) as f:
        floor = parse_floor(f.read())
    errors = check(base_floor_at(os.environ.get("BASE_SHA")), floor, measured)
    for error in errors:
        print(f"::error::{error}")
    if not errors:
        print(f"Line coverage {measured:.2f}% meets the {floor}% floor.")
        if measured >= floor + RAISE_HINT_MARGIN:
            print(f"::notice::Coverage is {measured - floor:.2f} points above the floor; raise {FLOOR_FILE}.")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
