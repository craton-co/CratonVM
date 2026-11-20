use super::*;

/// The JIT's copies of the collector's layout constants are the
/// collector's (`cratonvm-gc` is reachable from a test, not from the
/// emitter).
#[test]
fn the_offsets_match_the_collector_s() {
    use cratonvm_gc::tlab::JitZgcAnnounceTable as T;
    assert_eq!(
        ZGC_TLAB_CURSOR_OFFSET as usize,
        cratonvm_gc::Tlab::CURSOR_OFFSET
    );
    assert_eq!(
        ZGC_TLAB_START_OFFSET as usize,
        cratonvm_gc::Tlab::START_OFFSET
    );
    assert_eq!(
        ZGC_TLAB_OWNED_EPOCH_OFFSET as usize,
        cratonvm_gc::Tlab::ZGC_OWNED_EPOCH_OFFSET
    );
    assert_eq!(
        ZGC_TLAB_ANNOUNCE_OFFSET as usize,
        cratonvm_gc::Tlab::ZGC_ANNOUNCE_OFFSET
    );
    assert_eq!(ZGC_ANNOUNCE_WORDS as usize, T::WORDS_OFFSET);
    assert_eq!(ZGC_ANNOUNCE_BASE as usize, T::BASE_OFFSET);
    assert_eq!(ZGC_ANNOUNCE_SPAN as usize, T::SPAN_OFFSET);
    assert_eq!(ZGC_ANNOUNCE_BLOCKED as usize, T::BLOCKED_OFFSET);
    assert_eq!(ZGC_ANNOUNCE_EPOCH as usize, T::EPOCH_OFFSET);
}

/// What the stubs saw. Reached through the `vm_ptr` argument every
/// helper receives first (the harness passes this struct's address as the
/// VM pointer), so the test needs no process-global state
/// (`jit/tests/process_global_statics_ratchet.rs`).
#[repr(C)]
#[derive(Default)]
struct Calls {
    note: u64,
    note_obj: i64,
    other: u64,
}

// SAFETY: extern "C" test stub; `vm` is the address of the live `Calls`
// the harness passed as the VM pointer, and nothing else is dereferenced.
unsafe extern "C" fn stub_note(vm: i64, obj: i64, _size: i64) -> i64 {
    let calls = &mut *(vm as *mut Calls);
    calls.note += 1;
    calls.note_obj = obj;
    obj
}
// SAFETY: as above.
unsafe extern "C" fn stub_post_init(vm: i64, obj: i64, _cid: i64, _nf: i64) -> i64 {
    (*(vm as *mut Calls)).other += 1;
    obj
}
// SAFETY: as above; returns a sentinel no bump can produce.
unsafe extern "C" fn stub_new_object(vm: i64, _cid: i64, _nf: i64) -> i64 {
    (*(vm as *mut Calls)).other += 1;
    0x0BAD_0B1E
}
// SAFETY: extern "C" test stub that dereferences nothing.
unsafe extern "C" fn stub_null_thread() -> *mut std::ffi::c_void {
    std::ptr::null_mut()
}

/// Far above any class a test registers a compact layout for.
const CID: u32 = 0x0765_4322;
const NF: usize = 2;

/// The fake `JvmThread`: its `Tlab` at offset 0, laid out as the
/// collector's (`cursor, end, start, zgc_owned_epoch, zgc_announce`,
/// `zgc_owned_words`).
#[repr(C)]
#[allow(dead_code)] // read by the emitted code, not by Rust
struct FakeThread {
    cursor: u64,
    end: u64,
    start: u64,
    owned_epoch: u64,
    announce: u64,
    owned_words: u64,
}

/// The fake `JitZgcAnnounceTable` (`owner` is never read by the JIT).
#[repr(C)]
#[allow(dead_code)] // read by the emitted code, not by Rust
struct FakeTable {
    words: u64,
    base: u64,
    span: u64,
    blocked: u64,
    epoch: u64,
    owner: u64,
    /// `short_blocked` (zr z9/bo): the lean store gates on it, zero while armed and not blocked.
    short_blocked: u64,
    granule_bias: u64,
}

/// Emit the `ZgcAnnounce` inline `new` WITH the inline start-bit store,
/// run it against `thread` with `calls` as the VM pointer, and return RAX.
fn run(thread: &mut FakeThread, calls: &mut Calls) -> i64 {
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    // Cast: fn pointer to usize helper address (all four below).
    c.helpers.get_current_thread = stub_null_thread as *const () as usize;
    c.helpers.tlab_post_init = stub_post_init as *const () as usize;
    c.helpers.new_object = stub_new_object as *const () as usize;
    c.helpers.zgc_note_tlab_object = stub_note as *const () as usize;
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
    c.emit_inline_tlab_new_with(CID, NF, TlabPostInit::ZgcAnnounce, true, None);
    c.buf.emit(&[0xC9, 0xC3]); // leave; ret
    assert!(!c.buf.overflowed(), "the test buffer must hold the snippet");
    assert!(c.buf.codegen_failure_reason().is_none());

    crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: `entry_off` is a byte offset inside the same allocation.
    let entry = unsafe { c.buf.as_ptr().add(entry_off) };
    // SAFETY: the snippet builds and tears down its own frame, takes two
    // i64 arguments, reads the fake thread and the fake table it names,
    // writes the TLAB and the bitmap they name (all alive across the
    // call), calls only the stubs above (which write only `calls`), and
    // returns an i64. `c` owns the buffer and outlives the call.
    let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
    crate::x64::publish_jit_thread_mirror(thread as *mut FakeThread as usize);
    let r = f(calls as *mut Calls as i64, thread as *mut FakeThread as i64);
    crate::x64::publish_jit_thread_mirror(0);
    drop(c);
    r
}

/// zr z11/ia: `Tlab::zgc_owned_words` as the one u64 the fake thread
/// carries (first index in the low dword), from the collector's own
/// derivation.
fn owned_words_word(base: u64, span: u64, start: u64, end: u64) -> u64 {
    let u = |v: u64| usize::try_from(v).unwrap();
    let [first, past] =
        cratonvm_gc::tlab::zgc_owned_word_window(u(base), u(span), u(start), u(end));
    u64::from(first) | (u64::from(past) << 32)
}

/// One way to break (or keep) the contract, applied to a fully valid setup:
/// `(thread, table, heap base)`.
type Edit = fn(&mut FakeThread, &mut FakeTable, u64);

fn keep(_: &mut FakeThread, _: &mut FakeTable, _: u64) {}
fn unarm(t: &mut FakeThread, _: &mut FakeTable, _: u64) {
    t.announce = 0;
}
fn stale_epoch(t: &mut FakeThread, _: &mut FakeTable, _: u64) {
    t.owned_epoch -= 1;
}
fn marking(_: &mut FakeThread, tb: &mut FakeTable, _: u64) {
    tb.blocked = 1;
    tb.short_blocked = 1;
}
fn unpublished(_: &mut FakeThread, tb: &mut FakeTable, _: u64) {
    tb.words = 0;
    tb.short_blocked = 1;
}
fn below_base(_: &mut FakeThread, tb: &mut FakeTable, base: u64) {
    tb.base = base + 1024;
}
fn beyond_span(_: &mut FakeThread, tb: &mut FakeTable, _: u64) {
    tb.span = 520;
}
fn word_starts_before_chunk(t: &mut FakeThread, _: &mut FakeTable, base: u64) {
    t.start = base + 516;
}
fn word_ends_after_chunk(t: &mut FakeThread, _: &mut FakeTable, base: u64) {
    t.end = base + 1000;
}

/// Round 9 wave 8 (`perf-zgc-compiled-new-always-takes-the-rust-helper`):
/// with every condition of `JitZgcAnnounceTable`'s contract met, the
/// object's start bit is set inline and the announce helper is NOT called;
/// each condition broken on its own sends the site to the helper with the
/// bitmap untouched. Every arm returns the bumped object and reaches no
/// other helper.
#[test]
fn the_inline_start_bit_store_sets_the_bit_or_defers_to_the_helper() {
    if !inline_tlab_new_enabled() {
        return; // opted out by the environment: the arm is a bare helper call
    }
    // Object at heap offset 520: bit 65 = word 1, bit 1.
    const OFF: u64 = 520;
    let cases: [(&str, bool, Edit); 9] = [
        ("armed", true, keep),
        ("unarmed buffer", false, unarm),
        ("stale epoch", false, stale_epoch),
        ("marking", false, marking),
        ("unpublished", false, unpublished),
        ("below base", false, below_base),
        ("beyond span", false, beyond_span),
        (
            "word starts before the chunk",
            false,
            word_starts_before_chunk,
        ),
        ("word ends after the chunk", false, word_ends_after_chunk),
    ];
    for (name, inline, edit) in cases {
        let mut calls = Calls::default();
        let mut heap = vec![0u64; 256]; // 2 KiB of "arena"
        let base = heap.as_mut_ptr() as u64;
        let mut bits = vec![0u64; 8];
        let mut table = FakeTable {
            words: bits.as_mut_ptr() as u64,
            base,
            span: 2048,
            blocked: 0,
            epoch: 41,
            owner: base,
            short_blocked: 0,
            granule_bias: 0,
        };
        let mut thread = FakeThread {
            cursor: base + OFF,
            end: base + 2048,
            start: base,
            owned_epoch: 41,
            announce: 1,
            owned_words: 0,
        };
        edit(&mut thread, &mut table, base);
        // zr z11/ia: the window `Tlab::new` derives from the edited
        // geometry (the long form's owned-word test reads it).
        thread.owned_words = owned_words_word(table.base, table.span, thread.start, thread.end);
        if thread.announce != 0 {
            thread.announce = &table as *const FakeTable as u64;
        }

        let r = run(&mut thread, &mut calls);

        let obj = (base + OFF) as i64;
        // The footprint the bump took (header plus two legacy cells),
        // read back rather than re-derived from the layout constants.
        let size = thread.cursor - (base + OFF);
        assert!(size > 0 && size % 8 == 0, "{name}: bumped by {size}");
        assert_eq!(r, obj, "{name}: RAX is the bumped object");
        assert_eq!(heap[(OFF / 8) as usize] as u32, CID, "{name}: class id");
        assert_eq!(calls.other, 0, "{name}: no other helper");
        if inline {
            assert_eq!(calls.note, 0, "{name}: no call");
            assert_eq!(bits[1], 1u64 << 1, "{name}: the object's start bit");
            assert!(
                bits.iter().enumerate().all(|(i, w)| i == 1 || *w == 0),
                "{name}: no other bit"
            );
            // A second object in the same word keeps the first bit.
            let r2 = run(&mut thread, &mut calls);
            assert_eq!(r2, obj + size as i64, "{name}: second object");
            let bit2 = ((OFF + size) / 8) % 64;
            assert_eq!(bits[1], (1u64 << 1) | (1u64 << bit2), "{name}: both bits");
            assert_eq!(calls.note, 0, "{name}: still no call");
        } else {
            assert_eq!(calls.note, 1, "{name}: the helper");
            assert_eq!(calls.note_obj, obj, "{name}: obj_ptr");
            assert!(bits.iter().all(|w| *w == 0), "{name}: bitmap untouched");
        }
        // `table` is not dropped explicitly: `FakeTable` is eight `u64`s with no
        // `Drop` impl and no borrows, so `drop(table)` said nothing — and is
        // what clippy's `drop_non_drop` refuses. `bits` and `heap` own
        // allocations, so their drops are real and stay.
        drop(bits);
        drop(heap);
    }
}
