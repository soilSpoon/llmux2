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
/// Freebuff through the Codebuff Chat Completions API.
pub const FREEBUFF: &str = "freebuff";

/// xAI OAuth at api.x.ai. Chat and Responses, selected per model.
pub const XAI: &str = "xai";
/// Antigravity Cloud Code `streamGenerateContent` with Google OAuth.
pub const ANTIGRAVITY: &str = "antigravity";
/// Devin CLI bridge: one `devin acp` child per request, spoken to over stdio.
pub const DEVIN_ACP: &str = "devin_acp";
/// Placeholder endpoint for deployments that have no network upstream.
pub const DEVIN_ACP_BASE_URL: &str = "http://127.0.0.1/devin-acp";
/// Agent CLI bridge: serves chat requests by driving a configured agent CLI
/// child over ACP JSON-RPC on stdio. The credential declares the command.
pub const ACP_AGENT: &str = "acp_agent";
/// Placeholder endpoint for deployments that have no network upstream.
pub const ACP_AGENT_BASE_URL: &str = "http://127.0.0.1/acp-agent";
/// AWS Quick through the magpie gateway the desktop plugin exposes on loopback.
pub const AWS_QUICK: &str = "aws_quick";
/// Where the plugin's gateway listens, for deployments that omit `base_url`.
pub const AWS_QUICK_BASE_URL: &str = "http://127.0.0.1:3425";

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
            [
                OPENAI,
                OPENAI_COMPAT,
                OPENAI_RESPONSES,
                ANTHROPIC,
                DEEPAI,
                FREEBUFF,
                XAI,
                ANTIGRAVITY,
                DEVIN_ACP,
                ACP_AGENT,
                AWS_QUICK,
            ]
            .into_iter()
            .collect::<HashSet<_>>()
            .len(),
            11
        );
        assert_eq!(ANTHROPIC_KEY_HEADER, "x-api-key");
    }
}
