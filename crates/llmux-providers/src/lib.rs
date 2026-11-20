//! OpenAI-compatible and Anthropic Messages adapters and static-key credentials.

mod anthropic;
mod openai_compat;
mod sse;

pub use anthropic::Anthropic;
pub use openai_compat::{OpenAiChatMetadata, OpenAiCompat, StaticKey};
