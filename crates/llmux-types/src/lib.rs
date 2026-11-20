//! Shared contracts for routing, configuration, and upstream adapters.

mod adapter;
mod error;
mod model;
mod request;

pub use adapter::*;
pub use error::*;
pub use model::*;
pub use request::*;
