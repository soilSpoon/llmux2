//! OpenAI-compatible and Anthropic Messages adapters and static-key / OAuth credential sources.

mod anthropic;
mod messages;
pub mod oauth;
mod openai_compat;
mod openai_responses;
mod responses;
mod sse;
mod truncate;

pub use anthropic::Anthropic;
pub use openai_compat::{OpenAiCompat, RequestMetadata, StaticKey};
pub use openai_responses::OpenAiResponses;
pub use truncate::{truncate_middle, truncate_middle_for_format, Truncated};

fn upstream_model<'a>(
    req: &'a llmux_types::GatewayRequest,
    dep: &'a llmux_types::Deployment,
) -> &'a str {
    if dep.upstream_model.as_ref() == "*" {
        &req.model
    } else {
        &dep.upstream_model
    }
}
