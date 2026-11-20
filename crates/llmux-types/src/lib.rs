//! Shared contracts for routing, configuration, and upstream adapters.

mod adapter;
mod error;
mod model;
mod persist;
mod request;
mod upstream;

pub use adapter::*;
pub use error::*;
pub use model::*;
pub use persist::*;
pub use request::*;
pub use upstream::*;
