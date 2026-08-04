//! pgmind-eval — gate-suite runner for RFC-002 (invoked by eval/harness.py).
//! Emits one JSON object to stdout. Exit 0 = suite ran (status inside JSON).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use pgmind_core::{parse, LinkKind};
use serde_json::{json, Value};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("conformance") => conformance(&args[1]),
        Some("roundtrip") => roundtrip(&args[1..]),
        Some("fuzz-roundtrip") => fuzz_roundtrip(
            args[1].parse().expect("count"),
            args.get(2)
                .map(|s| s.parse().expect("seed"))
                .unwrap_or(0xC0FFEE),
        ),
        Some("hash-goldens") => hash_goldens(&args[1]),
        Some("perf") => perf(),
        Some("extraction-goldens") => extraction_goldens(&args[1]),
        _ => {
            eprintln!("usage: pgmind-eval <conformance|roundtrip|fuzz-roundtrip|hash-goldens|extraction-goldens> …");
            std::process::exit(2);
        }
    };
    println!("{}", serde_json::to_string_pretty(&result).unwrap());
}

/// Gate 1: all CommonMark 0.31.2 spec examples in the pure configuration.
fn conformance(spec_path: &str) -> Value {
    let spec: Vec<Value> = serde_json::from_str(&fs::read_to_string(spec_path).expect("read spec"))
        .expect("parse spec");
    let mut failures = vec![];
    for example in &spec {
        let md = example["markdown"].as_str().unwrap();
        let expected = example["html"].as_str().unwrap();
        let got = pgmind_core::conformance::render_pure_commonmark(md);
        if got != expected {
            failures.push(example["example"].as_i64().unwrap_or(-1));
        }
    }
    json!({
        "status": if failures.is_empty() { "ok" } else { "fail" },
        "total": spec.len(),
        "passed": spec.len() - failures.len(),
        "failures": failures,
    })
}

/// Gate 2 (corpus part): parse∘serialize byte identity via top-level tiling.
fn roundtrip(paths: &[String]) -> Value {
    let mut files: Vec<PathBuf> = vec![];
    for p in paths {
        collect_md(Path::new(p), &mut files);
    }
    files.sort();
    let mut failures = vec![];
    for f in &files {
        let Ok(source) = fs::read_to_string(f) else {
            failures.push(format!("{} (unreadable)", f.display()));
            continue;
        };
        let doc = parse(&source);
        if !doc.tiles_exactly(&source) {
            failures.push(f.display().to_string());
        }
    }
    json!({
        "status": if failures.is_empty() { "ok" } else { "fail" },
        "total": files.len(),
        "passed": files.len() - failures.len(),
        "failures": failures,
    })
}

fn collect_md(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for e in entries.flatten() {
                collect_md(&e.path(), out);
            }
        }
    } else if path.extension().is_some_and(|e| e == "md") {
        out.push(path.to_path_buf());
    }
}

/// Gate 2 (fuzz part): deterministic pseudo-random documents; tiling must hold.
fn fuzz_roundtrip(count: u64, seed: u64) -> Value {
    let mut rng = seed | 1;
    let mut next = move || {
        // xorshift64* — deterministic, no external randomness (workflow rules).
        rng ^= rng >> 12;
        rng ^= rng << 25;
        rng ^= rng >> 27;
        rng.wrapping_mul(0x2545F4914F6CDD1D)
    };
    let fragments: &[&str] = &[
        "# Heading {n}\n",
        "## Sub *{n}* [[note-{n}|alias]]\n",
        "Para {n} with [[target#Sec]] and #tag{n} text.\n",
        "Para with trailing spaces  \nhard break {n}.\n",
        "- item {n}\n- item [[x{n}]]\n  - nested {n}\n",
        "1. ordered {n}\n   continuation line\n",
        "> quoted {n}\n> more [[q{n}]]\n",
        "> lazy quote {n}\nlazy continuation\n",
        "```rust\nfn f{n}() {{}}\n```\n",
        "    indented code {n}\n",
        "| a | b |\n|---|---|\n| {n} | [[c\\|d]] |\n",
        "---\n",
        "Text with `code #notag [[nolink]]` span {n}.\n",
        "[ref{n}]: https://example.com/{n}\nUse [ref{n}] here.\n",
        "Block with id {n}. ^id-{n}\n",
        "<div>\nhtml block {n}\n</div>\n",
        "Setext {n}\n======\n",
        "\u{00e9}\u{0301} composed/decomposed {n}\n",
        "\n",
        "\n\n",
    ];
    let mut failures = 0u64;
    let mut first_failure: Option<String> = None;
    for i in 0..count {
        let mut doc = String::new();
        if next() % 4 == 0 {
            doc.push_str("---\ntitle: fuzz\ntags: [f]\n---\n");
        }
        let parts = 1 + (next() % 12);
        for _ in 0..parts {
            let frag = fragments[(next() % fragments.len() as u64) as usize];
            doc.push_str(&frag.replace("{n}", &i.to_string()));
        }
        let parsed = parse(&doc);
        if !parsed.tiles_exactly(&doc) {
            failures += 1;
            if first_failure.is_none() {
                first_failure = Some(doc.clone());
            }
        }
    }
    json!({
        "status": if failures == 0 { "ok" } else { "fail" },
        "total": count,
        "passed": count - failures,
        "first_failure": first_failure,
    })
}

/// Gate 3: golden vectors — pairs of (doc, block ord) with expected hash relation.
fn hash_goldens(path: &str) -> Value {
    let cases: Vec<Value> = serde_json::from_str(&fs::read_to_string(path).expect("read goldens"))
        .expect("parse goldens");
    let mut failures = vec![];
    for case in &cases {
        let name = case["name"].as_str().unwrap_or("?");
        let a = parse(case["doc_a"].as_str().unwrap());
        let b = parse(case["doc_b"].as_str().unwrap());
        let ba = case["block_a"].as_u64().unwrap_or(0) as usize;
        let bb = case["block_b"].as_u64().unwrap_or(0) as usize;
        let (Some(block_a), Some(block_b)) = (a.blocks.get(ba), b.blocks.get(bb)) else {
            failures.push(json!({ "name": name, "reason": "block index out of range",
                "blocks_a": a.blocks.len(), "blocks_b": b.blocks.len() }));
            continue;
        };
        let equal = block_a.content_hash == block_b.content_hash;
        let expect_equal = match case["expect"].as_str() {
            Some("equal") => true,
            Some("different") => false,
            other => {
                failures.push(json!({ "name": name, "reason": "invalid expect", "expect": other }));
                continue;
            }
        };
        if equal != expect_equal {
            failures.push(json!({
                "name": name,
                "expected": case["expect"],
                "normalized_a": block_a.normalized_content,
                "normalized_b": block_b.normalized_content,
            }));
        }
    }
    json!({
        "status": if failures.is_empty() { "ok" } else { "fail" },
        "total": cases.len(),
        "passed": cases.len() - failures.len(),
        "failures": failures,
    })
}

/// Gate 4: golden extraction cases (links, tags, ref ids, properties, kinds).
fn extraction_goldens(path: &str) -> Value {
    let cases: Vec<Value> = serde_json::from_str(&fs::read_to_string(path).expect("read goldens"))
        .expect("parse goldens");
    let mut failures = vec![];
    for case in &cases {
        let name = case["name"].as_str().unwrap_or("?");
        let doc = parse(case["doc"].as_str().unwrap());
        let mut problems = vec![];

        if let Some(expected) = case.get("links") {
            let got: Vec<Value> = doc
                .links
                .iter()
                .map(|l| {
                    let mut o = json!({ "kind": kind_str(l.kind), "target": l.target });
                    if let Some(a) = &l.anchor {
                        o["anchor"] = json!(a);
                    }
                    if let Some(a) = &l.alias {
                        o["alias"] = json!(a);
                    }
                    o
                })
                .collect();
            if &Value::Array(got.clone()) != expected {
                problems.push(json!({ "field": "links", "got": got, "expected": expected }));
            }
        }
        if let Some(expected) = case.get("tags") {
            let got: Vec<&str> = doc.tags.iter().map(|t| t.tag.as_str()).collect();
            if &json!(got) != expected {
                problems.push(json!({ "field": "tags", "got": got, "expected": expected }));
            }
        }
        if let Some(expected) = case.get("block_ref_ids") {
            let got: Vec<&str> = doc
                .blocks
                .iter()
                .filter_map(|b| b.block_ref_id.as_deref())
                .collect();
            if &json!(got) != expected {
                problems
                    .push(json!({ "field": "block_ref_ids", "got": got, "expected": expected }));
            }
        }
        if let Some(expected) = case.get("properties") {
            if &doc.properties != expected {
                problems.push(
                    json!({ "field": "properties", "got": doc.properties, "expected": expected }),
                );
            }
        }
        if let Some(expected) = case.get("kinds") {
            let got: Vec<&str> = doc.blocks.iter().map(|b| b.kind.tag()).collect();
            if &json!(got) != expected {
                problems.push(json!({ "field": "kinds", "got": got, "expected": expected }));
            }
        }
        if let Some(expected) = case.get("heading_paths") {
            let got: Vec<Vec<String>> = doc.blocks.iter().map(|b| b.heading_path.clone()).collect();
            if &json!(got) != expected {
                problems
                    .push(json!({ "field": "heading_paths", "got": got, "expected": expected }));
            }
        }
        let asserted = [
            "links",
            "tags",
            "block_ref_ids",
            "properties",
            "kinds",
            "heading_paths",
        ]
        .iter()
        .any(|k| case.get(*k).is_some());
        if !asserted {
            problems.push(json!({ "field": "-", "reason": "case asserts nothing" }));
        }
        if !problems.is_empty() {
            failures.push(json!({ "name": name, "problems": problems }));
        }
    }
    json!({
        "status": if failures.is_empty() { "ok" } else { "fail" },
        "total": cases.len(),
        "passed": cases.len() - failures.len(),
        "failures": failures,
    })
}

/// Pathological-input performance gate: each construction must parse within
/// PERF_LIMIT (guards the O(n^2) hotspots found in review — unclosed wiki
/// spam, tiny-paragraph floods, item floods).
fn perf() -> Value {
    const PERF_LIMIT_SECS: f64 = 10.0;
    let cases: Vec<(&str, String)> = vec![
        ("unclosed-wiki-spam", "[[a".repeat(700_000)),
        ("tiny-paragraph-flood", "x\n\n".repeat(700_000)),
        ("list-item-flood", "- x\n".repeat(700_000)),
        ("single-line-8mib", "word ".repeat(400_000)),
    ];
    let mut results = vec![];
    let mut failed = false;
    for (name, doc) in &cases {
        let start = std::time::Instant::now();
        let parsed = parse(doc);
        let secs = start.elapsed().as_secs_f64();
        let ok = secs < PERF_LIMIT_SECS && parsed.tiles_exactly(doc);
        failed |= !ok;
        results.push(json!({
            "name": name, "bytes": doc.len(), "seconds": secs,
            "blocks": parsed.blocks.len(), "ok": ok,
        }));
    }
    json!({
        "status": if failed { "fail" } else { "ok" },
        "limit_seconds": PERF_LIMIT_SECS,
        "cases": results,
    })
}

fn kind_str(k: LinkKind) -> &'static str {
    match k {
        LinkKind::Wikilink => "wikilink",
        LinkKind::Transclusion => "transclusion",
        LinkKind::Mdlink => "mdlink",
        LinkKind::Blockref => "blockref",
    }
}
