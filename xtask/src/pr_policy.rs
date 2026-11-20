use std::collections::HashMap;

const SECTIONS: &[&str] = &[
    "Summary",
    "Verification",
    "Reliability and compatibility",
    "Workflow and learning",
];

fn without_comments(body: &str) -> String {
    let mut text = String::new();
    let mut rest = body;
    while let Some(start) = rest.find("<!--") {
        let Some(end) = rest[start + 4..].find("-->") else {
            break;
        };
        text.push_str(&rest[..start]);
        rest = &rest[start + 4 + end + 3..];
    }
    text.push_str(rest);
    text
}

fn heading(line: &str) -> Option<(usize, String)> {
    let level = line.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&level) || !line[level..].starts_with(char::is_whitespace) {
        return None;
    }
    let title = line[level..].trim();
    if title.is_empty() {
        return None;
    }
    Some((level, title.trim_end_matches('#').trim().to_lowercase()))
}

fn sections(body: &str) -> HashMap<String, String> {
    let lines: Vec<_> = body.lines().collect();
    let headings: Vec<_> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| heading(line).map(|(level, title)| (i, level, title)))
        .collect();
    let mut found = HashMap::new();
    for (index, (start, level, title)) in headings.iter().enumerate() {
        let end = headings[index + 1..]
            .iter()
            .find(|(_, next_level, next_title)| {
                next_level <= level
                    || SECTIONS
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(next_title))
            })
            .map_or(lines.len(), |(i, _, _)| *i);
        let content = lines[start + 1..end]
            .iter()
            .filter(|line| heading(line).is_none() && !line.trim().chars().all(|c| c == '#'))
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        found.insert(title.clone(), content.trim().to_owned());
    }
    found
}

fn test_file(file: &str) -> bool {
    file.starts_with("crates/llmux-testkit/")
        || crate_path(file).is_some_and(|p| p.starts_with("tests/"))
}

fn crate_path(file: &str) -> Option<&str> {
    let (name, path) = file.strip_prefix("crates/")?.split_once('/')?;
    (!name.is_empty()).then_some(path)
}

fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn added_test(diff: &str) -> bool {
    diff.lines()
        .filter_map(|line| {
            line.strip_prefix('+')
                .filter(|rest| !rest.starts_with("++"))
        })
        .any(|line| {
            ["#[test", "#[tokio::test"].iter().any(|token| {
                line.match_indices(token)
                    .any(|(i, token)| !line[i + token.len()..].starts_with(word))
            }) || ["assert!", "assert_eq!", "assert_ne!", "assert_matches!"]
                .iter()
                .any(|token| {
                    line.match_indices(token)
                        .any(|(i, _)| i == 0 || !line[..i].ends_with(word))
                })
        })
}

pub fn check(body: &str, files: &[&str], diff: &str) -> Vec<String> {
    let text = without_comments(body);
    let found = sections(&text);
    let mut errors = Vec::new();
    for name in SECTIONS {
        if found.get(&name.to_lowercase()).is_none_or(String::is_empty) {
            errors.push(format!(
                "PR description section '## {name}' is missing or empty; fill in the PR template."
            ));
        }
    }
    let sources: Vec<_> = files
        .iter()
        .copied()
        .filter(|file| {
            crate_path(file).is_some_and(|path| {
                path.starts_with("src/") && path.len() > 7 && path.ends_with(".rs")
            }) && !test_file(file)
        })
        .collect();
    let exempt = text.lines().any(|line| {
        let line = line.trim_start_matches([' ', '\t']);
        let line = line
            .strip_prefix(['-', '*'])
            .unwrap_or(line)
            .trim_start_matches([' ', '\t']);
        line.get(..15)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Test exemption:"))
            && !line[15..].trim().is_empty()
    });
    if !sources.is_empty() && !files.iter().any(|f| test_file(f)) && !added_test(diff) && !exempt {
        errors.push(format!("Rust source changed without a test change: {}. Add or update a test that fails without the change, or add a 'Test exemption: <reason>' line to the PR description.", sources.join(", ")));
    }
    errors
}

pub fn run() -> Result<bool, String> {
    let base = std::env::var("BASE_SHA").map_err(|e| e.to_string())?;
    let head = std::env::var("HEAD_SHA").map_err(|e| e.to_string())?;
    let range = format!("{base}...{head}");
    let changed = crate::git(&["diff", "--name-only", "--diff-filter=AMR", &range])?;
    let diff = crate::git(&["diff", "--unified=0", &range, "--", "*.rs"])?;
    let errors = check(
        &std::env::var("PR_BODY").unwrap_or_default(),
        &changed.split_whitespace().collect::<Vec<_>>(),
        &diff,
    );
    crate::print_errors(&errors);
    if errors.is_empty() {
        println!("PR policy satisfied.");
    }
    Ok(errors.is_empty())
}

#[cfg(test)]
mod tests;
