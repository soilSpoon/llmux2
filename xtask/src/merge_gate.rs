use serde_json::{json, Value};
use std::{fs, path::Path, process::Command};

type Result<T> = std::result::Result<T, String>;
fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    value.get(key).ok_or_else(|| format!("'{key}'"))
}
fn text(value: &Value) -> Result<&str> {
    value
        .as_str()
        .ok_or_else(|| "Expected a string in GitHub response or merge policy".into())
}
fn array(value: &Value) -> Result<&Vec<Value>> {
    value
        .as_array()
        .ok_or_else(|| "Expected an array in GitHub response or merge policy".into())
}
fn optional_text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}
fn normalize(login: &str) -> String {
    let lower = login.to_lowercase();
    lower.strip_suffix("[bot]").unwrap_or(&lower).into()
}
fn reviewer_login(review: &Value) -> Result<String> {
    if !review.is_object() {
        return Err("Invalid GitHub reviewer".into());
    }
    let user = &review["user"];
    if user.is_null() {
        return Ok("[deleted]".into());
    }
    if !user.is_object() {
        return Err("Invalid GitHub reviewer".into());
    }
    Ok(normalize(text(
        user.get("login").unwrap_or(&json!("[deleted]")),
    )?))
}
fn id(value: &Value) -> Result<u64> {
    match value.get("id") {
        None => Ok(0),
        Some(v) => v.as_u64().ok_or_else(|| "Invalid GitHub item id".into()),
    }
}
fn sorted(items: &Value) -> Result<Vec<&Value>> {
    let mut values = array(items)?
        .iter()
        .map(|v| Ok((id(v)?, v)))
        .collect::<Result<Vec<_>>>()?;
    values.sort_by_key(|(id, _)| *id);
    Ok(values.into_iter().map(|(_, v)| v).collect())
}
fn contains_marker(body: &str, markers: &Value) -> Result<bool> {
    let body = body.to_lowercase();
    Ok(array(markers)?
        .iter()
        .map(text)
        .collect::<Result<Vec<_>>>()?
        .iter()
        .any(|m| body.contains(&m.to_lowercase())))
}
fn unavailable(review: &Value, policy: &Value) -> Result<bool> {
    let login = reviewer_login(review)?;
    let map = field(policy, "unable_to_review_markers")?;
    match map.get(&login) {
        Some(markers) => contains_marker(optional_text(&review["body"]), markers),
        None => Ok(false),
    }
}
fn session(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
}
fn authors(body: &str) -> Vec<String> {
    body.replace("\r\n", "\n")
        .split('\n')
        .filter_map(|line| {
            let value = line
                .strip_prefix("Author-Session: ")?
                .trim_end_matches([' ', '\t']);
            session(value).then(|| value.into())
        })
        .collect()
}
fn verification_markers(body: &str) -> Vec<(&str, &str, &str)> {
    let mut result = vec![];
    for (index, _) in body.match_indices("<!-- llmux-verify:v1 sha=") {
        let rest = &body[index + "<!-- llmux-verify:v1 sha=".len()..];
        let Some((sha, rest)) = rest.split_once(" verdict=") else {
            continue;
        };
        if sha.len() != 40
            || !sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            continue;
        }
        let Some((verdict, rest)) = rest.split_once(" reviewer=") else {
            continue;
        };
        let Some((reviewer, _)) = rest.split_once(" -->") else {
            continue;
        };
        if matches!(verdict, "pass" | "fail") && session(reviewer) {
            result.push((sha, verdict, reviewer));
        }
    }
    result
}
fn latest_insert<'a>(latest: &mut Vec<(String, &'a Value)>, key: String, value: &'a Value) {
    if let Some((_, old)) = latest.iter_mut().find(|(name, _)| name == &key) {
        *old = value;
    } else {
        latest.push((key, value));
    }
}

fn evaluate(data: &Value, required: &[String], policy: &Value) -> Result<Vec<String>> {
    for key in ["blocking_markers", "unable_to_review_markers"] {
        for markers in field(policy, key)?
            .as_object()
            .ok_or("Invalid merge policy marker map")?
            .values()
        {
            for marker in array(markers)? {
                text(marker)?;
            }
        }
    }
    for value in array(field(policy, "verifier_logins")?)? {
        text(value)?;
    }
    for gate in array(field(policy, "review_gates")?)? {
        text(field(gate, "context")?)?;
        text(field(gate, "creator_login")?)?;
        for key in ["status_skip_markers", "review_markers"] {
            for marker in array(field(gate, key)?)? {
                text(marker)?;
            }
        }
    }
    for item in std::iter::once(field(data, "pr")?)
        .chain(array(field(data, "reviews")?)?)
        .chain(array(field(data, "comments")?)?)
    {
        if let Some(body) = item.get("body").filter(|v| !v.is_null()) {
            text(body)?;
        }
    }
    let mut errors = vec![];
    let pr = field(data, "pr")?;
    let sha = text(field(field(pr, "head")?, "sha")?)?;
    if text(field(pr, "state")?)? != "open" {
        errors.push("PR is not open.".into());
    }
    if field(pr, "draft")?
        .as_bool()
        .ok_or("Invalid PR draft flag")?
    {
        errors.push("PR is a draft.".into());
    }
    if text(field(field(pr, "base")?, "ref")?)? != text(field(data, "default_branch")?)? {
        errors.push("PR base is not the default branch.".into());
    }
    if field(pr, "mergeable")? != true {
        errors.push("GitHub has not reported the PR mergeable (conflict or unknown).".into());
    }
    let mut checks = vec![];
    for check in sorted(field(data, "checks")?)? {
        if text(field(check, "head_sha")?)? == sha {
            latest_insert(&mut checks, text(field(check, "name")?)?.into(), check);
        }
    }
    for context in required {
        if !checks.iter().any(|(name, c)| {
            name == context && c["status"] == "completed" && c["conclusion"] == "success"
        }) {
            errors.push(format!(
                "Required check {} has no completed successful run on {sha}.",
                quoted(context)
            ));
        }
    }
    for &(ref name, check) in &checks {
        if text(field(check, "status")?)? != "completed" {
            errors.push(format!("check {name} still running"));
        }
        let conclusion = field(check, "conclusion")?;
        if matches!(
            conclusion.as_str(),
            Some(
                "failure"
                    | "cancelled"
                    | "timed_out"
                    | "action_required"
                    | "startup_failure"
                    | "stale"
            )
        ) {
            errors.push(format!(
                "Check {} failed: {}.",
                quoted(name),
                text(conclusion)?
            ));
        }
    }
    if array(field(data, "threads")?)?
        .iter()
        .map(|t| field(t, "isResolved"))
        .collect::<Result<Vec<_>>>()?
        .iter()
        .any(|v| **v != true)
    {
        errors.push("Unresolved review threads remain.".into());
    }
    let reviews = sorted(field(data, "reviews")?)?
        .into_iter()
        .filter(|r| !matches!(r["state"].as_str(), Some("PENDING" | "DISMISSED")))
        .collect::<Vec<_>>();
    // Each configured review gate can produce genuine head evidence three
    // ways: a review by the gate's creator containing a configured marker, a
    // latest commit status success, or a latest check-run success. A success
    // whose description or output carries a skip marker is a dead review —
    // it ran nothing — and does not count.
    let mut genuine = false;
    for gate in array(field(policy, "review_gates")?)? {
        let context = text(field(gate, "context")?)?;
        let login = normalize(text(field(gate, "creator_login")?)?);
        let skip_markers = field(gate, "status_skip_markers")?;
        let review_markers = field(gate, "review_markers")?;
        let mut marked = false;
        for review in &reviews {
            text(field(review, "state")?)?;
            if reviewer_login(review)? == login
                && review["commit_id"] == sha
                && !unavailable(review, policy)?
                && array(review_markers)?
                    .iter()
                    .map(text)
                    .collect::<Result<Vec<_>>>()?
                    .iter()
                    .any(|m| optional_text(&review["body"]).contains(m))
            {
                marked = true;
            }
        }
        if marked {
            genuine = true;
        }
        let mut statuses = vec![];
        for status in array(field(data, "statuses")?)? {
            if text(field(status, "context")?)?.to_lowercase() == context.to_lowercase()
                && reviewer_login(&json!({"user":status.get("creator")}))? == login
            {
                statuses.push((id(status)?, optional_text(&status["created_at"]), status));
            }
        }
        if let Some(status) = statuses
            .into_iter()
            .reduce(|first, next| {
                if (next.0, next.1) > (first.0, first.1) {
                    next
                } else {
                    first
                }
            })
            .map(|(_, _, s)| s)
        {
            if status["state"] == "success" {
                if !contains_marker(optional_text(&status["description"]), skip_markers)? {
                    genuine = true;
                }
            } else {
                errors.push(format!(
                    "{context} status is {}",
                    text(field(status, "state")?)?
                ));
            }
        }
        // Latest-by-id run wins across casings: the per-name dedup above keys
        // run names case-sensitively, so a newer skipped run under a
        // different casing would otherwise hide behind a stale genuine one.
        let mut run = None;
        for check in sorted(field(data, "checks")?)? {
            if text(field(check, "head_sha")?)? == sha
                && text(field(check, "name")?)?.eq_ignore_ascii_case(context)
                && normalize(optional_text(&check["app"]["slug"])) == login
            {
                run = Some(check);
            }
        }
        if let Some(check) = run {
            if check["conclusion"] == "success" {
                let output = [
                    optional_text(&check["output"]["title"]),
                    optional_text(&check["output"]["summary"]),
                    optional_text(&check["output"]["text"]),
                ]
                .join("\n");
                if !contains_marker(&output, skip_markers)? {
                    genuine = true;
                }
            }
        }
    }
    if !genuine {
        errors.push("No review gate produced a genuine review on the head SHA.".into());
    }
    let mut latest = vec![];
    for review in reviews {
        if review["state"] == "COMMENTED"
            && optional_text(&review["body"])
                .chars()
                .all(|c| c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}'))
        {
            continue;
        }
        latest_insert(&mut latest, reviewer_login(review)?, review);
    }
    for (login, review) in latest {
        if unavailable(review, policy)? {
            continue;
        }
        let markers = field(policy, "blocking_markers")?.get(&login);
        if review["state"] == "CHANGES_REQUESTED"
            || markers
                .map(|m| contains_marker(optional_text(&review["body"]), m))
                .transpose()?
                .unwrap_or(false)
        {
            errors.push(format!("Latest review by {login} is blocking."));
        }
    }
    let authors = authors(optional_text(&pr["body"]));
    if authors.len() != 1 {
        errors.push("PR body must contain exactly one Author-Session: <id> line.".into());
    }
    let allowlist = array(field(policy, "verifier_logins")?)?
        .iter()
        .map(|v| text(v).map(normalize))
        .collect::<Result<Vec<_>>>()?;
    let mut latest_marker = None;
    for comment in sorted(field(data, "comments")?)? {
        if allowlist.contains(&reviewer_login(comment)?) {
            for marker in verification_markers(optional_text(&comment["body"])) {
                if marker.0 == sha {
                    latest_marker = Some(marker);
                }
            }
        }
    }
    if let Some((_, verdict, reviewer)) = latest_marker {
        if verdict != "pass" {
            errors.push("Latest verification verdict for the head SHA is fail.".into());
        }
        if authors.len() == 1 && reviewer == authors[0] {
            errors.push("Verification reviewer is the author session (self-verification).".into());
        }
    } else {
        errors.push("No independent verification marker for the head SHA.".into());
    }
    Ok(errors)
}
fn quoted(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut result = String::from(quote);
    for c in value.chars() {
        match c {
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if c == quote => {
                result.push('\\');
                result.push(c);
            }
            c => result.push(c),
        }
    }
    result.push(quote);
    result
}
fn gh(args: &[String]) -> Result<Value> {
    let output = Command::new("gh")
        .arg("api")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        let command = std::iter::once("gh")
            .chain(std::iter::once("api"))
            .chain(args.iter().map(String::as_str))
            .map(quoted)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "Command '[{command}]' returned non-zero exit status {}.",
            output
                .status
                .code()
                .map_or_else(|| output.status.to_string(), |code| code.to_string())
        ));
    }
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    check_api_errors(&value)?;
    Ok(value)
}
fn check_api_errors(value: &Value) -> Result<()> {
    let errors = value.get("errors").is_some_and(|v| match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
    });
    if errors {
        Err("GitHub GraphQL returned errors".into())
    } else {
        Ok(())
    }
}
fn pages(
    endpoint: &str,
    key: Option<&str>,
    api: &mut impl FnMut(&[String]) -> Result<Value>,
) -> Result<Vec<Value>> {
    let mut items = vec![];
    let mut page = 1;
    loop {
        let value = api(&[format!(
            "{endpoint}{}per_page=100&page={page}",
            if endpoint.contains('?') { '&' } else { '?' }
        )])?;
        let batch = array(if let Some(key) = key {
            field(&value, key)?
        } else {
            &value
        })?;
        items.extend(batch.iter().cloned());
        if batch.len() < 100 {
            return Ok(items);
        }
        page += 1;
    }
}
fn fetch(number: i64, api: &mut impl FnMut(&[String]) -> Result<Value>) -> Result<Value> {
    let repo = api(&["repos/{owner}/{repo}".into()])?;
    let (owner, name) = text(field(&repo, "full_name")?)?
        .split_once('/')
        .ok_or("Invalid repository full_name")?;
    let endpoint = format!("repos/{owner}/{name}");
    let pr = api(&[format!("{endpoint}/pulls/{number}")])?;
    let sha = text(field(field(&pr, "head")?, "sha")?)?;
    let mut threads = vec![];
    let mut cursor = None;
    loop {
        let query="query($owner:String!,$name:String!,$number:Int!,$cursor:String) { repository(owner:$owner,name:$name) { pullRequest(number:$number) { reviewThreads(first:100,after:$cursor) { nodes { isResolved } pageInfo { hasNextPage endCursor } } } } }";
        let mut args = vec![
            "graphql".into(),
            "-f".into(),
            format!("query={query}"),
            "-f".into(),
            format!("owner={owner}"),
            "-f".into(),
            format!("name={name}"),
            "-F".into(),
            format!("number={number}"),
        ];
        if let Some(cursor) = &cursor {
            args.extend(["-f".into(), format!("cursor={cursor}")]);
        }
        let value = api(&args)?;
        check_api_errors(&value)?;
        let connection = field(
            field(field(field(&value, "data")?, "repository")?, "pullRequest")?,
            "reviewThreads",
        )?;
        threads.extend(array(field(connection, "nodes")?)?.iter().cloned());
        let info = field(connection, "pageInfo")?;
        if !field(info, "hasNextPage")?
            .as_bool()
            .ok_or("Invalid review thread pagination flag")?
        {
            break;
        }
        let next = text(field(info, "endCursor")?)
            .map_err(|_| "Missing review thread pagination cursor")?;
        if next.is_empty() || cursor.as_deref() == Some(next) {
            return Err("Missing review thread pagination cursor".into());
        }
        cursor = Some(next.to_owned());
    }
    let checks = pages(
        &format!("{endpoint}/commits/{sha}/check-runs?filter=all"),
        Some("check_runs"),
        api,
    )?;
    let reviews = pages(&format!("{endpoint}/pulls/{number}/reviews"), None, api)?;
    let statuses = pages(&format!("{endpoint}/commits/{sha}/statuses"), None, api)?;
    let comments = pages(&format!("{endpoint}/issues/{number}/comments"), None, api)?;
    let current = api(&[format!("{endpoint}/pulls/{number}")])?;
    if text(field(field(&current, "head")?, "sha")?)? != sha {
        return Err("PR head changed while fetching; rerun the gate".into());
    }
    Ok(
        json!({"pr":pr,"default_branch":field(&repo,"default_branch")?,"threads":threads,"checks":checks,"reviews":reviews,"statuses":statuses,"comments":comments}),
    )
}
fn required_checks(root: &Path) -> Result<Vec<String>> {
    let ruleset: Value = serde_json::from_slice(
        &fs::read(root.join(".github/rulesets/main.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let rule = array(field(&ruleset, "rules")?)?
        .iter()
        .find(|r| r["type"] == "required_status_checks")
        .ok_or("Ruleset contains no required checks")?;
    let checks = array(field(field(rule, "parameters")?, "required_status_checks")?)?;
    if checks.is_empty() {
        return Err("Ruleset contains no required checks".into());
    }
    checks
        .iter()
        .map(|c| text(field(c, "context")?).map(str::to_owned))
        .collect()
}
pub fn run(number: &str) -> Result<bool> {
    let result = (|| {
        let number = number
            .parse::<i64>()
            .map_err(|_| format!("argument number: invalid int value: {}", quoted(number)))?;
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let required = required_checks(root)?;
        let policy: Value = serde_json::from_slice(
            &fs::read(root.join(".github/merge-policy.json")).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let data = fetch(number, &mut gh)?;
        let errors = evaluate(&data, &required, &policy)?;
        for error in &errors {
            println!("FAIL: {error}");
        }
        if errors.is_empty() {
            println!(
                "Merge gate passed for PR #{number} at {}.",
                text(field(field(field(&data, "pr")?, "head")?, "sha")?)?
            );
        }
        Ok(errors.is_empty())
    })();
    result.map_err(|error: String| format!("FAIL: Unable to evaluate merge gate: {error}"))
}
#[cfg(test)]
mod tests;
