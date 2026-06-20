# Running the token-list Module Against logoscore

`logos-evm-token-list-module` downloads, parses, and merges
[Uniswap token-lists](https://github.com/Uniswap/token-lists) plus a user
"custom" list for the Logos multi-chain EVM wallet, deduped per chain. Downloads
go through the same fail-closed proxy chokepoint as the eth-rpc module.

This doc-test drives the module through a `logoscore` daemon against a **local
static file server** (offline, reproducible): it configures a list URL,
refreshes, reads the merged tokens, adds a custom token, and finally shows the
fail-closed refusal when a proxy is required but unavailable.

**What you'll build:** This `token_list_module`, packaged as `.lgx`, installed with `lgpm`, and driven through a `logoscore` daemon against a local list server.

**What you'll learn:**

- How list URLs are configured and fetched into the module
- How downloaded lists merge with a user custom list (deduped per chain)
- How the fail-closed proxy chokepoint refuses a fetch when a proxy is required but unavailable

## Prerequisites

- **Nix** with flakes enabled (see [nixos.org](https://nixos.org/download.html)).

- **A Linux or macOS machine** with `python3` available (used to serve the local token list).

---

## Step 1: Build logoscore and lgpm

### 1.1 Build logoscore

```bash
nix build 'github:logos-co/logos-logoscore-cli#cli' --out-link ./logos
```

### 1.2 Build lgpm

```bash
nix build 'github:logos-co/logos-package-manager#cli' -o lgpm
```

---

## Step 2: Build and install the token-list module

### 2.1 Build the module's .lgx

```bash
nix build 'github:logos-co/logos-evm-token-list-module#lgx' -o token-list-lgx
```

### 2.2 Seed the capability module

```bash
mkdir -p modules
cp -RL ./logos/modules/. ./modules/

```

### 2.3 Install the .lgx with lgpm

```bash
./lgpm/bin/lgpm --modules-dir ./modules --allow-unsigned install --file token-list-lgx/*.lgx
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

## Step 4: Run the daemon and drive the module

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

### 4.4 Start the daemon

```bash
logoscore -D -m ./modules > logs.txt &
```

```bash
sleep 3
```

### 4.5 Load the module

```bash
./logos/bin/logoscore load-module token_list_module
```

### 4.6 Configure the list URL

```bash
logoscore call token_list_module configure @tl_config.json
```

### 4.7 Refresh (fetch + parse + merge)

```bash
logoscore call token_list_module refresh_now
```

### 4.8 Read merged tokens for chain 1

```bash
logoscore call token_list_module get_tokens 1
```

### 4.9 Add a custom token

```bash
logoscore call token_list_module add_custom_token @custom_token.json
```

### 4.10 The custom token now appears

```bash
./logos/bin/logoscore call token_list_module get_tokens 1
```

### 4.11 List the configured sources

```bash
./logos/bin/logoscore call token_list_module get_list_sources
```

### 4.12 Fail-closed: refuse a fetch when a proxy is required

```bash
logoscore call token_list_module configure @tl_config_fc.json
logoscore call token_list_module refresh_now
```

### 4.13 Stop the daemon and the file server

```bash
./logos/bin/logoscore stop
pkill -f "http.server 8601" 2>/dev/null || true

```

```bash
sleep 2
```

### 4.14 Confirm the daemon has stopped

```bash
./logos/bin/logoscore status || true
```
