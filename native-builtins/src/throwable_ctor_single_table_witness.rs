    /// The only class this crate may register throwable constructors for
    /// outside the JDK-derived table, and why.
    ///
    /// `java/lang/VirtualMachineError` is the single name in
    /// `register_exception_extras_natives`' list that E34's 62-class table does
    /// not carry. MEASURED (`javap -p`, JDK 25.0.3+9-LTS): it declares `()V`,
    /// `(String)V`, `(String,Throwable)V` and `(Throwable)V`, all public — so
    /// the blanket four are, for this one class, exactly the right set. When a
    /// future regeneration adds it to the table, this list must shrink to
    /// empty; the assertion below fails until it does.
    const ALLOWED_OUTSIDE_THE_TABLE: &[&str] = &["java/lang/VirtualMachineError"];

    /// Text of one top-level `fn` in this file, from its signature to the next
    /// closing brace in column 0. Panics rather than returning an empty body.
    fn top_level_fn_body<'a>(src: &'a str, signature: &str, what: &str) -> &'a str {
        let start = src
            .find(signature)
            .unwrap_or_else(|| panic!("{what}: {signature:?} not found in native-builtins/src/lib.rs — this witness cannot check what it cannot find"));
        let rest = &src[start + 1..];
        let stop = rest
            .find("\n}\n")
            .unwrap_or_else(|| panic!("{what}: no column-0 closing brace after {signature:?}"));
        &rest[..stop]
    }

    #[test]
    fn the_blanket_four_ctor_loops_stay_collapsed_onto_the_table() {
        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let lib = ["formatter_alloc_sink.rs", "lock_key_for.rs"]
            .iter()
            .map(|part| std::fs::read_to_string(src_dir.join("lib_parts").join(part)).expect("registrar source is readable"))
            .collect::<Vec<_>>()
            .join("\n");
        let lang_misc = std::fs::read_to_string(src_dir.join("lang_misc.rs"))
            .expect("native-builtins/src/lang_misc.rs is readable");

        // Assembled, not written as one literal, so this test's own text is
        // never what a scan of these files finds.
        let ctor = format!("\"{}\"", "<init>");

        // --- 1. the one table, read out of the file that owns it -------------
        //
        // The table used to be a marker-fenced two-column list of
        // `("class", &[descriptors])` rows in this file, and this step used to
        // look for those markers. It is now SPLIT: `lang_misc.rs` keeps the
        // class-name column as `THROWABLE_FAMILY_CLASSES`, and the descriptor
        // column moved to `cratonvm_classloading::throwable_ctor_descriptors`
        // (which `THROWABLE_FAMILY_CLASSES`'s own doc comment points at). One
        // table still, stored in two places — and every step below only ever
        // used the class-name column, so this reads that.
        let anchor = "THROWABLE_FAMILY_CLASSES: &[&str] = &[";
        let start = lang_misc.find(anchor).unwrap_or_else(|| {
            panic!("{anchor:?} not found in lang_misc.rs; re-point this witness")
        }) + anchor.len();
        let stop = lang_misc[start..]
            .find(
                "
];",
            )
            .map(|i| start + i)
            .unwrap_or_else(|| {
                panic!("THROWABLE_FAMILY_CLASSES has no terminator in lang_misc.rs")
            });
        let table: Vec<&str> = lang_misc[start..stop]
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix('"')
                    .and_then(|rest| rest.strip_suffix("\","))
                    .filter(|class| class.contains('/'))
            })
            .collect();
        assert!(
            table.len() >= 60,
            "parsed only {} rows out of the throwable ctor table; the extractor is broken and \
             everything below would be comparing against nothing",
            table.len()
        );

        // --- 2. site 3 is gone and stays gone --------------------------------
        let synthetic = top_level_fn_body(
            &lib,
            "\npub fn register_synthetic_overrides(",
            "site 3 (register_synthetic_overrides)",
        );
        assert!(
            synthetic.len() > 50_000,
            "register_synthetic_overrides extracted as only {} bytes — the body scan is broken",
            synthetic.len()
        );
        let site3_binder = format!("for {} in", "exc_class");
        assert!(
            !synthetic.contains(&site3_binder),
            "`register_synthetic_overrides` has a `{site3_binder} ...` loop again. That was E34-1 \
             §7 site 3: 120 registrations over 30 class names, of which 82 restated the \
             JDK-derived table in lang_misc.rs (worse: LATER, so they won the slot) and 38 named \
             constructors JDK 25 does not declare. Add the class to the table instead."
        );

        // --- 3. site 4 registers no constructors from its list ---------------
        let extras = top_level_fn_body(
            &lib,
            "\npub fn register_exception_extras_natives(",
            "site 4 (register_exception_extras_natives)",
        );
        assert!(
            extras.len() > 2_000,
            "register_exception_extras_natives extracted as only {} bytes — body scan is broken",
            extras.len()
        );
        let loop_head = format!("for {} in &exceptions {{", "exc");
        let loop_start = extras.find(&loop_head).unwrap_or_else(|| {
            panic!("site 4's `{loop_head}` loop is gone; re-point this witness")
        });
        let loop_body = {
            let rest = &extras[loop_start..];
            let stop = rest
                .find("\n    }\n")
                .unwrap_or_else(|| panic!("site 4's loop has no column-4 closing brace"));
            &rest[..stop]
        };
        assert!(
            loop_body.len() > 200,
            "site 4's loop body extracted as {} bytes — the scan is broken",
            loop_body.len()
        );
        assert!(
            !loop_body.contains(&ctor),
            "`register_exception_extras_natives` registers constructors from its class list \
             again. That list is a getMessage/toString bridge list; it is NOT a constructor \
             table, and it drifted away from the real one for long enough to keep 85 \
             non-existent descriptors registered. The constructors belong in the per-class \
             table in lang_misc.rs."
        );

        // --- 4. every class it does bridge is a row of the table -------------
        let list_start = extras
            .find("let exceptions = [")
            .expect("site 4's class list is gone; re-point this witness");
        let list_end = extras[list_start..]
            .find("];")
            .map(|i| list_start + i)
            .expect("site 4's class list has no terminator");
        let listed: Vec<&str> = extras[list_start..list_end]
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                line.strip_prefix('"')
                    .and_then(|rest| rest.strip_suffix("\","))
                    .filter(|c| c.contains('/'))
            })
            .collect();
        assert!(
            listed.len() >= 50,
            "parsed only {} classes out of site 4's list; the extractor is broken",
            listed.len()
        );
        for class in &listed {
            assert!(
                table.contains(class) || ALLOWED_OUTSIDE_THE_TABLE.contains(class),
                "`register_exception_extras_natives` bridges {class}, which is neither a row of \
                 the JDK-derived throwable table in lang_misc.rs nor in this witness's \
                 allow-list. Two lists of throwable class names that are allowed to disagree is \
                 exactly how the four copies of this rule drifted apart. Add it to the table \
                 (regenerate with scratchpad/e34/Gen34.java) or justify it here."
            );
        }

        // --- 5. the allow-list cannot be padded ------------------------------
        for exempt in ALLOWED_OUTSIDE_THE_TABLE {
            assert!(
                listed.contains(exempt),
                "{exempt} is exempted by this witness but is not actually registered by \
                 `register_exception_extras_natives` — a stale exemption is a hole"
            );
            assert!(
                !table.contains(exempt),
                "{exempt} is now a row of the JDK-derived table, so its hand-written \
                 constructors in `register_exception_extras_natives` are a second copy again. \
                 Delete them and remove it from ALLOWED_OUTSIDE_THE_TABLE."
            );
        }

        // --- 6. the constructors site 4 still registers are the named few ----
        // Four for the allow-listed class, plus one deliberate restatement of
        // `NullPointerException.<init>(Ljava/lang/String;)V` that carries the
        // opt-in CRATONVM_DBG_NPE_TRACE forensic. Pinned so that a new
        // constructor smuggled in here is a failing diff, not a silent one.
        let ctors_outside_the_loop = extras.matches(&ctor).count();
        assert_eq!(
            ctors_outside_the_loop,
            4 * ALLOWED_OUTSIDE_THE_TABLE.len() + 1,
            "`register_exception_extras_natives` registers {ctors_outside_the_loop} constructor \
             descriptors; it is allowed exactly four per allow-listed class plus the one \
             `NullPointerException.<init>(Ljava/lang/String;)V` restatement that keeps the \
             surefire NPE forensic alive. Anything else belongs in the table."
        );
    }
