import unittest

import pr_policy

FILLED = """## Summary

Adds X.

## Verification

Red: `cargo test -p llmux-core x` failed with 500; green: passes.

## Reliability and compatibility

No config change.

## Workflow and learning

Repo-native fallback.
"""

SRC = ["crates/llmux-core/src/router.rs"]
SRC_DIFF = """diff --git a/crates/llmux-core/src/router.rs b/crates/llmux-core/src/router.rs
@@ -1,2 +1,2 @@
-    let x = 1;
+    let x = 2;
"""


class SectionsTest(unittest.TestCase):
    def test_filled_template_passes(self):
        self.assertEqual(pr_policy.check(FILLED, [], ""), [])

    def test_missing_section_is_reported(self):
        body = FILLED.replace("## Reliability and compatibility\n\nNo config change.\n", "")
        errors = pr_policy.check(body, [], "")
        self.assertEqual(len(errors), 1)
        self.assertIn("Reliability and compatibility", errors[0])

    def test_section_with_only_template_comment_is_empty(self):
        body = FILLED.replace(
            "Repo-native fallback.", "<!-- Name the pstack skills used -->"
        )
        errors = pr_policy.check(body, [], "")
        self.assertEqual(len(errors), 1)
        self.assertIn("Workflow and learning", errors[0])

    def test_empty_body_reports_every_section(self):
        self.assertEqual(len(pr_policy.check("", [], "")), 4)

    def test_heading_level_and_case_are_flexible(self):
        body = FILLED.replace("## Verification", "### verification")
        self.assertEqual(pr_policy.check(body, [], ""), [])


class TestRequirementTest(unittest.TestCase):
    def test_subheadings_belong_to_their_section(self):
        body = FILLED.replace(
            "Red: `cargo test -p llmux-core x` failed with 500; green: passes.",
            "### Red\n\nfailed with 500\n\n### Green\n\npasses",
        )
        self.assertEqual(pr_policy.check(body, [], ""), [])

    def test_section_with_only_empty_subheading_is_empty(self):
        body = FILLED.replace(
            "Red: `cargo test -p llmux-core x` failed with 500; green: passes.", "### Red"
        )
        errors = pr_policy.check(body, [], "")
        self.assertEqual(len(errors), 1)
        self.assertIn("Verification", errors[0])

    def test_source_change_without_test_change_fails(self):
        errors = pr_policy.check(FILLED, SRC, SRC_DIFF)
        self.assertEqual(len(errors), 1)
        self.assertIn("crates/llmux-core/src/router.rs", errors[0])

    def test_integration_test_file_satisfies(self):
        files = SRC + ["crates/llmux-server/tests/chat.rs"]
        self.assertEqual(pr_policy.check(FILLED, files, SRC_DIFF), [])

    def test_added_unit_test_in_source_satisfies(self):
        diff = SRC_DIFF + "+    #[tokio::test]\n+    async fn routes() {}\n"
        self.assertEqual(pr_policy.check(FILLED, SRC, diff), [])

    def test_changed_assertion_in_source_satisfies(self):
        diff = SRC_DIFF + "+        assert_eq!(x, 2);\n"
        self.assertEqual(pr_policy.check(FILLED, SRC, diff), [])

    def test_assertion_in_removed_line_does_not_satisfy(self):
        diff = SRC_DIFF + "-        assert_eq!(x, 1);\n"
        self.assertEqual(len(pr_policy.check(FILLED, SRC, diff)), 1)

    def test_testkit_change_counts_as_test_change(self):
        files = SRC + ["crates/llmux-testkit/src/lib.rs"]
        self.assertEqual(pr_policy.check(FILLED, files, SRC_DIFF), [])

    def test_testkit_only_change_needs_no_other_test(self):
        files = ["crates/llmux-testkit/src/lib.rs"]
        self.assertEqual(pr_policy.check(FILLED, files, ""), [])

    def test_non_source_change_needs_no_test(self):
        files = ["README.md", "Cargo.toml", "examples/llmux.example.toml"]
        self.assertEqual(pr_policy.check(FILLED, files, ""), [])

    def test_exemption_with_reason_satisfies(self):
        body = FILLED.replace(
            "Red:", "Test exemption: log message wording only.\n\nRed:"
        )
        self.assertEqual(pr_policy.check(body, SRC, SRC_DIFF), [])

    def test_exemption_without_reason_does_not_satisfy(self):
        body = FILLED.replace("Red:", "Test exemption:\n\nRed:")
        self.assertEqual(len(pr_policy.check(body, SRC, SRC_DIFF)), 1)

    def test_exemption_inside_comment_does_not_count(self):
        body = FILLED.replace("Red:", "<!-- Test exemption: placeholder -->\nRed:")
        self.assertEqual(len(pr_policy.check(body, SRC, SRC_DIFF)), 1)


if __name__ == "__main__":
    unittest.main()
