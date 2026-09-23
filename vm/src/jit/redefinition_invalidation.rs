// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The class-redefinition invalidation PAIRING, and the ratchet that keeps it.
//!
//! `SharedVm.jit` holds two independent stores that both describe a class by
//! its `ClassId`, and a redefinition invalidates BOTH or neither:
//!
//!  * `tiered_manager` — the tiering verdicts (`ineligible`, `c2_bailout`,
//!    trap counts, OSR denials, the de-speculation registry). Cleared by
//!    [`cratonvm_jit::tiered::TieredCompilationManager::on_class_redefined`].
//!  * `profile_store` — the interpreter profile (branch counts, back-edge
//!    counts, per-pc receiver-type tables, call-site counts). Cleared by
//!    `ProfileStore::invalidate_class`.
//!
//! They are siblings, not a hierarchy: `TieredCompilationManager` holds no
//! handle to `ProfileStore` and cannot reach it, so the pairing can only be
//! made by the CALLER. The class-UNLOAD path in `vm/src/memory/gc.rs` has
//! always made it. The class-REDEFINE path did not, and that gap is
//! `docs/internal/retired/r10-tier2-profile-store-survives-class-redefinition-20260921-RETIRED-20260922.md`.
//!
//! # Why a redefinition must purge the profile
//!
//! `ProfileStore` is keyed by `MethodKey { class_id, method_name, descriptor }`
//! and `class_id` is **stable across a redefinition** — that is the whole
//! reason `on_class_redefined` takes it as the identity to reset tiering state
//! *for*. So the same key under which `on_class_redefined` says "the old
//! bytecode's verdicts are worthless" is the key under which the old
//! bytecode's branch counts and per-pc receiver tables keep living, in the
//! same `MethodProfile`, blended with every post-redefinition recording. A
//! conditional-branch bytecode index that meant one thing before the redefine
//! and something structurally different after it — neither bytecode length nor
//! branch targets are required to be stable across a redefine — is recorded
//! and read back as if nothing had happened.
//!
//! It is a QUALITY defect and not a wrong-answer one: every consumer of a
//! profile read pairs it with a runtime guard (the receiver-type consumer
//! re-checks the receiver's actual class id at the guarded call site; the
//! branch-count consumer only influences fallthrough layout). What it costs is
//! the contract `on_class_redefined`'s own doc states — that a redefined
//! method "re-admits at its next stride instead of re-warming from zero" — by
//! letting its profile re-warm from a lie instead of from zero, in a way
//! indistinguishable from the ordinary staleness `jit/src/profile.rs`'s module
//! doc already accepts. That is precisely the risk: nothing points at it.
//!
//! # Why the guard below is textual
//!
//! The fix is one call at each of five production sites. Nothing in either
//! crate's type system makes them travel together — `on_class_redefined` is an
//! ordinary method on an unrelated struct — so the only thing that can notice
//! a sixth call site added without its partner, or an existing pair split by a
//! refactor, is a scan of the source that holds them. [`tests`] does that, and
//! says what it cannot do: it proves the call is WRITTEN next to its partner,
//! never that it RAN.
//!
//! The behavioural halves live where the state does:
//! `jit/tests/r10_tier2_profile_redefinition_pairing.rs` (the two stores are
//! independent, and `ProfileStore::invalidate_class` really does clear a
//! receiver table) and `invalidate_class_sweeps_all_shards` in
//! `jit/src/profile.rs`.

#[cfg(test)]
mod tests {
    /// Every production call site that resets tiering state for a redefined
    /// class must also invalidate that class's interpreter profile.
    ///
    /// The two files that hold them. That they are the ONLY two is not
    /// assumed here — it is asserted by
    /// [`no_other_file_under_vm_src_resets_tiering_for_a_redefined_class`],
    /// which walks the tree.
    const REDEFINE_SITES: &[(&str, &str)] = &[
        ("vm/src/native/jni.rs", include_str!("../native/jni.rs")),
        ("vm/src/vm/vm_exec.rs", include_str!("../vm/vm_exec.rs")),
    ];

    /// How many statement lines either side of the `on_class_redefined` call
    /// the partner may sit on.
    ///
    /// Not 1, because `vm_exec.rs:8928` spells its partner as a four-line
    /// builder chain (`self.shared` / `.jit` / `.tiered_manager` /
    /// `.on_class_redefined(..)`) and every one of those is a statement line.
    /// Not 20, because a window wide enough to span an unrelated block would
    /// pass a site whose "partner" belongs to a different class id. Six leaves
    /// room for one more chained call or a `let` without reaching out of the
    /// block.
    const WINDOW: usize = 6;

    /// Lines that are entirely a comment. Both call sites carry a comment
    /// block naming the other call, so matching raw text would pass on a site
    /// that only TALKS about invalidating the profile.
    fn is_comment(line: &str) -> bool {
        let t = line.trim_start();
        t.starts_with("//") || t.starts_with("/*") || t.starts_with('*')
    }

    /// Statement lines only, paired with their 1-based line number in the file.
    fn statements(src: &str) -> Vec<(usize, &str)> {
        src.lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l))
            .filter(|(_, l)| !is_comment(l))
            .collect()
    }

    #[test]
    fn every_redefinition_site_invalidates_the_profile_store_too() {
        let mut pairs = 0usize;
        let mut unpaired: Vec<String> = Vec::new();

        for (path, src) in REDEFINE_SITES {
            let stmts = statements(src);
            for (i, (lineno, line)) in stmts.iter().enumerate() {
                if !line.contains("on_class_redefined(") {
                    continue;
                }
                let lo = i.saturating_sub(WINDOW);
                let hi = (i + WINDOW + 1).min(stmts.len());
                let paired = stmts[lo..hi]
                    .iter()
                    .any(|(_, l)| l.contains("profile_store.invalidate_class("));
                if paired {
                    pairs += 1;
                } else {
                    unpaired.push(format!("{path}:{lineno}"));
                }
            }
        }

        assert!(
            unpaired.is_empty(),
            "a class redefinition resets tiering verdicts but leaves the \
             interpreter profile from the OLD bytecode under the same \
             (class_id, method, descriptor) key — `class_id` is stable across \
             a redefine. Add `profile_store.invalidate_class(<id>.as_u32())` \
             beside `on_class_redefined` at: {unpaired:?}. See this module's \
             doc and the UNLOAD path in vm/src/memory/gc.rs, which pairs them."
        );
        assert_eq!(
            pairs, 5,
            "the five production redefinition sites (one in vm/src/native/jni.rs, \
             four in vm/src/vm/vm_exec.rs) are the population this guard was \
             written against. A different count is not a failure of the pairing \
             but of this test's premise: re-read the sites and update the count \
             deliberately rather than relaxing the assertion."
        );
    }

    /// No OTHER file under `vm/src` resets tiering state for a redefined
    /// class.
    ///
    /// The test above reads two files by name, which is what makes it work
    /// anywhere the crate compiles — and also its blind spot: a sixth call
    /// site added in a third file would never be looked at. This walks the
    /// tree to close that, and is the only test here that touches the
    /// filesystem.
    ///
    /// SKIPPED, not failed, when `vm/src` is not on disk (a vendored or
    /// packaged build). The assertion above is the one that must hold
    /// everywhere; this one is the wider net when the sources are present.
    #[test]
    fn no_other_file_under_vm_src_resets_tiering_for_a_redefined_class() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        if !root.is_dir() {
            return;
        }
        // The two files the test above already covers, by the name it reports.
        let covered = ["jni.rs", "vm_exec.rs"];
        let mut stack = vec![root];
        let mut strays: Vec<String> = Vec::new();
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                // This module names the call in its own doc and in the string
                // literal the test above matches on; both are why it is here.
                if covered.contains(&name) || name == "redefinition_invalidation.rs" {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for (lineno, line) in statements(&src) {
                    if line.contains("on_class_redefined(") {
                        strays.push(format!("{}:{lineno}", path.display()));
                    }
                }
            }
        }
        assert!(
            strays.is_empty(),
            "a redefinition site outside the two files this module's pairing \
             test reads: {strays:?}. Either pair it with \
             `profile_store.invalidate_class(..)` and add its file to \
             `REDEFINE_SITES`, or it is unpaired and nothing is checking it."
        );
    }

    /// The contrast the pairing is modelled on: the class-UNLOAD path clears
    /// both stores under one class identity. If this ever stops being true the
    /// redefine sites lost their reference implementation.
    #[test]
    fn the_unload_path_still_pairs_both_invalidations() {
        let gc = include_str!("../memory/gc.rs");
        let stmts = statements(gc);
        let profile = stmts
            .iter()
            .position(|(_, l)| l.contains("profile_store.invalidate_class("))
            .expect("vm/src/memory/gc.rs unloads a class and must purge its profile");
        let tiered = stmts
            .iter()
            .position(|(_, l)| l.contains("tiered_manager"))
            .expect("vm/src/memory/gc.rs unloads a class and must purge its tiering state");
        assert!(
            tiered.abs_diff(profile) <= 8,
            "the unload path's two invalidations drifted apart; they are the \
             reference the five redefinition sites were written to mirror"
        );
    }
}
