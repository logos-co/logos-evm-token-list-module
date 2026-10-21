# Running the token-list Module Against logosctl

`logos-evm-token-list-module` downloads, parses, and merges
[Uniswap token-lists](https://github.com/Uniswap/token-lists) plus a user
"custom" list for the Logos multi-chain EVM wallet, deduped per chain. Downloads
go through the same fail-closed proxy chokepoint as the eth-rpc module.

This doc-test drives the module through a `logosctl` daemon against a **local
static file server** (offline, reproducible): it configures a list URL,
refreshes, reads the merged tokens, enables a token, pages the picker, and
finally shows the fail-closed refusal when a proxy is required but unavailable.

**What you'll build:** This `token_list_module`, packaged as `.lgx`, installed into a `logosctl` session, and driven through its daemon against a local list server.

**What you'll learn:**

- How list URLs are configured and fetched into the module
- How downloaded lists merge with a user custom list (deduped per chain)
- How an enabled token becomes a persisted offered-row snapshot
- How the offered-first picker supports search and pagination
- How the fail-closed proxy chokepoint refuses a fetch when a proxy is required but unavailable

## Prerequisites

- **Nix** with flakes enabled (see [nixos.org](https://nixos.org/download.html)).

- **A Linux or macOS machine** with `python3` available (used to serve the local token list).

---

## Step 1: Build logosctl

### 1.1 Build logosctl

```bash
nix build 'github:logos-co/logos-logoscore-cli#ctl' --out-link ./logos
```

---

## Step 2: Build and install the token-list module

### 2.1 Build the module's .lgx

```bash
nix build 'github:logos-co/logos-evm-token-list-module#lgx' -o token-list-lgx
```

### 2.2 Start the daemon

The session directory, `./session`, holds the daemon's config, its
installed modules and their data. `--detach` returns once the daemon
accepts commands.

```bash
logosctl daemon start --detach
```

### 2.3 Install the .lgx

A path ending in `.lgx` installs from disk, with no catalog involved. A
local build is unsigned, which the session's default policy accepts with
a warning. `-y` confirms, since there is no terminal to ask on.

```bash
logosctl install token-list-lgx/*.lgx -y
```

---

## Step 3: Serve a token list locally

### 3.1 Write a Uniswap-schema token list

```json
{
  "name": "Test List",
  "tokens": [
    { "chainId": 1, "address": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", "name": "USD Coin", "symbol": "USDC", "decimals": 6 },
    { "chainId": 1, "address": "0xdAC17F958D2ee523a2206206994597C13D831ec7", "name": "Tether", "symbol": "USDT", "decimals": 6 },
    { "chainId": 10, "address": "0x4200000000000000000000000000000000000042", "name": "Optimism", "symbol": "OP", "decimals": 18 }
  ]
}
```

### 3.2 Start a static file server

```bash
python3 -m http.server 8601 &
```

```bash
sleep 2
```

---

## Step 4: Drive the module

### 4.1 Write the configs

```json
{ "listUrls": ["http://127.0.0.1:8601/list.json"], "proxyRequired": false }
```

### 4.2 Write the fail-closed config

```json
{ "listUrls": ["http://127.0.0.1:8601/list.json"], "proxyRequired": true }
```

### 4.3 Write a custom token

```json
{ "chainId": 1, "address": "0x1111111111111111111111111111111111111111", "name": "Mine", "symbol": "MINE", "decimals": 18 }
```

### 4.4 Load the module

```bash
logosctl module load token_list_module
```

### 4.5 Configure the list URL

```bash
logosctl call token_list_module configure @tl_config.json
```

### 4.6 Refresh (fetch + parse + merge)

```bash
logosctl call token_list_module refresh_now
```

### 4.7 Read merged tokens for chain 1

```bash
logosctl call token_list_module get_tokens 1
```

### 4.8 Add a custom token

```bash
logosctl call token_list_module add_custom_token @custom_token.json
```

### 4.9 The custom token now appears

```bash
for i in 1 2 3 4 5 6; do
  out=$(./logos/bin/logosctl --config-dir ./session call token_list_module get_tokens 1 2>&1)
  case "$out" in *RPC_FAILED*) sleep 3 ;; *) break ;; esac
done
printf '%s\n' "$out"

```

### 4.10 Enable USDC from the catalogue

```bash
logosctl call token_list_module set_token_enabled 1 0xA0b8... true
```

### 4.11 Read the offered ERC-20 set

```bash
logosctl call token_list_module list_offered 1
```

### 4.12 Search the offered-first picker

```bash
logosctl call token_list_module list_available 1 0xA0b8... 0 10
```

### 4.13 List the configured sources

```bash
for i in 1 2 3 4 5 6; do
  out=$(./logos/bin/logosctl --config-dir ./session call token_list_module get_list_sources 2>&1)
  case "$out" in *RPC_FAILED*) sleep 3 ;; *) break ;; esac
done
printf '%s\n' "$out"

```

### 4.14 Fail-closed: refuse a fetch when a proxy is required

```bash
logosctl call token_list_module configure @tl_config_fc.json
logosctl call token_list_module refresh_now
```

### 4.15 Stop the daemon and the file server

```bash
logosctl daemon stop
```

### 4.16 Confirm the daemon has stopped

```bash
logosctl daemon status
```
