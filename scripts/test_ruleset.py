import json
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RULESET = ROOT / ".github" / "rulesets" / "main.json"
JOB_ID = re.compile(r"^  ([A-Za-z0-9_-]+):\s*$", re.MULTILINE)


def pull_request_jobs():
    jobs = set()
    for workflow in (ROOT / ".github" / "workflows").glob("*.yml"):
        text = workflow.read_text()
        if re.search(r"^  pull_request:", text, re.MULTILINE):
            jobs.update(JOB_ID.findall(text.split("\njobs:\n", 1)[1]))
    return jobs


class MainRulesetTest(unittest.TestCase):
    def setUp(self):
        self.ruleset = json.loads(RULESET.read_text())
        self.rules = {rule["type"]: rule.get("parameters", {}) for rule in self.ruleset["rules"]}

    def test_targets_the_default_branch_and_is_active(self):
        self.assertEqual(self.ruleset["target"], "branch")
        self.assertEqual(self.ruleset["enforcement"], "active")
        self.assertEqual(self.ruleset["conditions"]["ref_name"]["include"], ["~DEFAULT_BRANCH"])
        self.assertEqual(self.ruleset.get("bypass_actors", []), [])

    def test_blocks_force_push_and_deletion(self):
        self.assertIn("non_fast_forward", self.rules)
        self.assertIn("deletion", self.rules)

    def test_changes_go_through_a_squash_merged_pull_request(self):
        self.assertEqual(self.rules["pull_request"]["allowed_merge_methods"], ["squash"])

    def test_every_pull_request_job_is_required(self):
        required = {c["context"] for c in self.rules["required_status_checks"]["required_status_checks"]}
        self.assertEqual(required, pull_request_jobs())


if __name__ == "__main__":
    unittest.main()
