//! Logos module glue for `token_list_module` (rust-first authoring).
//!
//! The builder derives the `.lidl` from the `TokenListModule` trait below
//! (`codegen.rust = { trait, source: "src/glue.rs" }`). Compiled only with the
//! default `logos_module` feature; `cargo test --no-default-features` exercises
//! the `tokens` + `proxy` cores without the Logos runtime.
//!
//! Periodic refresh is driven on demand (`refresh_now`) — the wallet backend
//! schedules it on its own worker; this keeps the module single-threaded and
//! avoids sharing the token store across threads. `tokens_updated` is emitted
//! per affected chain after a successful refresh or custom-token change.

use serde_json::json;

use crate::tokens::{ListConfig, Token, TokenList};

pub trait TokenListModule: Send + 'static {
    /// `config_json`: `{ listUrls, proxy?, proxyRequired?, refreshSecs?, timeoutSecs? }`.
    fn configure(&mut self, config_json: String) -> bool;
    /// Fetch all configured lists through the fail-closed proxy → `{ ok, tokenCount }`.
    fn refresh_now(&mut self) -> String;
    /// Merged downloaded + custom tokens for a chain → `{ ok, tokens: [...] }`.
    fn get_tokens(&mut self, chain_id: i64) -> String;
    fn get_all_tokens(&mut self) -> String;
    /// Add a user token: `{ chainId, address, name, symbol, decimals, logoURI? }`.
    fn add_custom_token(&mut self, token_json: String) -> bool;
    fn remove_custom_token(&mut self, chain_id: i64, address: String) -> bool;
    fn get_custom_tokens(&mut self) -> String;
    fn get_list_sources(&mut self) -> String;
    fn on_context_ready(&mut self, _ctx: &RustModuleContext) {}
}

/// Typed events — emitted per chain whenever its token set changes.
pub trait TokenListModuleEvents {
    fn tokens_updated(&self, chain_id: i64);
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

#[derive(Default)]
struct TokenListModuleImpl {
    tl: Option<TokenList>,
}

impl TokenListModuleImpl {
    fn tl(&mut self) -> std::result::Result<&mut TokenList, String> {
        self.tl.as_mut().ok_or_else(|| "token_list not initialized (context not ready)".to_string())
    }
}

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

impl TokenListModule for TokenListModuleImpl {
    fn on_context_ready(&mut self, ctx: &RustModuleContext) {
        self.tl = Some(TokenList::with_dir(std::path::PathBuf::from(&ctx.instance_persistence_path)));
    }

    fn configure(&mut self, config_json: String) -> bool {
        let cfg: ListConfig = match serde_json::from_str(&config_json) {
            Ok(c) => c,
            Err(_) => return false,
        };
        match self.tl() {
            Ok(tl) => {
                tl.configure(cfg);
                true
            }
            Err(_) => false,
        }
    }

    fn refresh_now(&mut self) -> String {
        let res = match self.tl() {
            Ok(tl) => tl.refresh_now(),
            Err(e) => return err(e),
        };
        match res {
            Ok(count) => {
                let chains = self.tl().map(|tl| tl.chains()).unwrap_or_default();
                for c in chains {
                    emit_tokens_updated(c as i64);
                }
                json!({ "ok": true, "tokenCount": count }).to_string()
            }
            Err(e) => err(e),
        }
    }

    fn get_tokens(&mut self, chain_id: i64) -> String {
        match self.tl() {
            Ok(tl) => json!({ "ok": true, "tokens": tl.get_tokens(chain_id as u64) }).to_string(),
            Err(e) => err(e),
        }
    }

    fn get_all_tokens(&mut self) -> String {
        match self.tl() {
            Ok(tl) => json!({ "ok": true, "tokens": tl.get_all_tokens() }).to_string(),
            Err(e) => err(e),
        }
    }

    fn add_custom_token(&mut self, token_json: String) -> bool {
        let token: Token = match serde_json::from_str(&token_json) {
            Ok(t) => t,
            Err(_) => return false,
        };
        let chain = token.chain_id;
        match self.tl() {
            Ok(tl) => {
                tl.add_custom_token(token);
                emit_tokens_updated(chain as i64);
                true
            }
            Err(_) => false,
        }
    }

    fn remove_custom_token(&mut self, chain_id: i64, address: String) -> bool {
        let changed = match self.tl() {
            Ok(tl) => tl.remove_custom_token(chain_id as u64, &address),
            Err(_) => return false,
        };
        if changed {
            emit_tokens_updated(chain_id);
        }
        changed
    }

    fn get_custom_tokens(&mut self) -> String {
        match self.tl() {
            Ok(tl) => json!({ "ok": true, "tokens": tl.get_custom_tokens() }).to_string(),
            Err(e) => err(e),
        }
    }

    fn get_list_sources(&mut self) -> String {
        match self.tl() {
            Ok(tl) => json!({ "ok": true, "sources": tl.get_list_sources() }).to_string(),
            Err(e) => err(e),
        }
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<TokenListModuleImpl>();
}
