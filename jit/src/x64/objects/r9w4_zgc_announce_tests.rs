use super::*;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

#[test]
fn the_post_init_mode_follows_the_three_questions() {
    let wired = 0x1234usize;
    assert_eq!(tlab_post_init_mode(true, true, wired), TlabPostInit::Skip);
    assert_eq!(tlab_post_init_mode(true, true, 0), TlabPostInit::Skip);
    // ZGC: a no-op class still has to be announced.
    assert_eq!(
        tlab_post_init_mode(false, true, wired),
        TlabPostInit::ZgcAnnounce
    );
    // ...and without the thin helper it keeps the full one.
    assert_eq!(tlab_post_init_mode(false, true, 0), TlabPostInit::Helper);
    // A class with real post-init work never takes the announce helper:
    // it writes no primitive default and registers no finalizer.
    assert_eq!(
        tlab_post_init_mode(false, false, wired),
        TlabPostInit::Helper
    );
    assert_eq!(tlab_post_init_mode(false, false, 0), TlabPostInit::Helper);
}

static NOTE_CALLS: AtomicUsize = AtomicUsize::new(0);
static NOTE_ARGS: [AtomicI64; 3] = [AtomicI64::new(0), AtomicI64::new(0), AtomicI64::new(0)];
static POST_INIT_CALLS: AtomicUsize = AtomicUsize::new(0);
static POST_INIT_ARGS: [AtomicI64; 4] = [
    AtomicI64::new(0),
    AtomicI64::new(0),
    AtomicI64::new(0),
    AtomicI64::new(0),
];
static NEW_OBJECT_CALLS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: extern "C" test stub; records its integer arguments in statics
// and dereferences nothing.
unsafe extern "C" fn stub_note(vm: i64, obj: i64, size: i64) -> i64 {
    NOTE_CALLS.fetch_add(1, Ordering::SeqCst);
    NOTE_ARGS[0].store(vm, Ordering::SeqCst);
    NOTE_ARGS[1].store(obj, Ordering::SeqCst);
    NOTE_ARGS[2].store(size, Ordering::SeqCst);
    obj
}
// SAFETY: as above.
unsafe extern "C" fn stub_post_init(vm: i64, obj: i64, cid: i64, nf: i64) -> i64 {
    POST_INIT_CALLS.fetch_add(1, Ordering::SeqCst);
    POST_INIT_ARGS[0].store(vm, Ordering::SeqCst);
    POST_INIT_ARGS[1].store(obj, Ordering::SeqCst);
    POST_INIT_ARGS[2].store(cid, Ordering::SeqCst);
    POST_INIT_ARGS[3].store(nf, Ordering::SeqCst);
    obj
}
// SAFETY: as above; returns a sentinel no bump can produce.
unsafe extern "C" fn stub_new_object(_vm: i64, _cid: i64, _nf: i64) -> i64 {
    NEW_OBJECT_CALLS.fetch_add(1, Ordering::SeqCst);
    0x0BAD_0B1E
}
// SAFETY: as above.
unsafe extern "C" fn stub_null_thread() -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}

/// Far above any class a test registers a compact layout for, so the
/// allocation is the legacy `HEADER_SIZE + n * SLOT_SIZE` shape.
const CID: u32 = 0x0765_4321;
const NF: usize = 2;
const VM: i64 = 0x0C0F_FEE0;

/// Emit `emit_inline_tlab_new(CID, NF, mode)` inside a tiny frame of its
/// own, run it against a fake thread (`[cursor, end]`), and return RAX.
fn run(mode: TlabPostInit, thread: &mut [u64; 2]) -> i64 {
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    // Cast: fn pointer to usize helper address (all five below).
    c.helpers.get_current_thread = stub_null_thread as *const () as usize;
    c.helpers.tlab_post_init = stub_post_init as *const () as usize;
    c.helpers.new_object = stub_new_object as *const () as usize;
    c.helpers.zgc_note_tlab_object = stub_note as *const () as usize;
    c.helpers.tlab_cursor_offset_in_thread = 0;
    c.helpers.tlab_end_offset_in_thread = 8;
    c.helpers.class_id_offset_in_obj = 0;
    // [rbp-8] = vm_ptr, [rbp-16] = the cached JvmThread*. A non-null
    // cached slot means the emitter never fetches the thread itself.
    c.heap_local_offset = 8;
    c.jit_thread_slot_off = 16;

    // `Compiler::new` already emitted a prologue; enter after it.
    let entry_off = c.buf.pos();
    c.buf.emit(&[0x55]); // push rbp
    c.buf.emit(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    c.buf.emit(&[0x48, 0x83, 0xEC, 0x40]); // sub rsp, 0x40: aligned, with shadow space
    c.emit_store_local(8, ARG_REGS[0]);
    c.emit_store_local(16, ARG_REGS[1]);
    // Without the inline start-bit store whatever the environment says:
    // this fake thread is only `[cursor, end]` (that arm has its own
    // test, `r9w8_zgc_inline_announce_tests`).
    c.emit_inline_tlab_new_with(CID, NF, mode, false, None);
    c.buf.emit(&[0xC9, 0xC3]); // leave; ret
    assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
    assert!(c.buf.codegen_failure_reason().is_none());

    crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: `entry_off` is a byte offset inside the same allocation.
    let entry = unsafe { c.buf.as_ptr().add(entry_off) };
    // SAFETY: the snippet builds and tears down its own frame, takes two
    // i64 arguments in the platform's first two argument registers, writes
    // only through the fake thread and the TLAB it points at (both alive
    // across the call), calls only the stubs above, and returns an i64.
    // `c` owns the buffer and outlives the call.
    let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
    crate::x64::publish_jit_thread_mirror(thread.as_mut_ptr() as usize);
    let r = f(VM, thread.as_mut_ptr() as i64);
    crate::x64::publish_jit_thread_mirror(0);
    drop(c);
    r
}

/// Round 9 wave 4 (`NOTES-w3-x64obj3.md` request 3): the ZGC mode bumps
/// and publishes the header exactly as the skip arm does, then calls
/// `zgc_note_tlab_object(vm_ptr, obj_ptr, size_bytes)` with the footprint
/// the bump advanced the cursor by, and returns the helper's answer. It
/// never reaches `tlab_post_init` or `new_object`. The other two modes are
/// run through the same harness as controls.
#[test]
fn the_zgc_mode_announces_the_bumped_object_and_nothing_else() {
    if !inline_tlab_new_enabled() {
        return; // opted out by the environment: the arm is a bare helper call
    }
    let size = (HEADER_SIZE + NF * SLOT_SIZE) as i64;
    for mode in [
        TlabPostInit::ZgcAnnounce,
        TlabPostInit::Helper,
        TlabPostInit::Skip,
    ] {
        NOTE_CALLS.store(0, Ordering::SeqCst);
        POST_INIT_CALLS.store(0, Ordering::SeqCst);
        NEW_OBJECT_CALLS.store(0, Ordering::SeqCst);
        let mut heap = vec![0u64; 64];
        let base = heap.as_mut_ptr() as i64;
        let mut thread: [u64; 2] = [base as u64, base as u64 + 512];

        let r = run(mode, &mut thread);

        assert_eq!(r, base, "{mode:?}: RAX is the bumped object");
        assert_eq!(
            thread[0] as i64,
            base + size,
            "{mode:?}: the cursor advanced by one object"
        );
        assert_eq!(heap[0] as u32, CID, "{mode:?}: class id published");
        assert_eq!(
            NEW_OBJECT_CALLS.load(Ordering::SeqCst),
            0,
            "{mode:?}: no slow path"
        );
        match mode {
            TlabPostInit::ZgcAnnounce => {
                assert_eq!(NOTE_CALLS.load(Ordering::SeqCst), 1);
                assert_eq!(POST_INIT_CALLS.load(Ordering::SeqCst), 0);
                assert_eq!(NOTE_ARGS[0].load(Ordering::SeqCst), VM, "vm_ptr");
                assert_eq!(NOTE_ARGS[1].load(Ordering::SeqCst), base, "obj_ptr");
                assert_eq!(NOTE_ARGS[2].load(Ordering::SeqCst), size, "size_bytes");
            }
            TlabPostInit::Helper => {
                assert_eq!(NOTE_CALLS.load(Ordering::SeqCst), 0);
                assert_eq!(POST_INIT_CALLS.load(Ordering::SeqCst), 1);
                assert_eq!(POST_INIT_ARGS[0].load(Ordering::SeqCst), VM);
                assert_eq!(POST_INIT_ARGS[1].load(Ordering::SeqCst), base);
                assert_eq!(POST_INIT_ARGS[2].load(Ordering::SeqCst), i64::from(CID));
                assert_eq!(POST_INIT_ARGS[3].load(Ordering::SeqCst), NF as i64);
            }
            TlabPostInit::Skip => {
                assert_eq!(NOTE_CALLS.load(Ordering::SeqCst), 0);
                assert_eq!(POST_INIT_CALLS.load(Ordering::SeqCst), 0);
            }
            // zt t10/ca2: not in the mode list above (it needs a cached
            // thread slot; its own tests cover it).
            TlabPostInit::ZgcRegisterThenHelper => unreachable!("not run here"),
        }
        drop(heap);
    }
}

/// What the decline edges of the snippet below return: no bump and no
/// helper can produce it.
const DECLINED: i64 = -1;

/// The DECLINE form (round 12 wave 1, lane `calls`): the bump inside a
/// tiny frame whose every decline edge lands on `MOV RAX, DECLINED`.
fn run_declining(thread: &mut [u64; 2], thread_slot: u64) -> i64 {
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    // Cast: fn pointer to usize helper address (all three below).
    c.helpers.get_current_thread = stub_null_thread as *const () as usize;
    c.helpers.tlab_post_init = stub_post_init as *const () as usize;
    c.helpers.new_object = stub_new_object as *const () as usize;
    c.helpers.tlab_cursor_offset_in_thread = 0;
    c.helpers.tlab_end_offset_in_thread = 8;
    c.helpers.class_id_offset_in_obj = 0;
    c.heap_local_offset = 8;
    c.jit_thread_slot_off = 16;

    let entry_off = c.buf.pos();
    c.buf.emit(&[0x55]); // push rbp
    c.buf.emit(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    c.buf.emit(&[0x48, 0x83, 0xEC, 0x40]); // sub rsp, 0x40
    c.emit_store_local(8, ARG_REGS[0]);
    c.emit_store_local(16, ARG_REGS[1]);
    let mut decline: Vec<usize> = Vec::new();
    c.emit_inline_tlab_new_with(CID, NF, TlabPostInit::Skip, false, Some(&mut decline));
    let done = c.emit_jmp_rel32_patch();
    for p in decline {
        c.patch_rel32_to_here(p);
    }
    c.emit_mov_imm64(RAX, DECLINED);
    c.patch_rel32_to_here(done);
    c.buf.emit(&[0xC9, 0xC3]); // leave; ret
    assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
    assert!(c.buf.codegen_failure_reason().is_none());

    crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: `entry_off` is a byte offset inside the same allocation.
    let entry = unsafe { c.buf.as_ptr().add(entry_off) };
    // SAFETY: as `run` above; the snippet calls nothing at all.
    let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
    let slot = if thread_slot == 0 {
        0
    } else {
        thread.as_mut_ptr() as i64
    };
    crate::x64::publish_jit_thread_mirror(slot as usize);
    let r = f(VM, slot);
    crate::x64::publish_jit_thread_mirror(0);
    drop(c);
    r
}

/// The decline form bumps exactly as the `new` site does when the TLAB
/// has room, and otherwise lands on the CALLER's edge with the cursor
/// untouched: a full TLAB and a null cached thread both decline, and
/// neither ever reaches `new_object` (whose stub answers a sentinel no
/// decline returns).
#[test]
fn r12_the_decline_form_bumps_or_declines_and_never_calls() {
    if !inline_tlab_new_enabled() {
        return; // opted out by the environment: the form declines outright
    }
    let mut heap = vec![0u64; 64];
    let base = heap.as_mut_ptr() as u64;

    let mut thread: [u64; 2] = [base, base + 512];
    assert_eq!(
        run_declining(&mut thread, 1),
        base as i64,
        "room: the object"
    );
    // The footprint is the `new` site's own (the ZGC test above pins its
    // value); read it back rather than restate it.
    let size = thread[0] - base;
    assert!(
        size >= 16 && size % 8 == 0,
        "room: the cursor advanced by {size}"
    );
    assert_eq!(heap[0] as u32, CID, "room: class id published");

    let mut full: [u64; 2] = [base, base + size - 8];
    assert_eq!(run_declining(&mut full, 1), DECLINED, "full TLAB declines");
    assert_eq!(full[0], base, "full TLAB: the cursor is untouched");

    let mut unused: [u64; 2] = [base, base + 512];
    assert_eq!(
        run_declining(&mut unused, 0),
        DECLINED,
        "null thread declines"
    );
    assert_eq!(unused[0], base, "null thread: nothing bumped");
    drop(heap);
}
