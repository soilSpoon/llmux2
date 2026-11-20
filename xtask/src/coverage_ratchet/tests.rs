use super::*;
#[test]
fn measured_at_or_above_floor_passes() {
    assert!(check(Some(90.0), 90.0, 90.0).is_empty());
}
#[test]
fn measured_below_floor_fails() {
    let errors = check(Some(90.0), 90.0, 89.99);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("89.99"));
}
#[test]
fn lowering_the_floor_fails() {
    let errors = check(Some(90.0), 89.0, 95.0);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("lowered"));
}
#[test]
fn raising_the_floor_passes() {
    assert!(check(Some(90.0), 91.0, 91.5).is_empty());
}
#[test]
fn new_floor_without_base_passes() {
    assert!(check(None, 90.0, 90.5).is_empty());
}
#[test]
fn line_percent_is_read_from_llvm_cov_json() {
    assert_eq!(line_percent(&serde_json::json!({"data":[{"totals":{"lines":{"count":200,"covered":187,"percent":93.5}}}]})).unwrap(), 93.5);
}
#[test]
fn floor_file_parses_a_single_number() {
    assert_eq!(parse_floor("93.5\n").unwrap(), 93.5);
    assert!(parse_floor("").is_err());
}
#[test]
fn floor_must_be_a_finite_percentage() {
    for text in ["nan", "inf", "-1", "100.1"] {
        assert!(parse_floor(text).is_err(), "{text}");
    }
}
