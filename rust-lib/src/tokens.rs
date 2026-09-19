//! Token-list core — download / parse / merge Uniswap token lists plus a user
//! "custom" list and a shipped offline list compiled into the binary.
//!
//! Implements the [Uniswap token-list schema](https://github.com/Uniswap/token-lists):
//! a JSON document with a `tokens` array of `{ chainId, address, name, symbol,
//! decimals, logoURI }`. Multiple list URLs are fetched (through the fail-closed
//! [`crate::proxy`] chokepoint) and merged with the custom and embedded lists;
//! results are deduped per chain by address. Pure (no Logos deps), unit-tested
//! with `cargo test`.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Deserializer, Serialize};

use crate::offered;
use crate::proxy::{build_client, ProxyConfig};
use crate::store;

/// The shipped offline list. Embedded at compile time, not read from disk: a
/// cdylib core module has a data directory and no resource directory.
const EMBEDDED_LIST: &str = include_str!("../assets/uniswap-default.json");

/// Parsed once per process and shared by every [`TokenList`].
fn embedded_tokens() -> &'static [Token] {
    static CACHE: OnceLock<Vec<Token>> = OnceLock::new();
    CACHE.get_or_init(|| parse_list(EMBEDDED_LIST).map(|(_n, t)| t).unwrap_or_default())
}

/// A normalized token entry (our internal + persisted form).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
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

/// Which bucket answered. Lives on reply rows only — never on a persisted
/// [`Token`], where it would be a second truth that can disagree with the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenSource {
    Builtin,
    Enabled,
    Custom,
    Downloaded,
    Embedded,
}

/// One reply row: the token plus the bucket it came from.
#[derive(Clone, Debug, Serialize, PartialEq, Eq, Hash)]
pub struct TokenRow {
    #[serde(flatten)]
    pub token: Token,
    pub source: TokenSource,
    #[serde(default, skip_serializing_if = "is_false")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub builtin: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct EnabledTokenRow {
    #[serde(flatten)]
    pub token: Token,
    pub resolved: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OfferedError {
    BadAddress,
    UnknownToken,
    Builtin,
    Io,
}

impl OfferedError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadAddress => "bad_address",
            Self::UnknownToken => "unknown_token",
            Self::Builtin => "builtin",
            Self::Io => "io",
        }
    }
}

impl std::fmt::Display for OfferedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadAddress => write!(f, "address is not a 20-byte hex token address"),
            Self::UnknownToken => write!(f, "the catalogue does not describe this token"),
            Self::Builtin => write!(f, "a built-in token cannot be disabled"),
            Self::Io => write!(f, "could not save the enabled token set"),
        }
    }
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
#[serde(rename_all = "camelCase")]
pub struct ListSource {
    pub url: String,
    pub name: String,
    pub token_count: usize,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Who wrote the stored config. Upgrades one way only: none → default →
/// external, so a user's own settings can never be re-defaulted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigSource {
    #[default]
    None,
    Default,
    External,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
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
    /// The shipped offline floor. On by default so the module is useful with
    /// no configuration and no network.
    #[serde(default = "default_use_embedded")]
    pub use_embedded_list: bool,
}

fn default_timeout() -> u64 {
    // Under the 20 s call deadline a caller of refresh_now waits on, as eth_rpc's is.
    8
}

fn default_use_embedded() -> bool {
    true
}

/// Hand-written so `ListConfig::default()` and deserializing `{}` agree; a
/// derived Default gave `timeoutSecs: 0` where serde gives the default.
impl Default for ListConfig {
    fn default() -> Self {
        Self {
            list_urls: Vec::new(),
            proxy: None,
            proxy_required: false,
            refresh_secs: 0,
            timeout_secs: default_timeout(),
            use_embedded_list: default_use_embedded(),
        }
    }
}

/// Missing field → `None`; explicit `null` → `Some(None)`. Plain
/// `Option<Option<T>>` collapses both to `None`.
fn double_option<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

/// Wire form of [`ListConfig`]: every field optional, resolved against what is
/// already stored. Omission preserves — a caller that sends one key changes one
/// key, so a sibling app's silence cannot revoke a setting it never knew about.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListConfigWire {
    pub list_urls: Option<Vec<String>>,
    /// Absent keeps the proxy; an explicit `null` clears it.
    #[serde(default, deserialize_with = "double_option")]
    pub proxy: Option<Option<String>>,
    pub proxy_required: Option<bool>,
    pub refresh_secs: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub use_embedded_list: Option<bool>,
}

impl ListConfigWire {
    pub fn resolve(self, existing: Option<&ListConfig>) -> ListConfig {
        let base = existing.cloned().unwrap_or_default();
        ListConfig {
            list_urls: self.list_urls.unwrap_or(base.list_urls),
            proxy: match self.proxy {
                Some(p) => p,
                None => base.proxy,
            },
            proxy_required: self.proxy_required.unwrap_or(base.proxy_required),
            refresh_secs: self.refresh_secs.unwrap_or(base.refresh_secs),
            timeout_secs: self.timeout_secs.unwrap_or(base.timeout_secs),
            use_embedded_list: self.use_embedded_list.unwrap_or(base.use_embedded_list),
        }
    }
}

/// The persisted `list_config.json` envelope: the config plus who wrote it.
#[derive(Serialize, Deserialize)]
struct ConfigRecord {
    source: ConfigSource,
    config: ListConfig,
}

#[derive(Debug)]
pub enum TokenError {
    Proxy(String),
    Http(String),
    Parse(String),
    Io(String),
    /// The lists or proxy policy changed while a refresh was fetching.
    Superseded,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Proxy(e) => write!(f, "proxy: {e}"),
            TokenError::Http(e) => write!(f, "http: {e}"),
            TokenError::Parse(e) => write!(f, "parse: {e}"),
            TokenError::Io(e) => write!(f, "io: {e}"),
            TokenError::Superseded => {
                write!(f, "the list configuration changed during the fetch; refresh again")
            }
        }
    }
}

fn norm(addr: &str) -> String {
    addr.trim().to_lowercase()
}

/// A fingerprint of the served rows, per chain. Hashes are comparable within a
/// process only — the point is the diff, never the value.
pub type ChainDigests = BTreeMap<u64, u64>;

/// Chains whose served rows differ between two snapshots, including any chain
/// that appeared or vanished. An empty result means nothing moved.
pub fn changed_chains(before: &ChainDigests, after: &ChainDigests) -> Vec<u64> {
    let mut c: Vec<u64> = before.keys().chain(after.keys()).copied().collect();
    c.sort_unstable();
    c.dedup();
    c.retain(|k| before.get(k) != after.get(k));
    c
}

/// Row counts per bucket, for `config_status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counts {
    pub embedded: usize,
    pub downloaded: usize,
    pub custom: usize,
}

/// Token list manager: configuration, downloaded (merged) tokens, custom list.
pub struct TokenList {
    config: ListConfig,
    config_source: ConfigSource,
    sources: Vec<ListSource>,
    downloaded: Vec<Token>,
    custom: Vec<Token>,
    enabled: BTreeMap<u64, Vec<Token>>,
    dir: Option<PathBuf>,
}

impl TokenList {
    pub fn new() -> Self {
        Self {
            config: ListConfig::default(),
            config_source: ConfigSource::None,
            sources: Vec::new(),
            downloaded: Vec::new(),
            custom: Vec::new(),
            enabled: BTreeMap::new(),
            dir: None,
        }
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

    fn read_text(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.path(name)?).ok()
    }

    fn read_json<T: for<'de> Deserialize<'de>>(&self, name: &str) -> Option<T> {
        serde_json::from_str(&self.read_text(name)?).ok()
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

    fn write_json_atomic<T: Serialize>(&self, name: &str, value: &T) -> bool {
        let Some(path) = self.path(name) else { return true };
        let Ok(text) = serde_json::to_string_pretty(value) else { return false };
        store::write_then_rename(&path, &text)
    }

    fn load(&mut self) {
        if let Some(txt) = self.read_text("list_config.json") {
            self.load_config(&txt);
        }
        if let Some(t) = self.read_json::<Vec<Token>>("token_cache.json") {
            self.downloaded = t;
        }
        if let Some(t) = self.read_json::<Vec<Token>>("custom_tokens.json") {
            self.custom = t;
        }
        if let Some(t) = self.read_json::<BTreeMap<u64, Vec<Token>>>("enabled_tokens.json") {
            self.enabled = t;
        }
    }

    /// Envelope first: a bare `ListConfig` ignores unknown keys and would
    /// swallow one. A legacy bare file reads as `external` — somebody wrote it,
    /// and calling it a default would license `init_defaults` to overwrite it.
    fn load_config(&mut self, txt: &str) {
        if let Ok(rec) = serde_json::from_str::<ConfigRecord>(txt) {
            self.config = rec.config;
            self.config_source = match rec.source {
                ConfigSource::None => ConfigSource::External,
                s => s,
            };
        } else if let Ok(cfg) = serde_json::from_str::<ListConfig>(txt) {
            self.config = cfg;
            self.config_source = ConfigSource::External;
        }
    }

    /// Store a configuration. Returns whether anything actually moved — a
    /// re-send of the stored record writes nothing and announces nothing.
    /// The `source` upgrade to `external` counts as a change on its own: it is
    /// what stops a later `init_defaults` from overwriting the caller.
    pub fn configure(&mut self, config: ListConfig) -> bool {
        if self.config == config && self.config_source == ConfigSource::External {
            return false;
        }
        self.set_config(config, ConfigSource::External);
        true
    }

    fn set_config(&mut self, config: ListConfig, source: ConfigSource) {
        self.config = config;
        self.config_source = source;
        let rec = ConfigRecord { source, config: self.config.clone() };
        self.write_json("list_config.json", &rec);
    }

    /// Apply the built-in defaults, but only where nothing is configured.
    /// Idempotent and network-free; returns whether anything was written.
    pub fn init_defaults(&mut self) -> bool {
        if self.config_source != ConfigSource::None {
            return false;
        }
        self.set_config(ListConfig::default(), ConfigSource::Default);
        true
    }

    pub fn config(&self) -> &ListConfig {
        &self.config
    }

    pub fn config_source(&self) -> ConfigSource {
        self.config_source
    }

    pub fn counts(&self) -> Counts {
        Counts {
            embedded: self.embedded().len(),
            downloaded: self.downloaded.len(),
            custom: self.custom.len(),
        }
    }

    pub fn refresh_secs(&self) -> u64 {
        self.config.refresh_secs
    }

    /// The shipped offline floor, empty when the config turns it off.
    fn embedded(&self) -> &'static [Token] {
        if self.config.use_embedded_list {
            embedded_tokens()
        } else {
            &[]
        }
    }

    /// Fetch all configured list URLs through the fail-closed proxy, parse, and
    /// replace the downloaded set. Returns the total token count.
    pub fn refresh_now(&mut self) -> Result<usize, TokenError> {
        let plan = self.refresh_plan();
        let fetched = plan.fetch();
        self.apply_refresh(&plan, fetched)
    }

    /// What a refresh fetches. Taken from the store, then fetched with the store released.
    pub fn refresh_plan(&self) -> RefreshPlan {
        RefreshPlan {
            urls: self.config.list_urls.clone(),
            proxy: ProxyConfig::new(self.config.proxy.clone(), self.config.proxy_required, self.config.timeout_secs),
        }
    }

    /// Store what `plan` fetched: the per-URL outcomes and the merged downloaded set. Refused
    /// when the lists or proxy policy moved since the plan was taken.
    pub fn apply_refresh(&mut self, plan: &RefreshPlan, fetched: Fetched) -> Result<usize, TokenError> {
        if *plan != self.refresh_plan() {
            return Err(TokenError::Superseded);
        }
        let mut merged: Vec<Token> = Vec::new();
        let mut sources: Vec<ListSource> = Vec::new();
        for (url, outcome) in plan.urls.iter().zip(fetched?) {
            match outcome {
                Ok((name, tokens)) => {
                    sources.push(ListSource { url: url.clone(), name, token_count: tokens.len(), ok: true, error: None });
                    merged.extend(tokens);
                }
                Err(e) => {
                    sources.push(ListSource { url: url.clone(), name: String::new(), token_count: 0, ok: false, error: Some(e.to_string()) });
                }
            }
        }
        self.sources = sources;
        if merged != self.downloaded {
            self.downloaded = merged;
            self.write_json("token_cache.json", &self.downloaded);
        }
        Ok(self.downloaded.len())
    }

    /// Merge pinned → custom → downloaded → embedded for `chain_id`, deduped by
    /// address. A network fact this module vouches for wins every catalogue bucket.
    pub fn get_tokens(&self, chain_id: u64) -> Vec<TokenRow> {
        self.rows(chain_id, None)
    }

    /// Metadata for specific addresses only, matched case-insensitively. The
    /// full chain-1 reply is 86 KB; a wallet decorating a handful of rows
    /// should not pay for it.
    pub fn get_tokens_by_address(&self, chain_id: u64, addresses: &[String]) -> Vec<TokenRow> {
        let want: HashSet<String> = addresses.iter().map(|a| norm(a)).collect();
        self.rows(chain_id, Some(&want))
    }

    fn rows(&self, chain_id: u64, want: Option<&HashSet<String>>) -> Vec<TokenRow> {
        let pinned = offered::pinned(chain_id);
        let buckets = [
            (TokenSource::Builtin, pinned.as_slice()),
            (TokenSource::Custom, self.custom.as_slice()),
            (TokenSource::Downloaded, self.downloaded.as_slice()),
            (TokenSource::Embedded, self.embedded()),
        ];
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for (source, bucket) in buckets {
            for t in bucket {
                if t.chain_id != chain_id {
                    continue;
                }
                let key = norm(&t.address);
                if want.is_some_and(|w| !w.contains(&key)) {
                    continue;
                }
                if seen.insert(key) {
                    out.push(TokenRow {
                        token: t.clone(),
                        source,
                        enabled: self.is_enabled(chain_id, &t.address)
                            || source == TokenSource::Builtin,
                        builtin: source == TokenSource::Builtin,
                    });
                }
            }
        }
        out
    }

    pub fn get_all_tokens(&self) -> Vec<TokenRow> {
        self.chains().into_iter().flat_map(|c| self.get_tokens(c)).collect()
    }

    /// Upsert one user token. Returns whether the stored list moved; re-adding
    /// a byte-identical row is a no-op, not a change.
    pub fn add_custom_token(&mut self, token: Token) -> bool {
        let key = (token.chain_id, norm(&token.address));
        if self.custom.iter().any(|t| *t == token) {
            return false;
        }
        self.custom.retain(|t| (t.chain_id, norm(&t.address)) != key);
        self.custom.push(token);
        self.write_json("custom_tokens.json", &self.custom);
        true
    }

    /// Ingest one Uniswap-schema document into the CUSTOM list in a single
    /// call; the caller supplies the bytes, so this performs no network I/O.
    /// `replace` swaps the list wholesale, else entries merge (last wins per
    /// chain+address). Returns the new length. Which chains an import actually
    /// moved is a [`chain_digests`](Self::chain_digests) diff, not a guess from
    /// the document: a re-import of what is already stored moves nothing.
    pub fn import_custom_tokens(&mut self, list_json: &str, replace: bool) -> Result<usize, TokenError> {
        let (_name, tokens) = parse_list(list_json)?;
        let mut merged: Vec<Token> = if replace { Vec::new() } else { self.custom.clone() };
        let mut index: HashMap<(u64, String), usize> =
            merged.iter().enumerate().map(|(i, t)| ((t.chain_id, norm(&t.address)), i)).collect();
        for t in tokens {
            let key = (t.chain_id, norm(&t.address));
            match index.get(&key) {
                Some(&i) => merged[i] = t,
                None => {
                    index.insert(key, merged.len());
                    merged.push(t);
                }
            }
        }

        if merged == self.custom {
            return Ok(self.custom.len());
        }
        self.custom = merged;
        self.write_json("custom_tokens.json", &self.custom);
        Ok(self.custom.len())
    }

    pub fn remove_custom_token(&mut self, chain_id: u64, address: &str) -> bool {
        let before = self.custom.len();
        let key = (chain_id, norm(address));
        self.custom.retain(|t| (t.chain_id, norm(&t.address)) != key);
        let changed = self.custom.len() != before;
        if changed {
            self.write_json("custom_tokens.json", &self.custom);
        }
        let enabled_changed = self.remove_enabled(chain_id, address).unwrap_or(false);
        changed || enabled_changed
    }

    pub fn get_custom_tokens(&self) -> &[Token] {
        &self.custom
    }

    /// Fingerprint what `get_tokens` would serve, per chain. Snapshot around an
    /// operation and diff with [`changed_chains`] to announce only real changes.
    pub fn chain_digests(&self) -> ChainDigests {
        self.chains()
            .into_iter()
            .map(|c| {
                let mut h = DefaultHasher::new();
                self.get_tokens(c).hash(&mut h);
                self.list_offered(c).hash(&mut h);
                (c, h.finish())
            })
            .collect()
    }

    /// Distinct chain ids across every bucket (sorted).
    pub fn chains(&self) -> Vec<u64> {
        let mut chains = Self::distinct(
            self.custom.iter().chain(self.downloaded.iter()).chain(self.embedded()),
        );
        chains.extend(offered::chains());
        chains.extend(self.enabled.keys().copied());
        chains.sort_unstable();
        chains.dedup();
        chains
    }

    fn distinct<'a>(it: impl Iterator<Item = &'a Token>) -> Vec<u64> {
        let mut c: Vec<u64> = it.map(|t| t.chain_id).collect();
        c.sort_unstable();
        c.dedup();
        c
    }

    pub fn get_list_sources(&self) -> &[ListSource] {
        &self.sources
    }

    fn is_enabled(&self, chain_id: u64, address: &str) -> bool {
        self.enabled
            .get(&chain_id)
            .is_some_and(|rows| rows.iter().any(|t| t.address.eq_ignore_ascii_case(address)))
    }

    fn valid_address(address: &str) -> bool {
        let raw = address
            .trim()
            .strip_prefix("0x")
            .or_else(|| address.trim().strip_prefix("0X"))
            .unwrap_or("");
        raw.len() == 40 && raw.bytes().all(|b| b.is_ascii_hexdigit())
    }

    fn remove_enabled(&mut self, chain_id: u64, address: &str) -> Result<bool, OfferedError> {
        let before_all = self.enabled.clone();
        let Some(rows) = self.enabled.get_mut(&chain_id) else { return Ok(false) };
        let before = rows.len();
        rows.retain(|t| !t.address.eq_ignore_ascii_case(address.trim()));
        let changed = rows.len() != before;
        if rows.is_empty() {
            self.enabled.remove(&chain_id);
        }
        if changed && !self.write_json_atomic("enabled_tokens.json", &self.enabled) {
            self.enabled = before_all;
            return Err(OfferedError::Io);
        }
        Ok(changed)
    }

    pub fn set_token_enabled(
        &mut self,
        chain_id: u64,
        address: &str,
        enabled: bool,
    ) -> Result<bool, OfferedError> {
        let address = address.trim();
        if !Self::valid_address(address) {
            return Err(OfferedError::BadAddress);
        }
        if offered::is_pinned(chain_id, address) {
            return if enabled { Ok(false) } else { Err(OfferedError::Builtin) };
        }
        if !enabled {
            return self.remove_enabled(chain_id, address);
        }
        let token = self
            .get_tokens(chain_id)
            .into_iter()
            .find(|row| row.token.address.eq_ignore_ascii_case(address))
            .map(|row| row.token)
            .filter(|token| !token.symbol.trim().is_empty())
            .ok_or(OfferedError::UnknownToken)?;
        let before = self.enabled.clone();
        let rows = self.enabled.entry(chain_id).or_default();
        rows.retain(|t| !t.address.eq_ignore_ascii_case(address));
        rows.push(token);
        rows.sort_by_cached_key(|t| (t.symbol.to_ascii_lowercase(), norm(&t.address)));
        let changed = self.enabled != before;
        if changed && !self.write_json_atomic("enabled_tokens.json", &self.enabled) {
            self.enabled = before;
            return Err(OfferedError::Io);
        }
        Ok(changed)
    }

    pub fn get_enabled_tokens(&self, chain_id: u64) -> Vec<EnabledTokenRow> {
        self.enabled
            .get(&chain_id)
            .into_iter()
            .flatten()
            .map(|token| EnabledTokenRow {
                resolved: self
                    .get_tokens(chain_id)
                    .iter()
                    .any(|row| row.token.address.eq_ignore_ascii_case(&token.address)),
                token: token.clone(),
            })
            .collect()
    }

    pub fn list_offered(&self, chain_id: u64) -> Vec<TokenRow> {
        let catalogue = self.get_tokens(chain_id);
        let mut out: Vec<TokenRow> = catalogue.iter().filter(|row| row.builtin).cloned().collect();
        let mut enabled: Vec<TokenRow> = self
            .enabled
            .get(&chain_id)
            .into_iter()
            .flatten()
            .filter(|token| !offered::is_pinned(chain_id, &token.address))
            .map(|token| {
                let current = catalogue
                    .iter()
                    .find(|row| row.token.address.eq_ignore_ascii_case(&token.address));
                TokenRow {
                    token: token.clone(),
                    source: current.map(|row| row.source).unwrap_or(TokenSource::Enabled),
                    enabled: true,
                    builtin: false,
                }
            })
            .collect();
        enabled.sort_by_cached_key(|row| {
            (row.token.symbol.to_ascii_lowercase(), norm(&row.token.address))
        });
        out.extend(enabled);
        out
    }

    pub fn list_available(
        &self,
        chain_id: u64,
        query: &str,
        offset: usize,
        limit: Option<usize>,
    ) -> (usize, usize, Vec<TokenRow>) {
        let catalogue = self.get_tokens(chain_id);
        let listed = catalogue.len();
        let mut rows = self.list_offered(chain_id);
        let mut seen: HashSet<String> = rows.iter().map(|row| norm(&row.token.address)).collect();
        let mut rest: Vec<TokenRow> = catalogue
            .into_iter()
            .filter(|row| seen.insert(norm(&row.token.address)))
            .collect();
        rest.sort_by_cached_key(|row| {
            (row.token.symbol.to_ascii_lowercase(), norm(&row.token.address))
        });
        rows.extend(rest);
        rows.retain(|row| {
            let q = query.trim();
            q.is_empty()
                || row.token.address.eq_ignore_ascii_case(q)
                || row.token.symbol.to_ascii_lowercase().contains(&q.to_ascii_lowercase())
                || row.token.name.to_ascii_lowercase().contains(&q.to_ascii_lowercase())
        });
        let total = rows.len();
        let mut page: Vec<TokenRow> = rows.into_iter().skip(offset).collect();
        if let Some(limit) = limit {
            page.truncate(limit);
        }
        (total, listed, page)
    }
}

impl Default for TokenList {
    fn default() -> Self {
        Self::new()
    }
}

/// Each list's outcome in `listUrls` order, or the client that could not be built.
pub type Fetched = Result<Vec<Result<(String, Vec<Token>), TokenError>>, TokenError>;

/// The lists a refresh fetches and the proxy policy it fetches them under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefreshPlan {
    urls: Vec<String>,
    proxy: ProxyConfig,
}

impl RefreshPlan {
    /// All lists at once, so a refresh takes as long as its slowest list, not their sum.
    pub fn fetch(&self) -> Fetched {
        let client = build_client(&self.proxy).map_err(|e| TokenError::Proxy(e.to_string()))?;
        Ok(std::thread::scope(|s| {
            let jobs: Vec<_> = self.urls.iter().map(|url| s.spawn(|| fetch_list(&client, url))).collect();
            jobs.into_iter()
                .map(|j| j.join().unwrap_or_else(|_| Err(TokenError::Http("the fetch panicked".into()))))
                .collect()
        }))
    }
}

fn fetch_list(client: &reqwest::blocking::Client, url: &str) -> Result<(String, Vec<Token>), TokenError> {
    let resp = client.get(url).send().map_err(|e| TokenError::Http(e.to_string()))?;
    let text = resp.text().map_err(|e| TokenError::Http(e.to_string()))?;
    parse_list(&text)
}

pub(crate) fn parse_list(text: &str) -> Result<(String, Vec<Token>), TokenError> {
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

    /// Mainnet USDC — present in both LIST and the shipped list.
    const USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";

    fn offline() -> ListConfig {
        ListConfig { use_embedded_list: false, ..ListConfig::default() }
    }

    fn find<'a>(rows: &'a [TokenRow], symbol: &str) -> &'a TokenRow {
        rows.iter().find(|r| r.token.symbol == symbol).expect("symbol present")
    }

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
        tl.config = offline(); // isolate the downloaded/custom merge from the floor
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
        assert_eq!(chain1.len(), 3); // pinned WETH + USDC (deduped) + USDT
        let usdc = find(&chain1, "USDC");
        assert_eq!(usdc.token.name, "My USDC"); // custom won
        assert_eq!(usdc.source, TokenSource::Custom);
        assert_eq!(tl.get_tokens(10).len(), 1); // OP
    }

    fn mock_server(body: &'static str) -> String {
        slow_server(body, std::time::Duration::ZERO)
    }

    fn slow_server(body: &'static str, delay: std::time::Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                std::thread::sleep(delay);
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
        tl.configure(ListConfig { list_urls: vec![url], timeout_secs: 5, ..offline() });
        let n = tl.refresh_now().unwrap();
        assert_eq!(n, 3);
        assert_eq!(tl.get_tokens(1).len(), 3);
        assert!(tl.get_list_sources()[0].ok);
    }

    #[test]
    fn refresh_fail_closed_without_proxy() {
        let mut tl = TokenList::new();
        tl.configure(ListConfig {
            list_urls: vec!["https://tokens.example/list.json".into()],
            proxy_required: true,
            timeout_secs: 5,
            ..offline()
        });
        assert!(matches!(tl.refresh_now(), Err(TokenError::Proxy(_))));
    }

    #[test]
    fn a_refresh_whose_lists_moved_while_it_fetched_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let mut tl = TokenList::with_dir(dir.path().to_path_buf());
        tl.configure(ListConfig { list_urls: vec![mock_server(LIST)], timeout_secs: 5, ..offline() });
        let plan = tl.refresh_plan();
        let fetched = plan.fetch();
        tl.configure(ListConfig { list_urls: vec!["http://127.0.0.1:1/other.json".into()], timeout_secs: 5, ..offline() });
        assert!(matches!(tl.apply_refresh(&plan, fetched), Err(TokenError::Superseded)));
        assert_eq!((tl.counts().downloaded, tl.get_list_sources().len()), (0, 0));
        assert!(!dir.path().join("token_cache.json").exists());
    }

    #[test]
    fn every_list_is_fetched_at_once() {
        let slow = std::time::Duration::from_millis(800);
        let urls = vec![slow_server(LIST, slow), slow_server(LIST, slow)];
        let mut tl = TokenList::new();
        tl.configure(ListConfig { list_urls: urls, timeout_secs: 5, ..offline() });
        let started = std::time::Instant::now();
        tl.refresh_now().unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(1400), "took {:?}", started.elapsed());
        assert!(tl.get_list_sources().iter().all(|s| s.ok));
    }

    // ---- the shipped offline list -------------------------------------------

    #[test]
    fn the_shipped_list_is_the_one_we_vendored() {
        let all = embedded_tokens();
        assert_eq!(all.len(), 1709);
        let tl = TokenList::new();
        assert_eq!(tl.get_tokens(1).len(), 401);
        assert_eq!(tl.get_tokens(11155111).len(), 2);
        assert_eq!(tl.get_tokens(560048).len(), 0); // hoodi: the list covers it with nothing
        assert_eq!(tl.chains().len(), 25);
    }

    #[test]
    fn the_floor_is_labelled_embedded_and_never_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let tl = TokenList::with_dir(dir.path().to_path_buf());
        assert_eq!(find(&tl.get_tokens(1), "USDC").source, TokenSource::Embedded);
        // nothing was written: the floor lives in the binary, not the data dir
        assert!(!dir.path().join("token_cache.json").exists());
        assert!(!dir.path().join("custom_tokens.json").exists());
    }

    #[test]
    fn turning_the_floor_off_leaves_only_the_pinned_bucket() {
        let mut tl = TokenList::new();
        assert_eq!(tl.counts().embedded, 1709);
        tl.configure(ListConfig { use_embedded_list: false, ..ListConfig::default() });
        assert_eq!(tl.counts().embedded, 0);
        let rows = tl.get_tokens(1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, TokenSource::Builtin);
        assert_eq!(rows[0].token.symbol, "WETH");
    }

    // ---- default initialization ---------------------------------------------

    #[test]
    fn default_init_needs_no_network() {
        let dir = tempfile::tempdir().unwrap();
        let mut tl = TokenList::with_dir(dir.path().to_path_buf());
        assert_eq!(tl.config_source(), ConfigSource::None);
        assert!(tl.init_defaults());

        // nothing to fetch, and nothing scheduling a fetch
        assert!(tl.config().list_urls.is_empty());
        assert_eq!(tl.config().refresh_secs, 0);
        assert!(tl.get_list_sources().is_empty());
        assert!(tl.config().use_embedded_list);

        // a refresh under the defaults walks an empty URL list and serves the floor
        assert_eq!(tl.refresh_now().unwrap(), 0);
        assert_eq!(tl.get_tokens(1).len(), 401);
        assert!(tl.get_list_sources().is_empty());
    }

    #[test]
    fn init_defaults_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut tl = TokenList::with_dir(dir.path().to_path_buf());
        assert!(tl.init_defaults());
        let after_first = std::fs::read_to_string(dir.path().join("list_config.json")).unwrap();

        assert!(!tl.init_defaults()); // second call writes nothing
        assert!(!tl.init_defaults()); // and so does a third, from any caller
        assert_eq!(std::fs::read_to_string(dir.path().join("list_config.json")).unwrap(), after_first);
        assert_eq!(tl.config_source(), ConfigSource::Default);
    }

    #[test]
    fn init_defaults_is_idempotent_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut tl = TokenList::with_dir(dir.path().to_path_buf());
            assert!(tl.init_defaults());
        }
        // a fresh process reads `source: default` back and declines to re-apply
        let mut tl2 = TokenList::with_dir(dir.path().to_path_buf());
        assert_eq!(tl2.config_source(), ConfigSource::Default);
        assert!(!tl2.init_defaults());
    }

    #[test]
    fn init_defaults_never_clobbers_an_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let mine = ListConfig {
            list_urls: vec!["https://tokens.example/mine.json".into()],
            proxy: Some("socks5h://127.0.0.1:9050".into()),
            proxy_required: true,
            refresh_secs: 900,
            timeout_secs: 5,
            use_embedded_list: false,
        };
        {
            let mut tl = TokenList::with_dir(dir.path().to_path_buf());
            tl.configure(mine.clone());
        }
        let mut tl2 = TokenList::with_dir(dir.path().to_path_buf());
        assert_eq!(tl2.config_source(), ConfigSource::External);
        assert!(!tl2.init_defaults());
        assert_eq!(tl2.config(), &mine);
        assert_eq!(tl2.config_source(), ConfigSource::External); // never downgraded
    }

    #[test]
    fn a_legacy_bare_config_file_reads_as_external() {
        let dir = tempfile::tempdir().unwrap();
        // written before the envelope existed: somebody configured this
        std::fs::write(
            dir.path().join("list_config.json"),
            r#"{"listUrls":["https://tokens.example/l.json"],"proxyRequired":true,"timeoutSecs":7}"#,
        )
        .unwrap();
        let mut tl = TokenList::with_dir(dir.path().to_path_buf());
        assert_eq!(tl.config_source(), ConfigSource::External);
        assert!(!tl.init_defaults());
        assert_eq!(tl.config().timeout_secs, 7);
        assert!(tl.config().proxy_required);
        assert!(tl.config().use_embedded_list); // absent field takes the default: on
    }

    // ---- change detection ----------------------------------------------------

    #[test]
    fn init_defaults_announces_nothing_because_it_moves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut tl = TokenList::with_dir(dir.path().to_path_buf());
        let before = tl.chain_digests();
        assert!(tl.init_defaults()); // it did write list_config.json
        let after = tl.chain_digests();
        assert_eq!(after.len(), 25);
        // ...and every one of those chains serves the same rows it did before
        assert!(changed_chains(&before, &after).is_empty());
    }

    #[test]
    fn a_digest_moves_only_on_the_chain_that_changed() {
        let mut tl = TokenList::new();
        let before = tl.chain_digests();
        tl.add_custom_token(Token {
            chain_id: 10,
            address: "0x2222222222222222222222222222222222222222".into(),
            name: "Mine".into(),
            symbol: "MINE".into(),
            decimals: 18,
            logo_uri: None,
        });
        assert_eq!(changed_chains(&before, &tl.chain_digests()), vec![10]);
    }

    #[test]
    fn a_vanished_chain_counts_as_changed() {
        let mut tl = TokenList::new();
        let before = tl.chain_digests();
        tl.configure(ListConfig { use_embedded_list: false, ..ListConfig::default() });
        let after = tl.chain_digests();
        assert_eq!(after.keys().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(changed_chains(&before, &after).len(), 25);
    }

    #[test]
    fn a_refresh_that_loses_a_list_announces_the_chains_it_emptied() {
        let url = mock_server(LIST);
        let dir = tempfile::tempdir().unwrap();
        let mut tl = TokenList::with_dir(dir.path().to_path_buf());
        tl.configure(ListConfig { list_urls: vec![url], timeout_secs: 5, ..offline() });
        tl.refresh_now().unwrap();
        assert_eq!(tl.chains(), vec![1, 10]);

        // The chains a refresh empties are gone from the store by the time it
        // returns, so only a before/after diff can still name them.
        let before = tl.chain_digests();
        tl.configure(ListConfig { list_urls: vec!["http://127.0.0.1:1/l.json".into()], timeout_secs: 2, ..offline() });
        tl.refresh_now().unwrap();
        assert_eq!(tl.chain_digests().keys().copied().collect::<Vec<_>>(), vec![1]);
        assert_eq!(changed_chains(&before, &tl.chain_digests()), vec![1, 10]);
    }

    #[test]
    fn re_adding_an_identical_token_moves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut tl = TokenList::with_dir(dir.path().to_path_buf());
        let mine = Token {
            chain_id: 1,
            address: "0x1111111111111111111111111111111111111111".into(),
            name: "Mine".into(),
            symbol: "MINE".into(),
            decimals: 18,
            logo_uri: None,
        };
        assert!(tl.add_custom_token(mine.clone()));
        let before = tl.chain_digests();
        assert!(!tl.add_custom_token(mine.clone()));
        assert!(changed_chains(&before, &tl.chain_digests()).is_empty());

        // ...but an edit to the same address is a change
        assert!(tl.add_custom_token(Token { name: "Renamed".into(), ..mine }));
        assert_eq!(changed_chains(&before, &tl.chain_digests()), vec![1]);
        assert_eq!(tl.get_custom_tokens().len(), 1); // upsert, not append
    }

    #[test]
    fn a_reimport_of_what_is_already_stored_moves_nothing() {
        let mut tl = TokenList::new();
        tl.import_custom_tokens(LIST, true).unwrap();
        let before = tl.chain_digests();
        assert_eq!(tl.import_custom_tokens(LIST, true).unwrap(), 3);
        assert!(changed_chains(&before, &tl.chain_digests()).is_empty());
    }

    #[test]
    fn configure_declines_to_rewrite_what_it_already_holds() {
        let dir = tempfile::tempdir().unwrap();
        let mut tl = TokenList::with_dir(dir.path().to_path_buf());
        let cfg = ListConfig { timeout_secs: 9, ..ListConfig::default() };
        assert!(tl.configure(cfg.clone()));
        assert!(!tl.configure(cfg));
    }

    /// Why `config_changed` cannot be folded into `tokens_updated`: most config
    /// edits move the record and not one token row.
    #[test]
    fn a_config_edit_can_move_no_token_row_at_all() {
        let mut tl = TokenList::new();
        let before = tl.chain_digests();
        assert!(tl.configure(ListConfig { timeout_secs: 9, refresh_secs: 900, ..ListConfig::default() }));
        assert!(changed_chains(&before, &tl.chain_digests()).is_empty());
    }

    /// The one-way source upgrade is itself a change: it is what stops a later
    /// `init_defaults` from overwriting the caller.
    #[test]
    fn configure_upgrades_the_source_even_when_the_config_matches() {
        let dir = tempfile::tempdir().unwrap();
        let mut tl = TokenList::with_dir(dir.path().to_path_buf());
        assert!(tl.init_defaults());
        assert_eq!(tl.config_source(), ConfigSource::Default);
        assert!(tl.configure(ListConfig::default()));
        assert_eq!(tl.config_source(), ConfigSource::External);
        assert!(!tl.init_defaults());
    }

    // ---- precedence ----------------------------------------------------------

    #[test]
    fn a_custom_token_beats_the_shipped_list() {
        let mut tl = TokenList::new();
        tl.add_custom_token(Token {
            chain_id: 1,
            address: USDC.to_lowercase(), // same token, different case
            name: "My USDC".into(),
            symbol: "USDC".into(),
            decimals: 6,
            logo_uri: None,
        });
        let chain1 = tl.get_tokens(1);
        assert_eq!(chain1.len(), 401); // deduped against the shipped row, not added to it
        let usdc = find(&chain1, "USDC");
        assert_eq!(usdc.token.name, "My USDC");
        assert_eq!(usdc.source, TokenSource::Custom);
    }

    #[test]
    fn a_downloaded_entry_beats_the_shipped_list() {
        let mut tl = TokenList::new();
        let (_n, downloaded) = parse_list(LIST).unwrap();
        tl.downloaded = downloaded;
        let chain1 = tl.get_tokens(1);
        let usdc = find(&chain1, "USDC");
        assert_eq!(usdc.token.name, "USD Coin"); // LIST's name, not the shipped "USDCoin"
        assert_eq!(usdc.source, TokenSource::Downloaded);
        // OP is on chain 10, which the shipped list also covers
        assert_eq!(find(&tl.get_tokens(10), "OP").source, TokenSource::Downloaded);
    }

    #[test]
    fn a_refresh_where_every_url_fails_still_serves_the_floor() {
        let dir = tempfile::tempdir().unwrap();
        let mut tl = TokenList::with_dir(dir.path().to_path_buf());
        // port 1 refuses; the whole refresh yields nothing
        tl.configure(ListConfig { list_urls: vec!["http://127.0.0.1:1/l.json".into()], timeout_secs: 2, ..ListConfig::default() });
        assert_eq!(tl.refresh_now().unwrap(), 0);
        assert!(!tl.get_list_sources()[0].ok);
        assert_eq!(tl.get_tokens(1).len(), 401); // degraded to the floor, not to zero
    }

    // ---- narrow query + bulk ingest -----------------------------------------

    #[test]
    fn get_tokens_by_address_narrows_the_reply() {
        let tl = TokenList::new();
        let want = vec![USDC.to_lowercase(), "0xDEADbeef00000000000000000000000000000000".into()];
        let rows = tl.get_tokens_by_address(1, &want);
        assert_eq!(rows.len(), 1); // the unknown address is simply absent
        assert_eq!(rows[0].token.symbol, "USDC");
        assert_eq!(rows[0].source, TokenSource::Embedded);
        assert!(tl.get_tokens_by_address(1, &[]).is_empty());
    }

    #[test]
    fn import_custom_tokens_merges_then_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let mut tl = TokenList::with_dir(dir.path().to_path_buf());
        tl.add_custom_token(Token {
            chain_id: 1,
            address: "0x1111111111111111111111111111111111111111".into(),
            name: "Mine".into(),
            symbol: "MINE".into(),
            decimals: 18,
            logo_uri: None,
        });

        let before = tl.chain_digests();
        let n = tl.import_custom_tokens(LIST, false).unwrap();
        assert_eq!(n, 4); // MINE + the 3 from LIST
        assert_eq!(changed_chains(&before, &tl.chain_digests()), vec![1, 10]);
        assert_eq!(find(&tl.get_tokens(1), "MINE").source, TokenSource::Custom);

        let before = tl.chain_digests();
        let n = tl.import_custom_tokens(LIST, true).unwrap();
        assert_eq!(n, 3); // MINE dropped by the wholesale swap
        assert_eq!(changed_chains(&before, &tl.chain_digests()), vec![1]); // only chain 1 lost a row
        assert!(tl.get_custom_tokens().iter().all(|t| t.symbol != "MINE"));

        // persisted, and it is the custom bucket that moved
        let tl2 = TokenList::with_dir(dir.path().to_path_buf());
        assert_eq!(tl2.counts(), Counts { embedded: 1709, downloaded: 0, custom: 3 });
    }

    #[test]
    fn import_rejects_a_document_that_is_not_a_token_list() {
        let mut tl = TokenList::new();
        assert!(matches!(tl.import_custom_tokens("not json", false), Err(TokenError::Parse(_))));
        assert!(tl.get_custom_tokens().is_empty());
    }

    // ---- the offered set ----------------------------------------------------

    const WETH: &str = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2";
    const DAI: &str = "0x6B175474E89094C44Da98b954EedeAC495271d0F";

    fn token(chain_id: u64, symbol: &str, name: &str, decimals: u8, address: &str) -> Token {
        Token {
            chain_id,
            address: address.into(),
            name: name.into(),
            symbol: symbol.into(),
            decimals,
            logo_uri: None,
        }
    }

    fn symbols(rows: &[TokenRow]) -> Vec<String> {
        rows.iter().map(|row| row.token.symbol.clone()).collect()
    }

    #[test]
    fn only_mainnet_has_a_pinned_token() {
        assert_eq!(symbols(&TokenList::new().list_offered(1)), ["WETH"]);
        assert!(TokenList::new().list_offered(11_155_111).is_empty());
        assert!(TokenList::new().list_offered(560_048).is_empty());
    }

    #[test]
    fn the_pinned_row_is_enabled_builtin_and_first() {
        let row = TokenList::new().list_offered(1).remove(0);
        assert_eq!(row.source, TokenSource::Builtin);
        assert!(row.enabled && row.builtin);
        assert_eq!((row.token.symbol.as_str(), row.token.decimals), ("WETH", 18));
    }

    #[test]
    fn a_pinned_row_outranks_a_custom_copy() {
        let mut tl = TokenList::new();
        tl.add_custom_token(token(1, "WETH9", "Wrong", 6, &WETH.to_lowercase()));
        let row = tl.get_tokens_by_address(1, &[WETH.into()]).remove(0);
        assert_eq!((row.token.symbol.as_str(), row.token.decimals), ("WETH", 18));
        assert_eq!(row.source, TokenSource::Builtin);
    }

    #[test]
    fn a_pinned_row_cannot_be_disabled() {
        let mut tl = TokenList::new();
        assert_eq!(tl.set_token_enabled(1, WETH, false), Err(OfferedError::Builtin));
        assert_eq!(symbols(&tl.list_offered(1)), ["WETH"]);
    }

    #[test]
    fn enabling_a_pinned_row_is_a_silent_noop() {
        let mut tl = TokenList::new();
        assert_eq!(tl.set_token_enabled(1, &WETH.to_lowercase(), true), Ok(false));
        assert!(tl.get_enabled_tokens(1).is_empty());
    }

    #[test]
    fn malformed_addresses_are_refused() {
        let mut tl = TokenList::new();
        for address in ["", "0x", "0xnothex", "1234", "0x000000000000000000000000000000000000000z"] {
            assert_eq!(tl.set_token_enabled(1, address, true), Err(OfferedError::BadAddress));
        }
    }

    #[test]
    fn an_unknown_address_is_never_snapshotted() {
        let mut tl = TokenList::new();
        let unknown = "0x0000000000000000000000000000000000000001";
        assert_eq!(tl.set_token_enabled(1, unknown, true), Err(OfferedError::UnknownToken));
        assert!(tl.get_enabled_tokens(1).is_empty());
    }

    #[test]
    fn enabling_snapshots_every_catalogue_field() {
        let mut tl = TokenList::new();
        tl.downloaded = vec![token(1, "USDC", "USD Coin", 6, USDC)];
        assert_eq!(tl.set_token_enabled(1, &USDC.to_lowercase(), true), Ok(true));
        let row = tl.get_enabled_tokens(1).remove(0);
        assert_eq!(row.token, token(1, "USDC", "USD Coin", 6, USDC));
        assert!(row.resolved);
    }

    #[test]
    fn the_enabled_snapshot_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut tl = TokenList::with_dir(dir.path().to_path_buf());
            tl.downloaded = vec![token(1, "USDC", "USD Coin", 6, USDC)];
            assert_eq!(tl.set_token_enabled(1, USDC, true), Ok(true));
        }
        let tl = TokenList::with_dir(dir.path().to_path_buf());
        assert_eq!(tl.get_enabled_tokens(1)[0].token.decimals, 6);
        assert_eq!(symbols(&tl.list_offered(1)), ["WETH", "USDC"]);
    }

    #[test]
    fn enabled_sets_are_independent_per_chain() {
        let mut tl = TokenList::new();
        tl.downloaded = vec![token(1, "USDC", "USD Coin", 6, USDC), token(10, "USDC", "USD Coin", 6, USDC)];
        assert_eq!(tl.set_token_enabled(10, USDC, true), Ok(true));
        assert!(tl.get_enabled_tokens(1).is_empty());
        assert_eq!(symbols(&tl.list_offered(10)), ["USDC"]);
    }

    #[test]
    fn re_enabling_refreshes_a_changed_snapshot() {
        let mut tl = TokenList::new();
        tl.downloaded = vec![token(1, "USDC", "Old name", 6, USDC)];
        assert_eq!(tl.set_token_enabled(1, USDC, true), Ok(true));
        tl.downloaded[0].name = "USD Coin".into();
        assert_eq!(tl.set_token_enabled(1, USDC, true), Ok(true));
        assert_eq!(tl.get_enabled_tokens(1)[0].token.name, "USD Coin");
        assert_eq!(tl.set_token_enabled(1, USDC, true), Ok(false));
    }

    #[test]
    fn disabling_is_case_insensitive_and_idempotent() {
        let mut tl = TokenList::new();
        tl.downloaded = vec![token(1, "USDC", "USD Coin", 6, USDC)];
        tl.set_token_enabled(1, USDC, true).unwrap();
        assert_eq!(tl.set_token_enabled(1, &USDC.to_lowercase(), false), Ok(true));
        assert_eq!(tl.set_token_enabled(1, USDC, false), Ok(false));
    }

    #[test]
    fn an_orphaned_snapshot_stays_offered_and_says_enabled() {
        let mut tl = TokenList::new();
        tl.downloaded = vec![token(560_048, "TEST", "Test", 7, DAI)];
        tl.set_token_enabled(560_048, DAI, true).unwrap();
        tl.downloaded.clear();
        let row = tl.list_offered(560_048).remove(0);
        assert_eq!(row.source, TokenSource::Enabled);
        assert_eq!(row.token.decimals, 7);
        assert!(row.enabled && !row.builtin);
        assert!(!tl.get_enabled_tokens(560_048)[0].resolved);
    }

    #[test]
    fn removing_a_custom_token_also_removes_its_enabled_snapshot() {
        let mut tl = TokenList::new();
        tl.add_custom_token(token(10, "DAI", "Dai", 18, DAI));
        tl.set_token_enabled(10, DAI, true).unwrap();
        assert!(tl.remove_custom_token(10, DAI));
        assert!(tl.get_enabled_tokens(10).is_empty());
        assert!(tl.list_offered(10).is_empty());
    }

    #[test]
    fn offered_rows_are_pinned_then_enabled_alphabetically() {
        let mut tl = TokenList::new();
        tl.downloaded = vec![token(1, "USDC", "USD Coin", 6, USDC), token(1, "DAI", "Dai", 18, DAI)];
        tl.set_token_enabled(1, USDC, true).unwrap();
        tl.set_token_enabled(1, DAI, true).unwrap();
        assert_eq!(symbols(&tl.list_offered(1)), ["WETH", "DAI", "USDC"]);
    }

    #[test]
    fn enabling_does_not_turn_a_snapshot_into_a_catalogue_bucket() {
        let mut tl = TokenList::new();
        tl.config = offline();
        tl.downloaded = vec![token(10, "DAI", "Dai", 18, DAI)];
        tl.set_token_enabled(10, DAI, true).unwrap();
        tl.downloaded.clear();
        assert!(tl.get_tokens(10).is_empty());
        assert_eq!(symbols(&tl.list_offered(10)), ["DAI"]);
    }

    #[test]
    fn the_picker_puts_offered_rows_before_the_rest() {
        let mut tl = TokenList::new();
        tl.config = offline();
        tl.downloaded = vec![token(1, "USDC", "USD Coin", 6, USDC), token(1, "DAI", "Dai", 18, DAI)];
        tl.set_token_enabled(1, USDC, true).unwrap();
        let (total, listed, rows) = tl.list_available(1, "", 0, None);
        assert_eq!((total, listed), (3, 3));
        assert_eq!(symbols(&rows), ["WETH", "USDC", "DAI"]);
    }

    #[test]
    fn the_picker_matches_symbol_name_and_exact_address() {
        let mut tl = TokenList::new();
        tl.config = offline();
        tl.downloaded = vec![token(10, "DAI", "Dai Stablecoin", 18, DAI)];
        for query in ["dai", "stable", &DAI.to_lowercase()] {
            assert_eq!(symbols(&tl.list_available(10, query, 0, None).2), ["DAI"]);
        }
        assert!(tl.list_available(10, "0x6B17", 0, None).2.is_empty());
    }

    #[test]
    fn the_picker_total_is_before_pagination() {
        let mut tl = TokenList::new();
        tl.config = offline();
        tl.downloaded = (0..12).map(|i| token(10, &format!("T{i:02}"), "Token", 18, &format!("0x{i:040x}"))).collect();
        let (total, listed, first) = tl.list_available(10, "", 0, Some(5));
        assert_eq!((total, listed, first.len()), (12, 12, 5));
        assert_eq!(symbols(&first), ["T00", "T01", "T02", "T03", "T04"]);
    }

    #[test]
    fn picker_pages_tile_the_full_answer() {
        let mut tl = TokenList::new();
        tl.config = offline();
        tl.downloaded = (0..12).map(|i| token(10, &format!("T{i:02}"), "Token", 18, &format!("0x{:040x}", i + 1))).collect();
        let mut got = Vec::new();
        for offset in [0, 5, 10] {
            let (total, _, page) = tl.list_available(10, "", offset, Some(5));
            assert_eq!(total, 12);
            got.extend(symbols(&page));
        }
        assert_eq!(got, symbols(&tl.list_available(10, "", 0, None).2));
        assert!(tl.list_available(10, "", 99, Some(5)).2.is_empty());
    }

    #[test]
    fn an_enabled_only_chain_remains_in_the_digest_union() {
        let mut tl = TokenList::new();
        tl.config = offline();
        tl.downloaded = vec![token(560_048, "TEST", "Test", 18, DAI)];
        tl.set_token_enabled(560_048, DAI, true).unwrap();
        tl.downloaded.clear();
        assert!(tl.chains().contains(&560_048));
        assert!(tl.chain_digests().contains_key(&560_048));
    }

    #[test]
    fn toggling_one_chain_moves_only_its_digest() {
        let mut tl = TokenList::new();
        tl.downloaded = vec![token(1, "USDC", "USD Coin", 6, USDC), token(10, "DAI", "Dai", 18, DAI)];
        let before = tl.chain_digests();
        tl.set_token_enabled(10, DAI, true).unwrap();
        assert_eq!(changed_chains(&before, &tl.chain_digests()), vec![10]);
    }

    #[test]
    fn enabled_writes_leave_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut tl = TokenList::with_dir(dir.path().to_path_buf());
        tl.downloaded = vec![token(1, "USDC", "USD Coin", 6, USDC)];
        tl.set_token_enabled(1, USDC, true).unwrap();
        tl.set_token_enabled(1, USDC, false).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn enabled_and_builtin_flags_are_omitted_only_when_false() {
        let mut tl = TokenList::new();
        tl.downloaded = vec![token(10, "DAI", "Dai", 18, DAI)];
        let plain = serde_json::to_value(&tl.get_tokens(10)[0]).unwrap();
        assert!(plain.get("enabled").is_none() && plain.get("builtin").is_none());
        tl.set_token_enabled(10, DAI, true).unwrap();
        let enabled = serde_json::to_value(&tl.list_offered(10)[0]).unwrap();
        assert_eq!(enabled["enabled"], true);
        assert!(enabled.get("builtin").is_none());
    }

    // ---- the wire type -------------------------------------------------------

    /// Every reply type speaks one dialect. `ListSource` used to leak
    /// `token_count` into an otherwise camelCase wire.
    #[test]
    fn every_reply_type_is_camel_case_on_the_wire() {
        let src = ListSource { url: "http://x/l.json".into(), name: "L".into(), token_count: 7, ok: true, error: None };
        assert_eq!(
            serde_json::to_value(&src).unwrap(),
            serde_json::json!({"url":"http://x/l.json","name":"L","tokenCount":7,"ok":true})
        );
        let (_n, tokens) = parse_list(LIST).unwrap();
        let row = TokenRow {
            token: tokens[0].clone(),
            source: TokenSource::Downloaded,
            enabled: false,
            builtin: false,
        };
        let mut keys: Vec<String> = serde_json::to_value(&row).unwrap().as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["address", "chainId", "decimals", "logoURI", "name", "source", "symbol"]);
    }

    #[test]
    fn serde_and_derived_defaults_agree() {
        let from_serde: ListConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(from_serde, ListConfig::default());
        assert_eq!(from_serde.timeout_secs, 8);
        assert!(from_serde.use_embedded_list);
    }

    #[test]
    fn wire_omission_preserves_every_untouched_field() {
        let stored = ListConfig {
            list_urls: vec!["https://a/l.json".into()],
            proxy: Some("socks5h://127.0.0.1:9050".into()),
            proxy_required: true,
            refresh_secs: 900,
            timeout_secs: 7,
            use_embedded_list: false,
        };
        // the whole-record bug this type exists to prevent: one key, one change
        let wire: ListConfigWire = serde_json::from_str(r#"{"listUrls":["https://b/l.json"]}"#).unwrap();
        let out = wire.resolve(Some(&stored));
        assert_eq!(out.list_urls, vec!["https://b/l.json".to_string()]);
        assert_eq!(out, ListConfig { list_urls: out.list_urls.clone(), ..stored });
    }

    #[test]
    fn an_explicit_null_clears_the_proxy_but_silence_keeps_it() {
        let stored = ListConfig { proxy: Some("socks5h://127.0.0.1:9050".into()), ..ListConfig::default() };
        let keep: ListConfigWire = serde_json::from_str(r#"{"timeoutSecs":9}"#).unwrap();
        assert_eq!(keep.resolve(Some(&stored)).proxy, stored.proxy);
        let clear: ListConfigWire = serde_json::from_str(r#"{"proxy":null}"#).unwrap();
        assert_eq!(clear.resolve(Some(&stored)).proxy, None);
    }
}
