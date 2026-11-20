use super::*;

const CURSOR_OFF: i32 = 0x40;
const CID: u32 = 0x0765_4323;
/// `OR [RDX + R8*8], R9`: the start-bit store.
const OR_BIT: [u8; 4] = [0x4E, 0x09, 0x0C, 0xC2];

/// `MOV [R10 + CURSOR_OFF], reg` for `reg` = RAX (`modrm` 0x82, the old
/// commit) or RCX (0x8A, `emit_zgc_announce_commit`'s).
fn commit_bytes(modrm: u8) -> Vec<u8> {
    let mut v = vec![0x49, 0x89, modrm];
    v.extend_from_slice(&CURSOR_OFF.to_le_bytes());
    v
}

fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    hay.windows(needle.len())
        .enumerate()
        .filter(|(_, w)| *w == needle)
        .map(|(i, _)| i)
        .collect()
}

fn compiler() -> Compiler {
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    // Never executed: any non-zero, in-reach address will do.
    let any = compiler as *const () as usize; // Cast: fn pointer to a helper address
    c.helpers.get_current_thread = any;
    c.helpers.tlab_post_init = any;
    c.helpers.new_object = any;
    c.helpers.newarray = any;
    c.helpers.zgc_note_tlab_object = any;
    c.helpers.tlab_cursor_offset_in_thread = CURSOR_OFF as usize; // Cast: a small offset
    c.helpers.tlab_end_offset_in_thread = CURSOR_OFF as usize + 8; // Cast: a small offset
    c.helpers.class_id_offset_in_obj = 0;
    c.heap_local_offset = 8;
    c.jit_thread_slot_off = 16;
    c
}







/// What a decline edge of the box snippet returns.
const DECLINED: i64 = -1;

/// The single-pass DECLINE form with `inline_announce = true` -- the
/// box's ZGC announce -- run against a fake thread block
/// (`cursor, end, start, owned epoch, table`) whose address is the cached
/// thread slot. Returns RAX, or `DECLINED` from every decline edge.
fn run_box(thread: &mut [u64; 6]) -> i64 {
    let mut c = compiler();
    c.helpers.tlab_cursor_offset_in_thread = 0;
    c.helpers.tlab_end_offset_in_thread = 8;
    let entry_off = c.buf.pos();
    c.buf.emit(&[0x55, 0x48, 0x89, 0xE5]); // push rbp; mov rbp, rsp
    c.buf.emit(&[0x48, 0x83, 0xEC, 0x40]); // sub rsp, 0x40
    c.emit_store_local(16, ARG_REGS[0]);
    let mut decline: Vec<usize> = Vec::new();
    c.emit_inline_tlab_new_with(CID, 1, TlabPostInit::Skip, true, Some(&mut decline));
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
    // SAFETY: the snippet builds and tears down its own frame, reads the
    // thread block and the table it names, writes the TLAB, the cursor and
    // one bitmap word (all alive across the call), and calls nothing.
    let f: extern "C" fn(i64) -> i64 = unsafe { std::mem::transmute(entry) };
    crate::x64::publish_jit_thread_mirror(thread.as_mut_ptr() as usize);
    let r = f(thread.as_mut_ptr() as i64); // Cast: the thread block's address
    crate::x64::publish_jit_thread_mirror(0);
    drop(c);
    r
}

#[repr(C, align(512))]
struct Chunk([u64; 512]);



/// What the stand-in announce helper saw (reached through the VM-pointer
/// argument, so no process global): the thread's cursor AT the call.
#[repr(C)]
struct HelperProbe {
    thread: *const u64,
    cursor_at_call: u64,
    obj: i64,
    calls: u64,
    answer_zero: bool,
}

// SAFETY: extern "C" test stub; `vm` is the live `HelperProbe` the harness
// passed as the VM pointer, whose `thread` points at the live fake thread.
unsafe extern "C" fn probe_note(vm: i64, obj: i64, _size: i64) -> i64 {
    let p = &mut *(vm as *mut HelperProbe);
    p.cursor_at_call = *p.thread;
    p.obj = obj;
    p.calls += 1;
    if p.answer_zero {
        0
    } else {
        obj
    }
}







/// zt t10/ca2: what the stand-in announce and post-init helpers saw,
/// reached through the VM-pointer argument (no process global).
#[repr(C)]
struct RegisterFirstProbe {
    thread: *const u64,
    answer_zero: bool,
    note_cursor: u64,
    note_calls: u64,
    post_cursor: u64,
    post_obj: i64,
    post_fields: i64,
    post_calls: u64,
    /// 1 = the announce helper ran first, 2 = the post-init helper did.
    first: u64,
}

// SAFETY: extern "C" test stub; `vm` is the live `RegisterFirstProbe` the
// harness passed as the VM pointer, whose `thread` points at the live
// fake thread.
unsafe extern "C" fn rf_note(vm: i64, obj: i64, _size: i64) -> i64 {
    let p = &mut *(vm as *mut RegisterFirstProbe);
    p.note_cursor = *p.thread;
    p.note_calls += 1;
    if p.first == 0 {
        p.first = 1;
    }
    if p.answer_zero {
        0
    } else {
        obj
    }
}

// SAFETY: as `rf_note`.
unsafe extern "C" fn rf_post(vm: i64, obj: i64, _class_id: i64, fields: i64) -> i64 {
    let p = &mut *(vm as *mut RegisterFirstProbe);
    p.post_cursor = *p.thread;
    p.post_obj = obj;
    p.post_fields = fields;
    p.post_calls += 1;
    if p.first == 0 {
        p.first = 2;
    }
    obj
}

/// A compiler for the register-first harness: the thread block's cursor
/// at 0 and end at 8, both helpers the stand-ins above.
fn register_first_compiler() -> Compiler {
    let mut c = compiler();
    c.helpers.zgc_note_tlab_object = rf_note as *const () as usize; // Cast: a helper address
    c.helpers.tlab_post_init = rf_post as *const () as usize; // Cast: a helper address
    c.helpers.tlab_cursor_offset_in_thread = 0;
    c.helpers.tlab_end_offset_in_thread = 8;
    c
}

/// The single-pass `new` of `mode` against an UNARMED fake thread (the
/// inline start-bit store refuses, so the helper arm runs), under `edits`:
/// `(RAX, cursor after, the probe)`, cursors and objects as offsets from
/// the TLAB. The object is at 64.
fn run_register_first(
    edits: &[(&str, Option<&str>)],
    mode: TlabPostInit,
    answer_zero: bool,
) -> (i64, u64, RegisterFirstProbe) {
    let (rax, after, probe, _) = run_register_first_with(edits, mode, answer_zero, false);
    (rax, after, probe)
}

/// [`run_register_first`], with the thread ARMED when `armed` (a published
/// table at the owned epoch, the object's granule inside the owned-word
/// window: the inline start-bit store sets the bit, no announce call).
/// Also returns the table's first bitmap word after the run.
fn run_register_first_with(
    edits: &[(&str, Option<&str>)],
    mode: TlabPostInit,
    answer_zero: bool,
    armed: bool,
) -> (i64, u64, RegisterFirstProbe, u64) {
    let _flag = cratonvm_types::flags::override_thread(
        cratonvm_types::flags::VmFlags::from_env_with_edits(edits),
    );
    let chunk = Box::new(Chunk([0; 512]));
    let base = chunk.0.as_ptr() as u64; // Cast: the TLAB's address
    let mut bitmap = [0u64; 8];
    // words, base, span, blocked, epoch.
    let tab = [bitmap.as_mut_ptr() as u64, base, 4096, 0, 7, 0, 0, 0]; // Cast: an address
    let tab_addr = if armed { tab.as_ptr() as u64 } else { 0 }; // Cast: an address
    // cursor (unaligned by 4: the bump aligns), end, start, epoch, the
    // table (0: unarmed), owned words [0, 8).
    let mut thread = [base + 60, base + 4096, base, 7, tab_addr, 8 << 32];
    let mut probe = RegisterFirstProbe {
        thread: thread.as_ptr(),
        answer_zero,
        note_cursor: 0,
        note_calls: 0,
        post_cursor: 0,
        post_obj: 0,
        post_fields: 0,
        post_calls: 0,
        first: 0,
    };
    let mut c = register_first_compiler();
    let entry_off = c.buf.pos();
    c.buf.emit(&[0x55, 0x48, 0x89, 0xE5]); // push rbp; mov rbp, rsp
    c.buf.emit(&[0x48, 0x83, 0xEC, 0x40]); // sub rsp, 0x40
    c.emit_store_local(8, ARG_REGS[0]);
    c.emit_store_local(16, ARG_REGS[1]);
    c.emit_inline_tlab_new_with(CID, 2, mode, true, None);
    c.buf.emit(&[0xC9, 0xC3]); // leave; ret
    assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
    assert!(c.buf.codegen_failure_reason().is_none());
    crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: `entry_off` is a byte offset inside the same allocation.
    let entry = unsafe { c.buf.as_ptr().add(entry_off) };
    // SAFETY: the snippet builds and tears down its own frame, reads and
    // writes the fake thread and the chunk (alive across the call), and
    // calls only `rf_note` and `rf_post`, which write `probe`.
    let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
    // The TLS mirror is the preferred thread source when available.
    crate::x64::publish_jit_thread_mirror(thread.as_mut_ptr() as usize);
    let r = f(
        &mut probe as *mut RegisterFirstProbe as i64, // Cast: the VM pointer
        thread.as_mut_ptr() as i64,                   // Cast: the thread block
    );
    crate::x64::publish_jit_thread_mirror(0);
    drop(c);
    let off = |a: u64| a.wrapping_sub(base);
    let rax = if r == 0 { 0 } else { r - base as i64 }; // Cast: an address
    probe.note_cursor = off(probe.note_cursor);
    probe.post_cursor = off(probe.post_cursor);
    if probe.post_obj != 0 {
        probe.post_obj -= base as i64; // Cast: an address
    }
    let after = off(thread[0]);
    drop(chunk);
    (rax, after, probe, bitmap[0])
}








#[test]
fn the_committing_store_matches_its_raw_twin_byte_for_byte() {
    for commit in [
        ZgcAnnounceCommit::AlreadyCommitted,
        ZgcAnnounceCommit::Size(40),
        ZgcAnnounceCommit::ArrayFromHeader { log2_elem: 0 },
        ZgcAnnounceCommit::ArrayFromHeader { log2_elem: 3 },
    ] {
        let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let a0 = c.buf.pos();
        let (done_a, help_a) = c.emit_zgc_inline_announce(0x40, 0x48, commit);
        let a = c.buf.as_slice()[a0..c.buf.pos()].to_vec();
        let mut d = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
        let b0 = d.buf.pos();
        let (done_b, help_b) =
            emit_zgc_inline_announce_raw_committing(&mut d.buf, 0x40, 0x48, commit);
        let b = d.buf.as_slice()[b0..d.buf.pos()].to_vec();
        assert_eq!(a, b, "{commit:?}");
        assert_eq!(done_a - a0, done_b - b0, "{commit:?}");
        let rel = |v: &[usize], base: usize| v.iter().map(|p| p - base).collect::<Vec<_>>();
        assert_eq!(rel(&help_a, a0), rel(&help_b, b0), "{commit:?}");
        // The commit sits between the bit and the `MOV RAX, R11; JMP done`.
        let mut or_at = find_all(&a, &OR_BIT);
        if or_at.is_empty() {
            or_at = find_all(&a, &BTS_QWORD_RDX_RCX);
        }
        let rcx_commit = find_all(&a, &commit_bytes(0x8A));
        match commit {
            ZgcAnnounceCommit::AlreadyCommitted => assert!(rcx_commit.is_empty()),
            _ => assert!(
                rcx_commit.len() == 1 && rcx_commit[0] > or_at[0],
                "{commit:?}: {rcx_commit:?} vs {or_at:?}"
            ),
        }
    }
}

/// zt t11/ca3: what the stand-in helpers of the spill-sink harness saw,
/// reached through the VM-pointer argument (no process global).
#[repr(C)]
struct SinkProbe {
    note_calls: u64,
    new_calls: u64,
    /// zt t12/ca4: the object the announce helper was handed.
    note_obj: i64,
}

/// What the stand-in `new_object` returns.
const SINK_NEW_OBJECT: i64 = 0x51DE_0BEC;

// SAFETY: extern "C" test stub; `vm` is the live `SinkProbe` the harness
// passed as the VM pointer.
unsafe extern "C" fn sink_note(vm: i64, obj: i64, _size: i64) -> i64 {
    let p = &mut *(vm as *mut SinkProbe);
    p.note_calls += 1;
    p.note_obj = obj;
    obj
}

// SAFETY: as `sink_note`.
unsafe extern "C" fn sink_new_object(vm: i64, _class_id: i64, _fields: i64) -> i64 {
    let p = &mut *(vm as *mut SinkProbe);
    p.new_calls += 1;
    SINK_NEW_OBJECT
}


/// zt t12/ca4: the withheld half of a ZGC sink site's blind spill, as
/// `emit_deferred_alloc_blind_spill` emits it into `run_sink`'s slots.
fn withheld_zgc_spill_bytes() -> Vec<u8> {
    let mut c = compiler();
    c.reg_spill_base = 0x20;
    c.deferred_alloc_blind_spill = true;
    c.deferred_alloc_zgc_announce = true;
    let start = c.buf.pos();
    assert!(c.emit_deferred_alloc_blind_spill(), "a pending sunk spill");
    c.buf.as_slice()[start..c.buf.pos()].to_vec()
}



/// zt t14/x11 (`zgc-zt13x10-review-residuals-CLOSED-20261019.md` item 11): the
/// withheld half of a ZGC sink site's spill, checked by VALUE. t12/ca4's
/// test compared the site's bytes with `emit_deferred_alloc_blind_spill`'s
/// own output, so a fast path that clobbered a withheld register (a wrong
/// clobber set) changed both sides alike. Here every withheld register
/// holds a sentinel at the site: a refused announce leaves exactly those
/// sentinels in their `ALL_SPILL_GPRS` slots (the registers' values at
/// the safepoint), and the slots of the clobber set keep their marker (the
/// safepoint's half, not emitted by this harness). Positive control: the
/// armed fast path makes no call and writes no slot.
///
/// t1 wave 11 lane GCC (the gap wave 8 recorded on the zt4ca2 book): the
/// slot check cannot see a fast path that WRITES a withheld register, since
/// the armed path stores no slot. So the withheld registers are also read
/// right after the site: on the armed path every one must still hold its
/// sentinel (that is the whole licence for withholding it), on a refusal
/// the ones callee-saved under both ABIs (RBX, R12-R15; RSI / RDI are
/// SysV argument registers of the announce call). And every announce form
/// the switches can select runs: the long form with each zr z11/ia trim
/// killed, and the short form (`CRATONVM_ZGC_JIT_SHORT_ANNOUNCE=1`), whose
/// table words at 48 / 56 the 8-word table below supplies.


/// zt t11/ca3: a sunk spill at a site whose fast path would call (the
/// announce arm without the ZGC acknowledgement, or a `Helper` class)
/// fails the compile closed instead of shipping a call with eleven
/// registers unspilled.
#[test]
fn a_sunk_spill_with_a_call_on_the_fast_path_fails_closed() {
    if !inline_tlab_new_enabled() {
        return;
    }
    for (mode, zgc) in [
        (TlabPostInit::ZgcAnnounce, false),
        (TlabPostInit::Helper, false),
        (TlabPostInit::Helper, true),
        (TlabPostInit::ZgcRegisterThenHelper, true),
    ] {
        let mut c = compiler();
        c.deferred_alloc_blind_spill = true;
        c.deferred_alloc_zgc_announce = zgc;
        c.emit_inline_tlab_new_with(CID, 2, mode, true, None);
        assert_eq!(
            c.failed_site.map(|s| s.0),
            Some("singlepass-codegen/alloc-spill-sink-call-on-fast-path"),
            "{mode:?} zgc={zgc}"
        );
    }
    // Positive control: the `Skip` sink compiles.
    let mut c = compiler();
    c.deferred_alloc_blind_spill = true;
    c.emit_inline_tlab_new_with(CID, 2, TlabPostInit::Skip, false, None);
    assert!(c.failed_site.is_none(), "{:?}", c.failed_site);    }