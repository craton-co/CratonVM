use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn new15_continuation_is_registered() {
    let mut r = NativeMethodRegistry::new();
    register_new15_loom(&mut r);
    assert!(r
        .find(
            "jdk/internal/vm/Continuation",
            "<init>",
            "(Ljdk/internal/vm/ContinuationScope;Ljava/lang/Runnable;)V"
        )
        .is_some());
    assert!(r
        .find("jdk/internal/vm/Continuation", "run", "()V")
        .is_some());
    assert!(r
        .find(
            "jdk/internal/vm/Continuation",
            "yield",
            "(Ljdk/internal/vm/ContinuationScope;)Z"
        )
        .is_some());
    assert!(r
        .find("jdk/internal/vm/Continuation", "isDone", "()Z")
        .is_some());
}

#[test]
fn new15_continuation_scope_is_registered() {
    let mut r = NativeMethodRegistry::new();
    register_new15_loom(&mut r);
    assert!(r
        .find(
            "jdk/internal/vm/ContinuationScope",
            "<init>",
            "(Ljava/lang/String;)V"
        )
        .is_some());
    assert!(r
        .find(
            "jdk/internal/vm/ContinuationScope",
            "getName",
            "()Ljava/lang/String;"
        )
        .is_some());
}

#[test]
fn new15_forkjoinpool_common_is_registered() {
    let mut r = NativeMethodRegistry::new();
    register_new15_loom(&mut r);
    assert!(r
        .find(
            "java/util/concurrent/ForkJoinPool",
            "commonPool",
            "()Ljava/util/concurrent/ForkJoinPool;"
        )
        .is_some());
    assert!(r
        .find(
            "java/util/concurrent/ForkJoinPool",
            "getCommonPoolParallelism",
            "()I"
        )
        .is_some());
}

/// `commonPool()` answers one carrier per VM (gc-common w9-b,
/// `handoff-w9b-forkjoin-per-vm-callers.md`): the same object on every
/// call within a VM, a different one in another VM.
#[test]
fn w9b_common_pool_is_one_per_vm() {
    const VM_A: usize = 0x9B0F_1001;
    const VM_B: usize = 0x9B0F_1002;
    let mut r = NativeMethodRegistry::new();
    register_new15_loom(&mut r);
    let common = r
        .find(
            "java/util/concurrent/ForkJoinPool",
            "commonPool",
            "()Ljava/util/concurrent/ForkJoinPool;",
        )
        .expect("commonPool registered");
    let pool = |ctx: &mut crate::test_utils::MockNativeContext| match common(ctx, &[]) {
        Ok(Some(Value::Object(Some(p)))) => p.as_ptr() as usize,
        other => panic!("commonPool answered {other:?}"),
    };
    let mut a = crate::test_utils::MockNativeContext::new();
    a.set_vm_identity(VM_A);
    let mut b = crate::test_utils::MockNativeContext::new();
    b.set_vm_identity(VM_B);
    let (a1, b1) = (pool(&mut a), pool(&mut b));
    assert_ne!(a1, b1, "VM B must not answer VM A's pool");
    assert_eq!(pool(&mut a), a1, "one pool per VM");
    assert_eq!(pool(&mut b), b1, "one pool per VM");
    crate::phases_early::forget_vm_forkjoin_state(VM_A);
    crate::phases_early::forget_vm_forkjoin_state(VM_B);
}

#[test]
fn t19_k3_forkjoinpool_get_factory_is_registered() {
    let mut r = NativeMethodRegistry::new();
    register_new15_loom(&mut r);
    assert!(r
        .find(
            "java/util/concurrent/ForkJoinPool",
            "getFactory",
            "()Ljava/util/concurrent/ForkJoinPool$ForkJoinWorkerThreadFactory;"
        )
        .is_some());
}

#[test]
fn t19_k3_safe_factory_class_name_accepts_quarkus() {
    // The Quarkus factory name (the property value KC26 sets).
    assert!(is_safe_factory_class_name(
        "io.quarkus.bootstrap.forkjoin.QuarkusForkJoinWorkerThreadFactory"
    ));
    // A nested class name, i.e. one carrying the `$` separator. W7-14: this
    // asserts the *syntax* validator accepts `$`, nothing about which class
    // the JDK declares — the name below is the JDK-21-era one that JDK 25
    // dropped, and reading this case as "the JDK default" is how that name
    // kept looking load-bearing. Both spellings are exercised so the
    // distinction cannot quietly collapse again.
    assert!(is_safe_factory_class_name(
        "java.util.concurrent.ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory"
    ));
    // What JDK 25 actually declares, and what HotSpot 25 answers from
    // `commonPool().getFactory().getClass().getName()`.
    assert!(is_safe_factory_class_name(
        "java.util.concurrent.ForkJoinPool$DefaultForkJoinWorkerThreadFactory"
    ));
}

#[test]
fn t19_k3_safe_factory_class_name_rejects_path_traversal() {
    // Slashes (filesystem path) — rejected.
    assert!(!is_safe_factory_class_name("/etc/passwd"));
    // Backslashes (Windows path) — rejected.
    assert!(!is_safe_factory_class_name(r"C:\Windows\System32"));
    // Spaces — rejected.
    assert!(!is_safe_factory_class_name("foo bar.Baz"));
    // Empty — rejected.
    assert!(!is_safe_factory_class_name(""));
    // No dots (unqualified) — rejected.
    assert!(!is_safe_factory_class_name("Foo"));
}

#[test]
fn t19_k3_safe_factory_class_name_rejects_consecutive_dots() {
    assert!(!is_safe_factory_class_name("foo..bar"));
    assert!(!is_safe_factory_class_name(".leading"));
    assert!(!is_safe_factory_class_name("trailing."));
}

#[test]
fn t19_k3_safe_factory_class_name_rejects_oversize() {
    let huge = "a.".repeat(200);
    assert!(huge.len() > 256);
    assert!(!is_safe_factory_class_name(&huge));
}

#[test]
fn t19_k3_safe_factory_class_name_rejects_special_chars() {
    // Non-ASCII letter — rejected.
    assert!(!is_safe_factory_class_name("foo.b\u{00e9}r"));
    // Control char — rejected.
    assert!(!is_safe_factory_class_name("foo.bar\x00"));
    // Semicolon (descriptor terminator) — rejected.
    assert!(!is_safe_factory_class_name("foo.bar;"));
    // Bracket (array marker) — rejected.
    assert!(!is_safe_factory_class_name("[foo.bar"));
}

#[test]
fn new15_continuation_field_layout_constants_distinct() {
    assert_ne!(NEW15_CONT_SCOPE, NEW15_CONT_TARGET);
    assert_ne!(NEW15_CONT_TARGET, NEW15_CONT_STATE);
    assert_ne!(NEW15_CONT_STATE, NEW15_CONT_PIN);
    assert_ne!(NEW15_CONT_PIN, NEW15_CONT_PREEMPT);
    assert!(NEW15_CONT_PREEMPT < NEW15_CONT_FIELDS);
}

#[test]
fn new15_forkjoinpool_field_layout_constants_distinct() {
    assert_ne!(NEW15_FJP_PARALLELISM, NEW15_FJP_ACTIVE);
    assert!(NEW15_FJP_ACTIVE < NEW15_FJP_FIELDS);
}

#[test]
fn new15_continuation_states_are_distinct() {
    assert_ne!(NEW15_CONT_STATE_NEW, NEW15_CONT_STATE_RUNNING);
    assert_ne!(NEW15_CONT_STATE_RUNNING, NEW15_CONT_STATE_YIELDED);
    assert_ne!(NEW15_CONT_STATE_YIELDED, NEW15_CONT_STATE_DONE);
}

// -----------------------------------------------------------------------
// W7-75 — the read-side slot alias, and the guard that could not fire
// -----------------------------------------------------------------------

/// The real JDK 25 `jdk.internal.vm.Continuation` layout, `javap -p`
/// against Eclipse Adoptium 25.0.3.9, in declaration order with `static`
/// excluded. Superclass is `java.lang.Object`, which declares none, so this
/// is the whole transitive chain.
const REAL_CONT_FIELDS: &[(&str, &str)] = &[
    ("target", "Ljava/lang/Runnable;"),
    ("scope", "Ljdk/internal/vm/ContinuationScope;"),
    ("parent", "Ljdk/internal/vm/Continuation;"),
    ("child", "Ljdk/internal/vm/Continuation;"),
    ("tail", "Ljdk/internal/vm/StackChunk;"),
    ("done", "Z"),
    ("mounted", "Z"),
    ("yieldInfo", "Ljava/lang/Object;"),
    ("preempted", "Z"),
    ("scopedValueCache", "[Ljava/lang/Object;"),
];

/// The real JDK 25 `java.util.concurrent.ForkJoinPool` layout, same oracle.
/// Its superclass `AbstractExecutorService` declares NO instance field (its
/// only member is the static `$assertionsDisabled`), so ForkJoinPool's own
/// sixteen are the whole chain — which is what makes `parallelism` 15 and
/// not 15-plus-something.
const REAL_FJP_FIELDS: &[(&str, &str)] = &[
    ("termination", "Ljava/util/concurrent/CountDownLatch;"),
    ("saturate", "Ljava/util/function/Predicate;"),
    (
        "factory",
        "Ljava/util/concurrent/ForkJoinPool$ForkJoinWorkerThreadFactory;",
    ),
    ("ueh", "Ljava/lang/Thread$UncaughtExceptionHandler;"),
    ("container", "Ljdk/internal/vm/SharedThreadContainer;"),
    ("workerNamePrefix", "Ljava/lang/String;"),
    ("poolName", "Ljava/lang/String;"),
    ("delayScheduler", "Ljava/util/concurrent/DelayScheduler;"),
    ("queues", "[Ljava/util/concurrent/ForkJoinPool$WorkQueue;"),
    ("runState", "J"),
    ("keepAlive", "J"),
    ("config", "J"),
    ("stealCount", "J"),
    ("threadIds", "J"),
    ("ctl", "J"),
    ("parallelism", "I"),
];

/// The synthetic shapes, exactly as `ClassManager::instance_fields(n)`
/// fabricates them: anonymous `_f0..`, all `Ljava/lang/Object;`. This is
/// what makes the fallback tests real — a fabricated class declares NONE of
/// the JDK's names, which is precisely the witness `cont_slots` keys on.
const ANON_FIELD_NAMES: &[&str] = &["_f0", "_f1", "_f2", "_f3", "_f4"];

/// Teach the mock a class with this exact instance-field layout and hand
/// back a fresh instance of it. The mock resolves a name only through the
/// metadata a test declares (its `mock_field_slot` fallback table names
/// none of the fields used here — checked), so the witness is falsifiable.
fn instance_of(
    ctx: &mut crate::test_utils::MockNativeContext,
    class_name: &str,
    fields: &[(&str, &str)],
) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized(class_name)
        .expect("mock always resolves");
    ctx.set_declared_fields(
        cid,
        fields
            .iter()
            .enumerate()
            .map(
                |(slot_index, (name, descriptor))| cratonvm_native_api::FieldMetadata {
                    name: (*name).to_string(),
                    descriptor: (*descriptor).to_string(),
                    access_flags: 0,
                    slot_index,
                    declaring_class_id: cid,
                    is_static: false,
                },
            )
            .collect(),
    );
    ctx.alloc_object(cid, fields.len())
}

fn anon_fields(count: usize) -> Vec<(&'static str, &'static str)> {
    ANON_FIELD_NAMES[..count]
        .iter()
        .map(|name| (*name, "Ljava/lang/Object;"))
        .collect()
}

/// What W7-69 §6(1) recorded, asserted rather than described: every entry
/// of the synthetic map names a different field than the real class has at
/// that index, and `scope`/`target` are SWAPPED — the shape where both
/// resolve and neither complains.
#[test]
fn the_synthetic_continuation_map_disagrees_with_the_real_class_in_every_slot() {
    let at = |i: usize| REAL_CONT_FIELDS[i].0;
    assert_eq!(at(NEW15_CONT_SCOPE), "target");
    assert_eq!(at(NEW15_CONT_TARGET), "scope");
    assert_eq!(at(NEW15_CONT_STATE), "parent");
    assert_eq!(at(NEW15_CONT_PIN), "child");
    assert_eq!(at(NEW15_CONT_PREEMPT), "tail");
    // And the two that are swapped really are the same pair, not two
    // unrelated wrongs: the map's `scope` is the class's `target` and vice
    // versa. That is the `AsynchronousSocketChannel` shape W7-49 §5 names.
    assert_eq!(at(NEW15_CONT_SCOPE), "target");
    assert_eq!(at(NEW15_CONT_TARGET), "scope");
    // Real `parallelism` is a genuine `int`; slot 0 and 1 are references.
    assert_eq!(REAL_FJP_FIELDS[NEW15_FJP_PARALLELISM].0, "termination");
    assert_eq!(REAL_FJP_FIELDS[NEW15_FJP_ACTIVE].0, "saturate");
    assert_eq!(REAL_FJP_FIELDS[15], ("parallelism", "I"));
}

#[test]
fn cont_slots_resolves_the_real_layout_by_name() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = instance_of(&mut ctx, "jdk/internal/vm/Continuation", REAL_CONT_FIELDS);
    let s = cont_slots(&ctx, this);
    assert!(s.real, "the four-name witness must hold on the real layout");
    assert_eq!(s.target, 0);
    assert_eq!(s.scope, 1);
    assert_eq!(s.done, 5);
    assert_eq!(s.preempted, 8);
    assert_eq!(
        s.pin, None,
        "the real class declares no pin counter; writing one would stamp \
         an Int over `child`, a Continuation reference"
    );
    // None of those is the synthetic index it replaced.
    assert_ne!(s.scope, NEW15_CONT_SCOPE);
    assert_ne!(s.target, NEW15_CONT_TARGET);
    assert_ne!(s.done, NEW15_CONT_STATE);
}

#[test]
fn cont_slots_falls_back_to_the_synthetic_map_on_an_anonymous_class() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let fields = anon_fields(NEW15_CONT_FIELDS);
    let this = instance_of(&mut ctx, "jdk/internal/vm/Continuation", &fields);
    let s = cont_slots(&ctx, this);
    assert!(!s.real);
    assert_eq!(s, CONT_SLOTS_SYNTHETIC);
}

/// **The RED.** The completed-continuation guard, on a real receiver,
/// against the predicate it used to be.
///
/// The old predicate is reproduced here verbatim rather than described,
/// because "the guard is fixed" is exactly the claim a test that only
/// checks the new path cannot make: both would pass. It reads slot 2 —
/// `parent` — and a fresh `Continuation`'s `parent` is null, so the
/// `Value::Int` match falls through to `NEW` and DONE is unreachable no
/// matter how many times `run()` completed.
#[test]
fn the_completed_guard_fires_on_the_real_layout_and_the_old_predicate_did_not() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = instance_of(&mut ctx, "jdk/internal/vm/Continuation", REAL_CONT_FIELDS);
    // `parent` as the real class leaves it on a fresh instance.
    ctx.set_field(this, 2, Value::Object(None));
    let s = cont_slots(&ctx, this);

    assert!(!cont_is_done(&ctx, this, s), "NEW is not done");
    cont_set_done(&ctx, this, s, false); // run() marks it running
    assert!(!cont_is_done(&ctx, this, s), "RUNNING is not done");
    cont_set_done(&ctx, this, s, true); // run() completes

    assert!(
        cont_is_done(&ctx, this, s),
        "the guard must see the completion it just recorded"
    );

    // The old predicate, verbatim: `match get_field(this, NEW15_CONT_STATE)
    // { Value::Int(v) => v, _ => NEW } == DONE`.
    let old_verdict = match ctx.get_field(this, NEW15_CONT_STATE) {
        Value::Int(v) => v,
        _ => NEW15_CONT_STATE_NEW,
    } == NEW15_CONT_STATE_DONE;
    assert!(
        !old_verdict,
        "the old slot-2 predicate must NOT fire here — if it does, this \
         test is not measuring the defect it is named after"
    );
}

/// The same guard on a synthetic receiver keeps its old four-value
/// encoding, so the fallback is not a behaviour change for the mode that
/// was already correct.
#[test]
fn the_completed_guard_keeps_the_state_int_encoding_on_a_synthetic_receiver() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let fields = anon_fields(NEW15_CONT_FIELDS);
    let this = instance_of(&mut ctx, "jdk/internal/vm/Continuation", &fields);
    let s = cont_slots(&ctx, this);
    cont_set_done(&ctx, this, s, false);
    assert_eq!(
        ctx.get_field(this, NEW15_CONT_STATE),
        Value::Int(NEW15_CONT_STATE_RUNNING)
    );
    assert!(!cont_is_done(&ctx, this, s));
    cont_set_done(&ctx, this, s, true);
    assert_eq!(
        ctx.get_field(this, NEW15_CONT_STATE),
        Value::Int(NEW15_CONT_STATE_DONE)
    );
    assert!(cont_is_done(&ctx, this, s));
}

/// `getParallelism()` on a pool the native did not allocate. HotSpot
/// answers the pool's own `parallelism`; the synthetic index answered a
/// reference slot and fell back to 1.
#[test]
fn fjp_parallelism_resolves_to_the_real_int_field_not_termination() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let pool = instance_of(
        &mut ctx,
        "java/util/concurrent/ForkJoinPool",
        REAL_FJP_FIELDS,
    );
    // What real `ForkJoinPool(4)` bytecode leaves behind: parallelism at
    // its own index, `termination` a null latch.
    ctx.set_field(pool, 15, Value::Int(4));
    ctx.set_field(pool, NEW15_FJP_PARALLELISM, Value::Object(None));

    let s = fjp_slots(&ctx, pool);
    assert_eq!(s.parallelism, 15);
    assert_eq!(
        s.active, None,
        "the real class has no `active`; slot 1 is `saturate`, a Predicate"
    );
    assert_eq!(ctx.get_field(pool, s.parallelism), Value::Int(4));

    // The old read, verbatim, against the same object.
    let old = match ctx.get_field(pool, NEW15_FJP_PARALLELISM) {
        Value::Int(i) => i,
        _ => 1,
    };
    assert_eq!(
        old, 1,
        "slot 0 is `termination` — the old read could only ever answer its \
         fallback here, which is the whole finding"
    );
}

#[test]
fn fjp_slots_falls_back_to_the_synthetic_map_on_an_anonymous_class() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let fields = anon_fields(NEW15_FJP_FIELDS);
    let pool = instance_of(&mut ctx, "java/util/concurrent/ForkJoinPool", &fields);
    let s = fjp_slots(&ctx, pool);
    assert_eq!(s.parallelism, NEW15_FJP_PARALLELISM);
    assert_eq!(s.active, Some(NEW15_FJP_ACTIVE));
}

/// Both published maps must name the class they are about, or
/// `verify_declared_slot_maps` sweeps them against the wrong one — the
/// `PB_FIELD_*` misattribution W7-69 §4.4 had to correct by hand.
#[test]
fn the_published_slot_maps_name_their_own_classes() {
    assert_eq!(NEW15_CONT_SLOT_MAP.class, "jdk/internal/vm/Continuation");
    assert_eq!(NEW15_CONT_SLOT_MAP.slots.len(), NEW15_CONT_FIELDS);
    assert_eq!(
        NEW15_FJP_SLOT_MAP.class,
        "java/util/concurrent/ForkJoinPool"
    );
    assert_eq!(NEW15_FJP_SLOT_MAP.slots.len(), NEW15_FJP_FIELDS);
}
