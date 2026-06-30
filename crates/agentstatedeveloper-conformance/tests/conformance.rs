//! Cross-language conformance: capability matrix + contract interop.

use agentstatedeveloper_conformance::{
    cross_repo_edges, expected_matrix, fixture_for, inbound_endpoints, live_matrix,
    outbound_endpoints, COLUMNS,
};
use agentstatedeveloper_adapters::default_adapters;

fn render_matrix(rows: &[(String, [bool; 5])]) -> String {
    let mut out = String::new();
    out.push_str(&format!("{:<12}", "language"));
    for c in COLUMNS {
        out.push_str(&format!(" {:>11}", c));
    }
    out.push('\n');
    for (lang, cells) in rows {
        out.push_str(&format!("{lang:<12}"));
        for c in cells {
            out.push_str(&format!(" {:>11}", if *c { "ok" } else { "--" }));
        }
        out.push('\n');
    }
    out
}

/// Print-only discovery: dump the live matrix so we can fill `expected_matrix`.
/// Always passes; run with `--nocapture` to read it.
#[test]
fn print_live_matrix() {
    let rows: Vec<(String, [bool; 5])> = live_matrix()
        .into_iter()
        .map(|(lang, caps)| (lang, caps.cells()))
        .collect();
    eprintln!("\n=== LIVE CAPABILITY MATRIX ===\n{}", render_matrix(&rows));
}

/// Every built-in adapter must have a conformance fixture.
#[test]
fn every_adapter_has_a_fixture() {
    let missing: Vec<String> = default_adapters()
        .iter()
        .map(|a| a.language().to_string())
        .filter(|lang| fixture_for(lang).is_none())
        .collect();
    assert!(missing.is_empty(), "adapters without a fixture: {missing:?}");
}

/// The live matrix must match the spec. A regression flips a cell to `--`;
/// a newly-closed gap flips one to `ok`. Either way, update `expected_matrix`.
#[test]
fn matrix_matches_spec() {
    let expected = expected_matrix();
    if expected.is_empty() {
        // Discovery phase: spec not yet filled. `print_live_matrix` carries the data.
        return;
    }
    let live: Vec<(String, [bool; 5])> = live_matrix()
        .into_iter()
        .map(|(lang, caps)| (lang, caps.cells()))
        .collect();

    let live_rows: Vec<(String, [bool; 5])> = live.clone();
    for (lang, want) in &expected {
        let got = live
            .iter()
            .find(|(l, _)| l == lang)
            .unwrap_or_else(|| panic!("no live row for {lang}"));
        assert_eq!(
            &got.1, want,
            "\ncapability drift for `{lang}`\nexpected: {want:?}\n   found: {:?}\ncolumns: {COLUMNS:?}\n\nfull live matrix:\n{}",
            got.1,
            render_matrix(&live_rows)
        );
    }
}

/// The contract-keyed layer is language-agnostic: any fixture's inbound
/// `GET /users/{}` route must match any *other* fixture's outbound client call
/// to the same contract, across repo boundaries.
#[test]
fn contracts_match_across_languages() {
    let adapters = default_adapters();
    let want_contract = "http:GET /users/{}";

    // Collect, per language, its inbound and outbound endpoints (own repo id).
    let mut langs: Vec<(String, Vec<_>, Vec<_>)> = Vec::new();
    for a in adapters.iter() {
        let lang = a.language().to_string();
        let Some((file, source)) = fixture_for(&lang) else {
            continue;
        };
        let repo = format!("repo:{lang}");
        let ins = inbound_endpoints(a.as_ref(), file, source, &repo);
        let outs = outbound_endpoints(a.as_ref(), file, source, &repo);
        langs.push((lang, ins, outs));
    }

    let inbound_langs: Vec<&str> = langs
        .iter()
        .filter(|(_, ins, _)| ins.iter().any(|e| e.contract == want_contract))
        .map(|(l, _, _)| l.as_str())
        .collect();
    let outbound_langs: Vec<&str> = langs
        .iter()
        .filter(|(_, _, outs)| outs.iter().any(|e| e.contract == want_contract))
        .map(|(l, _, _)| l.as_str())
        .collect();

    assert!(
        inbound_langs.len() >= 2,
        "need ≥2 languages exposing inbound {want_contract}; got {inbound_langs:?}"
    );
    assert!(
        !outbound_langs.is_empty(),
        "need ≥1 language with outbound {want_contract}; got {outbound_langs:?}"
    );

    // Every ordered (inbound-lang A, outbound-lang B) pair, A != B, must yield a
    // cross-repo edge on the shared contract.
    let mut checked = 0;
    for (la, ins, _) in &langs {
        if ins.is_empty() {
            continue;
        }
        for (lb, _, outs) in &langs {
            if la == lb || outs.is_empty() {
                continue;
            }
            let mut combined = ins.clone();
            combined.extend(outs.clone());
            let edges = cross_repo_edges(&combined);
            assert!(
                edges.iter().any(|e| e.contract == want_contract),
                "no cross-language contract edge: inbound `{la}` × outbound `{lb}` (contract {want_contract})"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no cross-language pairs were exercised");
    eprintln!("contracts_match_across_languages: {checked} cross-language pairs matched on {want_contract}");
}
