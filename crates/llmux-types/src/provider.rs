//! The provider kind names the gateway registers, and the strings config,
//! adapters and the importer must agree on.

/// Direct OpenAI chat completions.
pub const OPENAI: &str = "openai";
/// Any OpenAI-compatible chat-completions upstream.
pub const OPENAI_COMPAT: &str = "openai_compat";
/// OpenCode's inference gateway. Requests must reproduce the pinned CLI
/// identity shape; plain OpenAI-compatible callers are a different contract.
pub const OPENCODE: &str = "opencode";
/// OpenCode inference base URL used when a deployment omits `base_url`.
pub const OPENCODE_BASE_URL: &str = "https://opencode.ai/inference/openai/v1";
/// OpenAI Responses API.
pub const OPENAI_RESPONSES: &str = "openai_responses";
/// Anthropic Messages API.
pub const ANTHROPIC: &str = "anthropic";
pub const DEEPAI: &str = "deepai";
/// DeepAI Code's OpenAI-compatible lane at `api.deepai.org/deepai_code`.
/// `api-key` authenticated, and every request carries the lane version header
/// `x-deepai-code-theos` (a deployment header, not a credential).
pub const DEEPAI_CODE: &str = "deepai_code";
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
                OPENCODE,
                OPENAI_RESPONSES,
                ANTHROPIC,
                DEEPAI,
                DEEPAI_CODE,
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
            13
        );
        assert_eq!(ANTHROPIC_KEY_HEADER, "x-api-key");
    }
}
