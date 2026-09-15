//! Pinned ERC-20 assets the module can vouch for independently of a downloaded list.

use crate::tokens::Token;

const WETH_MAINNET: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";

/// Sepolia and Hoodi deliberately have no pinned WETH. Several incompatible contracts use
/// that name on each testnet, so choosing one would turn an ecosystem preference into a
/// network fact. Mainnet's canonical WETH9 deployment is the only pinned row.
pub fn pinned(chain_id: u64) -> Vec<Token> {
    match chain_id {
        1 => vec![Token {
            chain_id,
            address: WETH_MAINNET.to_string(),
            name: "Wrapped Ether".to_string(),
            symbol: "WETH".to_string(),
            decimals: 18,
            logo_uri: None,
        }],
        _ => Vec::new(),
    }
}

pub fn is_pinned(chain_id: u64, address: &str) -> bool {
    pinned(chain_id).iter().any(|t| t.address.eq_ignore_ascii_case(address.trim()))
}

pub fn chains() -> &'static [u64] {
    &[1]
}

