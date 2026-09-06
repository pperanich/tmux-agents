//! The fixture corpus as a contract: the version inventory, the mapping expectations, and the
//! two halves of the unknown rule.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{assert_expected, describe, fixtures};
use crate::model::Store;
use crate::{Reader, Source, WindowRequest};

/// One store's declaration in `VERSIONS.toml`.
struct Declared {
    versions: Vec<String>,
    stamp_key: String,
    stamp_scope: String,
}

fn declarations() -> BTreeMap<String, Declared> {
    let path = fixtures().join("stores/VERSIONS.toml");
    let src = std::fs::read_to_string(&path).expect("read VERSIONS.toml");
    let table: toml::Table = src.parse().expect("VERSIONS.toml must parse");
    table
        .into_iter()
        .map(|(agent, body)| {
            let get = |k: &str| {
                body.get(k)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            let versions = body
                .get("versions")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            (
                agent,
                Declared {
                    versions,
                    stamp_key: get("stamp_key"),
                    stamp_scope: get("stamp_scope"),
                },
            )
        })
        .collect()
}

/// Every `.jsonl` under `dir`, recursively. A traversal error panics rather than truncating the
/// walk: a scan that silently found nothing would pass every assertion below on no evidence.
fn jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            jsonl_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
    out.sort();
}

/// `(agent, version)` from a path under `stores/`.
fn agent_and_version(path: &Path) -> (String, String) {
    let rel = path
        .strip_prefix(fixtures().join("stores"))
        .expect("a fixture under stores/");
    let mut parts = rel.components().map(|c| c.as_os_str().to_string_lossy());
    let agent = parts.next().expect("an agent directory").to_string();
    let version = parts.next().expect("a version directory").to_string();
    (agent, version)
}

fn read_events(store: Store, path: &Path) -> (Vec<crate::Event>, u64) {
    let source = Source {
        store,
        path: path.to_path_buf(),
        session: None,
    };
    let page = Reader::new()
        .window(&source, &WindowRequest::new(10_000).with_bodies())
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(
        page.older,
        None,
        "{} must fit in one page; grow the budget or shrink the fixture",
        path.display()
    );
    let mut events = page.events;
    events.reverse();
    (events, page.unknown)
}

/// The inventory holds in both directions, and every fixture's in-file version stamp agrees
/// with the version its path and filename claim.
#[test]
fn the_version_inventory_agrees_with_the_corpus() {
    let declared = declarations();
    let mut files = Vec::new();
    jsonl_files(&fixtures().join("stores"), &mut files);
    assert!(files.len() >= 5, "the walk is not seeing the corpus");

    let mut covered: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in &files {
        let (agent, version) = agent_and_version(path);
        let decl = declared.get(&agent).unwrap_or_else(|| {
            panic!("{agent} has fixtures but no VERSIONS.toml entry; add one naming its stamp")
        });
        assert!(
            decl.versions.contains(&version),
            "{}: version {version} is not declared for {agent}",
            path.display()
        );
        covered
            .entry(agent.clone())
            .or_default()
            .push(version.clone());

        // Only a fixture sitting directly in the version directory is a primary one; the nested
        // subagent transcripts are named by their child id instead.
        let primary = path
            .parent()
            .map(|p| p.ends_with(&version))
            .unwrap_or(false);
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if primary {
            assert!(
                name.contains(&version),
                "{name} does not carry the version it imitates ({version})"
            );
        }
        check_stamp(path, decl, &version);
    }

    for (agent, decl) in &declared {
        for version in &decl.versions {
            if decl.stamp_scope == "generated" {
                assert!(
                    generated(agent, version),
                    "{agent} {version} is declared as a generated store with no expectation \
                     committed beside it"
                );
                continue;
            }
            assert!(
                covered
                    .get(agent)
                    .is_some_and(|seen| seen.contains(version)),
                "{agent} {version} is declared in VERSIONS.toml with no fixture behind it"
            );
        }
    }
}

/// A store whose fixture is a database the suite builds rather than a file in the tree. What is
/// committed is the expectation, so that is what the inventory can hold it to.
fn generated(agent: &str, version: &str) -> bool {
    let dir = fixtures().join("stores").join(agent).join(version);
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".expected.txt"))
    })
}

/// The in-file stamp, read the way that store writes it.
fn check_stamp(path: &Path, decl: &Declared, version: &str) {
    if decl.stamp_scope == "filename" {
        return; // the store writes no version of its own; the filename is the only handle
    }
    let src = std::fs::read_to_string(path).expect("read the fixture");
    let keys: Vec<&str> = decl.stamp_key.split('.').collect();
    let mut seen = 0;
    for line in src.lines().filter(|l| !l.trim().is_empty()) {
        let value =
            crate::json::parse(line.trim()).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let Some(stamp) = value.path(&keys).and_then(crate::json::Value::as_str) else {
            continue;
        };
        assert_eq!(
            stamp,
            version,
            "{}: in-file stamp {stamp} disagrees with the path's {version}",
            path.display()
        );
        seen += 1;
        if decl.stamp_scope == "header" {
            break;
        }
    }
    assert!(
        seen > 0,
        "{} carries no {} stamp at all",
        path.display(),
        decl.stamp_key
    );
}

/// The version-inventory mapping half: every fixture's event sequence equals its committed
/// expectation, and no fixture under `stores/` produces an unknown.
#[test]
fn the_known_corpus_maps_cleanly_and_stays_pinned() {
    let mut files = Vec::new();
    jsonl_files(&fixtures().join("stores"), &mut files);
    for path in &files {
        let (agent, _) = agent_and_version(path);
        let store = Store::from_agent(&agent).unwrap_or_else(|| panic!("unknown agent {agent}"));
        let (events, unknown) = read_events(store, path);
        assert_eq!(
            unknown,
            0,
            "{} produced {unknown} unknown records; close the adapter hole or move it to drift/",
            path.display()
        );
        let lines: Vec<String> = events.iter().map(describe).collect();
        assert_expected(&path.with_extension("expected.txt"), &lines);
    }
}

/// The unknown-counter half: the drift corpus must raise the counter and must not error. The two rules
/// only look contradictory; the split is what lets a fixture refresh catch drift on the day it
/// lands while a released CLI update never breaks the reader.
#[test]
fn the_drift_corpus_raises_the_counter_without_erroring() {
    let mut files = Vec::new();
    jsonl_files(&fixtures().join("drift"), &mut files);
    assert!(!files.is_empty(), "the drift corpus must not be empty");
    for path in &files {
        let (events, unknown) = read_events(Store::Claude, path);
        assert!(
            unknown >= 2,
            "{} is meant to carry unknown record types",
            path.display()
        );
        assert!(
            !events.is_empty(),
            "{} must still parse the records it does know",
            path.display()
        );
    }
}
