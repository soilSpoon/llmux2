//! OpenAI-compatible and Anthropic Messages adapters and static-key / OAuth credential sources.

mod anthropic;
pub mod oauth;
mod openai_compat;
mod responses;
mod sse;

pub use anthropic::Anthropic;
pub use openai_compat::{OpenAiChatMetadata, OpenAiCompat, StaticKey};
