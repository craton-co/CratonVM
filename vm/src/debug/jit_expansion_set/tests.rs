use super::*;

/// The whole-package prefixes, which no expansion may key on.
const BARE: &[&str] = &["java/", "javax/", "jdk/", "sun/", "com/sun/"];

/// The whole-package literals in the vm-side JIT sources, reviewed in
/// wave 117: `jit/helpers.rs` (a field type's JDK test) and
/// `jit_bridge.rs` (the method-handle site link's loads, the owner
/// shadow and the faithful field site). None in `jit/src`.
const BARE_IN_VM_SOURCES: usize = 12;

fn sources() -> Vec<std::path::PathBuf> {
    let vm = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = vec![vm.join("src/vm/realms/jit_realm.rs")];
    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs")
                && path.file_name().is_some_and(|n| {
                    let s = n.to_string_lossy();
                    s != "tests.rs" && !s.ends_with("_tests.rs")
                })
            {
                out.push(path);
            }
        }
    }
    walk(&vm.join("src/runtime/interpreter/jit_bridge"), &mut out);
    walk(&vm.join("../jit/src"), &mut out);
    walk(&vm.join("src/jit"), &mut out);
    out
}

/// Every `"java/...` (and `javax/`, `jdk/`, `sun/`, `com/sun/`) string
/// literal of the file `text`, up to its closing quote or a `.` (a
/// `"java/lang/Object.hashCode"` method key names its class).
fn literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(at) = text[i..].find('"') {
        let start = i + at + 1;
        let mut end = start;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'_' | b'/' | b'$'))
        {
            end += 1;
        }
        let lit = &text[start..end];
        let closed = end < bytes.len() && matches!(bytes[end], b'"' | b'.');
        if closed && BARE.iter().any(|b| lit.starts_with(b)) {
            out.push(lit.to_string());
            // Past the closing character, as a regex scan would go on.
            i = end + 1;
            continue;
        }
        i = if end > start { end } else { start };
    }
    out
}

#[test]
fn every_jit_class_literal_is_covered() {
    let files = sources();
    assert!(
        files.len() > 10,
        "the JIT sources were not found: {files:?}"
    );
    let mut bare_in_vm = 0usize;
    let mut uncovered = std::collections::BTreeSet::new();
    for path in &files {
        // `is_jdk_throwable_init_candidate` gates throwable `<init>` bypasses
        // with `ends_with("Exception") || ends_with("Error")`,
        // `typecheck_site_is_boot_package` in `g1_aastore_screen` checks boot
        // package prefixes, and `ir_tier_attempt` excludes `java/` classes;
        // none of them is an expansion.
        if path.file_name().is_some_and(|n| {
            matches!(
                n.to_str(),
                Some(
                    "is_jdk_throwable_init_candidate.rs"
                        | "g1_aastore_screen.rs"
                        | "ir_tier_attempt.rs"
                )
            )
        }) {
            continue;
        }
        let text = std::fs::read_to_string(path).expect("a source file");
        let in_jit_crate = path.components().any(|c| c.as_os_str() == "jit")
            && !path
                .to_string_lossy()
                .replace('\\', "/")
                .contains("vm/src/jit");
        for lit in literals(&text) {
            if BARE.contains(&lit.as_str()) {
                assert!(
                    !in_jit_crate,
                    "{}: a whole-package literal `{lit}` in the JIT crate (an expansion \
                     keyed on a whole package would make every JDK class expandable)",
                    path.display()
                );
                bare_in_vm += 1;
                continue;
            }
            let covered = if lit.ends_with('/') {
                PREFIXES.contains(&lit.as_str())
            } else {
                may_expand(&lit)
            };
            if !covered {
                uncovered.insert(lit);
            }
        }
    }
    assert!(
        uncovered.is_empty(),
        "JDK classes the JIT names but the expansion set does not hold (add them and \
         their supertypes; see the module doc): {uncovered:?}"
    );
    assert_eq!(
        bare_in_vm, BARE_IN_VM_SOURCES,
        "the whole-package literals of the vm-side JIT sources changed: review the new one"
    );
}

#[test]
fn the_set_is_sorted_and_answers_by_class_and_prefix() {
    assert!(
        CLASSES.windows(2).all(|w| w[0] < w[1]),
        "sorted, no duplicate"
    );
    assert!(may_expand("java/lang/String"));
    assert!(
        may_expand("java/lang/AbstractStringBuilder"),
        "a supertype of a literal"
    );
    assert!(
        may_expand("java/lang/invoke/LambdaForm$MH/0x0000000800c01000"),
        "a prefix"
    );
    assert!(!may_expand("java/util/TreeMap"));
    assert!(!may_expand("java/text/SimpleDateFormat"));
}
