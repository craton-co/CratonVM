use super::*;

fn native(class: &str, method: &str) -> BacktraceFrame {
    BacktraceFrame::Entry(StackTraceEntry {
        class_name: Arc::from(class),
        method_name: Arc::from(method),
        method_descriptor: None,
        source_file: None,
        line_number: LINE_NUMBER_NATIVE,
        byte_code_index: -1,
        class_id: None,
        method_index: None,
    })
}

fn compiled() -> BacktraceFrame {
    BacktraceFrame::Compiled {
        label: Arc::from("app/Main.k(I)Ljava/lang/String;"),
        owner_class_id: 0,
        bci: 7,
        cp_stamp: None,
    }
}

#[test]
fn the_rows_are_the_measured_ones() {
    const SYS: &str = "java/lang/System";
    const ARR: &str = "java/lang/reflect/Array";
    const AIOOBE: &str = "java/lang/ArrayIndexOutOfBoundsException";
    const ASE: &str = "java/lang/ArrayStoreException";
    const NPE: &str = "java/lang/NullPointerException";
    const NASE: &str = "java/lang/NegativeArraySizeException";
    const IAE: &str = "java/lang/IllegalArgumentException";
    let drops = c2_intrinsic_drops_native_leaf;
    assert!(drops(SYS, "arraycopy", AIOOBE));
    assert!(drops(SYS, "arraycopy", ASE));
    // The NPE goes through an uncommon trap: HotSpot keeps the frame.
    assert!(!drops(SYS, "arraycopy", NPE));
    assert!(drops(ARR, "newArray", NASE));
    assert!(!drops(ARR, "newArray", IAE));
    assert!(!drops("java/lang/Thread", "sleep0", AIOOBE));
}

#[test]
fn only_a_compiled_innermost_frame_drops_the_leaf() {
    const AIOOBE: &str = "java/lang/ArrayIndexOutOfBoundsException";
    let drop = drop_c2_intrinsic_native_leaf;
    // `=0` in this test's environment keeps it; nothing else differs.
    let on = true;
    let ir = |_: InnermostActivation<'_>| true;
    let single_pass = |_: InnermostActivation<'_>| false;
    let caller = native("app/Main", "main");
    // An activation that stands for C2 code drops the leaf.
    let mut extra = vec![native("java/lang/System", "arraycopy")];
    let trace = [caller.clone(), compiled()];
    assert_eq!(drop(&trace, AIOOBE, &mut extra, &mut { ir }), on);
    // One that does not keeps it, as HotSpot's C1 does.
    let mut kept = vec![native("java/lang/System", "arraycopy")];
    assert!(!drop(&trace, AIOOBE, &mut kept, &mut { single_pass }));
    assert_eq!(kept.len(), 1);
    // An innermost frame that is neither compiled nor an interpreter
    // frame (or none at all) keeps it, unasked.
    let mut asked = false;
    let mut ask = |_: InnermostActivation<'_>| {
        asked = true;
        true
    };
    assert!(!drop(&[], AIOOBE, &mut kept, &mut ask));
    let interpreted_top = [compiled(), caller.clone()];
    assert!(!drop(&interpreted_top, AIOOBE, &mut kept, &mut ask));
    assert!(!asked);
    assert_eq!(kept.len(), 1);
    // `newInstance` stays under a dropped `newArray`.
    let mut chain = vec![
        native("java/lang/reflect/Array", "newInstance"),
        native("java/lang/reflect/Array", "newArray"),
    ];
    let dropped = drop(
        &trace,
        "java/lang/NegativeArraySizeException",
        &mut chain,
        &mut { ir },
    );
    assert_eq!(chain.len(), if dropped { 1 } else { 2 });
    assert_eq!(&*chain[0].to_entry().method_name, "newInstance");
}

/// The activation is named through the inlined levels standing on it.
#[test]
fn the_activation_is_the_compiled_frame_under_its_inlined_levels() {
    const K: &str = "app/Main.k(I)Ljava/lang/String;";
    let level = BacktraceFrame::Inlined {
        label: Arc::from("java/lang/reflect/Array.newInstance"),
        class_id: 0,
        bci: 2,
        cp_stamp: None,
        hidden: None,
    };
    let caller = native("app/Main", "main");
    let label =
        |t: &[BacktraceFrame]| innermost_compiled_activation_label(t).map(str::to_owned);
    assert_eq!(
        label(&[caller.clone(), compiled(), level.clone()]).as_deref(),
        Some(K)
    );
    assert_eq!(label(&[caller.clone(), compiled()]).as_deref(), Some(K));
    // Inlined levels over an interpreter frame name no activation.
    assert_eq!(label(&[caller.clone(), level]), None);
    assert_eq!(label(&[compiled(), caller]), None);
    assert_eq!(label(&[]), None);
}
