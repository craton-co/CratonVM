use super::*;
use cratonvm_reader::attribute::ExceptionTableEntry;

fn obj(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the record only compares.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn frame(code: &[u8], table: Vec<ExceptionTableEntry>, pc: usize) -> Frame {
    let mut f = Frame::new(
        crate::classloading::ClassId::new(3),
        "probe/T".to_string(),
        "m".to_string(),
        "(Ljava/lang/Object;Ljava/lang/Object;)V".to_string(),
        None,
        code.to_vec(),
        table,
        2,
        4,
        &[],
    );
    f.last_instr_pc = pc;
    f
}



/// SYNC22-4: the arithmetic that decides what a forced return / PopFrames
/// of a frame with an understated record still has to release.
#[test]
fn sync22_unrecorded_among_cancels_the_record_and_doubts_release_nothing() {
    let (a, b) = (obj(0x1000), obj(0x2000));
    // Nothing recorded: every open monitor is unrecorded, innermost first.
    assert_eq!(unrecorded_among(&[Some(a), Some(b)], &[]), vec![b, a]);
    // The record names the outer one: only the inner is left.
    assert_eq!(unrecorded_among(&[Some(a), Some(b)], &[a]), vec![b]);
    // An exact record leaves nothing.
    assert!(unrecorded_among(&[Some(a), Some(b)], &[a, b]).is_empty());
    // One object entered twice: each entry cancels one level.
    assert_eq!(unrecorded_among(&[Some(a), Some(a)], &[a]), vec![a]);
    // Doubts release nothing: a monitor no local is proven to hold, and a
    // recorded entry the bytecode's monitors do not account for.
    assert!(unrecorded_among(&[Some(a), None], &[]).is_empty());
    assert!(unrecorded_among(&[Some(a)], &[b]).is_empty());
    assert!(unrecorded_among(&[], &[a]).is_empty());
    assert!(unrecorded_among(&[], &[]).is_empty());
}

/// SYNC22-4 on a real frame: `synchronized (o) { ... }` resumed from
/// compiled code (nothing recorded) at the instruction after the enter
/// names the lock the local holds; at the exit with the lock temporary
/// the only holder; before the enter and after the exit nothing.
#[test]
fn sync22_a_frame_resumed_inside_a_block_names_the_lock_its_locals_hold() {
    #[rustfmt::skip]
    let code = [
        0x2a, 0x59, 0x4c, 0xc2, // 0: aload_0 dup astore_1 monitorenter
        0x01, 0x4b,             // 4: aconst_null astore_0
        0x2b, 0xc3,             // 6: aload_1 monitorexit
        0xb1,                   // 8: return
    ];
    let lock = obj(0x1000);
    let mut f = frame(&code, Vec::new(), 4);
    f.set_local(0, Value::Object(Some(lock)));
    f.set_local(1, Value::Object(Some(lock)));
    assert_eq!(unrecorded_block_monitors(&f, &[]), vec![lock]);
    assert!(unrecorded_block_monitors(&f, &[lock]).is_empty(), "recorded");
    // Two locals proven to hold the one lock that name two objects: a
    // doubt, so nothing is released.
    f.set_local(0, Value::Object(Some(obj(0x2000))));
    assert!(unrecorded_block_monitors(&f, &[]).is_empty(), "disagreeing aliases");
    // A dead alias reads null and says nothing.
    f.set_local(0, Value::Object(None));
    assert_eq!(unrecorded_block_monitors(&f, &[]), vec![lock]);
    // After `astore_0` the temporary alone holds it.
    f.set_local(0, Value::Object(None));
    f.last_instr_pc = 6;
    assert_eq!(unrecorded_block_monitors(&f, &[]), vec![lock]);
    // A temporary that is no object: no proof, no release.
    f.set_local(1, Value::Int(3));
    assert!(unrecorded_block_monitors(&f, &[]).is_empty());
    // Outside the block nothing is held.
    f.last_instr_pc = 0;
    assert!(unrecorded_block_monitors(&f, &[]).is_empty());
    f.last_instr_pc = 8;
    assert!(unrecorded_block_monitors(&f, &[]).is_empty());
}

/// javac's block, resumed from compiled code with an empty record: the
/// release of the block's own lock goes ahead as before, at both exits.
#[test]
fn a_resumed_javac_block_still_releases_its_own_lock() {
    #[rustfmt::skip]
    let code = [
        0x2a, 0x59, 0x4c, 0xc2,       // 0: aload_0 dup astore_1 monitorenter
        0x2a, 0xb6, 0x00, 0x02, 0x57, // 4: aload_0 invokevirtual pop
        0x2b, 0xc3,                   // 9: aload_1 monitorexit
        0xa7, 0x00, 0x08,             // 11: goto 19
        0x4d, 0x2b, 0xc3, 0x2c, 0xbf, // 14: astore_2 aload_1 monitorexit aload_2 athrow
        0xb1,                         // 19: return
    ];
    let row = |start_pc, end_pc| ExceptionTableEntry {
        start_pc,
        end_pc,
        handler_pc: 14,
        catch_type: 0,
    };
    for pc in [10usize, 16] {
        let f = frame(&code, vec![row(4, 11), row(14, 17)], pc);
        assert!(!frame_record_is_complete(&f), "pc {pc}");
    }
}

/// Review rev-vmjit item 1: a stale entry in a resumed javac block's
/// record must never turn its own release into a throw (javac's catch-any
/// covers that exit, so it would loop forever). The method is balanced,
/// so the record's length is never trusted there, stale entry or not.
#[test]
fn a_stale_entry_in_a_balanced_method_still_releases() {
    #[rustfmt::skip]
    let code = [
        0x2a, 0x59, 0x4c, 0xc2,
        0x2a, 0xb6, 0x00, 0x02, 0x57,
        0x2b, 0xc3,
        0xa7, 0x00, 0x08,
        0x4d, 0x2b, 0xc3, 0x2c, 0xbf,
        0xb1,
    ];
    let row = |start_pc, end_pc| ExceptionTableEntry {
        start_pc,
        end_pc,
        handler_pc: 14,
        catch_type: 0,
    };
    for pc in [10usize, 16] {
        let mut f = frame(&code, vec![row(4, 11), row(14, 17)], pc);
        f.held_monitors.push(obj(0xdead_0000)); // stale: no longer a held object
        assert!(!frame_record_is_complete(&f), "pc {pc}");
    }
}

/// An unbalanced method whose record holds a stale entry: the table does
/// not back it, so the record is not trusted and the exit releases.
#[test]
fn a_record_the_table_does_not_back_is_not_trusted() {
    let mut f = frame(&[0x2a, 0xc2, 0x2b, 0xc3, 0xb1], Vec::new(), 3);
    let stale = obj(0xdead_0000);
    let live = obj(0x1000);
    f.held_monitors.push(stale);
    assert!(!record_backed_by_table(&f, |_| 0, |_| 1), "a stale entry");
    let mut g = frame(&[0x2a, 0xc2, 0x2b, 0xc3, 0xb1], Vec::new(), 3);
    g.held_monitors.push(live);
    assert!(record_backed_by_table(&g, |o| usize::from(o == live), |_| 1));
    // Held, but named by more records than the table counts.
    assert!(!record_backed_by_table(&g, |o| usize::from(o == live), |_| 2));
}


