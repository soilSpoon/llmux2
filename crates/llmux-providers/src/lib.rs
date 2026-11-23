//! Upstream adapters and static-key / OAuth credential sources.

mod anthropic;
mod antigravity;
pub mod aws_quick;
mod deepai;
mod devin_acp;
mod freebuff;
mod messages;
pub mod nous_portal;
pub mod oauth;
mod openai_compat;
mod openai_responses;
mod responses;
mod sse;
mod truncate;
mod xai;

pub use anthropic::Anthropic;
pub use antigravity::Antigravity;
pub use aws_quick::{model_for as aws_quick_model_for, AwsQuick};
pub use deepai::DeepAi;
pub use devin_acp::{list_models as list_devin_models, DevinAcp, DevinAcpConfig};
pub use freebuff::Freebuff;
pub use llmux_types::{parse_devin_models, DevinModel};
pub use openai_compat::{OpenAiCompat, RequestMetadata, StaticKey};
pub use openai_responses::OpenAiResponses;
pub use truncate::{truncate_middle, truncate_middle_for_format, Truncated};
pub use xai::Xai;

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
