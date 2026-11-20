"""Fail a pull request that skips the llmux2 PR template or changes Rust source without a test."""

import os
import re
import subprocess
import sys

SECTIONS = (
    "Summary",
    "Verification",
    "Reliability and compatibility",
    "Workflow and learning",
)
COMMENT = re.compile(r"<!--.*?-->", re.DOTALL)
HEADING = re.compile(r"^(#{1,6})\s+(.+?)\s*#*\s*$", re.MULTILINE)
SOURCE = re.compile(r"^crates/[^/]+/src/.+\.rs$")
TEST_FILE = re.compile(r"^(crates/[^/]+/tests/|crates/llmux-testkit/)")
ADDED_TEST_LINE = re.compile(
    r"^\+(?!\+\+).*(#\[(tokio::)?test\b|\bassert(_eq|_ne|_matches)?!)", re.MULTILINE
)
EXEMPTION = re.compile(r"^[ \t]*[-*]?[ \t]*Test exemption:[ \t]*\S.*$", re.IGNORECASE | re.MULTILINE)


def is_section(heading):
    return heading.group(2).strip().lower() in {name.lower() for name in SECTIONS}


def sections(body):
    text = COMMENT.sub("", body)
    matches = list(HEADING.finditer(text))
    found = {}
    for i, match in enumerate(matches):
        level = len(match.group(1))
        end = next(
            (m.start() for m in matches[i + 1 :] if len(m.group(1)) <= level or is_section(m)),
            len(text),
        )
        found[match.group(2).strip().lower()] = HEADING.sub("", text[match.end() : end]).strip()
    return found


def check(body, changed_files, diff):
    errors = []
    found = sections(body or "")
    for name in SECTIONS:
        if not found.get(name.lower()):
            errors.append(
                f"PR description section '## {name}' is missing or empty; fill in the PR template."
            )

    sources = [f for f in changed_files if SOURCE.match(f) and not TEST_FILE.match(f)]
    has_test = any(TEST_FILE.match(f) for f in changed_files) or ADDED_TEST_LINE.search(diff)
    exempt = EXEMPTION.search(COMMENT.sub("", body or ""))
    if sources and not has_test and not exempt:
        errors.append(
            "Rust source changed without a test change: "
            + ", ".join(sources)
            + ". Add or update a test that fails without the change, or add a "
            "'Test exemption: <reason>' line to the PR description."
        )
    return errors


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def main():
    base, head = os.environ["BASE_SHA"], os.environ["HEAD_SHA"]
    changed = git("diff", "--name-only", "--diff-filter=AMR", f"{base}...{head}").split()
    diff = git("diff", "--unified=0", f"{base}...{head}", "--", "*.rs")
    errors = check(os.environ.get("PR_BODY", ""), changed, diff)
    for error in errors:
        print(f"::error::{error}")
    if not errors:
        print("PR policy satisfied.")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
