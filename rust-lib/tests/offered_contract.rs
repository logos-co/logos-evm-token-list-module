const GLUE: &str = include_str!("../src/glue.rs");
const TOKENS: &str = include_str!("../src/tokens.rs");
const STORE: &str = include_str!("../src/store.rs");

fn body<'a>(source: &'a str, signature: &str) -> &'a str {
    let start = source.find(signature).unwrap_or_else(|| panic!("missing {signature}"));
    let open = start + source[start..].find('{').expect("function body");
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[open..=open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unterminated {signature}")
}

fn glue_impl() -> &'static str {
    GLUE.rsplit_once("impl TokenListModule for").expect("module implementation").1
}

#[test]
fn enabling_reads_the_catalogue_before_it_writes_the_snapshot() {
    let source = body(TOKENS, "pub fn set_token_enabled(");
    let read = source.find(".get_tokens(chain_id)").expect("catalogue snapshot");
    let write = source.find("write_json_atomic").expect("atomic persisted write");
    assert!(read < write, "the catalogue must be read before enabled_tokens.json is written");
}

#[test]
fn enabled_rows_have_one_persisted_writer() {
    assert_eq!(TOKENS.matches("write_json_atomic(\"enabled_tokens.json\"").count(), 2);
    assert!(body(TOKENS, "fn remove_enabled(").contains("enabled_tokens.json"));
    assert!(body(TOKENS, "pub fn set_token_enabled(").contains("enabled_tokens.json"));
}

#[test]
fn offered_readers_are_silent() {
    for method in ["fn get_enabled_tokens(", "fn list_offered(", "fn list_available("] {
        let source = body(glue_impl(), method);
        assert!(!source.contains("emit_"), "{method} emits from a reader");
    }
}

#[test]
fn the_enabled_mutator_uses_the_shared_chain_event_diff() {
    let source = body(glue_impl(), "fn set_token_enabled(");
    assert!(source.contains("with_chain_events"));
    assert!(!source.contains("emit_tokens_updated"));
}

#[test]
fn chain_digests_cover_both_catalogue_and_offered_rows() {
    let source = body(TOKENS, "pub fn chain_digests(");
    assert!(source.contains("get_tokens(c)"));
    assert!(source.contains("list_offered(c)"));
}

#[test]
fn persisted_enabled_sets_use_write_then_rename() {
    assert!(STORE.contains("std::fs::write(&tmp, text)"));
    assert!(STORE.contains("std::fs::rename(&tmp, path)"));
    assert!(!STORE.contains("std::fs::write(path, text)"));
}

#[test]
fn every_declared_event_is_emitted() {
    for event in ["tokens_updated", "config_changed"] {
        assert!(GLUE.contains(&format!("fn {event}(")), "missing event declaration: {event}");
        assert!(GLUE.contains(&format!("emit_{event}(")), "missing event emission: {event}");
    }
}
