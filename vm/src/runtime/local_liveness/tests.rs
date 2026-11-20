use super::*;

fn mask_at(code: &[u8], table: &[ExceptionTableEntry], pc: usize) -> u64 {
    let arc: Arc<[u8]> = Arc::from(code.to_vec().into_boxed_slice());
    live_locals_mask(&arc, table, 8, [pc, pc]).low_word()
}

/// aload_1; astore_2; return  — after the store both 1 and 2 are dead at
/// `return` (bit 0 is always forced live).
#[test]
fn store_then_never_read_is_dead() {
    let code = [0x2b, 0x4d, 0xb1];
    assert_eq!(mask_at(&code, &[], 2), 1); // return: only forced slot 0
    assert_eq!(mask_at(&code, &[], 0), 0b10 | 1); // aload_1 uses slot 1
}

/// Backward branch keeps the loop variable live across the body:
///   0: aload_1 ; 1: pop ; 2: goto 0
#[test]
fn loop_backedge_keeps_slot_live() {
    let code = [0x2b, 0x57, 0xa7, 0xff, 0xfe]; // goto -2 → pc 0
    assert_eq!(mask_at(&code, &[], 2) & 0b10, 0b10);
}

/// A handler that reads slot 3 keeps it live throughout the try range
/// even though the mainline never reads it.
///   0: nop ; 1: nop ; 2: return ; 3: aload_3 ; 4: athrow
#[test]
fn exception_handler_use_extends_liveness() {
    let code = [0x00, 0x00, 0xb1, 0x2d, 0xbf];
    let table = [ExceptionTableEntry {
        start_pc: 0,
        end_pc: 2,
        handler_pc: 3,
        catch_type: 0,
    }];
    assert_eq!(mask_at(&code, &table, 0) & 0b1000, 0b1000);
    assert_eq!(mask_at(&code, &table, 1) & 0b1000, 0b1000);
    // Outside the guarded range the handler edge is gone and slot 3 dead.
    assert_eq!(mask_at(&code, &table, 2) & 0b1000, 0);
}

/// jsr makes the whole method unanalyzable → all-live.
#[test]
fn jsr_bails_to_all_live() {
    let code = [0xa8, 0x00, 0x03, 0xb1, 0xb1];
    assert_eq!(mask_at(&code, &[], 3), ALL_LIVE | 1);
}

/// lstore_1 kills BOTH slots 1 and 2.
///   0: lload_1 ; 1: lstore_1 ; 2: return
#[test]
fn cat2_store_kills_both_slots() {
    let code = [0x1f, 0x40, 0xb1];
    let m = mask_at(&code, &[], 2);
    assert_eq!(m & 0b110, 0);
    // And lload_1 at 0 uses both.
    let m0 = mask_at(&code, &[], 0);
    assert_eq!(m0 & 0b110, 0b110);
}

/// iinc never kills (read+write).
#[test]
fn iinc_is_a_use() {
    // 0: iinc 2, 1 ; 3: goto 0
    let code = [0x84, 0x02, 0x01, 0xa7, 0xff, 0xfd];
    assert_eq!(mask_at(&code, &[], 0) & 0b100, 0b100);
}

/// H2-CID0-BLOCKED regression: the enhanced-for iterator must stay live at
/// the call the thread parks in.
///
/// `for (Future<Void> job : jobs) job.get(5, MINUTES);` — javac emits a
/// synthetic `Iterator` local (slot 9 in
/// `TestMultiThread.testConcurrentUpdate`) that is read only at the loop
/// head, i.e. only across the BACKWARD branch from after the blocking
/// call. A liveness analysis that did not reach a fixpoint over that back
/// edge would report the slot dead exactly where the thread parks — and
/// this mask is what `Frame::scan_local_objects` filters the blocked-thread
/// root snapshot with, so a `false` here means the collector reclaims a
/// live iterator and the owner wakes to
/// `NoSuchMethodError java/lang/Object.hasNext()Z`. The whole enclosing
/// range is also inside a `try`, whose handler never reads the slot, so
/// the exception successor must not be allowed to kill it either.
///
/// Byte offsets mirror the real method (loop head 7, `Future.get` at 37).
#[test]
fn enhanced_for_iterator_is_live_at_the_blocking_call() {
    #[rustfmt::skip]
    let code = [
        0x19, 0x08,                    //  0: aload 8      (jobs)
        0xb6, 0x01, 0x0c,              //  2: invokevirtual iterator
        0x3a, 0x09,                    //  5: astore 9     (the iterator)
        0x19, 0x09,                    //  7: aload 9      <- loop head
        0xb9, 0x01, 0x10, 0x01, 0x00,  //  9: invokeinterface hasNext
        0x99, 0x00, 0x20,              // 14: ifeq -> 46
        0x19, 0x09,                    // 17: aload 9
        0xb9, 0x01, 0x15, 0x01, 0x00,  // 19: invokeinterface next
        0xc0, 0x01, 0x17,              // 24: checkcast Future
        0x3a, 0x0a,                    // 27: astore 10    (job)
        0x19, 0x0a,                    // 29: aload 10
        0x14, 0x01, 0x4c,              // 31: ldc2_w 5L
        0xb2, 0x01, 0x4e,              // 34: getstatic MINUTES
        0xb9, 0x01, 0x51, 0x04, 0x00,  // 37: invokeinterface Future.get
        0x57,                          // 42: pop
        0xa7, 0xff, 0xdc,              // 43: goto -> 7
        0xb1,                          // 46: return
        0x3a, 0x0b,                    // 47: astore 11    (handler)
        0x19, 0x0b,                    // 49: aload 11
        0xbf,                          // 51: athrow
    ];
    let table = [ExceptionTableEntry {
        start_pc: 0,
        end_pc: 46,
        handler_pc: 47,
        catch_type: 0,
    }];
    let arc: Arc<[u8]> = Arc::from(code.to_vec().into_boxed_slice());
    let iterator = 1u64 << 9;
    for pc in [37usize, 42, 43] {
        let mask = live_locals_mask(&arc, &table, 12, [pc, pc]).low_word();
        assert_ne!(
            mask & iterator,
            0,
            "the enhanced-for iterator (slot 9) must be live at pc={pc}: a blocked \
             thread's root snapshot is filtered by this mask, and dropping it is a \
             use-after-free the owner sees as ClassId(0)",
        );
    }
    // `job` (slot 10) is genuinely dead once `get()` has consumed it — the
    // analysis is doing real work here, not returning ALL_LIVE.
    let at_pop = live_locals_mask(&arc, &table, 12, [42, 42]);
    assert!(!at_pop.is_live(10), "slot 10 is dead after the call");
}

/// The per-thread memo is keyed by code-blob IDENTITY, never by content.
///
/// Two blobs with identical bytes but different exception tables have
/// different liveness (the handler edge keeps slot 3 live), and the memo
/// answering the second from the first would drop a GC root. Each blob is
/// queried twice so the second answer comes from the memo, not the
/// analysis.
#[test]
fn thread_memo_is_keyed_by_code_identity_not_content() {
    let bytes = [0x00u8, 0x00, 0xb1, 0x2d, 0xbf];
    let with_handler: Arc<[u8]> = Arc::from(bytes.to_vec().into_boxed_slice());
    let without_handler: Arc<[u8]> = Arc::from(bytes.to_vec().into_boxed_slice());
    let table = [ExceptionTableEntry {
        start_pc: 0,
        end_pc: 2,
        handler_pc: 3,
        catch_type: 0,
    }];
    for _ in 0..2 {
        let a = live_locals_mask(&with_handler, &table, 8, [0, 0]);
        let b = live_locals_mask(&without_handler, &[], 8, [0, 0]);
        assert!(a.is_live(3), "handler keeps slot 3 live");
        assert!(!b.is_live(3), "no handler, slot 3 dead");
    }
    // A memo hit answers exactly what the uncached analysis answers.
    let fresh = Arc::new(analyze(&with_handler, &table).expect("analyzable"));
    assert_eq!(
        live_locals_mask(&with_handler, &table, 8, [1, 2]).low_word(),
        mask_from(Some(&fresh), [1, 2], true).low_word()
    );
}

/// Unknown / mis-decoding bytecode falls back to all-live rather than
/// under-approximating.
#[test]
fn garbage_bails_to_all_live() {
    let code = [0xfe, 0x00];
    assert_eq!(mask_at(&code, &[], 0), ALL_LIVE | 1);
}

/// The SteadyChurn shape at unit scale: a temp stored in the setup block
/// and never read afterwards is DEAD in the loop that follows.
///   0: aload_1 ; 1: astore_3   (temp := param)
///   2: aload_2 ; 3: pop ; 4: goto 2   (endless loop reading only slot 2)
#[test]
fn scoped_out_temp_is_dead_in_later_loop() {
    let code = [0x2b, 0x4e, 0x2c, 0x57, 0xa7, 0xff, 0xfe];
    let in_loop = mask_at(&code, &[], 2);
    assert_eq!(in_loop & 0b1000, 0, "temp (slot 3) must be dead in loop");
    assert_eq!(in_loop & 0b100, 0b100, "loop variable (slot 2) live");
}

/// A method with more than 64 locals is analysed for ALL its slots up to
/// `MAX_TRACKED_LOCALS` (wave 2 covered only 0..64; before that it fell
/// back to all-live wholesale).
///   0: aload_1 ; 1: astore_3        (temp := param)
///   2: aload 70 ; 4: astore 71      (slots in the second word)
///   6: dload 63 ; 8: pop2           (cat-2 straddling the word boundary)
///   9: aload_2 ; 10: pop ; 11: goto 2
#[test]
fn slots_beyond_64_are_tracked() {
    let code = [
        0x2b, 0x4e, 0x19, 70, 0x3a, 71, 0x18, 63, 0x58, 0x2c, 0x57, 0xa7, 0xff, 0xf7,
    ];
    let arc: Arc<[u8]> = Arc::from(code.to_vec().into_boxed_slice());
    let in_loop = live_locals_mask(&arc, &[], 80, [9, 9]);
    assert_ne!(
        in_loop.low_word(),
        ALL_LIVE,
        "a >64-local method must be analysed"
    );
    assert!(!in_loop.is_live(3), "temp (slot 3) must be dead in loop");
    assert!(in_loop.is_live(2), "loop variable (slot 2) live");
    assert!(in_loop.is_live(63), "dload 63 keeps slot 63 live");
    assert!(in_loop.is_live(64), "... and its upper half, slot 64");
    assert!(
        in_loop.is_live(70),
        "aload 70 at the loop head keeps 70 live"
    );
    assert!(
        !in_loop.is_live(71),
        "slot 71 is stored before any read on every path: dead"
    );
    assert!(!in_loop.is_live(100), "a slot never touched is dead");
    // Past the words the table holds, every slot is live.
    assert!(in_loop.is_live(200));
}

/// Slots from `MAX_TRACKED_LOCALS` up stay untracked (live), and the rows
/// below them are still exact.
///   0: aconst_null ; 1: wide astore 600 ; 5: aload_1 ; 6: pop ; 7: goto 5
#[test]
fn slots_beyond_the_tracked_cap_are_live() {
    let code = [0x01, 0xc4, 0x3a, 0x02, 0x58, 0x2b, 0x57, 0xa7, 0xff, 0xfe];
    let arc: Arc<[u8]> = Arc::from(code.to_vec().into_boxed_slice());
    let in_loop = live_locals_mask(&arc, &[], 601, [5, 5]);
    assert!(in_loop.is_live(1));
    assert!(in_loop.is_live(600), "untracked slot must read as live");
    assert!(!in_loop.is_live(100), "tracked, never read: dead");
    assert!(!in_loop.is_live(511), "the last tracked slot is exact too");
}

/// A method above the old 32 KiB cap is analysed (wave 4 raised the cap to
/// the class-file maximum once the fixpoint became a worklist).
///   0: aload_1 ; 1: astore_3 ; 40_000 x nop ; aload_2 ; pop ; goto -2
#[test]
fn methods_above_32k_are_analysed() {
    let mut code: Vec<u8> = vec![0x2b, 0x4e];
    code.resize(2 + 40_000, 0x00);
    let head = code.len();
    code.extend_from_slice(&[0x2c, 0x57, 0xa7, 0xff, 0xfe]);
    let in_loop = mask_at(&code, &[], head);
    assert_ne!(in_loop, ALL_LIVE | 1, "a 40 KB method must be analysed");
    assert_eq!(in_loop & 0b1000, 0, "temp (slot 3) must be dead in loop");
    assert_eq!(in_loop & 0b100, 0b100, "loop variable (slot 2) live");
    // Liveness reaches back across the 40 000 straight-line nops too.
    assert_eq!(mask_at(&code, &[], 2) & 0b100, 0b100);
}

/// Past the class-file maximum (plus padding) the method is not analysed.
#[test]
fn code_past_the_class_file_limit_falls_back() {
    let code = vec![0x00u8; MAX_CODE_LEN + 1];
    assert_eq!(mask_at(&code, &[], 0), ALL_LIVE | 1);
}

/// Liveness must cross an inner back edge AND the outer one: slot 5 is read
/// only at the outer loop head, so inside the inner loop it is live only
/// through inner exit -> outer back edge -> head.
///   0: aload 5 ; 2: pop ; 3: aload_2 ; 4: pop ; 5: iload_3 ;
///   6: ifne -> 3 ; 9: goto -> 0
#[test]
fn nested_back_edges_reach_the_fixpoint() {
    let code = [
        0x19, 0x05, 0x57, 0x2c, 0x57, 0x1d, 0x9a, 0xff, 0xfd, 0xa7, 0xff, 0xf7,
    ];
    for pc in [3usize, 4, 5, 6, 9] {
        let m = mask_at(&code, &[], pc);
        assert_ne!(m & (1 << 5), 0, "slot 5 must be live at pc={pc}");
        assert_eq!(m & (1 << 4), 0, "slot 4 is never touched at pc={pc}");
    }
    assert_ne!(mask_at(&code, &[], 3) & 0b1100, 0);
}

/// gci i3/il: a static method's slot 0 is its first parameter, not a
/// receiver -- `relay(Object a) { dropStatic(a); ... }` suspended in its
/// `invokestatic` with `a` dead must not root `a`.
///   0: aload_0 ; 1: invokestatic #2 ; 4: return
#[test]
fn gci_i3il_a_static_methods_dead_first_parameter_is_not_forced_live() {
    let code: Arc<[u8]> = Arc::from(vec![0x2a, 0xb8, 0x00, 0x02, 0xb1].into_boxed_slice());
    // Suspended inside the invoke: pc and last_instr_pc both name it.
    let static_mask = live_locals_mask_for(&code, &[], 1, [1, 1], false);
    assert!(!static_mask.is_live(0), "static: slot 0 is dead after its last read");
    // Before the read, liveness keeps it on its own.
    let at_load = live_locals_mask_for(&code, &[], 1, [0, 0], false);
    assert!(at_load.is_live(0), "slot 0 is read at pc 0");
    // An instance method (and the legacy entry point) keeps the receiver.
    let instance_mask = live_locals_mask_for(&code, &[], 1, [1, 1], true);
    assert!(instance_mask.is_live(0), "the receiver stays live");
    assert!(live_locals_mask(&code, &[], 1, [1, 1]).is_live(0));
    // The per-thread memo answers each question for its own caller: the
    // second static query is a memo hit and must not inherit bit 0.
    let again = live_locals_mask_for(&code, &[], 1, [1, 1], false);
    assert!(!again.is_live(0), "the memo must not cache the forced bit");
}

/// An unanalysable method stays all-live for a static frame too.
#[test]
fn gci_i3il_static_fallback_is_still_all_live() {
    let code: Arc<[u8]> = Arc::from(vec![0xfe, 0x00].into_boxed_slice());
    let m = live_locals_mask_for(&code, &[], 1, [0, 0], false);
    assert_eq!(m.low_word(), ALL_LIVE);
}

fn typed_at(code: &[u8], table: &[ExceptionTableEntry], pc: usize, params: usize) -> TypedMask {
    let arc: Arc<[u8]> = Arc::from(code.to_vec().into_boxed_slice());
    typed_locals_mask_for(&arc, table, [pc, pc], params).expect("analyzable")
}

/// gci i10/np: the SteadyChurn shape keeps its temp TYPED in the later
/// loop (both edges into the loop head carry the reference), although
/// liveness calls it dead -- HotSpot's interpreter oop map keeps it too.
///   0: aload_1 ; 1: astore_3 ; 2: aload_2 ; 3: pop ; 4: goto 2
#[test]
fn gci_i10np_a_scoped_out_reference_stays_typed_in_a_later_loop() {
    let code = [0x2b, 0x4e, 0x2c, 0x57, 0xa7, 0xff, 0xfe];
    assert_eq!(mask_at(&code, &[], 2) & 0b1000, 0, "fixture: slot 3 dead");
    let typed = typed_at(&code, &[], 2, 3);
    assert!(typed.is_ref(3), "the temp is a reference on every path");
    assert!(typed.is_ref(1) && typed.is_ref(2), "unwritten parameters keep their type");
    // Before the store the temp slot holds nothing: not a parameter.
    assert!(!typed_at(&code, &[], 1, 3).is_ref(3));
}

/// A reference on one path and nothing on the other is a conflict at the
/// join (GenerateOopMap's ref/uninit merge), unless the slot is a
/// parameter (then both paths hold a reference-or-entry value).
///   0: iload_1 ; 1: ifeq -> 7 ; 4: aload_2 ; 5: astore_3 ; 6: nop ; 7: return
#[test]
fn gci_i10np_a_one_sided_store_is_not_typed_at_the_join() {
    let code = [0x1b, 0x99, 0x00, 0x06, 0x2c, 0x4e, 0x00, 0xb1];
    assert!(typed_at(&code, &[], 6, 3).is_ref(3), "only the storing path reaches pc 6");
    assert!(!typed_at(&code, &[], 7, 3).is_ref(3), "join of ref and unwritten");
    assert!(typed_at(&code, &[], 7, 4).is_ref(3), "a parameter slot keeps its type");
}

/// A primitive store (here `istore_3`, and a `long` store over the pair)
/// ends the reference type.
///   0: aload_1 ; 1: astore_3 ; 2: iconst_0 ; 3: istore_3 ; 4: nop ;
///   5: aload_1 ; 6: astore 4 ; 8: lconst_0 ; 9: lstore_3 ; 10: return
#[test]
fn gci_i10np_a_primitive_store_ends_the_type() {
    let code = [
        0x2b, 0x4e, 0x03, 0x3e, 0x00, 0x2b, 0x3a, 0x04, 0x09, 0x42, 0xb1,
    ];
    assert!(typed_at(&code, &[], 2, 2).is_ref(3));
    assert!(!typed_at(&code, &[], 4, 2).is_ref(3), "istore_3 ends it");
    assert!(typed_at(&code, &[], 8, 2).is_ref(4));
    let at_return = typed_at(&code, &[], 10, 2);
    assert!(!at_return.is_ref(3) && !at_return.is_ref(4), "lstore_3 covers 3 and 4");
}

/// An exception handler meets the state BEFORE each covered instruction:
/// a store inside the try does not type the slot at the handler.
///   0: nop ; 1: aload_1 ; 2: astore_3 ; 3: return ; 4: astore 5 ; 6: return
///   (try [0, 3) -> 4)
#[test]
fn gci_i10np_a_handler_sees_the_pre_store_state() {
    let code = [0x00, 0x2b, 0x4e, 0xb1, 0x3a, 0x05, 0xb1];
    let table = [ExceptionTableEntry {
        start_pc: 0,
        end_pc: 3,
        handler_pc: 4,
        catch_type: 0,
    }];
    assert!(typed_at(&code, &table, 3, 2).is_ref(3), "mainline: stored");
    assert!(!typed_at(&code, &table, 4, 2).is_ref(3), "handler: maybe never stored");
    assert!(typed_at(&code, &table, 6, 2).is_ref(5), "the handler's own astore types slot 5");
}

/// A back edge into pc 0 meets the entry state there.
///   0: aload_1 ; 1: astore_2 ; 2: iconst_0 ; 3: istore_1 ; 4: goto 0
#[test]
fn gci_i10np_a_loop_back_to_the_entry_meets_the_entry_state() {
    let code = [0x2b, 0x4d, 0x03, 0x3c, 0xa7, 0xff, 0xfc];
    let at_entry = typed_at(&code, &[], 0, 2);
    assert!(!at_entry.is_ref(1), "slot 1 is an int after the first iteration");
    assert!(!at_entry.is_ref(2), "slot 2 is unwritten on the entry path");
    assert!(typed_at(&code, &[], 2, 2).is_ref(2));
}

/// The parameter count is read from the descriptor.
#[test]
fn gci_i10np_entry_param_slots_counts_the_descriptor() {
    assert_eq!(entry_param_slots("()V", false), 0);
    assert_eq!(entry_param_slots("()V", true), 1);
    assert_eq!(entry_param_slots("(IJ[Ljava/lang/String;D)V", true), 7);
    assert_eq!(entry_param_slots("([[I[[Ljava/lang/Object;Z)I", false), 3);
    assert_eq!(entry_param_slots("IV", false), usize::MAX);
    assert_eq!(entry_param_slots("(Ljava/lang/Object", false), usize::MAX);
    assert_eq!(entry_param_slots("([", false), usize::MAX);
}

/// An unanalysable method has no type answer (the caller keeps liveness).
#[test]
fn gci_i10np_an_unanalysable_method_has_no_type_answer() {
    let arc: Arc<[u8]> = Arc::from(vec![0xfe, 0x00].into_boxed_slice());
    assert!(typed_locals_mask_for(&arc, &[], [0, 0], 1).is_none());
}
