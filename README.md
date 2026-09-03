# logos-evm-token-list-module

A Logos `core` module (Rust, rust-first cdylib) that manages **token lists** for
the Logos multi-chain EVM wallet: download, parse, and merge
[Uniswap token-lists](https://github.com/Uniswap/token-lists) plus a user "custom"
list and a **shipped offline list**, deduped per chain.

Downloads go through the same fail-closed proxy chokepoint as `eth_rpc_module`
(`src/proxy.rs`), so list fetches are proxyable/Tor-ready and refuse to fall back
to the clear when a proxy is required.

## Usable with no configuration

The module ships the Uniswap Labs Default list compiled into the binary
(`rust-lib/assets/uniswap-default.json`, 1709 tokens, 25 chains — see
`rust-lib/assets/PROVENANCE.md`). The built-in defaults are **offline only**: no list
URLs, no refresh interval, embedded list on. A consumer asks, then initializes:

```
config_status()                  -> { ok, state, source, config?, counts }
init_defaults()                  -> { ok, applied, state, source, config }
```

`state` is `unready` (context not ready — ask again), `unconfigured` (nothing has
ever been set) or `configured`. `source` is `none | default | external` and
upgrades one way only, so a user's own settings are never re-defaulted.
`init_defaults` is idempotent within and across process lifetimes: it declines
once `list_config.json` exists, and `applied: false` is an answer, not an error.

Callers must gate on `state == "unconfigured"` — this module holds one config
record, so an unconditional call would be a whole-record write.

## Three buckets, in precedence order

`custom` → `downloaded` → `embedded`. Every reply row carries the bucket that
answered it in a `source` field, so a consumer never has to guess. `refresh_now`
replaces the `downloaded` bucket only; a refresh in which every URL fails
degrades to `custom + embedded` instead of to nothing. The embedded list is
never persisted, never written, and unreachable from any network path.

## Contract (`TokenListModule`)

| method | notes |
|---|---|
| `configure(configJson)` | `{ listUrls?, proxy?, proxyRequired?, refreshSecs?, timeoutSecs?, useEmbeddedList? }` |
| `config_status()` | no network I/O; cheap on a consumer's startup path |
| `init_defaults()` | applies the offline defaults where nothing is configured |
| `refresh_now()` | the only method that fetches; nothing schedules it |
| `get_tokens(chainId)` | merged rows, each labelled `custom`/`downloaded`/`embedded` |
| `get_tokens_by_address(chainId, addressesJson)` | narrow query; 94 KB → 449 B for a two-token wallet refresh |
| `get_all_tokens()` | ~360 KB with the shipped list active |
| `add_custom_token(tokenJson)` | one user token |
| `import_custom_tokens(listJson, replace)` | bulk ingest of one Uniswap-schema document; no network → `{ ok, tokenCount }` |
| `remove_custom_token(chainId, address)` | |
| `get_custom_tokens()` | |
| `get_list_sources()` | rows are `{ url, name, tokenCount, ok, error? }` |

## Events

| event | when |
|---|---|
| `tokens_updated(chainId)` | the rows `get_tokens(chainId)` serves have changed |
| `config_changed(source)` | the stored config record moved; `source` is the new `none`/`default`/`external` |

**Every method that changes persisted state emits; every reader is silent; and a
call that changes nothing emits nothing.** So a subscriber can treat an event as
"something really moved" rather than "somebody called a setter".

Which chains moved is decided by fingerprinting the served rows either side of
the mutation, never by what the caller passed in. Re-adding a token you already
have, or re-importing the list you already stored, is silent; and a refresh in
which a list stops resolving emits for the chains it *emptied* — chains that are
gone from the store by the time the call returns, so nothing but a before/after
diff can still name them.

`config_changed` is separate because most config edits — a proxy, a refresh
interval, a list URL — move the record and not one token row, so folding it into
`tokens_updated` would make them unannounceable. It matters because this config
store is shared between apps: without the event, a second app's edit is only
reachable by polling `config_status`. The payload is advisory; re-read
`config_status()` for the record itself.

`init_defaults` therefore emits `config_changed` and normally no
`tokens_updated` at all — its defaults are exactly what an unconfigured module
already serves.

**Every config field is optional on the wire and omission preserves.** A caller
that sends `{"timeoutSecs":9}` changes the timeout and nothing else; an explicit
`"proxy": null` clears the proxy, while omitting the key keeps it. These stores
are shared between apps, so a whole-record write would let one app's silence
revoke a setting it never knew about.

`list_config.json` is stored as `{ "source": ..., "config": { ... } }`. A legacy
bare `ListConfig` file still loads, and reads as `external`: a file on disk means
somebody configured it.

## Build & test

```bash
cd rust-lib && cargo test --no-default-features   # core: parse/merge/precedence/defaults
nix build .#install                                # -> result/modules/token_list_module/
```

`rust-lib/assets/` must stay inside `rust-lib/` and stay git-tracked — the module
builder stages only that directory into the nix sandbox, and nix only sees
tracked files.
