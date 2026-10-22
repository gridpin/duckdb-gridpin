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
(GH_TOKEN, GITHUB_API_URL, GITHUB_REPOSITORY, GITHUB_RUN_ID and GITHUB_RUN_ATTEMPT come from the runner).
It talks to the REST API itself rather than through the gh CLI: in run 39733253086 (2026-10-22) the CLI failed to
fetch every job log and its captured error was lost, so every failure here prints the HTTP status and body.
"""
from __future__ import annotations

import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

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
SUMMARY = re.compile(r"All tests passed \([^)]*\)|ran tests: \d+ passed, (?:\d+ failed, )?\d+ skipped")


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


# A job's log can take a moment to become available after the job completes.
LOG_ATTEMPTS = 6
LOG_PAUSE = 20


class LogUnavailable(Exception):
    pass


class _KeepRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None  # the redirect comes back as an HTTPError, and the caller follows it without the token


def _request(url: str, token: str | None) -> urllib.request.Request:
    headers = {"User-Agent": "gridpin-check-tests-ran", "X-GitHub-Api-Version": "2022-11-28"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return urllib.request.Request(url, headers=headers)


def _read(response) -> str:
    return response.read().decode("utf-8", "replace")


def _describe(error: OSError) -> str:
    if isinstance(error, urllib.error.HTTPError):
        return f"HTTP {error.code} {' '.join(_read(error).split())[:200]}".strip()
    return f"{type(error).__name__}: {error}"


def github(path: str) -> str:
    request = _request(f"{os.environ.get('GITHUB_API_URL', 'https://api.github.com')}/{path}", os.environ["GH_TOKEN"])
    with urllib.request.build_opener(_KeepRedirect).open(request, timeout=60) as response:
        return _read(response)


def fetch_log(repo: str, job_id: int) -> str:
    """The log endpoint redirects to a signed URL on a storage host: it takes no token, and the token stays with
    the API."""
    try:
        return github(f"repos/{repo}/actions/jobs/{job_id}/logs")
    except urllib.error.HTTPError as error:
        location = error.headers.get("Location")
        if error.code not in (301, 302, 303, 307, 308) or not location:
            raise
    with urllib.request.urlopen(_request(location, None), timeout=60) as response:
        return _read(response)


def job_log(repo: str, job_id: int) -> str:
    errors = []
    for attempt in range(LOG_ATTEMPTS):
        if attempt:
            time.sleep(LOG_PAUSE)
        try:
            return fetch_log(repo, job_id)
        except OSError as error:  # URLError and HTTPError included
            errors.append(_describe(error))
    raise LogUnavailable("; ".join(dict.fromkeys(errors)))


def main() -> int:
    repo, run = os.environ["GITHUB_REPOSITORY"], os.environ["GITHUB_RUN_ID"]
    attempt = os.environ.get("GITHUB_RUN_ATTEMPT", "1")
    try:
        listing = github(f"repos/{repo}/actions/runs/{run}/attempts/{attempt}/jobs?per_page=100")
    except OSError as error:
        print(f"::error title=SQL tests::cannot list the jobs of run {run}: {_describe(error)}")
        return 1
    failures = []
    for (line, arch), job in sorted(tested_jobs(json.loads(listing)["jobs"]).items()):
        log = ""
        try:
            log = job_log(repo, job["id"])
            reason = verdict(log)
        except LogUnavailable as error:
            reason = f"log not available: {error}"
        print(f"{line} {arch}: {'tests ran' if reason is None else reason} ({job['name']})")
        # Annotations can be read without signing in, unlike job logs: the outcome of every job is public.
        if reason is None:
            print(f"::notice title=SQL tests::{line} {arch}: {SUMMARY.findall(ANSI.sub('', log))[-1]}")
        else:
            print(f"::error title=SQL tests::{line} {arch}: {reason}")
            failures.append(f"{line} {arch}")
    if failures:
        print(f"SQL tests did not really run on: {', '.join(failures)}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
