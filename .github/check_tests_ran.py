#!/usr/bin/env python3
"""Fail the pipeline unless the extension's SQL tests really ran on every platform that runs tests.

sqllogictest skips a whole test file when `require gridpin_ext` cannot load the extension, and the CI step that
ran it stays green. In run 39611788181 (2026-10-21) every platform printed "All tests were skipped" while the
pipeline was green: the extension was built but not linked into the test runner. This script reads the logs of
the jobs of the current run that run tests and fails unless each shows tests that ran, with none skipped. The
1.5 tools run the unittest binary directly ("All tests passed (N assertions in M test cases)" from Catch), the 2.0
tools run it through DuckDB's scripts/ci/run_tests.py ("ran tests: N passed, M skipped in Xs") without its
--fail-require-skip, which the workflow that calls them cannot pass.

The workflow runs it after both build jobs: python3 .github/check_tests_ran.py
(GH_TOKEN, GITHUB_REPOSITORY, GITHUB_RUN_ID and GITHUB_RUN_ATTEMPT come from the runner).
"""
from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import time

# Only native builds run tests: the extension CI tools cross-build linux_arm64 and osx_amd64 and skip their tests.
TESTED_ARCHS = ("linux_amd64", "osx_arm64", "windows_amd64")
# The `name:` of the two jobs in MainDistributionPipeline.yml; the CI tools prefix every job they run with it.
LINES = ("DuckDB 1.5", "DuckDB 2.0")
PASSED = re.compile(r"All tests passed \(([^)]*)\)")
RAN = re.compile(r"ran tests: (\d+) passed, (?:(\d+) failed, )?(\d+) skipped")
ASSERTIONS = re.compile(r"(\d+) assertions? in (\d+) test cases?")
ANSI = re.compile(r"\x1b\[[0-9;]*m")
# Both runners list the reasons of skipped tests as "require <extension>: <count>".
REQUIRE_SKIP = re.compile(r"^(?:\S+Z )?(require \w+: \d+)$", re.M)


def verdict(log: str) -> str | None:
    """None when the log shows tests that ran and passed; otherwise why it does not."""
    text = ANSI.sub("", log)
    if "All tests were skipped" in text:
        return "all tests were skipped"
    if "No tests ran" in text:
        return "no tests ran"
    require = REQUIRE_SKIP.search(text)
    if require:
        return f"skipped by {require.group(1)}"
    runs = RAN.findall(text)
    if runs:
        passed, failed, skipped = (int(n or 0) for n in runs[-1])
        if failed or skipped or not passed:
            return f"ran tests: {passed} passed, {failed} failed, {skipped} skipped"
        return None
    found = PASSED.findall(text)
    if not found:
        return "no test summary line"
    summary = found[-1]
    if "skipped" in summary:
        return f"a test was skipped: ({summary})"
    counts = ASSERTIONS.search(summary)
    if not counts or int(counts.group(1)) == 0:
        return f"no assertions ran: ({summary})"
    return None


def tested_jobs(jobs: list[dict]) -> dict[tuple[str, str], dict]:
    """(line, arch) -> job, for every line and every arch that runs tests; a missing pair is an error."""
    picked: dict[tuple[str, str], dict] = {}
    for job in jobs:
        caller, _, inner = job["name"].partition(" / ")
        line = next((name for name in LINES if name in caller), None)
        arch = next((name for name in TESTED_ARCHS if re.search(rf"\b{name}\b", inner)), None)
        if line and arch:
            if (line, arch) in picked:
                raise SystemExit(f"two jobs for {line} {arch}: {picked[line, arch]['name']!r} and {job['name']!r}")
            picked[line, arch] = job
    missing = [f"{line} {arch}" for line in LINES for arch in TESTED_ARCHS if (line, arch) not in picked]
    if missing:
        raise SystemExit(f"no job found for: {', '.join(missing)}")
    return picked


def gh_api(path: str) -> str:
    return subprocess.run(["gh", "api", path], check=True, capture_output=True, text=True).stdout


def job_log(repo: str, job_id: int) -> str:
    # A job's log can take a moment to become available after the job completes.
    for attempt in range(6):
        try:
            return gh_api(f"repos/{repo}/actions/jobs/{job_id}/logs")
        except subprocess.CalledProcessError:
            if attempt == 5:
                raise
            time.sleep(20)
    raise AssertionError("unreachable")


def main() -> int:
    repo, run = os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_RUN_ID"]
    attempt = os.environ.get("GITHUB_RUN_ATTEMPT", "1")
    jobs = json.loads(gh_api(f"repos/{repo}/actions/runs/{run}/attempts/{attempt}/jobs?per_page=100"))["jobs"]
    failures = []
    for (line, arch), job in sorted(tested_jobs(jobs).items()):
        reason = verdict(job_log(repo, job["id"]))
        print(f"{line} {arch}: {'tests ran' if reason is None else reason} ({job['name']})")
        if reason is not None:
            failures.append(f"{line} {arch}")
    if failures:
        print(f"SQL tests did not really run on: {', '.join(failures)}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
