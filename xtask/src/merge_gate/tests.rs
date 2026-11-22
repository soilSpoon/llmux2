use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

fn checks_from_rules(rules: Value) -> Result<Vec<String>> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "llmux-gate-rules-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let directory = root.join(".github/rulesets");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join("main.json"),
        json!({"rules": rules}).to_string(),
    )
    .unwrap();
    let result = required_checks(&root);
    fs::remove_dir_all(root).unwrap();
    result
}

#[test]
fn required_checks_rejects_empty_list() {
    let mut rules = json!([{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"check"},{"context":"coverage"}]}}]);
    assert_eq!(
        checks_from_rules(rules.clone()),
        Ok(vec!["check".into(), "coverage".into()])
    );
    rules[0]["parameters"]["required_status_checks"] = json!([]);
    assert_eq!(
        checks_from_rules(rules),
        Err("Ruleset contains no required checks".into())
    );
}

#[test]
fn required_checks_rejects_missing_list_or_rule() {
    for (rules, message) in [
        (
            json!([{"type":"required_status_checks","parameters":{}}]),
            "'required_status_checks'",
        ),
        (json!([]), "Ruleset contains no required checks"),
    ] {
        assert_eq!(checks_from_rules(rules), Err(message.into()));
    }
}

fn sha() -> String {
    "a".repeat(40)
}
fn marker(hash: &str, verdict: &str, reviewer: &str) -> String {
    format!("<!-- llmux-verify:v1 sha={hash} verdict={verdict} reviewer={reviewer} -->")
}
fn policy() -> Value {
    json!({"blocking_markers":{"codesolarlght":["Changes recommended"]},"devin_review_status_context":"Devin Review","devin_review_markers":["<!-- devin-review-summary -->","<!-- devin-review-session-triaged -->"],"verifier_logins":["devin-ai-integration","soilspoon"],"unable_to_review_markers":{"copilot":["quota exceeded"]}})
}
fn fixture() -> Value {
    json!({"pr":{"state":"open","draft":false,"base":{"ref":"main"},"mergeable":true,"head":{"sha":sha()},"body":"Author-Session: author"},"default_branch":"main","checks":[{"name":"check","status":"completed","conclusion":"success","head_sha":sha()}],"threads":[{"isResolved":true}],"statuses":[],"reviews":[{"id":1,"user":{"login":"devin-ai-integration"},"commit_id":sha(),"state":"COMMENTED","body":"<!-- devin-review-summary -->"}],"comments":[{"id":1,"user":{"login":"devin-ai-integration[bot]"},"body":marker(&sha(),"pass","independent")}]})
}
fn errors(data: &Value) -> Vec<String> {
    evaluate(data, &["check".into()], &policy()).unwrap()
}
fn status(id: u64, state: &str, context: &str, login: &str) -> Value {
    json!({"id":id,"state":state,"context":context,"creator":{"login":login}})
}
#[test]
fn devin_status_context_is_case_insensitive() {
    for context in ["devin review", "DEVIN REVIEW", "DeViN ReViEw"] {
        for state in ["success", "pending"] {
            let mut d = fixture();
            d["reviews"] = json!([]);
            d["statuses"] = json!([status(1, state, context, "devin-ai-integration[bot]")]);
            if state == "success" {
                assert!(errors(&d).is_empty());
            } else {
                assert!(errors(&d).contains(&"Devin Review status is pending".into()));
            }
        }
    }
}
#[test]
fn untrusted_status_cannot_displace_devin_status() {
    for state in ["pending", "failure", "error", "success"] {
        let mut d = fixture();
        d["statuses"] = json!([
            status(1, state, "Devin Review", "Devin-AI-Integration[bot]"),
            status(
                2,
                if state == "success" {
                    "failure"
                } else {
                    "success"
                },
                "Devin Review",
                "other-integration[bot]"
            )
        ]);
        if state == "success" {
            assert!(errors(&d).is_empty());
        } else {
            assert!(errors(&d).contains(&format!("Devin Review status is {state}")));
        }
    }
}
#[test]
fn devin_status_success_without_review() {
    let mut d = fixture();
    d["reviews"] = json!([]);
    d["statuses"] = json!([status(
        1,
        "success",
        "Devin Review",
        "devin-ai-integration[bot]"
    )]);
    assert!(errors(&d).is_empty());
}
#[test]
fn latest_devin_status_overrides_review() {
    for state in ["pending", "failure", "error"] {
        let mut d = fixture();
        d["statuses"] = json!([
            status(2, state, "Devin Review", "devin-ai-integration[bot]"),
            status(1, "success", "Devin Review", "devin-ai-integration[bot]")
        ]);
        assert!(errors(&d).contains(&format!("Devin Review status is {state}")));
    }
}
#[test]
fn untrusted_or_unrelated_status_does_not_satisfy_review() {
    for (context, login) in [
        ("Devin Review", "someone-else"),
        ("Other Review", "devin-ai-integration[bot]"),
    ] {
        let mut d = fixture();
        d["reviews"] = json!([]);
        d["statuses"] = json!([status(1, "success", context, login)]);
        assert!(!errors(&d).is_empty());
    }
}
#[test]
fn empty_reply_review_does_not_clear_blocker() {
    for body in [
        json!(""),
        json!("  \r\n"),
        Value::Null,
        json!("\u{1c}\u{1d}\u{1e}\u{1f}"),
    ] {
        let mut d = fixture();
        d["reviews"].as_array_mut().unwrap().extend([json!({"id":2,"user":{"login":"codesolarlght"},"state":"COMMENTED","body":"Changes recommended"}),json!({"id":3,"user":{"login":"codesolarlght"},"state":"COMMENTED","body":body})]);
        assert!(errors(&d).contains(&"Latest review by codesolarlght is blocking.".into()));
        d["reviews"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":4,"user":{"login":"codesolarlght"},"state":"APPROVED","body":""}));
        assert!(errors(&d).is_empty());
    }
}
#[test]
fn devin_review_requires_real_body_marker() {
    for body in ["", "Fixed in abc123", "Devin Review", "<!-- unrelated -->"] {
        let mut d = fixture();
        d["reviews"][0]["body"] = json!(body);
        assert!(
            errors(&d).contains(&"Devin Review has not submitted a review on the head SHA.".into())
        );
    }
    for body in policy()["devin_review_markers"].as_array().unwrap() {
        let mut d = fixture();
        d["reviews"][0]["body"] = body.clone();
        assert!(errors(&d).is_empty());
        d["reviews"][0]["user"]["login"] = json!("someone-else");
        assert!(!errors(&d).is_empty());
    }
}
#[test]
fn queued_rerun_without_timestamps_blocks() {
    let mut d = fixture();
    d["checks"][0]["id"] = json!(1);
    d["checks"][0]["started_at"] = json!("2026-01-01T00:00:00Z");
    let mut c = d["checks"][0].clone();
    c["id"] = json!(2);
    c["started_at"] = Value::Null;
    c["completed_at"] = Value::Null;
    c["status"] = json!("queued");
    c["conclusion"] = Value::Null;
    d["checks"].as_array_mut().unwrap().push(c);
    assert!(errors(&d).contains(&"check check still running".into()));
}
#[test]
fn cancelled_then_success() {
    let mut d = fixture();
    let mut c = d["checks"][0].clone();
    c["id"] = json!(2);
    c["conclusion"] = json!("cancelled");
    let mut next = d["checks"][0].clone();
    next["id"] = json!(3);
    d["checks"].as_array_mut().unwrap().extend([c, next]);
    assert!(errors(&d).is_empty());
}
#[test]
fn optional_check_still_running() {
    let mut d = fixture();
    let mut c = d["checks"][0].clone();
    c["name"] = json!("CodeSolar Review");
    c["status"] = json!("in_progress");
    c["conclusion"] = Value::Null;
    d["checks"].as_array_mut().unwrap().push(c);
    assert!(errors(&d).contains(&"check CodeSolar Review still running".into()));
}
#[test]
fn crlf_author_session() {
    let mut d = fixture();
    d["pr"]["body"] = json!("Author-Session: author\r\nMore text\r\n");
    assert!(errors(&d).is_empty());
}
#[test]
fn dismissed_reviews_do_not_count() {
    let mut d = fixture();
    d["reviews"][0]["state"] = json!("DISMISSED");
    assert!(!errors(&d).is_empty());
    let mut d = fixture();
    d["reviews"].as_array_mut().unwrap().push(json!({"id":2,"user":{"login":"codesolarlght"},"state":"DISMISSED","body":"Changes recommended"}));
    assert!(errors(&d).is_empty());
}
#[test]
fn latest_check_run_wins() {
    for name in ["check", "extra"] {
        for (conclusion, passes) in [
            (json!("success"), true),
            (json!("failure"), false),
            (Value::Null, false),
        ] {
            let mut d = fixture();
            let mut c = d["checks"][0].clone();
            c["name"] = json!(name);
            c["id"] = json!(2);
            c["started_at"] = json!("2026-01-01T00:00:00Z");
            c["conclusion"] = json!(if passes { "failure" } else { "success" });
            let mut latest = c.clone();
            latest["id"] = json!(3);
            latest["started_at"] = json!("2026-01-02T00:00:00Z");
            latest["status"] = json!(if conclusion.is_null() {
                "in_progress"
            } else {
                "completed"
            });
            latest["conclusion"] = conclusion;
            d["checks"].as_array_mut().unwrap().extend([c, latest]);
            assert_eq!(errors(&d).is_empty(), passes, "{name}");
        }
    }
}
#[test]
fn untrusted_verifier_is_ignored() {
    let mut d = fixture();
    d["comments"][0]["user"]["login"] = json!("outsider");
    assert!(!errors(&d).is_empty());
}
#[test]
fn pass() {
    assert!(errors(&fixture()).is_empty());
}
#[test]
fn github_bot_logins() {
    let mut d = fixture();
    d["reviews"][0]["user"]["login"] = json!("devin-ai-integration[bot]");
    assert!(errors(&d).is_empty());
    d["reviews"].as_array_mut().unwrap().push(json!({"id":2,"user":{"login":"codesolarlght[bot]"},"state":"COMMENTED","body":"Changes recommended"}));
    assert!(!errors(&d).is_empty());
}
#[test]
fn failures() {
    for name in [
        "closed",
        "draft",
        "base",
        "conflict",
        "unknown_mergeability",
        "missing_check",
        "pending",
        "failed",
        "skipped",
        "old_check",
        "other_failed",
        "thread",
        "no_devin",
        "old_devin",
        "changes_requested",
        "blocking_marker",
        "no_verification",
        "old_verification",
        "self",
        "failed_verification",
        "no_author",
        "ambiguous_author",
    ] {
        let mut d = fixture();
        match name {"closed"=>d["pr"]["state"]=json!("closed"),"draft"=>d["pr"]["draft"]=json!(true),"base"=>d["pr"]["base"]["ref"]=json!("other"),"conflict"=>d["pr"]["mergeable"]=json!(false),"unknown_mergeability"=>d["pr"]["mergeable"]=Value::Null,"missing_check"=>d["checks"]=json!([]),"pending"=>d["checks"][0]["status"]=json!("in_progress"),"failed"=>d["checks"][0]["conclusion"]=json!("failure"),"skipped"=>d["checks"][0]["conclusion"]=json!("skipped"),"old_check"=>d["checks"][0]["head_sha"]=json!("b".repeat(40)),"other_failed"=>{let mut c=d["checks"][0].clone();c["name"]=json!("extra");c["conclusion"]=json!("failure");d["checks"].as_array_mut().unwrap().push(c);},"thread"=>d["threads"].as_array_mut().unwrap().push(json!({"isResolved":false})),"no_devin"=>d["reviews"]=json!([]),"old_devin"=>d["reviews"][0]["commit_id"]=json!("b".repeat(40)),"changes_requested"=>d["reviews"][0]["state"]=json!("CHANGES_REQUESTED"),"blocking_marker"=>d["reviews"].as_array_mut().unwrap().push(json!({"id":2,"user":{"login":"codesolarlght"},"state":"COMMENTED","body":"Changes recommended"})),"no_verification"=>d["comments"]=json!([]),"old_verification"=>d["comments"][0]["body"]=json!(marker(&"b".repeat(40),"pass","independent")),"self"=>d["comments"][0]["body"]=json!(marker(&sha(),"pass","author")),"failed_verification"=>d["comments"].as_array_mut().unwrap().push(json!({"id":2,"user":{"login":"soilSpoon"},"body":marker(&sha(),"fail","independent")})),"no_author"=>d["pr"]["body"]=json!(""),"ambiguous_author"=>d["pr"]["body"]=json!("Author-Session: a\nAuthor-Session: b"),_=>unreachable!()};
        assert!(!errors(&d).is_empty(), "{name}");
    }
}
#[test]
fn latest_review_and_marker_win() {
    let mut d = fixture();
    d["reviews"].as_array_mut().unwrap().extend([json!({"id":2,"user":{"login":"codesolarlght"},"body":"Changes recommended","state":"COMMENTED"}),json!({"id":3,"user":{"login":"codesolarlght"},"body":"Fixed","state":"APPROVED"}),json!({"id":4,"user":{"login":"copilot"},"body":"quota exceeded","state":"CHANGES_REQUESTED"})]);
    d["comments"].as_array_mut().unwrap().extend([json!({"id":2,"body":marker(&sha(),"fail","independent"),"user":{"login":"soilSpoon"}}),json!({"id":3,"body":marker(&sha(),"pass","independent"),"user":{"login":"soilSpoon"}}),json!({"id":4,"body":marker(&"b".repeat(40),"fail","independent"),"user":{"login":"soilSpoon"}})]);
    assert!(errors(&d).is_empty());
}
#[test]
fn rest_pagination() {
    let mut calls = vec![];
    let values = pages("endpoint", None, &mut |args| {
        calls.push(args[0].clone());
        Ok(if calls.len() == 1 {
            json!((0..100).map(|id| json!({"id":id})).collect::<Vec<_>>())
        } else {
            json!([{"id":100}])
        })
    })
    .unwrap();
    assert_eq!(values.len(), 101);
    assert!(calls[1].contains("page=2"));
}
fn fake_fetch(changed: bool) -> Result<Value> {
    let mut pulls = 0;
    fetch(1, &mut |args| {
        Ok(match args[0].as_str() {
            "repos/{owner}/{repo}" => json!({"full_name":"owner/repo","default_branch":"main"}),
            "repos/owner/repo/pulls/1" => {
                pulls += 1;
                let mut p = fixture()["pr"].clone();
                if changed && pulls > 1 {
                    p["head"]["sha"] = json!("b".repeat(40));
                }
                p
            }
            "graphql" => {
                let second = args.contains(&"cursor=next".into());
                json!({"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[{"isResolved":!second}],"pageInfo":{"hasNextPage":!second,"endCursor":"next"}}}}}})
            }
            s if s.contains("check-runs") => json!({"check_runs":[]}),
            _ => json!([]),
        })
    })
}
#[test]
fn fetch_thread_pagination_and_head_race() {
    assert_eq!(
        fake_fetch(false).unwrap()["threads"],
        json!([{"isResolved":true},{"isResolved":false}])
    );
    assert!(fake_fetch(true).unwrap_err().contains("head changed"));
}
#[test]
fn api_error_exits_closed() {
    assert!(fetch(1, &mut |_| Err("offline".into()))
        .unwrap_err()
        .contains("offline"));
}

#[test]
fn malformed_inputs_fail_closed() {
    for key in ["checks", "threads", "reviews", "comments", "statuses"] {
        let mut d = fixture();
        d[key] = Value::Null;
        assert!(evaluate(&d, &["check".into()], &policy()).is_err(), "{key}");
    }
    for key in ["pr", "reviews", "comments"] {
        let mut d = fixture();
        if key == "pr" {
            d[key]["body"] = json!(42);
        } else {
            d[key][0]["body"] = json!(42);
        }
        assert!(evaluate(&d, &["check".into()], &policy()).is_err(), "{key}");
    }
    let mut d = fixture();
    d["reviews"][0]["user"] = json!(["bad"]);
    assert!(evaluate(&d, &["check".into()], &policy()).is_err());
    for key in [
        "blocking_markers",
        "unable_to_review_markers",
        "verifier_logins",
        "devin_review_markers",
    ] {
        let mut p = policy();
        p[key] = json!(42);
        assert!(
            evaluate(&fixture(), &["check".into()], &p).is_err(),
            "{key}"
        );
    }
}
#[test]
fn session_and_marker_syntax_is_exact() {
    for body in ["Author-Session: id._:-42\t ", "Other\nAuthor-Session: id\n"] {
        assert_eq!(authors(body).len(), 1);
    }
    for body in [
        " Author-Session: id",
        "Author-Session:  id",
        "Author-Session: ",
        "Author-Session: id extra",
        "Author-Session: café",
        "Author-Session: id\r",
    ] {
        assert!(authors(body).is_empty(), "{body:?}");
    }
    for body in [
        marker(&sha(), "Pass", "other"),
        marker(&"A".repeat(40), "pass", "other"),
        marker(&sha(), "pass", "bad session"),
        marker(&sha(), "pass", ""),
    ] {
        assert!(verification_markers(&body).is_empty(), "{body:?}");
    }
    let mut d = fixture();
    d["comments"][0]["body"] = json!(format!(
        "{}\n{}",
        marker(&sha(), "pass", "other"),
        marker(&sha(), "fail", "other")
    ));
    assert_eq!(
        errors(&d),
        vec!["Latest verification verdict for the head SHA is fail."]
    );
    d["comments"][0]["body"] = json!(marker(&sha(), "pass", "other"));
    d["comments"][0]["user"]["login"] = json!("SOILSPOON[BOT]");
    assert!(errors(&d).is_empty());
}
#[test]
fn all_failed_conclusions_and_old_optional_runs() {
    for conclusion in [
        "failure",
        "cancelled",
        "timed_out",
        "action_required",
        "startup_failure",
        "stale",
    ] {
        let mut d = fixture();
        let mut c = d["checks"][0].clone();
        c["name"] = json!("extra");
        c["conclusion"] = json!(conclusion);
        d["checks"].as_array_mut().unwrap().push(c);
        assert_eq!(
            errors(&d),
            vec![format!("Check 'extra' failed: {conclusion}.")]
        );
    }
    let mut d = fixture();
    let mut c = d["checks"][0].clone();
    c["id"] = json!(100);
    c["head_sha"] = json!("b".repeat(40));
    c["conclusion"] = json!("failure");
    d["checks"].as_array_mut().unwrap().push(c);
    assert!(errors(&d).is_empty());
    d["reviews"][0]["state"] = json!("PENDING");
    assert!(!errors(&d).is_empty());
}
#[test]
fn pagination_errors_and_graphql_errors_fail_closed() {
    assert!(pages("endpoint", None, &mut |_| Ok(json!({"unexpected":[]}))).is_err());
    assert!(pages("endpoint", Some("check_runs"), &mut |_| Ok(json!({}))).is_err());
    for errors in [
        json!([{"message":"denied"}]),
        json!({"message":"denied"}),
        json!("denied"),
    ] {
        assert_eq!(
            check_api_errors(&json!({"errors":errors})).unwrap_err(),
            "GitHub GraphQL returned errors"
        );
    }
    for errors in [Value::Null, json!([]), json!({}), json!(false), json!(0)] {
        assert!(check_api_errors(&json!({"errors":errors})).is_ok());
    }
    for cursor in [Value::Null, json!("")] {
        let error=fetch(1,&mut |args|Ok(match args[0].as_str(){"repos/{owner}/{repo}"=>json!({"full_name":"owner/repo","default_branch":"main"}),"repos/owner/repo/pulls/1"=>fixture()["pr"].clone(),"graphql"=>json!({"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":cursor}}}}}}),_=>panic!()})).unwrap_err();
        assert_eq!(error, "Missing review thread pagination cursor");
    }
}
