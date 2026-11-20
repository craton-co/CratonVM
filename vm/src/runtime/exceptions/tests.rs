use super::*;
use crate::config::VmConfig;
use crate::vm::Vm;

fn test_vm() -> Vm {
    Vm::new(VmConfig::default())
}

/// Interpreter round i3 wave 79, lane L3: a hidden class's `/0x<hex>`
/// tail survives the dotting (HotSpot's `external_name()`); an ordinary
/// class, or a hidden one without a hex tail, is dotted throughout.
#[test]
fn i79_external_name_keeps_a_hidden_classs_hex_tail() {
    assert_eq!(
        external_name_keeping_hidden_tail("p/q/Foo$Bad/0x0000020001001400", true),
        "p.q.Foo$Bad/0x0000020001001400"
    );
    assert_eq!(
        external_name_keeping_hidden_tail("p/q/Foo$Bad/0x1", false),
        "p.q.Foo$Bad.0x1"
    );
    assert_eq!(external_name_keeping_hidden_tail("p/Foo", true), "p.Foo");
    assert_eq!(
        external_name_keeping_hidden_tail("p/Foo/0xzz", true),
        "p.Foo.0xzz"
    );
    assert_eq!(
        external_name_keeping_hidden_tail("Foo/0x1f", true),
        "Foo/0x1f"
    );
}

/// Interpreter round i4 wave 84, lane L2: the `ArrayStoreException` funnel
/// keeps a hidden element class's `/0x<hex>` tail, bare and as an array's
/// bottom element, and leaves every ordinary name as before.
#[test]
fn i84_l2_array_store_message_keeps_a_hidden_classs_hex_tail() {
    assert_eq!(
        hotspot_external_name("p/Main$$Lambda/0x0000020001001400"),
        "p.Main$$Lambda/0x0000020001001400"
    );
    assert_eq!(
        hotspot_external_name("[Lp/Main$$Lambda/0x1a;"),
        "[Lp.Main$$Lambda/0x1a;"
    );
    assert_eq!(
        hotspot_external_name("java/lang/Integer"),
        "java.lang.Integer"
    );
    assert_eq!(
        hotspot_external_name("[Ljava/lang/Integer;"),
        "[Ljava.lang.Integer;"
    );
    assert_eq!(hotspot_external_name("[[I"), "[[I");
}

/// JIT round 21 wave 22, lane hidden-resid (item 7 of
/// `r21-review-w15-native-findings-FIXED-20261020.md`,
/// `CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`): with the class at hand its own
/// hidden flag decides the `/0x<hex>` tail; an ordinary `p/0x1F` is
/// dotted as `Klass::external_name()` dots it. `=0` and arrays keep the
/// shape rule.
#[test]
fn a_held_class_is_named_by_its_hidden_flag_not_its_name_shape() {
    assert_eq!(hotspot_external_name_by_flag("p/0x1F", false, true), "p.0x1F");
    assert_eq!(hotspot_external_name_by_flag("p/0x1F", true, true), "p/0x1F");
    assert_eq!(
        hotspot_external_name_by_flag("p/Main$$Lambda/0x1a", true, true),
        "p.Main$$Lambda/0x1a"
    );
    assert_eq!(hotspot_external_name_by_flag("java/lang/Integer", false, true), "java.lang.Integer");
    // `=0`: the shape judgement, as before.
    assert_eq!(hotspot_external_name_by_flag("p/0x1F", false, false), "p/0x1F");
    // An array keeps the shape rule (its own flag is not its element's).
    assert_eq!(
        hotspot_external_name_by_flag("[Lp/Main$$Lambda/0x1a;", false, true),
        "[Lp.Main$$Lambda/0x1a;"
    );
}

/// JIT round 21 wave 24, lane names (item 5 of
/// `r21-review-w23-findings-FIXED-20261020.md`): with the bottom class at hand
/// its hidden flag decides an array's tail too. Failed before: the
/// checkcast and `aastore` texts printed `[Lfz9/0x2A;` (shape) for an
/// array of an ORDINARY class, where HotSpot prints `[Lfz9.0x2A;`.
#[test]
fn an_array_is_named_by_its_bottom_class_hidden_flag() {
    assert_eq!(hotspot_external_name_by_bottom_flag("[Lfz9/0x2A;", false), "[Lfz9.0x2A;");
    assert_eq!(hotspot_external_name_by_bottom_flag("[[Lfz9/0x2A;", true), "[[Lfz9/0x2A;");
    assert_eq!(
        hotspot_external_name_by_bottom_flag("[Lp/Main$$Lambda/0x1a;", true),
        "[Lp.Main$$Lambda/0x1a;"
    );
    assert_eq!(hotspot_external_name_by_bottom_flag("fz9/0x1F", false), "fz9.0x1F");
    assert_eq!(hotspot_external_name_by_bottom_flag("fz9/H/0x1F", true), "fz9.H/0x1F");
    assert_eq!(hotspot_external_name_by_bottom_flag("[[I", false), "[[I");
    // An unknown bottom keeps the site's own (shape) text.
    assert_eq!(
        external_name_by_bottom_class_or("[Lfz9/0x2A;", None, hotspot_external_name),
        hotspot_external_name("[Lfz9/0x2A;")
    );
    // The funnel's shape rewrite leaves both renderings unchanged.
    assert_eq!(hotspot_external_name("[Lfz9.H/0x2a;"), "[Lfz9.H/0x2a;");
    assert_eq!(hotspot_external_name("fz9.H/0x2a"), "fz9.H/0x2a");
}

/// JIT round 21 wave 25, lane hexname (item 5 of
/// `r21-review-w23-findings-FIXED-20261020.md`): the compiled `checkcast`
/// helper's own fallback (a bare `replace('/', ".")`) dotted a hidden
/// bottom's tail (`[Lp.H.0x1a;`, HotSpot `[Lp.H/0x1a;`); by the bottom
/// flag both kinds are right, and the funnel still finds the ordinary
/// receiver's class by id (its dotted name spells the internal one), so
/// the module/loader clause is unchanged.
/// JIT round 21 wave 26, lane hexname (`CRATONVM_CAST_TEXT_BY_RESOLVED_CLASS`):
/// a target id the store does not hold names nothing, so the site keeps
/// its pool-name rendering; an absent operand id gives no array clause.
#[test]
fn an_unheld_cast_target_keeps_the_pool_name() {
    let vm = test_vm();
    let absent = ClassId::new(u32::MAX - 9);
    assert_eq!(resolved_hidden_cast_target_name(&vm.shared, absent, "p/Hd"), None);
    assert_eq!(
        cast_target_display(&vm.shared, Some(absent), "p/Hd", |n| n.replace('/', ".")),
        "p.Hd"
    );
    assert_eq!(cast_target_display(&vm.shared, None, "[Lp/Hd;", |n| n.replace('/', ".")), "[Lp.Hd;");
    assert!(klass_origin_by_id(&vm.shared, absent, "[Lp.Hd/0x1;", true).is_none());
    assert!(klass_origin_by_id(&vm.shared, absent, "[I", true).is_none());
}

#[test]
fn the_compiled_cast_texts_are_named_by_the_bottom_class() {
    let bare = |d: &str| d.replace('/', ".");
    assert_eq!(bare("[Lp/H/0x1a;"), "[Lp.H.0x1a;");
    assert_eq!(hotspot_external_name_by_bottom_flag("[Lp/H/0x1a;", true), "[Lp.H/0x1a;");
    assert_eq!(hotspot_external_name_by_bottom_flag("[Lfz9/0x2A;", false), "[Lfz9.0x2A;");
    assert_eq!(hotspot_external_name_by_bottom_flag("p/H/0x1a", true), "p.H/0x1a");
    assert_eq!(external_name_by_bottom_class_or("p/H/0x1a", None, bare), "p.H.0x1a");
    assert!(internal_name_spells_display("fz9/0x2A", "fz9.0x2A"));
    assert_eq!(
        split_cast_operands("class fz9.0x2A cannot be cast to class java.lang.String"),
        Some(("fz9.0x2A", "java.lang.String"))
    );
}

/// Wave 24, lane names (item 6 of `r21-review-w23-findings-FIXED-20261020.md`):
/// an id the store does not hold answers `None`, which keeps the shape
/// rule in `raise_could_not_initialize_class_for` (it used to read as
/// "ordinary" and dot `p/Main$$Lambda/0x1a` whole), and no bottom flag.
#[test]
fn an_id_the_store_does_not_hold_has_no_hidden_answer() {
    let vm = test_vm();
    let absent = ClassId::new(u32::MAX - 7);
    assert_eq!(class_hidden_in_store(&vm.shared, absent), None);
    assert_eq!(bottom_class_hidden(&vm.shared, absent, "p/Main$$Lambda/0x1a"), None);
}

/// `throwable_own_field_index` answers from THIS VM's `ThrowableLayoutCache`
/// with the slot the native context would resolve, and a second VM's slot
/// is not consulted. Also pins that a name outside Throwable's six is
/// refused rather than walked.
#[test]
fn throwable_own_field_slots_come_from_this_vms_layout_cache() {
    use std::sync::atomic::Ordering::Relaxed;
    let first = test_vm();
    let second = test_vm();
    let Some(cause) = throwable_own_field_index(&first.shared, "cause") else {
        // Stripped VM (no JDK on the test classpath): no Throwable layout.
        return;
    };
    let expected = {
        let cm = first.shared.classes.class_manager.read();
        let tid = cm
            .get_loaded_class_id("java/lang/Throwable")
            .expect("resolved just above");
        crate::vm::vm_exec::resolve_field_index_in_hierarchy(tid, "cause", &cm.class_store)
    };
    assert_eq!(
        Some(cause),
        expected,
        "must match the native context's resolution"
    );

    second
        .shared
        .threads
        .throwable_layout_cache
        .cause
        .store(cause + 1000, Relaxed);
    assert_eq!(
        throwable_own_field_index(&first.shared, "cause"),
        Some(cause),
        "another VM's slot must not leak into this one"
    );
    assert_eq!(
        throwable_own_field_index(&second.shared, "cause"),
        Some(cause + 1000),
        "each VM reads its own slot"
    );
    assert_eq!(
        throwable_own_field_index(&first.shared, "notAThrowableField"),
        None
    );
}

/// `throwable_suppressed_sentinel`'s `(ClassId, static index)` pair lives
/// in the per-VM layout cache. It was a pair of process-global statics, so
/// a second VM in the process read the first VM's ids.
#[test]
fn suppressed_sentinel_slots_are_per_vm() {
    use crate::vm::realms::thread_realm::{
        UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID, UNRESOLVED_THROWABLE_FIELD_INDEX,
    };
    use std::sync::atomic::Ordering::Relaxed;
    let first = test_vm();
    let second = test_vm();
    if throwable_suppressed_sentinel(&first.shared).is_none() {
        // Stripped VM, or `Throwable.<clinit>` has not run: nothing cached.
        return;
    }
    let cache = &first.shared.threads.throwable_layout_cache;
    let cid = cache.throwable_class_id.load(Relaxed);
    let idx = cache.suppressed_sentinel_static_index.load(Relaxed);
    assert_ne!(
        cid, UNRESOLVED_STACK_TRACE_ELEMENT_CLASS_ID,
        "resolved into THIS VM's cache"
    );
    assert_ne!(
        idx, UNRESOLVED_THROWABLE_FIELD_INDEX,
        "resolved into THIS VM's cache"
    );

    // Poison the second VM's pair; the first VM's answer must not move.
    let other = &second.shared.threads.throwable_layout_cache;
    other.throwable_class_id.store(cid, Relaxed);
    other
        .suppressed_sentinel_static_index
        .store(idx + 1000, Relaxed);
    assert!(throwable_suppressed_sentinel(&first.shared).is_some());
    assert_eq!(cache.suppressed_sentinel_static_index.load(Relaxed), idx);
}

/// Round 9 wave 8 (vmrt8): a failing cast pays for the reclamation-ring
/// forensics once per `(receiver class, target)` pair, not per refusal
/// (that was a ~5 ms `ClassCastException`), and a zero header always
/// pays. Class ids here are far outside anything a test VM defines, so
/// the process-wide pair set cannot collide with another test.
#[test]
fn cast_refusal_forensics_run_once_per_pair_and_always_for_a_zero_header() {
    let cid = 0x7ACE_0001;
    assert!(cast_refusal_forensics_admitted(cid, "java.lang.Integer"));
    assert!(
        !cast_refusal_forensics_admitted(cid, "java.lang.Integer"),
        "a repeat of the same refusal is a type-test idiom and must not rescan"
    );
    assert!(
        !cast_refusal_forensics_admitted(cid, "java/lang/Integer"),
        "the JIT's slashed spelling is the same pair as the interpreter's dotted one"
    );
    assert!(
        cast_refusal_forensics_admitted(cid, "java.lang.Long"),
        "a new target is a new pair"
    );
    assert!(
        cast_refusal_forensics_admitted(0x7ACE_0002, "java.lang.Integer"),
        "a new receiver class is a new pair"
    );
    for _ in 0..3 {
        assert!(
            cast_refusal_forensics_admitted(0, "java.lang.Integer"),
            "a ClassId(0) receiver is the reclaimed-span signature: always admitted"
        );
    }
}

/// Round 15 wave 5 (lane throw1): the lock-free pair table behind
/// `cast_refusal_forensics_admitted` admits each key once, probes past a
/// taken slot, and admits nothing new once the cap is recorded. A local
/// table, so the process-wide one is not touched.
#[test]
fn cast_forensics_table_admits_each_key_once_and_stops_at_the_cap() {
    let seen = Box::new(CastForensicsSeen::new());
    assert!(seen.first_sighting(7));
    assert!(!seen.first_sighting(7), "a repeat is not admitted");
    // Widening: the slot count fits u64.
    let same_slot = 7 + CAST_FORENSICS_SLOTS as u64;
    assert!(
        seen.first_sighting(same_slot),
        "a key on a taken slot probes on"
    );
    assert!(!seen.first_sighting(same_slot));
    for k in 0..(CAST_FORENSICS_PAIR_CAP as u64) {
        let _ = seen.first_sighting(1_000_000 + k);
    }
    assert!(
        !seen.first_sighting(u64::MAX - 3),
        "past the cap nothing new is admitted"
    );
    assert!(!seen.first_sighting(7));
    assert_ne!(cast_forensics_key(1, "a/B"), 0);
    assert_eq!(
        cast_forensics_key(5, "java/lang/Integer"),
        cast_forensics_key(5, "java.lang.Integer"),
        "the slashed and dotted spellings are one pair"
    );
    assert_ne!(
        cast_forensics_key(5, "java.lang.Integer"),
        cast_forensics_key(6, "java.lang.Integer")
    );
}

/// Round 15 wave 5 (lane throw1): a VM-built throwable whose constructor
/// is plain bytecode reaches the ordinary tail on its first, by-name
/// construction, which fills the callback memo for exactly
/// `(class, "<init>", descriptor)`; the second construction is served from
/// that memo and still RUNS the constructor's bytecode. The class's
/// `<init>(String)` is `aconst_null; athrow`, so "the constructor ran" is
/// observable: JNI `ThrowNew` answers the `NullPointerException` it threw.
/// (The JDK throwables of the test VM are `--compatible` shadows, natives
/// the memo never records, which is why a test class is used.)
#[test]
fn a_vm_built_throwable_constructor_is_served_from_the_callback_memo() {
    use crate::runtime::native_callee_memo;
    const NAME: &str = "cratonvm/test/Throw1CtorMemo";
    const DESC: &str = "(Ljava/lang/String;)V";
    if !throw1_switches().ctor_memo || !crate::runtime::env_cache::native_callback_memo() {
        // A kill switch is set in this process: nothing to prove.
        return;
    }
    let mut vm = test_vm();
    // Stripped VM: without a buildable NPE the constructor's `athrow`
    // cannot produce the exception the second half observes.
    let Ok(probe) = create_exception_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/NullPointerException",
        None,
    ) else {
        return;
    };
    let npe_class = vm.shared.mem.heap.class_id_of(probe);
    let ctor = jni_test_ctor(DESC, 2, vec![0x01, 0xbf]);
    let class_id = add_jni_throw_new_test_class(&vm, NAME, vec![ctor], true);
    assert_eq!(
        native_callee_memo::lookup(&vm.shared, class_id, "<init>", DESC),
        None,
        "nothing is memoised before the first construction"
    );
    // Interpreter round i4 wave 96 (lane L7): the memo's default stamp
    // carries the PROCESS-wide `class_origin_epoch` (and a store-epoch
    // slot test VMs may share), which every concurrently running test
    // that defines or promotes a class moves (wave 95 added several:
    // the narrow class-memo promotion tests, the define-census scope).
    // A bump between the fill and the probe retires the entry on
    // purpose, so in the full lib run the first probe could miss
    // (measured: alone -> ok, full run -> None). A miss re-arms and
    // refills on the next by-name construction, so retry until a quiet
    // window; a fill that is really skipped never hits.
    let mut filled = false;
    for _ in 0..64 {
        let first = create_exception_object_for_jni_throw_new(
            &vm.shared,
            &mut vm.main_thread,
            class_id,
            NAME,
            Some("m"),
        )
        .expect("the constructor's exception is a result");
        assert_eq!(vm.shared.mem.heap.class_id_of(first), npe_class);
        let memo = native_callee_memo::lookup(&vm.shared, class_id, "<init>", DESC);
        if memo == Some(class_id) {
            filled = true;
            break;
        }
    }
    assert!(
        filled,
        "the by-name construction reached the ordinary tail and filled the memo"
    );
    assert_eq!(
        native_callee_memo::lookup(&vm.shared, class_id, "<init>", "()V"),
        None,
        "only the descriptor that ran is memoised"
    );
    let pins = vm.main_thread.native_pin_roots.len();
    let second = create_exception_object_for_jni_throw_new(
        &vm.shared,
        &mut vm.main_thread,
        class_id,
        NAME,
        Some("m"),
    )
    .expect("the constructor's exception is a result");
    assert_eq!(
        vm.shared.mem.heap.class_id_of(second),
        npe_class,
        "the memoised construction ran the constructor's bytecode"
    );
    assert_eq!(vm.main_thread.native_pin_roots.len(), pins, "pins released");
}

/// A callback memo for an exact receiver remains valid across unrelated
/// class definitions and a duplicate-name mapping. These global epochs do
/// not change the receiver's already-defined hierarchy or dispatch.
#[test]
fn edge_epoch_callback_memo_survives_an_unrelated_class_definition() {
    use crate::runtime::native_callee_memo;

    let vm = Vm::new(VmConfig {
        native_callback_memo_edge_epoch: true,
        ..VmConfig::default()
    });
    let receiver =
        add_jni_throw_new_test_class(&vm, "cratonvm/test/CallbackMemoReceiver", vec![], false);
    native_callee_memo::fill(&vm.shared, receiver, "run", "()V", receiver);
    assert_eq!(
        native_callee_memo::lookup(&vm.shared, receiver, "run", "()V"),
        Some(receiver)
    );

    let epochs = cratonvm_classloading::store_epochs(vm.shared.jit.class_layout_domain);
    let definition_epoch = epochs.class_definition_epoch();
    let unrelated =
        add_jni_throw_new_test_class(&vm, "cratonvm/test/CallbackMemoUnrelated", vec![], false);
    assert!(
        epochs.class_definition_epoch() > definition_epoch,
        "the fixture changes the global class-definition epoch"
    );
    let name_epoch = epochs.class_name_generation();
    vm.shared.classes.class_manager_write().register_class_name(
        crate::classloading::ClassLoaderId::Extension,
        "cratonvm/test/CallbackMemoReceiver",
        unrelated,
    );
    assert!(
        epochs.class_name_generation() > name_epoch,
        "the fixture changes the global name-resolution epoch"
    );
    assert_eq!(
        native_callee_memo::lookup(&vm.shared, receiver, "run", "()V"),
        Some(receiver),
        "a new class cannot alter dispatch for the receiver ClassId"
    );
}

// -----------------------------------------------------------------------
// `klass_origin`'s descriptor parse (W7-37 §B8.1 / §B11.1)
// -----------------------------------------------------------------------

/// The operand spellings are HotSpot's, taken verbatim from a Temurin
/// 25.0.3.9 run of the `CastMsgs` probe (see
/// `docs/feature-designs/real-jdk-mode.md`
/// §1). They are *dotted descriptors*, not JEP 358 source form: HotSpot
/// prints `class [I cannot be cast to class [Ljava.lang.String;`, never
/// `int[]` / `String[]`, for a cast refusal.
///
/// The bug this pins: `klass_origin` used to look the *array class* up in
/// the definition index and only parse the `[` prefix afterwards, so the
/// array arm ran only when something unrelated had already defined that
/// exact array class — non-deterministically, per program. Parsing first
/// removes the dependence on incidental index state, and this test is the
/// part of that which needs no VM.
#[test]
fn origin_lookup_resolves_the_bottom_component_not_the_array() {
    // Plain classes resolve themselves — including one whose name starts
    // with the descriptor tag `L`, which a naive `strip_prefix('L')` eats.
    assert_eq!(
        origin_lookup("java.lang.String"),
        OriginLookup::Plain("java.lang.String")
    );
    assert_eq!(origin_lookup("Long"), OriginLookup::Plain("Long"));
    assert_eq!(
        origin_lookup("CastProbe"),
        OriginLookup::Plain("CastProbe"),
        "a default-package application class has no separator at all"
    );

    // Reference arrays resolve the BOTTOM component, at every depth: HotSpot
    // walks `ObjArrayKlass::bottom_klass`, so `[[Ljava.lang.String;` and
    // `[Ljava.lang.String;` print the same module/loader clause.
    assert_eq!(
        origin_lookup("[Ljava.lang.String;"),
        OriginLookup::Component("java.lang.String")
    );
    assert_eq!(
        origin_lookup("[[Ljava.lang.String;"),
        OriginLookup::Component("java.lang.String")
    );
    assert_eq!(
        origin_lookup("[LCastProbe;"),
        OriginLookup::Component("CastProbe"),
        "measured: `[LCastProbe; is in unnamed module of loader 'app'` — an \
         array takes the COMPONENT's defining loader (JVMS §5.3.3 step 2), \
         so the component is the class that has to be resolved"
    );

    // Primitive arrays need no lookup at any depth. `[[I` is an
    // objArrayKlass whose bottom klass is the typeArrayKlass `[I`, and
    // HotSpot's `else` arm hard-codes java.base for it — measured:
    // `([[I and java.lang.String are in module java.base of loader
    // 'bootstrap')`.
    for name in ["[I", "[[I", "[J", "[[[D", "[Z"] {
        assert_eq!(
            origin_lookup(name),
            OriginLookup::PrimitiveArray,
            "{name} must answer java.base/bootstrap without a lookup"
        );
    }
}

/// `split_cast_operands` has to leave an already-rewritten message alone,
/// or the funnel would nest parentheticals every time a message passed
/// through it twice. Array operands are the case worth pinning because
/// they contain `;` and `[` and nothing else in the splitter looks at
/// those.
#[test]
fn split_cast_operands_handles_array_descriptors_and_is_idempotent() {
    assert_eq!(
        split_cast_operands("class [I cannot be cast to class [Ljava.lang.String;"),
        Some(("[I", "[Ljava.lang.String;"))
    );
    assert_eq!(
        split_cast_operands("[I cannot be cast to [Ljava.lang.String;"),
        Some(("[I", "[Ljava.lang.String;")),
        "the bare pre-rewrite wording must split too — that is the shape \
         the interpreter's checkcast raise site produces"
    );
    assert_eq!(
        split_cast_operands(
            "class [I cannot be cast to class [Ljava.lang.String; ([I and \
             [Ljava.lang.String; are in module java.base of loader 'bootstrap')"
        ),
        None,
        "an already-rewritten message must NOT split again"
    );
}

// -----------------------------------------------------------------------
// Cached debug-flag helpers (throw hot path)
// -----------------------------------------------------------------------


// -----------------------------------------------------------------------
// Stack-trace capture: no duplicate walk, no lost fidelity
// -----------------------------------------------------------------------

/// The skip guard must fail **closed** when the thread has no Java frames.
/// A freshly built VM's main thread is in exactly that state, and it is the
/// state every bootstrap-era throwable is constructed in — so the explicit
/// `fillInStackTrace` still runs there and the legacy behaviour is bit-for-bit
/// preserved.
#[test]
fn duplicate_trace_guard_is_closed_with_no_frames() {
    let mut vm = test_vm();
    let Ok(obj) = create_exception_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/IllegalStateException",
        Some("probe"),
    ) else {
        // Stripped VM (no JDK on the test classpath): nothing to probe with.
        return;
    };
    // Any heap object works as the probe: with zero frames the guard must
    // short-circuit to `false` before it ever looks at a field.
    if vm.main_thread.frames.is_empty() {
        assert!(
            !trace_already_captured_at_current_depth(&vm.shared, &vm.main_thread, obj),
            "guard must not skip fillInStackTrace when there are no frames"
        );
    }
}

/// Round 12 wave 7 (lane compat): step 4 leaves the trace to an
/// application override of `fillInStackTrace()` (the "quiet exception"
/// idiom), and keeps its backstop for the JDK's own overrides and for a
/// class with none. Mode-independent: the walk reads no compatibility mode.
#[test]
fn step_four_defers_only_to_an_application_fill_override() {
    use std::collections::HashMap;
    // (is_throwable, built_in, declares_fill, superclass)
    let classes: HashMap<u32, (bool, bool, bool, Option<u32>)> = HashMap::from([
        (1, (true, true, true, None)),        // java/lang/Throwable
        (2, (false, true, false, Some(1))),   // java/lang/RuntimeException
        (3, (false, true, true, Some(2))),    // java/lang/NullPointerException
        (4, (false, false, true, Some(2))),   // app Quiet extends RuntimeException
        (5, (false, false, false, Some(4))),  // app LoudQuiet extends Quiet
        (6, (false, false, false, Some(2))),  // app Plain extends RuntimeException
        (7, (false, false, false, Some(3))),  // app MyNpe extends NullPointerException
        (8, (false, false, false, Some(99))), // superclass not in the store
    ]);
    let walk = |start: u32| {
        first_application_fill_override(ClassId::new(start), |cid| {
            let &(is_throwable, built_in, declares_fill, sup) = classes.get(&cid.as_u32())?;
            Some(FillOverrideStep {
                is_throwable,
                built_in,
                declares_fill,
                superclass: sup.map(ClassId::new),
            })
        })
    };
    assert!(walk(4), "an application override decides");
    assert!(walk(5), "and so does an inherited one");
    assert!(
        !walk(1) && !walk(2) && !walk(6),
        "no override: the backstop stays"
    );
    assert!(!walk(3) && !walk(7), "a JDK override keeps the backstop");
    assert!(!walk(8), "an unknown class keeps the backstop");
}

/// The core fidelity invariant behind dropping the redundant
/// `fillInStackTrace` call: whenever the constructor's capture actually ran
/// (proved by the `backtrace = this` self-marker that
/// `capture_throwable_trace` parks there), a trace **must** be registered in
/// the VM-wide store keyed by the throwable's identity hash.
///
/// If that implication ever breaks, `create_exception_object` could skip the
/// explicit `fillInStackTrace` and leave the throwable with an empty
/// `getStackTrace()` — the precise regression this change must never cause.
#[test]
fn constructor_capture_marker_implies_a_registered_trace() {
    let mut vm = test_vm();
    let Ok(obj) = create_exception_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/IllegalStateException",
        Some("boom"),
    ) else {
        // Stripped VM (no JDK on the test classpath): nothing to assert.
        return;
    };
    let Some(bt_idx) = instance_field_index_by_name(&vm.shared, obj, "backtrace") else {
        return;
    };
    if vm.shared.mem.heap.get_field(obj, bt_idx) != Value::Object(Some(obj)) {
        // Our capture never ran for this class in this configuration.
        return;
    }
    assert!(
        vm.shared.throwable_stack_trace_for(obj).is_some(),
        "backtrace self-marker is set but no trace was registered"
    );
}

/// Nested / rethrown shape: building a second throwable while the first is
/// still live must not evict or overwrite the first one's trace. The store
/// is keyed by the throwable, so both entries have to coexist — otherwise a
/// `catch (E e) { throw new F(e); }` chain would print the wrong frames for
/// the cause.
#[test]
fn nested_throwables_keep_independent_traces() {
    let mut vm = test_vm();
    let Ok(inner) = create_exception_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/IllegalStateException",
        Some("inner"),
    ) else {
        return;
    };
    let Some(inner_trace) = vm.shared.throwable_stack_trace_for(inner) else {
        return;
    };

    // Pin `inner` so building the outer throwable cannot collect it, and
    // re-read it afterwards in case the collection moved it.
    vm.main_thread.native_pin_roots.push(inner);
    let outer = create_exception_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/IllegalArgumentException",
        Some("outer"),
    );
    let inner = vm.main_thread.native_pin_roots.pop().expect("pinned inner");

    let Ok(outer) = outer else { return };

    assert!(
        vm.shared.throwable_stack_trace_for(outer).is_some(),
        "outer throwable lost its trace"
    );
    assert_eq!(
        vm.shared.throwable_stack_trace_len_for(inner),
        Some(inner_trace.len()),
        "constructing the outer throwable clobbered the inner one's trace"
    );
}

// -----------------------------------------------------------------------
// NotImplemented stays as InternalError
// -----------------------------------------------------------------------

#[test]
fn throw_not_implemented_stays_internal() {
    let mut vm = test_vm();
    let error = RuntimeError::NotImplemented {
        feature: "test".to_string(),
    };
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(result, MethodCallFailed::InternalError(_)));
}

#[test]
fn linkage_no_such_method_error_is_throwable_or_falls_back() {
    let mut vm = test_vm();
    let error = LinkageError::NoSuchMethodError {
        class_name: "org/junit/runner/Description".to_string(),
        method_name: "createSuiteDescription".to_string(),
        method_descriptor: "(Ljava/lang/String;)Lorg/junit/runner/Description;".to_string(),
    };
    let result = throw_linkage_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::ExceptionThrown(_)
            | MethodCallFailed::InternalError(VmError::Linkage(
                LinkageError::NoSuchMethodError { .. }
            ))
    ));
}

// -----------------------------------------------------------------------
// All RuntimeError variants produce a result (fallback to InternalError
// when the exception class can't be loaded without rt.jar, which is fine)
// -----------------------------------------------------------------------

#[test]
fn throw_null_pointer_exception() {
    let mut vm = test_vm();
    let error = RuntimeError::NullPointerException {
        message: Some("test NPE".to_string()),
    };
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    // Without rt.jar, falls back to InternalError — that's expected
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn throw_arithmetic_exception() {
    let mut vm = test_vm();
    let error = RuntimeError::ArithmeticException {
        message: "/ by zero".to_string(),
    };
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn throw_array_index_out_of_bounds() {
    let mut vm = test_vm();
    let error = RuntimeError::aioobe_index_only(42);
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn throw_class_cast_exception() {
    let mut vm = test_vm();
    let error = RuntimeError::ClassCastException {
        message: "String cannot be cast to Integer".to_string(),
    };
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn throw_negative_array_size() {
    let mut vm = test_vm();
    let error = RuntimeError::NegativeArraySizeException { size: -1 };
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn throw_stack_overflow() {
    let mut vm = test_vm();
    let error = RuntimeError::StackOverflowError;
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn throw_out_of_memory() {
    let mut vm = test_vm();
    let error = RuntimeError::OutOfMemoryError {
        message: "heap exhausted".to_string(),
    };
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn throw_class_not_found() {
    let mut vm = test_vm();
    let error = RuntimeError::ClassNotFoundException {
        class_name: "com/example/Missing".to_string(),
    };
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn throw_unsatisfied_link() {
    let mut vm = test_vm();
    let error = RuntimeError::UnsatisfiedLinkError {
        message: "native method not found".to_string(),
    };
    // This variant maps to java/lang/UnsatisfiedLinkError.
    // Without rt.jar, falls back to InternalError.
    // The class may not have enough fields for the message, so we just
    // verify it doesn't panic by catching the result.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        throw_runtime_error(&vm.shared, &mut vm.main_thread, error)
    }));
    // Either succeeds with a MethodCallFailed, or panics due to field count mismatch
    // (which is a known limitation without rt.jar). Both are acceptable.
    drop(result);
}

#[test]
fn throw_number_format() {
    let mut vm = test_vm();
    let error = RuntimeError::NumberFormatException {
        message: "For input string: \"abc\"".to_string(),
    };
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn throw_illegal_argument() {
    let mut vm = test_vm();
    let error = RuntimeError::IllegalArgumentException {
        message: "bad arg".to_string(),
    };
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn throw_io_exception() {
    let mut vm = test_vm();
    let error = RuntimeError::IOException {
        message: "read error".to_string(),
    };
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn throw_runtime_error_releases_exception_construction_pins() {
    let mut vm = test_vm();
    let pin_base = vm.main_thread.native_pin_roots.len();
    let error = RuntimeError::IOException {
        message: "read error".to_string(),
    };
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
    assert_eq!(
        vm.main_thread.native_pin_roots.len(),
        pin_base,
        "exception construction pins must not leak after throw_runtime_error returns"
    );
}

#[test]
fn throw_concurrent_modification() {
    let mut vm = test_vm();
    let error = RuntimeError::ConcurrentModificationException;
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn throw_interrupted() {
    let mut vm = test_vm();
    let error = RuntimeError::InterruptedException;
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn throw_unsupported_operation() {
    let mut vm = test_vm();
    let error = RuntimeError::UnsupportedOperationException {
        message: "not supported".to_string(),
    };
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

// ── Exception creation and formatting tests ───────────────────────

#[test]
fn runtime_error_npe_with_none_message() {
    let mut vm = test_vm();
    let error = RuntimeError::NullPointerException { message: None };
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn runtime_error_display_formatting() {
    // Verify the error variants carry their messages correctly
    let error = RuntimeError::ArithmeticException {
        message: "/ by zero".to_string(),
    };
    let formatted = format!("{error}");
    assert!(formatted.contains("by zero") || formatted.contains("Arithmetic"));
}

#[test]
fn runtime_error_array_index_carries_index() {
    let error = RuntimeError::aioobe_index_only(-1);
    // Just verify the variant holds the data; we can't check Java object
    // creation without rt.jar but we can verify the Rust side.
    //
    // This asserted `message == None` until 2026-08-06, on the reasoning
    // that a call site which cannot name the array's length must not invent
    // any text. `ca7526cf0` replaced that with the JDK's OWN one-argument
    // wording, which is the better answer for the same reason: `new
    // ArrayIndexOutOfBoundsException(int)` in `java.base` produces exactly
    // `"Array index out of range: N"`, so this is HotSpot's string for a
    // site that knows only the index, not a guess at the one it cannot
    // build. The genuinely-null case moved to `aioobe_no_message`, and is
    // asserted below so this pair cannot silently collapse into one.
    if let RuntimeError::ArrayIndexOutOfBoundsException { index, message } = error {
        assert_eq!(index, -1);
        assert_eq!(
            message.as_deref(),
            Some("Array index out of range: -1"),
            "aioobe_index_only is the \"cannot name the length\" \
             constructor — it carries the JDK's own one-argument wording, \
             which is exact for what the site knows"
        );
    } else {
        panic!("wrong variant");
    }
}

/// The null-message sibling, which is a separate constructor precisely so
/// that "HotSpot really prints nothing here" is stated rather than
/// inherited.
///
/// `java.lang.reflect.Array`'s accessors raise AIOOBE with no text at all,
/// so `Array.get(new int[4], 9).getMessage()` is null on HotSpot while a
/// plain `a[9]` in bytecode says "Index 9 out of bounds for length 4".
#[test]
fn runtime_error_array_index_can_carry_no_message_at_all() {
    let error = RuntimeError::aioobe_no_message(9);
    if let RuntimeError::ArrayIndexOutOfBoundsException { index, message } = error {
        assert_eq!(index, 9);
        assert_eq!(
            message, None,
            "aioobe_no_message exists for the sites a HotSpot control shows \
             producing a null getMessage(); giving it text would make \
             `Array.get` diverge from the JDK"
        );
    } else {
        panic!("wrong variant");
    }
}

#[test]
fn runtime_error_array_index_carries_hotspot_message() {
    let error = RuntimeError::aioobe(9, 4);
    if let RuntimeError::ArrayIndexOutOfBoundsException { index, message } = error {
        assert_eq!(index, 9);
        assert_eq!(
            message.as_deref(),
            Some("Index 9 out of bounds for length 4")
        );
    } else {
        panic!("wrong variant");
    }
}

#[test]
fn throw_all_remaining_variants_coverage() {
    // Cover remaining variants that weren't tested individually
    let mut vm = test_vm();

    let errors: Vec<RuntimeError> = vec![
        RuntimeError::ArrayStoreException {
            message: "bad store".to_string(),
        },
        RuntimeError::IllegalMonitorStateException {
            message: "not owner".to_string(),
        },
        RuntimeError::sioobe_index(99, 5),
        RuntimeError::NoSuchFieldException {
            field_name: "missing".to_string(),
        },
        RuntimeError::NoSuchMethodException {
            message: "missing()V".to_string(),
        },
        RuntimeError::IllegalAccessException {
            message: "private".to_string(),
        },
        RuntimeError::InaccessibleObjectException {
            message: "module not open".to_string(),
        },
        RuntimeError::FileNotFoundException {
            path: "/tmp/gone.txt".to_string(),
        },
        RuntimeError::IllegalStateException {
            message: "bad state".to_string(),
        },
        RuntimeError::NoSuchElementException {
            message: "empty iterator".to_string(),
        },
        RuntimeError::InputMismatchException {
            message: "expected int".to_string(),
        },
    ];

    for error in errors {
        let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
        assert!(matches!(
            result,
            MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
        ));
    }
}

// --- Task #57: IllegalCallerException mapping ---

/// The new `RuntimeError::IllegalCallerException` variant must map to
/// the Java class `java/lang/IllegalCallerException` — not
/// `java/lang/IllegalStateException`, which the Panama native-access
/// gate previously folded into.
///
/// We mirror the `(class_name, message)` table inside `throw_runtime_error`
/// directly rather than invoking the full throw machinery, because the
/// in-process test VM has no rt.jar and therefore can't actually load
/// the exception class. The mapping itself is what matters.
#[test]
fn task57_illegal_caller_maps_to_java_lang_illegal_caller_exception() {
    let err = RuntimeError::IllegalCallerException {
        message: "denied".into(),
    };
    // The variant's Display matches the bare-class-name convention used
    // throughout this enum, so the mapping table can rely on it.
    assert_eq!(format!("{err}"), "IllegalCallerException: denied");

    // Drive the full conversion: we expect either InternalError (no
    // rt.jar in the test VM) OR ExceptionThrown. Either way, the call
    // must not panic — proving the new variant is wired through the
    // match arm.
    let mut vm = test_vm();
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, err);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

/// Regression guard: `IllegalStateException` must continue to map to
/// `java/lang/IllegalStateException`. The newly-added arm above sits
/// next to the existing IllegalStateException arm in the match table,
/// so we sanity-check both still route correctly.
#[test]
fn task57_illegal_state_still_maps_to_java_lang_illegal_state_exception() {
    let mut vm = test_vm();
    let err = RuntimeError::IllegalStateException {
        message: "still here".into(),
    };
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, err);
    assert!(matches!(
        result,
        MethodCallFailed::InternalError(_) | MethodCallFailed::ExceptionThrown(_)
    ));
}

#[test]
fn not_implemented_error_preserves_feature_name() {
    let error = RuntimeError::NotImplemented {
        feature: "fancy_feature".to_string(),
    };
    if let RuntimeError::NotImplemented { feature } = &error {
        assert_eq!(feature, "fancy_feature");
    } else {
        panic!("wrong variant");
    }
    // Confirm it maps to InternalError
    let mut vm = test_vm();
    let result = throw_runtime_error(&vm.shared, &mut vm.main_thread, error);
    match result {
        MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::NotImplemented {
            feature,
        })) => {
            assert_eq!(feature, "fancy_feature");
        }
        _ => panic!("expected InternalError wrapping NotImplemented"),
    }
}

// -----------------------------------------------------------------------
// r9w9 exc9: OmitStackTraceInFastThrow
// -----------------------------------------------------------------------

/// A site is hot strictly after `threshold` throws, per key; a full table
/// is cleared rather than grown, and never answers "hot" for a site it
/// has not counted past the threshold.
#[test]
fn fast_throw_site_turns_hot_after_the_threshold_and_the_table_is_bounded() {
    let mut counts: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
    for i in 1..=3 {
        assert!(
            !fast_throw_note_site(&mut counts, 7, 3, 16),
            "throw {i} of 3 must still be built in full"
        );
    }
    assert!(
        fast_throw_note_site(&mut counts, 7, 3, 16),
        "the 4th is hot"
    );
    assert!(fast_throw_note_site(&mut counts, 7, 3, 16), "and stays hot");
    assert!(
        !fast_throw_note_site(&mut counts, 8, 3, 16),
        "another site counts on its own"
    );

    // Cap: filling the table with distinct sites clears it, and the old
    // hot site starts over (slower, never wrong).
    let mut small: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
    for _ in 0..3 {
        let _ = fast_throw_note_site(&mut small, 1, 1, 2);
    }
    assert!(fast_throw_note_site(&mut small, 1, 1, 2));
    let _ = fast_throw_note_site(&mut small, 2, 1, 2);
    assert_eq!(small.len(), 2);
    let _ = fast_throw_note_site(&mut small, 3, 1, 2);
    assert_eq!(small.len(), 1, "a full table is cleared before a new key");
    assert!(
        !fast_throw_note_site(&mut small, 1, 1, 2),
        "a cleared site is cold again"
    );
}

#[test]
fn fast_throw_kind_covers_exactly_the_five_implicit_exceptions() {
    use super::FastThrowKind as K;
    let cases = [
        (
            RuntimeError::NullPointerException { message: None },
            Some(K::NullPointer),
        ),
        (
            RuntimeError::ArrayIndexOutOfBoundsException {
                index: 5,
                message: None,
            },
            Some(K::ArrayIndexOutOfBounds),
        ),
        (
            RuntimeError::ArithmeticException {
                message: "/ by zero".to_string(),
            },
            Some(K::Arithmetic),
        ),
        (
            RuntimeError::ClassCastException {
                message: "x".to_string(),
            },
            Some(K::ClassCast),
        ),
        (
            RuntimeError::ArrayStoreException {
                message: "x".to_string(),
            },
            Some(K::ArrayStore),
        ),
        (
            RuntimeError::NotImplemented {
                feature: "x".to_string(),
            },
            None,
        ),
    ];
    for (error, want) in cases {
        assert_eq!(FastThrowKind::of_runtime_error(&error), want, "{error:?}");
    }
    assert_eq!(
        K::ArrayIndexOutOfBounds.class_name(),
        "java/lang/ArrayIndexOutOfBoundsException"
    );
}

/// The resumed-compiled-frame scope names one depth, nests, and restores
/// the outer one on drop.
#[test]
fn resumed_compiled_frame_scope_nests_and_restores() {
    let read = || RESUMED_COMPILED_FRAME_DEPTH.with(|d| d.get().0);
    assert_eq!(read(), 0);
    {
        let _outer = ResumedCompiledFrameScope::enter(3);
        assert_eq!(read(), 3);
        {
            let _inner = ResumedCompiledFrameScope::enter(7);
            assert_eq!(read(), 7);
        }
        assert_eq!(read(), 3, "the inner scope restores the outer depth");
    }
    assert_eq!(read(), 0);
}

/// The stackless throwable is an instance of the requested class and has
/// NO entry in the VM trace store (so `getStackTrace()` is empty), where
/// the full build of the same class registers one.
#[test]
fn stackless_exception_has_the_class_and_no_stored_trace() {
    let mut vm = test_vm();
    let Ok(full) = create_exception_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/ArithmeticException",
        Some("/ by zero"),
    ) else {
        // Stripped VM (no JDK on the test classpath): nothing to probe with.
        return;
    };
    let class_id = vm.shared.mem.heap.class_id_of(full);
    let Some(fast) =
        create_stackless_exception_object(&vm.shared, "java/lang/ArithmeticException")
    else {
        // Only legitimate refusal after a successful full build: the class
        // is neither initialised nor safely initialisable without Java
        // (r9w10 exc10 — an uninitialised `ArithmeticException` under an
        // initialised `Throwable` is NOT a reason to refuse any more).
        let cm = vm.shared.classes.class_manager.read();
        let class = cm.get_class(class_id).expect("the full build loaded it");
        assert!(
            !crate::vm::is_class_initialized_fast(class),
            "an initialised, loaded exception class must yield a stackless instance"
        );
        return;
    };
    assert_eq!(vm.shared.mem.heap.class_id_of(fast), class_id);
    assert_ne!(fast, full, "a fresh instance per throw, never a shared one");
    // `cause = this` on both, whatever the layout: a constructor seeded it
    // on `full`, and the door did on `fast`, which has none. A never-written
    // compact slot would read `Object(None)` -- a refused `initCause`.
    let cause = throwable_field_index_or_walk(&vm.shared, fast, "cause")
        .expect("ArithmeticException inherits Throwable.cause");
    assert_eq!(
        vm.shared.mem.heap.get_field(full, cause),
        Value::Object(Some(full))
    );
    assert_eq!(
        vm.shared.mem.heap.get_field(fast, cause),
        Value::Object(Some(fast))
    );
    assert!(
        vm.shared.throwable_stack_trace_for(fast).is_none(),
        "a fast throw must not walk the stack into the trace store"
    );
}

/// Off (`-XX:-OmitStackTraceInFastThrow`, per VM), the door never
/// answers, however hot the site: every throw is built in full.
#[test]
fn fast_throw_door_is_closed_when_the_flag_is_off() {
    let vm = Vm::new(VmConfig {
        omit_stack_trace_in_fast_throw: Some(false),
        ..VmConfig::default()
    });
    assert!(!omit_stack_trace_in_fast_throw(&vm.shared));
    for _ in 0..(FAST_THROW_HOT_THRESHOLD * 2) {
        assert!(
            try_fast_throw(&vm.shared, &vm.main_thread, FastThrowKind::Arithmetic, None)
                .is_none()
        );
    }
}


/// nio21 wave 16, lane L5: the fast throw hands out ONE bare instance per
/// kind per VM, as HotSpot's `Universe::_*_exception` -- no constructor
/// (`cause` null, not `this`), no stored trace, and its `detailMessage`
/// cleared on every hand-out.
#[test]
fn nio21_w16_l5_preallocated_fast_throw_instance_is_one_bare_object_per_kind() {
    let vm = test_vm();
    let Some(a) = preallocated_fast_throw_instance(&vm.shared, FastThrowKind::Arithmetic)
    else {
        // Stripped VM (no JDK on the test classpath), or a class that
        // cannot be instantiated without Java: the full build is used.
        return;
    };
    let b = preallocated_fast_throw_instance(&vm.shared, FastThrowKind::Arithmetic)
        .expect("built once, then reused");
    assert_eq!(a, b, "the same object every time, as on HotSpot");
    if let Some(n) = preallocated_fast_throw_instance(&vm.shared, FastThrowKind::NullPointer) {
        assert_ne!(n, a, "one instance per kind");
    }
    let cause = throwable_field_index_or_walk(&vm.shared, a, "cause")
        .expect("ArithmeticException inherits Throwable.cause");
    assert_eq!(
        vm.shared.mem.heap.get_field(a, cause),
        Value::Object(None),
        "no constructor ran: initCause must refuse, as on HotSpot"
    );
    assert!(vm.shared.throwable_stack_trace_for(a).is_none());
    let msg = throwable_field_index_or_walk(&vm.shared, a, "detailMessage")
        .expect("Throwable.detailMessage");
    // Anything written into the shared object's message (HotSpot's comment:
    // "Weblogic sometimes mutates the detail message") is gone at the next
    // hand-out.
    vm.shared.mem.heap.set_field(a, msg, Value::Object(Some(a)));
    let c = preallocated_fast_throw_instance(&vm.shared, FastThrowKind::Arithmetic)
        .expect("still built");
    assert_eq!(c, a);
    assert_eq!(vm.shared.mem.heap.get_field(a, msg), Value::Object(None));
}


/// The interpreter records a trap only at a bytecode that raises that
/// kind implicitly (HotSpot's `note_trap` callers), so an exception a
/// native raises while its caller stands at an unrelated bytecode is not
/// mistaken for one.
#[test]
fn fast_throw_trap_opcodes_are_the_implicit_raisers() {
    use FastThrowKind as K;
    // iaload / aastore / getfield / invokevirtual / arraylength / athrow / monitorenter.
    for op in [0x2e, 0x53, 0xb4, 0xb6, 0xbe, 0xbf, 0xc2] {
        assert!(opcode_raises_implicitly(op, K::NullPointer), "{op:#x}");
    }
    // invokestatic and new never raise an implicit NPE.
    assert!(!opcode_raises_implicitly(0xb8, K::NullPointer));
    assert!(!opcode_raises_implicitly(0xbb, K::NullPointer));
    assert!(opcode_raises_implicitly(0x33, K::ArrayIndexOutOfBounds));
    assert!(!opcode_raises_implicitly(0xbe, K::ArrayIndexOutOfBounds));
    for op in [0x6c, 0x6d, 0x70, 0x71] {
        assert!(opcode_raises_implicitly(op, K::Arithmetic), "{op:#x}");
    }
    // fdiv / ddiv divide by zero without throwing.
    assert!(!opcode_raises_implicitly(0x6e, K::Arithmetic));
    assert!(opcode_raises_implicitly(0xc0, K::ClassCast));
    assert!(
        !opcode_raises_implicitly(0xc1, K::ClassCast),
        "instanceof never throws"
    );
    assert!(opcode_raises_implicitly(0x53, K::ArrayStore));
    assert!(
        !opcode_raises_implicitly(0x4f, K::ArrayStore),
        "iastore stores no reference"
    );
}

/// The trap profile is bounded: filling it clears it, and a site then has
/// to throw again before it can be hot.
#[test]
fn fast_throw_trap_profile_is_bounded() {
    let profile = FastThrowTrapProfile::default();
    let label: Arc<str> = Arc::from("p/Probe.m:()V");
    profile.note(&label, 0, FastThrowKind::NullPointer);
    assert!(profile.has_trap_at(&label, 0, FastThrowKind::NullPointer));
    for bci in 1..FAST_THROW_TRAP_PROFILE_CAP as u32 {
        profile.note(&label, bci, FastThrowKind::NullPointer);
    }
    assert!(
        profile.has_trap_at(&label, 0, FastThrowKind::NullPointer),
        "at the cap, kept"
    );
    profile.note(&label, u32::MAX, FastThrowKind::NullPointer);
    assert!(
        !profile.has_trap_at(&label, 0, FastThrowKind::NullPointer),
        "past it, cleared"
    );
    assert!(profile.has_trap_at(&label, u32::MAX, FastThrowKind::NullPointer));
}

/// r9w10 exc10: the stackless door no longer demands an INITIALISED
/// class — the full build never initialises the implicit-exception
/// classes, so that demand kept the door shut forever. It demands instead
/// that initialising would run no Java: a `<clinit>`-free, untouched chain
/// up to an initialised ancestor.
#[test]
fn stackless_door_accepts_a_clinit_free_chain_under_an_initialised_ancestor() {
    // 1 AIOOBE -> 2 IOOBE -> 3 RuntimeException -> 4 Throwable (initialised).
    let chain = |overrides: &[(u32, ClassInitView)]| {
        let mut m: std::collections::HashMap<u32, ClassInitView> =
            std::collections::HashMap::new();
        let plain = |sup: u32| ClassInitView {
            initialized: false,
            untouched: true,
            has_clinit: false,
            superclass: Some(ClassId::new(sup)),
        };
        m.insert(1, plain(2));
        m.insert(2, plain(3));
        m.insert(3, plain(4));
        m.insert(
            4,
            ClassInitView {
                initialized: true,
                untouched: false,
                has_clinit: true,
                superclass: None,
            },
        );
        for (k, v) in overrides {
            m.insert(*k, *v);
        }
        m
    };
    let ask = |m: &std::collections::HashMap<u32, ClassInitView>| {
        initialisation_is_unobservable(ClassId::new(1), |cid| m.get(&cid.as_u32()).copied())
    };

    assert!(ask(&chain(&[])), "the implicit-exception shape is accepted");

    let with = |id: u32, f: &dyn Fn(&mut ClassInitView)| {
        let mut m = chain(&[]);
        if let Some(v) = m.get_mut(&id) {
            f(v);
        }
        m
    };
    assert!(
        !ask(&with(2, &|v| v.has_clinit = true)),
        "a <clinit> below the initialised ancestor would be skipped"
    );
    assert!(
        !ask(&with(1, &|v| v.untouched = false)),
        "an in-progress or failed initialisation is never bypassed"
    );
    assert!(
        !ask(&with(4, &|v| v.initialized = false)),
        "no initialised ancestor: the <clinit> of Throwable has not run"
    );
    assert!(
        // gate-ok(not-a-Class): `v` is a `ClassInitView`, not a `Class`.
        !ask(&with(3, &|v| v.superclass = Some(ClassId::new(99)))),
        "a class the lookup cannot see refuses"
    );
    assert!(
        ask(&with(1, &|v| v.initialized = true)),
        "an initialised class is accepted outright"
    );
    // A cycle (never a real hierarchy) terminates and refuses.
    // gate-ok(not-a-Class): `v` is a `ClassInitView`, not a `Class`.
    assert!(!ask(&with(3, &|v| v.superclass = Some(ClassId::new(1)))));
}

/// A `Class` for the JNI `ThrowNew` tests: `methods` only, no fields of its
/// own, extending the bootstrap `java/lang/Throwable` when `throwable` and
/// that class is loaded (a stripped VM has none, and the code under test
/// then skips its `Throwable` check too).
fn add_jni_throw_new_test_class(
    vm: &Vm,
    name: &str,
    methods: Vec<cratonvm_reader::method::ClassFileMethod>,
    throwable: bool,
) -> ClassId {
    use crate::classloading::{Class, ClassLoaderId, ClassState};
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    let mut cm = vm.shared.classes.class_manager_write();
    let superclass = if throwable {
        cm.loaded_class_under_exact_key("java/lang/Throwable", ClassLoaderId::Bootstrap)
    } else {
        None
    };
    // Throwable's instance fields, or slack for Throwable-shaped slot
    // writes should a fallback path be taken without it.
    let inherited = superclass
        .and_then(|s| cm.get_class(s))
        .map_or(8, |c| c.num_total_fields);
    let id = cm.class_store.next_id();
    cm.class_store.add(Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: cratonvm_types::intern_arc(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Initialized,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: ClassAccessFlags::from_bits_truncate(0x0021),
        superclass,
        interfaces: vec![],
        fields: vec![],
        methods,
        first_field_index: inherited,
        num_total_fields: inherited,
        bootstrap_methods: vec![],
        signature: None,
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: cratonvm_classloading::ClassOrigin::default(),
        has_finalizer: false,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    });
    cm.register_class_name(ClassLoaderId::Application, name, id);
    id
}

/// A public `<init>` of `descriptor` whose body is `code`.
fn jni_test_ctor(
    descriptor: &str,
    max_locals: u16,
    code: Vec<u8>,
) -> cratonvm_reader::method::ClassFileMethod {
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    cratonvm_reader::method::ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: std::sync::Arc::from("<init>"),
        descriptor: std::sync::Arc::from(descriptor),
        attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 1,
                max_locals,
                code: cratonvm_reader::ByteView::from_vec(code),
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    }
}

/// JNI `ThrowNew` follows HotSpot's `Exceptions::new_exception`: a
/// constructor that throws makes ITS exception the one to throw. The
/// half-built throwable used to be returned with the message written in by
/// hand. The class's `<init>(String)` is `aconst_null; athrow`.
#[test]
fn jni_throw_new_rethrows_what_the_constructor_threw() {
    const NAME: &str = "cratonvm/test/JniThrowNewCtorThrows";

    let mut vm = test_vm();
    // Stripped VM: without a buildable NPE the constructor's `athrow`
    // cannot produce the exception this test is about.
    let Ok(probe) = create_exception_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/NullPointerException",
        None,
    ) else {
        return;
    };
    let npe_class = vm.shared.mem.heap.class_id_of(probe);
    // `<init>(String)`: `aconst_null; athrow`.
    let ctor = jni_test_ctor("(Ljava/lang/String;)V", 2, vec![0x01, 0xbf]);
    let class_id = add_jni_throw_new_test_class(&vm, NAME, vec![ctor], true);
    let pins = vm.main_thread.native_pin_roots.len();
    let thrown = create_exception_object_for_jni_throw_new(
        &vm.shared,
        &mut vm.main_thread,
        class_id,
        NAME,
        Some("m"),
    )
    .expect("the constructor's exception is a result, not a failure");
    assert_eq!(
        vm.shared.mem.heap.class_id_of(thrown),
        npe_class,
        "ThrowNew throws what the constructor threw"
    );
    assert_eq!(vm.main_thread.native_pin_roots.len(), pins, "pins released");
}

/// JDK-8048190 (HotSpot 21+): "Could not initialize class" carries an
/// `ExceptionInInitializerError` naming the original failure and thread.
#[test]
fn could_not_initialize_class_carries_the_recorded_cause() {
    let mut vm = test_vm();
    let Ok(boom) = create_exception_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/IllegalStateException",
        Some("boom"),
    ) else {
        return;
    };
    // Stripped VM: the stand-in class may carry no readable
    // `detailMessage`, and then there is no message to record.
    let message_readable = throwable_field_index_or_walk(&vm.shared, boom, "detailMessage")
        .filter(|&i| i < vm.shared.mem.heap.get_header(boom).num_slots() as usize)
        .is_some_and(|i| {
            matches!(
                vm.shared.mem.heap.get_field(boom, i),
                Value::Object(Some(_))
            )
        });
    if !message_readable {
        return;
    }
    // The original exception's trace (wave 8, L5): the recorded cause
    // must carry it, as HotSpot's does, not the "Could not initialize"
    // site's.
    let origin_frame = |method: &str| crate::native::registry::StackTraceEntry {
        class_name: std::sync::Arc::from("p/Origin"),
        method_name: std::sync::Arc::from(method),
        method_descriptor: None,
        source_file: None,
        line_number: -1,
        byte_code_index: 3,
        class_id: None,
        method_index: None,
    };
    vm.shared.store_throwable_stack_trace(
        boom,
        std::sync::Arc::from(vec![origin_frame("main"), origin_frame("<clinit>")]),
    );
    let method_names = |trace: &[crate::native::registry::StackTraceEntry]| {
        trace
            .iter()
            .map(|e| e.method_name.to_string())
            .collect::<Vec<_>>()
    };
    // The table is keyed by the failed class's id only.
    let failed_class = ClassId::new(0x7fff_0000);
    record_class_init_error(
        &vm.shared,
        &vm.main_thread,
        failed_class,
        &MethodCallFailed::ExceptionThrown(boom),
    );
    let recorded = vm
        .shared
        .classes
        .class_init_errors
        .lock()
        .get(&failed_class)
        .cloned()
        .expect("a thrown exception is recorded");
    assert_eq!(
        recorded.trace.as_deref().map(method_names),
        Some(vec!["main".to_string(), "<clinit>".to_string()]),
        "the original exception's trace is recorded"
    );
    let recorded = recorded.message;
    assert!(
        recorded.starts_with("Exception java.lang.IllegalStateException: boom [in thread \"")
            && recorded.ends_with("\"]"),
        "HotSpot's create_initialization_error spelling, got {recorded}"
    );
    // Stripped VM: no buildable ExceptionInInitializerError, no cause.
    if create_exception_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/ExceptionInInitializerError",
        Some("probe"),
    )
    .is_err()
    {
        return;
    }
    let pins = vm.main_thread.native_pin_roots.len();
    let MethodCallFailed::ExceptionThrown(ncdfe) = raise_could_not_initialize_class_for(
        &vm.shared,
        &mut vm.main_thread,
        failed_class,
        "p/Failed",
    ) else {
        return;
    };
    assert_eq!(vm.main_thread.native_pin_roots.len(), pins, "pins released");
    let heap = &vm.shared.mem.heap;
    let cause_idx =
        throwable_field_index_or_walk(&vm.shared, ncdfe, "cause").expect("Throwable.cause");
    let Value::Object(Some(cause)) = heap.get_field(ncdfe, cause_idx) else {
        panic!("the NoClassDefFoundError has no cause");
    };
    assert_ne!(cause, ncdfe, "a cause, not the unset self-marker");
    let msg_idx = throwable_field_index_or_walk(&vm.shared, cause, "detailMessage")
        .expect("Throwable.detailMessage");
    let Value::Object(Some(msg)) = heap.get_field(cause, msg_idx) else {
        panic!("the cause has no message");
    };
    assert_eq!(
        crate::vm::read_java_string(heap, msg).as_deref(),
        Some(&*recorded)
    );
    // Stripped VM: a stand-in whose construction did not run our capture
    // (`backtrace != this`) is left with its own trace.
    let ours = throwable_field_index_or_walk(&vm.shared, cause, "backtrace")
        .is_some_and(|i| heap.get_field(cause, i) == Value::Object(Some(cause)));
    if ours {
        assert_eq!(
            vm.shared
                .throwable_stack_trace_for(cause)
                .as_deref()
                .map(method_names),
            Some(vec!["main".to_string(), "<clinit>".to_string()]),
            "the cause carries the original exception's trace"
        );
        let depth_idx =
            throwable_field_index_or_walk(&vm.shared, cause, "depth").expect("Throwable.depth");
        assert_eq!(heap.get_field(cause, depth_idx), Value::Int(2));
    }
    // JIT round 21 wave 26, lane reserr (`CRATONVM_INIT_ERROR_SAVED_CAUSE`):
    // a later error is a NEW `NoClassDefFoundError` carrying the SAME saved
    // `ExceptionInInitializerError` (HotSpot's init-error table).
    let first_cause = heap.get_field(ncdfe, cause_idx);
    let MethodCallFailed::ExceptionThrown(again) = raise_could_not_initialize_class_for(
        &vm.shared,
        &mut vm.main_thread,
        failed_class,
        "p/Failed",
    ) else {
        panic!("the second raise builds its error");
    };
    let heap = &vm.shared.mem.heap;
    let cause = vm
        .shared
        .classes
        .class_init_errors
        .lock()
        .get(&failed_class)
        .and_then(|row| row.saved_cause)
        .and_then(|h| vm.shared.natives.jni_global_refs.lock().resolve(h))
        .expect("the first raise saved its cause");
    assert_ne!(again, ncdfe, "each raise is a new error");
    assert_eq!(first_cause, Value::Object(Some(cause)), "the saved cause is the first one");
    assert_eq!(
        heap.get_field(again, cause_idx),
        Value::Object(Some(cause)),
        "the later error carries the saved cause"
    );
    // JIT round 22 wave 1, lane review: a raiser that lost the race to
    // save its own cause gets the winner's object back (its own handle is
    // released), and a class with no row saves nothing.
    assert_eq!(
        save_init_error_cause(&vm.shared, failed_class, again),
        Some(cause),
        "the loser hands out the saved cause"
    );
    assert_eq!(
        vm.shared
            .classes
            .class_init_errors
            .lock()
            .get(&failed_class)
            .and_then(|row| row.saved_cause)
            .and_then(|h| vm.shared.natives.jni_global_refs.lock().resolve(h)),
        Some(cause),
        "the saved row is unchanged"
    );
    assert_eq!(save_init_error_cause(&vm.shared, ClassId::new(0x7fff_0001), again), None);
}

/// `ThrowNew(clazz, msg)` on a `Throwable` that declares no `(String)`
/// constructor throws `NoSuchMethodError` with HotSpot's message, rather
/// than running the inherited `Throwable.<init>(String)` on it (or falling
/// back to `()V` and writing the message by hand).
#[test]
fn jni_throw_new_without_a_string_constructor_throws_no_such_method_error() {
    const NAME: &str = "cratonvm/test/JniThrowNewNoStringCtor";

    let mut vm = test_vm();
    let Ok(probe) = create_exception_object(
        &vm.shared,
        &mut vm.main_thread,
        "java/lang/NoSuchMethodError",
        None,
    ) else {
        return;
    };
    let nsme_class = vm.shared.mem.heap.class_id_of(probe);
    // `<init>()V` only: `return`.
    let ctor = jni_test_ctor("()V", 1, vec![0xb1]);
    let class_id = add_jni_throw_new_test_class(&vm, NAME, vec![ctor], true);
    let pins = vm.main_thread.native_pin_roots.len();
    let thrown = create_exception_object_for_jni_throw_new(
        &vm.shared,
        &mut vm.main_thread,
        class_id,
        NAME,
        Some("m"),
    )
    .expect("NoSuchMethodError is the exception to throw, not a failure");
    assert_eq!(
        vm.shared.mem.heap.class_id_of(thrown),
        nsme_class,
        "a missing (String) constructor throws NoSuchMethodError"
    );
    assert_eq!(
        nsme_message(NAME, "<init>", "(Ljava/lang/String;)V"),
        "'void cratonvm.test.JniThrowNewNoStringCtor.<init>(java.lang.String)'"
    );
    assert_eq!(vm.main_thread.native_pin_roots.len(), pins, "pins released");
}

/// `ThrowNew` on a class that is not a `Throwable` is refused: nothing is
/// built, so JNI returns `JNI_ERR` with no exception pending.
#[test]
fn jni_throw_new_refuses_a_class_that_is_not_a_throwable() {
    const NAME: &str = "cratonvm/test/JniThrowNewNotThrowable";

    let mut vm = test_vm();
    let throwable_loaded = vm
        .shared
        .classes
        .class_manager
        .read()
        .loaded_class_under_exact_key(
            "java/lang/Throwable",
            cratonvm_types::ClassLoaderId::Bootstrap,
        )
        .is_some();
    if !throwable_loaded {
        return;
    }
    let ctor = jni_test_ctor("(Ljava/lang/String;)V", 2, vec![0xb1]);
    let class_id = add_jni_throw_new_test_class(&vm, NAME, vec![ctor], false);
    let pins = vm.main_thread.native_pin_roots.len();
    let result = create_exception_object_for_jni_throw_new(
        &vm.shared,
        &mut vm.main_thread,
        class_id,
        NAME,
        Some("m"),
    );
    assert!(
        matches!(result, Err(MethodCallFailed::InternalError(_))),
        "a non-Throwable class must be refused, got {result:?}"
    );
    assert_eq!(
        vm.main_thread.native_pin_roots.len(),
        pins,
        "no pins leaked"
    );
}
