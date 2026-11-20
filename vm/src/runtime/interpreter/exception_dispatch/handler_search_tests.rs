//! `find_exception_handler_impl` takes the class-manager lock only when it
//! reaches a covering TYPED row; a covering catch-all ahead of it, or no
//! covering row at all, is answered from the table alone. These pin the
//! JVMS §2.10 search order that restructuring had to preserve: rows in
//! table order, `[start_pc, end_pc)` with an exclusive end, first match
//! wins whether it is typed or catch-all.
use super::{
    find_exception_handler, find_exception_handler_any_pc, find_exception_handler_pc_unknown,
    load_catch_types_at_link, run_jit_callee_handler, search_frame_for_unwind,
    unwind_to_handler, CalleeHandlerMiss, UnwindSearch, JIT_THROW_PC_UNSTAMPED_EXPLICIT,
};
use crate::classloading::resolution::CachedBytecodeMethod;
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use crate::error::MethodCallFailed;
use crate::runtime::frame::Frame;
use crate::threading::jvm_thread::JvmThread;
use crate::types::{ObjectRef, Value};
use crate::vm::SharedVm;
use cratonvm_reader::attribute::ExceptionTableEntry;
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

const A: &str = "cratonvm/test/HandlerOrderExcA";
/// Named by the owner's cp #4 and defined nowhere. Not a JDK or enterprise
/// name, so the global path would not fabricate a stub for it.
const MISSING: &str = "cratonvm/test/HandlerOrderMissingCatchType";

/// A VM with exception classes A and B (unrelated), and an owner class
/// whose constant pool slot 2 is `CONSTANT_Class A` and slot 4 is
/// `CONSTANT_Class MISSING`.
fn fixture() -> (std::sync::Arc<SharedVm>, ClassId, ClassId, ClassId) {
    let shared = std::sync::Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let (exc_a, exc_b) = {
        let mut cm = shared.classes.class_manager.write();
        (
            cm.try_ensure_synthetic_class(A, 0)
                .expect("Compatible mode fabricates"),
            cm.try_ensure_synthetic_class("cratonvm/test/HandlerOrderExcB", 0)
                .expect("Compatible mode fabricates"),
        )
    };
    let owner = {
        let mut cm = shared.classes.class_manager.write();
        let id = cm.class_store.next_id();
        let pool = ConstantPool::new(vec![
            ConstantPoolEntry::Tombstone,
            ConstantPoolEntry::Utf8(A.into()), // 1
            ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
            ConstantPoolEntry::Utf8(MISSING.into()), // 3
            ConstantPoolEntry::ClassReference { name_index: 3 }, // 4
        ]);
        cm.class_store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc("cratonvm/test/HandlerOrderOwner"),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: pool,
            access_flags: ClassAccessFlags::empty(),
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
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
        cm.register_class_name(
            ClassLoaderId::Application,
            "cratonvm/test/HandlerOrderOwner",
            id,
        );
        id
    };
    (shared, owner, exc_a, exc_b)
}

fn row(start_pc: u16, end_pc: u16, handler_pc: u16, catch_type: u16) -> ExceptionTableEntry {
    ExceptionTableEntry {
        start_pc,
        end_pc,
        handler_pc,
        catch_type,
    }
}

fn frame(owner: ClassId, table: Vec<ExceptionTableEntry>) -> Frame {
    Frame::new(
        owner,
        "cratonvm/test/HandlerOrderOwner".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        table,
        4,
        2,
        &[],
    )
}

fn search(shared: &SharedVm, f: &Frame, pc: usize, exc: ObjectRef) -> Option<usize> {
    find_exception_handler(shared, f, pc, exc).map(|(handler, _)| handler)
}

#[test]
fn first_covering_row_wins_in_table_order_typed_or_catch_all() {
    let (shared, owner, exc_a, exc_b) = fixture();
    let a = shared.mem.heap.alloc_object(exc_a, 0);
    let b = shared.mem.heap.alloc_object(exc_b, 0);

    // `catch (A) {..} finally {..}` shape: typed row, then catch-all.
    let typed_then_any = frame(owner, vec![row(0, 10, 30, 2), row(0, 10, 40, 0)]);
    assert_eq!(search(&shared, &typed_then_any, 5, a), Some(30));
    assert_eq!(
        search(&shared, &typed_then_any, 5, b),
        Some(40),
        "a typed row that does not match must fall through to the catch-all after it"
    );

    // A catch-all AHEAD of a matching typed row wins: first match, not
    // best match. This is the row the lock-free early return answers.
    let any_then_typed = frame(owner, vec![row(0, 10, 40, 0), row(0, 10, 30, 2)]);
    assert_eq!(search(&shared, &any_then_typed, 5, a), Some(40));
}

#[test]
fn protected_range_is_start_inclusive_end_exclusive() {
    let (shared, owner, exc_a, _) = fixture();
    let a = shared.mem.heap.alloc_object(exc_a, 0);
    for ct in [0u16, 2] {
        let f = frame(owner, vec![row(5, 10, 40, ct)]);
        assert_eq!(
            search(&shared, &f, 4, a),
            None,
            "catch_type {ct}: pc < start"
        );
        assert_eq!(
            search(&shared, &f, 5, a),
            Some(40),
            "catch_type {ct}: pc == start"
        );
        assert_eq!(
            search(&shared, &f, 9, a),
            Some(40),
            "catch_type {ct}: pc == end-1"
        );
        assert_eq!(
            search(&shared, &f, 10, a),
            None,
            "catch_type {ct}: pc == end"
        );
        assert_eq!(
            search(&shared, &f, super::OSR_FRAME_DECLINED_TO_CATCH, a),
            None,
            "the OSR 'already declined' sentinel must match no row"
        );
    }
}

#[test]
fn rows_not_covering_the_pc_are_skipped_before_and_after_the_lock() {
    let (shared, owner, exc_a, exc_b) = fixture();
    let a = shared.mem.heap.alloc_object(exc_a, 0);
    let b = shared.mem.heap.alloc_object(exc_b, 0);
    // An uncovering catch-all first, then a covering typed row, then an
    // uncovering catch-all again.
    let f = frame(
        owner,
        vec![row(0, 3, 20, 0), row(3, 10, 30, 2), row(10, 20, 50, 0)],
    );
    assert_eq!(search(&shared, &f, 5, a), Some(30));
    assert_eq!(search(&shared, &f, 5, b), None);
    assert_eq!(search(&shared, &f, 1, b), Some(20));
    // No table at all.
    assert_eq!(search(&shared, &frame(owner, vec![]), 0, a), None);
}

fn thread_with(owner: ClassId, table: Vec<ExceptionTableEntry>) -> JvmThread {
    let mut thread = JvmThread::new(crate::ThreadId(0), "test");
    thread.frames.push(frame(owner, table));
    thread
}

fn unwind_search(
    shared: &SharedVm,
    thread: &mut JvmThread,
    pc: usize,
    exc: ObjectRef,
) -> UnwindSearch {
    let pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    let found = search_frame_for_unwind(shared, thread, 0, pc, pin);
    thread.native_pin_roots.truncate(pin);
    found
}

/// The unwinder's search keeps the thread-less search's order and range
/// answers for resolvable catch types, including on the second search of
/// the same row (which may be answered from `thread.cast_sites`).
#[test]
fn unwind_search_agrees_with_the_table_order_search_for_resolvable_rows() {
    let (shared, owner, exc_a, exc_b) = fixture();
    let a = shared.mem.heap.alloc_object(exc_a, 0);
    let b = shared.mem.heap.alloc_object(exc_b, 0);
    let mut thread = thread_with(owner, vec![row(0, 10, 30, 2), row(0, 10, 40, 0)]);
    for _ in 0..2 {
        assert_eq!(
            unwind_search(&shared, &mut thread, 5, a),
            UnwindSearch::Found(30)
        );
        assert_eq!(
            unwind_search(&shared, &mut thread, 5, b),
            UnwindSearch::Found(40)
        );
        assert_eq!(
            unwind_search(&shared, &mut thread, 10, a),
            UnwindSearch::NotFound
        );
    }
    // A cached target must not turn a non-matching class into a match.
    let mut typed_only = thread_with(owner, vec![row(0, 10, 30, 2)]);
    for _ in 0..2 {
        assert_eq!(
            unwind_search(&shared, &mut typed_only, 5, a),
            UnwindSearch::Found(30)
        );
        assert_eq!(
            unwind_search(&shared, &mut typed_only, 5, b),
            UnwindSearch::NotFound
        );
    }
}

/// A refused catch row memoises the refusal per exception class
/// (`CastSite::negative_catch`); the memo answers only that class, and a
/// class the row DOES catch is never recorded as refused, whatever order the
/// two arrive in.
#[test]
fn catch_row_refusal_is_memoised_per_exception_class() {
    let (shared, owner, exc_a, exc_b) = fixture();
    let a = shared.mem.heap.alloc_object(exc_a, 0);
    let b = shared.mem.heap.alloc_object(exc_b, 0);
    let mut t = thread_with(owner, vec![row(0, 10, 30, 2)]);
    for _ in 0..3 {
        assert_eq!(unwind_search(&shared, &mut t, 5, b), UnwindSearch::NotFound);
        assert_eq!(
            unwind_search(&shared, &mut t, 5, a),
            UnwindSearch::Found(30)
        );
    }
    // The table can be latched off or an epoch can move under a parallel
    // test, dropping a put; what must never be seen is a wrong memo.
    if let Some(site) = t.cast_sites.get(owner, 2).copied() {
        assert_ne!(site.negative_catch, Some(exc_a));
        assert!(!site.admits(exc_b));
    }
}

/// JVMS §5.4.3 / HotSpot `exception_handler_for_exception`: a covering row
/// whose catch type cannot be resolved throws the resolution error, which
/// replaces the in-flight exception, and the search restarts in the same
/// frame at that row's handler pc. It used to be read as "no match" and
/// the ORIGINAL exception kept unwinding.
#[test]
fn unresolvable_catch_type_replaces_the_exception_and_restarts_at_its_handler() {
    let (shared, owner, exc_a, _) = fixture();
    let mut thread = thread_with(owner, vec![]);
    if crate::runtime::exceptions::create_exception_object(
        &shared,
        &mut thread,
        "java/lang/NoClassDefFoundError",
        Some("probe"),
    )
    .is_err()
    {
        // Stripped VM: the error cannot be materialized, and the search
        // then keeps the lenient answer by design.
        return;
    }
    let a = shared.mem.heap.alloc_object(exc_a, 0);
    let error_class_name = |obj: ObjectRef| {
        let cid = shared.mem.heap.class_id_of(obj);
        shared
            .classes
            .class_manager
            .read()
            .get_class(cid)
            .map(|c| c.name.to_string())
    };

    // The search alone reports the failure at the failing row's handler.
    let mut t = thread_with(owner, vec![row(0, 10, 30, 4), row(0, 10, 40, 0)]);
    match unwind_search(&shared, &mut t, 5, a) {
        UnwindSearch::ResolutionFailed { handler_pc, error } => {
            assert_eq!(
                handler_pc, 30,
                "the FAILING row's handler, not the catch-all's"
            );
            assert_eq!(
                error_class_name(error).as_deref(),
                Some("java/lang/NoClassDefFoundError")
            );
        }
        other => panic!("expected a resolution failure, got {other:?}"),
    }

    // The unwinder: the error restarts the search at pc 30, which the
    // catch-all over [30, 35) covers, so it resumes at 50 holding the ERROR.
    let mut t = thread_with(owner, vec![row(0, 10, 30, 4), row(30, 35, 50, 0)]);
    let mut frame_idx = 0usize;
    let pins_before = t.native_pin_roots.len();
    unwind_to_handler(&shared, &mut t, &mut frame_idx, 0, a, 5)
        .expect("the restarted search finds the catch-all");
    assert_eq!(
        t.native_pin_roots.len(),
        pins_before,
        "the walk releases its pin"
    );
    assert_eq!(t.frames[0].pc, 50);
    let caught = match t.frames[0].stack.pop() {
        Ok(Value::Object(Some(r))) => r,
        other => panic!("handler frame must hold the throwable, got {other:?}"),
    };
    assert_eq!(
        error_class_name(caught).as_deref(),
        Some("java/lang/NoClassDefFoundError")
    );

    // No row covers the restart pc: the ERROR escapes, not the original.
    let mut t = thread_with(owner, vec![row(0, 10, 30, 4)]);
    let mut frame_idx = 0usize;
    match unwind_to_handler(&shared, &mut t, &mut frame_idx, 0, a, 5) {
        Err(MethodCallFailed::ExceptionThrown(escaped)) => {
            assert_ne!(escaped, a);
            assert_eq!(
                error_class_name(escaped).as_deref(),
                Some("java/lang/NoClassDefFoundError")
            );
        }
        other => panic!("expected the resolution error to escape, got {other:?}"),
    }

    // A handler inside its own failing row cannot loop forever.
    let mut t = thread_with(owner, vec![row(0, 40, 30, 4)]);
    let mut frame_idx = 0usize;
    assert!(matches!(
        unwind_to_handler(&shared, &mut t, &mut frame_idx, 0, a, 5),
        Err(MethodCallFailed::ExceptionThrown(_))
    ));
}

/// `(handler pc, throwable left in the pin)` of a compiled-frame search over
/// `thread.frames[0]`; `pc == None` is the pc-unknown search.
fn compiled_search(
    shared: &SharedVm,
    thread: &mut JvmThread,
    pc: Option<usize>,
    exc: ObjectRef,
) -> (Option<usize>, ObjectRef) {
    let pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    let found = match pc {
        Some(pc) => find_exception_handler_any_pc(shared, thread, 0, pc, pin, true),
        None => find_exception_handler_pc_unknown(shared, thread, 0, pin),
    };
    let now = thread.native_pin_roots[pin];
    thread.native_pin_roots.truncate(pin);
    (found, now)
}

/// The compiled-frame routes' search gives the interpreter unwinder's
/// answers: same table order and ranges for resolvable rows, and a catch
/// type that fails to resolve REPLACES the throwable and restarts the search
/// at the failing row's handler. Until round i1 wave 3 these routes kept the
/// loader-blind lazy load and read the failure as "no match", so a frame
/// and the same frame compiled disagreed.
#[test]
fn compiled_frame_search_matches_the_unwinder_on_resolution_failure() {
    let (shared, owner, exc_a, exc_b) = fixture();
    let mut thread = thread_with(owner, vec![]);
    let a = shared.mem.heap.alloc_object(exc_a, 0);
    let b = shared.mem.heap.alloc_object(exc_b, 0);

    // Resolvable rows: range and order as `find_exception_handler`.
    let mut t = thread_with(owner, vec![row(0, 10, 30, 2), row(0, 10, 40, 0)]);
    assert_eq!(compiled_search(&shared, &mut t, Some(5), a), (Some(30), a));
    assert_eq!(compiled_search(&shared, &mut t, Some(5), b), (Some(40), b));
    assert_eq!(compiled_search(&shared, &mut t, Some(10), a), (None, a));
    // pc unknown: the typed row matches on class; the narrow catch-all
    // does not count.
    let mut t = thread_with(owner, vec![row(3, 10, 30, 2), row(3, 10, 40, 0)]);
    assert_eq!(compiled_search(&shared, &mut t, None, a), (Some(30), a));
    assert_eq!(compiled_search(&shared, &mut t, None, b), (None, b));

    if crate::runtime::exceptions::create_exception_object(
        &shared,
        &mut thread,
        "java/lang/NoClassDefFoundError",
        Some("probe"),
    )
    .is_err()
    {
        // Stripped VM: the error cannot be materialized, and the search
        // then keeps the lenient answer by design.
        return;
    }
    let is_ncdfe = |obj: ObjectRef| {
        let cid = shared.mem.heap.class_id_of(obj);
        shared
            .classes
            .class_manager
            .read()
            .get_class(cid)
            .is_some_and(|c| c.name.to_string() == "java/lang/NoClassDefFoundError")
    };

    // Known pc: the error restarts the search at 30, which the catch-all
    // over [30, 35) covers.
    let mut t = thread_with(owner, vec![row(0, 10, 30, 4), row(30, 35, 50, 0)]);
    let (found, now) = compiled_search(&shared, &mut t, Some(5), a);
    assert_eq!(found, Some(50));
    assert!(is_ncdfe(now), "the handler receives the resolution ERROR");

    // Unknown pc: the typed row is judged on class alone, fails to resolve,
    // and the restart at its handler is a KNOWN pc — so a catch-all that
    // does not span the method is honoured there.
    let (found, now) = compiled_search(&shared, &mut t, None, a);
    assert_eq!(found, Some(50));
    assert!(is_ncdfe(now));

    // Nothing covers the restart pc: the ERROR is what propagates.
    let mut t = thread_with(owner, vec![row(0, 10, 30, 4), row(0, 10, 40, 0)]);
    let (found, now) = compiled_search(&shared, &mut t, Some(5), a);
    assert_eq!(found, None);
    assert_ne!(now, a);
    assert!(is_ncdfe(now));

    // A handler inside its own failing row terminates.
    let mut t = thread_with(owner, vec![row(0, 40, 30, 4)]);
    let (found, now) = compiled_search(&shared, &mut t, Some(5), a);
    assert_eq!(found, None);
    assert!(is_ncdfe(now));
}

/// A static `()V` callee of `owner` whose table is `table`, for
/// [`run_jit_callee_handler`].
fn cached_callee(
    owner: ClassId,
    table: Vec<ExceptionTableEntry>,
) -> std::sync::Arc<CachedBytecodeMethod> {
    let parts = cratonvm_jit_api::CachedMethodParts {
        declaring_class_id: owner,
        class_name: std::sync::Arc::from("cratonvm/test/HandlerOrderOwner"),
        method_name: std::sync::Arc::from("callee"),
        method_descriptor: std::sync::Arc::from("()V"),
        source_file: None,
        code: crate::runtime::frame::padded_bytecode(&[0u8; 40]),
        exception_table: std::sync::Arc::from(table.as_slice()),
        max_stack: 4,
        max_locals: 2,
        num_params: 0,
        is_synchronized: false,
        is_static: true,
    };
    std::sync::Arc::new(CachedBytecodeMethod::from_parts(parts))
}

/// The implicit-signal doors in `jit/helpers.rs` (`try_run_callee_handler`)
/// tell a replaced throwable from a merely relocated one by comparing the
/// miss's throwable with THEIR OWN pin of the original, which
/// `run_jit_callee_handler`'s search never writes. Pins that contract: a
/// resolvable miss hands back the pinned original, a failing catch type
/// hands back a distinct resolution error, and the caller's pin still holds
/// the original either way.
#[test]
fn callee_handler_miss_hands_back_a_replacement_distinct_from_the_callers_pin() {
    let (shared, owner, exc_a, exc_b) = fixture();
    let mut thread = thread_with(owner, vec![]);
    let b = shared.mem.heap.alloc_object(exc_b, 0);

    // The caller's pin, as `try_run_callee_handler` holds it.
    let caller_pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(b);
    // Row #2 (`A`) resolves and does not catch `B`: a definite miss with
    // the original throwable.
    let cached = cached_callee(owner, vec![row(0, 10, 30, 2)]);
    match run_jit_callee_handler(&shared, &mut thread, &cached, 5, b, &[]) {
        Err((CalleeHandlerMiss::NotCaught, now)) => {
            assert_eq!(
                now, thread.native_pin_roots[caller_pin],
                "not a replacement"
            );
        }
        Err(_) => panic!("expected a definite miss"),
        Ok(_) => panic!("row 2 does not catch B"),
    }
    assert_eq!(thread.native_pin_roots.len(), caller_pin + 1);
    thread.native_pin_roots.truncate(caller_pin);

    if crate::runtime::exceptions::create_exception_object(
        &shared,
        &mut thread,
        "java/lang/NoClassDefFoundError",
        Some("probe"),
    )
    .is_err()
    {
        // Stripped VM: no error to replace it with (lenient answer).
        return;
    }
    let a = shared.mem.heap.alloc_object(exc_a, 0);
    let caller_pin = thread.native_pin_roots.len();
    thread.native_pin_roots.push(a);
    // Row #4 names a missing class: its resolution error replaces `a`, and
    // nothing covers the restart pc 30.
    let cached = cached_callee(owner, vec![row(0, 10, 30, 4)]);
    match run_jit_callee_handler(&shared, &mut thread, &cached, 5, a, &[]) {
        Err((CalleeHandlerMiss::NotCaught, now)) => {
            let original = thread.native_pin_roots[caller_pin];
            assert_ne!(now, original, "the resolution error replaced it");
            let cid = shared.mem.heap.class_id_of(now);
            assert_eq!(
                shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(cid)
                    .map(|c| c.name.to_string())
                    .as_deref(),
                Some("java/lang/NoClassDefFoundError")
            );
        }
        Err(_) => panic!("a resolution restart is a definite miss"),
        Ok(_) => panic!("nothing covers the restart pc"),
    }
    assert_eq!(thread.native_pin_roots.len(), caller_pin + 1);
    thread.native_pin_roots.truncate(caller_pin);
}


/// A compiled body with one `PendingException` point at `bci`, for the
/// body-identity tests below.
fn body_with_rethrow_point(bci: u32) -> cratonvm_jit::CompiledMethod {
    use cratonvm_jit::deopt::{DeoptAction, DeoptReason, DeoptimizationPoint, FrameState};
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // ret
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.deopt_points.push(DeoptimizationPoint {
        native_offset: 0,
        bci,
        reason: DeoptReason::PendingException,
        action: DeoptAction::Reinterpret,
        semantics: cratonvm_jit::deopt::ResumeSemantics::for_reason(
            DeoptReason::PendingException,
        ),
        speculation_id: 0,
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
    });
    cm
}

/// Interpreter round i1 wave 11
/// (`interpreter-L7-deopt-sinks-match-stashed-frames-by-name-FIXED-20260925.md`):
/// the drain of a compiled body's throw routes through a stashed
/// exceptional frame only when that body published it. A same-named frame
/// whose point belongs to another body is dropped, and the throw is then
/// routed by the body's own stamp — here one outside every protected
/// range, so it propagates; the body's own frame (bci 5, inside the
/// catch-all's `[0, 10)`) is caught.
#[test]
fn the_exception_drain_takes_only_the_frame_its_own_body_published() {
    use super::{route_jit_signal_exception, JitThrowPc};
    use cratonvm_jit::deopt::{DeoptimizationPoint, ReconstructedFrame};
    let (shared, owner, exc_a, _) = fixture();
    let mut thread = thread_with(owner, vec![]);
    while cratonvm_jit::deopt::take_exceptional_frame_with_point().is_some() {}
    let ran = body_with_rethrow_point(5);
    let other = body_with_rethrow_point(5);
    let own_point = &ran.deopt_points[0] as *const DeoptimizationPoint as usize;
    let other_point = &other.deopt_points[0] as *const DeoptimizationPoint as usize;
    let cached = cached_callee(owner, vec![row(0, 10, 30, 0)]);
    let published = || ReconstructedFrame {
        class_id: 0,
        method_key: "cratonvm/test/HandlerOrderOwner.callee:()V".to_string(),
        bci: 5,
        ..Default::default()
    };
    let a = shared.mem.heap.alloc_object(exc_a, 0);

    // Another body's frame: dropped; the stamp outside every range wins.
    cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, other_point);
    let depth = thread.frames.len();
    let r = route_jit_signal_exception(
        &shared,
        &mut thread,
        0,
        &ran,
        &cached,
        JitThrowPc::OutsideAllRanges(20),
        a,
        &[],
    );
    assert!(
        matches!(r, Err(MethodCallFailed::ExceptionThrown(_))),
        "another body's throw bci must not route this body's exception"
    );
    assert_eq!(thread.frames.len(), depth);
    assert!(
        cratonvm_jit::deopt::take_exceptional_frame_with_point().is_none(),
        "a foreign frame is dropped, not left for a later throw"
    );

    // This body's own frame: its bci is inside the catch-all's range.
    let a = shared.mem.heap.alloc_object(exc_a, 0);
    cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
    let r = route_jit_signal_exception(
        &shared,
        &mut thread,
        0,
        &ran,
        &cached,
        JitThrowPc::OutsideAllRanges(20),
        a,
        &[],
    );
    assert!(
        matches!(r, Ok(super::CachedCallResult::FramePushed)),
        "the body's own frame routes to its handler"
    );
    assert_eq!(thread.frames.len(), depth + 1);
    assert_eq!(thread.frames[depth].pc, 30);
    assert!(cratonvm_jit::deopt::take_exceptional_frame_with_point().is_none());
}

/// Interpreter round i1 wave 24, lane L2
/// (`docs/feature-designs/real-jdk-mode.md`,
/// the exceptional channel): a reason-9 frame naming a lock its compiled
/// code took (`relock == false`) either hands the lock to the handler
/// frame, whose `held_monitors` record then names it, or — when the
/// exception leaves the activation without its handler — releases it, as
/// javac's catch-all would have before rethrowing. Both sinks: the door's
/// drain (`route_jit_signal_exception`) and the compiled-callee handler
/// (`run_jit_callee_handler`).
#[test]
fn the_exception_routes_hand_compiled_locks_to_the_handler_or_release_them() {
    use super::{route_jit_signal_exception, JitThrowPc};
    use cratonvm_jit::deopt::{
        DeoptimizationPoint, FrameValue, MonitorInfo, ReconstructedFrame,
    };
    let (shared, owner, exc_a, _) = fixture();
    let mut thread = thread_with(owner, vec![]);
    let tid = thread.thread_id;
    while cratonvm_jit::deopt::take_exceptional_frame_with_point().is_some() {}
    let ran = body_with_rethrow_point(5);
    let own_point = &ran.deopt_points[0] as *const DeoptimizationPoint as usize;
    let lock = shared.mem.heap.alloc_object(exc_a, 0);
    // Cast: object pointer to the raw word a stash carries.
    let lock_word = lock.as_ptr() as usize as u64;
    let published = || ReconstructedFrame {
        class_id: 0,
        method_key: "cratonvm/test/HandlerOrderOwner.callee:()V".to_string(),
        bci: 5,
        monitors: vec![MonitorInfo {
            object: FrameValue::Object(lock_word),
            lock_depth: 1,
            relock: false,            }],
        ..Default::default()
    };
    let pins = thread.native_pin_roots.len();

    // No row covers bci 5: the exception leaves the activation, and the
    // lock its compiled code took goes with it.
    shared.threads.monitors.enter(lock, tid);
    let a = shared.mem.heap.alloc_object(exc_a, 0);
    cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
    let r = route_jit_signal_exception(
        &shared,
        &mut thread,
        0,
        &ran,
        &cached_callee(owner, vec![row(20, 30, 40, 0)]),
        JitThrowPc::OutsideAllRanges(20),
        a,
        &[],
    );
    assert!(matches!(r, Err(MethodCallFailed::ExceptionThrown(_))));
    assert!(
        !shared.threads.monitors.holds(lock, tid),
        "a propagated exception releases what the activation's compiled code locked"
    );
    assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");

    // The catch-all covers bci 5: the handler frame resumes the activation,
    // keeps the lock, and records it.
    shared.threads.monitors.enter(lock, tid);
    let a = shared.mem.heap.alloc_object(exc_a, 0);
    let depth = thread.frames.len();
    cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
    let r = route_jit_signal_exception(
        &shared,
        &mut thread,
        0,
        &ran,
        &cached_callee(owner, vec![row(0, 10, 30, 0)]),
        JitThrowPc::OutsideAllRanges(20),
        a,
        &[],
    );
    assert!(matches!(r, Ok(super::CachedCallResult::FramePushed)));
    assert_eq!(thread.frames.len(), depth + 1);
    assert!(
        shared.threads.monitors.holds(lock, tid),
        "the handler frame inherits the hold"
    );
    assert_eq!(
        thread.frames[depth].held_monitors.count_of(lock),
        1,
        "the handler frame's record names the compiled code's lock"
    );
    assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
    assert!(shared.threads.monitors.exit(lock, tid).is_ok());
    assert!(
        shared.threads.monitors.exit(lock, tid).is_err(),
        "held exactly once"
    );

    // The compiled-callee handler sink: a miss abandons the claimed
    // activation (the caller propagates or re-runs it), so it releases.
    shared.threads.monitors.enter(lock, tid);
    let b = shared.mem.heap.alloc_object(exc_a, 0);
    cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
    let cached = cached_callee(owner, vec![row(20, 30, 40, 0)]);
    match run_jit_callee_handler(&shared, &mut thread, &cached, 5, b, &[]) {
        Err((CalleeHandlerMiss::NotCaught, _)) => {}
        Err(_) => panic!("a known throw pc no row covers is a definite miss"),
        Ok(_) => panic!("no row covers bci 5"),
    }
    assert!(
        !shared.threads.monitors.holds(lock, tid),
        "a callee-handler miss releases the claimed frame's compiled lock"
    );
    assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
    assert!(cratonvm_jit::deopt::take_exceptional_frame_with_point().is_none());
}

/// A `synchronized` INSTANCE twin of [`cached_callee`].
fn cached_synchronized_instance_callee(
    owner: ClassId,
    table: Vec<ExceptionTableEntry>,
) -> std::sync::Arc<CachedBytecodeMethod> {
    let parts = cratonvm_jit_api::CachedMethodParts {
        declaring_class_id: owner,
        class_name: std::sync::Arc::from("cratonvm/test/HandlerOrderOwner"),
        method_name: std::sync::Arc::from("callee"),
        method_descriptor: std::sync::Arc::from("()V"),
        source_file: None,
        code: crate::runtime::frame::padded_bytecode(&[0u8; 40]),
        exception_table: std::sync::Arc::from(table.as_slice()),
        max_stack: 4,
        max_locals: 2,
        num_params: 0,
        is_synchronized: true,
        is_static: false,
    };
    std::sync::Arc::new(CachedBytecodeMethod::from_parts(parts))
}

/// Round 14 wave 1 (lane sync,
/// `r14w1-sync-handler-sinks-conflate-the-method-monitor-with-local-0-FIXED-20260929.md`):
/// after `astore_0 other` in a synchronized instance method, the handler
/// frame the door's drain pushes keeps `other` as its local 0 and holds
/// the INVOCATION's receiver as its method monitor, never `other`; and the
/// callee-handler sink's mapping keeps a live local 0 as the frame
/// published it.
#[test]
fn a_handler_frame_locks_the_invocation_receiver_not_its_local_0() {
    use super::{precise_handler_frame_for, route_jit_signal_exception, JitThrowPc};
    use cratonvm_jit::deopt::{DeoptimizationPoint, FrameValue, ReconstructedFrame};
    if !super::handler_sink_genuine_receiver_enabled() {
        return;
    }
    let (shared, owner, exc_a, _) = fixture();
    let mut thread = thread_with(owner, vec![]);
    let tid = thread.thread_id;
    while cratonvm_jit::deopt::take_exceptional_frame_with_point().is_some() {}
    let ran = body_with_rethrow_point(5);
    let own_point = &ran.deopt_points[0] as *const DeoptimizationPoint as usize;
    let this = shared.mem.heap.alloc_object(owner, 0);
    let other = shared.mem.heap.alloc_object(owner, 0);
    // Cast: object pointer to the raw word a stash carries.
    let other_word = other.as_ptr() as usize as u64;
    let published = || ReconstructedFrame {
        class_id: 0,
        method_key: "cratonvm/test/HandlerOrderOwner.callee:()V".to_string(),
        bci: 5,
        locals: vec![FrameValue::Object(other_word)],
        ..Default::default()
    };
    let cached = cached_synchronized_instance_callee(owner, vec![row(0, 10, 30, 0)]);
    let pins = thread.native_pin_roots.len();

    // The door's drain.
    let a = shared.mem.heap.alloc_object(exc_a, 0);
    let depth = thread.frames.len();
    cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
    let r = route_jit_signal_exception(
        &shared,
        &mut thread,
        0,
        &ran,
        &cached,
        JitThrowPc::OutsideAllRanges(20),
        a,
        &[Value::Object(Some(this))],
    );
    assert!(matches!(r, Ok(super::CachedCallResult::FramePushed)));
    assert_eq!(thread.frames.len(), depth + 1);
    let handler = &thread.frames[depth];
    assert_eq!(handler.get_local_unchecked(0), Value::Object(Some(other)));
    assert_eq!(
        handler.monitor_on_exit,
        Some(this),
        "the monitor is the receiver's"
    );
    assert!(shared.threads.monitors.holds(this, tid));
    assert!(
        !shared.threads.monitors.holds(other, tid),
        "local 0 is not locked"
    );
    assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
    assert!(shared.threads.monitors.exit(this, tid).is_ok());
    thread.frames.truncate(depth);

    // The callee-handler sink's mapping: a LIVE local 0 is kept.
    cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
    let mut claimed = None;
    match precise_handler_frame_for(
        &shared,
        &mut thread,
        &cached,
        &[Value::Object(Some(this))],
        &mut claimed,
    ) {
        Some((5, super::PreciseHandlerLocals::Mapped(locals))) => {
            assert_eq!(locals.first().copied(), Some(Value::Object(Some(other))));
        }
        _ => panic!("the frame of this method maps"),
    }
    assert!(cratonvm_jit::deopt::take_exceptional_frame_with_point().is_none());
}

/// JIT round 17 wave 2, lane syncdoor2 (SD17-1,
/// `r17-syncdoor-declined-handler-in-self-locking-body-reacquires-FIXED-20261005.md`):
/// a self-locking body's precise exceptional frame that names the METHOD
/// monitor (its stub kept the hold) is adopted by the handler frame as its
/// `monitor_on_exit`, with no second acquire and no block-monitor record;
/// a miss releases it exactly once; the callee-handler sink's claim names
/// it and seeds it the same way.
#[test]
fn a_handed_method_monitor_is_adopted_by_the_handler_frame_or_released_once() {
    use super::{precise_handler_frame_for, route_jit_signal_exception, JitThrowPc};
    use cratonvm_jit::deopt::{
        DeoptimizationPoint, FrameValue, MonitorInfo, ReconstructedFrame,
    };
    let (shared, owner, exc_a, _) = fixture();
    let mut thread = thread_with(owner, vec![]);
    let tid = thread.thread_id;
    while cratonvm_jit::deopt::take_exceptional_frame_with_point().is_some() {}
    let ran = body_with_rethrow_point(5);
    let own_point = &ran.deopt_points[0] as *const DeoptimizationPoint as usize;
    let this = shared.mem.heap.alloc_object(owner, 0);
    // Cast: object pointer to the raw word a stash carries.
    let this_word = this.as_ptr() as usize as u64;
    // The shape `x64/deopt_stubs.rs` publishes: local 0 and the one
    // monitor are the receiver, one taken level.
    let published = || ReconstructedFrame {
        class_id: 0,
        method_key: "cratonvm/test/HandlerOrderOwner.callee:()V".to_string(),
        bci: 5,
        locals: vec![FrameValue::Object(this_word)],
        monitors: vec![MonitorInfo {
            object: FrameValue::Object(this_word),
            lock_depth: 1,
            relock: false,            }],
        ..Default::default()
    };
    let catching = cached_synchronized_instance_callee(owner, vec![row(0, 10, 30, 0)]);
    let missing = cached_synchronized_instance_callee(owner, vec![row(20, 30, 40, 0)]);
    let pins = thread.native_pin_roots.len();
    let receiver = [Value::Object(Some(this))];

    // The door's drain, a handler covers bci 5: the frame owns the one
    // hold the compiled prologue took.
    shared.threads.monitors.enter(this, tid);
    let a = shared.mem.heap.alloc_object(exc_a, 0);
    let depth = thread.frames.len();
    cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
    let r = route_jit_signal_exception(
        &shared,
        &mut thread,
        0,
        &ran,
        &catching,
        JitThrowPc::OutsideAllRanges(20),
        a,
        &receiver,
    );
    assert!(matches!(r, Ok(super::CachedCallResult::FramePushed)));
    assert_eq!(thread.frames.len(), depth + 1);
    assert_eq!(thread.frames[depth].monitor_on_exit, Some(this));
    assert_eq!(
        thread.frames[depth].held_monitors.count_of(this),
        0,
        "the method monitor is not a block monitor"
    );
    assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
    assert!(shared.threads.monitors.exit(this, tid).is_ok());
    assert!(
        shared.threads.monitors.exit(this, tid).is_err(),
        "held exactly once: adopted, not re-acquired"
    );
    thread.frames.truncate(depth);

    // The door's drain, no handler covers bci 5: released once.
    shared.threads.monitors.enter(this, tid);
    let a = shared.mem.heap.alloc_object(exc_a, 0);
    cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
    let r = route_jit_signal_exception(
        &shared,
        &mut thread,
        0,
        &ran,
        &missing,
        JitThrowPc::OutsideAllRanges(20),
        a,
        &receiver,
    );
    assert!(matches!(r, Err(MethodCallFailed::ExceptionThrown(_))));
    assert!(
        !shared.threads.monitors.holds(this, tid),
        "a miss releases it"
    );
    assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");

    // The callee-handler sink, a miss: released once.
    shared.threads.monitors.enter(this, tid);
    let b = shared.mem.heap.alloc_object(exc_a, 0);
    cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
    match run_jit_callee_handler(&shared, &mut thread, &missing, 5, b, &receiver) {
        Err((CalleeHandlerMiss::NotCaught, _)) => {}
        Err(_) => panic!("a known throw pc no row covers is a definite miss"),
        Ok(_) => panic!("no row covers bci 5"),
    }
    assert!(
        !shared.threads.monitors.holds(this, tid),
        "a callee-handler miss releases it"
    );
    assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");

    // The callee-handler sink's claim (its Found arm's half that runs no
    // Java): it names the hold, so the sink skips its acquire, and the
    // seeding makes it the handler frame's `monitor_on_exit`.
    shared.threads.monitors.enter(this, tid);
    cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
    let mut claimed = None;
    assert!(precise_handler_frame_for(
        &shared,
        &mut thread,
        &catching,
        &receiver,
        &mut claimed
    )
    .is_some());
    let locks = claimed.expect("the claim pins the frame's locks");
    assert!(locks.hands_over_method_monitor());
    let mut handler = frame(owner, vec![row(0, 10, 30, 0)]);
    locks.seed_frame_and_unpin(&mut thread, &mut handler);
    assert_eq!(handler.monitor_on_exit, Some(this));
    assert_eq!(handler.held_monitors.count_of(this), 0);
    assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
    assert!(shared.threads.monitors.exit(this, tid).is_ok());
    assert!(
        shared.threads.monitors.exit(this, tid).is_err(),
        "held exactly once"
    );

    // A frame naming a lock on another object, or of a method that is
    // not synchronized, hands nothing over: it stays a block monitor.
    let other = shared.mem.heap.alloc_object(owner, 0);
    let mut foreign_lock = published();
    // Cast: object pointer to the raw word a stash carries.
    foreign_lock.monitors[0].object = FrameValue::Object(other.as_ptr() as usize as u64);
    cratonvm_jit::deopt::restash_exceptional_frame_with_point(foreign_lock, None, own_point);
    let mut claimed = None;
    let _ = precise_handler_frame_for(&shared, &mut thread, &catching, &receiver, &mut claimed);
    let locks = claimed.expect("claimed");
    assert!(!locks.hands_over_method_monitor());
    locks.unpin_keeping_the_locks(&mut thread);
    cratonvm_jit::deopt::restash_exceptional_frame_with_point(published(), None, own_point);
    let mut claimed = None;
    let plain = cached_callee(owner, vec![row(0, 10, 30, 0)]);
    let _ = precise_handler_frame_for(&shared, &mut thread, &plain, &[], &mut claimed);
    let locks = claimed.expect("claimed");
    assert!(!locks.hands_over_method_monitor(), "not synchronized");
    locks.unpin_keeping_the_locks(&mut thread);
    assert_eq!(thread.native_pin_roots.len(), pins, "no pin leaks");
    assert!(cratonvm_jit::deopt::take_exceptional_frame_with_point().is_none());
}


/// Round 11 wave 11 (lane irexc,
/// `r11w10-irexc-reason9-drain-does-not-repair-the-receiver-slot`): the
/// reason-9 drain puts the caller's receiver back into a slot 0 the
/// snapshot published as dead — which is what a synchronized instance
/// method's monitor re-acquire reads — and leaves a live slot 0, a static
/// method's slot 0 and a missing receiver alone.
#[test]
fn a_dead_receiver_slot_is_restored_and_nothing_else_is() {
    use super::restore_dead_receiver;
    use cratonvm_jit::deopt::FrameValue;
    let (shared, owner, _, _) = fixture();
    let this = shared.mem.heap.alloc_object(owner, 0);
    let fallback = [Value::Object(Some(this)), Value::Int(9)];
    let dead = [FrameValue::Undefined, FrameValue::Int(7)];

    // What `ir_deopt_locals` maps `Undefined` to.
    let mut locals = vec![Value::Int(0), Value::Int(7)];
    restore_dead_receiver(&mut locals, &dead, false, &fallback);
    assert_eq!(locals, vec![Value::Object(Some(this)), Value::Int(7)]);

    let mut live = vec![Value::Int(5), Value::Int(7)];
    restore_dead_receiver(
        &mut live,
        &[FrameValue::Int(5), FrameValue::Int(7)],
        false,
        &fallback,
    );
    assert_eq!(
        live,
        vec![Value::Int(5), Value::Int(7)],
        "a live slot 0 is the bytecode's"
    );

    let mut stat = vec![Value::Int(0), Value::Int(7)];
    restore_dead_receiver(&mut stat, &dead, true, &fallback);
    assert_eq!(
        stat,
        vec![Value::Int(0), Value::Int(7)],
        "a static method has no receiver"
    );

    let mut none = vec![Value::Int(0)];
    restore_dead_receiver(&mut none, &dead, false, &[]);
    assert_eq!(none, vec![Value::Int(0)]);
}

/// Define and register a field-less, method-less class `name` for `loader`.
fn define_class(
    shared: &SharedVm,
    name: &str,
    loader: ClassLoaderId,
    constant_pool: ConstantPool,
) -> ClassId {
    let mut cm = shared.classes.class_manager.write();
    let id = cm.class_store.next_id();
    cm.class_store.add(Class {
        id,
        loader_id: loader,
        name: cratonvm_types::intern_arc(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Initialized,
        initializing_thread: None,
        constant_pool,
        access_flags: ClassAccessFlags::empty(),
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods: vec![],
        first_field_index: 0,
        num_total_fields: 0,
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
    cm.register_class_name(loader, name, id);
    id
}

/// The loader-blind name rule is `--compatible`-only for a catch row whose
/// type RESOLVED, as the cast arms' `loader_aware_name_assignable` is: a
/// same-named class from an unrelated loader is caught under
/// `--compatible` (the Spring forked-loader / AssertJ `IntrospectionError`
/// accommodation, unchanged) and NOT under `--real-jdk`, where only
/// `is_subclass_of` admits (HotSpot's answer). Asked twice per mode so the
/// second answer comes from the warm `cast_sites` row.
///
/// Wave 52 (lane L7): the rule is the `NamespacedClassTypes` alignment
/// row's, as the cast arms': a `--compatible` VM with every row on (the
/// third arm) refuses the same-named class as `--real-jdk` does. The
/// first arm turns the row off, so it keeps the loader-blind answer
/// (`CRATONVM_COMPAT_ALIGN=-namespaced-class-types`).
#[test]
fn resolved_catch_row_name_rule_is_compatible_only() {
    const SAME: &str = "cratonvm/test/HandlerNameRuleExc";
    for (mode, rows_on) in [
        (cratonvm_types::compat::CompatibilityMode::Compatible, false),
        (cratonvm_types::compat::CompatibilityMode::JdkOnly, false),
        (cratonvm_types::compat::CompatibilityMode::Compatible, true),
    ] {
        let mut config = crate::config::VmConfig::default();
        config.compatibility_mode = mode;
        // `JdkOnly` rejects the synthetic library; off in both arms so the
        // two VMs differ only in the mode (as `jit_bridge`'s `vm_with`).
        config.use_synthetic_jdk = false;
        let mut shared = SharedVm::new(config);
        // The row is on by default since wave 52 (lane L4), so the
        // loader-blind `--compatible` arm turns it off explicitly; the
        // `--real-jdk` VM keeps the set it was built with.
        if rows_on {
            shared.classes.hotspot_aligned = cratonvm_types::HotSpotAlignments::all();
        } else if !mode.is_jdk_only() {
            shared.classes.hotspot_aligned = cratonvm_types::HotSpotAlignments::defaults()
                .with(
                    cratonvm_types::HotSpotAlignment::NamespacedClassTypes,
                    false,
                );
        }
        let empty_pool = || ConstantPool::new(vec![ConstantPoolEntry::Tombstone]);
        let app = define_class(&shared, SAME, ClassLoaderId::Application, empty_pool());
        let other = define_class(&shared, SAME, ClassLoaderId::UserDefined(77), empty_pool());
        let owner = define_class(
            &shared,
            "cratonvm/test/HandlerNameRuleOwner",
            ClassLoaderId::Application,
            ConstantPool::new(vec![
                ConstantPoolEntry::Tombstone,
                ConstantPoolEntry::Utf8(SAME.into()), // 1
                ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
            ]),
        );
        let exact = shared.mem.heap.alloc_object(app, 0);
        let same_name = shared.mem.heap.alloc_object(other, 0);
        let mut t = thread_with(owner, vec![row(0, 10, 30, 2)]);
        for _ in 0..2 {
            assert_eq!(
                unwind_search(&shared, &mut t, 5, exact),
                UnwindSearch::Found(30),
                "{mode:?}: the resolved class itself"
            );
            let expected = if mode.is_jdk_only() || rows_on {
                UnwindSearch::NotFound
            } else {
                UnwindSearch::Found(30)
            };
            assert_eq!(
                unwind_search(&shared, &mut t, 5, same_name),
                expected,
                "{mode:?}: a same-named class from another loader"
            );
        }
    }
}

/// i5-L5 verifier page: under `--real-jdk` the link step loads every typed
/// catch type through the class's loader (HotSpot's
/// `verify_exception_handler_table`). A catch type that will not load is a
/// linkage error of the class; a loaded non-`Throwable` is a `VerifyError`;
/// a loadable `Throwable` subclass links.
#[test]
fn link_time_catch_type_loading_rejects_a_missing_or_non_throwable_type() {
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    const EXC: &str = "cratonvm/test/LinkCatchExc";
    const PLAIN: &str = "cratonvm/test/LinkCatchNotThrowable";
    const GONE: &str = "cratonvm/test/LinkCatchMissing";

    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
    config.use_synthetic_jdk = false;
    let shared = SharedVm::new(config);
    let empty_pool = || ConstantPool::new(vec![ConstantPoolEntry::Tombstone]);
    let throwable = match shared
        .classes
        .class_manager
        .read()
        .find_bootstrap_class_by_name("java/lang/Throwable")
    {
        Some(id) => id,
        None => define_class(
            &shared,
            "java/lang/Throwable",
            ClassLoaderId::Bootstrap,
            empty_pool(),
        ),
    };
    let exc = define_class(&shared, EXC, ClassLoaderId::Application, empty_pool());
    shared
        .classes
        .class_manager
        .write()
        .set_superclass(exc, Some(throwable));
    define_class(&shared, PLAIN, ClassLoaderId::Application, empty_pool());

    // An owner whose one method has a single typed row naming `catch`.
    let owner_catching = |owner_name: &str, catch: &str| {
        let owner = define_class(
            &shared,
            owner_name,
            ClassLoaderId::Application,
            ConstantPool::new(vec![
                ConstantPoolEntry::Tombstone,
                ConstantPoolEntry::Utf8(catch.into()), // 1
                ConstantPoolEntry::ClassReference { name_index: 1 }, // 2
            ]),
        );
        let method = cratonvm_reader::method::ClassFileMethod {
            access_flags: MethodAccessFlags::STATIC,
            name: std::sync::Arc::from("m"),
            descriptor: std::sync::Arc::from("()V"),
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 0,
                    // nop; return; (handler) pop; return
                    code: cratonvm_reader::ByteView::from_vec(vec![0x00, 0xb1, 0x57, 0xb1]),
                    exception_table: vec![row(0, 1, 2, 2)],
                    attributes: vec![],
                },
            )))],
        };
        if let Some(class) = shared.classes.class_manager.write().get_class_mut(owner) {
            class.methods = vec![method];
        }
        owner
    };
    let mut thread = JvmThread::new(crate::ThreadId(0), "test");

    let ok = owner_catching("cratonvm/test/LinkCatchOwnerOk", EXC);
    assert!(
        load_catch_types_at_link(&shared, &mut thread, ok).is_ok(),
        "a loadable Throwable subclass links"
    );
    let missing = owner_catching("cratonvm/test/LinkCatchOwnerMissing", GONE);
    assert!(
        load_catch_types_at_link(&shared, &mut thread, missing).is_err(),
        "a catch type that will not load fails the link"
    );
    let plain = owner_catching("cratonvm/test/LinkCatchOwnerPlain", PLAIN);
    assert!(
        load_catch_types_at_link(&shared, &mut thread, plain).is_err(),
        "a catch type that is not a Throwable is a VerifyError"
    );
}
