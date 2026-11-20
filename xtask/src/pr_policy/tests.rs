use super::*;

const FILLED: &str = "## Summary\n\nAdds X.\n\n## Verification\n\nRed: `cargo test -p llmux-core x` failed with 500; green: passes.\n\n## Reliability and compatibility\n\nNo config change.\n\n## Workflow and learning\n\nRepo-native fallback.\n";
const SRC: &[&str] = &["crates/llmux-core/src/router.rs"];
const DIFF: &str = "diff --git a/crates/llmux-core/src/router.rs b/crates/llmux-core/src/router.rs\n@@ -1,2 +1,2 @@\n-    let x = 1;\n+    let x = 2;\n";

#[test]
fn filled_template_passes() {
    assert!(check(FILLED, &[], "").is_empty());
}
#[test]
fn missing_section_is_reported() {
    let errors = check(
        &FILLED.replace(
            "## Reliability and compatibility\n\nNo config change.\n",
            "",
        ),
        &[],
        "",
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("Reliability and compatibility"));
}
#[test]
fn section_with_only_template_comment_is_empty() {
    let errors = check(
        &FILLED.replace(
            "Repo-native fallback.",
            "<!-- Name the pstack skills used -->",
        ),
        &[],
        "",
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("Workflow and learning"));
}
#[test]
fn empty_body_reports_every_section() {
    assert_eq!(check("", &[], "").len(), 4);
}
#[test]
fn heading_level_and_case_are_flexible() {
    assert!(check(
        &FILLED.replace("## Verification", "### verification"),
        &[],
        ""
    )
    .is_empty());
}
#[test]
fn subheadings_belong_to_their_section() {
    assert!(check(
        &FILLED.replace(
            "Red: `cargo test -p llmux-core x` failed with 500; green: passes.",
            "### Red\n\nfailed with 500\n\n### Green\n\npasses"
        ),
        &[],
        ""
    )
    .is_empty());
}
#[test]
fn template_section_at_deeper_level_still_ends_previous_section() {
    let errors = check(
        &FILLED.replace("Adds X.\n\n## Verification", "### Verification"),
        &[],
        "",
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("Summary"));
}
#[test]
fn section_with_only_empty_subheading_is_empty() {
    let errors = check(
        &FILLED.replace(
            "Red: `cargo test -p llmux-core x` failed with 500; green: passes.",
            "### Red",
        ),
        &[],
        "",
    );
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("Verification"));
}
#[test]
fn source_change_without_test_change_fails() {
    let errors = check(FILLED, SRC, DIFF);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains(SRC[0]));
}
#[test]
fn integration_test_file_satisfies() {
    assert!(check(FILLED, &[SRC[0], "crates/llmux-server/tests/chat.rs"], DIFF).is_empty());
}
#[test]
fn added_unit_test_in_source_satisfies() {
    assert!(check(
        FILLED,
        SRC,
        &format!("{DIFF}+    #[tokio::test]\n+    async fn routes() {{}}\n")
    )
    .is_empty());
}
#[test]
fn changed_assertion_in_source_satisfies() {
    assert!(check(FILLED, SRC, &format!("{DIFF}+        assert_eq!(x, 2);\n")).is_empty());
}
#[test]
fn assertion_in_removed_line_does_not_satisfy() {
    assert_eq!(
        check(FILLED, SRC, &format!("{DIFF}-        assert_eq!(x, 1);\n")).len(),
        1
    );
}
#[test]
fn testkit_change_counts_as_test_change() {
    assert!(check(FILLED, &[SRC[0], "crates/llmux-testkit/src/lib.rs"], DIFF).is_empty());
}
#[test]
fn testkit_only_change_needs_no_other_test() {
    assert!(check(FILLED, &["crates/llmux-testkit/src/lib.rs"], "").is_empty());
}
#[test]
fn non_source_change_needs_no_test() {
    assert!(check(
        FILLED,
        &["README.md", "Cargo.toml", "examples/llmux.example.toml"],
        ""
    )
    .is_empty());
}
#[test]
fn exemption_with_reason_satisfies() {
    assert!(check(
        &FILLED.replace("Red:", "Test exemption: log message wording only.\n\nRed:"),
        SRC,
        DIFF
    )
    .is_empty());
}
#[test]
fn exemption_without_reason_does_not_satisfy() {
    assert_eq!(
        check(
            &FILLED.replace("Red:", "Test exemption:\n\nRed:"),
            SRC,
            DIFF
        )
        .len(),
        1
    );
}
#[test]
fn exemption_inside_comment_does_not_count() {
    assert_eq!(
        check(
            &FILLED.replace("Red:", "<!-- Test exemption: placeholder -->\nRed:"),
            SRC,
            DIFF
        )
        .len(),
        1
    );
}
