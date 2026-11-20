use super::*;

/// A model marker: pop gray, mark, follow the rows of `tables` (the side
/// tables its batch captured) from each newly marked object.
fn drain(
    // addr-keyed: local to this test (a synthetic heap).
    marked: &std::cell::RefCell<rustc_hash::FxHashSet<usize>>,
    gray: &mut Vec<usize>,
    // addr-keyed: local to this test (a synthetic heap).
    tables: &rustc_hash::FxHashMap<usize, Vec<usize>>,
) {
    while let Some(obj) = gray.pop() {
        if marked.borrow_mut().insert(obj) {
            if let Some(values) = tables.get(&obj) {
                gray.extend(values.iter().copied());
            }
        }
    }
}

/// The window, interleaved: a batch captured the side tables before the
/// scan filed the rows and blackened the owner; the scan then files them
/// and the remark's drain (current tables, nothing gray) never rescans the
/// black owner, so its value stays unmarked. The remark's closure marks
/// it, and a value that owns a row itself (a loader object behind a
/// static) brings that row along; a dead owner's value stays unmarked.
#[test]
fn gci_i16mr_a_row_filed_after_its_owner_was_scanned_is_closed_by_the_remark() {
    const OWNER: usize = 0x16_6000;
    const VALUE: usize = 0x16_6100;
    const DEEP: usize = 0x16_6200;
    const DEAD_OWNER: usize = 0x16_6300;
    const DEAD_VALUE: usize = 0x16_6400;
    let marked = std::cell::RefCell::new(rustc_hash::FxHashSet::<usize>::default());
    // addr-keyed: local to this test (a synthetic heap).
    let captured: rustc_hash::FxHashMap<usize, Vec<usize>> = rustc_hash::FxHashMap::default();
    drain(&marked, &mut vec![OWNER], &captured);
    // addr-keyed: local to this test (a synthetic heap).
    let mut rows: rustc_hash::FxHashMap<usize, Vec<usize>> = rustc_hash::FxHashMap::default();
    rows.insert(OWNER, vec![VALUE]);
    rows.insert(VALUE, vec![DEEP]);
    rows.insert(DEAD_OWNER, vec![DEAD_VALUE]);
    drain(&marked, &mut Vec::new(), &rows);
    assert!(marked.borrow().contains(&OWNER));
    assert!(!marked.borrow().contains(&VALUE), "the window: the black owner is not rescanned");

    let is_live = |a: usize| marked.borrow().contains(&a);
    let handed = mark_live_row_values_to_fixpoint(&rows, &is_live, &mut |values: &[usize]| {
        let mut gray = values.to_vec();
        drain(&marked, &mut gray, &rows);
    });
    assert_eq!(handed, 2, "OWNER's value, then VALUE's (already marked by the drain)");
    let m = marked.borrow();
    assert!(m.contains(&VALUE) && m.contains(&DEEP), "the row closed over the live owner");
    assert!(!m.contains(&DEAD_OWNER) && !m.contains(&DEAD_VALUE), "a dead owner adds nothing");
}

fn read(rel: &str) -> String {
    if rel == "src/runtime/interpreter/gc_and_alloc.rs" {
        return crate::runtime::interpreter::gc_and_alloc::read_gc_and_alloc_src_from_disk()
            .replace("\r\n", "\n");
    }
    let path = format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{path}: {e}"))
        .replace("\r\n", "\n")
}

fn at(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("not found: {needle}"))
}

fn body<'a>(src: &'a str, start: &str) -> &'a str {
    let rest = &src[at(src, start)..];
    &rest[..at(rest, "\n}\n")]
}

/// Source pin: the remark callback closes the rows FIRST (before the
/// reference rows, class unloading, the keyed verdicts and the JNI weak
/// clear read the bitmap), on every G1 remark (no switch), through the
/// fixed point and the heap's own resurrection drain.
#[test]
fn gci_i16mr_the_g1_remark_closes_the_rows_before_any_verdict() {
    let src = read("src/runtime/interpreter/gc_and_alloc.rs");
    let rm = body(
        &src,
        "\npub(in crate::runtime::interpreter) fn g1_final_remark_cleanup(",
    );
    let close = at(
        rm,
        "let _ = g1_remark_close_metadata_rows(shared, is_live);",
    );
    let refs = at(
        rm,
        "let keep = g1_remark_process_references(shared, is_live);",
    );
    let judge = at(rm, ".judge_mirror_keyed_parked_after_mark(is_live, ");
    let jni = at(
        rm,
        "clear_weak_global_refs_unmarked(shared, is_live, &keep)",
    );
    assert!(
        close < refs && refs < judge && judge < jni,
        "{close} {refs} {judge} {jni}"
    );
    let f = body(
        &src,
        "\npub(in crate::runtime::interpreter::gc_and_alloc) fn g1_remark_close_metadata_rows(",
    );
    for needle in [
        "is_g1()",
        // gci i18/nr (gci i17/rv finding 9): this VM's rows only.
        "metadata_pin::vm_snapshot(shared.vm_identity)",
        "mark_live_row_values_to_fixpoint(",
        "g1_remark_mark_kept_closure(shared, values)",
    ] {
        assert!(f.contains(needle), "{needle}");
    }
    assert!(!f.contains("runtime_flag"), "default path: no switch");
}
