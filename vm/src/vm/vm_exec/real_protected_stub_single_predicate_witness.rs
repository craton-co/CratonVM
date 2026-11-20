/// Read one repo-relative `vm/...` path out of the WORKING TREE.
///
/// Same shape as `native_override.rs`'s own `vm_src`, deliberately: a
/// source witness reads the tree it was compiled from, and duplicating six
/// lines is cheaper than exporting a test helper across two modules.
fn vm_src(rel: &str) -> String {
    let under_crate = rel
        .strip_prefix("vm/")
        .expect("scan paths are vm-crate paths");
    let path = format!("{}/{under_crate}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
}

/// The cold dispatch implementation, separate from this test's literals.
fn vm_exec_body() -> String {
    vm_src("vm/src/vm/vm_exec/invoke_on_class_shared_inner.rs")
}

/// Ten of the twelve class-name literals `real_protected_stub_class_common`
/// holds, quoted exactly as a `matches!` arm would spell them.
///
/// **Two are deliberately absent**, and the reason is a finding rather than
/// an exclusion for convenience: `"java/lang/management/ManagementFactory"`
/// and `"java/util/concurrent/ThreadPoolExecutor"` legitimately appear in
/// `invoke_on_class_shared_inner` — in a SEPARATE, method-scoped table with
/// the OPPOSITE polarity (force the native to win OVER real bytecode),
/// alongside `org/apache/juli/*` and `org/jboss/logmanager/Logger` entries.
/// That table is a third hard-coded override surface the P0 row does not
/// mention; including its two shared names here would make this test fail
/// on a tree that is correct.
const ALLOWLIST_LITERALS: &[&str] = &[
    "\"java/util/concurrent/locks/ReentrantLock\"",
    "\"java/util/concurrent/LinkedBlockingDeque\"",
    "\"java/util/concurrent/atomic/AtomicBoolean\"",
    "\"java/util/EnumSet\"",
    "\"java/time/Instant\"",
    "\"java/time/ZonedDateTime\"",
    "\"java/io/FileInputStream\"",
    "\"java/lang/ref/Cleaner\"",
    "\"java/lang/ref/Cleaner$Cleanable\"",
    "\"java/util/StringJoiner\"",
];

/// The cold path holds no second copy of the allow-list.
///
/// Searching for the QUOTED literal, not the bare class name: the bare name
/// appears in the call site's own comment, which recounts the divergence
/// this test exists to prevent. A comment naming `StringJoiner` is the
/// history; a `matches!` arm spelling the quoted literal is the defect.
#[test]
fn the_cold_path_re_inlines_no_copy_of_the_allowlist() {
    let body = vm_exec_body();
    for literal in ALLOWLIST_LITERALS {
        assert!(
            !body.contains(literal),
            "vm_exec.rs spells the allow-list literal {literal}. That is the \
             re-inlined second copy the 2026-08-04 centralisation removed: two \
             hand-maintained lists made a SyntheticStub native's \
             yield-to-real-bytecode verdict depend on how many times its call site \
             had executed. Call \
             `crate::runtime::interpreter::real_protected_stub_class` instead, and \
             if the class genuinely needs a per-path answer, add it to \
             `real_protected_stub_class_common` with a stated reason rather than \
             forking the list."
        );
    }
}

/// The cold path now resolves every registration against loaded bytecode.
/// The former SyntheticStub-only allow-list must not be restored here.
#[test]
fn the_cold_path_replaces_stub_protection_with_resolved_bytecode() {
    let body = vm_exec_body();
    assert_eq!(body.matches("real_protected_stub_class(").count(), 0);
    assert!(body.contains("resolve_dispatch_in("));
}

/// Exactly one definition of the predicate exists, and it is not in the file
/// the P0 row names.
///
/// `docs/feature-designs/real-jdk-mode.md` attributes `real_protected_stub_class`
/// to `vm/src/runtime/interpreter/invoke.rs`. It is in `native_override.rs`.
/// A row that names the wrong file sends its reader looking for a list that
/// is not there, and "I could not find it" reads as "it was already removed".
#[test]
fn exactly_one_definition_of_the_predicate_exists() {
    let overrides = vm_src("vm/src/runtime/interpreter/native_override.rs");
    assert_eq!(
        overrides
            .matches("fn real_protected_stub_class_common")
            .count(),
        1,
        "native_override.rs must hold exactly one definition of \
         real_protected_stub_class_common — it is the single list both dispatch \
         paths read"
    );
    let invoke = vm_src("vm/src/runtime/interpreter/invoke.rs");
    assert!(
        !invoke.contains("fn real_protected_stub_class"),
        "invoke.rs now defines real_protected_stub_class. The P0 *Native-first \
         dispatch* row has always claimed it lives here and it never did; if the \
         symbol has genuinely moved, update that row rather than leaving two \
         possible homes for one predicate."
    );
}
