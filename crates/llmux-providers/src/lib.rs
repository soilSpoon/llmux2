//! OpenAI-compatible adapter and static-key / OAuth credential sources.

pub mod oauth;
mod openai_compat;
mod sse;

pub use openai_compat::{OpenAiChatMetadata, OpenAiCompat, StaticKey};
