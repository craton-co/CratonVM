use super::*;

const THREAD_OFF: i32 = 8;
const CONTEXT_OFF: i32 = 16;
const LENGTH_OFF: i32 = 24;
const T_INT: u8 = 10;
const T_LONG: u8 = 11;
/// A class id no other test registers a layout for: the legacy shape.
const LEGACY_CID: u32 = 915_171;
const STUB_OBJ: u64 = 0xABC0;

/// What the stand-in helper and stub saw, through the context word.
#[repr(C)]
#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
struct Seen {
    helper_calls: u64,
    helper_size: u64,
    helper_answers_zero: u64,
    stub_calls: u64,
    stub_imm: u64,
    stub_arg: u64,
}

extern "C" fn note(ctx: u64, obj: u64, size: u64) -> u64 {
    // SAFETY: the tests pass `&mut Seen` as the context word.
    let seen = unsafe { &mut *(ctx as *mut Seen) };
    seen.helper_calls += 1;
    seen.helper_size = size;
    if seen.helper_answers_zero != 0 {
        0
    } else {
        obj
    }
}

extern "C" fn stub(ctx: u64, imm: u64, arg: u64) -> u64 {
    // SAFETY: as above.
    let seen = unsafe { &mut *(ctx as *mut Seen) };
    seen.stub_calls += 1;
    seen.stub_imm = imm;
    seen.stub_arg = arg;
    STUB_OBJ
}

fn note_addr() -> usize {
    let f: extern "C" fn(u64, u64, u64) -> u64 = note;
    f as usize
}

fn stub_addr() -> usize {
    let f: extern "C" fn(u64, u64, u64) -> u64 = stub;
    f as usize
}

#[derive(Clone, Copy)]
enum Site {
    Array {
        atype: u8,
        const_length: Option<u32>,
    },
    New,
}

fn array_plan(atype: u8, const_length: Option<u32>) -> InlineTlabArrayPlan {
    InlineTlabArrayPlan {
        thread_slot_off: THREAD_OFF,
        thread_tls_disp: 0,
        cursor_off: 0,
        end_off: 8,
        context_off: CONTEXT_OFF,
        atype,
        length_off: LENGTH_OFF,
        announce: note_addr(),
        component_class_id: (atype == 0).then_some(7),
        const_length,
    }
}

fn new_plan() -> InlineTlabPlan {
    InlineTlabPlan {
        thread_slot_off: THREAD_OFF,
        thread_tls_disp: 0,
        cursor_off: 0,
        end_off: 8,
        post_init: 0x1000,
        context_off: CONTEXT_OFF,
        class_id: LEGACY_CID,
        num_fields: 1,
        announce: note_addr(),
        skip_post_init: false,
        register_first: 0,
        post_init_helper_first: false,
    }
}

/// `extern "C" fn(thread, ctx, length) -> object | 0`. Unsplit: the whole
/// sequence, then `LEAVE; RET`. Split: the fast half, the continuation
/// `LEAVE; RET`, then the cold half and what the lowerer emits after it
/// (`TEST RAX, RAX; JNZ continuation`, then the exit, here `LEAVE; RET`
/// with RAX = 0). `None` when the split form declines.
fn build(site: Site, split: bool) -> Option<ExecutableBuffer> {
    let mut buf = ExecutableBuffer::new(4096)?;
    // PUSH RBP; MOV RBP, RSP; SUB RSP, 64 (slots, then Win64 shadow space).
    buf.emit(&[0x55, 0x48, 0x89, 0xE5, 0x48, 0x83, 0xEC, 0x40]);
    emit_store_frame(&mut buf, THREAD_OFF, ENTRY_ABI_REGS[0]);
    emit_store_frame(&mut buf, CONTEXT_OFF, ENTRY_ABI_REGS[1]);
    emit_store_frame(&mut buf, LENGTH_OFF, ENTRY_ABI_REGS[2]);
    if !split {
        match site {
            Site::Array {
                atype,
                const_length,
            } => {
                emit_inline_tlab_newarray_ir(
                    &mut buf,
                    &array_plan(atype, const_length),
                    stub_addr(),
                    0,
                )?;
            }
            Site::New => {
                emit_inline_tlab_new_ir(&mut buf, &new_plan(), stub_addr(), 0)?;
            }
        }
        buf.emit(&[0xC9, 0xC3]); // LEAVE; RET
    } else {
        let resume;
        match site {
            Site::Array {
                atype,
                const_length,
            } => {
                let plan = array_plan(atype, const_length);
                let s = emit_inline_tlab_newarray_ir_split(&mut buf, &plan, stub_addr())?;
                resume = buf.pos();
                buf.emit(&[0xC9, 0xC3]); // the continuation: LEAVE; RET
                emit_inline_tlab_newarray_ir_cold_half(&mut buf, &plan, stub_addr(), 0, s);
            }
            Site::New => {
                let plan = new_plan();
                let s = emit_inline_tlab_new_ir_split(&mut buf, &plan, stub_addr())?;
                resume = buf.pos();
                buf.emit(&[0xC9, 0xC3]);
                emit_inline_tlab_new_ir_cold_half(&mut buf, &plan, stub_addr(), 0, s, |_| {});
            }
        }
        buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        let ok = emit_jcc(&mut buf, 0x85); // JNZ continuation
        let rel = i32::try_from(resume).ok()? - (i32::try_from(ok).ok()? + 4);
        let _ = buf.try_patch_i32(ok, rel);
        buf.emit(&[0xC9, 0xC3]); // the exit: LEAVE; RET with RAX = 0
    }
    if buf.overflowed() {
        return None;
    }
    buf.finalize();
    Some(buf)
}

#[repr(C, align(512))]
struct Chunk([u64; 512]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Inline,
    Blocked,
    Unpublished,
    HelperZero,
    Exhausted,
}

/// `(object offset or raw answer, cursor offset after, bitmap words 0..2,
/// what the helper and stub saw)` for one allocation at offset 64 of a
/// 4 KiB, 512-aligned chunk.
fn run(buf: &ExecutableBuffer, route: Route) -> (u64, u64, [u64; 2], Seen) {
    let chunk = Box::new(Chunk([0; 512]));
    let base = chunk.0.as_ptr() as u64; // Cast: the TLAB's address
    let mut bitmap = [0u64; 8];
    let blocked = u64::from(route == Route::Blocked);
    // words, base, span, blocked, epoch, -, short_blocked, granule_bias.
    let tab = [
        bitmap.as_mut_ptr() as u64, // Cast: the bitmap's address
        base,
        4096,
        blocked,
        7,
        0,
        blocked,
        (bitmap.as_mut_ptr() as u64).wrapping_sub((base >> 9) * 8), // Cast: as above
    ];
    // The helper's `0` needs the helper arm: an unpublished table.
    let announce = if matches!(route, Route::Unpublished | Route::HelperZero) {
        0
    } else {
        tab.as_ptr() as u64 // Cast: the table's address
    };
    let end = if route == Route::Exhausted {
        base + 72
    } else {
        base + 4096
    };
    // cursor, end, start, owned epoch, announce, owned-word window [0, 8).
    let mut thread = [base + 64, end, base, 7, announce, 8 << 32];
    let mut seen = Seen {
        helper_answers_zero: u64::from(route == Route::HelperZero),
        ..Seen::default()
    };
    // SAFETY: `buf` is a finalized `extern "C" fn(u64, u64, u64) -> u64`
    // that touches `thread`, the chunk, `bitmap` and `seen` (through the
    // stand-in helper and stub).
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64, u64) -> u64>(buf.as_ptr())
    };
    let ctx = std::ptr::addr_of_mut!(seen) as u64; // Cast: the context word
    let r = f(thread.as_mut_ptr() as u64, ctx, 5); // Cast: the thread block
    let obj = if r >= base && r < base + 4096 {
        r - base
    } else {
        r
    };
    let out = (
        obj,
        thread[0].wrapping_sub(base),
        [bitmap[0], bitmap[1]],
        seen,
    );
    drop(chunk);
    out
}

fn sites() -> Vec<Site> {
    let mut v = vec![
        Site::Array {
            atype: T_LONG,
            const_length: Some(0),
        },
        Site::Array {
            atype: T_INT,
            const_length: None,
        },
        Site::Array {
            atype: 0,
            const_length: None,
        },
    ];
    if cratonvm_types::class_layout(LEGACY_CID).is_none() {
        v.push(Site::New);
    }
    v
}

const ROUTES: [Route; 5] = [
    Route::Inline,
    Route::Blocked,
    Route::Unpublished,
    Route::HelperZero,
    Route::Exhausted,
];

fn with(edits: &[(&str, Option<&str>)]) -> impl Drop {
    cratonvm_types::flags::override_thread(cratonvm_types::flags::VmFlags::from_env_with_edits(
        edits,
    ))
}




