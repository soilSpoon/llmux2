//! OpenAI-compatible and Anthropic Messages adapters and static-key / OAuth credential sources.

mod anthropic;
pub mod oauth;
mod openai_compat;
mod openai_responses;
mod responses;
mod sse;
mod truncate;

pub use anthropic::Anthropic;
pub use openai_compat::{OpenAiChatMetadata, OpenAiCompat, RequestMetadata, StaticKey};
pub use openai_responses::OpenAiResponses;
pub use truncate::{truncate_middle, Truncated};
