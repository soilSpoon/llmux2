//! The Devin CLI model catalog, parsed from `devin models` output.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevinModel {
    pub id: String,
    pub free: bool,
}

/// Parses `devin models` output: model rows are indented, start with the id and
/// end with a bracketed spec such as `[262K context, Free]`.
pub fn parse_devin_models(listing: &str) -> Vec<DevinModel> {
    listing
        .lines()
        .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
        .filter_map(|line| {
            let line = line.trim();
            let id = line.split_whitespace().next()?;
            let valid = id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.');
            let spec = line.strip_suffix(']')?.rsplit_once('[')?.1;
            valid.then(|| DevinModel {
                id: id.to_owned(),
                free: spec.split(',').any(|part| part.trim() == "Free"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "Available models (3 families)\n\nSWE-2 (swe-2)\n  aliases: swe\n  swe-2-high   SWE-2 High  [262K context, Free]\n  swe-2-high-lightning   SWE-2 High Lightning  [262K context, $1 / 1M Input, new]\n\nAdaptive (adaptive)\n  adaptive  Adaptive  [$0.5 / 1M Input]\n";

    #[test]
    fn catalog_parses_ids_and_free_marker_only() {
        let models = parse_devin_models(LISTING);
        let ids: Vec<_> = models.iter().map(|m| (m.id.as_str(), m.free)).collect();
        assert_eq!(
            ids,
            [
                ("swe-2-high", true),
                ("swe-2-high-lightning", false),
                ("adaptive", false)
            ]
        );
        assert!(
            parse_devin_models("Available models\n\nSWE-2 (swe-2)\n  aliases: swe\n").is_empty()
        );
    }
}
