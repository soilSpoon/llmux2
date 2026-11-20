use std::fs;

const FLOOR_FILE: &str = ".github/coverage-floor";
const RAISE_HINT_MARGIN: f64 = 1.0;

pub fn check(base: Option<f64>, floor: f64, measured: f64) -> Vec<String> {
    let mut errors = Vec::new();
    if let Some(base) = base.filter(|base| floor < *base) {
        errors.push(format!(
            "{FLOOR_FILE} lowered from {base:?} to {floor:?}; the floor may only rise."
        ));
    }
    if measured < floor {
        errors.push(format!("Line coverage {measured:.2}% is below the {floor:?}% floor in {FLOOR_FILE}; add tests for the uncovered code."));
    }
    errors
}
pub fn parse_floor(text: &str) -> Result<f64, String> {
    let floor = text.trim().parse::<f64>().map_err(|e| e.to_string())?;
    if !(0.0..=100.0).contains(&floor) {
        return Err(format!(
            "{FLOOR_FILE} must be a percentage between 0 and 100, got {:?}",
            text.trim()
        ));
    }
    Ok(floor)
}
pub fn line_percent(report: &serde_json::Value) -> Result<f64, String> {
    let value = &report["data"][0]["totals"]["lines"]["percent"];
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| "missing or invalid line coverage percentage".into())
}

pub fn run(path: &str) -> Result<bool, String> {
    let report = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let measured = line_percent(&serde_json::from_str(&report).map_err(|e| e.to_string())?)?;
    let floor = parse_floor(&fs::read_to_string(FLOOR_FILE).map_err(|e| e.to_string())?)?;
    let base = std::env::var("BASE_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|sha| crate::git(&["show", &format!("{sha}:{FLOOR_FILE}")]).ok())
        .map(|text| parse_floor(&text))
        .transpose()?;
    let errors = check(base, floor, measured);
    crate::print_errors(&errors);
    if errors.is_empty() {
        println!("Line coverage {measured:.2}% meets the {floor:?}% floor.");
        if measured >= floor + RAISE_HINT_MARGIN {
            println!(
                "::notice::Coverage is {:.2} points above the floor; raise {FLOOR_FILE}.",
                measured - floor
            );
        }
    }
    Ok(errors.is_empty())
}

#[cfg(test)]
mod tests;
