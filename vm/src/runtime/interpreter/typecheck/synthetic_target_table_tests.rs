use super::synthetic_implements_by_name;
use cratonvm_types::synthetic_targets::{
    synthetic_implements_may_admit, SYNTHETIC_IMPLEMENTS_TARGETS,
};

/// Every string literal of `synthetic_implements_by_name`'s source (comment
/// lines skipped): each name a rule compares the object or the target with.
fn rule_literals() -> Vec<String> {
    let src = include_str!("../typecheck.rs");
    let start = src
        .find("pub(super) fn synthetic_implements_by_name(")
        .expect("the rules function is in this file");
    // The function ends at the first line that is exactly a closing brace.
    let close: String = ['\n', char::from(0x7D_u8), '\n'].iter().collect();
    let len = src[start..].find(&close).expect("the rules function ends");
    let body = &src[start..start + len];
    let quote = char::from(0x22_u8);
    let mut out = Vec::new();
    for line in body.lines() {
        let code = line.trim_start();
        if code.starts_with("//") {
            continue;
        }
        let parts: Vec<&str> = code.split(quote).collect();
        // Odd-indexed pieces are inside quotes (no escaped quotes occur).
        for lit in parts.iter().skip(1).step_by(2) {
            out.push((*lit).to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

fn object_corpus(literals: &[String]) -> Vec<String> {
    let mut objs: Vec<String> = literals.to_vec();
    for lit in literals {
        if !lit.contains('/') {
            // Word and substring rules (`$Itr`, `List`, `Deque`, ...) in
            // the shapes they are written against.
            for shape in [
                format!("java/util/{lit}"),
                format!("java/util/A{lit}"),
                format!("java/util/A{lit}Impl"),
                format!("java/util/Outer{lit}"),
                format!("java/util/Outer${lit}"),
                format!("java/util/concurrent/Outer${lit}"),
                format!("com/example/A{lit}"),
                format!("java/util/Collections${lit}"),
            ] {
                objs.push(shape);
            }
        }
    }
    for extra in [
        "[I",
        "[Ljava/lang/String;",
        "java/util/ArrayList",
        "java/util/HashMap$KeyIterator",
        "java/util/LinkedHashMap$LinkedKeySet",
        "java/util/ImmutableCollections$ListN",
        "com/example/Foo",
        "R19Str6Rows$Foo",
        "",
    ] {
        objs.push(extra.to_string());
    }
    objs.sort();
    objs.dedup();
    objs
}

#[test]
fn the_synthetic_target_table_is_exactly_what_the_rules_admit() {
    let literals = rule_literals();
    // Positive control on the scan itself: it saw the rules.
    for known in ["java/util/Map$Entry", "$Itr", "Deque", "java/lang/Object"] {
        assert!(
            literals.iter().any(|l| l == known),
            "the literal scan missed {known}"
        );
    }
    let objs = object_corpus(&literals);
    let mut targets: Vec<String> = literals.clone();
    targets.extend(SYNTHETIC_IMPLEMENTS_TARGETS.iter().map(|t| t.to_string()));
    for extra in [
        "R19Str6Rows$Foo",
        "java/lang/Runnable",
        "java/util/Map",
        "java/util/RandomAccess",
        "java/lang/CharSequence",
        "",
    ] {
        targets.push(extra.to_string());
    }
    targets.sort();
    targets.dedup();
    let mut admitted_by_entry = vec![0usize; SYNTHETIC_IMPLEMENTS_TARGETS.len()];
    for t in &targets {
        for o in &objs {
            if synthetic_implements_by_name(o, t) {
                assert!(
                    synthetic_implements_may_admit(t),
                    "{o} admitted for target {t}, which the shared table does not name"
                );
                let i = SYNTHETIC_IMPLEMENTS_TARGETS
                    .iter()
                    .position(|x| x == t)
                    .expect("in the table");
                admitted_by_entry[i] += 1;
            }
        }
    }
    // The other direction: no table entry is dead weight.
    for (i, n) in admitted_by_entry.iter().enumerate() {
        assert!(
            *n > 0,
            "table entry {} is admitted by no object of the corpus",
            SYNTHETIC_IMPLEMENTS_TARGETS[i]
        );
    }
}
