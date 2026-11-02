//! Current-worktree excerpts and bounded JSON for CLI context-for.
//! Indexed ranges locate evidence; they do NOT prove the index is fresh.
use std::io::Read;
use std::path::Path;

use serde_json::{Value, json};

const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// These are byte-identity states, not confidence/quality buckets. `matched`
/// certifies this symbol's indexed file/range only, never graph completeness.
pub(super) fn verify_indexed_source(
    mut source: Value,
    indexed: Option<&Value>,
    symbol: &agentstatedeveloper_core::Symbol,
) -> Value {
    let Some(indexed) = indexed else {
        return source;
    };
    let Some(expected) = indexed["file_blake3"].as_str() else {
        return source;
    };
    let binding_matches = indexed["file"] == symbol.file
        && indexed["symbol_fp"] == symbol.symbol_fp
        && indexed["start_line"] == symbol.start.line
        && indexed["end_line"] == symbol.end.line
        && expected.len() == 64
        && expected.bytes().all(|b| b.is_ascii_hexdigit());
    if !binding_matches {
        source["index_match"] = json!("not_verified");
        source["provenance_warning"] = json!("provenance does not match the indexed symbol record");
        return source;
    }
    source["indexed_file_blake3"] = json!(expected);
    let Some(current) = source["file_blake3"].as_str() else {
        return source;
    };
    if current == expected {
        source["index_match"] = json!("matched");
        source
    } else {
        // Suppress text rather than presenting a different function at an old
        // range as if it were the requested symbol.
        json!({"status":"unavailable","reason":"source changed since indexing; reindex before using indexed ranges",
            "index_match":"mismatch","file_blake3":current,"indexed_file_blake3":expected})
    }
}

pub(super) fn source_excerpt(root: &Path, file: &str, start: u32, end: u32, lines: usize) -> Value {
    let unavailable = |reason: &str| json!({"status":"unavailable", "reason":reason,"index_match":"not_verified"});
    if start == 0 || end < start {
        return unavailable("invalid indexed range");
    }
    let Ok(root) = root.canonicalize() else {
        return unavailable("worktree root unavailable");
    };
    let Ok(path) = root.join(file).canonicalize() else {
        return unavailable("source file unavailable");
    };
    if !path.starts_with(&root) {
        return unavailable("source resolves outside current worktree");
    }
    let Ok(metadata) = path.metadata() else {
        return unavailable("source metadata unavailable");
    };
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return unavailable("source is not a regular file within the 1 MiB read limit");
    }
    let mut bytes = Vec::new();
    let read =
        std::fs::File::open(&path).and_then(|f| f.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes));
    if read.is_err() || bytes.len() as u64 > MAX_FILE_BYTES {
        return unavailable("source read failed or exceeded limit");
    }
    let file_blake3 = blake3::hash(&bytes).to_hex().to_string();
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return json!({"status":"unavailable","reason":"source is not UTF-8","file_blake3":file_blake3,"index_match":"not_verified"});
    };
    let selected: Vec<_> = text
        .lines()
        .skip(start as usize - 1)
        .take((end - start + 1) as usize)
        .take(lines)
        .collect();
    if selected.is_empty() {
        return json!({"status":"unavailable","reason":"indexed range is outside current source","file_blake3":file_blake3,"index_match":"not_verified"});
    }
    let joined = selected.join("\n");
    let mut boundary = joined.len().min(4096);
    while !joined.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let excerpt = &joined[..boundary];
    let returned_end = start as usize + excerpt.lines().count().saturating_sub(1);
    // The excerpt fingerprint identifies current bytes, not indexed bytes.
    json!({"status":"available", "origin":"current_worktree", "file_blake3":file_blake3,
        "index_match":"not_verified", "start_line":start, "end_line":returned_end,
        "indexed_end_line":end, "truncated":returned_end < end as usize || boundary < joined.len(),
        "complete_lines":boundary == joined.len(), "text":excerpt})
}

fn fits(value: &Value, bytes: usize) -> bool {
    // Include the newline printed by the CLI.
    serde_json::to_vec(value).unwrap().len() < bytes
}

/// A byte ceiling, not a tokenizer. Never slice serialized JSON or split UTF-8.
/// Any lost information is signalled; missing callers must not mean no callers.
pub(super) fn bounded(mut value: Value, bytes: usize) -> Value {
    let requested = value["symbols"].as_array().map_or(0, Vec::len);
    value["_budget"] = json!({"max_utf8_bytes":bytes,"truncated":false,
        "omitted_items":0,"omitted_symbols":0,"source_text_trimmed":false});
    if fits(&value, bytes) {
        return value;
    }
    value["_budget"]["truncated"] = json!(true);
    // Remove verbose optional metadata before source evidence and safety guards.
    for key in [
        "decisions_and_notes",
        "proofs",
        "ownership_discovery",
        "ownership",
        "concepts",
        "validation_scenarios",
        "effects_detail",
        "effects",
        "cross_service",
        "dataflow",
    ] {
        for symbol in value["symbols"].as_array_mut().unwrap() {
            if let Some(removed) = symbol.as_object_mut().unwrap().remove(key) {
                if removed != Value::Null && removed != json!([]) && removed != json!({}) {
                    // All omissions are signalled by the global truncated bit.
                    symbol["context_metadata_omitted"] = json!(true);
                }
            }
        }
        if fits(&value, bytes) {
            return value;
        }
    }
    // Shorten the longest source excerpt first. Keep locations, hashes and flags.
    loop {
        let mut longest: Option<(usize, Option<usize>, usize)> = None;
        for (i, symbol) in value["symbols"].as_array().unwrap().iter().enumerate() {
            let size = symbol["source"]["text"].as_str().map_or(0, str::len);
            if size > longest.map_or(0, |x| x.2) {
                longest = Some((i, None, size));
            }
            if let Some(callers) = symbol["callers"].as_array() {
                for (j, caller) in callers.iter().enumerate() {
                    let size = caller["source"]["text"].as_str().map_or(0, str::len);
                    if size > longest.map_or(0, |x| x.2) {
                        longest = Some((i, Some(j), size));
                    }
                }
            }
        }
        let Some((i, caller, _)) = longest else { break };
        let source = if let Some(j) = caller {
            &mut value["symbols"][i]["callers"][j]["source"]
        } else {
            &mut value["symbols"][i]["source"]
        };
        let text = source["text"].as_str().unwrap();
        let shorter: String = text.chars().take(text.chars().count() / 2).collect();
        source["text"] = json!(shorter);
        let line_count = source["text"].as_str().unwrap().lines().count();
        source["end_line"] = source["start_line"]
            .as_u64()
            .and_then(|start| {
                if line_count == 0 {
                    None
                } else {
                    Some(json!(start + line_count as u64 - 1))
                }
            })
            .unwrap_or(Value::Null);
        source["truncated"] = json!(true);
        source["complete_lines"] = json!(false);
        source["budget_trimmed"] = json!(true);
        value["_budget"]["source_text_trimmed"] = json!(true);
        if fits(&value, bytes) {
            return value;
        }
    }
    // Trim arrays with an explicit omitted-item count; never hide a safety guard
    // while retaining an apparently complete symbol.
    let mut omitted = 0;
    for key in [
        "covering_tests",
        "callees",
        "callers",
        "runtime_missed_callees",
    ] {
        for i in 0..requested {
            loop {
                let removed = value["symbols"][i][key].as_array_mut().and_then(Vec::pop);
                if removed.is_none() {
                    break;
                }
                omitted += 1;
                value["_budget"]["omitted_items"] = json!(omitted);
                if fits(&value, bytes) {
                    return value;
                }
            }
        }
    }
    while !value["symbols"].as_array().unwrap().is_empty() {
        value["symbols"].as_array_mut().unwrap().pop();
        let remaining = value["symbols"].as_array().unwrap().len();
        value["count"] = json!(remaining);
        value["_budget"]["omitted_symbols"] = json!(requested - remaining);
        if fits(&value, bytes) {
            return value;
        }
    }
    // Even the query string can be larger than the entire budget.
    json!({"count":0,"symbols":[],"_budget":{"max_utf8_bytes":bytes,"truncated":true,
        "omitted_symbols":requested,"query_omitted":true}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn freshness_requires_matching_file_and_symbol_binding() {
        let symbol: agentstatedeveloper_core::Symbol = serde_json::from_value(json!({
            "symbol_id":"sym_a","symbol_fp":"fp_a","qname":"f","file":"f.rs",
            "language":"rust","kind":"function","start":{"line":2,"col":0},
            "end":{"line":4,"col":1},"signature":null
        }))
        .unwrap();
        let source = json!({"status":"available","file_blake3":"a".repeat(64),"text":"fn f() {}","index_match":"not_verified"});
        let record = json!({"file":"f.rs","symbol_fp":"fp_a","start_line":2,"end_line":4,"file_blake3":"a".repeat(64)});
        assert_eq!(
            verify_indexed_source(source.clone(), Some(&record), &symbol)["index_match"],
            "matched"
        );
        assert_eq!(
            verify_indexed_source(source.clone(), None, &symbol)["index_match"],
            "not_verified"
        );
        let mut changed = source.clone();
        changed["file_blake3"] = json!("b".repeat(64));
        let stale = verify_indexed_source(changed, Some(&record), &symbol);
        assert_eq!(stale["index_match"], "mismatch");
        assert!(stale.get("text").is_none());
        for key in ["symbol_fp", "start_line", "file_blake3"] {
            let mut bad = record.clone();
            bad[key] = json!("invalid");
            assert_eq!(
                verify_indexed_source(source.clone(), Some(&bad), &symbol)["index_match"],
                "not_verified"
            );
        }
    }
    #[test]
    fn source_ranges_hashes_changes_and_unavailable_states() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("sample.rs");
        std::fs::write(&p, "// header\nfn café() {\n  work();\n}\n").unwrap();
        let first = source_excerpt(tmp.path(), "sample.rs", 2, 4, 2);
        assert_eq!(first["text"], "fn café() {\n  work();");
        assert_eq!(first["end_line"], 3);
        assert_eq!(first["truncated"], true);
        assert_eq!(first["index_match"], "not_verified");
        std::fs::write(&p, "// changed\nfn café() {\n  changed();\n}\n").unwrap();
        let second = source_excerpt(tmp.path(), "sample.rs", 2, 4, 80);
        assert_ne!(first["file_blake3"], second["file_blake3"]);
        assert!(second["text"].as_str().unwrap().contains("changed()"));
        assert_eq!(
            source_excerpt(tmp.path(), "missing.rs", 1, 2, 80)["status"],
            "unavailable"
        );
        assert_eq!(
            source_excerpt(tmp.path(), "sample.rs", 0, 2, 80)["status"],
            "unavailable"
        );
        assert_eq!(
            source_excerpt(tmp.path(), "sample.rs", 99, 100, 80)["status"],
            "unavailable"
        );
        std::fs::write(&p, "🦀".repeat(2000)).unwrap();
        let long_line = source_excerpt(tmp.path(), "sample.rs", 1, 1, 80);
        assert_eq!(long_line["text"].as_str().unwrap().len(), 4096);
        assert_eq!(long_line["truncated"], true);
        std::fs::write(&p, [0xff]).unwrap();
        assert_eq!(
            source_excerpt(tmp.path(), "sample.rs", 1, 1, 80)["status"],
            "unavailable"
        );
        std::fs::write(&p, vec![b'x'; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert_eq!(
            source_excerpt(tmp.path(), "sample.rs", 1, 2, 80)["status"],
            "unavailable"
        );
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outside.rs"), "secret").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside.rs"),
            tmp.path().join("alias.rs"),
        )
        .unwrap();
        let v = source_excerpt(tmp.path(), "alias.rs", 1, 1, 80);
        assert_eq!(v["status"], "unavailable");
        assert!(!v.to_string().contains("secret"));
    }
    #[test]
    fn budget_is_valid_json_byte_bound_even_for_unicode_escapes_and_huge_query() {
        let value = json!({"query":"huge".repeat(10000),"count":1,"symbols":[{
            "symbol":{"qname":"f"},"invariants":["must preserve when retained"],"hazards":[],
            "source":{"text":"🦀\"\n".repeat(2000)},"callers":[{"source":{"text":"界".repeat(1000)}}]}]});
        for bytes in [512, 1024, 2048, 8192] {
            let v = bounded(value.clone(), bytes);
            let encoded = serde_json::to_vec(&v).unwrap();
            assert!(encoded.len() < bytes);
            assert!(serde_json::from_slice::<Value>(&encoded).is_ok());
            assert_eq!(v["_budget"]["truncated"], true);
        }
        let small = json!({"query":"f","count":1,"symbols":[{"symbol":{"qname":"f"},"invariants":["guard"],"hazards":[]}]});
        let v = bounded(small, 512);
        assert_eq!(v["_budget"]["truncated"], false);
        assert_eq!(v["symbols"][0]["invariants"][0], "guard");
    }

    #[test]
    fn budget_preserves_guards_while_removing_verbose_context() {
        let value = json!({"query":"f","count":1,"symbols":[{
            "symbol":{"qname":"f"},"invariants":["must validate input"],"hazards":["public API"],
            "decisions_and_notes":["verbose".repeat(1000)],
            "source":{"start_line":10,"text":"fn example() {}\n".repeat(200)}}]});
        let v = bounded(value, 2048);
        assert!(serde_json::to_vec(&v).unwrap().len() < 2048);
        assert_eq!(v["symbols"][0]["invariants"][0], "must validate input");
        assert_eq!(v["symbols"][0]["hazards"][0], "public API");
        assert_eq!(v["symbols"][0]["source"]["truncated"], true);
        assert_eq!(v["symbols"][0]["context_metadata_omitted"], true);
        assert_eq!(v["_budget"]["truncated"], true);
    }
}
