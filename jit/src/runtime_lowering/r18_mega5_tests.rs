//! JIT round 18 wave 11, lane mega5: THROWS18-5 (the hashed stub's whole
//! probe in RAX/R10/R11, `CRATONVM_JIT_MEGA_STAGE_ONCE`), THROWS18-6 (the
//! stub's lean hit protocol, `CRATONVM_JIT_MEGA_LEAN_STUB_HIT`) and the
//! admission of THROWS18-7 (`CRATONVM_JIT_MEGA_GATE_FIRST`). The probe is
//! EXECUTED on this host's ABI against the real per-site hashed ways, class
//! cells and shared table, and must answer exactly what the stub's three
//! steps answer, in the stub's order, without touching an argument
//! register.
use super::r12w3_mega2_table_probe_tests::{
    body, publish, run, Obj, ARG, ARG1, CTX, MISS, RECV,
};
use super::*;
use crate::{CompiledMethod, JitCache, MegaDispatchTable};

/// Set on the harness's answer when the probe missed.
const MISSED: u64 = 1 << 63;
/// Every argument register holds this across the probe.
const CANARY: u64 = 0x0C0F_FEE0_5EED_0042;
/// The harness's answer when an argument register changed.
const CLOBBERED: u64 = 0x0BAD_0000_0000_0BAD;
/// The column the test sites are bound at.
const COL: u32 = 3;

/// `CMP <r>, RAX` (REX.W [+B] 39 /r).
fn cmp_with_rax(buf: &mut ExecutableBuffer, r: u8) {
    buf.emit(&[0x48 | ((r >> 3) & 1), 0x39, 0xC0 | (r & 7)]);
}

/// `extern "C" fn(pic, class_id, canary) -> u64` around
/// [`emit_mega_register_probe`]: R10 = the slot, RAX = the class id, every
/// argument register = the canary. Answers the TAGGED word on a hit,
/// `MISSED | RAX` (all 64 bits: the zero-extension is part of the miss
/// contract) on a miss, and `CLOBBERED` if any argument register moved.
fn harness(pic: &JitPICSlot, cell_first: bool) -> ExecutableBuffer {
    let mut buf = ExecutableBuffer::new(2048).expect("buffer");
    emit_mov_reg(&mut buf, R10, ENTRY_ABI_REGS[0]);
    emit_mov_reg(&mut buf, RAX, ENTRY_ABI_REGS[1]);
    let canary = ENTRY_ABI_REGS[2];
    for &r in ENTRY_ABI_REGS {
        if r != canary {
            emit_mov_reg(&mut buf, r, canary);
        }
    }
    let misses =
        emit_mega_register_probe(&mut buf, pic as *const JitPICSlot as usize, cell_first);
    assert_eq!(misses.len(), 1, "one miss exit");
    let mut clobbered = Vec::new();
    // Hit: R11 = the tagged word.
    emit_mov_imm64(&mut buf, RAX, CANARY);
    for &r in ENTRY_ABI_REGS {
        cmp_with_rax(&mut buf, r);
        clobbered.push(emit_jcc(&mut buf, 0x85));
    }
    emit_mov_reg(&mut buf, RAX, R11);
    buf.emit_byte(0xC3); // RET
    for m in misses {
        patch_rel32_to_here(&mut buf, m);
    }
    buf.emit(&[0x49, 0x89, 0xC3]); // MOV R11, RAX
    emit_mov_imm64(&mut buf, RAX, CANARY);
    for &r in ENTRY_ABI_REGS {
        cmp_with_rax(&mut buf, r);
        clobbered.push(emit_jcc(&mut buf, 0x85));
    }
    emit_mov_reg(&mut buf, RAX, R11);
    buf.emit(&[0x48, 0x0F, 0xBA, 0xE8, 0x3F]); // BTS RAX, 63
    buf.emit_byte(0xC3); // RET
    for c in clobbered {
        patch_rel32_to_here(&mut buf, c);
    }
    emit_mov_imm64(&mut buf, RAX, CLOBBERED);
    buf.emit_byte(0xC3); // RET
    assert!(!buf.overflowed(), "{:?}", buf.codegen_failure_reason());
    buf.finalize();
    buf
}

fn probe(harness: &ExecutableBuffer, pic: &JitPICSlot, class_id: u32) -> u64 {
    // SAFETY: `harness` finalized a complete `extern "C" fn(u64, u64, u64)
    // -> u64` that reads `pic` and the table words it names, writes only
    // caller-saved registers, and calls nothing.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64, u64) -> u64>(harness.as_ptr())
    };
    f(pic as *const JitPICSlot as u64, u64::from(class_id), CANARY)
}

fn bound_pic(table: &MegaDispatchTable, selector: u32) -> Box<JitPICSlot> {
    let pic = Box::new(JitPICSlot::new());
    pic.bind_mega_dispatch(table, selector);
    pic.bind_mega_class_slot(table, COL);
    pic
}

/// What the stub's three steps answer for `class_id`, in its order: the
/// Rust mirrors of the cell, the site's hashed ways and the shared table.
fn expected(
    table: &MegaDispatchTable,
    pic: &JitPICSlot,
    selector: u32,
    class_id: u32,
    cell_first: bool,
) -> u64 {
    let cell = || table.lookup_class_cell(class_id, selector, COL);
    let hashed = || pic.lookup_megamorphic(class_id);
    let first = if cell_first {
        cell().or_else(hashed)
    } else {
        hashed().or_else(cell)
    };
    let found = first.or_else(|| table.lookup(class_id, selector).map(|(e, c, _)| (e, c)));
    match found {
        Some((entry, ctx)) => crate::jit_ic_entry_word(entry, ctx),
        None => MISSED | u64::from(class_id),
    }
}

#[test]
fn r18_mega5_register_probe_has_its_shape() {
    let pic = 0x7fff_0000_0000_2000usize;
    for cell_first in [true, false] {
        let mut buf = ExecutableBuffer::new(1024).expect("buffer");
        let exits = emit_mega_register_probe(&mut buf, pic, cell_first);
        assert!(!buf.overflowed(), "{:?}", buf.codegen_failure_reason());
        assert_eq!(exits.len(), 1);
        let code = buf.as_slice();
        // The column probe opens with `MOV R11, [R10 + root]`, the hashed
        // ways with `IMUL R11D, EAX, imm32`, the table with
        // `MOV R11D, [R10 + selector]`.
        let column: [u8; 3] = [0x4D, 0x8B, 0x9A];
        let hashed: [u8; 3] = [0x44, 0x69, 0xD8];
        let table: [u8; 3] = [0x45, 0x8B, 0x9A];
        let at = |pat: &[u8]| code.windows(pat.len()).position(|w| w == pat);
        let (c, h, t) = (at(&column), at(&hashed), at(&table));
        assert!(c.is_some() && h.is_some() && t.is_some());
        if cell_first {
            assert_eq!(c, Some(0), "cell first");
            assert!(h < t);
        } else {
            assert_eq!(h, Some(0), "hashed ways first");
            assert!(c < t);
        }
        // The slot is baked again after the column probe clobbered R10.
        let mov_pic: Vec<u8> = [&[0x49u8, 0xBA][..], &(pic as u64).to_le_bytes()[..]].concat();
        assert_eq!(code.windows(10).filter(|w| *w == &mov_pic[..]).count(), 1);
        // The single exit is a `JMP rel32` preceded by the miss pad's
        // `MOV EAX, EAX`, and the hit falls through at the end.
        let exit = exits[0];
        assert_eq!(code.get(exit - 3..exit), Some(&[0x89u8, 0xC0, 0xE9][..]));
        assert_eq!(exit + 4, code.len(), "the hit falls through past the exit");
    }
}

/// For every class, under every mix of the three tables and both orders,
/// the probe answers the stub's answer, never touches an argument
/// register, and misses with the zero-extended class id.
#[test]
fn r18_mega5_register_probe_agrees_with_the_stub_order() {
    let cache = JitCache::new();
    let in_cell = publish(&cache, "m5-cell", body(ENTRY_ABI_REGS[1], 0x510));
    let in_site = publish(&cache, "m5-site", body(ENTRY_ABI_REGS[2], 0x520));
    let in_table = publish(&cache, "m5-table", body(ENTRY_ABI_REGS[1], 0x530));
    let table = cache.mega_dispatch_table();
    let selector = table
        .selector_id("p/M5", "f", "(J)J", 0, 51, 0)
        .expect("selector");
    let pic = bound_pic(table, selector);
    let classes: Vec<u32> = (1..420).collect();
    for &c in &classes {
        if c % 3 == 0 {
            table.install_with_class_slot(
                c,
                selector,
                Some((COL, 8)),
                in_cell.entry_ptr() as u64,
                false,
                false,
            );
        }
        if c % 5 == 0 {
            pic.install(c, "M5", in_site.entry_ptr() as u64, true, false);
        }
        if c % 7 == 0 {
            table.install(c, selector, in_table.entry_ptr() as u64, false, false);
        }
    }
    for cell_first in [true, false] {
        let code = harness(&pic, cell_first);
        let mut answered = [0usize; 2];
        for &c in &classes {
            let want = expected(table, &pic, selector, c, cell_first);
            let got = probe(&code, &pic, c);
            assert_ne!(got, CLOBBERED, "class {c}: an argument register moved");
            assert_eq!(got, want, "class {c}, cell_first {cell_first}");
            answered[usize::from(got & MISSED == 0)] += 1;
        }
        assert!(
            answered[1] > 0 && answered[0] > 0,
            "hits and misses both ran: {answered:?}"
        );
        drop(code);
    }
    // A site bound to nothing reads nothing, and misses cleanly.
    let fresh = Box::new(JitPICSlot::new());
    let code = harness(&fresh, true);
    for c in [1u32, 3, 5, 7, 1_000_000] {
        assert_eq!(probe(&code, &fresh, c), MISSED | u64::from(c));
    }
    drop(code);
    // Retirement reaches every step: nothing retired is answered.
    let code = harness(&pic, true);
    table.retire_entry(in_cell.entry_ptr() as usize);
    pic.clear_entries();
    let survivor = crate::jit_ic_entry_word(in_table.entry_ptr() as u64, false);
    for &c in &classes {
        let got = probe(&code, &pic, c);
        assert_eq!(
            got,
            expected(table, &pic, selector, c, true),
            "class {c} after retirement"
        );
        assert!(
            got & MISSED != 0 || got == survivor,
            "class {c}: a retired body was answered"
        );
    }
    table.clear_entries();
    for &c in &classes {
        assert_eq!(probe(&code, &pic, c), MISSED | u64::from(c));
    }
    drop(code);
    for b in [in_cell, in_site, in_table] {
        crate::defer_jit_owner(Some(b));
    }
}


extern "C" fn fake_frame_record(_rbp: u64) {}

/// What the fake callee-deopt service answers.
const SERVICED: u64 = 0x777;

extern "C" fn fake_service(_vm: u64, _info: u64, _args: u64, _n: u64) -> u64 {
    SERVICED
}

/// A callee that answers the `i64::MIN` sentinel.
fn sentinel_body() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("buffer");
    emit_mov_imm64(&mut buf, RAX, i64::MIN as u64);
    buf.emit_byte(0xC3); // RET
    CompiledMethod::new(buf)
}

/// Bit set on the harness's answer when the stub left through a lean exit.
const LEAN_EXIT: u64 = 1 << 62;

fn frame_record_addr() -> usize {
    let f: extern "C" fn(u64) = fake_frame_record;
    f as usize
}

/// The r12w3 frame harness around [`emit_hashed_vtable_stub_body_with`]
/// with a wired (fake) service and frame record: `MISS` on the
/// fall-through, the result on the ordinary hit exit, the result with
/// [`LEAN_EXIT`] on a lean exit. Returns the code and the lean exit count.
fn build_lean(pic: &JitPICSlot, lean: Option<u32>) -> (ExecutableBuffer, usize) {
    let mut buf = ExecutableBuffer::new(4096).expect("buffer");
    buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x40]);
    emit_store_frame(&mut buf, CTX, ENTRY_ABI_REGS[0]);
    emit_store_frame(&mut buf, RECV, ENTRY_ABI_REGS[1]);
    emit_store_frame(&mut buf, ARG1, ENTRY_ABI_REGS[2]);
    let service: extern "C" fn(u64, u64, u64, u64) -> u64 = fake_service;
    let service = service as usize;
    let (hits, _imm, _fast, _ret, lean_exits) = emit_hashed_vtable_stub_body_with(
        &mut buf,
        pic as *const JitPICSlot as usize,
        CTX,
        &[RECV, ARG1],
        frame_record_addr(),
        service,
        0x1,
        // `[RECV, ARG1]` is a contiguous descending block.
        RECV,
        None,
        None,
        true,
        true,
        true,
        false,
        lean,
        None,
    );
    assert_eq!(hits.len(), 1);
    emit_mov_imm64(&mut buf, RAX, MISS);
    buf.emit(&[0xC9, 0xC3]); // LEAVE; RET
    for h in hits {
        patch_rel32_to_here(&mut buf, h);
    }
    buf.emit(&[0xC9, 0xC3]);
    let n = lean_exits.len();
    for l in lean_exits {
        patch_rel32_to_here(&mut buf, l);
    }
    buf.emit(&[0x48, 0x0F, 0xBA, 0xE8, 62]); // BTS RAX, 62
    buf.emit(&[0xC9, 0xC3]);
    assert!(!buf.overflowed(), "{:?}", buf.codegen_failure_reason());
    buf.finalize();
    (buf, n)
}

/// THROWS18-6: the lean stub leaves a not-sentinel hit through its lean
/// exit with the caller's identity published, and a sentinel through the
/// service and the ordinary exit; without a lean identity (or without the
/// inline TLS mirrors) it is the stub it always was.
#[test]
fn r18_mega5_lean_stub_hit_takes_the_hit_join() {
    const ID: u32 = 0x00AB_CD12;
    let cache = JitCache::new();
    let plain = publish(&cache, "m5-lean", body(ENTRY_ABI_REGS[1], 0x100));
    let sentinel = publish(&cache, "m5-sentinel", sentinel_body());
    let table = cache.mega_dispatch_table();
    let selector = table
        .selector_id("p/M5L", "f", "(J)J", 0, 52, 0)
        .expect("selector");
    let pic = Box::new(JitPICSlot::new());
    pic.bind_mega_dispatch(table, selector);
    table.install(7, selector, plain.entry_ptr() as u64, false, false);
    table.install(9, selector, sentinel.entry_ptr() as u64, false, false);
    let (seven, nine, eleven) = (Obj::new(7), Obj::new(9), Obj::new(11));

    let (classic, none) = build_lean(&pic, None);
    assert_eq!(none, 0, "no identity, no lean exit");
    assert_eq!(
        run(&classic, seven.addr()),
        ARG ^ 0x100,
        "the ordinary exit"
    );
    assert_eq!(
        run(&classic, nine.addr()),
        SERVICED,
        "the sentinel is serviced"
    );
    assert_eq!(run(&classic, eleven.addr()), MISS);

    let shape = lean_stub_hit_shape(frame_record_addr());
    let (lean, exits) = build_lean(&pic, Some(ID));
    if !shape {
        assert_eq!(exits, 0, "no inline TLS mirrors: the classic stub");
        assert_eq!(lean.as_slice(), classic.as_slice());
    } else {
        assert_eq!(exits, 1, "one lean exit");
        // The identity store carries the caller's id, not the shared 0.
        let mut store = vec![0xC7u8, 0x04, 0x25];
        store.extend_from_slice(&(crate::x64::inline_cm_tls_disp() as u32).to_le_bytes());
        let with_id: Vec<u8> = [&store[..], &ID.to_le_bytes()[..]].concat();
        let with_zero: Vec<u8> = [&store[..], &0u32.to_le_bytes()[..]].concat();
        let has = |code: &[u8], pat: &[u8]| code.windows(pat.len()).any(|w| w == pat);
        assert!(has(lean.as_slice(), &with_id) && !has(lean.as_slice(), &with_zero));
        assert!(has(classic.as_slice(), &with_zero) && !has(classic.as_slice(), &with_id));
        assert_eq!(
            run(&lean, seven.addr()),
            (ARG ^ 0x100) | LEAN_EXIT,
            "the hit joins"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            crate::x64::inline_cm_tls_mirror_read(),
            Some(ID),
            "our identity"
        );
        assert_eq!(
            run(&lean, nine.addr()),
            SERVICED,
            "a sentinel takes the service"
        );
        assert_eq!(run(&lean, eleven.addr()), MISS);
    }
    drop((classic, lean));
    table.clear_entries();
    for b in [plain, sentinel] {
        crate::defer_jit_owner(Some(b));
    }
}
