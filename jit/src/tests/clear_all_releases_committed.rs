#[allow(unused_imports)]
use super::*;

/// `clear_all` must return every unreferenced executable mapping, and with
/// it the `COMMITTED_JIT_CODE_BYTES` those mappings account for.
///
/// `COMMITTED_JIT_CODE_BYTES` is process-global and this lib test binary
/// runs its tests on a thread pool, so absolute before/after totals are NOT
/// a stable observable: every other test that allocates or drops an
/// `ExecutableBuffer` moves the same counter. The old
/// `populated >= before + 8 * 4096` / `reclaimed <= populated - 8 * 4096`
/// pair therefore failed whenever concurrent frees (resp. allocations)
/// happened to outpace this test's own bookkeeping between the two loads.
///
/// What this test owns instead:
///  * the counter is an exact ledger — `ExecutableBuffer::new` adds
///    its charge and its `Drop` subtracts that charge — so at any
///    instant it equals the summed charges of all *live* buffers. That
///    makes `>= our own live bytes` a sound lower bound no matter what
///    other threads are doing.
///  * `Drop` is the *only* site that decrements the counter and releases
///    the mapping, so proving `clear_all` dropped the last owner of each
///    body proves exactly `committed_by_test` bytes went back.
#[test]
pub(in crate::tests) fn test_clear_all_releases_committed_executable_bytes() {
    const BODY_BYTES: usize = 4096;
    const BODIES: u32 = 8;

    let cache = JitCache::new();
    let mut bodies = Vec::with_capacity(BODIES as usize);
    let mut committed_by_test = 0;
    for i in 0..BODIES {
        let mut buf = ExecutableBuffer::new(BODY_BYTES).expect("alloc cache body");
        buf.emit(&[0xC3]);
        let class: Arc<str> = Arc::from("ReclaimBytes");
        let method: Arc<str> = Arc::from(format!("m{i}"));
        let desc: Arc<str> = Arc::from("()V");
        let cid = cratonvm_types::ClassId::new(i + 1);
        let compiled = CompiledMethod::new(buf);
        committed_by_test += compiled._buffer.charged;
        cache.put(class.clone(), method.clone(), desc.clone(), cid, compiled);
        let cm = cache.get(&class, &method, &desc, cid).expect("published");
        bodies.push(Arc::downgrade(&cm));
    }

    // Our eight mappings are live, so the global ledger must be at least
    // that large — every other contribution to it is a live buffer too.
    let populated = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        populated >= committed_by_test,
        "committed ledger {populated} is below this test's own live mappings \
         ({committed_by_test} bytes)"
    );

    assert_eq!(cache.clear_all(), BODIES as usize);

    // Every body lost its last owner, so `CompiledMethod::drop` ran for all
    // eight — unmapping the code and returning `committed_by_test` bytes.
    for (i, weak) in bodies.iter().enumerate() {
        assert!(
            eventually_drained(|| weak.strong_count() == 0),
            "clear_all must return body m{i}'s executable mapping"
        );
    }
}

#[test]
pub(in crate::tests) fn test_jit_cache_get_missing() {
    let cache = JitCache::new();
    let class: Arc<str> = Arc::from("Missing");
    let method: Arc<str> = Arc::from("missing");
    let desc: Arc<str> = Arc::from("()V");
    assert!(cache
        .get(&class, &method, &desc, cratonvm_types::ClassId::new(1))
        .is_none());
}

/// A type-check site's `(ptr, len)` pair is the KEY of two thread-local
/// memos that outlive any one compiled method
/// (`JIT_TYPECHECK_TARGET_CACHE`, `JIT_TYPECHECK_ANSWER_CACHE` in
/// `vm/src/jit/helpers.rs`). It must therefore be a permanent, unique
/// identity for one class name.
///
/// Regression: these names used to be `Box<str>`s owned by
/// `CompiledMethod::_jit_strings`, freed on tier-up / invalidation /
/// eviction, so the allocator recycled the block for the next
/// compilation's name of the same length and both memos then answered for
/// the class that used to live there. `java/lang/String` and
/// `java/lang/Number` are both 16 bytes, which is how a `String` came to
/// pass `instanceof java/lang/Number` and blow up
/// `scala.runtime.BoxesRunTime.equals2`'s very next `checkcast`.
#[test]
pub(in crate::tests) fn typecheck_class_names_are_interned_by_content_not_owned_by_a_compilation() {
    let (p_string, l_string) = intern_typecheck_class_name("java/lang/String");
    assert_eq!(l_string, 16);

    // Churn the allocator the way a run of compile-then-drop cycles does.
    // Nothing here may be handed the interned block back.
    let mut churn: Vec<Box<str>> = Vec::new();
    for _ in 0..512 {
        churn.push(String::from("java/lang/Number").into_boxed_str());
        churn.push(String::from("java/lang/Object").into_boxed_str());
        if churn.len() > 8 {
            churn.drain(..4);
        }
    }
    drop(churn);

    let (p_number, l_number) = intern_typecheck_class_name("java/lang/Number");
    assert_eq!(l_number, l_string, "the two names are the same length");
    assert_ne!(
        p_string, p_number,
        "two different class names must never share a (ptr, len) key"
    );

    // Same content -> same pointer, so two sites checking the same class
    // share one memo entry instead of evicting each other.
    assert_eq!(
        intern_typecheck_class_name("java/lang/String"),
        (p_string, l_string)
    );

    // And the bytes still read back as themselves after all that churn.
    for (ptr, len, expect) in [
        (p_string, l_string, "java/lang/String"),
        (p_number, l_number, "java/lang/Number"),
    ] {
        // SAFETY: interned entries are leaked and never freed.
        let s = unsafe { std::str::from_utf8(std::slice::from_raw_parts(ptr, len)) }.unwrap();
        assert_eq!(s, expect);
    }
}

/// Two loaders' same-named classes are two classes, and a type-check site
/// must be able to say which one it meant.
///
/// The class dictionary is keyed by `(ClassLoaderId, name)`, but a
/// compiled type-check site used to carry only the name — so
/// `jit_typecheck_resolve` re-resolved it at run time, could land on the
/// wrong copy, and covered for that with a loader-blind name walk that
/// accepted *either* copy. Interning by `(name, resolved ClassId)` gives
/// each copy its own site identity and lets the helper answer by identity.
#[test]
pub(in crate::tests) fn a_typecheck_site_is_interned_under_the_class_id_its_loader_resolved() {
    let (p_a, l_a) = intern_typecheck_target("com/example/Forked", Some(4_242));
    let (p_b, l_b) = intern_typecheck_target("com/example/Forked", Some(4_243));
    assert_eq!(l_a, l_b);
    assert_ne!(
        p_a, p_b,
        "one name resolved to two loaders' classes must be two sites"
    );
    assert_eq!(typecheck_target_for_site(p_a), Some(4_242));
    assert_eq!(typecheck_target_for_site(p_b), Some(4_243));

    // Stable: recompiling the same site rejoins the same identity.
    assert_eq!(
        intern_typecheck_target("com/example/Forked", Some(4_242)),
        (p_a, l_a)
    );

    // A site whose target was not loaded at compile time records nothing,
    // and so keeps the pre-existing name-resolution behaviour.
    let (p_none, _) = intern_typecheck_class_name("com/example/Forked");
    assert_ne!(p_none, p_a);
    assert_eq!(typecheck_target_for_site(p_none), None);
}

/// Interpreter round i1 wave 12, L2 (i11-L2): an unbound type-check site
/// carries its holder AND the index of its `CONSTANT_Class` entry, the key
/// JVMS §5.4.3 records a resolution failure under, so the run-time helper can
/// consult and record it as the interpreter's `checkcast` does.
#[test]
pub(in crate::tests) fn an_unbound_typecheck_site_carries_its_holder_and_cp_index() {
    const NAME: &str = "com/example/i12l2/Unbound";
    let (p_a, _) = intern_typecheck_unresolved_in(NAME, 7_001, 11);
    let (p_b, _) = intern_typecheck_unresolved_in(NAME, 7_001, 12);
    let (p_c, _) = intern_typecheck_unresolved_in(NAME, 7_002, 11);
    assert_ne!(p_a, p_b, "two entries of one pool are two sites");
    assert_ne!(p_a, p_c, "two holders are two sites");
    assert_eq!(typecheck_holder_for_site(p_a), Some((7_001, 11)));
    assert_eq!(typecheck_holder_for_site(p_b), Some((7_001, 12)));
    assert_eq!(typecheck_holder_for_site(p_c), Some((7_002, 11)));
    assert_eq!(typecheck_target_for_site(p_a), None);
    // Stable across recompiles of the same site.
    assert_eq!(intern_typecheck_unresolved_in(NAME, 7_001, 11).0, p_a);
    // The JIT-crate doors' mapping of a deferred `new`-site answer keeps the
    // index as well.
    let (p_new, _) = intern_typecheck_for_new_site(
        NAME,
        Some(JitNewSite::Deferred {
            holder_class_id: 7_001,
            cp_idx: 11,
        }),
        None,
    );
    assert_eq!(p_new, p_a);
    // Resolved at run time: the site becomes a recorded target and stops
    // answering as unbound.
    let (p_rec, _) = intern_typecheck_unresolved_in(NAME, 7_003, 5);
    record_typecheck_target_for_site(p_rec, 9_999);
    assert_eq!(typecheck_holder_for_site(p_rec), None);
    assert_eq!(typecheck_target_for_site(p_rec), Some(9_999));
}

/// T10.3 — Verify the FxHashMap swap preserves insert/lookup semantics for
/// 100 distinct JitKey entries with different class/method/descriptor mixes.
#[test]
pub(in crate::tests) fn t10_jit_cache_fxhash_insert_lookup() {
    let mut cache = JitCache::new();
    let mut keys: Vec<(Arc<str>, Arc<str>, Arc<str>, cratonvm_types::ClassId)> =
        Vec::with_capacity(100);
    for i in 0..100 {
        let class: Arc<str> = Arc::from(format!("pkg/Cls{i}"));
        let method: Arc<str> = Arc::from(format!("m{i}"));
        let desc: Arc<str> = Arc::from(format!("(I)I{i}"));
        let cid = cratonvm_types::ClassId::new((i + 1) as u32);
        let mut buf = ExecutableBuffer::new(16).expect("alloc failed");
        buf.emit(&[0xC3]); // RET
        let cm = CompiledMethod::new(buf);
        cache.put(class.clone(), method.clone(), desc.clone(), cid, cm);
        keys.push((class, method, desc, cid));
    }
    assert_eq!(cache.len(), 100);
    for (class, method, desc, cid) in &keys {
        assert!(
            cache.get(class, method, desc, *cid).is_some(),
            "missing key {}/{}/{}",
            class,
            method,
            desc
        );
    }
    // Negative lookup: a class name we never inserted.
    let missing_cls: Arc<str> = Arc::from("pkg/Unseen");
    let missing_m: Arc<str> = Arc::from("x");
    let missing_d: Arc<str> = Arc::from("()V");
    assert!(cache
        .get(
            &missing_cls,
            &missing_m,
            &missing_d,
            cratonvm_types::ClassId::new(9999)
        )
        .is_none());
}

// ── count_param_slots tests ─────────────────────────────────────

#[test]
pub(in crate::tests) fn test_count_param_slots_basic() {
    assert_eq!(count_param_slots("(II)I"), 2);
    assert_eq!(count_param_slots("()V"), 0);
    assert_eq!(count_param_slots("(I)V"), 1);
}

#[test]
pub(in crate::tests) fn test_count_param_slots_long_double() {
    assert_eq!(count_param_slots("(JD)V"), 2);
    assert_eq!(count_param_slots("(IJI)I"), 3);
}

#[test]
pub(in crate::tests) fn test_count_param_slots_objects_arrays() {
    assert_eq!(count_param_slots("(Ljava/lang/String;)V"), 1);
    assert_eq!(count_param_slots("([I[Ljava/lang/Object;I)V"), 3);
    assert_eq!(count_param_slots("([[I)V"), 1);
}

#[test]
pub(in crate::tests) fn test_count_param_slots_empty_string() {
    assert_eq!(count_param_slots(""), 0);
}

// ── indy_arg_type_tags tests ─────────────────────────────────────

#[test]
pub(in crate::tests) fn indy_arg_type_tags_matches_the_writingservlet_concat_shape() {
    // WritingServlet.doGet's `makeConcatWithConstants` bootstrap
    // descriptor: (int length, String buffered, long elapsedNanos).
    assert_eq!(
        indy_arg_type_tags("(ILjava/lang/String;J)Ljava/lang/String;"),
        vec![b'I', b'L', b'J']
    );
}

#[test]
pub(in crate::tests) fn indy_arg_type_tags_one_tag_per_compact_slot() {
    assert_eq!(
        count_param_slots("(IJDF)V"),
        indy_arg_type_tags("(IJDF)V").len()
    );
    assert_eq!(indy_arg_type_tags("(IJDF)V"), vec![b'I', b'J', b'D', b'F']);
}

#[test]
pub(in crate::tests) fn indy_arg_type_tags_arrays_are_l() {
    assert_eq!(
        indy_arg_type_tags("([I[Ljava/lang/Object;)V"),
        vec![b'L', b'L']
    );
}

#[test]
pub(in crate::tests) fn indy_arg_type_tags_empty() {
    assert_eq!(indy_arg_type_tags("()V"), Vec::<u8>::new());
    assert_eq!(indy_arg_type_tags(""), Vec::<u8>::new());
}

#[test]
pub(in crate::tests) fn test_compute_param_jvm_slots_category2_instance() {
    assert_eq!(
        compute_param_jvm_slots("(Ljava/lang/Object;JZ)V", false),
        (vec![0, 1, 2, 4], 5)
    );
    assert_eq!(
        compute_param_jvm_slots("(JLjava/lang/Object;)V", true),
        (vec![0, 2], 3)
    );
}

#[test]
pub(in crate::tests) fn invokestatic_self_call_tail_jump_predicate() {
    let tail_call = [0x1a, 0xb8, 0x00, 0x01, 0xac, 0x00, 0x00];
    assert!(invokestatic_self_call_uses_tail_jump(&tail_call, 5, 1));

    let non_tail_call = [0x1a, 0xb8, 0x00, 0x01, 0x1a, 0xac, 0x00, 0x00];
    assert!(!invokestatic_self_call_uses_tail_jump(&non_tail_call, 6, 1));

    let void_return_tail_shape = [0xb8, 0x00, 0x01, 0xb1, 0x00, 0x00];
    assert!(!invokestatic_self_call_uses_tail_jump(
        &void_return_tail_shape,
        4,
        0
    ));
}

// ── return_type tests ───────────────────────────────────────────






/// Interpreter round i1 wave 11: no method is kept on dispatch by its NAME.
/// BCEL's `ClassParser.readInterfaces` (the `TOMCAT-SILENT-HANG.5` edge) was;
/// it now answers like its neighbour, and only a recorded recursion cycle
/// keeps a statically bound call on `jit_invoke_dispatch`. A fresh registry,
/// so no other test's cycle can answer here.
#[test]
pub(in crate::tests) fn no_method_is_kept_on_dispatch_by_its_name() {
    let verdicts = JitVerdictRegistry::default();
    for method in ["readInterfaces", "readFields"] {
        assert!(
            !verdicts.direct_call_requires_dispatch(
                "org/apache/tomcat/util/bcel/classfile/ClassParser",
                method,
                "()V",
            ),
            "{method} must bind like any other method"
        );
    }
}

#[test]
pub(in crate::tests) fn test_return_type_basic() {
    assert_eq!(return_type("(II)I"), b'I');
    assert_eq!(return_type("()V"), b'V');
    assert_eq!(return_type("(I)J"), b'J');
    assert_eq!(return_type("()Ljava/lang/String;"), b'L');
}

#[test]
pub(in crate::tests) fn test_return_type_no_closing_paren() {
    // Malformed descriptor — should return 'V' as default
    assert_eq!(return_type("II"), b'V');
}

// ── return_type edge cases ─────────────────────────────────────

#[test]
pub(in crate::tests) fn test_return_type_array() {
    assert_eq!(return_type("()[I"), b'[');
}

#[test]
pub(in crate::tests) fn test_return_type_double() {
    assert_eq!(return_type("(II)D"), b'D');
}

// ── Escape analysis IR wiring tests ────────────────────────────

#[test]
pub(in crate::tests) fn test_ea_from_ir_returns_id_map() {
    // Build a minimal IR graph: Start(0) → Const(1) → Return(2)
    let mut g = ir::Graph::empty(0);
    let start = g.add(ir::Op::Start, ir::IrType::Void, vec![], None);
    let c = g.add(ir::Op::Const(42), ir::IrType::Int, vec![], None);
    let ret = g.add(ir::Op::Return, ir::IrType::Void, vec![start, c], None);
    g.entry = start;
    g.exit = ret;

    let (ea_graph, id_map) = escape_analysis_from_ir(&g);

    // id_map length matches IR node count
    assert_eq!(id_map.len(), g.nodes.len());
    // Entry maps to EA Start (0), Exit maps to EA Return (1)
    assert_eq!(id_map[start as usize], 0);
    assert_eq!(id_map[ret as usize], 1);
    // Const node should map to a valid EA node (not usize::MAX)
    assert_ne!(id_map[c as usize], usize::MAX);
    // EA graph should have nodes
    assert!(ea_graph.nodes.len() >= 3);
}

#[test]
pub(in crate::tests) fn test_apply_ea_marks_dead_nodes() {
    // Build an IR graph with: Start, New, Const, Store, Load, Return
    // The New doesn't escape, so EA should find it scalar-replaceable.
    let mut g = ir::Graph::empty(0);
    let start = g.add(ir::Op::Start, ir::IrType::Void, vec![], None);
    let alloc = g.add(
        ir::Op::New {
            class_id: 1,
            num_fields: 2,
        },
        ir::IrType::Ref,
        vec![start],
        None,
    );
    let c42 = g.add(ir::Op::Const(42), ir::IrType::Int, vec![], None);
    // Store field 0 (MemKind::Int = discriminant 0)
    let store = g.add(
        ir::Op::Store(ir::MemKind::Int),
        ir::IrType::Void,
        vec![alloc, c42],
        None,
    );
    // Load field 0 (MemKind::Int = discriminant 0)
    let load = g.add(
        ir::Op::Load(ir::MemKind::Int),
        ir::IrType::Int,
        vec![alloc],
        None,
    );
    let ret = g.add(ir::Op::Return, ir::IrType::Void, vec![start, load], None);
    g.entry = start;
    g.exit = ret;

    // Run EA
    let (ea_graph, id_map) = escape_analysis_from_ir(&g);
    let ea_result = escape_analysis::analyze_escapes(&ea_graph);

    // Apply to IR
    if !ea_result.scalar_replaceable.is_empty() || !ea_result.elide_locks.is_empty() {
        apply_ea_to_ir(&mut g, &id_map, &ea_result);
    }

    // If EA found the alloc as scalar-replaceable, the allocation,
    // store, and load should all be Dead now.
    if !ea_result.scalar_replaceable.is_empty() {
        assert_eq!(g.nodes[alloc as usize].op, ir::Op::Dead);
        assert_eq!(g.nodes[store as usize].op, ir::Op::Dead);
        assert_eq!(g.nodes[load as usize].op, ir::Op::Dead);
        // The return node's input that was the load should now point
        // to the stored constant value (c42).
        assert!(
            g.nodes[ret as usize].inputs.contains(&c42),
            "Return should reference the constant directly after scalar replacement"
        );
    }
}

// --- Phase 80.3: Value Enum Layout Safety Tests ---

#[test]
pub(in crate::tests) fn value_size_is_16_bytes() {
    assert_eq!(std::mem::size_of::<Value>(), 16);
}

#[test]
pub(in crate::tests) fn value_alignment_at_most_8() {
    assert!(std::mem::align_of::<Value>() <= 8);
}

#[test]
pub(in crate::tests) fn objectref_is_pointer_sized() {
    assert_eq!(
        std::mem::size_of::<ObjectRef>(),
        std::mem::size_of::<*mut u8>()
    );
}

#[test]
pub(in crate::tests) fn value_to_bytes_roundtrip() {
    // Verify value_to_bytes produces correct bytes for known values.
    let val = Value::Int(0x12345678);
    let bytes = super::value_to_bytes(val);
    // The Int discriminant and payload must be present somewhere in the 16 bytes.
    // Check the i32 payload bytes appear.
    let payload = 0x12345678_i32.to_le_bytes();
    let found = (0..13).any(|i| bytes[i..i + 4] == payload);
    assert!(found, "Int payload must be present in byte representation");
}

#[test]
pub(in crate::tests) fn probe_object_ptr_offset_in_range() {
    // The offset must be a valid position within a 16-byte Value.
    let off = super::probe_object_ptr_offset();
    assert!(
        off < 9,
        "offset must leave room for 8-byte pointer within 16 bytes, got {off}"
    );
}

#[test]
pub(in crate::tests) fn probe_object_null_template_reconstructs_none() {
    // The null template (lo, hi) must reconstruct to Value::Object(None)
    // when written back and transmuted.
    let (lo, hi) = super::probe_object_null_template();
    let mut bytes = [0u8; 16];
    bytes[0..8].copy_from_slice(&lo.to_le_bytes());
    bytes[8..16].copy_from_slice(&hi.to_le_bytes());
    // SAFETY: `Value` is 16 bytes (asserted at compile time), and these bytes are the template the VM derived from a real `Value::Object(None)`.
    let reconstructed: Value = unsafe { std::mem::transmute(bytes) };
    assert_eq!(
        reconstructed,
        Value::Object(None),
        "null template must reconstruct to Value::Object(None)"
    );
}

// ===================================================================
// Session 33: JIT Monomorphic Inline Cache (MIC) tests
// ===================================================================

#[test]
pub(in crate::tests) fn s33_mic_slot_new_is_empty() {
    let mic = JitMICSlot::new();
    assert_eq!(
        mic.cached_class_id
            .load(std::sync::atomic::Ordering::Relaxed),
        JitMICSlot::EMPTY_CLASS_ID
    );
    assert!(mic.cached_class_name.lock().is_none());
    assert_eq!(mic.cached_entry(), (0, false));
    assert_eq!(mic.hits.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(mic.misses.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(mic.total_observations(), 0);
    assert_eq!(mic.hit_rate_pct(), 0);
}

#[test]
pub(in crate::tests) fn s33_mic_slot_prepopulate() {
    let mic = JitMICSlot::new();
    mic.prepopulate(42);
    assert_eq!(
        mic.cached_class_id
            .load(std::sync::atomic::Ordering::Relaxed),
        42
    );
    // Entry word should still be 0 (prepopulate only sets class_id)
    assert_eq!(
        mic.cached_entry_word
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[test]
pub(in crate::tests) fn s33_mic_slot_update_all_fields() {
    let mic = JitMICSlot::new();
    // Even: bit 0 of an entry word is the context tag.
    mic.update(7, "com/example/MyClass", 0xDEAD_BEE0, true, false);
    assert_eq!(
        mic.cached_class_id
            .load(std::sync::atomic::Ordering::Acquire),
        7
    );
    assert_eq!(
        mic.cached_class_name.lock().as_deref(),
        Some("com/example/MyClass")
    );
    assert_eq!(mic.cached_entry(), (0xDEAD_BEE0, true));
    assert_eq!(
        mic.cached_entry_word
            .load(std::sync::atomic::Ordering::Acquire),
        0xDEAD_BEE0 | JIT_IC_NEEDS_CONTEXT_TAG
    );
}

/// An address with bit 0 set cannot be encoded next to the context tag, so
/// it is refused rather than published as a different address.
#[test]
pub(in crate::tests) fn s33_mic_slot_refuses_an_entry_with_the_tag_bit_set() {
    let mic = JitMICSlot::new();
    mic.update(7, "com/example/MyClass", 0xDEAD_BEEF, false, false);
    assert_eq!(mic.cached_entry(), (0, false));
}

#[test]
pub(in crate::tests) fn s33_mic_slot_clear_compiled_entry_keeps_receiver_cache() {
    let mic = JitMICSlot::new();
    mic.update(7, "com/example/MyClass", 0xDEAD_BEE0, true, false);
    mic.clear_compiled_entry();

    assert_eq!(
        mic.cached_class_id
            .load(std::sync::atomic::Ordering::Acquire),
        7
    );
    assert_eq!(
        mic.cached_class_name.lock().as_deref(),
        Some("com/example/MyClass")
    );
    assert_eq!(mic.cached_entry(), (0, false));
}

/// r11-rt-mic-hit-miss-counters-share-the-inline-cache-line: the dispatch
/// helper's per-call records stop writing the slot's cache line once the
/// numbers anything reads are settled.
#[test]
pub(in crate::tests) fn r11_rt_mic_dispatch_counters_saturate() {
    use std::sync::atomic::Ordering;
    let mic = JitMICSlot::new();
    for _ in 0..10 {
        mic.note_dispatch_miss();
    }
    assert_eq!(mic.misses.load(Ordering::Relaxed), MIC_TO_PIC_THRESHOLD + 1);
    for _ in 0..5000 {
        mic.note_dispatch_hit();
    }
    assert_eq!(mic.hits.load(Ordering::Relaxed), 4096);
    // `dominant_receiver_at_bci`'s verdict is unchanged by the saturation.
    let mut cm = CompiledMethod::new(ExecutableBuffer::new(64).unwrap());
    let slot = Box::new(JitMICSlot::new_at(3));
    slot.cached_class_id.store(9, Ordering::Relaxed);
    slot.cached_entry_word.store(0x4000, Ordering::Relaxed);
    for _ in 0..MIC_TO_PIC_THRESHOLD {
        slot.note_dispatch_miss();
    }
    cm._jit_mic_slots.push(slot);
    assert_eq!(cm.dominant_receiver_at_bci(3), Some(9));
    cm._jit_mic_slots[0].note_dispatch_miss();
    cm._jit_mic_slots[0].note_dispatch_miss();
    assert_eq!(cm.dominant_receiver_at_bci(3), None);
}

#[test]
pub(in crate::tests) fn s33_mic_slot_hit_miss_counters() {
    let mic = JitMICSlot::new();
    for _ in 0..10 {
        mic.record_hit();
    }
    for _ in 0..5 {
        mic.record_miss();
    }
    assert_eq!(mic.hits.load(std::sync::atomic::Ordering::Relaxed), 10);
    assert_eq!(mic.misses.load(std::sync::atomic::Ordering::Relaxed), 5);
    assert_eq!(mic.total_observations(), 15);
    // hit_rate = 10/15 * 100 = 66
    assert_eq!(mic.hit_rate_pct(), 66);
}

#[test]
pub(in crate::tests) fn s33_mic_slot_is_monomorphic() {
    let mic = JitMICSlot::new();
    // Not enough observations
    for _ in 0..5 {
        mic.record_hit();
    }
    assert!(!mic.is_monomorphic());
    // 10 hits, 0 misses → 100% hit rate, ≥10 obs → monomorphic
    for _ in 0..5 {
        mic.record_hit();
    }
    assert!(mic.is_monomorphic());
}

#[test]
pub(in crate::tests) fn s33_mic_slot_mostly_hits_is_monomorphic() {
    let mic = JitMICSlot::new();
    for _ in 0..18 {
        mic.record_hit();
    }
    for _ in 0..2 {
        mic.record_miss();
    }
    assert!(mic.is_monomorphic());
}

/// A raw inline MIC has no helper boundary between its guard load and
/// indirect CALL, so retargeting a populated slot risks pairing one
/// receiver's class guard with another receiver's entry. `update` is
/// therefore monomorphic for the slot's lifetime: the first class to
/// install wins, and a later `update` for a DIFFERENT class is a no-op
/// (that receiver simply takes the ordinary helper path instead).
#[test]
pub(in crate::tests) fn s33_mic_slot_update_first_install_wins_no_retarget() {
    let mic = JitMICSlot::new();
    mic.update(1, "A", 100, false, false);
    mic.update(2, "B", 200, true, false);
    assert_eq!(
        mic.cached_class_id
            .load(std::sync::atomic::Ordering::Acquire),
        1
    );
    assert_eq!(mic.cached_class_name.lock().as_deref(), Some("A"));
    assert_eq!(mic.cached_entry(), (100, false));
}

/// A re-`update` for the SAME class as already installed must stay a
/// no-op (idempotent), not tear the entry/name/context triple.
#[test]
pub(in crate::tests) fn s33_mic_slot_update_same_class_is_idempotent() {
    let mic = JitMICSlot::new();
    mic.update(1, "A", 100, false, false);
    mic.update(1, "A", 998, true, false);
    assert_eq!(mic.cached_entry(), (100, false));
}

/// `prepopulate` seeds only `cached_class_id` (a profile-derived guard
/// hint) with `cached_entry_word` still 0. The first `update` for that
/// SAME class id must still publish a real entry — this is the shape
/// `mark_current_compile_method_recursive_cycle`'s caller
/// (profile-guided MIC seeding, see `try_compile_inner`) relies on.
#[test]
pub(in crate::tests) fn s33_mic_slot_update_resolves_prepopulated_hint() {
    let mic = JitMICSlot::new();
    mic.prepopulate(7);
    assert_eq!(mic.cached_entry(), (0, false));
    mic.update(7, "Hinted", 0x4242, true, false);
    assert_eq!(
        mic.cached_class_id
            .load(std::sync::atomic::Ordering::Acquire),
        7
    );
    assert_eq!(mic.cached_entry(), (0x4242, true));
    assert_eq!(mic.cached_class_name.lock().as_deref(), Some("Hinted"));
}

#[test]
pub(in crate::tests) fn s33_mic_slot_concurrent_updates() {
    use std::sync::Arc;
    let mic = Arc::new(JitMICSlot::new());
    let mut handles = Vec::new();
    for i in 0..8 {
        let mic_clone = mic.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..100 {
                if i % 2 == 0 {
                    mic_clone.record_hit();
                } else {
                    mic_clone.record_miss();
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    // 4 threads * 100 hits + 4 threads * 100 misses = 800
    assert_eq!(mic.total_observations(), 800);
    assert_eq!(mic.hits.load(std::sync::atomic::Ordering::Relaxed), 400);
    assert_eq!(mic.misses.load(std::sync::atomic::Ordering::Relaxed), 400);
}

#[test]
pub(in crate::tests) fn s33_mic_slot_hit_rate_boundary() {
    // Exactly 90% hit rate should count as monomorphic
    let mic = JitMICSlot::new();
    for _ in 0..9 {
        mic.record_hit();
    }
    for _ in 0..1 {
        mic.record_miss();
    }
    // 10 total, 90% hits → monomorphic
    assert!(mic.is_monomorphic());
}

#[test]
pub(in crate::tests) fn s33_mic_slot_entry_ptr_zero_means_unresolved() {
    let mic = JitMICSlot::new();
    mic.prepopulate(5);
    // Even with class_id populated, a zero word means no direct dispatch
    assert_eq!(
        mic.cached_entry_word
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    // After update with non-zero entry, it's resolved
    mic.update(5, "Foo", 0x1234, false, false);
    assert_ne!(
        mic.cached_entry_word
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}


/// Build a minimal bytecode header that ends in `ireturn` so `jit_scan`
/// sees a valid terminator after the probe opcodes.
pub(in crate::tests) fn ireturn_tail(mut prefix: Vec<u8>) -> Vec<u8> {
    // Push a zero int so ireturn has an operand.
    prefix.push(0x03); // iconst_0
    prefix.push(0xac); // ireturn
    prefix
}

pub(in crate::tests) fn lreturn_tail(mut prefix: Vec<u8>) -> Vec<u8> {
    prefix.push(0x09); // lconst_0
    prefix.push(0xad); // lreturn
    prefix
}

pub(in crate::tests) fn freturn_tail(mut prefix: Vec<u8>) -> Vec<u8> {
    prefix.push(0x0b); // fconst_0
    prefix.push(0xae); // freturn
    prefix
}

pub(in crate::tests) fn dreturn_tail(mut prefix: Vec<u8>) -> Vec<u8> {
    prefix.push(0x0e); // dconst_0
    prefix.push(0xaf); // dreturn
    prefix
}

/// RG.1 — A method containing `invokedynamic` (0xba) no longer vetoes
/// JIT compilation at the scan stage.
///
/// Previously ANY invokedynamic anywhere in a method's bytecode —
/// reachable or not — permanently blacklisted the WHOLE method from JIT
/// compilation. The overwhelmingly common source of a "surprise"
/// invokedynamic in otherwise-ordinary hot methods is
/// `assert cond : "msg" + var;` (javac lowers the message concat via
/// `StringConcatFactory`, guarded by the `assertionsDisabled` dead
/// branch), so this blanket veto forced hot per-call-site methods that
/// merely CONTAIN a dead assert into the interpreter forever (see
/// `...binary-docvalues-range-hang...md` for the concrete
/// Lucene `FSTCompiler`/`NodeHash` repro).
///
/// The new design: the scanner accepts 0xba and simply records the site
/// (`indy_ops`); the codegen (which DOES have CP access, unlike this
/// scan) lowers the instruction to an UNCONDITIONAL jump to the existing
/// uncommon-trap deopt stub (`DeoptReason::UnreachedCode`). If this exact
/// program point is ever actually reached at runtime (assertions
/// enabled, or a genuinely live indy), the method permanently reverts to
/// interpreter-only execution for the rest of the process — i.e. today's
/// status quo for that one method — so the interpreter's real
/// invokedynamic dispatch (makeConcat, lambda bootstraps) still runs
/// whenever the instruction is genuinely exercised. In the common case
/// (assertions disabled, dead branch) the trap is never taken and the
/// surrounding hot method compiles and runs at full JIT speed.
#[test]
pub(in crate::tests) fn rg1_jit_accepts_invokedynamic_at_scan_stage() {
    let code = ireturn_tail(vec![
        0xba, 0x00, 0x01, 0x00, 0x00, // invokedynamic #1, 0, 0
    ]);
    assert!(
        is_jit_compatible(&code, code.len(), "()I"),
        "the scanner must no longer bail on invokedynamic — it defers to \
         an unconditional uncommon-trap deopt in the codegen instead"
    );
    // The scan also records the site so the codegen can resolve its
    // descriptor and lower it to the deopt stub.
    let scan = x64::jit_scan(&code, code.len(), "()I").expect("scan must succeed");
    assert_eq!(
        scan.indy_ops,
        vec![(0, 1u16)],
        "invokedynamic site (pc, cp_index) must be recorded for codegen resolution"
    );
}

/// RG.2 — JIT accepts fcmpl/fcmpg/dcmpl/dcmpg opcodes. The scanner groups
/// them with lcmp in the 0x94..=0x98 range. NaN canonicalization itself is
/// verified in the compiler — here we only pin the scanner shape.
#[test]
pub(in crate::tests) fn rg2_jit_accepts_fp_compare_opcodes() {
    // Float: fconst_0 (0x0b), fconst_1 (0x0c), fcmpl (0x95), ireturn
    let fcmpl = vec![0x0b, 0x0c, 0x95, 0x03, 0xac];
    assert!(
        is_jit_compatible(&fcmpl, fcmpl.len(), "()I"),
        "fcmpl must be JIT-compatible"
    );

    // fconst_0, fconst_1, fcmpg
    let fcmpg = vec![0x0b, 0x0c, 0x96, 0x03, 0xac];
    assert!(
        is_jit_compatible(&fcmpg, fcmpg.len(), "()I"),
        "fcmpg must be JIT-compatible"
    );

    // dconst_0 (0x0e), dconst_1 (0x0f), dcmpl (0x97)
    let dcmpl = vec![0x0e, 0x0f, 0x97, 0x03, 0xac];
    assert!(
        is_jit_compatible(&dcmpl, dcmpl.len(), "()I"),
        "dcmpl must be JIT-compatible"
    );

    // dconst_0, dconst_1, dcmpg (0x98)
    let dcmpg = vec![0x0e, 0x0f, 0x98, 0x03, 0xac];
    assert!(
        is_jit_compatible(&dcmpg, dcmpg.len(), "()I"),
        "dcmpg must be JIT-compatible"
    );
}

/// RG.3 — JIT accepts lcmp (0x94).
#[test]
pub(in crate::tests) fn rg3_jit_accepts_lcmp() {
    // lconst_0 (0x09), lconst_1 (0x0a), lcmp (0x94), ireturn
    let lcmp = vec![0x09, 0x0a, 0x94, 0x03, 0xac];
    assert!(
        is_jit_compatible(&lcmp, lcmp.len(), "()I"),
        "lcmp must be JIT-compatible"
    );
}

/// RG.4 — JIT accepts tableswitch and lookupswitch and correctly skips
/// over their variable-length payloads during scanning.
#[test]
pub(in crate::tests) fn rg4_jit_accepts_switch_opcodes() {
    // tableswitch at pc=0 needs padding to 4-byte-aligned, so we prepend
    // iconst_0 (1 byte) + nop padding; but the scanner handles alignment
    // from the tableswitch opcode's own pc. Use a 1-byte prefix so pc=1,
    // and tableswitch at pc=1 needs 3 bytes of padding.
    //
    // Layout: [iconst_0=0x03] [tableswitch=0xaa] [pad0 pad0 pad0]
    //         [default=0,0,0,8] [low=0,0,0,0] [high=0,0,0,0]
    //         [offset0=0,0,0,8] [ireturn=0xac]
    //
    // default/offset0 are 32-bit signed offsets from the tableswitch pc.
    let mut ts = vec![0x03, 0xaa, 0, 0]; // iconst_0, tableswitch, 2 pad bytes (1..4 boundary)
    ts.extend_from_slice(&[0, 0, 0, 8]); // default offset = +8 (ireturn)
    ts.extend_from_slice(&[0, 0, 0, 0]); // low = 0
    ts.extend_from_slice(&[0, 0, 0, 0]); // high = 0 (1 entry)
    ts.extend_from_slice(&[0, 0, 0, 8]); // offset[0] = +8
    ts.push(0x03); // iconst_0
    ts.push(0xac); // ireturn
    assert!(
        is_jit_compatible(&ts, ts.len(), "()I"),
        "tableswitch must be JIT-compatible"
    );

    // lookupswitch: [iconst_0] [lookupswitch=0xab] [pad] [default=+14]
    //               [npairs=1] [match=0, offset=+14] [iconst_0] [ireturn]
    let mut ls = vec![0x03, 0xab, 0, 0]; // iconst_0, lookupswitch, 2 pad bytes
    ls.extend_from_slice(&[0, 0, 0, 16]); // default = +16
    ls.extend_from_slice(&[0, 0, 0, 1]); // npairs = 1
    ls.extend_from_slice(&[0, 0, 0, 0]); // match = 0
    ls.extend_from_slice(&[0, 0, 0, 16]); // offset = +16
    ls.push(0x03);
    ls.push(0xac);
    assert!(
        is_jit_compatible(&ls, ls.len(), "()I"),
        "lookupswitch must be JIT-compatible"
    );
}

/// RG.5 (updated by RBC.6) — the scanner now ACCEPTS explicit `athrow`
/// and records `has_athrow`; the compile gates restrict compilation to
/// methods with NO local exception handlers (the codegen lowers athrow
/// to "stash pending exception + return the i64::MIN deopt sentinel",
/// which cannot dispatch to an in-method handler — see
/// `try_compile_inner`'s `exception_table.is_empty()` gate and the OSR
/// trigger's decline in `vm/src/runtime/interpreter.rs::try_osr`).
#[test]
pub(in crate::tests) fn rg5_jit_scan_accepts_athrow_and_flags_it() {
    // aconst_null (0x01), athrow (0xbf), then iconst_0/ireturn filler.
    let code = vec![0x01, 0xbf, 0x03, 0xac];
    let scan =
        x64::jit_scan(&code, code.len(), "()I").expect("athrow method must pass jit_scan (RBC.6)");
    assert!(
        scan.has_athrow,
        "jit_scan must record has_athrow so compile gates can apply"
    );
    // A method without athrow must NOT set the flag.
    let plain = vec![0x03, 0xac];
    let scan =
        x64::jit_scan(&plain, plain.len(), "()I").expect("trivial method must pass jit_scan");
    assert!(!scan.has_athrow);
}

/// RG.6 — JIT scanner accepts `monitorenter` (0xc2) and `monitorexit`
/// (0xc3) so synchronized blocks are JIT-eligible. The compiler then
/// either elides the exact lock site (escape analysis proves its receiver
/// thread-local) or lowers it through the direct thin-lock runtime stub.
#[test]
pub(in crate::tests) fn rg6_jit_accepts_monitor_enter_exit() {
    // aconst_null, dup (0x59), monitorenter, monitorexit, pop (0x57), ireturn
    let code = vec![0x01, 0x59, 0xc2, 0xc3, 0x57, 0x03, 0xac];
    assert!(
        is_jit_compatible(&code, code.len(), "()I"),
        "monitorenter/exit must be accepted so synchronized methods can JIT"
    );
}

// A handler that falls THROUGH to a loop back edge reads every local that
// edge reads. `run_jit_callee_handler` asks exactly this question (through
// the public [`handler_resume_requires_precise_locals`]) before it is
// rebuild a compiled callee's handler frame from the callee's incoming
// arguments alone, so a `false` here is a null local at runtime.
//
// The shape is Spring Boot's
// `BindConverter.convert(Object, TypeDescriptor, TypeDescriptor)`:
// `for (ConversionService d : this.delegates) { try { … } catch (…) { … } }`,
// whose catch block ends by continuing the loop. Local 2 here is the
// enhanced-for's synthetic iterator — assigned before the try, read by the
// back edge, and reachable from the handler only through its trailing
// `goto`.
// Calls `precise_exception_frame_sites_supported`, whose admitted-opcode
// set is the x64 backend's — its siblings below carry the same gate.
#[cfg(target_arch = "x86_64")]
#[test]
pub(in crate::tests) fn handler_falling_through_to_a_loop_back_edge_reads_the_iterator_local() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    let code = vec![
        0x03, // 0:  iconst_0
        0x3c, // 1:  istore_1              n = 0
        0x2a, // 2:  aload_0               the List parameter
        0xb9, 0x00, 0x01, 0x01, 0x00, // 3:  invokeinterface List.iterator
        0x4d, // 8:  astore_2              the synthetic iterator
        0x2c, // 9:  aload_2               <- loop head
        0xb9, 0x00, 0x02, 0x01, 0x00, // 10: invokeinterface Iterator.hasNext
        0x99, 0x00, 0x11, // 15: ifeq -> 32
        0x2c, // 18: aload_2               <- protected range starts
        0xb9, 0x00, 0x03, 0x01, 0x00, // 19: invokeinterface Iterator.next
        0x57, // 24: pop
        0xa7, 0xff, 0xf0, // 25: goto -> 9 (back edge)
        0x4e, // 28: astore_3              <- handler: catch (…) e
        0xa7, 0xff, 0xec, // 29: goto -> 9 (handler CONTINUES the loop)
        0x1b, // 32: iload_1
        0xac, // 33: ireturn
    ];
    let table = vec![ExceptionTableEntry {
        start_pc: 18,
        end_pc: 28,
        handler_pc: 28,
        catch_type: 0,
    }];
    assert!(
        handler_resume_requires_precise_locals(
            &code,
            code.len(),
            &table,
            "(Ljava/util/List;)I",
            true
        ),
        "the handler's trailing goto reaches `aload_2`, the iterator local"
    );
    // And it is a shape the precise-frame relaxation admits — which is why
    // it COMPILES instead of staying interpreted, and therefore why the
    // reconstruction question arises at all.
    assert!(precise_exception_frame_sites_supported(
        &code,
        code.len(),
        &table
    ));
}

/// Negative control for the test above: the same loop, but the handler
/// RETURNS. Nothing it reaches reads a non-parameter local, so the
/// params-only reconstruction is sound and must not be refused — the
/// `iload_1` at pc 32 is past the handler's own `ireturn` and must not be
/// scanned into (an earlier raw-pc-order version of this dataflow did
/// exactly that and over-rejected).
#[test]
pub(in crate::tests) fn handler_that_returns_does_not_read_a_non_param_local() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    let code = vec![
        0x03, // 0:  iconst_0
        0x3c, // 1:  istore_1
        0x2a, // 2:  aload_0
        0xb9, 0x00, 0x01, 0x01, 0x00, // 3:  invokeinterface List.iterator
        0x4d, // 8:  astore_2
        0x2c, // 9:  aload_2
        0xb9, 0x00, 0x02, 0x01, 0x00, // 10: invokeinterface Iterator.hasNext
        0x99, 0x00, 0x11, // 15: ifeq -> 32
        0x2c, // 18: aload_2
        0xb9, 0x00, 0x03, 0x01, 0x00, // 19: invokeinterface Iterator.next
        0x57, // 24: pop
        0xa7, 0xff, 0xf0, // 25: goto -> 9
        0x4e, // 28: astore_3           <- handler
        0x03, // 29: iconst_0
        0xac, // 30: ireturn
        0x00, // 31: nop (padding so 32 is the ifeq target)
        0x1b, // 32: iload_1
        0xac, // 33: ireturn
    ];
    let table = vec![ExceptionTableEntry {
        start_pc: 18,
        end_pc: 28,
        handler_pc: 28,
        catch_type: 0,
    }];
    assert!(!handler_resume_requires_precise_locals(
        &code,
        code.len(),
        &table,
        "(Ljava/util/List;)I",
        true
    ));
}

/// gcd d10/f (2026-09-28): the verdict the callee-deopt service's handler
/// arm asks (`run_jit_callee_handler`'s fail-closed refusal, through the
/// public [`handler_resume_requires_precise_locals`]) for a handler that reads
/// a PARAMETER the method reassigned in its protected range. The params-only
/// rebuild would hand that parameter its ENTRY value, so the answer must be
/// `true` (precise frame, or decline) -- as it has been since JIT round 11
/// (`regalloc::handler_has_unsafe_local_read` drops every written slot from
/// the initial safe set). Pinned here at the public entry, on the exact
/// `javac` 25 bytecode of `tools/bench/Gcd1ArgPinReassignedParamHandlerProbe`'s
/// `f(int x, String s)`, because the suspicion
/// (`docs/feature-designs/jit-ir-optimizations.md`)
/// was raised by reading only the public wrapper's initial mask.
#[test]
pub(in crate::tests) fn a_handler_reading_a_reassigned_parameter_needs_precise_locals() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    // static int f(int x, String s) {
    //     try { x = x * 2 + 1; return Integer.parseInt(s); }
    //     catch (NumberFormatException e) { return x; }
    // }
    let code = vec![
        0x1a, // 0:  iload_0
        0x05, // 1:  iconst_2
        0x68, // 2:  imul
        0x04, // 3:  iconst_1
        0x60, // 4:  iadd
        0x3b, // 5:  istore_0            x reassigned inside the try
        0x2b, // 6:  aload_1
        0xb8, 0x00, 0x07, // 7:  invokestatic Integer.parseInt
        0xac, // 10: ireturn
        0x4d, // 11: astore_2            <- handler
        0x1a, // 12: iload_0             reads x
        0xac, // 13: ireturn
    ];
    let table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 10,
        handler_pc: 11,
        catch_type: 1,
    }];
    let desc = "(ILjava/lang/String;)I";
    assert!(
        handler_resume_requires_precise_locals(&code, code.len(), &table, desc, true),
        "the handler reads x, which the try body reassigned"
    );

    // The same reassignment through `iinc` (x += 7), padded to the same pcs.
    let mut iinc = code.clone();
    iinc[0..6].copy_from_slice(&[0x84, 0x00, 0x07, 0x00, 0x00, 0x00]); // iinc 0 7; nop x3
    assert!(
        handler_resume_requires_precise_locals(&iinc, iinc.len(), &table, desc, true),
        "iinc writes the parameter too"
    );

    // Control: the try body computes the same value but never stores it, so
    // the entry value IS the value the handler must see.
    let mut untouched = code.clone();
    untouched[5] = 0x57; // pop
    assert!(
        !handler_resume_requires_precise_locals(&untouched, untouched.len(), &table, desc, true),
        "an unwritten parameter is safe on the params-only rebuild"
    );
}

#[cfg(target_arch = "x86_64")]
#[test]
pub(in crate::tests) fn synchronized_cleanup_has_precise_exception_site_coverage() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    // javac-style synchronized cleanup: save the lock in local 1, execute
    // a non-throwing body, release it, and use a catch-all handler to
    // release + rethrow. The handler reads the non-parameter lock local,
    // while every potentially throwing protected instruction is a monitor
    // runtime call that publishes a reason-9 snapshot.
    let code = vec![
        0x2a, // 0: aload_0
        0x59, // 1: dup
        0x4c, // 2: astore_1
        0xc2, // 3: monitorenter
        0x03, // 4: iconst_0
        0x3d, // 5: istore_2
        0x84, 0x02, 0x01, // 6: iinc 2, 1
        0x2b, // 9: aload_1
        0xc3, // 10: monitorexit
        0xb1, // 11: return
        0x4e, // 12: astore_3
        0x2b, // 13: aload_1
        0xc3, // 14: monitorexit
        0x2d, // 15: aload_3
        0xbf, // 16: athrow
    ];
    let table = vec![
        ExceptionTableEntry {
            start_pc: 4,
            end_pc: 11,
            handler_pc: 12,
            catch_type: 0,
        },
        ExceptionTableEntry {
            start_pc: 12,
            end_pc: 15,
            handler_pc: 12,
            catch_type: 0,
        },
    ];
    assert!(local_handler_reads_unsafe_local(
        &code,
        code.len(),
        &table,
        "(Ljava/lang/Object;)V",
        true,
    ));
    assert!(precise_exception_frame_sites_supported(
        &code,
        code.len(),
        &table,
    ));
}

#[cfg(target_arch = "x86_64")]
#[test]
pub(in crate::tests) fn protected_virtual_and_interface_calls_are_precise_exception_covered() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    // 0xb6/0xb9 ARE admitted (2026-07-31). They were admitted once before
    // (2026-07-27) and removed again by `a523715a84`, because admitting
    // them let Spring's `SimpleApplicationEventMulticaster.invokeListener`
    // compile and then read its own pre-`try` `errorHandler` local back as
    // null inside the handler
    // (springboot-rerun-20260728-small-residuals-cluster, Case 3).
    //
    // What makes them safe now is that `a523715a84` did not only narrow
    // this list — it also ADDED the `protected_precise_handler_call`
    // suppression to `x64.rs`, which forces a protected virtual/interface
    // call onto the dispatch path (ending in
    // `emit_post_invoke_exception_check`, which records the caller's
    // complete state) instead of the inline MIC/PIC cascade that CALLs a
    // raw entry with nothing recording the caller's locals. That was the
    // actual defect; the narrowing was belt to its braces.
    //
    // The acceptance test for the braces holding on their own is
    // `probes/HandlerLocalAcrossProtectedInvokeProbe.java`, which builds
    // the Spring shape — a non-parameter local assigned before the `try`,
    // read back in the handler — through `invokevirtual`,
    // `invokeinterface` and `invokespecial` throw sites, plus a nested
    // `try`, a reassignment after the protected call, and the
    // null-`errorHandler` path that must skip the protected region
    // entirely. Re-verify with THAT probe, not by inspection, before
    // touching this list again.
    let code = vec![
        0x2a, // 0: aload_0
        0xb6, 0x00, 0x01, // 1: invokevirtual #1
        0x57, // 4: pop
        0x2a, // 5: aload_0
        0xb9, 0x00, 0x02, 0x01, 0x00, // 6: invokeinterface #2, count=1
        0x57, // 11: pop
        0xb1, // 12: return
        0x4c, // 13: astore_1
        0x2b, // 14: aload_1
        0xbf, // 15: athrow
    ];
    let table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 13,
        handler_pc: 13,
        catch_type: 0,
    }];
    assert!(precise_exception_frame_sites_supported(
        &code,
        code.len(),
        &table,
    ));

    // One un-admitted throwing opcode anywhere in the same protected
    // range is still enough to withhold coverage for the whole method —
    // this list is a conjunction, not a majority vote.
    //
    // The witness was `aaload` (0x32), "its inline bounds/null check bails
    // to the shared sentinel stub, which records no frame". That stopped
    // being true when the bounds check and the array null check grew
    // publishing exits (deopt-stub reasons 11 and 10) and array loads were
    // admitted, so the witness moved to `aastore` (0x53), un-admitted for its
    // void ZGC-barrier fallback arm. Round 11 wave 8 made that arm refuse the
    // compile when protected and admitted `aastore` (and the integer
    // divides), so every candidate opcode is now admitted: `aastore` in the
    // same range must NOT withhold coverage any more. The conjunction is still
    // exercised: a stream cut inside the protected invokevirtual cannot be
    // walked, and one unwalkable site withholds coverage for the method.
    let mut with_aastore = code.clone();
    with_aastore.splice(1..1, [0x53]);
    let aastore_table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 14,
        handler_pc: 14,
        catch_type: 0,
    }];
    assert!(precise_exception_frame_sites_supported(
        &with_aastore,
        with_aastore.len(),
        &aastore_table,
    ));
    assert!(!precise_exception_frame_sites_supported(&code, 3, &table));
}

/// `instanceof` inside a protected range no longer withholds coverage
/// (2026-08-05). It was never a throwing opcode in this backend: the 0xc1
/// arm emits one call to `jit_instanceof`, which returns 0 or 1 on every
/// path and never stashes a pending exception, and the codegen emits no
/// post-call exception check after it because there is nothing to check.
///
/// This is a static-admission test. The behavioural acceptance test is
/// `JitPreciseHandlerFrame.instanceofStep`
/// (`vm/tests/jit_local_exception_handler_tests.rs`), whose handler reads a
/// non-parameter local and whose protected range holds an `instanceof`
/// alongside a throwing `invokeinterface` — run THAT before touching the
/// list again.
#[cfg(target_arch = "x86_64")]
#[test]
pub(in crate::tests) fn protected_instanceof_is_precise_exception_covered() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    // `JitPreciseHandlerFrame.instanceofStep`'s shape, reduced: the range
    // holds an `instanceof` and an `invokeinterface`, the handler follows.
    let code = vec![
        0x2b, // 0: aload_1
        0xc1, 0x00, 0x01, // 1: instanceof #1
        0x57, // 4: pop
        0x2b, // 5: aload_1
        0xb9, 0x00, 0x02, 0x01, 0x00, // 6: invokeinterface #2, count=1
        0x57, // 11: pop
        0xb1, // 12: return
        0x4c, // 13: astore_1
        0xb1, // 14: return
    ];
    let table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 13,
        handler_pc: 13,
        catch_type: 0,
    }];
    assert!(
        precise_exception_frame_sites_supported(&code, code.len(), &table),
        "an `instanceof` in a protected range must not withhold coverage"
    );
    assert_eq!(
        first_unsupported_precise_frame_site(&code, code.len(), &table),
        None
    );

    // The contrast case needs an opcode that genuinely still withholds.
    //
    // It used to be `checkcast` (0xc0), which was a valid foil until
    // 2026-08-11: `checkcast` throws, but its ONE lowering always follows
    // `helpers.checkcast` with `emit_post_invoke_exception_check`, so it
    // was publishing all along and is now admitted
    // (`precise_getstatic_checkcast_enabled`). Using it here would assert
    // the opposite of what the backend does.
    //
    // `arraylength` (0xbe) filled that role until round 9 (2026-09-18), when
    // its in-place null check started publishing (`emit_precise_array_npe_
    // check`) and it was admitted. `ldc` (0x12) filled it until 2026-09-19,
    // when its two emitters were found to end in `emit_post_alloc_oom_check`
    // -- which records a reason-9 frame at a protected bci -- and it was
    // admitted under `precise_alloc_athrow_enabled`. The integer divide
    // (`idiv`, 0x6c) was the foil until round 11 wave 8, when its zero guard
    // started publishing through reason 12. No withholding opcode is left, so
    // the splice below now checks that a protected idiv is ADMITTED; the
    // truncation check in `rbc6_admits_a_protected_throw_new_with_a_message_constant`
    // keeps the scanner's refusal path covered.
    let mut with_idiv = code.clone();
    with_idiv.splice(1..1, [0x6c]);
    let idiv_table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 14,
        handler_pc: 14,
        catch_type: 0,
    }];
    assert_eq!(
        first_unsupported_precise_frame_site(&with_idiv, with_idiv.len(), &idiv_table,),
        None,
        "a protected idiv publishes through reason 12 and must not withhold coverage"
    );

    // The opcode that left the list on 2026-09-19, in the same shape.
    let mut with_ldc = code.clone();
    with_ldc.splice(1..1, [0x12, 0x01]);
    let ldc_table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 15,
        handler_pc: 15,
        catch_type: 0,
    }];
    assert_eq!(
        first_unsupported_precise_frame_site(&with_ldc, with_ldc.len(), &ldc_table,),
        None,
        "a protected ldc publishes through emit_post_alloc_oom_check and must not withhold coverage"
    );

    // And the opcode that changed sides must now be clear, in the same
    // shape the old assertion used — so this test fails if the admission
    // is ever reverted without revisiting the argument for it.
    let mut with_checkcast = code.clone();
    with_checkcast.splice(1..1, [0xc0, 0x00, 0x03]);
    let cc_table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 16,
        handler_pc: 16,
        catch_type: 0,
    }];
    assert_eq!(
        first_unsupported_precise_frame_site(&with_checkcast, with_checkcast.len(), &cc_table,),
        None,
        "checkcast publishes via emit_post_invoke_exception_check and must be admitted"
    );
}

/// `getfield`/`putfield` inside a protected range no longer withhold
/// coverage (2026-08-02). The exclusion outlived its cause: both top-level
/// field arms grew a precise null trap after it was written — see
/// `precise_frame_publishing_opcode`'s own doc for which commit did which.
///
/// This is a static-admission test. The behavioural acceptance tests are
/// `probes/Rbc6FieldProbe.java` (a handler reading a non-parameter local
/// after a field NPE) and `probes/SyncBlockFieldProbe.java` (javac's
/// `synchronized` cleanup handler releasing the monitor on the way out) —
/// re-run THOSE, not this, before touching the list again.
#[cfg(target_arch = "x86_64")]
#[test]
pub(in crate::tests) fn protected_field_access_is_precise_exception_covered() {
    use cratonvm_reader::attribute::ExceptionTableEntry;

    let code = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1
        0x57, // 4: pop
        0x2a, // 5: aload_0
        0x03, // 6: iconst_0
        0xb5, 0x00, 0x02, // 7: putfield #2
        0xb1, // 10: return
        0x4c, // 11: astore_1
        0x2b, // 12: aload_1
        0xbf, // 13: athrow
    ];
    let table = vec![ExceptionTableEntry {
        start_pc: 0,
        end_pc: 11,
        handler_pc: 11,
        catch_type: 0,
    }];
    assert!(precise_exception_frame_sites_supported(
        &code,
        code.len(),
        &table,
    ));
}

/// RG.7 — `System.arraycopy` is routed through the native registry as an
/// interpreter-level intrinsic. We verify the invokestatic dispatch path
/// is scanner-compatible; the actual copy is performed by the native.
#[test]
pub(in crate::tests) fn rg7_jit_accepts_arraycopy_invoke_site() {
    // aconst_null, iconst_0, aconst_null, iconst_0, iconst_0,
    // invokestatic #1 (placeholder CP index — scanner only checks opcode shape),
    // ireturn
    let code = vec![0x01, 0x03, 0x01, 0x03, 0x03, 0xb8, 0x00, 0x01, 0x03, 0xac];
    assert!(
        is_jit_compatible(&code, code.len(), "()I"),
        "invokestatic for System.arraycopy must scan OK"
    );
}

/// RG.8 — A method with a loop (back-edge via goto backward) must still
/// scan as JIT-compatible; the compiler emits an oop-map safepoint at
/// each back-edge when lowering to machine code.
#[test]
pub(in crate::tests) fn rg8_jit_accepts_loop_with_backedge() {
    // iconst_0, istore_1 (0x3c)              ; i = 0
    // iload_1 (0x1b), iconst_5 (0x08), if_icmpge (0xa2) +10 (forward to ireturn)
    // iinc local=1 by 1 (0x84)                ; i++
    // goto -8 (0xa7) backward to iload_1     ; back-edge
    // iconst_0, ireturn
    // Offsets are relative to the branch opcode's pc.
    let code = vec![
        0x03, 0x3c, // iconst_0, istore_1
        0x1b, 0x08, 0xa2, 0x00, 0x0a, // iload_1, iconst_5, if_icmpge +10
        0x84, 0x01, 0x01, // iinc 1, 1
        0xa7, 0xff, 0xf8, // goto -8
        0x03, 0xac, // iconst_0, ireturn
    ];
    assert!(
        is_jit_compatible(&code, code.len(), "()I"),
        "loop with back-edge must be JIT-compatible (safepoint injected by compiler)"
    );
}

/// RG.9 — `tiered.rs` carries the tier-up manager; verify the module
/// exists and exposes the expected hot-counter threshold so the
/// interpreter-to-JIT transition (OSR entry) is active at runtime.
#[test]
pub(in crate::tests) fn rg9_tiered_manager_exists() {
    // Spot-check: module is reachable from the jit crate root and the
    // default policy produces a usable manager.
    let _manager = crate::tiered::TieredCompilationManager::with_default_policy();
}

/// RG.10 — The interpreter uses typed value-stack slots with automatic
/// reference-int coercion. We cannot touch the putfield/putstatic blocks
/// per project constraints, so this test pins the reader's ability to
/// decode those opcodes — any regression at the reader layer would show
/// here first.
#[test]
pub(in crate::tests) fn rg10_reader_decodes_putfield_putstatic() {
    use cratonvm_reader::instruction::Instruction;
    // putfield #1
    let (inst, next) = Instruction::decode(&[0xb5, 0x00, 0x01], 0).unwrap();
    assert_eq!(next, 3);
    assert!(matches!(inst, Instruction::Putfield(1)));
    // putstatic #2
    let (inst, next) = Instruction::decode(&[0xb3, 0x00, 0x02], 0).unwrap();
    assert_eq!(next, 3);
    assert!(matches!(inst, Instruction::Putstatic(2)));
}

/// RG.11 — The `wide` prefix (0xc4) extends the following index to u16.
/// Reader produces an `Iload(u16)` (or Lload/Fload/Dload/Aload/Istore/...
/// variants) with the widened index. Method with >255 locals must work.
#[test]
pub(in crate::tests) fn rg11_wide_prefix_decodes_u16_index() {
    use cratonvm_reader::instruction::Instruction;
    // wide iload 300 → 0xc4 0x15 0x01 0x2c
    let (inst, next) = Instruction::decode(&[0xc4, 0x15, 0x01, 0x2c], 0).unwrap();
    assert_eq!(next, 4);
    match inst {
        Instruction::Iload(idx) => assert_eq!(idx, 300),
        other => panic!("expected Iload(300), got {other:?}"),
    }
    // wide iinc index=500 const=2 → 0xc4 0x84 0x01 0xf4 0x00 0x02
    let (inst, next) = Instruction::decode(&[0xc4, 0x84, 0x01, 0xf4, 0x00, 0x02], 0).unwrap();
    assert_eq!(next, 6);
    match inst {
        Instruction::Iinc { index, constant } => {
            assert_eq!(index, 500);
            assert_eq!(constant, 2);
        }
        other => panic!("expected Iinc, got {other:?}"),
    }
}

/// RG.12 — The `invokedynamic` opcode (0xba) is decoded by the reader
/// with its 4-operand shape (cp index + 2 reserved zero bytes) and
/// routed to `Instruction::Invokedynamic(u16)`. The interpreter then
/// delegates to the real bootstrap (LambdaMetafactory, etc.) in
/// `vm/src/runtime/invokedynamic.rs`.
#[test]
pub(in crate::tests) fn rg12_reader_decodes_invokedynamic() {
    use cratonvm_reader::instruction::Instruction;
    // invokedynamic #42, 0, 0 → 0xba 0x00 0x2a 0x00 0x00
    let (inst, next) = Instruction::decode(&[0xba, 0x00, 0x2a, 0x00, 0x00], 0).unwrap();
    assert_eq!(next, 5);
    match inst {
        Instruction::Invokedynamic(idx) => assert_eq!(idx, 42),
        other => panic!("expected Invokedynamic(42), got {other:?}"),
    }
}

/// RG.3 extra — JIT accepts all long/double return types so lcmp/dcmp can
/// appear in methods returning any numeric primitive.
#[test]
pub(in crate::tests) fn rg3_jit_accepts_long_and_double_returns() {
    assert!(is_jit_compatible(&lreturn_tail(vec![]), 2, "()J"));
    assert!(is_jit_compatible(&freturn_tail(vec![]), 2, "()F"));
    assert!(is_jit_compatible(&dreturn_tail(vec![]), 2, "()D"));
}

/// Scan-result sanity: an empty-body `public void foo() {}` must scan.
#[test]
pub(in crate::tests) fn rg_scan_sanity_void_return() {
    let code = vec![0xb1]; // return (void)
    let r = jit_scan(&code, code.len(), "()V");
    assert!(r.is_some(), "void return must scan");
}

// ── JIT code-cache cap tests ────────────────────────────────────
//
// These avoid mutating `CRATONVM_JIT_CODE_CACHE_MAX_MB`: `jit_code_cache_cap_bytes`
// caches its value in a process-wide `OnceLock`, so an env-var test would be
// order-dependent and could poison the cache for the rest of the suite. We
// exercise the observable behaviour through `COMMITTED_JIT_CODE_BYTES` and the
// public accessors instead.

#[test]
pub(in crate::tests) fn code_cache_cap_default_is_nonzero_and_below_disable_sentinel() {
    let cap = jit_code_cache_cap_bytes();
    // Whatever the env says, a sane cap is either a real byte bound or the
    // explicit "disabled" sentinel — never an accidental 0 that would refuse
    // all compilation.
    assert!(cap > 0, "cap must never be zero");
    // The compiled-in default is a generous, finite bound.
    assert_eq!(DEFAULT_JIT_CODE_CACHE_CAP_BYTES, 1024 * 1024 * 1024);
}

#[test]
pub(in crate::tests) fn code_cache_not_at_capacity_when_committed_is_small() {
    // In the test process essentially no JIT code is committed, so with any
    // realistic cap (or the disabled sentinel) we must be under capacity and
    // therefore still willing to compile.
    let cap = jit_code_cache_cap_bytes();
    if cap == usize::MAX {
        // Cap disabled in this environment: at-capacity is unconditionally false.
        assert!(!jit_code_cache_at_capacity());
        return;
    }
    let committed = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    // Sanity: the test process hasn't committed a quarter-gig of code.
    assert!(committed < cap, "unexpectedly large committed code in test");
    assert!(!jit_code_cache_at_capacity());
}

#[test]
pub(in crate::tests) fn code_cache_committed_counter_tracks_buffer_allocation() {
    // A before/after DELTA is not a sound observable here: the counter is
    // process-global and every other test that drops an `ExecutableBuffer`
    // decrements it, so `after` can legitimately sit below `before + 4096`.
    // What does hold at every instant is that the ledger is exact — `new`
    // adds `capacity`, `Drop` subtracts it — so while our buffer is live
    // the total cannot be below our own contribution. Same reasoning as
    // `test_clear_all_releases_committed_executable_bytes`.
    let buf = ExecutableBuffer::new(4096).expect("alloc failed");
    let after = COMMITTED_JIT_CODE_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        after >= 4096,
        "committed ledger {after} is below this test's own live mapping (4096 bytes)"
    );
    // Drop now returns the executable mapping. Exact post-drop accounting
    // is covered by the reclamation tests because this suite runs other
    // buffer-allocation tests concurrently.
    drop(buf);
}

#[test]
pub(in crate::tests) fn code_cache_cap_refusals_accessor_is_monotonic() {
    // The accessor reads the global refusal counter; it never decreases.
    let a = jit_code_cache_cap_refusals();
    let b = jit_code_cache_cap_refusals();
    assert!(b >= a);
}

/// Interpreter round i1 wave 11: every compile verdict lives in the COMPILING
/// VM's registry. Two VMs allocate `ClassId`s from 0, so the same id and names
/// routinely name a different method in each; a verdict one VM records must
/// answer for that VM only, and never reach the no-VM registry the free
/// functions read.
#[test]
pub(in crate::tests) fn a_verdict_recorded_in_one_vm_registry_is_not_another_vms() {
    let class = "craton/test/i11/PerVmVerdicts";
    let id = cratonvm_types::ClassId::new(7);
    let vm_a = JitVerdictRegistry::default();
    let vm_b = JitVerdictRegistry::default();
    vm_a.mark_bail_listed(id, class, "bailed", "()V");
    vm_a.record_compile_refusal(id, class, "bailed", "()V", "i11-test-refusal");
    vm_a.mark_osr_entry_rejected(id, class, "loop", "()V", 12);

    assert!(vm_a.is_bail_listed(id, class, "bailed", "()V"));
    assert!(vm_a.bail_reason_for(id, class, "bailed", "()V").is_some());
    assert!(vm_a.is_osr_entry_rejected(id, class, "loop", "()V", 12));
    assert_eq!(vm_a.bail_list_size(), 1);
    assert_eq!(vm_a.osr_entry_reject_count(), 1);

    assert!(!vm_b.is_bail_listed(id, class, "bailed", "()V"));
    assert!(vm_b.bail_reason_for(id, class, "bailed", "()V").is_none());
    assert!(!vm_b.is_osr_entry_rejected(id, class, "loop", "()V", 12));
    assert_eq!(vm_b.bail_list_size(), 0);
    assert!(!is_jit_bail_listed(id, class, "bailed", "()V"));
    assert!(!is_osr_entry_rejected(id, class, "loop", "()V", 12));

    // Forgetting the class in another VM leaves this VM's verdicts alone.
    assert_eq!(vm_b.forget_for_class(id, class), 0);
    assert!(vm_a.is_bail_listed(id, class, "bailed", "()V"));
    assert_eq!(vm_a.forget_for_class(id, class), 2);
    assert!(!vm_a.is_bail_listed(id, class, "bailed", "()V"));
}

/// Interpreter round i1 wave 13: a registry's verdicts expire on ITS VM's
/// redefinitions and code-cache flushes only. They used to stamp with the
/// process-wide epochs, so one VM's `redefineClass` re-opened every other
/// VM's bail list and OSR rejects.
#[test]
pub(in crate::tests) fn verdicts_expire_on_their_own_registrys_epochs_only() {
    let class = "craton/test/i13/PerVmEpochs";
    let id = cratonvm_types::ClassId::new(11);
    let refusal = osr_refusal(OSR_REFUSE_INLINED_SCOPE, "i13-probe");
    assert!(osr_refusal_depends_on_compile_state(&refusal));
    let vm_a = JitVerdictRegistry::default();
    let vm_b = JitVerdictRegistry::default();
    vm_a.mark_bail_listed(id, class, "bailed", "()V");
    vm_a.record_compile_refusal(id, class, "bailed", "()V", "i13-test-refusal");
    vm_a.mark_osr_entry_rejected(id, class, "loop", "()V", 12);
    vm_a.mark_osr_entry_rejected_by(id, class, "loop", "()V", 20, &refusal);

    // Another VM's redefinition and flush reach none of them.
    vm_b.note_redefinition();
    vm_b.note_code_cache_flush();
    assert!(vm_a.is_bail_listed(id, class, "bailed", "()V"));
    assert!(vm_a.bail_reason_for(id, class, "bailed", "()V").is_some());
    assert!(vm_a.is_osr_entry_rejected(id, class, "loop", "()V", 12));
    assert!(vm_a.is_osr_entry_rejected(id, class, "loop", "()V", 20));
    assert_eq!(vm_a.bail_list_size(), 1);

    // This VM's code-cache flush expires only the compile-state verdict.
    vm_a.note_code_cache_flush();
    assert!(vm_a.is_bail_listed(id, class, "bailed", "()V"));
    assert!(vm_a.is_osr_entry_rejected(id, class, "loop", "()V", 12));
    assert!(!vm_a.is_osr_entry_rejected(id, class, "loop", "()V", 20));
    assert_eq!(vm_a.osr_entry_reject_count(), 1);

    // This VM's redefinition expires them all.
    vm_a.note_redefinition();
    assert!(!vm_a.is_bail_listed(id, class, "bailed", "()V"));
    assert!(vm_a.bail_reason_for(id, class, "bailed", "()V").is_none());
    assert!(!vm_a.is_osr_entry_rejected(id, class, "loop", "()V", 12));
    assert!(!vm_a.is_osr_entry_rejected(id, class, "loop", "()V", 20));
    assert_eq!(vm_a.bail_list_size(), 0);
    assert_eq!(vm_a.osr_entry_reject_count(), 0);
}

/// Interpreter round i1 wave 15, lane L4: a VM's two verdict registries read
/// the VM's code-state epoch as their flush epoch (`FlushEpoch::Shared`). The
/// owner advancing that one counter expires their flush-dependent verdicts;
/// their own flush hooks never move the VM's counter, and a redefinition
/// moves only their redefine epochs.
#[test]
pub(in crate::tests) fn a_shared_flush_epoch_is_advanced_by_its_owner_only() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let class = "craton/test/i15/SharedFlushEpoch";
    let id = cratonvm_types::ClassId::new(0x1504);
    let refusal = osr_refusal(OSR_REFUSE_INLINED_SCOPE, "i15-probe");
    let epoch = std::sync::Arc::new(AtomicU64::new(1));
    let vm = JitVerdictRegistry::with_flush_epoch(std::sync::Arc::clone(&epoch));
    let despec = crate::compile_gate::RuntimeDespecRegistry::new_registered_with_flush_epoch(
        std::sync::Arc::clone(&epoch),
    );
    vm.mark_osr_entry_rejected(id, class, "loop", "()V", 12);
    vm.mark_osr_entry_rejected_by(id, class, "loop", "()V", 20, &refusal);
    despec.mark(id, class, "spec", "()V");

    vm.note_code_cache_flush();
    despec.note_code_cache_flush();
    assert_eq!(
        epoch.load(Ordering::Acquire),
        1,
        "the hooks own no flush epoch"
    );
    assert!(vm.is_osr_entry_rejected(id, class, "loop", "()V", 20));
    assert!(despec.is_not_compilable(id, class, "spec", "()V"));

    // What `JitRealm::flush_code_cache` does: one advance, both registries.
    epoch.fetch_add(1, Ordering::AcqRel);
    assert!(vm.is_osr_entry_rejected(id, class, "loop", "()V", 12));
    assert!(!vm.is_osr_entry_rejected(id, class, "loop", "()V", 20));
    assert!(!despec.is_not_compilable(id, class, "spec", "()V"));

    vm.note_redefinition();
    assert_eq!(epoch.load(Ordering::Acquire), 2);
    assert!(!vm.is_osr_entry_rejected(id, class, "loop", "()V", 12));
}

/// Interpreter round i1 wave 13: a compile that began before the last
/// redefinition or flush (its epoch witness is older than the install epoch)
/// records no verdict, so a bail of the OLD bytecode cannot bail-list the new
/// one. The witness is set by hand on this thread, not by moving the process
/// epoch, which would refuse a concurrent test's publication.
#[test]
pub(in crate::tests) fn a_compile_older_than_the_install_epoch_records_no_verdict() {
    let class = "craton/test/i13/StaleCompile";
    let id = cratonvm_types::ClassId::new(12);
    let vm = JitVerdictRegistry::default();
    let now = jit_install_epoch();
    COMPILE_INSTALL_EPOCH.with(|c| c.set(Some((now.wrapping_sub(1), 0))));
    vm.mark_bail_listed(id, class, "bailed", "()V");
    vm.record_compile_refusal(id, class, "bailed", "()V", "i13-test-refusal");
    vm.mark_osr_entry_rejected(id, class, "loop", "()V", 4);
    COMPILE_INSTALL_EPOCH.with(|c| c.set(None));
    assert!(!vm.is_bail_listed(id, class, "bailed", "()V"));
    assert!(vm.bail_reason_for(id, class, "bailed", "()V").is_none());
    assert!(!vm.is_osr_entry_rejected(id, class, "loop", "()V", 4));

    // With no compile open (a door recording outside `try_compile`), the
    // verdicts are recorded as before.
    vm.mark_bail_listed(id, class, "bailed", "()V");
    vm.mark_osr_entry_rejected(id, class, "loop", "()V", 4);
    assert!(vm.is_bail_listed(id, class, "bailed", "()V"));
    assert!(vm.is_osr_entry_rejected(id, class, "loop", "()V", 4));
}

/// Interpreter round i1 wave 17, lane L5
/// (`interpreter-L4-verdict-recording-witness-reads-the-process-epoch-FIXED`):
/// a compile admitted against a registry judges
/// its verdicts stale against THAT registry's epochs. Another VM's
/// redefinition and flush, and a moved process epoch (set by hand on this
/// thread, as above), do not drop them; this VM's own flush and redefinition
/// still do. A registry the compile was not admitted against keeps the
/// process witness.
#[test]
pub(in crate::tests) fn a_compiles_verdicts_are_judged_against_its_own_registrys_epochs() {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::Arc;
    let class = "craton/test/i17/OwnWitness";
    let id = cratonvm_types::ClassId::new(0x1751);
    let flush_a = Arc::new(AtomicU64::new(1));
    let redefine_a = Arc::new(AtomicU32::new(0));
    let flush_b = Arc::new(AtomicU64::new(1));
    let redefine_b = Arc::new(AtomicU32::new(0));
    let vm_a = JitVerdictRegistry::with_vm_epochs(Arc::clone(&flush_a), Arc::clone(&redefine_a));
    let vm_b = JitVerdictRegistry::with_vm_epochs(Arc::clone(&flush_b), Arc::clone(&redefine_b));
    let stale_process_witness = || {
        let now = jit_install_epoch();
        COMPILE_INSTALL_EPOCH.with(|c| c.set(Some((now.wrapping_sub(1), 0))));
    };

    // A compile of VM A is open; VM B redefines and flushes, and the process
    // epoch moves (every VM's events move it).
    stale_process_witness();
    let witness = vm_a.open_verdict_epoch_witness();
    redefine_b.fetch_add(1, Ordering::AcqRel);
    flush_b.fetch_add(1, Ordering::AcqRel);
    vm_a.mark_bail_listed(id, class, "bailed", "()V");
    vm_a.mark_osr_entry_rejected(id, class, "loop", "()V", 4);
    // VM B was not what this compile was admitted against: process witness.
    vm_b.mark_bail_listed(id, class, "bailed", "()V");
    assert!(
        vm_a.is_bail_listed(id, class, "bailed", "()V"),
        "B's events do not reach A"
    );
    assert!(vm_a.is_osr_entry_rejected(id, class, "loop", "()V", 4));
    assert!(
        !vm_b.is_bail_listed(id, class, "bailed", "()V"),
        "a registry the compile was not admitted against keeps the process witness"
    );

    // VM A's own flush during the compile still suppresses.
    flush_a.fetch_add(1, Ordering::AcqRel);
    vm_a.mark_bail_listed(id, class, "flushed", "()V");
    assert!(!vm_a.is_bail_listed(id, class, "flushed", "()V"));
    drop(witness);

    // And its own redefinition, under a fresh witness.
    let witness = vm_a.open_verdict_epoch_witness();
    redefine_a.fetch_add(1, Ordering::AcqRel);
    vm_a.mark_bail_listed(id, class, "redefined", "()V");
    assert!(!vm_a.is_bail_listed(id, class, "redefined", "()V"));
    drop(witness);
    COMPILE_INSTALL_EPOCH.with(|c| c.set(None));
    assert_eq!(COMPILE_VERDICT_EPOCHS.with(|c| c.get()), None);
}

/// The verdict witness nests like the install witness: the outermost
/// compile's epochs are kept, and dropping the inner witness restores them.
#[test]
pub(in crate::tests) fn the_verdict_witness_keeps_the_outermost_compile() {
    let outer_vm = JitVerdictRegistry::default();
    let inner_vm = JitVerdictRegistry::default();
    let outer = outer_vm.open_verdict_epoch_witness();
    let seen = COMPILE_VERDICT_EPOCHS.with(|c| c.get());
    assert!(seen.is_some());
    let inner = inner_vm.open_verdict_epoch_witness();
    assert_eq!(
        COMPILE_VERDICT_EPOCHS.with(|c| c.get()),
        seen,
        "outermost wins"
    );
    drop(inner);
    assert_eq!(COMPILE_VERDICT_EPOCHS.with(|c| c.get()), seen);
    drop(outer);
    assert_eq!(COMPILE_VERDICT_EPOCHS.with(|c| c.get()), None);
}

/// Interpreter round i1 wave 17, stage 1 of
/// `i14-L3-proposal-class-scoped-redefinition-invalidation`: the census counts
/// the bodies a redefined class declares and the bodies that inlined from it,
/// not an unrelated body, and evicts nothing.
#[test]
pub(in crate::tests) fn redefinition_dependents_counts_declaring_and_inlining_bodies_only() {
    fn body(inlined: &[(&str, &str, &str)]) -> CompiledMethod {
        let mut cm = CompiledMethod::new(ExecutableBuffer::new(64).unwrap());
        cm.inlined_methods = inlined
            .iter()
            .map(|(c, m, d)| (c.to_string(), m.to_string(), d.to_string()))
            .collect();
        cm
    }
    let cache = JitCache::new();
    let redefined = cratonvm_types::ClassId::new(0x1760);
    let caller = cratonvm_types::ClassId::new(0x1761);
    let target = "craton/test/i17/Redefined";
    cache.put(
        target.into(),
        "own".into(),
        "()V".into(),
        redefined,
        body(&[]),
    );
    cache.put(
        "craton/test/i17/Caller".into(),
        "inlines".into(),
        "()V".into(),
        caller,
        body(&[(target, "own", "()V")]),
    );
    cache.put(
        "craton/test/i17/Caller".into(),
        "plain".into(),
        "()V".into(),
        caller,
        body(&[]),
    );
    assert_eq!(cache.len(), 3);
    assert_eq!(cache.redefinition_dependents(redefined, target), 2);
    assert_eq!(cache.len(), 3, "a census evicts nothing");
    let nobody = cratonvm_types::ClassId::new(0x1762);
    assert_eq!(
        cache.redefinition_dependents(nobody, "craton/test/i17/Nobody"),
        0
    );
}

/// Interpreter round i1 wave 17 (`i15-L4-proposal-one-redefine-epoch-per-vm`):
/// a VM's two verdict registries share the VM's redefine epoch. The owner
/// advancing it expires every verdict of both; their own redefinition hooks
/// move neither shared counter.
#[test]
pub(in crate::tests) fn a_shared_redefine_epoch_is_advanced_by_its_owner_only() {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::Arc;
    let class = "craton/test/i17/SharedRedefineEpoch";
    let id = cratonvm_types::ClassId::new(0x1752);
    let flush = Arc::new(AtomicU64::new(1));
    let redefine = Arc::new(AtomicU32::new(0));
    let vm = JitVerdictRegistry::with_vm_epochs(Arc::clone(&flush), Arc::clone(&redefine));
    let despec = crate::compile_gate::RuntimeDespecRegistry::new_registered_with_vm_epochs(
        Arc::clone(&flush),
        Arc::clone(&redefine),
    );
    vm.mark_bail_listed(id, class, "bailed", "()V");
    despec.mark(id, class, "spec", "()V");

    vm.note_redefinition();
    despec.note_redefinition();
    assert_eq!(
        redefine.load(Ordering::Acquire),
        0,
        "the hooks own no redefine epoch"
    );
    assert_eq!(flush.load(Ordering::Acquire), 1, "nor a flush epoch");
    assert!(vm.is_bail_listed(id, class, "bailed", "()V"));
    assert!(despec.is_not_compilable(id, class, "spec", "()V"));

    // What `JitRealm::note_class_redefinition` does: one advance, both registries.
    redefine.fetch_add(1, Ordering::AcqRel);
    assert!(!vm.is_bail_listed(id, class, "bailed", "()V"));
    assert!(!despec.is_not_compilable(id, class, "spec", "()V"));
}

/// The bail list still short-circuits WITHIN one VM: a compile that bails
/// permanently records into the registry its request carried, and the next
/// attempt through the same registry is refused before the pipeline runs,
/// while a request carrying another registry is not.
#[test]
pub(in crate::tests) fn a_bail_recorded_through_a_request_is_asked_through_the_same_registry() {
    let class = "craton/test/i11/RequestVerdicts";
    let id = cratonvm_types::ClassId::new(9);
    let vm_a = JitVerdictRegistry::default();
    let vm_b = JitVerdictRegistry::default();
    // SAFETY: an all-zero `JitRuntimeHelpers` is valid (every entry is an
    // address or a flag); nothing below is executed.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    let cached = CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
        declaring_class_id: id,
        class_name: Arc::from(class),
        method_name: Arc::from("m"),
        method_descriptor: Arc::from("()V"),
        source_file: None,
        // `return`, padded the way the VM pads bytecode.
        code: Arc::from(&[0xb1u8, 0, 0][..]),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 0,
        max_locals: 0,
        num_params: 0,
        is_synchronized: false,
        is_static: true,
    });
    // Since wave 17 the registry is a required input of the constructor, and
    // since wave 18 the deferred-`new` memo too (one `CompileRealm`): there is
    // no request without them, so no fallback to assert on.
    let (memo_a, memo_b) = (DeferredNewRetries::default(), DeferredNewRetries::default());
    let realm_a = CompileRealm {
        verdicts: &vm_a,
        deferred_new_retries: &memo_a,
        jdk_only_sinks: crate::jdk_only_sink::process_jdk_only_sinks(),
    };
    let realm_b = CompileRealm {
        verdicts: &vm_b,
        deferred_new_retries: &memo_b,
        jdk_only_sinks: crate::jdk_only_sink::process_jdk_only_sinks(),
    };
    let a = CompileRequest::new(&cached, &helpers, realm_a);
    let b = CompileRequest::new(&cached, &helpers, realm_b);
    assert!(std::ptr::eq(a.verdicts(), &vm_a));
    assert!(std::ptr::eq(b.verdicts(), &vm_b));
    assert!(std::ptr::eq(a.deferred_new_retries, &memo_a));
    assert!(std::ptr::eq(b.deferred_new_retries, &memo_b));
    // A struct update keeps the registries of the request it copies.
    let a_optimizing = CompileRequest {
        optimize: true,
        ..a
    };
    assert!(std::ptr::eq(a_optimizing.verdicts(), &vm_a));
    assert!(std::ptr::eq(a_optimizing.deferred_new_retries, &memo_a));
    // The no-VM realm names the two process registries, together.
    let process = CompileRealm::process();
    assert!(std::ptr::eq(process.verdicts, process_jit_verdicts()));
    assert!(std::ptr::eq(
        process.deferred_new_retries,
        crate::jfr_compile_decision::process_deferred_new_retries()
    ));

    a.verdicts().mark_bail_listed(id, class, "m", "()V");
    assert!(
        try_compile_request(&a).is_none(),
        "the VM that bail-listed the method refuses it at admission"
    );
    assert!(a.verdicts().is_bail_listed(id, class, "m", "()V"));
    assert!(
        !b.verdicts().is_bail_listed(id, class, "m", "()V"),
        "another VM's request never sees the bail"
    );
}

/// Interpreter round i1 wave 38, lane L2 (item 1 of the wave-37 compile-door
/// review): a backend refusal of the optimizing OSR route's compile
/// (`CompileRequest::osr_optimizing_route`) is the route's own verdict, which
/// the VM memoises; it must not bail-list the method, because the bail list
/// closes every door, the single-pass OSR door included. The same refusal
/// through an ordinary request still bail-lists (the positive control).
#[test]
pub(in crate::tests) fn an_osr_optimizing_route_refusal_does_not_bail_list_the_method() {
    let class = "craton/test/i38/OsrRouteRefusal";
    let id = cratonvm_types::ClassId::new(11);
    let verdicts = JitVerdictRegistry::default();
    let memo = DeferredNewRetries::default();
    // SAFETY: an all-zero `JitRuntimeHelpers` is valid (every entry is an
    // address or a flag); nothing below is executed.
    let helpers: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    // 0xcb is no JVM opcode: `jit_scan` rejects it, a permanent bail
    // (`jit-scan-reject`) that sets `backend_attempted`. Then `return`, padded
    // the way the VM pads bytecode.
    let cached = CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
        declaring_class_id: id,
        class_name: Arc::from(class),
        method_name: Arc::from("m"),
        method_descriptor: Arc::from("()V"),
        source_file: None,
        code: Arc::from(&[0xcbu8, 0xb1, 0, 0][..]),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 0,
        max_locals: 0,
        num_params: 0,
        is_synchronized: false,
        is_static: true,
    });
    let realm = CompileRealm {
        verdicts: &verdicts,
        deferred_new_retries: &memo,
        jdk_only_sinks: crate::jdk_only_sink::process_jdk_only_sinks(),
    };
    let ordinary = CompileRequest::new(&cached, &helpers, realm);
    let route = CompileRequest {
        osr_optimizing_route: true,
        ..CompileRequest::new(&cached, &helpers, realm)
    };
    assert!(try_compile_request(&route).is_none());
    assert!(
        !verdicts.is_bail_listed(id, class, "m", "()V"),
        "the OSR route's refusal is not a bail-list verdict"
    );
    assert!(try_compile_request(&ordinary).is_none());
    assert!(
        verdicts.is_bail_listed(id, class, "m", "()V"),
        "the same refusal through the method-entry door bail-lists"
    );
}

/// The recursion-cycle sets are the compiling VM's too: a cycle one VM's
/// compile closes keeps that VM's calls on dispatch and no other VM's.
/// Push `keys` on this thread's compile stack, run `body`, and pop them again.
pub(in crate::tests) fn with_i12_compile_stack<R>(keys: &[JitCompileMethodKey], body: impl FnOnce() -> R) -> R {
    JIT_COMPILE_STACK.with(|stack| stack.borrow_mut().extend(keys.iter().cloned()));
    let out = body();
    JIT_COMPILE_STACK.with(|stack| {
        let mut stack = stack.borrow_mut();
        let keep = stack.len() - keys.len();
        stack.truncate(keep);
    });
    out
}

/// Interpreter round i1 wave 12: a cycle member carries its declaring class's
/// identity, so a cycle through one loader's `p/A.a` keeps calls into THAT
/// class on dispatch and none into another loader's same-named `p/A.a`. An
/// asker that cannot name its class still gets the conservative answer.
/// A member recorded without an identity (a compile with no `ClassId`) stands
/// for every same-named class, as the name-keyed sets did.
#[test]
pub(in crate::tests) fn an_id_less_cycle_member_stands_for_every_same_named_class() {
    let vm = JitVerdictRegistry::default();
    let stack = [
        JitCompileMethodKey::new("i12/q/A", "a", "()V"),
        JitCompileMethodKey::new("i12/q/B", "b", "()V"),
    ];
    let four = cratonvm_types::ClassId::new(4);
    let nine = cratonvm_types::ClassId::new(9);
    let closed = with_i12_compile_stack(&stack, || {
        vm.note_recursive_compile_cycle("i12/q/A", "a", "()V", true, || Some(four))
    });
    assert!(closed, "an id-less outer compile matches any identity");
    assert!(vm.direct_call_requires_dispatch_of("i12/q/B", "b", "()V", || Some(nine)));
}

/// The compile stack is matched by identity too: an outer compile of ANOTHER
/// loader's same-named method is not the call's target, so the edge closes no
/// cycle and records nothing, while the same class's outer compile does.
#[test]
pub(in crate::tests) fn an_outer_compile_of_another_same_named_class_closes_no_cycle() {
    let one = cratonvm_types::ClassId::new(1);
    let two = cratonvm_types::ClassId::new(2);
    let five = cratonvm_types::ClassId::new(5);
    let vm = JitVerdictRegistry::default();
    let stack = [
        JitCompileMethodKey::of_class(one, "i12/r/A", "a", "()V"),
        JitCompileMethodKey::of_class(five, "i12/r/B", "b", "()V"),
    ];
    let other = with_i12_compile_stack(&stack, || {
        vm.note_recursive_compile_cycle("i12/r/A", "a", "()V", true, || Some(two))
    });
    assert!(!other);
    assert!(!vm.direct_call_requires_dispatch("i12/r/A", "a", "()V"));
    assert!(!vm.is_recursive_cycle_participant("i12/r/B", "b", "()V"));
    let same = with_i12_compile_stack(&stack, || {
        vm.note_recursive_compile_cycle("i12/r/A", "a", "()V", true, || Some(one))
    });
    assert!(same);
    assert!(vm.direct_call_requires_dispatch_of("i12/r/A", "a", "()V", || Some(one)));
}

/// The asker's class identity may come from a VM resolver, so it is asked only
/// on a name hit (never for an empty set, the overwhelmingly common case),
/// with the set's lock released and this thread's compile stack not borrowed.
#[test]
pub(in crate::tests) fn a_cycle_ask_resolves_the_class_only_on_a_name_hit_and_outside_every_lock() {
    let one = cratonvm_types::ClassId::new(1);
    let vm = JitVerdictRegistry::default();
    let asked = std::cell::Cell::new(0u32);
    assert!(
        !vm.direct_call_requires_dispatch_of("i12/s/A", "a", "()V", || {
            asked.set(asked.get() + 1);
            Some(one)
        })
    );
    assert!(!with_i12_compile_stack(&[], || {
        vm.note_recursive_compile_cycle("i12/s/A", "a", "()V", true, || {
            asked.set(asked.get() + 1);
            Some(one)
        })
    }));
    assert_eq!(asked.get(), 0, "no name hit, no resolution");

    let stack = [
        JitCompileMethodKey::of_class(one, "i12/s/A", "a", "()V"),
        JitCompileMethodKey::of_class(one, "i12/s/B", "b", "()V"),
    ];
    let closed = with_i12_compile_stack(&stack, || {
        vm.note_recursive_compile_cycle("i12/s/A", "a", "()V", true, || {
            asked.set(asked.get() + 1);
            let stack_free = JIT_COMPILE_STACK.with(|s| s.try_borrow_mut().is_ok());
            assert!(stack_free, "the compile stack is released while resolving");
            Some(one)
        })
    });
    assert!(closed);
    assert_eq!(asked.get(), 1);
    assert!(
        vm.direct_call_requires_dispatch_of("i12/s/B", "b", "()V", || {
            asked.set(asked.get() + 1);
            let sticky_free = vm.sticky_dispatch_methods.try_write().is_some();
            let recursive_free = vm.recursive_cycle_methods.try_write().is_some();
            assert!(
                sticky_free && recursive_free,
                "no cycle-set lock is held while resolving"
            );
            Some(one)
        })
    );
    assert_eq!(asked.get(), 2);
}

/// Interpreter round i1 wave 13: the cycle checks name an inherited-name call
/// (`invokestatic Sub.f`, which JVMS 5.4.3.3 resolves to `A.f`) by the class
/// that DECLARES the method, so an open `A.f` closes the cycle and a sticky
/// `A.f` keeps the call on dispatch. The declaring resolver is asked only when
/// an open compile or a member has the method's names and the site names the
/// constant-pool class.
#[test]
pub(in crate::tests) fn an_inherited_name_call_is_checked_against_its_declaring_class() {
    let a = cratonvm_types::ClassId::new(21);
    let vm = JitVerdictRegistry::default();
    let asked = std::cell::Cell::new(0u32);
    let declaring_of = |cp_idx: u16| -> Option<String> {
        asked.set(asked.get() + 1);
        (cp_idx == 7).then(|| "i13/A".to_string())
    };
    let declaring: &dyn Fn(u16) -> Option<String> = &declaring_of;
    let ask = |class: &str, method: &str, names_cp_class: bool| {
        cycle_site_declaring_class(&vm, Some(declaring), 7, class, method, "(I)I", || {
            names_cp_class
        })
    };

    // Nothing open and no member with these names: not resolved at all.
    assert_eq!(ask("i13/Sub", "f", true), None);
    assert_eq!(asked.get(), 0);

    let stack = [
        JitCompileMethodKey::of_class(a, "i13/A", "f", "(I)I"),
        JitCompileMethodKey::of_class(a, "i13/A", "g", "(I)I"),
    ];
    let closed = with_i12_compile_stack(&stack, || {
        // A pinned or redirected site already names its own class.
        assert_eq!(ask("i13/Sub", "f", false), None);
        assert_eq!(asked.get(), 0);
        let cycle = ask("i13/Sub", "f", true);
        assert_eq!(cycle.as_deref(), Some("i13/A"));
        assert_eq!(asked.get(), 1);
        // By the site's name the open `A.f` is not seen, and nothing is
        // recorded...
        assert!(!vm.note_recursive_compile_cycle("i13/Sub", "f", "(I)I", true, || None));
        // ...by the declaring class the edge closes the cycle.
        vm.note_recursive_compile_cycle(
            cycle.as_deref().unwrap_or("i13/Sub"),
            "f",
            "(I)I",
            true,
            || None,
        )
    });
    assert!(closed);
    assert!(vm.direct_call_requires_dispatch_of("i13/A", "f", "(I)I", || None));
    assert!(!vm.direct_call_requires_dispatch_of("i13/Sub", "f", "(I)I", || None));

    // With the compile stack gone, the recorded member still makes a later
    // compile resolve the inherited name, and keeps the call on dispatch.
    let later = ask("i13/Sub", "f", true);
    assert_eq!(later.as_deref(), Some("i13/A"));
    assert!(vm.direct_call_requires_dispatch_of(
        later.as_deref().unwrap_or("i13/Sub"),
        "f",
        "(I)I",
        || None
    ));
    // A method no member names is not resolved; a site that already names
    // the declaring class keeps its name.
    let before = asked.get();
    assert_eq!(ask("i13/Sub", "h", true), None);
    assert_eq!(asked.get(), before);
    assert_eq!(ask("i13/A", "f", true), None);
}

/// Interpreter round i1 wave 13: an UNLOADED class's recursion-cycle members
/// are forgotten by its exact identity, so the sets stop growing with class
/// churn and a class loaded under the recycled id does not inherit them.
/// Another class's members and id-less members (which stand for every
/// same-named class) stay.
#[test]
pub(in crate::tests) fn an_unloaded_classs_cycle_members_are_forgotten_by_identity() {
    let gone = cratonvm_types::ClassId::new(31);
    let kept = cratonvm_types::ClassId::new(32);
    let vm = JitVerdictRegistry::default();
    let stack = [
        JitCompileMethodKey::of_class(gone, "i13/u/A", "a", "()V"),
        JitCompileMethodKey::of_class(kept, "i13/u/B", "b", "()V"),
    ];
    assert!(with_i12_compile_stack(&stack, || {
        vm.note_recursive_compile_cycle("i13/u/A", "a", "()V", true, || Some(gone))
    }));
    assert_eq!(vm.recursion_cycle_member_counts(), (2, 2));
    assert_eq!(
        vm.forget_cycle_members_of_unloaded_class(cratonvm_types::ClassId::new(0)),
        0,
        "the no-identity sentinel forgets nothing"
    );
    assert_eq!(vm.forget_cycle_members_of_unloaded_class(gone), 2);
    assert_eq!(vm.recursion_cycle_member_counts(), (1, 1));
    assert!(!vm.direct_call_requires_dispatch_of("i13/u/A", "a", "()V", || Some(gone)));
    assert!(vm.direct_call_requires_dispatch_of("i13/u/B", "b", "()V", || Some(kept)));

    let id_less = [
        JitCompileMethodKey::new("i13/u/C", "c", "()V"),
        JitCompileMethodKey::new("i13/u/D", "d", "()V"),
    ];
    assert!(with_i12_compile_stack(&id_less, || {
        vm.note_recursive_compile_cycle("i13/u/C", "c", "()V", true, || None)
    }));
    assert_eq!(
        vm.forget_cycle_members_of_unloaded_class(cratonvm_types::ClassId::new(33)),
        0
    );
    assert!(vm.direct_call_requires_dispatch("i13/u/C", "c", "()V"));
}

/// The per-VM verdict counts reach the `JIT method stats:` line, as fields
/// appended at its END (scripts parse the line's earlier fields).
#[test]
pub(in crate::tests) fn the_method_stats_line_carries_the_vms_verdict_counts() {
    let id = cratonvm_types::ClassId::new(3);
    let class = "craton/test/i12/ReportedVerdicts";
    let vm = JitVerdictRegistry::default();
    assert_eq!(
        crate::tiered::verdict_count_fields(&vm),
        " | bail-listed=0 osr-entry-rejects=0 cycle-members=0 sticky=0 \
         | redefinitions=0 redefine-evicted-bodies=0 redefine-dependent-bodies=0"
    );
    vm.mark_bail_listed(id, class, "m", "()V");
    vm.mark_bail_listed(id, class, "n", "()V");
    vm.mark_osr_entry_rejected(id, class, "loop", "()V", 12);
    let stack = [
        JitCompileMethodKey::of_class(id, class, "m", "()V"),
        JitCompileMethodKey::of_class(id, class, "n", "()V"),
    ];
    // A statically bound closing edge records the cycle without making it
    // sticky.
    assert!(with_i12_compile_stack(&stack, || {
        vm.note_recursive_compile_cycle(class, "m", "()V", false, || Some(id))
    }));
    assert_eq!(
        crate::tiered::verdict_count_fields(&vm),
        " | bail-listed=2 osr-entry-rejects=1 cycle-members=2 sticky=0 \
         | redefinitions=0 redefine-evicted-bodies=0 redefine-dependent-bodies=0"
    );
    // Interpreter round i1 wave 17: the redefinition census rides at the end.
    vm.note_redefinition_census(5, Some(2));
    vm.note_redefinition_census(3, None);
    assert_eq!(vm.redefinition_census(), (2, 8, 2));
    assert!(crate::tiered::verdict_count_fields(&vm)
        .ends_with(" | redefinitions=2 redefine-evicted-bodies=8 redefine-dependent-bodies=2"));
}

// ───────────────────────────────────────────────────────────────────────
// Interpreter round i1 wave 20, lane L1: stage 2b of
// `i14-L3-proposal-class-scoped-redefinition-invalidation` — a redefinition
// withdraws the bodies it can make stale, one test per dependency kind.
// ───────────────────────────────────────────────────────────────────────

/// A published-shape body: one `RET`, so a caller may bake its entry.
pub(in crate::tests) fn i20_l1_body(inlined: &[(&str, &str, &str)]) -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    let mut cm = CompiledMethod::new(buf);
    cm.inlined_methods = inlined
        .iter()
        .map(|(c, m, d)| (c.to_string(), m.to_string(), d.to_string()))
        .collect();
    cm
}

/// An OSR body for `put_osr`, which debug-asserts `compiled_via_osr`.
pub(in crate::tests) fn i20_l1_osr_body() -> CompiledMethod {
    let mut cm = i20_l1_body(&[]);
    cm.compiled_via_osr = true;
    cm
}

pub(in crate::tests) fn i20_l1_put(cache: &JitCache, class: &str, name: &str, id: u32, cm: CompiledMethod) {
    use std::sync::Arc;
    cache.put(
        Arc::from(class),
        Arc::from(name),
        Arc::from("()V"),
        cratonvm_types::ClassId::new(id),
        cm,
    );
}

pub(in crate::tests) fn i20_l1_has(cache: &JitCache, class: &str, name: &str, id: u32) -> bool {
    cache
        .get(class, name, "()V", cratonvm_types::ClassId::new(id))
        .is_some()
}

/// Every dependency kind a body records is withdrawn by a scoped
/// redefinition, and nothing else is: the class's own method-entry and OSR
/// bodies, an inlined callee, a speculated (devirtualised) receiver, a
/// class-hierarchy (unique concrete method) bind, a direct call and a direct
/// call to that direct caller. The census counts the same set first.
#[test]
pub(in crate::tests) fn a_scoped_redefinition_withdraws_each_dependency_kind_and_nothing_else() {
    use std::sync::Arc;
    let cache = JitCache::new();
    let target = "craton/test/i20/Redefined";
    let (redefined, callers) = (0x2001_u32, 0x2002_u32);
    let redefined_id = cratonvm_types::ClassId::new(redefined);
    let caller = "craton/test/i20/Caller";

    i20_l1_put(&cache, target, "own", redefined, i20_l1_body(&[]));
    cache.put_osr(
        Arc::from(target),
        Arc::from("loop"),
        Arc::from("()V"),
        redefined_id,
        i20_l1_osr_body(),
    );
    let inlined = InlineDependency::InlinedCallee {
        class_name: target.to_string(),
        method_name: "own".to_string(),
        descriptor: "()V".to_string(),
    }
    .as_invalidation_triple();
    let speculated = InlineDependency::SpeculatedReceiver {
        class_name: target.to_string(),
        class_id: redefined,
        method_name: "virt".to_string(),
        descriptor: "()V".to_string(),
    }
    .as_invalidation_triple();
    let hierarchy = InlineDependency::UniqueConcreteMethod {
        class_name: target.to_string(),
        class_id: redefined,
        method_name: "only".to_string(),
        descriptor: "()V".to_string(),
    }
    .as_invalidation_triple();
    let one =
        |t: &(String, String, String)| i20_l1_body(&[(t.0.as_str(), t.1.as_str(), t.2.as_str())]);
    i20_l1_put(&cache, caller, "inlines", callers, one(&inlined));
    i20_l1_put(&cache, caller, "speculates", callers, one(&speculated));
    i20_l1_put(&cache, caller, "cha", callers, one(&hierarchy));
    let own_entry = cache
        .get(target, "own", "()V", redefined_id)
        .expect("published")
        .entry_ptr() as usize;
    let mut direct = i20_l1_body(&[]);
    direct._direct_callee_entries = vec![own_entry];
    i20_l1_put(&cache, caller, "direct", callers, direct);
    let direct_entry = cache
        .get(
            caller,
            "direct",
            "()V",
            cratonvm_types::ClassId::new(callers),
        )
        .expect("the direct caller publishes")
        .entry_ptr() as usize;
    let mut transitive = i20_l1_body(&[]);
    transitive._direct_callee_entries = vec![direct_entry];
    i20_l1_put(&cache, caller, "transitive", callers, transitive);
    i20_l1_put(&cache, caller, "plain", callers, i20_l1_body(&[]));
    // Same name, other class id: an unrelated class's own body survives.
    i20_l1_put(&cache, target, "own", 0x2003, i20_l1_body(&[]));
    assert_eq!(cache.len(), 9);

    assert_eq!(cache.redefinition_dependents(redefined_id, target), 7);
    let (removed, withdrawn) = cache.invalidate_for_redefinition(redefined_id, target);
    assert_eq!(
        removed, 7,
        "own + OSR + inlined + speculated + CHA + direct + transitive"
    );
    assert_eq!(cache.len(), 2);
    assert!(!i20_l1_has(&cache, target, "own", redefined));
    assert!(cache.get_osr(target, "loop", "()V", redefined_id).is_none());
    for gone in ["inlines", "speculates", "cha", "direct", "transitive"] {
        assert!(
            !i20_l1_has(&cache, caller, gone, callers),
            "{gone} is withdrawn"
        );
    }
    assert!(
        i20_l1_has(&cache, caller, "plain", callers),
        "an unrelated body survives"
    );
    assert!(
        i20_l1_has(&cache, target, "own", 0x2003),
        "another class of the same name survives"
    );
    assert_eq!(withdrawn.len(), 7, "one key per withdrawn method");
    assert!(withdrawn
        .iter()
        .any(|(c, m, _, id)| &**c == target && &**m == "loop" && *id == redefined_id));
}

/// A compile that began before the scoped redefinition and declares or
/// inlined the class is refused publication by the logged record; one that
/// began before it and depends on nothing of the class publishes, since no
/// flush barrier is armed. A body of the class compiled before the
/// redefinition no longer counts as running its class's current bytecode;
/// one compiled after it, and another class's body, do.
#[test]
pub(in crate::tests) fn a_scoped_redefinition_refuses_stale_in_flight_bodies_only() {
    let cache = JitCache::new();
    let target = "craton/test/i20/InFlight";
    let other_class = "craton/test/i20/Other";
    let (redefined, other) = (0x2011_u32, 0x2012_u32);
    let redefined_id = cratonvm_types::ClassId::new(redefined);
    let other_id = cratonvm_types::ClassId::new(other);

    // Stamped now, i.e. "compilation began before the redefinition".
    let stale_own = i20_l1_body(&[]);
    let stale_inliner = i20_l1_body(&[(target, "m", "()V")]);
    let unrelated = i20_l1_body(&[]);
    let before = stale_own.install_epoch;

    let _ = cache.invalidate_for_redefinition(redefined_id, target);
    i20_l1_put(&cache, target, "m", redefined, stale_own);
    i20_l1_put(&cache, other_class, "inl", other, stale_inliner);
    i20_l1_put(&cache, other_class, "plain", other, unrelated);
    assert!(!i20_l1_has(&cache, target, "m", redefined), "old bytecode");
    assert!(!i20_l1_has(&cache, other_class, "inl", other));
    assert!(
        i20_l1_has(&cache, other_class, "plain", other),
        "a scoped redefinition arms no flush barrier"
    );

    assert!(!cache.compiled_since_redefinition_of(redefined_id, before));
    assert!(cache.compiled_since_redefinition_of(other_id, before));
    let after = i20_l1_body(&[]).install_epoch;
    assert!(cache.compiled_since_redefinition_of(redefined_id, after));
    i20_l1_put(&cache, target, "m", redefined, i20_l1_body(&[]));
    assert!(
        i20_l1_has(&cache, target, "m", redefined),
        "a compile that began after it publishes"
    );
}

/// The copied-bytecode marks the VM consults before scoping a redefinition
/// are per cache and survive a full flush (see `copied_bytecode_classes`);
/// the flush empties the per-class redefinition barriers, which its own
/// barrier is above.
#[test]
pub(in crate::tests) fn copied_bytecode_marks_are_per_cache_and_survive_a_flush() {
    let (cache, other) = (JitCache::new(), JitCache::new());
    let id = cratonvm_types::ClassId::new(0x2021);
    assert!(!cache.bytecode_was_copied(id));
    cache.note_bytecode_copied(id);
    cache.note_bytecode_copied(id);
    assert!(cache.bytecode_was_copied(id));
    assert!(!other.bytecode_was_copied(id), "per cache, so per VM");
    let _ = cache.invalidate_for_redefinition(id, "craton/test/i20/Copied");
    assert!(!cache.redefinition_barriers.lock().is_empty());
    cache.clear_all();
    assert!(cache.bytecode_was_copied(id), "never cleared");
    assert!(cache.redefinition_barriers.lock().is_empty());
}

/// `clear_all_collecting` hands back one key per withdrawn method (a
/// method-entry and an OSR body of one method are one key), for the VM's
/// tier demotion.
#[test]
pub(in crate::tests) fn a_full_flush_hands_back_one_key_per_withdrawn_method() {
    use std::sync::Arc;
    let cache = JitCache::new();
    let id = cratonvm_types::ClassId::new(0x2031);
    let class = "craton/test/i20/Flushed";
    i20_l1_put(&cache, class, "both", 0x2031, i20_l1_body(&[]));
    cache.put_osr(
        Arc::from(class),
        Arc::from("both"),
        Arc::from("()V"),
        id,
        i20_l1_osr_body(),
    );
    i20_l1_put(&cache, class, "entry", 0x2031, i20_l1_body(&[]));
    let (count, withdrawn) = cache.clear_all_collecting();
    assert_eq!(count, 3);
    assert_eq!(withdrawn.len(), 2);
    assert!(cache.is_empty());
}

/// A caller that baked a direct call to a body a tier-up has since
/// SUPERSEDED (replaced in the cache, not retired: it is still correct code
/// until a redefinition) keeps calling that old body, whose entry is no
/// published body's. The published-entry closure cannot see it; the scoped
/// eviction walks the callers' roots and withdraws it, and a caller of that
/// caller.
#[test]
pub(in crate::tests) fn a_scoped_redefinition_withdraws_a_caller_of_a_superseded_body() {
    let cache = JitCache::new();
    let target = "craton/test/i20/Superseded";
    let caller = "craton/test/i20/SupersededCaller";
    let (redefined, callers) = (0x2051_u32, 0x2052_u32);
    let redefined_id = cratonvm_types::ClassId::new(redefined);
    let callers_id = cratonvm_types::ClassId::new(callers);

    i20_l1_put(&cache, target, "own", redefined, i20_l1_body(&[]));
    let old_entry = cache
        .get(target, "own", "()V", redefined_id)
        .expect("published")
        .entry_ptr() as usize;
    let mut direct = i20_l1_body(&[]);
    direct._direct_callee_entries = vec![old_entry];
    i20_l1_put(&cache, caller, "direct", callers, direct);
    let direct_entry = cache
        .get(caller, "direct", "()V", callers_id)
        .expect("the caller publishes")
        .entry_ptr() as usize;
    let mut outer = i20_l1_body(&[]);
    outer._direct_callee_entries = vec![direct_entry];
    i20_l1_put(&cache, caller, "outer", callers, outer);
    // The tier-up: a new body under the same key; the caller keeps the old.
    i20_l1_put(&cache, target, "own", redefined, i20_l1_body(&[]));
    let new_entry = cache
        .get(target, "own", "()V", redefined_id)
        .expect("republished")
        .entry_ptr() as usize;
    assert_ne!(new_entry, old_entry);
    i20_l1_put(&cache, caller, "plain", callers, i20_l1_body(&[]));

    assert_eq!(cache.redefinition_dependents(redefined_id, target), 3);
    let (removed, _) = cache.invalidate_for_redefinition(redefined_id, target);
    assert_eq!(
        removed, 3,
        "the new body, its old body's caller, and that caller's caller"
    );
    assert!(!i20_l1_has(&cache, caller, "direct", callers));
    assert!(!i20_l1_has(&cache, caller, "outer", callers));
    assert!(i20_l1_has(&cache, caller, "plain", callers));
}

/// A compile that began before a scoped redefinition and baked a direct call
/// to a body of the redefined class that a tier-up has since superseded is
/// refused publication: the scan retired only the published body, so the
/// retired-callee check alone would let it publish a call into the old
/// bytecode. A compile that began after the redefinition publishes.
#[test]
pub(in crate::tests) fn a_compile_that_baked_a_superseded_body_of_the_redefined_class_is_refused() {
    let cache = JitCache::new();
    let target = "craton/test/i20/Baked";
    let caller = "craton/test/i20/BakedCaller";
    let (redefined, callers) = (0x2061_u32, 0x2062_u32);
    let redefined_id = cratonvm_types::ClassId::new(redefined);

    i20_l1_put(&cache, target, "own", redefined, i20_l1_body(&[]));
    // Keeps the old body alive past its supersession, as a caller would;
    // released through the retirement queue.
    let old = RetainedCode::new(
        cache
            .get(target, "own", "()V", redefined_id)
            .expect("published"),
    );
    let old_entry = old.entry_ptr() as usize;
    let mut in_flight = i20_l1_body(&[]);
    in_flight._direct_callee_entries = vec![old_entry];
    // The tier-up, then the redefinition.
    i20_l1_put(&cache, target, "own", redefined, i20_l1_body(&[]));
    let (removed, _) = cache.invalidate_for_redefinition(redefined_id, target);
    assert_eq!(removed, 1);
    assert!(
        !old.retired.load(std::sync::atomic::Ordering::SeqCst),
        "the superseded body is in no map, so the scan did not retire it"
    );

    i20_l1_put(&cache, caller, "late", callers, in_flight);
    assert!(!i20_l1_has(&cache, caller, "late", callers));

    i20_l1_put(&cache, target, "own", redefined, i20_l1_body(&[]));
    let new_entry = cache
        .get(target, "own", "()V", redefined_id)
        .expect("recompiled")
        .entry_ptr() as usize;
    let mut fresh = i20_l1_body(&[]);
    fresh._direct_callee_entries = vec![new_entry];
    i20_l1_put(&cache, caller, "fresh", callers, fresh);
    assert!(i20_l1_has(&cache, caller, "fresh", callers));
    drop(old);
}

/// The `(holder, cp index)` helpers' packed holder word
/// (`i19-L3-compiled-bodies-of-a-redefined-class-resolve-old-indices-in-the-new-pool`):
/// the class id in the low half, the constant-pool stamp plus one in the
/// high half, `0` there meaning no stamp; a word with no stamp is the class
/// id exactly as before.
#[test]
pub(in crate::tests) fn the_cp_holder_word_round_trips_its_stamp() {
    assert_eq!(cp_holder_word_parts(0x2071), (0x2071, None));
    assert_eq!(
        cp_holder_word_parts(0x2071 | (1_u64 << 32)),
        (0x2071, Some(0))
    );
    assert_eq!(
        cp_holder_word_parts(u64::from(u32::MAX) | (43_u64 << 32)),
        (u32::MAX, Some(42))
    );
    let word = cp_holder_word(0x2072);
    let (holder, stamp) = cp_holder_word_parts(word);
    assert_eq!(holder, 0x2072);
    assert_eq!(stamp, current_compile_cp_stamp());
}

/// Interpreter round i1 wave 21, lane L1
/// (`interpreter-L1-other-cp-indexed-jit-helpers-carry-no-cp-stamp-FIXED`): a
/// `multianewarray` site word's truncated stamp is rebuilt against the count
/// the helper reads -- exactly for code younger than the modulus, as the
/// newest matching count for older code -- and a word with no stamp, or one
/// naming a count above `now`, answers `None`. The class id and cp index are
/// unchanged by the stamp.
#[test]
pub(in crate::tests) fn the_multianewarray_site_stamp_is_rebuilt_against_the_current_count() {
    let m = MULTIANEWARRAY_STAMP_MODULUS;
    let site =
        |stamp: u64| -> i64 { (0x2201_u64 | (7_u64 << 32) | ((stamp % m + 1) << 48)) as i64 };
    for stamp in [0, 1, 41, m - 1, m, m + 1, 3 * m + 5, 70_000] {
        assert_eq!(unpack_multianewarray_site(site(stamp)), (0x2201, 7));
        assert_eq!(multianewarray_site_stamp(site(stamp), stamp), Some(stamp));
        assert_eq!(
            multianewarray_site_stamp(site(stamp), stamp + 9),
            Some(stamp)
        );
        assert_eq!(
            multianewarray_site_stamp(site(stamp), stamp + m - 1),
            Some(stamp),
            "exact up to the modulus"
        );
        assert_eq!(
            multianewarray_site_stamp(site(stamp), stamp + m),
            Some(stamp + m),
            "older code is answered the newest matching count"
        );
    }
    assert_eq!(multianewarray_site_stamp(site(40), 39), None, "above now");
    assert_eq!(
        multianewarray_site_stamp((0x2201_u64 | (7_u64 << 32)) as i64, 5),
        None
    );
    // What the packer bakes: this compile's stamp, or none without a source.
    let packed = pack_multianewarray_site(0x2201, 7);
    assert_eq!(unpack_multianewarray_site(packed), (0x2201, 7));
    match current_compile_cp_stamp() {
        Some(now) => assert_eq!(multianewarray_site_stamp(packed, now), Some(now)),
        None => assert_eq!(multianewarray_site_stamp(packed, 0), None),
    }
}

/// Interpreter round i1 wave 21, lane L1
/// (`interpreter-L1-other-cp-indexed-jit-helpers-carry-no-cp-stamp-FIXED`): an unbound
/// type-check site's holder is interned as a `cp_holder_word`, so the
/// run-time helper can translate the index of code compiled before its
/// holder's redefinition; the plain accessor still answers the bare id.
#[test]
pub(in crate::tests) fn an_unbound_typecheck_site_carries_the_cp_stamp_of_its_first_compile() {
    const NAME: &str = "com/example/i21l1/Unbound";
    let (p, _) = intern_typecheck_unresolved_in(NAME, 7_101, 13);
    let (word, cp_index) = typecheck_holder_word_for_site(p).expect("an unbound site");
    assert_eq!(cp_index, 13);
    assert_eq!(
        cp_holder_word_parts(word),
        (7_101, current_compile_cp_stamp())
    );
    assert_eq!(typecheck_holder_for_site(p), Some((7_101, 13)));
    // A recompile reuses the pointer, and the stamp it was interned with.
    assert_eq!(intern_typecheck_unresolved_in(NAME, 7_101, 13).0, p);
    assert_eq!(typecheck_holder_word_for_site(p), Some((word, 13)));
    let (resolved, _) = intern_typecheck_target(NAME, Some(7_102));
    assert_eq!(typecheck_holder_word_for_site(resolved), None);
}

// ───────────────────────────────────────────────────────────────────────
// Interpreter round i1 wave 21, lane L1: a running compiled caller does not
// reach a redefined callee's old body
// (`interpreter-L0-a-running-compiled-caller-keeps-calling-a-redefined-callees-old-body-FIXED`).
// ───────────────────────────────────────────────────────────────────────

/// `for (i = 0; i < 10; i++) b(); b(); return;` -- the invoke at pc 8 is
/// inside the loop, the one at pc 17 after it.
///
/// ```text
///  0 iconst_0   1 istore_0   2 iload_0   3 bipush 10   5 if_icmpge +12 (17)
///  8 invokestatic #1   11 iinc 0,1   14 goto -12 (2)
/// 17 invokestatic #1   20 return
/// ```
pub(in crate::tests) const I21_L1_LOOP_THEN_CALL: [u8; 21] = [
    0x03, 0x3b, 0x1a, 0x10, 0x0a, 0xa2, 0x00, 0x0c, 0xb8, 0x00, 0x01, 0x84, 0x00, 0x01, 0xa7, 0xff,
    0xf4, 0xb8, 0x00, 0x01, 0xb1,
];

pub(in crate::tests) fn i21_l1_plan(
    osr_body: bool,
    pc: usize,
    invoke_kind: u8,
    cells: &mut Vec<std::sync::Arc<JitCycleEdgeCell>>,
) -> JitDirectCall {
    let plan = if osr_body {
        retirable_osr_direct_call
    } else {
        retirable_direct_call
    };
    plan(
        &I21_L1_LOOP_THEN_CALL,
        21,
        pc,
        invoke_kind,
        0x7100_0000_0000,
        true,
        0,
        b'V',
        "craton/test/i21/B",
        "b",
        "()V",
        cells,
    )
}

/// A method-entry body gets a retire cell for an in-loop `invokestatic` only;
/// an OSR body gets one for the site after its loop too, which its one
/// long-running activation reaches after a redefinition that happened while
/// the loop ran. The other conditions are unchanged.
/// A retire cell keeps the body its caller baked after a tier-up superseded
/// it, so the scoped redefinition's scan -- which clears the cells holding a
/// body it withdrew, i.e. a published one -- missed it, and a still-running
/// OSR caller kept entering the old bytecode through it. The redefinition now
/// clears a cell holding a body of the class, and one holding a superseded
/// body that calls into the class; an unrelated cell keeps its body.
#[test]
pub(in crate::tests) fn a_scoped_redefinition_clears_retire_cells_holding_superseded_bodies() {
    use std::sync::Arc;
    let cache = JitCache::new();
    let target = "craton/test/i21/Target";
    let helper = "craton/test/i21/Helper";
    let caller = "craton/test/i21/Caller";
    let (redefined, helpers, callers) = (0x2101_u32, 0x2102_u32, 0x2103_u32);
    let redefined_id = cratonvm_types::ClassId::new(redefined);
    let callers_id = cratonvm_types::ClassId::new(callers);
    let entry = |class: &str, method: &str, id: u32| {
        cache
            .get(class, method, "()V", cratonvm_types::ClassId::new(id))
            .expect("published")
            .entry_ptr() as usize
    };
    // A caller's OSR body whose sites reach each pinned entry through a
    // retire cell, as the OSR door plans them.
    fn osr_caller(
        cache: &JitCache,
        caller: &str,
        callers_id: cratonvm_types::ClassId,
        pinned: &[(usize, &str, &str)],
    ) -> Vec<Arc<JitCycleEdgeCell>> {
        let mut osr = i20_l1_body(&[]);
        osr.compiled_via_osr = true;
        let mut cells = Vec::new();
        for &(pinned_entry, class, callee) in pinned {
            let cell = JitCycleEdgeCell::new_retire(pinned_entry, class, callee, "()V", false);
            osr._jit_cycle_cells.push(Arc::clone(&cell));
            osr._direct_callee_entries.push(pinned_entry);
            cells.push(cell);
        }
        cache.put_osr(
            Arc::from(caller),
            Arc::from("main"),
            Arc::from("()V"),
            callers_id,
            osr,
        );
        cells
    }

    i20_l1_put(&cache, target, "own", redefined, i20_l1_body(&[]));
    let old_target = entry(target, "own", redefined);
    let mut calls_target = i20_l1_body(&[]);
    calls_target._direct_callee_entries = vec![old_target];
    i20_l1_put(&cache, helper, "mid", helpers, calls_target);
    let old_helper = entry(helper, "mid", helpers);
    i20_l1_put(&cache, helper, "plain", helpers, i20_l1_body(&[]));
    let unrelated = entry(helper, "plain", helpers);
    let cells = osr_caller(
        &cache,
        caller,
        callers_id,
        &[
            (old_target, target, "own"),
            (old_helper, helper, "mid"),
            (unrelated, helper, "plain"),
        ],
    );
    for (cell, want) in cells.iter().zip([old_target, old_helper, unrelated]) {
        assert_eq!(cell.current_entry() as usize, want, "filled at publication");
    }

    // The tier-ups: new bodies under the same keys; the cells keep the old.
    i20_l1_put(&cache, target, "own", redefined, i20_l1_body(&[]));
    i20_l1_put(&cache, helper, "mid", helpers, i20_l1_body(&[]));
    assert_ne!(entry(target, "own", redefined), old_target);
    assert_eq!(cells[0].current_entry() as usize, old_target);
    assert_eq!(cells[1].current_entry() as usize, old_helper);

    // The caller is kept alive past its withdrawal, as a thread still inside
    // it would keep it: otherwise its own drop withdraws EVERY cell it owns
    // (`withdraw_inline_cache_owners`, review rev-x64 item 6), and the
    // assertions below would read that drop rather than the redefinition's
    // scan. The old `strong_count == 1` gate skipped the drop-time withdrawal
    // only because this test's `cells` vector is an extra strong holder; the
    // cache's registries hold `Weak`s, so production always took it.
    let caller_body = RetainedCode::new(
        cache
            .get_osr(caller, "main", "()V", callers_id)
            .expect("the caller's OSR body is published"),
    );
    let _ = cache.invalidate_for_redefinition(redefined_id, target);
    assert_eq!(
        cells[0].current_entry(),
        0,
        "the superseded body of the redefined class is not reachable any more"
    );
    assert_eq!(
        cells[1].current_entry(),
        0,
        "nor a superseded body that calls into it"
    );
    assert_eq!(
        cells[2].current_entry() as usize,
        unrelated,
        "an unrelated callee keeps its cell"
    );
    assert!(
        cache.get_osr(caller, "main", "()V", callers_id).is_none(),
        "the caller itself is withdrawn, as in wave 20"
    );
    cache.clear_all();
    assert_eq!(cells[2].current_entry(), 0);
    drop(caller_body);
}

/// A MIC or PIC way filled while a method's C1 body was current keeps that
/// body after a tier-up superseded it, and generated code calls a filled slot
/// without asking the helper, where the redefine epoch is checked. The scoped
/// redefinition's scan clears only slots holding a body it withdrew (a
/// published one), so every later call through such a slot ran the class's
/// old bytecode; the full flush cleared every slot. The redefinition now
/// clears each slot whose retained body the class's redefinition makes
/// stale, and leaves the others; the caller itself stays published. With the
/// supersede retarget on (the default since round 11 wave 19), a bound slot
/// already dropped the superseded body at the tier-up, and the redefinition
/// finds it cleared; the end state is the same either way.
#[test]
pub(in crate::tests) fn a_scoped_redefinition_clears_inline_caches_holding_superseded_bodies() {
    let cache = JitCache::new();
    let target = "craton/test/i21/IcTarget";
    let other = "craton/test/i21/IcOther";
    let caller = "craton/test/i21/IcCaller";
    let (redefined, others, callers) = (0x2111_u32, 0x2112_u32, 0x2113_u32);
    let redefined_id = cratonvm_types::ClassId::new(redefined);
    let callers_id = cratonvm_types::ClassId::new(callers);
    let entry = |class: &str, id: u32| {
        cache
            .get(class, "m", "()V", cratonvm_types::ClassId::new(id))
            .expect("published")
            .entry_ptr() as u64
    };

    i20_l1_put(&cache, target, "m", redefined, i20_l1_body(&[]));
    let old_target = entry(target, redefined);
    i20_l1_put(&cache, other, "m", others, i20_l1_body(&[]));
    let unrelated = entry(other, others);
    let mut body = i20_l1_body(&[]);
    body._jit_mic_slots.push(Box::new(JitMICSlot::new()));
    body._jit_mic_slots.push(Box::new(JitMICSlot::new()));
    body._jit_pic_slots.push(Box::new(JitPICSlot::new()));
    i20_l1_put(&cache, caller, "run", callers, body);
    let published = cache
        .get(caller, "run", "()V", callers_id)
        .expect("the caller publishes");
    published._jit_mic_slots[0].update(41, "IcReceiverA", old_target, false, false);
    published._jit_mic_slots[1].update(42, "IcReceiverB", unrelated, false, false);
    published._jit_pic_slots[0].install(43, "IcReceiverC", old_target, false, false);
    published._jit_pic_slots[0].install(44, "IcReceiverD", unrelated, false, false);
    assert_eq!(published._jit_mic_slots[0].cached_entry().0, old_target);
    assert_eq!(
        published._jit_pic_slots[0].lookup(43),
        Some((old_target, false))
    );

    // The tier-up. Since round 11 wave 19 (lane lhm) the supersede itself
    // retires every bound slot that publishes the displaced body
    // (`CompiledMethod::retarget_superseded_inline_caches`, default on); only
    // under its kill switch (`CRATONVM_JIT_IC_SUPERSEDE_RETARGET=0`) does the
    // slot keep the superseded body for the redefinition below to clear.
    // (Interpreter round i1 wave 25, lane L6: this assertion predated the
    // knob and failed on the wave-24 host run, where the knob was on.)
    i20_l1_put(&cache, target, "m", redefined, i20_l1_body(&[]));
    assert_ne!(entry(target, redefined), old_target);
    let kept = if cache.ic_supersede_retarget {
        0
    } else {
        old_target
    };
    assert_eq!(
        published._jit_mic_slots[0].cached_entry().0,
        kept,
        "retarget on: withdrawn by the supersede; off: still the old body"
    );

    let _ = cache.invalidate_for_redefinition(redefined_id, target);
    assert_eq!(
        published._jit_mic_slots[0].cached_entry(),
        (0, false),
        "the MIC is sent back to its helper"
    );
    assert_eq!(
        published._jit_pic_slots[0].lookup(43),
        None,
        "so is the PIC way"
    );
    assert_eq!(
        published._jit_mic_slots[1].cached_entry().0,
        unrelated,
        "an unrelated target stays cached"
    );
    assert_eq!(
        published._jit_pic_slots[0].lookup(44),
        Some((unrelated, false))
    );
    assert!(
        i20_l1_has(&cache, caller, "run", callers),
        "a caller through inline caches only is not withdrawn"
    );
    drop(published);
    cache.clear_all();
}

/// str16: the splice twin of the String pin. A callee that calls
/// `String.hashCode()` (a loop the IR tier cannot expand, in or out of a
/// splice) is kept a call; one whose only String calls are the three field
/// accessors, or one with no String call, may still be spliced; a hit in a
/// NESTED body counts; and without a resolved layout nothing is refused.


/// JIT round 19, lane eager19 (EAGERCOLD18-5): the eager-chain budget kept
/// beside the compile stack is reset by a depth-0 `JitCompileStackGuard::enter`
/// and only there: a nested enter keeps the outer compile's spend.
#[test]
pub(in crate::tests) fn the_compile_stack_chain_budget_resets_only_at_a_top_level_enter() {
    let cached = |name: &str| {
        CachedBytecodeMethod::from_parts(cratonvm_jit_api::CachedMethodParts {
            declaring_class_id: cratonvm_types::ClassId::new(3),
            class_name: std::sync::Arc::from("r19/Budget"),
            method_name: std::sync::Arc::from(name),
            method_descriptor: std::sync::Arc::from("()V"),
            source_file: None,
            code: std::sync::Arc::from([0xb1u8, 0, 0].as_slice()),
            exception_table: std::sync::Arc::from(Vec::new().as_slice()),
            max_stack: 0,
            max_locals: 0,
            num_params: 0,
            is_synchronized: false,
            is_static: true,
        })
    };
    let (outer, inner) = (cached("outer"), cached("inner"));
    {
        let _top = JitCompileStackGuard::enter(&outer);
        assert_eq!(jit_compile_chain_budget_spent(), 0);
        assert!(jit_compile_chain_budget_try_spend(2));
        {
            let _nested = JitCompileStackGuard::enter(&inner);
            assert_eq!(
                jit_compile_chain_budget_spent(),
                1,
                "a nested enter keeps it"
            );
            assert!(jit_compile_chain_budget_try_spend(2));
            assert!(!jit_compile_chain_budget_try_spend(2), "spent");
        }
        assert_eq!(jit_compile_chain_budget_spent(), 2);
    }
    // Outside any compile a spend is a top-level request of its own.
    assert!(jit_compile_chain_budget_try_spend(2));
    assert_eq!(jit_compile_chain_budget_spent(), 1);
    let _next = JitCompileStackGuard::enter(&outer);
    assert_eq!(
        jit_compile_chain_budget_spent(),
        0,
        "the next top-level compile"
    );
}


/// A clone of an inline site shares its nested rows (the resolver memo clones
/// an answer per repeat); editing a shared list copies it and leaves the other
/// holder's rows alone.
#[test]
pub(in crate::tests) fn a_cloned_nested_list_is_shared_until_it_is_edited() {
    let row = |pc| NestedInlineSite {
        callee_pc: pc,
        ..Default::default()
    };
    let mut a = InlineSite::default();
    a.nested_sites.push(row(1));
    let b = a.clone();
    assert!(std::ptr::eq(&*a.nested_sites, &*b.nested_sites), "shared");
    a.nested_sites.push(row(2));
    a.nested_sites[0].callee_pc = 9;
    assert_eq!(a.nested_sites.len(), 2);
    assert_eq!((b.nested_sites.len(), b.nested_sites[0].callee_pc), (1, 1));
    assert_ne!(a, b);
}

#[test]
fn lambda321_5_root_site_arg_classes_resolves_indy_arg() {
    // 0: aload_0; 1: invokedynamic #1; 6: invokeinterface #2 2 0; 11: return
    let code = [
        0x2a, 0xba, 0x00, 0x01, 0x00, 0x00, 0xb9, 0x00, 0x02, 0x02, 0x00, 0xb1,
    ];
    let builder = ir::IrBuilder::new(1, 2);
    let new_locals = HashMap::new();
    let indy_classes = HashMap::from([(1usize, 42u32)]);
    let indy_shapes = HashMap::from([(1usize, (0usize, b'L'))]);
    let classes = ir_root_site_arg_classes(
        &builder,
        None,
        &new_locals,
        &indy_classes,
        &indy_shapes,
        &[],
        None,
        &code,
        code.len(),
        &[],
        6,
        0xb9,
        "(Ljava/util/function/Consumer;)V",
    );
    assert_eq!(classes, vec![None, Some(42)]);
}
