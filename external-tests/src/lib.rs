#![cfg_attr(not(feature = "main"), no_std)]

#[cfg(feature = "main")]
pub mod instances;

pub mod macros;
pub use macros::*;

#[cfg(feature = "main")]
pub use tokio;

pub use test_macros::*;
