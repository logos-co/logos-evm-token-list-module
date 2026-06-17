//! token_list_module — download / parse / merge Uniswap token lists + a user
//! "custom" list.
//!
//! On-demand refresh fetches every configured list URL through the fail-closed
//! [`proxy`] chokepoint; results are merged with the custom list and deduped per
//! chain. The `tokens` + `proxy` cores are plain Rust, unit-tested with
//! `cargo test --no-default-features`; the Logos glue is behind the default
//! `logos_module` feature.

mod proxy;
mod tokens;

pub use tokens::{ListConfig, ListSource, Token, TokenError, TokenList};

#[cfg(feature = "logos_module")]
mod glue;
