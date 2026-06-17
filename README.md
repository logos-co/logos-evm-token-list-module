# logos-evm-token-list-module

A Logos `core` module (Rust, rust-first cdylib) that manages **token lists** for
the Logos multi-chain EVM wallet: download, parse, and merge
[Uniswap token-lists](https://github.com/Uniswap/token-lists) plus a user "custom"
list, deduped per chain.

Downloads go through the same fail-closed proxy chokepoint as `eth_rpc_module`
(`src/proxy.rs`), so list fetches are proxyable/Tor-ready and refuse to fall back
to the clear when a proxy is required.

## Contract (`TokenListModule`)

`configure({listUrls, proxy?, proxyRequired?, refreshSecs?})`, `refresh_now`,
`get_tokens(chainId)`, `get_all_tokens`, `add_custom_token`,
`remove_custom_token`, `get_custom_tokens`, `get_list_sources`. Event:
`tokens_updated(chainId)`. Periodic refresh is driven on demand (the wallet
backend schedules it).

## Build & test

```bash
cd rust-lib && cargo test --no-default-features   # parse/merge/custom + mock fetch + fail-closed
nix build .#install                                # -> result/modules/token_list_module/
```
