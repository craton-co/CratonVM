//! Interpreter round i1 wave 16, lane L1: the per-thread `invokedynamic`
//! entry (`IndySiteCache`) lends its `Arc` instead of cloning it, memoises
//! a zero-capture lambda's singleton key, and is tagged with the class-name
//! generation. Parallel tests move the global epochs, so a test whose fill
//! does not stick returns early rather than failing.

use super::*;
use crate::runtime::interpreter::constants::w9c_resolution_oom_tests::small_gen_vm;
use crate::runtime::interpreter::site_cache::{NameEpochs, SiteCache};
use crate::threading::jvm_thread::ThreadId;

const CLASS: u32 = 0x7_1601;
const METHOD: &str = "i16l1";

fn frame_at(class_id: ClassId, pc: usize, stack: &[Value]) -> Frame {
    let mut frame = Frame::new(
        class_id,
        String::new(),
        String::from(METHOD),
        String::from("()V"),
        None,
        vec![0xb1],
        vec![],
        10,
        0,
        &[],
    );
    frame.pc = pc;
    for v in stack {
        let _ = frame.stack.push(*v);
    }
    frame
}

/// Put `make()` into the thread's table until one fill sticks.
fn fill(
    thread: &mut JvmThread,
    class_id: ClassId,
    cp: u16,
    make: impl Fn() -> CachedIndySite,
) -> bool {
    (0..100).any(|_| {
        thread
            .indy_sites
            .put(class_id, cp, IndySiteCache::epochs_now(), make());
        thread.indy_sites.get(class_id, cp).is_some()
    })
}

fn lend(thread: &mut JvmThread, class_id: ClassId, cp: u16) -> Option<Arc<ResolvedCallSite>> {
    match thread.indy_sites.get_mut(class_id, cp) {
        Some(CachedIndySite::Other(slot)) => slot.take(),
        _ => None,
    }
}

/// An object shaped like a `String` — slot 0 a `byte[]`, slot 1 a LATIN1
/// coder, four slots, which is exactly `AbstractStringBuilder`'s layout —
/// passes the speculative shape reader, but the operand renderers copy
/// units only from a `String` by class: a `StringBuilder` concat operand
/// or record component used to render as its whole buffer.
#[test]
fn a_string_shaped_object_is_not_rendered_as_a_string() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let s = create_java_string(&shared, "ok");
    assert!(crate::vm::is_java_lang_string(&shared, s));
    let stand_in = shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Integer")
        .expect("load a stand-in class");
    let buffer =
        shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), cratonvm_types::ArrayElementType::Byte, 3);
    let builder = shared.mem.heap.alloc_object(stand_in, 4);
    shared
        .mem
        .heap
        .set_field(builder, 0, Value::Object(Some(buffer)));
    shared.mem.heap.set_field(builder, 1, Value::Int(0));
    assert_eq!(
        read_java_string_units(&shared.mem.heap, builder).map(|u| u.len()),
        Some(3),
        "the hazard: the shape reader answers for it"
    );
    assert!(!crate::vm::is_java_lang_string(&shared, builder));
    assert!(!crate::vm::is_java_lang_string(&shared, buffer));
    if let Ok(RenderedOperand::Units(units)) =
        render_operand_checked(&shared, None, &Value::Object(Some(builder)), 'L')
    {
        panic!("a non-String was rendered from its buffer: {units:?}");
    }
}

/// The concat path appends a `String` operand's units in place: the same
/// units the reader returns, a lone surrogate included, after whatever
/// `out` already holds; a non-String answers `false` and appends nothing.
#[test]
fn a_string_operand_is_appended_in_place_losslessly() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut out: Vec<u16> = vec![u16::from(b'>')];
    let latin1 = create_java_string(&shared, "hi");
    assert!(crate::vm::append_java_string_units(
        &shared.mem.heap,
        latin1,
        &mut out
    ));
    let wide = [u16::from(b'x'), 0xD800, 0x4E2D];
    let utf16 = crate::vm::create_java_string_from_units(&shared, &wide);
    assert!(crate::vm::append_java_string_units(
        &shared.mem.heap,
        utf16,
        &mut out
    ));
    assert_eq!(
        out,
        vec![
            u16::from(b'>'),
            u16::from(b'h'),
            u16::from(b'i'),
            u16::from(b'x'),
            0xD800,
            0x4E2D
        ]
    );
    assert_eq!(
        read_java_string_units(&shared.mem.heap, utf16).as_deref(),
        Some(&out[3..])
    );
    let ints =
        shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), cratonvm_types::ArrayElementType::Int, 2);
    let before = out.len();
    assert!(!crate::vm::append_java_string_units(
        &shared.mem.heap,
        ints,
        &mut out
    ));
    assert_eq!(out.len(), before);
}

/// Stage 2a of the generic-indy proposal: a linked site resolves
/// `MethodHandle` once, to the id the class table answers, and reuses it.
#[test]
fn a_generic_shape_resolves_method_handle_once() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let shape = GenericIndyShape::new("(Ljava/lang/Object;Z)V");
    assert!(shape.method_handle_class.get().is_none());
    let Ok(first) = shape.method_handle_class(&shared) else {
        return; // A stripped VM without `MethodHandle`.
    };
    assert_eq!(shape.method_handle_class.get(), Some(&first));
    assert_eq!(shape.method_handle_class(&shared).ok(), Some(first));
    assert_eq!(
        shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id("java/lang/invoke/MethodHandle"),
        Some(first)
    );
}

#[test]
fn the_indy_table_is_tagged_with_the_name_generation() {
    assert_eq!(
        std::any::TypeId::of::<IndySiteCache>(),
        std::any::TypeId::of::<SiteCache<CachedIndySite, NameEpochs>>(),
        "a class definition must not drop a warm invokedynamic entry"
    );
}

/// The lend / return protocol: a hit moves the entry's `Arc` out and puts
/// it back; a refill made while it was out wins, and an epoch move drops
/// the loan. No path doubles or leaks a reference.
#[test]
fn a_lent_site_goes_back_only_into_its_own_live_empty_entry() {
    if cratonvm_classloading::any_class_redefined() {
        return; // Latched off process-wide by another test.
    }
    let mut thread = JvmThread::new(ThreadId(0), "i16-l1-lend");
    let class_id = ClassId::new(CLASS);
    let site = Arc::new(ResolvedCallSite::EnumSwitch {
        labels: vec![Arc::from("A")],
    });
    if !fill(&mut thread, class_id, 4, || {
        CachedIndySite::Other(Some(Arc::clone(&site)))
    }) {
        return;
    }
    assert_eq!(Arc::strong_count(&site), 2);
    let Some(loan) = lend(&mut thread, class_id, 4) else {
        return;
    };
    assert_eq!(
        Arc::strong_count(&site),
        2,
        "lending moves, it does not clone"
    );
    assert!(matches!(
        thread.indy_sites.get(class_id, 4),
        Some(CachedIndySite::Other(None))
    ));
    return_lent_indy_site(&mut thread, class_id, 4, loan);
    if !matches!(
        thread.indy_sites.get(class_id, 4),
        Some(CachedIndySite::Other(Some(_)))
    ) {
        return; // The epochs moved while lent: the loan was dropped.
    }
    assert_eq!(Arc::strong_count(&site), 2);

    // A re-entrant execution refilled the entry while it was lent.
    let Some(loan) = lend(&mut thread, class_id, 4) else {
        return;
    };
    thread.indy_sites.put(
        class_id,
        4,
        IndySiteCache::epochs_now(),
        CachedIndySite::Other(Some(Arc::clone(&site))),
    );
    if Arc::strong_count(&site) != 3 {
        return;
    }
    return_lent_indy_site(&mut thread, class_id, 4, loan);
    assert_eq!(
        Arc::strong_count(&site),
        2,
        "the loan is dropped, not doubled"
    );

    // An epoch move while lent: the loan is dropped and the entry misses.
    let Some(loan) = lend(&mut thread, class_id, 4) else {
        return;
    };
    crate::runtime::interpreter::bump_resolution_epoch();
    return_lent_indy_site(&mut thread, class_id, 4, loan);
    assert_eq!(Arc::strong_count(&site), 1);
    assert!(thread.indy_sites.get(class_id, 4).is_none());
}

/// A warm shape-bearing site runs from the thread's entry and leaves the
/// entry holding the `Arc` again, with the refcount where it was.
#[test]
fn a_warm_enum_switch_runs_from_the_thread_entry_and_restores_it() {
    if cratonvm_classloading::any_class_redefined() {
        return;
    }
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "i16-l1-enum");
    let class_id = ClassId::new(CLASS + 1);
    let site = Arc::new(ResolvedCallSite::EnumSwitch {
        labels: vec![Arc::from("A"), Arc::from("B")],
    });
    for _ in 0..100 {
        thread.frames.clear();
        thread
            .frames
            .push(frame_at(class_id, 9, &[Value::Object(None), Value::Int(0)]));
        if !fill(&mut thread, class_id, 6, || {
            CachedIndySite::Other(Some(Arc::clone(&site)))
        }) {
            return;
        }
        // A miss (the epochs moved after the fill) reaches the slow path,
        // which finds no such class: retry.
        if execute_invokedynamic(&shared, &mut thread, 0, 6).is_err() {
            continue;
        }
        // `SwitchBootstraps.enumSwitch` answers -1 for a null target.
        assert_eq!(thread.frames[0].stack.pop().ok(), Some(Value::Int(-1)));
        if matches!(
            thread.indy_sites.get(class_id, 6),
            Some(CachedIndySite::Other(Some(_)))
        ) {
            assert_eq!(Arc::strong_count(&site), 2);
        }
        return;
    }
}

/// A zero-capture lambda's entry keys the singleton by its memo at the
/// instruction's own pc, and by the frame's own key anywhere else.
#[test]
fn a_warm_zero_capture_lambda_keys_its_singleton_by_the_memo_at_its_pc() {
    if cratonvm_classloading::any_class_redefined() {
        return;
    }
    let shared = small_gen_vm();
    let mut thread = JvmThread::new(ThreadId(0), "i16-l1-singleton");
    let class_id = ClassId::new(CLASS + 2);
    let proxy = shared.alloc_lambda_proxy_id();
    const MEMO: u64 = 0x1616_1616_1616_1616;
    let run = |thread: &mut JvmThread, pc: usize| -> Option<ObjectRef> {
        thread.frames.clear();
        thread.frames.push(frame_at(class_id, pc, &[]));
        let filled = fill(thread, class_id, 8, || CachedIndySite::Lambda {
            proxy_class_id: proxy,
            num_captures: 0,
            singleton_key: Some((MEMO, 13)),
            singleton: None,
        });
        if !filled {
            return None;
        }
        execute_invokedynamic(&shared, thread, 0, 8).ok()?;
        match thread.frames[0].stack.pop().ok()? {
            Value::Object(o) => o,
            _ => None,
        }
    };
    let Some(at_memo) = run(&mut thread, 13) else {
        return;
    };
    let minted = lambda_singleton_cache()
        .read()
        .get(&(shared.vm_identity, proxy, MEMO, 13))
        .and_then(|slot| slot.get());
    assert_eq!(minted, Some(at_memo), "the memo keys the singleton");
    // Wave 20: that first hit learnt the row's slot, and a later hit on
    // the live entry answers from it -- the same instance.
    let learnt = matches!(
        thread.indy_sites.get(class_id, 8),
        Some(CachedIndySite::Lambda {
            singleton: Some(_),
            ..
        })
    );
    if learnt {
        thread.frames.clear();
        thread.frames.push(frame_at(class_id, 13, &[]));
        if execute_invokedynamic(&shared, &mut thread, 0, 8).is_ok() {
            assert_eq!(
                thread.frames[0].stack.pop().ok(),
                Some(Value::Object(Some(at_memo))),
                "the learnt slot answers the minted instance"
            );
        }
    }
    if let Some(again) = run(&mut thread, 13) {
        assert_eq!(again, at_memo, "one instance per instruction");
    }
    if let Some(elsewhere) = run(&mut thread, 21) {
        assert_ne!(elsewhere, at_memo, "another pc is another key");
        let own_key = lambda_site_method_key_of(METHOD, "()V");
        let minted = lambda_singleton_cache()
            .read()
            .get(&(shared.vm_identity, proxy, own_key, 21))
            .and_then(|slot| slot.get());
        assert_eq!(minted, Some(elsewhere));
    }
    forget_vm_generic_indy_sites(shared.vm_identity);
}
