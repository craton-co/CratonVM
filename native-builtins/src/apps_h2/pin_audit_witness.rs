/// Read the working tree, not a runtime behaviour: the claim is about what
/// this file says.
const SRC: &str = include_str!("../apps_h2.rs");

/// `ctx` operations that can run Java or allocate, and therefore can move
/// every object this file holds in a Rust local.
const MOVERS: &[&str] = &[
    "ctx.invoke",
    "ctx.new_object",
    "ctx.new_ref_array",
    "ctx.new_array",
    "ctx.create_string",
    "ctx.ensure_class_initialized",
    // Resolves through a user loader's `loadClass` when the name is not
    // already in that loader's table.
    "ctx.class_id_by_name_via_referencing_class",
];

/// Functions that reach a mover and deliberately pin nothing, each with the
/// reason. "receiver-only" means every reference the function holds across
/// the mover is used solely as the RECEIVER of a later `ctx` call, which
/// `load_and_forward` repairs at the entry point.
const JUSTIFIED_UNPINNED: &[(&str, &str)] = &[
    (
        "register_apps_h2_overrides",
        "registration only; holds no ObjectRef",
    ),
    (
        "h2_internal_error",
        "the string it creates is the very next call's argument",
    ),
    (
        "h2_new_empty_hashmap",
        "the allocation IS the value; nothing is held across it",
    ),
    (
        "h2_class_seen_from",
        "holds no ObjectRef; resolves a ClassId from a ClassId",
    ),
    (
        "h2_static_object",
        "holds no ObjectRef across the resolve or <clinit>; it moved exactly as much before it took an anchor",
    ),
    (
        "h2_expression_column_get_value_bytecode",
        "forwards `args` untouched",
    ),
    ("h2_row_get", "hands its argument straight to the callee"),
    (
        "h2_utils_get_resource",
        "the array it allocates is only ever its own store target",
    ),
    (
        "h2_parser_set_token_index",
        "hands its argument straight to the callee",
    ),
    (
        "h2_parser_advance_to",
        "receiver-only; the one value store is forwarded by set_field_by_name",
    ),
    (
        "h2_token_as_identifier_native",
        "hands its argument straight to the callee",
    ),
    (
        "h2_token_as_identifier_value",
        "the string it creates is returned immediately",
    ),
    (
        "h2_syntax_error",
        "hands its argument straight to the callee",
    ),
    (
        "table_filter_prepare",
        "hands its argument straight to the callee",
    ),
    (
        "h2_db_exception",
        "the array is re-read through a pin; see the body",
    ),
];

/// Top-level functions only, keyed on column 0.
///
/// Every native in this file is declared at column 0; everything inside
/// `mod tests` is indented. Using the indentation is what keeps a `#[test]`
/// fn out of the audit without needing to brace-match a module whose format
/// strings contain braces of their own.
fn function_bodies_of(src: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in src.lines() {
        let at_top_level = !line.starts_with(' ') && !line.starts_with('\t');
        let after_fn = if at_top_level {
            line.strip_prefix("pub fn ")
                .or_else(|| line.strip_prefix("pub(super) fn "))
                .or_else(|| line.strip_prefix("pub fn "))
                .or_else(|| line.strip_prefix("fn "))
        } else {
            None
        };
        if let Some(rest) = after_fn {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                if let Some(done) = current.take() {
                    out.push(done);
                }
                current = Some((name, String::new()));
            }
        }
        if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some(done) = current.take() {
        out.push(done);
    }
    out
}

fn function_bodies() -> Vec<(String, String)> {
    function_bodies_of(SRC)
}

/// The predicate itself, over arbitrary source, so the negative control
/// below exercises the same code the audit runs on the real file.
fn unpinned_movers_in(src: &str) -> Vec<String> {
    function_bodies_of(src)
        .into_iter()
        .filter(|(name, body)| {
            MOVERS.iter().any(|m| body.contains(m))
                && !body.contains("pin_native_root")
                && !JUSTIFIED_UNPINNED.iter().any(|(n, _)| n == name)
        })
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn the_corpus_is_the_file_this_test_thinks_it_is() {
    // An absence proves nothing about a file that stopped containing the
    // thing being scanned for. Anchor on two names this audit is about.
    assert!(
        SRC.contains("fn table_filter_prepare_on"),
        "apps_h2.rs no longer defines `table_filter_prepare_on`, so this scan is reading \
         the wrong text and its verdict means nothing"
    );
    assert!(
        SRC.contains("pin_native_root"),
        "apps_h2.rs contains no pins at all — the whole audit has been reverted"
    );
    let bodies = function_bodies();
    assert!(
        bodies.len() > 60,
        "only {} top-level functions parsed out of apps_h2.rs; the parser is broken, not the file",
        bodies.len()
    );
}

/// A guard that cannot fail reads exactly like one that works.
///
/// Feed the same predicate a function that breaks the rule and one that
/// keeps it, and check it separates them — otherwise the green above could
/// mean "the scan found nothing" or "the scan looks at nothing", and those
/// are not the same claim.
#[test]
fn the_scan_can_fail() {
    let offending = "fn h2_synthetic_offender(ctx: &mut dyn NativeContext, v: ObjectRef) {\n\
                         let other = ctx.invoke_virtual(v, \"x\", \"()V\", &[]);\n\
                         let _ = (v, other);\n\
                     }\n";
    assert_eq!(
        unpinned_movers_in(offending),
        vec!["h2_synthetic_offender".to_string()],
        "the predicate does not flag a function that calls back into Java holding a raw \
         ObjectRef, so its silence on apps_h2.rs means nothing"
    );

    let compliant = "fn h2_synthetic_compliant(ctx: &mut dyn NativeContext, v: ObjectRef) {\n\
                         let pin = ctx.pin_native_root(v);\n\
                         let _ = ctx.invoke_virtual(v, \"x\", \"()V\", &[]);\n\
                         let _ = ctx.read_native_pin(pin, v);\n\
                     }\n";
    assert!(
        unpinned_movers_in(compliant).is_empty(),
        "the predicate flags a function that DOES pin, so it would force noise into \
         JUSTIFIED_UNPINNED instead of finding defects"
    );
}

#[test]
fn every_h2_native_that_moves_the_heap_pins_or_is_named_as_safe() {
    let offenders = unpinned_movers_in(SRC);
    assert!(
        offenders.is_empty(),
        "these apps_h2 natives call back into Java (or allocate) while holding raw \
         ObjectRefs and pin nothing: {offenders:?}. Either pin and re-read what is live \
         across the call (see `h2_condition_and_or_get_value` for the idiom), or add the \
         function to JUSTIFIED_UNPINNED with the reason it cannot go stale."
    );
}

#[test]
fn the_justification_list_has_no_dead_entries() {
    let names: Vec<String> = function_bodies().into_iter().map(|(n, _)| n).collect();
    for (name, _) in JUSTIFIED_UNPINNED {
        assert!(
            names.iter().any(|n| n == name),
            "JUSTIFIED_UNPINNED names `{name}`, which apps_h2.rs no longer defines. A stale \
             exception list is how an audit stops auditing."
        );
    }
}
