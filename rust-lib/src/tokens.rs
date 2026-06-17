//! Token-list core — download / parse / merge Uniswap token lists plus a user
//! "custom" list.
//!
//! Implements the [Uniswap token-list schema](https://github.com/Uniswap/token-lists):
//! a JSON document with a `tokens` array of `{ chainId, address, name, symbol,
//! decimals, logoURI }`. Multiple list URLs are fetched (through the fail-closed
//! [`crate::proxy`] chokepoint) and merged with the custom list; results are
//! deduped per chain by address. Pure (no Logos deps), unit-tested with
//! `cargo test`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::proxy::{build_client, ProxyConfig};

/// A normalized token entry (our internal + persisted form).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Token {
    #[serde(rename = "chainId")]
    pub chain_id: u64,
    pub address: String,
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
    #[serde(rename = "logoURI", default, skip_serializing_if = "Option::is_none")]
    pub logo_uri: Option<String>,
}

/// A token entry as it appears in a Uniswap token-list document (lenient).
#[derive(Deserialize)]
struct RawToken {
    #[serde(rename = "chainId")]
    chain_id: u64,
    address: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    symbol: String,
    #[serde(default)]
    decimals: u8,
    #[serde(rename = "logoURI", default)]
    logo_uri: Option<String>,
}

#[derive(Deserialize)]
struct TokenListDoc {
    #[serde(default)]
    name: String,
    #[serde(default)]
    tokens: Vec<RawToken>,
}

/// Per-list-URL fetch status, surfaced to the UI.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ListSource {
    pub url: String,
    pub name: String,
    pub token_count: usize,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListConfig {
    #[serde(default)]
    pub list_urls: Vec<String>,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub proxy_required: bool,
    #[serde(default)]
    pub refresh_secs: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 {
    30
}

#[derive(Debug)]
pub enum TokenError {
    Proxy(String),
    Http(String),
    Parse(String),
    Io(String),
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Proxy(e) => write!(f, "proxy: {e}"),
            TokenError::Http(e) => write!(f, "http: {e}"),
            TokenError::Parse(e) => write!(f, "parse: {e}"),
            TokenError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

fn norm(addr: &str) -> String {
    addr.trim().to_lowercase()
}

/// Token list manager: configuration, downloaded (merged) tokens, custom list.
pub struct TokenList {
    config: ListConfig,
    sources: Vec<ListSource>,
    downloaded: Vec<Token>,
    custom: Vec<Token>,
    dir: Option<PathBuf>,
}

impl TokenList {
    pub fn new() -> Self {
        Self { config: ListConfig::default(), sources: Vec::new(), downloaded: Vec::new(), custom: Vec::new(), dir: None }
    }

    /// Open a list backed by a persistence directory, loading any cached state.
    pub fn with_dir(dir: PathBuf) -> Self {
        let mut s = Self::new();
        s.dir = Some(dir);
        s.load();
        s
    }

    fn path(&self, name: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(name))
    }

    fn read_json<T: for<'de> Deserialize<'de>>(&self, name: &str) -> Option<T> {
        let p = self.path(name)?;
        let txt = std::fs::read_to_string(p).ok()?;
        serde_json::from_str(&txt).ok()
    }

    fn write_json<T: Serialize>(&self, name: &str, value: &T) {
        if let Some(p) = self.path(name) {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(txt) = serde_json::to_string_pretty(value) {
                let _ = std::fs::write(p, txt);
            }
        }
    }

    fn load(&mut self) {
        if let Some(c) = self.read_json::<ListConfig>("list_config.json") {
            self.config = c;
        }
        if let Some(t) = self.read_json::<Vec<Token>>("token_cache.json") {
            self.downloaded = t;
        }
        if let Some(t) = self.read_json::<Vec<Token>>("custom_tokens.json") {
            self.custom = t;
        }
    }

    pub fn configure(&mut self, config: ListConfig) {
        self.config = config;
        self.write_json("list_config.json", &self.config);
    }

    pub fn config(&self) -> &ListConfig {
        &self.config
    }

    pub fn refresh_secs(&self) -> u64 {
        self.config.refresh_secs
    }

    /// Fetch all configured list URLs through the fail-closed proxy, parse, and
    /// replace the downloaded set. Returns the total token count.
    pub fn refresh_now(&mut self) -> Result<usize, TokenError> {
        let pc = ProxyConfig::new(self.config.proxy.clone(), self.config.proxy_required, self.config.timeout_secs);
        let client = build_client(&pc).map_err(|e| TokenError::Proxy(e.to_string()))?;

        let mut merged: Vec<Token> = Vec::new();
        let mut sources: Vec<ListSource> = Vec::new();
        for url in &self.config.list_urls {
            match fetch_list(&client, url) {
                Ok((name, tokens)) => {
                    sources.push(ListSource { url: url.clone(), name, token_count: tokens.len(), ok: true, error: None });
                    merged.extend(tokens);
                }
                Err(e) => {
                    sources.push(ListSource { url: url.clone(), name: String::new(), token_count: 0, ok: false, error: Some(e.to_string()) });
                }
            }
        }
        self.downloaded = merged;
        self.sources = sources;
        self.write_json("token_cache.json", &self.downloaded);
        Ok(self.downloaded.len())
    }

    /// Merge downloaded + custom for `chain_id`, deduped by address (custom wins).
    pub fn get_tokens(&self, chain_id: u64) -> Vec<Token> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        // custom first so it takes precedence on dedup
        for t in self.custom.iter().chain(self.downloaded.iter()) {
            if t.chain_id != chain_id {
                continue;
            }
            if seen.insert(norm(&t.address)) {
                out.push(t.clone());
            }
        }
        out
    }

    pub fn get_all_tokens(&self) -> Vec<Token> {
        let mut chains: Vec<u64> =
            self.custom.iter().chain(self.downloaded.iter()).map(|t| t.chain_id).collect();
        chains.sort_unstable();
        chains.dedup();
        chains.into_iter().flat_map(|c| self.get_tokens(c)).collect()
    }

    pub fn add_custom_token(&mut self, token: Token) {
        let key = (token.chain_id, norm(&token.address));
        self.custom.retain(|t| (t.chain_id, norm(&t.address)) != key);
        self.custom.push(token);
        self.write_json("custom_tokens.json", &self.custom);
    }

    pub fn remove_custom_token(&mut self, chain_id: u64, address: &str) -> bool {
        let before = self.custom.len();
        let key = (chain_id, norm(address));
        self.custom.retain(|t| (t.chain_id, norm(&t.address)) != key);
        let changed = self.custom.len() != before;
        if changed {
            self.write_json("custom_tokens.json", &self.custom);
        }
        changed
    }

    pub fn get_custom_tokens(&self) -> &[Token] {
        &self.custom
    }

    /// Distinct chain ids present across downloaded + custom tokens (sorted).
    pub fn chains(&self) -> Vec<u64> {
        let mut c: Vec<u64> = self.custom.iter().chain(self.downloaded.iter()).map(|t| t.chain_id).collect();
        c.sort_unstable();
        c.dedup();
        c
    }

    pub fn get_list_sources(&self) -> &[ListSource] {
        &self.sources
    }
}

impl Default for TokenList {
    fn default() -> Self {
        Self::new()
    }
}

fn fetch_list(client: &reqwest::blocking::Client, url: &str) -> Result<(String, Vec<Token>), TokenError> {
    let resp = client.get(url).send().map_err(|e| TokenError::Http(e.to_string()))?;
    let text = resp.text().map_err(|e| TokenError::Http(e.to_string()))?;
    parse_list(&text)
}

fn parse_list(text: &str) -> Result<(String, Vec<Token>), TokenError> {
    let doc: TokenListDoc = serde_json::from_str(text).map_err(|e| TokenError::Parse(e.to_string()))?;
    let tokens = doc
        .tokens
        .into_iter()
        .map(|t| Token {
            chain_id: t.chain_id,
            address: t.address,
            name: t.name,
            symbol: t.symbol,
            decimals: t.decimals,
            logo_uri: t.logo_uri,
        })
        .collect();
    Ok((doc.name, tokens))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const LIST: &str = r#"{
      "name": "Test List",
      "tokens": [
        {"chainId":1,"address":"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48","name":"USD Coin","symbol":"USDC","decimals":6,"logoURI":"http://x/usdc.png"},
        {"chainId":1,"address":"0xdAC17F958D2ee523a2206206994597C13D831ec7","name":"Tether","symbol":"USDT","decimals":6},
        {"chainId":10,"address":"0x4200000000000000000000000000000000000042","name":"Optimism","symbol":"OP","decimals":18}
      ]
    }"#;

    #[test]
    fn parses_uniswap_schema() {
        let (name, tokens) = parse_list(LIST).unwrap();
        assert_eq!(name, "Test List");
        assert_eq!(tokens.len(), 3);
        let usdc = &tokens[0];
        assert_eq!(usdc.symbol, "USDC");
        assert_eq!(usdc.decimals, 6);
        assert_eq!(usdc.chain_id, 1);
    }

    #[test]
    fn custom_add_remove_persists() {
        let dir = tempfile::tempdir().unwrap();
        let mine = Token {
            chain_id: 1,
            address: "0x1111111111111111111111111111111111111111".into(),
            name: "Mine".into(),
            symbol: "MINE".into(),
            decimals: 18,
            logo_uri: None,
        };
        {
            let mut tl = TokenList::with_dir(dir.path().to_path_buf());
            tl.add_custom_token(mine.clone());
            assert_eq!(tl.get_custom_tokens().len(), 1);
        }
        // reopen — custom token persisted
        let mut tl2 = TokenList::with_dir(dir.path().to_path_buf());
        assert_eq!(tl2.get_custom_tokens(), &[mine]);
        assert!(tl2.remove_custom_token(1, "0x1111111111111111111111111111111111111111"));
        assert_eq!(tl2.get_custom_tokens().len(), 0);
    }

    #[test]
    fn get_tokens_merges_and_dedups_custom_wins() {
        let mut tl = TokenList::new();
        // pretend USDC was downloaded
        let (_n, downloaded) = parse_list(LIST).unwrap();
        tl.downloaded = downloaded;
        // user overrides USDC name via a custom entry (same address, different case)
        tl.custom.push(Token {
            chain_id: 1,
            address: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
            name: "My USDC".into(),
            symbol: "USDC".into(),
            decimals: 6,
            logo_uri: None,
        });
        let chain1 = tl.get_tokens(1);
        assert_eq!(chain1.len(), 2); // USDC (deduped) + USDT
        let usdc = chain1.iter().find(|t| t.symbol == "USDC").unwrap();
        assert_eq!(usdc.name, "My USDC"); // custom won
        assert_eq!(tl.get_tokens(10).len(), 1); // OP
    }

    fn mock_server(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}/list.json")
    }

    #[test]
    fn refresh_fetches_and_serves_tokens() {
        let url = mock_server(LIST);
        let dir = tempfile::tempdir().unwrap();
        let mut tl = TokenList::with_dir(dir.path().to_path_buf());
        tl.configure(ListConfig { list_urls: vec![url], proxy: None, proxy_required: false, refresh_secs: 0, timeout_secs: 5 });
        let n = tl.refresh_now().unwrap();
        assert_eq!(n, 3);
        assert_eq!(tl.get_tokens(1).len(), 2);
        assert!(tl.get_list_sources()[0].ok);
    }

    #[test]
    fn refresh_fail_closed_without_proxy() {
        let mut tl = TokenList::new();
        tl.configure(ListConfig {
            list_urls: vec!["https://tokens.example/list.json".into()],
            proxy: None,
            proxy_required: true,
            refresh_secs: 0,
            timeout_secs: 5,
        });
        assert!(matches!(tl.refresh_now(), Err(TokenError::Proxy(_))));
    }
}
