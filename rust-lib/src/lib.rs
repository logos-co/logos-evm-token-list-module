//! token_list_module — reusable EVM token catalogue and enabled-token store.
//!
//! On-demand refresh fetches every configured list URL through the fail-closed
//! [`proxy`] chokepoint; results are merged with the custom list and deduped per
//! chain. The `tokens` + `proxy` cores are plain Rust, unit-tested with
//! `cargo test --no-default-features`; the Logos glue is behind the default
//! `logos_module` feature.

mod proxy;
mod offered;
mod store;
mod tokens;

pub use tokens::{
    changed_chains, ChainDigests, ConfigSource, Counts, ListConfig, ListConfigWire, ListSource,
    EnabledTokenRow, OfferedError, Token, TokenError, TokenList, TokenRow, TokenSource,
};

#[cfg(feature = "logos_module")]
mod glue;
