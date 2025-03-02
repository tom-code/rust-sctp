//! > **Wrapper to use linux kernel sctp module in rust with focus on async/tokio compatibility.**
//! ## Aspirations
//!
//! - Support enough features required for basic telco use
//!   - features required for diameter and sigtran protocols
//!   - multihoming
//!
//! ## Examples
//! Simple sctp echo server:
//! ```no_run
#![doc = include_str!("../examples/echo.rs")]
//! ````
//! 
//! Simple sctp client:
//! ```no_run
#![doc = include_str!("../examples/client.rs")]
//! ````

pub mod sctp;
pub use sctp::*;

pub mod split;
pub use split::*;

pub mod asyn;
pub use asyn::*;
