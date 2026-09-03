# rust-lib/assets/uniswap-default.json

Lives inside `rust-lib/` because the module builder stages only that directory into the
nix sandbox; `include_str!` from anywhere else compiles locally and fails in nix.

The embedded default token list, so the module is useful offline with no configuration.

| | |
|---|---|
| Source | https://tokens.uniswap.org/ |
| Fetched | 2026-08-28 |
| List name | Uniswap Labs Default |
| List version | 22.14.0 (timestamp 2026-08-27T16:32:10.318Z) |
| Tokens | 1709 |
| Bytes | 667661 |
| sha256 | b48aa88509989df02f55f8e7851089df469f3b02301c8a0f7d52c8c7aaf60b3b |
| ETag (IPFS CID) | QmTmP5mkxC71ycGWJjCHRia3C6WaFEq1twpEUjWu1aJ2TC |

The served document carries no `license` field and the response no license header; the
`Uniswap/default-token-list` GitHub repository is GPL-3.0, which is a different artifact.
Shipping this endpoint's output was the maintainer's explicit decision.

Refresh by re-fetching the URL and updating every row above — the hash is the point, not decoration.
