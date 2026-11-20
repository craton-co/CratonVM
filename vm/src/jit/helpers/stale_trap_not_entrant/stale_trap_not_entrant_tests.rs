use super::should_patch;

#[test]
fn only_an_entrant_stale_named_body_is_patched_and_only_when_enabled() {
    // The one case: enabled, a named body that is not the live one, still entrant.
    assert!(should_patch(true, false, false, Some((false, false))));
    // Off: nothing.
    assert!(!should_patch(false, false, false, Some((false, false))));
    // The live body is never patched here (the eviction path owns it).
    assert!(!should_patch(true, false, false, Some((true, false))));
    // Already not entrant: nothing to do.
    assert!(!should_patch(true, false, false, Some((false, true))));
    // No named body: the address test alone does not name what to patch.
    assert!(!should_patch(true, false, false, None));
    // Pre-redefinition source: the redefinition patches it.
    assert!(!should_patch(true, true, false, Some((false, false))));
    // A sibling key's live body: evicted under its own key.
    assert!(!should_patch(true, false, true, Some((false, false))));
}

/// Source pin (working-tree read): the stash's trapping body is taken only
/// through [`super::taken_trapping_body_retained`], so no caller can drop
/// the `Weak` upgrade bare. Fails on the pre-j27-w7 tree (six bare takes in
/// `thread.rs`, `deopt_resume.rs`, `jit_bridge.rs`).
#[test]
fn taken_trapping_body_is_only_taken_retained() {
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read vm/src").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let needle = concat!("deopt::taken_trapping_body", "(");
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    walk(&src, &mut files);
    let mut offenders = Vec::new();
    let mut own = 0usize;
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let n = text.matches(needle).count();
        if path.ends_with("jit/helpers/stale_trap_not_entrant.rs") {
            own = n;
        } else if n > 0 {
            offenders.push(format!("{} ({n})", path.display()));
        }
    }
    assert!(offenders.is_empty(), "bare takes outside the helper: {offenders:?}");
    assert_eq!(own, 1, "the helper itself takes the body once");    }