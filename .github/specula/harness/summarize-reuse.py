#!/usr/bin/env python3
"""Summarize actual component-test stdout; this is not a Specula model trace."""
import json
from pathlib import Path
import re
import sys

output = Path(sys.argv[1]).resolve()
if (output / "tests.exit").read_text().strip() != "0":
    raise SystemExit("component test failed; preserving native logs without a success summary")
matches = re.findall(
    r"NATIVE-REUSE-COMPLETED-WITHOUT-POLL accepted=(true|false) "
    r"coalesced=(true|false) notified=(true|false)",
    (output / "tests.log").read_text(),
)
if len(matches) != 1:
    raise SystemExit(f"expected exactly one actual observation; found {len(matches)}")
accepted, coalesced, notified = (value == "true" for value in matches[0])
result = {
    "schema": "native-microvm-request-reuse-observation-v1",
    "status": "passed",
    "source_sha": (output / "source.sha").read_text().strip(),
    "test_sha256": (output / "test.sha256").read_text().split()[0],
    "native_log": str(output / "tests.log"),
    "stimulus": "complete first transaction, then normal PIO write before explicit device poll",
    "observed_return_values": {
        "accepted": accepted, "coalesced": coalesced, "notified": notified,
    },
    "semantics": "actual test output, not model state, instrumentation events, or a defect claim",
}
(output / "observation.json").write_text(json.dumps(result, indent=2) + "\n")
print(json.dumps(result, indent=2))
