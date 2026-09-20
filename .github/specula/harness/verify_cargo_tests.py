#!/usr/bin/env python3
"""Reject empty/ignored-only cargo fallback runs using actual libtest output."""
import json
from pathlib import Path
import re
import sys


def executed_tests(text, name_filter):
    summaries = re.findall(
        r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; "
        r"(\d+) ignored; (\d+) measured; (\d+) filtered out;",
        text, re.MULTILINE,
    )
    if not summaries or any(status != "ok" or int(failed) for status, _, failed, *_ in summaries):
        raise ValueError("cargo fallback lacks complete successful libtest summaries")
    passed = sum(int(summary[1]) for summary in summaries)
    matching = [
        name for name in re.findall(r"^test (.+?) \.\.\. ok\s*$", text, re.MULTILINE)
        if name_filter in name
    ]
    if not matching or passed < len(matching):
        raise ValueError("cargo fallback executed no confirmed passing matching tests")
    return {
        "schema": "native-cargo-test-count-v1", "status": "passed",
        "filter": name_filter, "matching_passed": len(matching),
        "total_passed": passed, "matching_test_names": matching,
    }


if __name__ == "__main__":
    try:
        result = executed_tests(Path(sys.argv[1]).read_text(), sys.argv[2])
    except ValueError as error:
        raise SystemExit(str(error)) from error
    print(json.dumps(result, indent=2))
