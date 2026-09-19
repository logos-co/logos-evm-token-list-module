//! A refresh waits on the network with the store released, so reads never queue behind it.

const GLUE: &str = include_str!("../src/glue.rs");
const METADATA: &str = include_str!("../../metadata.json");

fn span(source: &str, open: usize, pair: (char, char)) -> std::ops::Range<usize> {
    let mut depth = 0usize;
    for (offset, ch) in source[open..].char_indices() {
        if ch == pair.0 {
            depth += 1;
        } else if ch == pair.1 {
            depth -= 1;
            if depth == 0 {
                return open..open + offset + 1;
            }
        }
    }
    panic!("unbalanced {pair:?} at {open}")
}

fn body<'a>(source: &'a str, signature: &str) -> &'a str {
    let start = source.find(signature).unwrap_or_else(|| panic!("missing {signature}"));
    &source[span(source, start + source[start..].find('{').expect("function body"), ('{', '}'))]
}

fn refresh(glue: &str) -> &str {
    body(glue.rsplit_once("impl TokenListModule for").expect("module implementation").1, "fn refresh_now(")
}

/// Every fetch sits outside every call that holds the store, between the plan and the apply.
fn fetches_unlocked(glue: &str) -> bool {
    let src = refresh(glue);
    let fetches: Vec<usize> = src.match_indices(".fetch()").map(|(i, _)| i).collect();
    let held: Vec<_> = ["self.read(", "self.with_chain_events(", "self.tl.read(", "self.tl.write("]
        .iter()
        .flat_map(|h| src.match_indices(h).map(move |(i, _)| span(src, i + h.len() - 1, ('(', ')'))))
        .collect();
    let (plan, apply) = (src.find("refresh_plan"), src.find("apply_refresh"));
    !fetches.is_empty()
        && fetches.iter().all(|f| !held.iter().any(|s| s.contains(f)))
        && !src.contains(".refresh_now()")
        && plan < fetches.first().copied()
        && fetches.last().copied() < apply
}

fn mutate(src: &str, from: &str, to: &str) -> String {
    assert_eq!(src.matches(from).count(), 1, "mutation target must occur exactly once: {from}");
    src.replacen(from, to, 1)
}

#[test]
fn calls_are_dispatched_concurrently() {
    assert!(METADATA.contains(r#""concurrency": "multi""#));
}

#[test]
fn a_refresh_fetches_with_the_store_released() {
    assert!(fetches_unlocked(GLUE));
    let inside = mutate(GLUE, "tl.apply_refresh(&plan, fetched)", "tl.apply_refresh(&plan, plan.fetch())");
    assert!(!fetches_unlocked(&inside), "a fetch inside the apply's write lock went unnoticed");
    let whole = mutate(GLUE, "tl.apply_refresh(&plan, fetched)", "tl.refresh_now()");
    assert!(!fetches_unlocked(&whole), "the old fetch-under-the-lock refresh went unnoticed");
}

#[test]
fn chain_events_are_announced_once_the_lock_is_released() {
    let src = body(GLUE, "fn with_chain_events<");
    let block = src.find("let (out, moved) = {").expect("the locked block");
    let held = span(src, block + src[block..].find('{').expect("its brace"), ('{', '}'));
    assert!(src.find("emit_tokens_updated").is_some_and(|e| e > held.end));
}
