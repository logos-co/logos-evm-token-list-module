//! Logos module glue for `token_list_module` (rust-first authoring).
//!
//! The builder derives the `.lidl` from the `TokenListModule` trait below
//! (`codegen.rust = { trait, source: "src/glue.rs" }`). Compiled only with the
//! default `logos_module` feature; `cargo test --no-default-features` exercises
//! the `tokens` + `proxy` cores without the Logos runtime.
//!
//! `init_defaults` turns on the shipped offline list and nothing else, so a
//! consumer that calls it performs no network I/O. `refresh_now` is the only
//! method that fetches, and nothing schedules it. It fetches with the store
//! released, so every other call is answered from the current rows meanwhile.
//!
//! Every method that changes persisted state announces it and every reader is
//! silent; a call that changes nothing announces nothing, so a subscriber can
//! treat an event as "something really moved".

use std::sync::{RwLock, TryLockError};

use serde_json::json;

use crate::tokens::{changed_chains, ConfigSource, ListConfigWire, Token, TokenList};

pub trait TokenListModule: Send + Sync + 'static {
    /// Set configuration: `{ listUrls?, proxy?, proxyRequired?, refreshSecs?,
    /// timeoutSecs?, useEmbeddedList? }`. Omitted keys keep their stored value.
    /// Emits `config_changed`, plus `tokens_updated` for any chain the new
    /// config re-serves (`useEmbeddedList` is the one that moves rows).
    fn configure(&self, config_json: String) -> bool;
    /// Whether a config has been SET, and what it is → `{ ok, state, source,
    /// config?, counts }`. No network I/O, so it is cheap on a startup path.
    fn config_status(&self) -> String;
    /// Apply the built-in offline defaults, only where nothing is configured →
    /// `{ ok, applied, state, source, config }`. Idempotent; `applied: false`
    /// means somebody got there first and is not an error. Emits
    /// `config_changed` when it applied, and normally no `tokens_updated`.
    fn init_defaults(&self) -> String;
    /// Fetch all configured lists through the fail-closed proxy → `{ ok, tokenCount }`.
    fn refresh_now(&self) -> String;
    /// Merged builtin + custom + downloaded + embedded tokens for a chain, each row
    /// labelled with the bucket it came from → `{ ok, tokens: [...] }`.
    fn get_tokens(&self, chain_id: i64) -> String;
    /// Metadata for specific tokens only; `addresses_json` is a JSON array of
    /// hex addresses matched case-insensitively → `{ ok, tokens: [...] }`.
    fn get_tokens_by_address(&self, chain_id: i64, addresses_json: String) -> String;
    fn get_all_tokens(&self) -> String;
    /// Enable snapshots the catalogue row; disabling removes the snapshot. Pinned rows cannot
    /// be disabled. `{ ok, chainId, address, enabled, changed }`.
    fn set_token_enabled(&self, chain_id: i64, address: String, enabled: bool) -> String;
    /// Persisted snapshots for one chain, each with `resolved` saying whether a current
    /// catalogue bucket still describes the address.
    fn get_enabled_tokens(&self, chain_id: i64) -> String;
    /// Pinned rows followed by enabled snapshots. ERC-20 only.
    fn list_offered(&self, chain_id: i64) -> String;
    /// Offered rows first, then the rest of the catalogue, with search and pagination.
    fn list_available(
        &self,
        chain_id: i64,
        query: String,
        offset: i64,
        limit: i64,
    ) -> String;
    /// Add a user token: `{ chainId, address, name, symbol, decimals, logoURI? }`.
    fn add_custom_token(&self, token_json: String) -> bool;
    /// Bulk-ingest one Uniswap-schema document into the CUSTOM list; the caller
    /// supplies the bytes, so this fetches nothing. `replace` swaps the list
    /// wholesale, else entries merge → `{ ok, tokenCount }`.
    fn import_custom_tokens(&self, list_json: String, replace: bool) -> String;
    fn remove_custom_token(&self, chain_id: i64, address: String) -> bool;
    fn get_custom_tokens(&self) -> String;
    fn get_list_sources(&self) -> String;
    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

/// Typed events. Every method that moves persisted state announces it; a
/// reader announces nothing.
pub trait TokenListModuleEvents {
    /// The rows `get_tokens(chain_id)` serves have changed. Emitted once per
    /// affected chain, after the change is on disk.
    fn tokens_updated(&self, chain_id: i64);
    /// The stored configuration record moved. Separate from `tokens_updated`
    /// because most config edits (a proxy, a refresh interval, a list URL) move
    /// no token row at all, and this store is shared between apps: without this
    /// a consumer can only learn of another app's edit by polling
    /// `config_status`. `source` is the new `none|default|external` and is
    /// ADVISORY — re-read `config_status()` for the record itself.
    fn config_changed(&self, source: String);
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

#[derive(Default)]
struct TokenListModuleImpl {
    /// Held to read or change the rows, never across a fetch.
    tl: RwLock<Option<TokenList>>,
    /// One refresh at a time.
    refreshing: std::sync::Mutex<()>,
}

const NOT_READY: &str = "token_list not initialized (context not ready)";

impl TokenListModuleImpl {
    fn read<T>(&self, f: impl FnOnce(&TokenList) -> T) -> std::result::Result<T, String> {
        let guard = self.tl.read().map_err(|_| "token_list lock poisoned".to_string())?;
        guard.as_ref().map(f).ok_or_else(|| NOT_READY.to_string())
    }

    /// Run a mutation and announce the chains whose served rows actually moved.
    /// Fingerprinting either side is the only honest test: what a caller passed
    /// in says nothing about whether it differed from what was stored, and a
    /// refresh that drops a list must announce the chains it emptied — which no
    /// after-the-fact scan of the store can name. Announced once the lock is released.
    fn with_chain_events<T>(&self, f: impl FnOnce(&mut TokenList) -> T) -> std::result::Result<T, String> {
        let (out, moved) = {
            let mut guard = self.tl.write().map_err(|_| "token_list lock poisoned".to_string())?;
            let tl = guard.as_mut().ok_or_else(|| NOT_READY.to_string())?;
            let before = tl.chain_digests();
            let out = f(&mut *tl); // reborrow: the digest below still needs `tl`
            (out, changed_chains(&before, &tl.chain_digests()))
        };
        for c in moved {
            emit_tokens_updated(c as i64);
        }
        Ok(out)
    }
}

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

fn bad_chain() -> String {
    json!({ "ok": false, "code": "bad_chain", "error": "chainId must be non-negative" }).to_string()
}

/// The `source` as `config_status` spells it — same serde rename, so the event
/// payload and the status reply cannot drift apart.
fn source_str(s: ConfigSource) -> String {
    json!(s).as_str().unwrap_or_default().to_string()
}

/// "Ask me again" — distinct from "I have no config", which a bool would blur.
fn unready() -> String {
    json!({ "ok": false, "state": "unready", "error": "token_list context not ready" }).to_string()
}

/// The `{ state, source, config?, counts }` body shared by both status replies.
fn status_body(tl: &TokenList) -> serde_json::Map<String, serde_json::Value> {
    let configured = tl.config_source() != ConfigSource::None;
    let c = tl.counts();
    let mut m = serde_json::Map::new();
    m.insert("state".into(), json!(if configured { "configured" } else { "unconfigured" }));
    m.insert("source".into(), json!(tl.config_source()));
    if configured {
        m.insert("config".into(), json!(tl.config()));
    }
    m.insert("counts".into(), json!({ "embedded": c.embedded, "downloaded": c.downloaded, "custom": c.custom }));
    m
}

fn reply(mut m: serde_json::Map<String, serde_json::Value>) -> String {
    m.insert("ok".into(), json!(true));
    serde_json::Value::Object(m).to_string()
}

impl TokenListModule for TokenListModuleImpl {
    fn on_context_ready(&self, ctx: &RustModuleContext) {
        let tl = TokenList::with_dir(std::path::PathBuf::from(&ctx.instance_persistence_path));
        let _ = tl.counts(); // parse the embedded list now (~1.2ms), not on a user's first call
        if let Ok(mut guard) = self.tl.write() {
            *guard = Some(tl);
        }
    }

    fn configure(&self, config_json: String) -> bool {
        let wire: ListConfigWire = match serde_json::from_str(&config_json) {
            Ok(c) => c,
            Err(_) => return false,
        };
        // A config the module already holds is accepted and stays silent.
        match self.with_chain_events(|tl| {
            let resolved = wire.resolve(Some(tl.config()));
            tl.configure(resolved).then(|| source_str(tl.config_source()))
        }) {
            Ok(moved) => {
                if let Some(source) = moved {
                    emit_config_changed(&source);
                }
                true
            }
            Err(_) => false,
        }
    }

    fn config_status(&self) -> String {
        self.read(|tl| reply(status_body(tl))).unwrap_or_else(|_| unready())
    }

    fn init_defaults(&self) -> String {
        // The defaults are exactly what an unconfigured module already serves,
        // so the usual outcome is a config_changed with no tokens_updated.
        let out = self.with_chain_events(|tl| {
            let applied = tl.init_defaults();
            let source = source_str(tl.config_source());
            let mut m = status_body(tl);
            m.insert("applied".into(), json!(applied));
            if !applied {
                m.insert("reason".into(), json!("already configured"));
            }
            (applied, source, reply(m))
        });
        match out {
            Ok((applied, source, body)) => {
                if applied {
                    emit_config_changed(&source);
                }
                body
            }
            Err(_) => unready(),
        }
    }

    fn refresh_now(&self) -> String {
        let _one = match self.refreshing.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return err("a refresh is already running"),
        };
        let plan = match self.read(TokenList::refresh_plan) {
            Ok(plan) => plan,
            Err(e) => return err(e),
        };
        let fetched = plan.fetch();
        match self.with_chain_events(|tl| tl.apply_refresh(&plan, fetched)) {
            Ok(Ok(count)) => json!({ "ok": true, "tokenCount": count }).to_string(),
            Ok(Err(e)) => err(e),
            Err(e) => err(e),
        }
    }

    fn get_tokens(&self, chain_id: i64) -> String {
        if chain_id < 0 { return bad_chain(); }
        self.read(|tl| json!({ "ok": true, "tokens": tl.get_tokens(chain_id as u64) }).to_string())
            .unwrap_or_else(err)
    }

    fn get_tokens_by_address(&self, chain_id: i64, addresses_json: String) -> String {
        if chain_id < 0 { return bad_chain(); }
        let addresses: Vec<String> = match serde_json::from_str(&addresses_json) {
            Ok(a) => a,
            Err(e) => return err(e),
        };
        self.read(|tl| {
            json!({ "ok": true, "tokens": tl.get_tokens_by_address(chain_id as u64, &addresses) }).to_string()
        })
        .unwrap_or_else(err)
    }

    fn get_all_tokens(&self) -> String {
        self.read(|tl| json!({ "ok": true, "tokens": tl.get_all_tokens() }).to_string()).unwrap_or_else(err)
    }

    fn set_token_enabled(&self, chain_id: i64, address: String, enabled: bool) -> String {
        if chain_id < 0 {
            return bad_chain();
        }
        match self.with_chain_events(|tl| tl.set_token_enabled(chain_id as u64, &address, enabled)) {
            Ok(Ok(changed)) => json!({ "ok": true, "chainId": chain_id, "address": address,
                                       "enabled": enabled, "changed": changed }).to_string(),
            Ok(Err(e)) => json!({ "ok": false, "code": e.code(), "error": e.to_string() }).to_string(),
            Err(e) => err(e),
        }
    }

    fn get_enabled_tokens(&self, chain_id: i64) -> String {
        if chain_id < 0 { return bad_chain(); }
        self.read(|tl| {
            json!({ "ok": true, "chainId": chain_id, "tokens": tl.get_enabled_tokens(chain_id as u64) }).to_string()
        })
        .unwrap_or_else(err)
    }

    fn list_offered(&self, chain_id: i64) -> String {
        if chain_id < 0 { return bad_chain(); }
        self.read(|tl| {
            json!({ "ok": true, "chainId": chain_id, "tokens": tl.list_offered(chain_id as u64) }).to_string()
        })
        .unwrap_or_else(err)
    }

    fn list_available(&self, chain_id: i64, query: String, offset: i64, limit: i64) -> String {
        if chain_id < 0 { return bad_chain(); }
        let offset = usize::try_from(offset).unwrap_or(0);
        let limit = usize::try_from(limit).ok().filter(|n| *n > 0);
        self.read(|tl| {
            let (total, listed, tokens) = tl.list_available(chain_id as u64, &query, offset, limit);
            json!({ "ok": true, "chainId": chain_id, "total": total, "offset": offset,
                    "shown": tokens.len(), "hasMore": offset.saturating_add(tokens.len()) < total,
                    "listed": listed, "tokens": tokens }).to_string()
        })
        .unwrap_or_else(err)
    }

    fn add_custom_token(&self, token_json: String) -> bool {
        let token: Token = match serde_json::from_str(&token_json) {
            Ok(t) => t,
            Err(_) => return false,
        };
        // true means "stored", which a re-add of an identical row also is.
        self.with_chain_events(|tl| tl.add_custom_token(token)).is_ok()
    }

    fn import_custom_tokens(&self, list_json: String, replace: bool) -> String {
        match self.with_chain_events(|tl| tl.import_custom_tokens(&list_json, replace)) {
            Ok(Ok(count)) => json!({ "ok": true, "tokenCount": count }).to_string(),
            Ok(Err(e)) => err(e),
            Err(e) => err(e),
        }
    }

    fn remove_custom_token(&self, chain_id: i64, address: String) -> bool {
        self.with_chain_events(|tl| tl.remove_custom_token(chain_id as u64, &address)).unwrap_or(false)
    }

    fn get_custom_tokens(&self) -> String {
        self.read(|tl| json!({ "ok": true, "tokens": tl.get_custom_tokens() }).to_string()).unwrap_or_else(err)
    }

    fn get_list_sources(&self) -> String {
        self.read(|tl| json!({ "ok": true, "sources": tl.get_list_sources() }).to_string())
            .unwrap_or_else(err)
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<TokenListModuleImpl>();
}
