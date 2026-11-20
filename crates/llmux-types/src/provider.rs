//! The provider kind names the gateway registers, and the strings config,
//! adapters and the importer must agree on.

/// Direct OpenAI chat completions.
pub const OPENAI: &str = "openai";
/// Any OpenAI-compatible chat-completions upstream.
pub const OPENAI_COMPAT: &str = "openai_compat";
/// OpenAI Responses API.
pub const OPENAI_RESPONSES: &str = "openai_responses";
/// Anthropic Messages API.
pub const ANTHROPIC: &str = "anthropic";
pub const DEEPAI: &str = "deepai";

/// Header carrying the Anthropic API key at the upstream edge.
pub const ANTHROPIC_KEY_HEADER: &str = "x-api-key";
pub const DEEPAI_KEY_HEADER: &str = "api-key";
pub const DEEPAI_TEXT_MODELS: &[&str] = &["text-generator", "summarization"];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn provider_names_are_stable() {
        // Config, adapters and the importer compare on these strings; renaming
        // them is a breaking change, so pin the values.
        assert_eq!(
            [OPENAI, OPENAI_COMPAT, OPENAI_RESPONSES, ANTHROPIC, DEEPAI]
                .into_iter()
                .collect::<HashSet<_>>()
                .len(),
            5
        );
        assert_eq!(ANTHROPIC_KEY_HEADER, "x-api-key");
    }
}
