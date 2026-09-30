// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The two *generated* flag documents have to still match the code.
//!
//! `docs/flag-tokens.md` and the Full inventory table in
//! `docs/config/flag-inventory.md` both say they are generated from
//! [`INVENTORY`]. Neither had anything checking it, and both drifted:
//!
//! * `flag-tokens.md` was three tokens behind and carried one that had been
//!   retired (`jit-bisect-skip`);
//! * `flag-inventory.md` was **42 rows** behind and claimed a declared count of
//!   647 against a true 689 — while its own header said "generated, not
//!   maintained" and `render-tokens.sh` claimed, wrongly, that
//!   `flag_surface.rs` "fails the build if this table and the code disagree".
//!
//! A generated file with no generator and no check is worse than a
//! hand-maintained one, because the "generated" label is exactly what stops
//! anyone from reading it sceptically. Regenerate with
//! `tools/flag-census/render-tokens.sh` and
//! `tools/flag-census/render-inventory.sh`.
//!
//! What is deliberately NOT asserted: the exact text of a row. These tests pin
//! the *set* of variables and the counts, which is what drifts. Pinning every
//! column would make an unrelated `Read in` change fail the build in a file the
//! author has no reason to look at, and the generator is the fix for that
//! anyway.

use cratonvm_types::flag_groups::{Group, INVENTORY, SCALARS};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("types/ always has a workspace root above it")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = workspace_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {} ({e}) — failing rather than passing without having \
             checked anything",
            path.display()
        )
    })
}

fn declared() -> BTreeSet<String> {
    INVENTORY
        .iter()
        .flat_map(|e| [e.on_key, e.off_key].into_iter().flatten())
        .chain(SCALARS.iter().copied())
        .chain(Group::ALL.iter().map(|g| g.var()))
        .map(str::to_string)
        .collect()
}

/// The first cell of a `| \`X\` | … |` row, unquoted, or `None`.
fn row_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("| `")?;
    let (name, _) = rest.split_once("` |")?;
    name.starts_with("CRATONVM_").then_some(name)
}

/// Every declared variable has a row, and every row that is not declared is
/// explicitly marked as an allowlisted one.
///
/// The first half is the drift that actually happened. The second stops the
/// table from quietly regrowing the surface the consolidation removed: an extra
/// row has to say `n/a (undeclared)` and `live getenv`, which is a claim someone
/// can check, rather than just existing.
#[test]
fn the_flag_inventory_table_matches_the_declared_surface() {
    let doc = read("docs/config/flag-inventory.md");
    let table = doc
        .split_once("## Full inventory")
        .expect("the Full inventory section")
        .1;

    let mut rows: BTreeSet<String> = BTreeSet::new();
    let mut undeclared_rows = 0usize;
    let declared = declared();
    for line in table.lines() {
        let Some(name) = row_name(line) else { continue };
        assert!(
            rows.insert(name.to_string()),
            "{name} appears twice in the Full inventory table"
        );
        if !declared.contains(name) {
            undeclared_rows += 1;
            assert!(
                line.contains("n/a (undeclared)") && line.contains("live getenv"),
                "{name} has a row but is not declared, so it must be marked \
                 `n/a (undeclared)` / `live getenv` and appear in \
                 `flag_declaration_guard.rs::ALLOWED`:\n  {line}"
            );
        }
    }

    // A scan that stops matching passes as happily as one that matches
    // everything. The table has hundreds of rows; a handful means the format
    // moved under `row_name` and this test is about to approve anything.
    assert!(
        rows.len() > 400,
        "only {} rows parsed out of the Full inventory table — the row format \
         changed and this test was about to check nothing",
        rows.len()
    );

    let missing: Vec<_> = declared.difference(&rows).cloned().collect();
    assert!(
        missing.is_empty(),
        "these variables are declared but have no row in the Full inventory \
         table of docs/config/flag-inventory.md. Regenerate it with \
         `tools/flag-census/render-inventory.sh`:\n  {}",
        missing.join("\n  ")
    );

    // The header states the same three numbers the table now demonstrates.
    let header = format!(
        "{} rows: {} declared, {} allowlisted.",
        rows.len(),
        declared.len(),
        undeclared_rows
    );
    assert!(
        table.contains(&header),
        "the Full inventory header does not state what the table contains — \
         expected {header:?}. Regenerate with \
         `tools/flag-census/render-inventory.sh`."
    );
    let count_claim = format!(
        "| **declared** in `flag_groups::INVENTORY` + scalars + group \
         variables | **{}** |",
        declared.len()
    );
    assert!(
        doc.contains(&count_claim),
        "the \"Where the surface stands\" declared count is stale — expected \
         {count_claim:?}"
    );
}

/// `docs/flag-tokens.md` lists exactly the tokens `INVENTORY` declares.
///
/// Its own header has always promised a test does this. Until now none did,
/// and it was three rows wrong.
#[test]
fn the_flag_token_reference_matches_the_inventory() {
    let doc = read("docs/flag-tokens.md");

    let mut listed: BTreeSet<(String, String)> = BTreeSet::new();
    for line in doc.lines() {
        // `| `token` | `CRATONVM_KEY` |`, and for a knob with an explicit
        // spelling in each direction the renderer puts BOTH keys in the one
        // cell: `| `aqs` | `CRATONVM_REAL_AQS / CRATONVM_SYNTHETIC_AQS` |`.
        // Splitting the cell is what makes this comparable to `INVENTORY`,
        // which holds them as two fields.
        let Some(rest) = line.strip_prefix("| `") else {
            continue;
        };
        let Some((token, rest)) = rest.split_once("` | `") else {
            continue;
        };
        let Some((cell, _)) = rest.split_once("` |") else {
            continue;
        };
        for key in cell.split(" / ") {
            if key.starts_with("CRATONVM_") {
                listed.insert((token.to_string(), key.to_string()));
            }
        }
    }

    let expected: BTreeSet<(String, String)> = INVENTORY
        .iter()
        .flat_map(|e| {
            [e.on_key, e.off_key]
                .into_iter()
                .flatten()
                .map(|k| (e.token.to_string(), k.to_string()))
        })
        .collect();

    assert!(
        listed.len() > 400,
        "only {} token rows parsed out of docs/flag-tokens.md — the row format \
         changed and this test was about to check nothing",
        listed.len()
    );

    let missing: Vec<_> = expected
        .difference(&listed)
        .map(|(t, k)| format!("{t} -> {k}"))
        .collect();
    let extra: Vec<_> = listed
        .difference(&expected)
        .map(|(t, k)| format!("{t} -> {k}"))
        .collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "docs/flag-tokens.md disagrees with INVENTORY. Regenerate it with \
         `tools/flag-census/render-tokens.sh`.\n  missing from the doc: {:?}\n  \
         in the doc but not declared: {:?}",
        missing,
        extra
    );
}
