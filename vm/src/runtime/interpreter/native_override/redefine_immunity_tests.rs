use super::redefine_immune_forced_native;

/// CratonVM's synthetic collections must keep their native shadow across a
/// redefinition. Their real JDK bodies index a `table`/`root`/`head` field
/// graph the synthetic objects do not have, so running them returns silent
/// nonsense — `TreeMap.get` null, `ConcurrentHashMap.size` 0,
/// `HashMap.keySet` empty. `RedefineCollectionLayoutProbe` is the witness.
#[test]
fn synthetic_collections_keep_their_natives_across_a_redefinition() {
    for class in [
        "java/util/ArrayDeque",
        "java/util/ArrayList",
        "java/util/HashMap",
        "java/util/HashSet",
        "java/util/IdentityHashMap",
        "java/util/LinkedHashMap",
        "java/util/LinkedHashSet",
        "java/util/LinkedList",
        "java/util/TreeMap",
        "java/util/TreeSet",
        "java/util/concurrent/ConcurrentHashMap",
    ] {
        for (name, desc) in [
            ("get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            ("size", "()I"),
            ("containsKey", "(Ljava/lang/Object;)Z"),
            ("keySet", "()Ljava/util/Set;"),
            ("entrySet", "()Ljava/util/Set;"),
            ("iterator", "()Ljava/util/Iterator;"),
            ("toString", "()Ljava/lang/String;"),
        ] {
            assert!(
                redefine_immune_forced_native(class, name, desc),
                "{class}.{name}{desc} must survive a redefinition"
            );
        }
    }
}

/// `ThreadLocal`'s values live in a Rust thread-local keyed by identity
/// hash, not in `Thread.threadLocals`, so its real JDK body answers `null`
/// for every value the process ever set. Mockito instruments the whole
/// superclass chain, so mocking ANY `ThreadLocal` subclass retransforms
/// `java.lang.ThreadLocal` and arms it — measured at
/// `ThreadLocal.get` native calls 23 -> 4 with the mock target as the only
/// variable. `probes/ThreadLocalRetransformProbe.java` is the witness.
#[test]
fn thread_local_keeps_its_natives_across_a_redefinition() {
    for class in ["java/lang/ThreadLocal", "java/lang/InheritableThreadLocal"] {
        for (name, desc) in [
            ("<init>", "()V"),
            ("get", "()Ljava/lang/Object;"),
            ("set", "(Ljava/lang/Object;)V"),
            ("remove", "()V"),
            ("initialValue", "()Ljava/lang/Object;"),
            (
                "withInitial",
                "(Ljava/util/function/Supplier;)Ljava/lang/ThreadLocal;",
            ),
        ] {
            assert!(
                redefine_immune_forced_native(class, name, desc),
                "{class}.{name}{desc} must survive a redefinition"
            );
        }
    }
}

/// Every operation CratonVM forces to its `ZipFile`/`JarFile` native must
/// survive a redefinition of those two classes on the SLOW-path
/// aggregator, and must NOT be immune on the invoke-cache one. One `spy()`
/// of a `JarFile` subclass retransforms the whole chain, and the real
/// bodies read a `res`/`zsrc` field graph no CratonVM archive has — but a
/// Mockito inline MOCK of the same class has no archive handle either, and
/// only the slow path can tell the two apart. See
/// `redefine_immune_layout_native`'s note on why this arm is the one that
/// must differ between the aggregators.
#[test]
fn leaves_the_zip_arm_to_the_receiver_aware_slow_path() {
    for name in [
        "<init>",
        "getEntry",
        "getInputStream",
        "entries",
        "stream",
        "getComment",
        "close",
        "getName",
        "isMultiRelease",
        "size",
    ] {
        assert!(
            redefine_immune_forced_native("java/util/zip/ZipFile", name, "()V"),
            "java/util/zip/ZipFile.{name} must survive a redefinition"
        );
        assert!(
            !super::redefine_immune_layout_native("java/util/zip/ZipFile", name, "()V"),
            "java/util/zip/ZipFile.{name} must NOT be immune on the invoke-cache \
             path: that path cannot tell a real archive from a mock of the same \
             class, so it has to defer to the receiver-aware slow path"
        );
    }
    for name in [
        "<init>",
        "getManifest",
        "getManifestFromReference",
        "stream",
        "entries",
        "getEntry",
        "getJarEntry",
        "getInputStream",
        "size",
        "close",
        "getName",
    ] {
        assert!(
            redefine_immune_forced_native("java/util/jar/JarFile", name, "()V"),
            "java/util/jar/JarFile.{name} must survive a redefinition"
        );
        assert!(
            !super::redefine_immune_layout_native("java/util/jar/JarFile", name, "()V"),
            "java/util/jar/JarFile.{name} must NOT be immune on the invoke-cache \
             path: see the ZipFile arm above"
        );
    }
}

/// The immunity is method-wise, and its boundary is the force-native list:
/// an operation CratonVM does NOT claim is ordinary bytecode and must stay
/// evictable, or an agent could never weave it. A `JarFile` SUBCLASS is
/// ordinary throughout -- its own bodies are its own bytecode, and
/// `NestedJarFile.getInputStream` is exactly such an override.
#[test]
fn leaves_unclaimed_zip_operations_evictable() {
    for (class, name) in [
        ("java/util/jar/JarFile", "getVersion"),
        ("java/util/jar/JarFile", "isSigned"),
        ("java/util/zip/ZipFile", "getComment2"),
        (
            "org/springframework/boot/loader/jar/NestedJarFile",
            "getInputStream",
        ),
    ] {
        assert!(
            !redefine_immune_forced_native(class, name, "()V"),
            "{class}.{name} must stay evictable"
        );
    }
}

/// The two aggregators are consulted by different dispatch paths — the
/// invoke-cache sites use `redefine_immune_layout_native`, everything else
/// `redefine_immune_forced_native`. An arm added to one only is the failure
/// the collection entry already made once: the probe went from 32 broken
/// operations to 18 instead of to 0, because the cache sites re-assembled
/// their own chain and never saw it. So assert the layout aggregator
/// directly rather than trusting that both were edited.
#[test]
fn thread_local_immunity_reaches_the_invoke_cache_sites_too() {
    for class in ["java/lang/ThreadLocal", "java/lang/InheritableThreadLocal"] {
        assert!(
            super::redefine_immune_layout_native(class, "get", "()Ljava/lang/Object;"),
            "{class}.get must be immune on the invoke-cache path as well"
        );
    }
}

/// A VM-minted carrier has no bytecode in any image, so the "yield to the
/// woven body" rule has nothing to yield TO -- `find_method_recursive` walks
/// past it to `java/lang/Object`, whose generation Mockito bumps the first
/// time anything mocks a class. Measured: without this arm, one `mock()`
/// made every `Collections.unmodifiableList` / `List.of` in the process
/// compare by identity.
#[test]
fn vm_minted_carriers_keep_their_natives_across_a_redefinition() {
    for class in [
        "cratonvm/internal/UnmodifiableList",
        "cratonvm/internal/UnmodifiableSet",
        "cratonvm/internal/UnmodifiableMap",
        "cratonvm/internal/UnmodifiableCollection",
        "cratonvm/internal/UnmodifiableSortedSet",
        "cratonvm/internal/UnmodifiableNavigableSet",
        "cratonvm/internal/UnmodifiableEntrySet",
        "cratonvm/internal/UnmodifiableMapEntry",
        "cratonvm/internal/ArrayListSubList",
        "cratonvm/internal/foreign/MemorySegmentImpl",
    ] {
        for (name, desc) in [
            ("equals", "(Ljava/lang/Object;)Z"),
            ("hashCode", "()I"),
            ("size", "()I"),
            ("toString", "()Ljava/lang/String;"),
        ] {
            assert!(
                redefine_immune_forced_native(class, name, desc),
                "{class}.{name}{desc} must survive a redefinition"
            );
        }
    }
}

/// Same arm, the other aggregator -- the failure
/// `thread_local_immunity_reaches_the_invoke_cache_sites_too` exists to
/// catch, made once already by the collection arm.
#[test]
fn vm_minted_carrier_immunity_reaches_the_invoke_cache_sites_too() {
    assert!(super::redefine_immune_layout_native(
        "cratonvm/internal/UnmodifiableList",
        "equals",
        "(Ljava/lang/Object;)Z"
    ));
}

#[test]
fn ordinary_classes_stay_evictable() {
    assert!(!redefine_immune_forced_native(
        "com/example/Service",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;"
    ));
    // The prefix is `cratonvm/internal/`, not `cratonvm`: an application
    // class that merely starts with the vendor word is an ordinary class.
    assert!(!redefine_immune_forced_native(
        "cratonvm/app/Service",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;"
    ));
    // A ThreadLocal SUBCLASS is an ordinary class: its own methods are its
    // own bytecode, dispatch resolves to them before this gate is reached,
    // and a mock of one must still be able to intercept them. Only the two
    // JDK classes whose bodies CratonVM replaces are immune.
    assert!(!redefine_immune_forced_native(
        "org/springframework/core/NamedThreadLocal",
        "get",
        "()Ljava/lang/Object;"
    ));
    // The two operations the Mockito suite genuinely stubs on a mocked
    // StringBuilder — see `is_string_builder_layout_native_override`.
    assert!(!redefine_immune_forced_native(
        "java/lang/StringBuilder",
        "length",
        "()I"
    ));
    assert!(!redefine_immune_forced_native(
        "java/lang/StringBuilder",
        "substring",
        "(I)Ljava/lang/String;"
    ));
}

/// No dispatch site may name a layout arm directly: they must go through
/// `redefine_immune_layout_native` (cache paths) or
/// `redefine_immune_forced_native` (slow path). Six sites used to open-code
/// `string_builder || path`, so the collections entry silently did not apply
/// there and the collection probe stalled at 18 of 32 rather than 0.
///
/// The reflection arm is NOT policed: two cache sites legitimately omit it,
/// and forcing them to include it regressed ByteBuddy type creation.
#[test]
fn layout_immunity_is_not_open_coded() {
    // The aggregators this gate polices moved out of  with the
    // rest of the override policy in the SEAM-02 split, and this test moved
    // with them. Scan the file they are actually in — which is this one.
    //
    // A third way for this gate to go stale, after the hard-coded line band
    // and the declaration parser described above: pointing at a file the
    // subject has left. That fails CLOSED here (the aggregator lookup finds
    // nothing and the assertion below fires), which is the right direction,
    // and is how the split found it.
    // Normalise line endings before ANY byte-offset arithmetic below.
    // `include_str!` embeds the file's raw bytes and this repository is
    // checked out with CRLF on Windows (`core.autocrlf=true`). Two things
    // then go wrong, and the second is the one that bit:
    //
    //   * the `"\n}"` body terminator still matches (`\r\n}` contains
    //     `\n}`), so that one is fine either way; but
    //   * `offset += line.len() + 1` below assumes `lines()` stripped ONE
    //     byte. On CRLF it strips two, so `line_start` drifts a byte per
    //     line. By the aggregators (~line 4900) it is ~4900 bytes short of
    //     the true offset, the `inside_aggregator` range check misses, and
    //     the aggregators' OWN arms are reported as offenders — the gate
    //     failing for a reason that has nothing to do with what it
    //     polices, which is precisely the staleness mode the comment above
    //     was written to prevent. A fourth way to go stale, after the two
    //     it already lists.
    //
    // Same fix, same reason, as `jit::ir_lower`'s `declared_op_variants`
    // and `vm::runtime::env_cache`'s flags scan.
    // A FIFTH staleness mode, found 2026-08-06: this gate policed exactly
    // one file. The rule it states — "an arm may be named only inside the
    // two aggregators" — is a rule about the override policy, not about a
    // file, and the aggregators are `pub(super)`, so every sibling module
    // can name an arm and none of them were being read.
    //
    // `vm/src/runtime/interpreter.rs` did: its no-`Code` dispatch arm
    // hand-rolled `reflection && string_builder && path`, the aggregate
    // minus five arms including `synthetic_collection`. That is the same
    // open-coding whose 2026-07-31 instance took the collection probe to 18
    // rather than 0 — the incident written up on
    // `redefine_immune_forced_native` as the reason this gate exists. It
    // sat in a sibling file for as long as the gate has been green.
    //
    // Scanning the whole `runtime` subtree is the fix. `include_str!` needs
    // literal paths, so the sibling list is explicit; a new module that
    // names an arm is not covered until it is added here, which is the
    // remaining hole and is at least a hole in one obvious place.
    let sources: [(&str, String); 7] = [
        (
            "native_override.rs",
            include_str!("../native_override.rs").replace("\r\n", "\n"),
        ),
        (
            "../interpreter.rs",
            concat!(include_str!("../../interpreter/mod.rs"), "\n", include_str!("../../interpreter/cached_invoke_target.rs"), "\n", include_str!("../../interpreter/execute.rs"), "\n", include_str!("../../interpreter/resume_continuation.rs"), "\n", include_str!("../../interpreter/execute_frame_from_index_loop.rs"), "\n", include_str!("../../interpreter/resolve_array_class_loader_aware.rs"), "\n", include_str!("../../interpreter/park_for_debugger_under.rs"), "\n", include_str!("../../interpreter/double_arith_apply.rs")).replace("\r\n", "\n"),
        ),
        // `execute`'s no-`Code` arm, the site that once open-coded the
        // aggregate, moved here verbatim in interpreter round i1 wave 4.
        (
            "execute_entry.rs",
            include_str!("../execute_entry.rs").replace("\r\n", "\n"),
        ),
        ("invoke.rs", include_str!("../invoke.rs").replace("\r\n", "\n")),
        (
            "dispatch_virtual.rs",
            include_str!("../dispatch_virtual.rs").replace("\r\n", "\n"),
        ),
        (
            "../instrument.rs",
            include_str!("../../instrument.rs").replace("\r\n", "\n"),
        ),
        (
            "../../vm/vm_exec.rs",
            concat!(include_str!("../../../vm/vm_exec/mod.rs"), "\n", include_str!("../../../vm/vm_exec/method.rs"), "\n", include_str!("../../../vm/vm_exec/report_pad_access.rs"), "\n", include_str!("../../../vm/vm_exec/diagnose_sync_on_value_based_class.rs"), "\n", include_str!("../../../vm/vm_exec/thread_interrupted_field_slot_of.rs"), "\n", include_str!("../../../vm/vm_exec/clear_breakpoints_of_redefined_class.rs"), "\n", include_str!("../../../vm/vm_exec/load_class.rs"), "\n", include_str!("../../../vm/vm_exec/native_new_initializes_class_first.rs"), "\n", include_str!("../../../vm/vm_exec/new_object.rs"), "\n", include_str!("../../../vm/vm_exec/gen_referent_read_keep_alive.rs"), "\n", include_str!("../../../vm/vm_exec/append_native_standin_frames.rs"), "\n", include_str!("../../../vm/vm_exec/unloaded_native_libraries.rs"), "\n", include_str!("../../../vm/vm_exec/extract_annotations_from_attributes.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_static_settling_with_first.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_special_shared.rs"), "\n", include_str!("../../../vm/vm_exec/proxy_resolve_declaring_method.rs"), "\n", include_str!("../../../vm/vm_exec/invoke_on_class_shared_inner.rs"), "\n", include_str!("../../../vm/vm_exec/jni_table_native_call.rs")).replace("\r\n", "\n"),
        ),
    ];

    // The aggregators live in THIS file, so the exemption range is computed
    // from this file's text and applies only while scanning it.
    let src = sources[0].1.as_str();

    // The exemption is the RULE, located in the source: an arm may be named
    // only inside the two aggregators, whose entire job is to compose them.
    //
    // Twice now this gate has been written as a proxy for that rule, and
    // twice the proxy went stale against a growing file. First a hard-coded
    // line band, `(5000..9800)`: `redefine_immune_forced_native` slid down
    // to 9793-9822, so its own arms were reported as offenders and the gate
    // failed for a reason that had nothing to do with what it polices.
    // Then an enclosing-function tracker that recognised exactly four
    // declaration spellings — a function written any other way never
    // updated the name, so its body was attributed to whatever came before.
    // That one failed OPEN, which is worse: dropping
    //
    //     pub(super) unsafe fn probe(class_name: &str) -> bool {
    //         redefine_immune_synthetic_collection_native(class_name)
    //     }
    //
    // straight after `redefine_immune_synthetic_collection_native` PASSED,
    // because the stale name was that exempt predicate's.
    //
    // So: no line numbers, no declaration parsing, no carried state. Find
    // the aggregator bodies and ask whether the call is inside one. If an
    // aggregator is ever renamed this stops finding it and its own arms
    // start failing — loud, and the right direction to fail in.
    //
    // The list IS the exemption, and its LENGTH is the findability
    // check: a composer added here can never silently widen the gate's
    // blind spot without also being visible in this array.
    const EXEMPT_BODIES: [&str; 3] = [
        "redefine_immune_layout_native",
        "redefine_immune_forced_native",
        // The receiver-aware composer. It names the ZIP arm for the same
        // reason the two aggregators name their own: composing them IS its
        // job. What the gate forbids is a DISPATCH SITE open-coding an arm,
        // and this is not one — every dispatch site calls
        // `redefine_immune_forced_native_for_receiver`, never this.
        "zip_immunity_waived_for_receiver",
    ];
    let aggregator_bodies: Vec<(usize, usize)> = EXEMPT_BODIES
        .iter()
        .filter_map(|name| {
            let start = src.find(&format!("fn {name}("))?;
            // A top-level body ends at the first `}` in column 0 after it.
            let end = src[start..]
                .find("\n}")
                .map_or(src.len(), |i| start + i + 2);
            Some((start, end))
        })
        .collect();
    let missing: Vec<&str> = EXEMPT_BODIES
        .iter()
        .copied()
        .filter(|name| !src.contains(&format!("fn {name}(")))
        .collect();
    assert!(
        missing.is_empty() && aggregator_bodies.len() == EXEMPT_BODIES.len(),
        "every exempt body must be findable. Not found: {missing:?}"
    );

    let mut offenders = Vec::new();
    for (file, text) in &sources {
        // The aggregator exemption is a byte range in `native_override.rs`
        // only. In any other file there is nothing to exempt — naming an
        // arm there is the offence, wherever in the file it sits.
        let exempt: &[(usize, usize)] = if *file == "native_override.rs" {
            &aggregator_bodies
        } else {
            &[]
        };
        let mut offset = 0usize;
        for (n, line) in text.lines().enumerate() {
            let line_start = offset;
            offset += line.len() + 1; // `lines()` strips a single `\n`
            let code = line.trim_start();
            // Comments, and this test's own list of names (string literals).
            if code.starts_with("//") || code.starts_with('"') {
                continue;
            }
            if exempt
                .iter()
                .any(|&(start, end)| line_start >= start && line_start < end)
            {
                continue;
            }
            for part in [
                "redefine_immune_string_builder_native(",
                "redefine_immune_path_native(",
                "redefine_immune_jfr_native(",
                "redefine_immune_synthetic_collection_native(",
                "redefine_immune_vm_minted_carrier_native(",
                "redefine_immune_thread_local_native(",
                "redefine_immune_zip_file_native(",
            ] {
                // An arm's own `fn` declaration is not a call site.
                if code.contains(part) && !code.contains(&format!("fn {part}")) {
                    offenders.push(format!("{}:{}: {}", file, n + 1, code));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "call redefine_immune_layout_native (cache paths) or \
         redefine_immune_forced_native (slow path) instead of naming an arm:\n{}",
        offenders.join("\n")
    );
}
