import json
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CONFIG = ROOT / ".devin" / "config.json"
SHA = re.compile(r"^[0-9a-f]{40}$")


def load():
    return json.loads(CONFIG.read_text())


class DevinConfigTest(unittest.TestCase):
    def test_requires_engineering_workflow_plugins(self):
        sources = {
            (entry.get("repo") or entry.get("url"), entry.get("path"))
            for entry in load()["requiredPlugins"]
        }
        self.assertIn(("https://github.com/cursor/plugins.git", "pstack"), sources)
        self.assertIn(("EveryInc/compound-engineering-plugin", None), sources)

    def test_required_plugins_are_pinned_to_commits(self):
        for entry in load()["requiredPlugins"]:
            with self.subTest(entry=entry):
                self.assertIsInstance(entry, dict)
                self.assertRegex(entry.get("sha", ""), SHA)
                self.assertNotIn("ref", entry)


if __name__ == "__main__":
    unittest.main()
