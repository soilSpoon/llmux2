//! Minimal OpenAI-compatible adapter and static-key credential source.

mod openai_compat;
mod sse;

pub use openai_compat::{OpenAiChatMetadata, OpenAiCompat, StaticKey};
