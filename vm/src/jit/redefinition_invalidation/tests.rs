/// Every production call site that resets tiering state for a redefined
/// class must also invalidate that class's interpreter profile.
///
/// The two files that hold them. That they are the ONLY two is not
/// assumed here — it is asserted by
/// [`no_other_file_under_vm_src_resets_tiering_for_a_redefined_class`],
/// which walks the tree.
const REDEFINE_SITES: &[(&str, &str)] = &[
    (
        "native/jni/jni_empty_array_elements.rs",
        include_str!("../../native/jni/jni_empty_array_elements.rs"),
    ),
    (
        "vm/vm_exec/load_class.rs",
        include_str!("../../vm/vm_exec/load_class.rs"),
    ),
    (
        "vm/vm_exec/clear_breakpoints_of_redefined_class.rs",
        include_str!("../../vm/vm_exec/clear_breakpoints_of_redefined_class.rs"),
    ),
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
    // Only exclude files the pairing test reads, plus this test module.
    let covered: Vec<_> = REDEFINE_SITES.iter().map(|(path, _)| *path).collect();
    let mut stack = vec![root.clone()];
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
            let rel = path.strip_prefix(&root).expect("file is under vm/src");
            let rel = rel.to_string_lossy().replace('\\', "/");
            if covered.contains(&rel.as_str()) || rel == "jit/redefinition_invalidation/tests.rs" {
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
    let gc = include_str!("../../memory/gc.rs");
    let stmts = statements(gc);
    let profile = stmts
        .iter()
        .position(|(_, l)| l.contains("profile_store.invalidate_classes("))
        .expect("vm/src/memory/gc.rs unloads classes and must purge their profiles");
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
