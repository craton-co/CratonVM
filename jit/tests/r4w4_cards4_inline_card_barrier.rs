// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Generational GC round 4, wave 4, lane `cards4` — the inline generational
//! post barrier (`CRATONVM_JIT_INLINE_CARD_MARK=1`), EXECUTED through the
//! single-pass backend's real store arms:
//!
//! * `aastore` (inline arm): the ELEMENT's card is checked BEFORE and AFTER
//!   the store, and the collector's barrier is called only for a clean card;
//! * `aastore` (default arms, flag off): the barrier call is handed the
//!   element's slot address on an element-precise card table;
//! * `putfield` — the GATED compact arm (a published mask plan) and the
//!   ungated compact arm (no plan): the same PRE/POST check around the store;
//! * the flag without a card view (G1, ZGC, a test table) changes nothing.
//!
//! The "old generation" is a heap buffer covered by a real
//! `cratonvm_gc::card_table::CardTable`, and the `write_barrier` entry is a
//! stub that does what `GenerationalHeap::write_barrier` does for an in-range
//! store — `CardTable::mark_dirty_lockfree(obj)` — and records what it saw,
//! including the watched slot's value AT CALL TIME, which is how the PRE
//! barrier is shown to run before the store.
//!
//! Every test takes `LOCK`: the stub's bookkeeping is file-global.
#![cfg(target_arch = "x86_64")]

use cratonvm_gc::card_table::{CardTable, CardTableOptions, CARD_SIZE};
use cratonvm_jit::JitRuntimeHelpers;
use cratonvm_jit::{try_compile, CachedBytecodeMethod, CompiledMethod};
use cratonvm_types::{
    ClassId, ARRAY_DATA_OFFSET, ARRAY_LENGTH_OFFSET, COMPACT_HEADER_SIZE, GC_FLAGS_BYTE_OFFSET,
    GC_FLAG_COMPACT, GC_FLAG_OLD_GEN,
};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

static LOCK: Mutex<()> = Mutex::new(());

/// One region spanning `[0x1000, usize::MAX)`: every 8-aligned test buffer
/// passes the guarded receiver checks.
static WIDE_OPEN: [AtomicUsize; 6] = [
    AtomicUsize::new(0x1000),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

/// The generational plan: SATB idle, post gate armed, MASK shape.
static PRE_GATE: AtomicU8 = AtomicU8::new(0);
static POST_GATE: AtomicU8 = AtomicU8::new(1);

/// `&CardTable` of the test's old generation, as an address.
static CARD_TABLE: AtomicUsize = AtomicUsize::new(0);
static BARRIER_CALLS: AtomicUsize = AtomicUsize::new(0);
static PUTFIELD_HELPER_CALLS: AtomicUsize = AtomicUsize::new(0);
/// A slot whose value the barrier stub records when it runs (0 = none).
static WATCH: AtomicUsize = AtomicUsize::new(0);
/// `(obj argument, watched slot's value at the call)` per barrier call.
static SEEN: Mutex<Vec<(usize, u64)>> = Mutex::new(Vec::new());

unsafe extern "C" fn benign() -> i64 {
    0
}
unsafe extern "C" fn record_throw_bci(_bci: i64) {}
unsafe extern "C" fn self_guard(_vm: i64) -> i64 {
    0
}
unsafe extern "C" fn native_stack_floor() -> i64 {
    0
}
unsafe extern "C" fn type_check_ok(_vm: i64, _array: i64, _value: i64) -> i64 {
    0
}
unsafe extern "C" fn satb_idle(_vm: i64, _old: i64) {}
unsafe extern "C" fn counting_putfield_object(_vm: i64, _obj: i64, _idx: i64, _val: i64) {
    PUTFIELD_HELPER_CALLS.fetch_add(1, Ordering::SeqCst);
}

/// What `GenerationalHeap::write_barrier` does for a store the JIT already
/// range-tested: dirty the card of `obj` (an address, possibly an element
/// slot) through the card table's own mark path.
unsafe extern "C" fn recording_write_barrier(_vm: i64, obj: i64, _val: i64) {
    BARRIER_CALLS.fetch_add(1, Ordering::SeqCst);
    let watch = WATCH.load(Ordering::SeqCst);
    let seen = if watch == 0 {
        0
    } else {
        // SAFETY: `WATCH` is set only to a live, 8-aligned slot of a buffer the
        // running test owns for the whole call.
        unsafe { std::ptr::read_volatile(watch as *const u64) }
    };
    if let Ok(mut v) = SEEN.lock() {
        v.push((obj as usize, seen));
    }
    let ct = CARD_TABLE.load(Ordering::SeqCst);
    if ct != 0 {
        // SAFETY: `CARD_TABLE` holds the address of a `CardTable` the running
        // test keeps alive (and resets to 0) around every call.
        unsafe { &*(ct as *const CardTable) }.mark_dirty_lockfree(obj as usize);
    }
}

fn reset() {
    BARRIER_CALLS.store(0, Ordering::SeqCst);
    PUTFIELD_HELPER_CALLS.store(0, Ordering::SeqCst);
    WATCH.store(0, Ordering::SeqCst);
    if let Ok(mut v) = SEEN.lock() {
        v.clear();
    }
}

fn seen() -> Vec<(usize, u64)> {
    SEEN.lock().map(|v| v.clone()).unwrap_or_default()
}

/// A simulated old generation: a zeroed buffer and a card table over it.
struct OldGen {
    buf: Vec<u64>,
    ct: Box<CardTable>,
}

impl OldGen {
    fn new(options: CardTableOptions) -> Self {
        let buf = vec![0u64; 8192]; // 64 KiB = 128 cards
        let base = buf.as_ptr() as usize;
        let ct = Box::new(CardTable::with_options(base, buf.len() * 8, options));
        CARD_TABLE.store(&*ct as *const CardTable as usize, Ordering::SeqCst);
        Self { buf, ct }
    }
    fn base(&self) -> usize {
        self.buf.as_ptr() as usize
    }
    fn card(&self, addr: usize) -> usize {
        (addr - self.base()) / CARD_SIZE
    }
    fn dirty(&self, addr: usize) -> bool {
        self.ct.is_dirty(self.card(addr))
    }
    /// An old reference array of `len` elements at byte offset `off`.
    fn array(&mut self, off: usize, len: u32) -> usize {
        let a = self.base() + off;
        // SAFETY: `off + header + len * 8` is inside the 64 KiB buffer for
        // every caller here, and `a` is 8-aligned.
        unsafe {
            std::ptr::write_unaligned((a + ARRAY_LENGTH_OFFSET) as *mut u32, len);
            *((a + GC_FLAGS_BYTE_OFFSET) as *mut u8) = GC_FLAG_OLD_GEN;
        }
        a
    }
    /// An old compact object with one reference field, at
    /// `COMPACT_HEADER_SIZE` (a compact instance has no shape word: its
    /// field count is its class layout's).
    fn object(&mut self, off: usize) -> usize {
        let o = self.base() + off;
        // SAFETY: inside the buffer, 8-aligned.
        unsafe {
            *((o + GC_FLAGS_BYTE_OFFSET) as *mut u8) = GC_FLAG_OLD_GEN | GC_FLAG_COMPACT;
        }
        o
    }
}

impl Drop for OldGen {
    fn drop(&mut self) {
        CARD_TABLE.store(0, Ordering::SeqCst);
    }
}

fn read_word(addr: usize) -> u64 {
    // SAFETY: every caller passes an 8-aligned slot of a live test buffer.
    unsafe { std::ptr::read_volatile(addr as *const u64) }
}

fn element_slot(array: usize, i: usize) -> usize {
    array + ARRAY_DATA_OFFSET + i * cratonvm_types::narrow_oop::ref_element_size()
}

/// The helper table: every call target benign, bounds wide open, the card
/// view of `old` (or none), and optionally the generational gate plan.
fn helpers(old: Option<&OldGen>, gates: bool) -> JitRuntimeHelpers {
    let s = benign as *const () as usize;
    let (view, lo, hi) = match old {
        Some(o) => (
            o.ct.jit_card_view_addr(),
            o.base(),
            o.base() + o.buf.len() * 8,
        ),
        None => (0, 0, 0),
    };
    JitRuntimeHelpers {
        jit_card_table_addr: view,
        jit_card_old_base: lo,
        jit_card_old_end: hi,
        newarray: s,
        new_object: s,
        anewarray_object: s,
        baload: s,
        bastore: s,
        iaload: s,
        iastore: s,
        aaload: s,
        aastore: s,
        aastore_type_check: type_check_ok as *const () as usize,
        multianewarray_2d: s,
        arraylength: s,
        getfield: s,
        putfield_int: s,
        putfield_long: s,
        putfield_float: s,
        putfield_double: s,
        putfield_object: counting_putfield_object as *const () as usize,
        getstatic: s,
        putstatic_int: s,
        putstatic_long: s,
        putstatic_float: s,
        putstatic_double: s,
        putstatic_object: s,
        checkcast: s,
        instanceof_check: s,
        throw_aioobe: s,
        throw_arithmetic: s,
        invoke_dispatch: s,
        invoke_virtual_mic: s,
        lambda_int_to_double: s,
        write_barrier: recording_write_barrier as *const () as usize,
        satb_pre_write_barrier: satb_idle as *const () as usize,
        uncommon_trap: s,
        math_fma_double: s,
        math_fma_float: s,
        tlab_cursor_offset_in_thread: 0,
        tlab_end_offset_in_thread: 8,
        class_id_offset_in_obj: 0,
        throw_exception: s,
        jit_npe_with_action: s,
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        self_call_stack_guard: self_guard as *const () as usize,
        region_bounds_addr: WIDE_OPEN.as_ptr() as usize,
        read_bounds_addr: WIDE_OPEN.as_ptr() as usize,
        native_stack_floor_fn: native_stack_floor as *const () as usize,
        ldc_string: s,
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: s,
        ref_store_pre_gate: if gates {
            std::ptr::addr_of!(PRE_GATE) as usize
        } else {
            0
        },
        ref_store_post_gate: if gates {
            std::ptr::addr_of!(POST_GATE) as usize
        } else {
            0
        },
        ref_store_post_young_floor: 0,
        ref_store_post_skip_mask: if gates {
            usize::from(GC_FLAG_OLD_GEN)
        } else {
            0
        },
        ..Default::default()
    }
}

/// `code` padded with the VM's two trailing zero bytes, as a static method.
fn cached(name: &str, descriptor: &str, mut code: Vec<u8>, params: u16) -> CachedBytecodeMethod {
    code.push(0x00);
    code.push(0x00);
    CachedBytecodeMethod {
        declaring_class_id: ClassId::new(1),
        class_name: Arc::from("r4w4/Cards"),
        method_name: Arc::from(name),
        method_descriptor: Arc::from(descriptor),
        source_file: None,
        code: Arc::from(code.as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 8,
        max_locals: params,
        num_params: params,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        hidden_frame: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
        array_inc_declined: std::sync::OnceLock::new(),
        ctor_verdict_memo: std::sync::OnceLock::new(),
        pool_generation: u64::MAX,
    }
}

/// Compile single-pass with `CRATONVM_JIT_INLINE_CARD_MARK` as given. The flag
/// is read per store site at compile time, on this thread.

fn call(m: &CompiledMethod, args: &[i64]) {
    let dummy_vm = [0u64; 16];
    // SAFETY: JIT-compiled from valid bytecode; every reference argument is a
    // live, 8-aligned test buffer (or null), and every reachable helper is a
    // non-panicking stub.
    unsafe {
        if m.needs_context() {
            m.try_call_with_context(dummy_vm.as_ptr() as i64, args)
        } else {
            m.try_call(args)
        }
    }
    .expect("jit call");
}

/// `static void set(Object[] a, int i, Object v) { a[i] = v; }`
fn aastore_method() -> CachedBytecodeMethod {
    cached(
        "set",
        "([Ljava/lang/Object;ILjava/lang/Object;)V",
        vec![0x2a, 0x1b, 0x2c, 0x53, 0xb1],
        3,
    )
}

/// `static void put(Object h, Object v) { h.f = v; }` (`f` resolved as a
/// compact reference field at body offset 0).
fn putfield_method() -> CachedBytecodeMethod {
    cached(
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        vec![0x2a, 0x2b, 0xb5, 0x00, 0x01, 0xb1],
        2,
    )
}

const PRECISE: CardTableOptions = CardTableOptions {
    summary: true,
    precise_ref_array_marks: true,
};





