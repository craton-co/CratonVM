//! gc-common w9-c: the allocations of constant resolution beyond `ldc`'s
//! String arms -- a `CONSTANT_MethodType`'s descriptor, a
//! `CONSTANT_MethodHandle`'s member name and mirrors, a condy's and an
//! indy's bootstrap name and static arguments -- follow `new`'s contract
//! (`common-w8v-constant-resolution-strings-abort-on-a-full-heap`).
//!
//! The unit-test VM has no JDK, so `MethodHandles.lookup()` (which every
//! bootstrap and `MethodHandle` resolution calls first) cannot run here;
//! the tests drive the helpers those resolutions call, on a heap held full
//! by pins, and ratchet the files against the infallible constructors.

use super::*;
use crate::config::{GcAlgorithm, VmConfig};
use crate::threading::jvm_thread::ThreadId;

pub(crate) fn small_gen_vm() -> SharedVm {
    SharedVm::new(VmConfig {
        gc_algorithm: GcAlgorithm::Generational,
        max_heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        ..VmConfig::default()
    })
}

/// Fill `shared`'s heap with objects pinned on `thread` until a forced
/// collection frees nothing (the `w8v_ldc_literal_oom_tests` recipe: both
/// the young path and the whole-heap path, collecting between passes,
/// because a young collection promotes the pins and empties eden). `true`
/// once neither path can place a one-slot object.
pub(crate) fn fill_heap_with_pins(shared: &SharedVm, thread: &mut JvmThread) -> bool {
    // JIT r20 lane gcI: a LOWER bound of the live bytes pinned so far
    // (8 bytes per slot, headers ignored). Above the 8 MB `-Xmx` it proves
    // the heap admitted more live data than its maximum: a VM defect, not
    // a slow fill, so it fails loudly instead of reading as "never full".
    let mut pinned_lower_bound = 0usize;
    for _ in 0..64 {
        for full_heap in [false, true] {
            for slots in [1024usize, 64, 1] {
                for _ in 0..1_000_000 {
                    let heap = &shared.mem.heap;
                    let got = if full_heap {
                        heap.try_alloc_object_full(ClassId::new(0), slots)
                    } else {
                        heap.try_alloc_object(ClassId::new(0), slots)
                    };
                    match got {
                        Some(o) => {
                            thread.native_pin_roots.push(o);
                            pinned_lower_bound += slots * 8;
                            assert!(
                                pinned_lower_bound <= 8 * 1024 * 1024,
                                "the 8 MB heap admitted more than 8 MB of pinned live data"
                            );
                        }
                        None => break,
                    }
                }
            }
        }
        crate::runtime::interpreter::maybe_gc_forced_pub_at(shared, thread, "w9c-test-fill");
        // JIT r20 lane gcI: a probe that SUCCEEDS is pinned too. Left
        // unpinned it is garbage the next forced collection frees, which
        // re-opens exactly one object's room for the next probe: since the
        // old generation may grow past its 2/3 cap on a refused request
        // (`OldGen::grow_after_refusal`), the first post-growth probe
        // succeeds and the recipe oscillated on its own garbage for all
        // 64 passes. Pinned, every pass only ever adds live data, so the
        // fill is monotone and ends once the (bounded) heap is full.
        let heap = &shared.mem.heap;
        match heap.try_alloc_object(ClassId::new(0), 1) {
            Some(o) => thread.native_pin_roots.push(o),
            None => match heap.try_alloc_object_full(ClassId::new(0), 1) {
                Some(o) => thread.native_pin_roots.push(o),
                None => return true,
            },
        }
    }
    false
}

fn assert_java_heap_space<T: std::fmt::Debug>(got: Result<T, MethodCallFailed>) {
    match got {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError { message },
        ))) => assert!(message.starts_with("Java heap space"), "{message}"),
        Ok(v) => panic!("a pinned-full heap cannot hold it, got {v:?}"),
        Err(other) => panic!("expected OutOfMemoryError, got {other:?}"),
    }
}

/// A class id no mirror exists for. The mirror code does not require the
/// class to be loaded (it names an unknown id `unknown_<n>`), and the id
/// sits below the lambda-proxy range.
const NO_MIRROR_YET: u32 = 0x0765_4321;

/// A cached mirror is answered again, identical, without allocating.
#[test]
fn class_mirror_or_oom_answers_the_cached_mirror() {
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w9c-mirror-cache");
    let id = ClassId::new(NO_MIRROR_YET);
    let a = class_mirror_or_oom(&shared, &mut thread, id, "w9c-test").expect("room");
    let b = class_mirror_or_oom(&shared, &mut thread, id, "w9c-test").expect("hit");
    assert_eq!(a, b);
    assert_eq!(
        shared.classes.class_mirrors.read().get(&id).copied(),
        Some(a)
    );
}

/// On a full heap the mirror is refused with `OutOfMemoryError` after the
/// ladder, the cache is left without a row, and no pin is left behind.
#[test]
fn class_mirror_or_oom_on_a_full_heap_is_an_oome() {
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w9c-mirror-full");
    // Prime `java/lang/Class` and the cached mirror width on an empty
    // heap, so the refusal below is the mirror's own allocation.
    class_mirror_or_oom(
        &shared,
        &mut thread,
        ClassId::new(NO_MIRROR_YET + 1),
        "w9c-prime",
    )
    .expect("an empty heap has room for a mirror");
    assert!(
        fill_heap_with_pins(&shared, &mut thread),
        "the pinned objects never filled the 8 MB heap"
    );
    let pins = thread.native_pin_roots.len();
    let id = ClassId::new(NO_MIRROR_YET);
    assert_java_heap_space(class_mirror_or_oom(&shared, &mut thread, id, "w9c-test"));
    assert!(shared.classes.class_mirrors.read().get(&id).is_none());
    assert_eq!(thread.native_pin_roots.len(), pins);
}

/// A surrogate-bearing String static argument (condy) on a full heap is
/// refused with `OutOfMemoryError`; the process-wide surrogate pool is not
/// touched, because the claim runs only after a successful allocation.
#[test]
fn a_wide_literal_on_a_full_heap_is_an_oome() {
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "w9c-wide-full");
    assert!(
        fill_heap_with_pins(&shared, &mut thread),
        "the pinned objects never filled the 8 MB heap"
    );
    // A lone high surrogate, then text no other test uses.
    let mut units: Vec<u16> = vec![0xD800];
    units.extend("w9c-wide-literal-on-a-full-heap".encode_utf16());
    assert_java_heap_space(intern_wide_string_literal_or_oom(
        &shared,
        &mut thread,
        &units,
    ));
    assert!(
        cratonvm_native_builtins::lang_string::surrogate_intern_probe(
            shared.vm_identity,
            &units
        )
        .is_none()
    );
}

/// The surrogate intern pool is per VM (applied `handoff-w9c-surrogate-intern-pool-per-vm`):
/// each VM answers its own canonical object for a lone-surrogate literal,
/// resolvable in its own global-ref table, and a released VM's rows are gone.
#[test]
fn a_wide_literal_is_canonical_per_vm_and_forgotten_with_its_vm() {
    let a = small_gen_vm();
    let b = small_gen_vm();
    let mut ta = JvmThread::new(ThreadId(0), "w9c-wide-a");
    let mut tb = JvmThread::new(ThreadId(0), "w9c-wide-b");
    let mut units: Vec<u16> = vec![0xD800];
    units.extend("w9c-wide-literal-per-vm".encode_utf16());
    let a1 = intern_wide_string_literal_or_oom(&a, &mut ta, &units).expect("room");
    let a2 = intern_wide_string_literal_or_oom(&a, &mut ta, &units).expect("pooled");
    let b1 = intern_wide_string_literal_or_oom(&b, &mut tb, &units).expect("room");
    let b2 = intern_wide_string_literal_or_oom(&b, &mut tb, &units).expect("pooled");
    assert_eq!(a1, a2, "canonical in VM A");
    assert_eq!(b1, b2, "canonical in VM B, not a fresh String per ldc");
    let probe = cratonvm_native_builtins::lang_string::surrogate_intern_probe;
    let ha = probe(a.vm_identity, &units).expect("A's row");
    let hb = probe(b.vm_identity, &units).expect("B's row");
    assert_eq!(
        a.natives
            .jni_global_refs
            .lock()
            .resolve(ha as crate::native::jni::JObject),
        Some(a1)
    );
    assert_eq!(
        b.natives
            .jni_global_refs
            .lock()
            .resolve(hb as crate::native::jni::JObject),
        Some(b1)
    );
    cratonvm_native_builtins::lang_string::surrogate_intern_forget_vm(a.vm_identity);
    assert!(probe(a.vm_identity, &units).is_none(), "A's rows go with A");
    assert_eq!(probe(b.vm_identity, &units), Some(hb), "B's rows stay");
    cratonvm_native_builtins::lang_string::surrogate_intern_forget_vm(b.vm_identity);
}

/// gcg g2/w item 2 (GC wave 9, lane gcD): `String.intern()` pools a
/// lone-surrogate String under a WEAK handle. `ldc` upgrades a live weak
/// row to a strong one on the same object (a literal lives with its
/// class), and takes over a row whose weak handle no longer resolves
/// (its String died) instead of answering a fresh String per execution.
#[test]
fn gcd_w9_a_wide_literal_upgrades_a_weak_row_and_replaces_a_dead_one() {
    use crate::native::jni::JObject;
    use cratonvm_native_builtins::lang_string::{
        surrogate_intern_claim, surrogate_intern_probe,
    };
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "gcd-w9-wide");
    let vm = shared.vm_identity;

    // A live weak row, as `intern()` leaves it.
    let mut units: Vec<u16> = vec![0xD800];
    units.extend("gcd-w9-wide-weak-row".encode_utf16());
    let interned = create_string_from_units_or_oom(&shared, &mut thread, &units).expect("room");
    let weak = shared.natives.jni_global_refs.lock().add_weak(interned) as usize;
    assert_eq!(surrogate_intern_claim(vm, units.clone(), weak), weak);
    let lit = intern_wide_string_literal_or_oom(&shared, &mut thread, &units).expect("pooled");
    assert_eq!(lit, interned, "the literal is the interned instance");
    let row = surrogate_intern_probe(vm, &units).expect("row");
    assert_ne!(row, weak, "the row was upgraded");
    let refs = shared.natives.jni_global_refs.lock();
    assert!(!refs.is_weak(row as JObject), "the literal's row is strong");
    assert_eq!(refs.resolve(row as JObject), Some(interned));
    assert_eq!(
        refs.resolve(weak as JObject),
        None,
        "the weak handle was released"
    );
    drop(refs);

    // A dead row: a handle that does not resolve, as a cleared weak row's
    // does not. A tagged value no allocation can produce, so a later
    // `add` cannot reuse it (releasing a real handle here would let the
    // allocator hand the same box address to the literal's own handle).
    let mut units2: Vec<u16> = vec![0xDC00];
    units2.extend("gcd-w9-wide-dead-row".encode_utf16());
    let dead: usize = 0x1_0001;
    assert_eq!(
        shared
            .natives
            .jni_global_refs
            .lock()
            .resolve(dead as JObject),
        None
    );
    assert_eq!(surrogate_intern_claim(vm, units2.clone(), dead), dead);
    let a = intern_wide_string_literal_or_oom(&shared, &mut thread, &units2).expect("room");
    let b = intern_wide_string_literal_or_oom(&shared, &mut thread, &units2).expect("pooled");
    assert_eq!(a, b, "one instance once the dead row is replaced");
    let row2 = surrogate_intern_probe(vm, &units2).expect("row");
    assert_ne!(row2, dead);
    assert_eq!(
        shared
            .natives
            .jni_global_refs
            .lock()
            .resolve(row2 as JObject),
        Some(a)
    );
    cratonvm_native_builtins::lang_string::surrogate_intern_forget_vm(vm);
}

/// The production parts of this file and of `invokedynamic.rs` call no
/// infallible String or mirror constructor: each resolution allocation
/// goes through an `_or_oom` helper. This is the page's confirmation grep
/// as a ratchet. It bans a CALL anywhere above the test modules, so it
/// survives restructuring; a `try_` form is not a match.
#[test]
fn resolution_paths_call_no_infallible_constructor() {
    // Line endings normalised, so a CRLF checkout splits the same way.
    fn production_part(src: &str) -> String {
        let src = src.replace("\r\n", "\n");
        src.split("\n#[cfg(test)]\nmod ")
            .next()
            .unwrap_or_default()
            .to_string()
    }
    fn infallible_calls(src: &str, needle: &str) -> usize {
        src.match_indices(needle)
            .filter(|(at, _)| !src[..*at].ends_with("try_"))
            .count()
    }
    let files = [
        (
            "constants.rs",
            production_part(include_str!("../constants.rs")),
        ),
        (
            "invokedynamic.rs",
            production_part(concat!(include_str!("../../invokedynamic/mod.rs"), "\n", include_str!("../../invokedynamic/dbg_indy_all.rs"), "\n", include_str!("../../invokedynamic/execute_invokedynamic_at_uncensused.rs"), "\n", include_str!("../../invokedynamic/object_methods_getter_refusal.rs"), "\n", include_str!("../../invokedynamic/descriptor_param_count_and_last_is.rs"))),
        ),
    ];
    for (file, src) in &files {
        for needle in [
            "create_java_string(",
            "create_java_string_from_units(",
            "create_java_string_uninterned(",
            "get_or_create_class_mirror(",
            "get_or_create_primitive_mirror(",
            ".get_class_mirror(",
        ] {
            assert_eq!(
                infallible_calls(src.as_str(), needle),
                0,
                "{file} calls `{needle}` outside its tests. A constant \
                 resolution runs per entry after start-up, so it must \
                 collect, retry and throw OutOfMemoryError instead \
                 (`intern_string_literal_or_oom`, \
                 `intern_wide_string_literal_or_oom`, \
                 `create_string_from_units_or_oom`, `class_mirror_or_oom`, \
                 `primitive_mirror_or_oom`)"
            );
        }
    }
}
