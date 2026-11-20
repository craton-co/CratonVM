#[allow(unused_imports)]
use super::*;

/// The `CRATONVM_DBG_PAD_ACCESS` row: one line per `(class, slot, op)` the
/// first time it is seen, then again at 1,000, 1,000,000 and every further
/// power of ten, so a hot writer is visible without flooding. A slot at or
/// past `num_total_fields` is a native over-allocation (the layout-alias
/// census's species), and an index past the object is the straystack door's;
/// neither is reported here.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn report_pad_access(
    shared: &SharedVm,
    thread: &JvmThread,
    obj: ObjectRef,
    class_id: ClassId,
    index: usize,
    op: &'static str,
) {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    if class_id.as_u32() == 0 {
        return;
    }
    let num_slots = shared.mem.heap.get_header(obj).num_slots() as usize;
    if index >= num_slots {
        return;
    }
    let (name, total) = match shared.classes.class_manager.read().get_class(class_id) {
        Some(c) => (std::sync::Arc::clone(&c.name), c.num_total_fields),
        None => return,
    };
    if index >= total {
        return;
    }
    // Diagnostic-only dedup table, keyed by the per-VM class id; it holds no
    // compatibility state and is never read by anything but this print.
    static SEEN: OnceLock<Mutex<HashMap<(u32, usize, &'static str), u64>>> = OnceLock::new();
    let hits = {
        let mut seen = SEEN
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let n = seen.entry((class_id.as_u32(), index, op)).or_insert(0);
        *n += 1;
        *n
    };
    let power_of_ten = |mut n: u64| {
        while n >= 10 && n % 10 == 0 {
            n /= 10;
        }
        n == 1
    };
    if hits != 1 && !(hits >= 1000 && power_of_ten(hits)) {
        return;
    }
    let native = CURRENT_NATIVE_STACK
        .with(|s| s.borrow().last().map(|(_, n)| n.clone()))
        .unwrap_or_else(|| "<unnamed: set CRATONVM_DBG_STRAYSTACK=1>".to_string());
    let frame = thread
        .frames
        .last()
        .map(|f| {
            format!(
                "{}.{}{}",
                f.class_name(),
                f.method_name(),
                f.method_descriptor()
            )
        })
        .unwrap_or_default();
    eprintln!(
        "[pad-access] {op} {name} slot={index} num_total_fields={total} hits={hits} \
         native={native} java={frame}"
    );
}

/// The token [`NativeContextImpl`] puts in a TRUSTED
/// [`cratonvm_native_api::ReadCursor`].
///
/// Opaque to `native-api` and meaningful only to this implementor. It is a
/// constant rather than, say, the object's header address, because nothing on
/// the read path needs the address — the cursor already carries the forwarded
/// `ObjectRef`, and what the token decides is only "may this region skip the
/// membership walk". A distinctive value rather than `1` so that a token
/// minted by some other heap cannot be mistaken for this one.
pub(in crate::vm::vm_exec) const READ_CURSOR_TRUSTED: usize = 0x5ead_c072;

/// Whether a read region may skip the membership walk its checked accessors do.
///
/// Default ON. `CRATONVM_NATIVE_NO_READ_CURSOR=1` makes every cursor
/// untrusted, restoring one `is_object_address` per accessor call — so the
/// whole validate-once region is an A/B inside ONE binary, exactly as
/// `CRATONVM_GC_NO_VALIDATE_ONCE` is for the twins one layer down. A check
/// added for a reproduced crash deserves a falsifier that does not need a
/// second build.
///
/// Read once; consulted on entry to every region.
#[inline]
pub(in crate::vm::vm_exec) fn native_read_cursor_trusted() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        !false
    })
}

/// Throttle stride for the whole-young scan (every Nth native). Default 1.
#[inline]
pub(in crate::vm::vm_exec) fn youngscan_stride() -> u64 {
    use std::sync::OnceLock;
    static S: OnceLock<u64> = OnceLock::new();
    *S.get_or_init(|| {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(1)
    })
}

/// One-shot latch so the youngscan dumps only the FIRST `0x4` (its writer).
pub(in crate::vm::vm_exec) static YOUNGSCAN_FOUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[inline]
pub(in crate::vm::vm_exec) fn youngscan_found() -> bool {
    YOUNGSCAN_FOUND.load(std::sync::atomic::Ordering::Relaxed)
}
/// CAS the latch; returns `true` if it was ALREADY set (so the caller should skip).
#[inline]
pub(in crate::vm::vm_exec) fn youngscan_mark_found() -> bool {
    YOUNGSCAN_FOUND.swap(true, std::sync::atomic::Ordering::Relaxed)
}

/// The eleven startup-static diagnostic bits, computed once.
#[inline]
pub(in crate::vm::vm_exec) fn native_diag_static() -> u32 {
    static M: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *M.get_or_init(|| {
        let mut m = 0;
        if blockgc_dbg() {
            m |= native_diag::BLOCKGC;
        }
        if JvmThread::vm_state_diagnostics_enabled() {
            m |= native_diag::VM_STATE;
        }
        if youngscan_straystack_enabled() {
            m |= native_diag::STRAYSTACK;
        }
        if crate::runtime::ec_watch::native_enabled() {
            m |= native_diag::EC_WATCH;
        }
        if crate::runtime::memwatch::is_watching() {
            m |= native_diag::MEMWATCH;
        }
        if youngscan_enabled() {
            m |= native_diag::YOUNGSCAN;
        }
        if crate::runtime::interpreter::dbg_linkage() {
            m |= native_diag::LINKAGE;
        }
        if crate::memory::gc::altrace_enabled_vm() {
            m |= native_diag::ALTRACE;
        }
        if unpin_ring_enabled() {
            m |= native_diag::UNPIN_RING;
        }
        if cratonvm_gc::stale_objref_debug::enabled() {
            m |= native_diag::STALE_OBJREF;
        }
        if crate::runtime::interpreter::remap_trace_on() {
            m |= native_diag::REMAP_TRACE;
        }
        m
    })
}

/// The two runtime-toggleable bits. Two relaxed loads, deliberately not cached.
#[inline]
pub(in crate::vm::vm_exec) fn native_diag_dynamic() -> u32 {
    let mut m = 0;
    if crate::dispatch_trace::is_enabled() {
        m |= native_diag::DISPATCH_TRACE;
    }
    // Covers BOTH of `record_enter`'s gates (the ring and
    // `CRATONVM_TRACK_NATIVE`); see `native_ring::any_recording_enabled`.
    if cratonvm_native_api::native_ring::any_recording_enabled() {
        m |= native_diag::RING;
    }
    m
}

/// Every diagnostic gate on the native funnel, as one word.
///
/// Zero on any run with no diagnostics armed, which is the case this exists to
/// make fast.
#[inline]
pub(in crate::vm::vm_exec) fn native_diag_mask() -> u32 {
    native_diag_static() | native_diag_dynamic()
}

/// Per-call diagnostic state that has to survive across the callback.
///
/// All-zero/`None` on the fast path, and constructed without touching any of
/// the cold code.
#[derive(Default)]
pub(in crate::vm::vm_exec) struct NativeDiagState {
    /// Resolved native name, when `CRATONVM_DBG_VM_STATE` is on.
    pub(in crate::vm::vm_exec) vm_state_name: Option<String>,
    /// Ring token from `record_enter`; `None` when the ring never ran.
    pub(in crate::vm::vm_exec) ring_idx: Option<usize>,
    /// Whether the straystack thread-local was pushed and must be popped.
    pub(in crate::vm::vm_exec) straystack_pushed: bool,
}

/// Forward one `Value` crossing the native-API boundary through the read
/// barrier.
///
/// Natives hold `ObjectRef`s in Rust locals, which are in no root set, across
/// operations that can collect — that is precisely why every entry point of
/// this API already forwards its RECEIVER. The VALUES they hand back were not
/// forwarded, so a native that read an object before a callback and stored it
/// afterwards wrote a stale pointer straight into the heap, where the next
/// reader `checkcast`s it and gets whatever now occupies the address.
///
/// Cheap and confined: this is the native boundary, not the interpreter's
/// `putfield`, and the barrier short-circuits on anything that is not a moved
/// object.
/// Stores `NativeContext::set_field_by_name` DROPPED because the receiver's
/// class does not declare the named field.
pub(in crate::vm::vm_exec) static FIELD_BY_NAME_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many `set_field_by_name` calls named a field their receiver does not
/// have. Always counted; see [`note_field_by_name_dropped`] for what a non-zero
/// value does and does not mean.
pub fn field_by_name_dropped_count() -> u64 {
    FIELD_BY_NAME_DROPPED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The silent arm of `set_field_by_name`, made audible under
/// `CRATONVM_DBG_DEADREF_STORE`.
///
/// # Why this is where a stale receiver actually surfaces
///
/// `set_field_by_name` resolves the field against the class of whatever object
/// is AT the address it is handed. A native that kept a receiver across an
/// allocation hands it a vacated address — and a young semispace is re-served
/// from the same base every cycle, so that address is usually occupied by some
/// unrelated object by the time the store runs. The field name then does not
/// resolve, and the store disappears **before** it reaches the heap: no
/// `set_field`, so no `[deadref-store]`, no `[deadref-recv]`, no
/// `[CELLWATCH]`, no `[PUTFIELD-WATCH]`. Nothing in the VM said anything.
///
/// That is exactly how `native_assertj_lightweight_comparable_assert`'s
/// `objects` store vanished (see
/// `docs/feature-designs/real-jdk-mode.md`),
/// and it is why the heap-side screens could not have found it however many
/// arms they grew.
///
/// # Why it is counted always and printed only under the flag
///
/// A miss is not always a defect: several natives set a field that only some
/// subclasses declare (`strings` and `failures` on the AssertJ assertions are
/// the local example), so an unconditional warning would be noise — MEASURED, a
/// single `BindableTests` run drops 2 710 stores and ~2 700 of them are one
/// benign shape, `StringBuilder.toStringCache`. The COUNT is free and makes
/// "did this happen at all" answerable; the line with its backtrace is behind
/// the same switch as the rest of the stale-reference family, because that is
/// when someone is asking this question, and it is deduped by
/// `(receiver class, field name)` so that population cannot crowd out the one
/// row a reader opened the log for.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn note_field_by_name_dropped(
    class_id: ClassId,
    class_name: &str,
    field_name: &str,
    obj: ObjectRef,
    value: Value,
) {
    FIELD_BY_NAME_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if !cratonvm_types::flags().gc.dbg_deadref_store {
        return;
    }
    // DEDUPE BY SHAPE, not by occurrence count. A plain "first N" cap is worse
    // than useless here and was measured to be: a BindableTests run drops 2 710
    // stores, ~2 700 of them one benign shape (`StringBuilder.toStringCache`),
    // so a 32-line cap spends every line on the noise and never reaches the
    // one that matters. One line per distinct `(receiver class, field name)`
    // turns the same population into a handful of rows, and a stale receiver is
    // a shape nothing else in the run produces.
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<(u32, String)>>> =
        std::sync::Mutex::new(None);
    let mut guard = match SEEN.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let seen = guard.get_or_insert_with(std::collections::HashSet::new);
    if seen.len() >= 256 || !seen.insert((class_id.as_u32(), field_name.to_string())) {
        return;
    }
    let n = seen.len() - 1;
    drop(guard);
    eprintln!(
        "[field-by-name-dropped] #{n} obj=0x{:x} recv_class={class_name} \
         recv_class_id={} field={field_name:?} value={value:?} — the receiver's class does not \
         declare this field, so the store was DROPPED. Either the caller named a field only some \
         subclasses have, or the receiver is a stale address now occupied by a different object.\n{:?}",
        obj.as_ptr() as usize,
        class_id.as_u32(),
        std::backtrace::Backtrace::force_capture(),
    );
}

/// The raw half of `NativeContextImpl::copy_reference_array_elements`: copy
/// `len` reference slots word by word, or answer `false` (nothing written)
/// when the heap needs the per-element accessors.
///
/// Taken only on ZGC with BOTH of its reference barriers quiet:
///
/// * `mark_active` -- the SATB pre-barrier `set_array_element` publishes the
///   overwritten value through. It is raised inside the mark-start pause, and
///   a native runs outside every pause (the census waits for it), so a value
///   read `false` here stays `false` until this copy returns.
/// * `load_barrier_armed` -- a coloured slot word must be healed through the
///   read barrier, which a verbatim copy would skip.
///
/// The one post-store obligation left is ZGC's object-keyed card
/// (`note_ref_store`), the same call every per-element store makes for the
/// same `dst`, so recording it once is identical.
///
/// Each slot moves as ONE relaxed atomic word of the heap's reference width
/// (`element_byte_size(Reference)`: 8, or 4 under narrow oops, whose encoding
/// is position-independent), so a thread racing on `dst` reads either the old
/// or the new reference and never a torn one.
#[allow(clippy::too_many_arguments)]
#[cfg(feature = "zgc")]
#[inline]
unsafe fn zgc_satb_pre_slot(z: &cratonvm_gc::zgc::ZgcRealHeap, dst_base: *const u8, slot: usize) {
    // SAFETY: caller guarantees `dst_base + slot * 8` is a valid, 8-aligned,
    // uncolored live reference-array slot.
    let old_word = unsafe {
        (*(dst_base.add(slot * 8) as *const std::sync::atomic::AtomicU64))
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    if old_word != 0 {
        z.satb_pre_barrier(old_word as usize);
    }
}

pub(crate) fn copy_reference_slots_raw(
    heap: &crate::memory::VmHeap,
    src: ObjectRef,
    src_off: usize,
    dst: ObjectRef,
    dst_off: usize,
    len: usize,
    backward: bool,
) -> bool {
    if copy_reference_slots_raw_g1_gen(heap, src, src_off, dst, dst_off, len, backward) {
        return true;
    }
    #[cfg(feature = "zgc")]
    {
        if !heap.is_zgc() {
        return false;
    }
    if heap.mark_active() {
            return false;
        }
        let (Some(src_base), Some(dst_base)) = (heap.array_data_ptr(src), heap.array_data_ptr(dst))
        else {
            return false;
        };
        // Armed: each word through the load barrier, stored colored.
        let z = heap;
        if z.load_barrier_armed() {
            let copied = z.copy_reference_slots_colored(
                src_base as usize,
                src_off,
                dst_base as usize,
                dst_off,
                len,
                backward,
            );
            if copied {
                heap.zgc_note_ref_store(dst.as_ptr() as usize);
            }
            return copied;
        }
        let width = cratonvm_types::element_byte_size(ArrayElementType::Reference);
        if heap.zgc_mark_active() {
            if width != 8 {
                return false;
            }
            for i in 0..len {
                // SAFETY: `dst` is a live reference array, `dst_off..dst_off+len`
                // is in bounds, and `!z.load_barrier_armed()`.
                let word = unsafe {
                    (*(dst_base.add((dst_off + i) * 8) as *const std::sync::atomic::AtomicU64))
                        .load(std::sync::atomic::Ordering::Relaxed)
                };
                if word != 0 {
                    heap.zgc_satb_pre_barrier(word as usize);
                }
            }
        }
        // SAFETY: the caller validated both arrays as live reference arrays
        // and both ranges against their lengths; every slot address is
        // `width`-aligned (the payload starts 8-aligned and slots are
        // `width` wide), and nothing can move either array while this native
        // runs. Copying in memmove order keeps an overlapping same-array
        // range correct.
        unsafe {
            match width {
                8 => copy_slots_atomic::<std::sync::atomic::AtomicU64>(
                    src_base, src_off, dst_base, dst_off, len, backward,
                ),
                4 => copy_slots_atomic::<std::sync::atomic::AtomicU32>(
                    src_base, src_off, dst_base, dst_off, len, backward,
                ),
                _ => return false,
            }
        }
        heap.zgc_note_ref_store(dst.as_ptr() as usize);
        true
    }
    #[cfg(not(feature = "zgc"))]
    {
        let _ = (heap, src, src_off, dst, dst_off, len, backward);
        false
    }
}



/// The G1 / Generational half of [`copy_reference_slots_raw`]: copy `len`
/// 8-byte reference slots word by word, then run, once per STORED non-null
/// reference, exactly the post-store obligation the heap's own
/// `set_array_element` runs per element -- G1's `post_write_barrier_rset`
/// (its only post barrier), Generational's `write_barrier_ref_array_element`
/// (the element-precise card). `false`, NOTHING written, whenever a store
/// would need more:
///
/// * a SATB pre-barrier: G1 with its marking window open
///   (`raw_ref_array_copy_admitted`), Generational with its native-store SATB
///   queue active (`native_store_must_publish`) -- both windows open only in
///   the initial-mark pause, which a caller holding raw references (a native,
///   or a JIT helper that does not poll) excludes, so a `false` read here
///   stays `false` for the copy;
/// * a G1 array that may be humongous (its payload is reached through a
///   region-translated span, not one flat range);
/// * narrow (4-byte) reference slots, which these barriers would need decoded.
///
/// Words are copied verbatim (an auto-box wrapper stays the wrapper), as the
/// ZGC arm does, in memmove order.
#[allow(clippy::too_many_arguments)]
pub(in crate::vm::vm_exec) fn copy_reference_slots_raw_g1_gen(
    heap: &crate::memory::VmHeap,
    src: ObjectRef,
    src_off: usize,
    dst: ObjectRef,
    dst_off: usize,
    len: usize,
    backward: bool,
) -> bool {
    if cratonvm_types::element_byte_size(ArrayElementType::Reference) != 8 {
        return false;
    }
    let admitted = if heap.is_g1() {
        let total = |array: ObjectRef| {
            heap.array_length(array)
                .checked_mul(8)
                .and_then(|bytes| bytes.checked_add(cratonvm_types::ARRAY_DATA_OFFSET))
                .unwrap_or(usize::MAX)
        };
        heap.g1_raw_ref_array_copy_admitted(total(src), total(dst))
    } else if heap.is_generational() {
        !heap.native_store_must_publish()
    } else {
        false
    };
    if !admitted {
        return false;
    }
    let (Some(src_base), Some(dst_base)) = (heap.array_data_ptr(src), heap.array_data_ptr(dst))
    else {
        return false;
    };
    // SAFETY: the caller validated both arrays as live, non-humongous (G1)
    // reference arrays and both ranges against their lengths; slots are
    // 8-aligned 8-byte words, and nothing can move either array while the
    // caller holds these references.
    unsafe {
        copy_slots_atomic::<std::sync::atomic::AtomicU64>(
            src_base, src_off, dst_base, dst_off, len, backward,
        );
    }
    for i in 0..len {
        // SAFETY: as above; the slot just written.
        let word = unsafe {
            (*(dst_base.add((dst_off + i) * 8) as *const std::sync::atomic::AtomicU64))
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        if word == 0 {
            continue;
        }
        // SAFETY: a non-zero word of a live reference slot is a live object.
        let target = unsafe { ObjectRef::from_raw(word as usize as *mut u8) };
        if heap.is_g1() {
            heap.g1_post_write_barrier_rset(dst, target);
        } else if heap.is_generational() {
            heap.gen_write_barrier_ref_array_element(dst, dst_off + i, Value::Object(Some(target)));
        }
    }
    true
}

/// JIT r23 wave 3 review (lane L5's `try_reference_arraycopy`): the CHECKED
/// raw reference copy -- each slot is loaded once, proven with `storable`
/// (`false` stops the copy there) and the SAME word stored, so a racing
/// writer of `src` can never slip an unproven reference into `dst` between a
/// separate proof pass and the copy (HotSpot's checkcast arraycopy checks the
/// element it stores, too). Forward only; `src` and `dst` must be different
/// arrays (a checked copy always is: one array has one component id).
///
/// `None` -- nothing written -- when the collector state needs the
/// per-element accessors (exactly the states [`copy_reference_slots_raw`]
/// refuses: ZGC marking or load barrier armed, G1 marking window or a possibly
/// humongous array, Generational native SATB publishing, narrow slots, the
/// `CRATONVM_G1_GEN_RAW_REF_ARRAYCOPY` kill switch on G1 / Generational).
/// `Some(n)`: the first `n` slots were copied with their post-store
/// obligations (ZGC's object card, G1's `post_write_barrier_rset`,
/// Generational's element card); `n < len` means slot `n` failed the proof.
#[allow(clippy::too_many_arguments)]
pub(crate) fn copy_reference_slots_raw_checked(
    heap: &crate::memory::VmHeap,
    src: ObjectRef,
    src_off: usize,
    dst: ObjectRef,
    dst_off: usize,
    len: usize,
    storable: &mut dyn FnMut(ObjectRef) -> bool,
) -> Option<usize> {
    if cratonvm_types::element_byte_size(ArrayElementType::Reference) != 8
        || src.as_ptr() == dst.as_ptr()
    {
        return None;
    }
    let admitted = if heap.is_g1() {
        let total = |array: ObjectRef| {
            heap.array_length(array)
                .checked_mul(8)
                .and_then(|bytes| bytes.checked_add(cratonvm_types::ARRAY_DATA_OFFSET))
                .unwrap_or(usize::MAX)
        };
        heap.g1_raw_ref_array_copy_admitted(total(src), total(dst))
    } else if heap.is_generational() {
        !heap.native_store_must_publish()
    } else {
        !heap.mark_active() && !heap.load_barrier_armed()
    };
    if !admitted {
        return None;
    }
    let zgc_satb = heap.zgc_mark_active();
    let (Some(src_base), Some(dst_base)) = (heap.array_data_ptr(src), heap.array_data_ptr(dst))
    else {
        return None;
    };
    let mut n = 0usize;
    while n < len {
        // SAFETY: the caller validated both arrays as live (G1: non-humongous)
        // reference arrays and both ranges against their lengths; slots are
        // 8-aligned 8-byte words holding plain pointers in an admitted state,
        // and nothing can move either array while the caller holds them.
        let word = unsafe {
            (*(src_base.add((src_off + n) * 8) as *const std::sync::atomic::AtomicU64))
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        if word != 0 {
            // SAFETY: a non-zero word of a live reference slot is a live object.
            let elem = unsafe { ObjectRef::from_raw(word as usize as *mut u8) };
            if !storable(elem) {
                break;
            }
        }
        if zgc_satb {
            // SAFETY: `dst_base + (dst_off + n) * 8` is a valid, uncolored slot.
            let word = unsafe {
                (*(dst_base.add((dst_off + n) * 8) as *const std::sync::atomic::AtomicU64))
                    .load(std::sync::atomic::Ordering::Relaxed)
            };
            if word != 0 {
                heap.zgc_satb_pre_barrier(word as usize);
            }
        }
        // SAFETY: as for the load.
        unsafe {
            (*(dst_base.add((dst_off + n) * 8) as *const std::sync::atomic::AtomicU64))
                .store(word, std::sync::atomic::Ordering::Relaxed);
        }
        if word != 0 {
            // SAFETY: as above.
            let target = unsafe { ObjectRef::from_raw(word as usize as *mut u8) };
            if heap.is_g1() {
            heap.g1_post_write_barrier_rset(dst, target);
        } else if heap.is_generational() {
            heap.gen_write_barrier_ref_array_element(dst, dst_off + n, Value::Object(Some(target)));
        }
        }
        n += 1;
    }
    #[cfg(feature = "zgc")]
    {
        if n > 0 {
            heap.zgc_note_ref_store(dst.as_ptr() as usize);
        }
    }
    Some(n)
}

// ---------------------------------------------------------------------------
// Round 16 wave 9 (lane satb): the SATB pre-barrier of a NATIVE reference store
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/jit-barrier-lowering.md`.
// SATB keeps every object reachable at the mark snapshot, so a store that
// deletes an edge must log the value it overwrites while a concurrent mark is
// running. The interpreter's `aastore` and the JIT's store helpers read the old
// value and call `VmHeap::satb_barrier` before the store. Per collector, the
// heap's own store accessors differ:
//
// * G1 (`G1Collector::set_field` / `set_array_element`) and ZGC
//   (`ZgcRealHeap::set_field` / `set_array_element`) publish the overwritten
//   reference INSIDE the accessor, under their own mark-active gate. A native
//   store reaching them is already covered; publishing it again here would
//   only log it twice.
// * Generational (`GenerationalHeap::set_field` / `set_field_as` /
//   `set_field_volatile` / `set_array_element`) does the card mark only. Its
//   `write_barrier` doc says a caller that reaches the heap API directly ("a
//   native, a reflective store, `ctx.set_field`") must call `satb_barrier`
//   itself -- and `NativeContextImpl`'s store accessors did not, so every
//   `System.arraycopy` reference loop, every `ArrayList` native shifting its
//   backing array and every native field store could hide the value it
//   overwrote from the old generation's concurrent marker.
//
// So the accessors below ask `native_store_must_publish` (one Acquire load of
// the Generational SATB queue's state; `false` on G1/ZGC and while nothing
// marks) and only then read the old value and publish it. Kill switch
// `CRATONVM_NATIVE_STORE_SATB=0` (read only once the gate has said "marking",
// so the steady state never reads it).

/// What a native reference store needs from the heap to publish the value it
/// is about to overwrite. A trait so the decision can be unit-tested against a
/// mock heap that records the barrier calls.
pub(crate) trait NativeStoreSatb {
    /// `true` only when the heap is accepting SATB logs AND its own store
    /// accessors do not publish the overwritten reference themselves.
    fn native_store_must_publish(&self) -> bool;
    /// The element about to be overwritten (`Value::Object(None)` when there
    /// is none to read: out of range, not an array).
    fn overwritten_array_element(&self, obj: ObjectRef, index: usize) -> Value;
    /// The field slot about to be overwritten, read through the same
    /// descriptor the store will use (`None`: the descriptor-less accessor).
    fn overwritten_field(&self, obj: ObjectRef, index: usize, desc: Option<u8>) -> Value;
    /// The SATB pre-barrier entry (`VmHeap::satb_barrier`).
    fn publish_overwritten(&self, old: Value);
}

impl NativeStoreSatb for crate::memory::VmHeap {
    #[inline]
    fn native_store_must_publish(&self) -> bool {
        match self {
            _ if self.is_generational() => self.gen_satb_active(),
            // G1 and ZGC publish inside their accessors (see above).
            _ => false,
        }
    }

    fn overwritten_array_element(&self, obj: ObjectRef, index: usize) -> Value {
        self.get_array_element(obj, index)
            .unwrap_or(Value::Object(None))
    }

    fn overwritten_field(&self, obj: ObjectRef, index: usize, desc: Option<u8>) -> Value {
        match desc {
            Some(desc) => self.get_field_as(obj, index, desc),
            None => self.get_field(obj, index),
        }
    }

    #[inline]
    fn publish_overwritten(&self, old: Value) {
        self.satb_barrier(old);
    }
}

/// Can a slot of field descriptor `desc` hold a reference? Only a primitive
/// descriptor byte says no; `None` and the raw-slot descriptor
/// (`RAW_SLOT_DESCRIPTOR`, 0) say "maybe", and the read decides.
#[inline]
pub(in crate::vm::vm_exec) fn descriptor_may_hold_reference(desc: Option<u8>) -> bool {
    !matches!(
        desc,
        Some(b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z')
    )
}

/// The SATB pre-barrier of a native reference-ARRAY store of `obj[index]`.
/// Call it with the forwarded array, BEFORE the store. Returns whether a
/// reference was published (for tests). `enabled` is the kill switch, asked
/// only after the heap said a publish is needed.
#[inline]
pub(crate) fn publish_overwritten_array_element<H: NativeStoreSatb + ?Sized>(
    heap: &H,
    enabled: impl FnOnce() -> bool,
    obj: ObjectRef,
    index: usize,
) -> bool {
    if !heap.native_store_must_publish() || !enabled() {
        return false;
    }
    let old = heap.overwritten_array_element(obj, index);
    if !matches!(old, Value::Object(Some(_))) {
        return false;
    }
    heap.publish_overwritten(old);
    true
}

/// The SATB pre-barrier of a native FIELD store of slot `index` of `obj`,
/// with the descriptor the store will use (`None` for the descriptor-less
/// accessors). Same contract as [`publish_overwritten_array_element`].
#[inline]
pub(crate) fn publish_overwritten_field<H: NativeStoreSatb + ?Sized>(
    heap: &H,
    enabled: impl FnOnce() -> bool,
    obj: ObjectRef,
    index: usize,
    desc: Option<u8>,
) -> bool {
    if !heap.native_store_must_publish() || !descriptor_may_hold_reference(desc) || !enabled() {
        return false;
    }
    let old = heap.overwritten_field(obj, index, desc);
    if !matches!(old, Value::Object(Some(_))) {
        return false;
    }
    heap.publish_overwritten(old);
    true
}

/// One relaxed atomic word per slot -- see [`copy_reference_slots_raw`].
///
/// JIT r23 wave 3 review: not `cfg(feature = "zgc")` any more -- the G1 /
/// Generational arm (`copy_reference_slots_raw_g1_gen`) is built without the
/// feature and copies through [`copy_slots_atomic`] too.
pub(in crate::vm::vm_exec) trait RefSlotWord {
    unsafe fn copy_one(src: *mut u8, dst: *mut u8);
    const WIDTH: usize;
}

impl RefSlotWord for std::sync::atomic::AtomicU64 {
    const WIDTH: usize = 8;
    #[inline(always)]
    unsafe fn copy_one(src: *mut u8, dst: *mut u8) {
        use std::sync::atomic::{AtomicU64, Ordering};
        let v = (*(src as *const AtomicU64)).load(Ordering::Relaxed);
        (*(dst as *const AtomicU64)).store(v, Ordering::Relaxed);
    }
}

#[cfg(feature = "zgc")]
impl RefSlotWord for std::sync::atomic::AtomicU32 {
    const WIDTH: usize = 4;
    #[inline(always)]
    unsafe fn copy_one(src: *mut u8, dst: *mut u8) {
        use std::sync::atomic::{AtomicU32, Ordering};
        let v = (*(src as *const AtomicU32)).load(Ordering::Relaxed);
        (*(dst as *const AtomicU32)).store(v, Ordering::Relaxed);
    }
}

#[inline]
pub(in crate::vm::vm_exec) unsafe fn copy_slots_atomic<W: RefSlotWord>(
    src_base: *mut u8,
    src_off: usize,
    dst_base: *mut u8,
    dst_off: usize,
    len: usize,
    backward: bool,
) {
    let src = src_base.add(src_off * W::WIDTH);
    let dst = dst_base.add(dst_off * W::WIDTH);
    if backward {
        for i in (0..len).rev() {
            W::copy_one(src.add(i * W::WIDTH), dst.add(i * W::WIDTH));
        }
    } else {
        for i in 0..len {
            W::copy_one(src.add(i * W::WIDTH), dst.add(i * W::WIDTH));
        }
    }
}

#[inline]
pub(in crate::vm::vm_exec) fn forward_boundary_value(heap: &crate::memory::VmHeap, value: Value) -> Value {
    match value {
        Value::Object(Some(obj)) => Value::Object(Some(heap.load_and_forward(obj))),
        other => other,
    }
}

/// JIT round 23 wave 2e (`jit-r23-w2-proxy-proposals.md` item 1): the
/// receiver-prepended argument list `NativeContextImpl::invoke_virtual` builds
/// per native->Java callback, inline on the stack for up to eight values (the
/// receiver plus seven arguments) instead of a process-heap `Vec`, whose
/// allocate/free pair was 5-21 % of the proxy-define row's samples on Windows
/// (`RtlAllocateHeap` under `invoke_virtual`). Larger lists spill as a `Vec`
/// would.
pub(in crate::vm::vm_exec) type InvokeVirtualArgs = smallvec::SmallVec<[Value; 8]>;

/// The dispatch class name `NativeContextImpl::invoke_virtual` resolves: the
/// class's own interned `Arc<str>` (a refcount bump) or the lambda proxy's
/// functional interface, never a fresh `String` per call (same item). Read only
/// as `&str`.
pub(in crate::vm::vm_exec) enum InvokeVirtualClassName {
    Static(&'static str),
    Shared(std::sync::Arc<str>),
    Owned(String),
}

impl std::ops::Deref for InvokeVirtualClassName {
    type Target = str;
    fn deref(&self) -> &str {
        match self {
            InvokeVirtualClassName::Static(s) => s,
            InvokeVirtualClassName::Shared(s) => s,
            InvokeVirtualClassName::Owned(s) => s,
        }
    }
}

/// Superclass links [`NativeContextImpl::invoke_virtual`]'s `loadClass` screen
/// reads; a chain longer than this is not a real hierarchy.
pub(in crate::vm::vm_exec) const LOAD_CLASS_CHAIN_LIMIT: usize = 256;

/// One class on a loader receiver's superclass chain, as
/// [`load_class_uses_base_native`] reads it.
#[derive(Clone, Copy)]
pub(in crate::vm::vm_exec) struct LoadClassLink {
    /// The class is `java/lang/ClassLoader` itself.
    pub(in crate::vm::vm_exec) is_class_loader: bool,
    /// The class declares the `loadClass` method being called.
    pub(in crate::vm::vm_exec) declares: bool,
    /// The class has real application bytes (class path or a user loader).
    pub(in crate::vm::vm_exec) application_bytes: bool,
}

/// May a native->Java `loadClass(String)` on a loader receiver run CratonVM's
/// base `ClassLoader` native (parent-first delegation) rather than a Java body?
///
/// `chain` is the receiver's class first, then each superclass. The receiver
/// itself answers `false` when it declares the method (any origin: the
/// historical rule) and `true` when it is `ClassLoader`. A superclass answers
/// `false` only when it has application bytes and declares the method: a
/// genuine override HotSpot would select (JVMS 5.4.6), while an inherited JDK
/// body, a stub or a boot-image class keeps the base native as before. The walk
/// ends at `ClassLoader`. An empty chain (class not stored) is `false`.
/// `prx23`.
pub(in crate::vm::vm_exec) fn load_class_uses_base_native(chain: impl IntoIterator<Item = LoadClassLink>) -> bool {
    let mut seen_any = false;
    for (index, link) in chain.into_iter().enumerate() {
        seen_any = true;
        if link.is_class_loader {
            return true;
        }
        if link.declares && (index == 0 || link.application_bytes) {
            return false;
        }
    }
    seen_any
}


/// Forward every reference ARGUMENT a native hands to a callback.
///
/// The companion to [`forward_boundary_value`], and the gap it leaves. Every
/// `NativeContext` entry point forwards its RECEIVER, and the write entry points
/// were taught to forward their VALUE; the `invoke_*` family passed `args`
/// straight through. So a native that read an object, called back into Java, and
/// then passed that object to a second callback handed a **stale pointer into a
/// Java frame** — from where it is stored, compared, or `checkcast`-ed with
/// nothing on the path to repair it.
///
/// This is the generic form of a defect fixed one native at a time five times
/// over (`h2_comparison_compare`, `h2_comparison_get_value`,
/// `h2_parser_test_token_fast`, `h2_condition_and_or_get_value`,
/// `h2_coalesce_function_get_value` — see
/// `docs/feature-designs/native-handle-discipline.md`).
/// A native may hold raw `ObjectRef`s across its own Rust code — the STW census
/// waits for `NativeRunning` — but a callback into Java ends that protection,
/// and this is the one boundary every such argument must cross.
///
/// **It does not make per-site pinning unnecessary.** It repairs the value ON
/// THE WAY IN; the native's own Rust local stays stale, so anything that
/// compares it by identity, reads a field off it, or passes it somewhere else
/// still needs a pin and a re-read. What it removes is the worst outcome — the
/// stale pointer escaping into the heap.
///
/// Allocates only when something actually moved: the common case walks the
/// arguments, finds every one already current, and returns the caller's slice
/// untouched.
#[inline]
pub(in crate::vm::vm_exec) fn forward_boundary_args<'a>(
    heap: &crate::memory::VmHeap,
    args: &'a [Value],
    buf: &'a mut Vec<Value>,
) -> &'a [Value] {
    let mut forwarded = false;
    for (index, value) in args.iter().enumerate() {
        let Value::Object(Some(obj)) = value else {
            continue;
        };
        let moved = heap.load_and_forward(*obj);
        if moved == *obj {
            continue;
        }
        if !forwarded {
            buf.clear();
            buf.extend_from_slice(args);
            forwarded = true;
        }
        buf[index] = Value::Object(Some(moved));
    }
    if forwarded {
        buf.as_slice()
    } else {
        args
    }
}

/// Stamp a point of a reflective door's gate or of its callee's re-entry for
/// the door census (`CRATONVM_REFLECT_DOOR_PHASE_STATS`; interpreter round i3
/// wave 81, lane L3): the census splits its `gap` and `call` columns at these
/// stamps. A measurement instrument; off, one registry field load.
#[inline]
pub(crate) fn reflect_door_gate_stamp(
    shared: &SharedVm,
    point: cratonvm_native_builtins::ReflectDoorGatePoint,
) {
    if shared.natives.native_methods.reflect_door_phase_stats() {
        cratonvm_native_builtins::reflect_door_gate_stamp(point);
    }
}

#[track_caller]
pub fn safe_native_call(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: NativeCallback,
    args: &[Value],
) -> MethodCallResult {
    // DIAGNOSTIC (perf/invokeinterface-native-arbitration-20260824).
    //
    // `java/util/ArrayList$Itr.hasNext`/`next` are entered 2 000 000 times on a
    // 2000x1000 walk and NEVER appear at any of the six
    // `real_protected_stub_class` doors -- while `cratonvm/internal/
    // UnmodifiableListItr`, also an iterator reached by `invokeinterface`, DOES
    // appear at two of them. So "interface calls bypass the arbitration" is not
    // the rule, and the next question is which of this function's 77 callers
    // actually invokes the Itr natives.
    //
    // `#[track_caller]` again rather than 77 hand-placed traces.
    if crate::runtime::interpreter::dbg_native_entry() {
        let l = std::panic::Location::caller();
        crate::runtime::interpreter::native_entry_note(l.file(), l.line());
    }
    safe_native_call_impl(shared, thread, callback, args, false, false, false)
}

/// Variant for dispatch paths that have already validated every non-null
/// `Value::Object` against this VM's heap. It preserves ordinary native-call
/// pinning and all return/exception handling while avoiding a duplicate heap
/// membership search for each argument.
#[track_caller]
pub(crate) fn safe_native_call_prevalidated_objects(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: NativeCallback,
    args: &[Value],
) -> MethodCallResult {
    // Same tally as `safe_native_call`. This variant is the one the HOT paths
    // use -- the first pass instrumented only the other wrapper and saw ~10 000
    // entries where the probe makes 600 000 native calls, which is how this
    // split came to light.
    if crate::runtime::interpreter::dbg_native_entry() {
        let l = std::panic::Location::caller();
        crate::runtime::interpreter::native_entry_note(l.file(), l.line());
    }
    safe_native_call_impl(shared, thread, callback, args, true, false, false)
}

/// The funnel for a compiled call served from a WARM JIT native-site-cache
/// entry under `CRATONVM_JIT_NATIVE_LEAN_DOOR=1` (JIT round 17 wave 2, lane
/// funnel; `docs/feature-designs/real-jdk-mode.md`).
///
/// [`safe_native_call_prevalidated_objects`] minus the two steps that are
/// dead on that route, and nothing else:
///
/// * **the argument re-forward.** The door already forwarded every
///   descriptor-declared reference (`forward_jit_reference_args`) and then
///   heap-validated each one (`decode_dispatch_values_into_shaped` and the
///   receiver guard). Between those and this call the door runs only memo
///   probes, atomic loads and the site's receiver/redefine/capability tests,
///   none of which is a GC point, so every object argument is its own
///   forwarding target already.
/// * **the native-array GC hook.** It is gated on
///   `env_cache::disable_jit()`, which is false whenever compiled code runs.
///
/// Every other step is the ordinary funnel's, in the same order: the pins
/// (the native may still call Java), the STW probe, the young-pressure and
/// OOME-debt relief, the thread-state span, the unwind permission, the JNI
/// drain, the return-value heal and the handoff root. The caller states the
/// precondition by taking this route only for an entry it did not fill in
/// this call; it must not use it for a sync, signature-polymorphic or leaf
/// entry (those have their own wrappers).
pub(crate) fn safe_native_call_jit_lean(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: NativeCallback,
    args: &[Value],
) -> MethodCallResult {
    // Same tally as the other two wrappers, so a run that counts native
    // entries sees this route too.
    if crate::runtime::interpreter::dbg_native_entry() {
        crate::runtime::interpreter::native_entry_note(file!(), line!());
    }
    safe_native_call_impl(shared, thread, callback, args, true, true, false)
}

/// The JIT native doors' funnel for a native whose object result is a FRESH
/// live base (JIT round 19 wave 3, lane funnel3; FUNNEL2-19-3,
/// `CRATONVM_JIT_NATIVE_FRESH_RESULT`): [`safe_native_call_prevalidated_objects`]
/// (`lean == false`) or [`safe_native_call_jit_lean`] (`lean == true`, same
/// precondition as that wrapper), except that the returned object is healed
/// with `load_and_forward_validated` -- the forwarding read without the entry
/// heap walk.
///
/// The caller states, through
/// `cratonvm_native_builtins::is_fresh_object_result_native`, that the
/// callback's object result is an allocation (or a validated read) made after
/// its last GC point, so it IS a live base when the callback returns; the
/// funnel runs no GC point between that return and the heal. The heal still
/// follows a forwarding pointer, so a relocation that moved it anyway is
/// repaired exactly as before. Every other step is the wrapped funnel's.
pub(crate) fn safe_native_call_jit_fresh_result(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: NativeCallback,
    args: &[Value],
    lean: bool,
) -> MethodCallResult {
    if crate::runtime::interpreter::dbg_native_entry() {
        crate::runtime::interpreter::native_entry_note(file!(), line!());
    }
    // A handshake pause that EXCUSES a running native
    // (`CRATONVM_HANDSHAKE_NATIVE_EXCUSE`) can relocate while the callback
    // runs; the claim is then not "no GC point since the allocation", so the
    // ordinary walk stays.
    let fresh = !shared.mem.gc_barrier.native_excuse_enabled();
    safe_native_call_impl(shared, thread, callback, args, true, lean, fresh)
}

/// The call a native's noted [`cratonvm_native_api::ServedRaiseChain`]
/// belongs to (JIT round j27 wave 4, lane MHROWS2): the thread's frame depth
/// and the standing frame's `last_instr_pc` when the native noted it -- the
/// frame the funnel's mark is set at. A native pushes no frame of its own and
/// its Java callbacks pop theirs, so a chain noted for the raise the funnel
/// then marks stands at that same call. A chain left behind (its native
/// returned normally, or its error left by another way than the funnel)
/// stands at an earlier call, and a later raise of the same class elsewhere
/// drops it instead of listing its frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ServedRaiseCallSite {
    pub(in crate::vm::vm_exec) depth: usize,
    pub(in crate::vm::vm_exec) pc: Option<usize>,
}

impl ServedRaiseCallSite {
    pub(crate) fn of(frames: &crate::runtime::frame::FrameStack) -> Self {
        Self {
            depth: frames.len(),
            pc: frames.last().map(|frame| frame.last_instr_pc),
        }
    }
}

/// A chain as a native noted it: the chain and the call it was noted at
/// (`JvmThread::served_raise_chain`).
#[derive(Debug)]
pub(crate) struct NotedServedRaiseChain {
    pub(crate) chain: Box<cratonvm_native_api::ServedRaiseChain>,
    pub(crate) site: ServedRaiseCallSite,
}



/// The chain a raise of `throwable_class` standing at `raising_at` takes from
/// what was `noted`: a chain of that class, noted at that call (any call when
/// `site_checked` is off). Every other noted chain is dropped. Pure.
pub(crate) fn served_chain_for_raise(
    noted: Option<Box<NotedServedRaiseChain>>,
    throwable_class: &str,
    raising_at: ServedRaiseCallSite,
    site_checked: bool,
) -> Option<Box<cratonvm_native_api::ServedRaiseChain>> {
    let noted = noted?;
    let own_class = noted.chain.throwable_class == throwable_class;
    let own_call = !site_checked || noted.site == raising_at;
    (own_class && own_call).then_some(noted.chain)
}


/// Interpreter round i2 wave 53, lane L3
/// (`i52-L3-proposal-a-bridge-that-raises-names-the-jdk-method-it-stands-for`):
/// a native returned an unbuilt `RuntimeError`, which its caller is about to
/// turn into a throwable. Mark the raise on the thread -- the innermost
/// interpreted frame's depth and `last_instr_pc`, which the capture checks
/// still stand before it believes the mark (`JvmThread::served_native_raise`,
/// `stackwalker::served_raise_frames`). Under
/// `VmConfig::served_raise_frames` only; out of line, on the error path.
///
/// An implicit NPE's message (`Cannot invoke "Object.getClass()" because ...`)
/// keeps its action half on the mark: the capture finds that invoke
/// (`stackwalker::implicit_npe_invoke_sites`) instead of a construction.
///
/// Interpreter round i2 wave 54, lane L3: `receiver` is the native's first
/// argument as it stands now (the funnel's pin of it, or a leaf's forwarded
/// argument); under `VmConfig::served_raise_receiver` the mark keeps its
/// class, for a virtual or interface call's selection at the capture
/// (`ServedNativeRaise::receiver_class`). A chain the native noted
/// (`JvmThread::served_raise_chain`) is taken here, and kept only for a raise
/// of its throwable class at the call it was noted at
/// ([`served_chain_for_raise`]).
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn note_served_native_raise(
    shared: &SharedVm,
    thread: &mut JvmThread,
    error: &RuntimeError,
    receiver: Option<ObjectRef>,
) {
    let noted = thread.served_raise_chain.take();
    if !shared.config.served_raise_frames {
        return;
    }
    // Not a Java exception (`NotImplemented`): no throwable will be built.
    let Some((throwable_class, message)) = error.as_java_throwable() else {
        return;
    };
    let chain = served_chain_for_raise(
        noted,
        throwable_class,
        ServedRaiseCallSite::of(&thread.frames),
        true,
    );
    // An array's header carries its component's class id: no receiver class
    // for an array (`Object` methods only, never a served raise's shape).
    let receiver_class = receiver
        .filter(|_| true)
        .filter(|&object| shared.mem.heap.kind_of(object) == ObjectKind::Object)
        .map(|object| shared.mem.heap.class_id_of(object));
    let message: Option<Box<str>> = message.map(|m| Box::from(&*m));
    let npe_action = match error {
        RuntimeError::NullPointerException {
            message: Some(message),
        } => crate::runtime::stackwalker::ServedNativeRaise::npe_action_of(message),
        _ => None,
    };
    let depth = thread.frames.len();
    // Interpreter round i2 wave 60, lane L3: the same raise re-marked by an
    // outer funnel (the `MethodHandle.invokeExact` bridge returning its
    // target's error) keeps the native an inner door entered
    // (`note_door_native_callee`).
    let native_callee = thread.served_native_raise.as_deref().and_then(|old| {
        (old.depth == depth
            && old.throwable_class == throwable_class
            && thread
                .frames
                .last()
                .is_some_and(|f| f.last_instr_pc == old.pc))
        .then_some(old.native_callee)
        .flatten()
    });
    // Wave 61, lane L3: under the keyed families the inner mark stands whole
    // -- its message, implicit-NPE action and chain are the served
    // method's, not the outer bridge's (`stackwalker::keyed_served_raise_frames`).
    if native_callee.is_some() {
        return;
    }
    // Interpreter round i5 wave 104, lane L3: a raise marked inside a served
    // initialization's `<clinit>` keeps that chain's mark underneath it
    // (`ServedNativeRaise::outer`), put back when this mark is taken.
    let outer = thread
        .served_native_raise
        .take()
        .and_then(|old| standing_init_chain_mark(&thread.frames, old));
    let mark = thread
        .frames
        .last()
        .map(|frame| crate::runtime::stackwalker::ServedNativeRaise {
            throwable_class,
            depth,
            pc: frame.last_instr_pc,
            class_id: frame.class_id,
            method_name: Box::from(frame.method_name()),
            npe_action,
            message,
            receiver_class,
            chain,
            native_callee,
            outer,
        });
    if crate::runtime::env_cache::frame_trace() {
        if let (Some(mark), Some(frame)) = (mark.as_ref(), thread.frames.last()) {
            eprintln!(
                "[SERVED_RAISE_MARK] set {} at {}.{} depth={} pc={} chain_hops={}",
                mark.throwable_class,
                frame.class_name(),
                frame.method_name(),
                mark.depth,
                mark.pc,
                mark.chain.as_ref().map_or(0, |chain| chain.hops.len())
            );
        }
    }
    thread.served_native_raise = mark.map(Box::new);
}

/// Where a by-name door ([`NativeInvokeAccess`]'s `invoke_*`) selects its
/// method from, for [`note_door_native_callee`].
#[derive(Clone, Copy)]
pub(in crate::vm::vm_exec) enum DoorOwner<'n> {
    /// A resolved class: a static or special door, or a virtual door's
    /// receiver class (read before the call).
    Id(ClassId),
    /// A class name, resolved as the innermost interpreted frame's loader
    /// would resolve it.
    Name(&'n str),
}

/// The thread's served-raise mark as it stands, by address: a door compares
/// it after its call to know whether the funnel marked a raise INSIDE the
/// call ([`note_door_native_callee`]). One load.
#[inline(always)]
pub(in crate::vm::vm_exec) fn served_raise_mark_addr(thread: &JvmThread) -> usize {
    thread
        .served_native_raise
        .as_deref()
        .map_or(0, |mark| std::ptr::from_ref(mark) as usize)
}

/// Interpreter round i2 wave 60, lane L3
/// (`docs/feature-designs/real-jdk-mode.md`):
/// a by-name door returned an unbuilt error. When the funnel marked it inside
/// this call (the mark is not the one `before` names), at the door's own
/// depth (no Java frame between the door and the native), and no inner door
/// noted a native already, note the `native` method the door selected on the
/// mark (`ServedNativeRaise::native_callee`): the capture lists its
/// `(Native Method)` frame, which no bytecode call site names. Out of line, on
/// the error path.
///
/// Under `VmConfig::served_raise_keyed` (interpreter round i2 wave 61, lane
/// L3) the door notes the method it selected whatever its kind: a JDK Java
/// method a native served, whose frames the capture lists by method
/// (`stackwalker::keyed_served_raise_frames`).
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn note_door_native_callee(
    shared: &SharedVm,
    thread: &mut JvmThread,
    before: usize,
    owner: DoorOwner<'_>,
    method_name: &str,
    descriptor: &str,
) {
    let depth = thread.frames.len();
    let requester = thread.frames.last().map(|frame| frame.class_id);
    let fresh = |mark: &crate::runtime::stackwalker::ServedNativeRaise| {
        std::ptr::from_ref(mark) as usize != before
            && mark.depth == depth
            && mark.native_callee.is_none()
    };
    if !thread.served_native_raise.as_deref().is_some_and(fresh) {
        return;
    }
    let callee = {
        let cm = shared.classes.class_manager.read();
        let start = match owner {
            DoorOwner::Id(id) => Some(id),
            DoorOwner::Name(name) => {
                requester.and_then(|requester| cm.find_class_by_name_for_class(name, requester))
            }
        };
        start.and_then(|start| {
            crate::runtime::stackwalker::door_native_callee(
                &cm.class_store,
                start,
                method_name,
                descriptor,
                true,
            )
        })
    };
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[SERVED_RAISE_MARK] door {method_name}{descriptor} native callee {callee:?}");
    }
    if let Some(mark) = thread.served_native_raise.as_deref_mut() {
        mark.native_callee = callee;
    }
}

/// [`note_door_native_callee`] when `result` is an unbuilt error: the doors'
/// one added branch on their return.
#[inline(always)]
pub(in crate::vm::vm_exec) fn door_result_noted(
    shared: &SharedVm,
    thread: &mut JvmThread,
    before: usize,
    owner: DoorOwner<'_>,
    method_name: &str,
    descriptor: &str,
    result: MethodCallResult,
) -> MethodCallResult {
    if let Err(MethodCallFailed::InternalError(VmError::Runtime(_))) = &result {
        note_door_native_callee(shared, thread, before, owner, method_name, descriptor);
    }
    result
}

/// Interpreter round i2 wave 58, lane L3
/// (`NativeClassAccess::initialize_class_served`): a native serving a JDK
/// method initializes `class_id`, which HotSpot initializes `chain`'s hops
/// below that method. The initialization's `ExceptionInInitializerError` (or,
/// for a class that already failed, `NoClassDefFoundError`) is built while the
/// native runs, so the funnel never marks it: mark it here, before, as
/// `note_served_native_raise` would, and withdraw the mark after. A capture of
/// another class in between (the `<clinit>`'s own throwable) leaves it; the
/// capture of the failure takes it while the native's caller still stands at
/// the call (`stackwalker::served_raise_frames`, the chain's frames). Under
/// `VmConfig::served_raise_frames` and `served_raise_chain`; the chain's
/// classes are loaded first, as `note_served_raise_chain` loads them.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn initialize_class_served_cold(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    mut chain: cratonvm_native_api::ServedRaiseChain,
) -> Result<(), MethodCallFailed> {
    let marked = shared.config.served_raise_frames
        && shared.config.served_raise_chain
        && chain
            .hops
            .iter()
            .all(|hop| shared.load_class_transformed(thread, hop.class).is_ok());
    let mut restore: Option<Box<crate::runtime::stackwalker::ServedNativeRaise>> = None;
    if marked {
        let failed_before = shared
            .classes
            .class_manager
            .read()
            .get_class(class_id)
            .is_some_and(|c| c.state == crate::classloading::ClassState::InitializationError);
        chain.throwable_class = if failed_before {
            "java/lang/NoClassDefFoundError"
        } else {
            "java/lang/ExceptionInInitializerError"
        };
        // Interpreter round i5 wave 104, lane L3
        // (`i103-L3-clinit-trigger-throwable-traces-lack-served-door-frames`
        // residue (a)): a served initialization nested inside another's
        // `<clinit>` keeps the outer chain's mark underneath its own
        // (`ServedNativeRaise::outer`) and puts it back after, so later
        // captures and walks in the outer `<clinit>` still list the outer
        // chain (they listed nothing once the nested mark was withdrawn).
        restore = thread
            .served_native_raise
            .take()
            .and_then(|prev| standing_init_chain_mark(&thread.frames, prev));
        let depth = thread.frames.len();
        let mark =
            thread
                .frames
                .last()
                .map(|frame| crate::runtime::stackwalker::ServedNativeRaise {
                    throwable_class: chain.throwable_class,
                    depth,
                    pc: frame.last_instr_pc,
                    class_id: frame.class_id,
                    method_name: Box::from(frame.method_name()),
                    npe_action: None,
                    message: None,
                    receiver_class: None,
                    chain: Some(Box::new(chain)),
                    native_callee: None,
                    outer: restore.clone(),
                });
        thread.served_native_raise = mark.map(Box::new);
    }
    let result = super::ensure_class_initialized_shared(shared, thread, class_id);
    if marked {
        thread.served_native_raise = restore;
    }
    result
}

/// Interpreter round i5 wave 104, lane L3: the served class-initialization
/// chain mark of `mark`'s stack that still stands on `frames` -- `mark`
/// itself when it is one ([`served_init_chain_clinit`]), else the nearest of
/// its [`outer`](crate::runtime::stackwalker::ServedNativeRaise::outer)
/// marks that is -- or `None`. What a mark set over `mark` keeps underneath
/// it; a stale raise mark or a chain whose `<clinit>` returned is dropped,
/// as the single mark was. Cold: asked only while a mark stands.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn standing_init_chain_mark(
    frames: &crate::runtime::frame::FrameStack,
    mut mark: Box<crate::runtime::stackwalker::ServedNativeRaise>,
) -> Option<Box<crate::runtime::stackwalker::ServedNativeRaise>> {
    // Bounded: a mark stack is as deep as the served initializations nested
    // on this thread's stack.
    for _ in 0..1024 {
        if served_init_chain_clinit(frames, &mark).is_some() {
            return Some(mark);
        }
        mark = mark.outer.take()?;
    }
    None
}

/// Take the thread's mark for the capture that believes it, putting back the
/// standing class-init chain mark it displaced (`ServedNativeRaise::outer`;
/// interpreter round i5 wave 104, lane L3).
pub(in crate::vm::vm_exec) fn take_served_raise_mark(
    thread: &mut JvmThread,
) -> Option<Box<crate::runtime::stackwalker::ServedNativeRaise>> {
    let mut mark = thread.served_native_raise.take()?;
    if let Some(outer) = mark.outer.take() {
        thread.served_native_raise = standing_init_chain_mark(&thread.frames, outer);
    }
    Some(mark)
}

/// Is every interpreter frame of `frames[from..]` part of the construction of
/// a `throwable_class` -- an `<init>` of that class or one of its
/// superclasses, or `fillInStackTrace` -- the frames HotSpot's
/// `fill_in_stack_trace` skips? `false` when `from` is past the top.
pub(in crate::vm::vm_exec) fn only_construction_above(
    shared: &SharedVm,
    frames: &[crate::runtime::frame::Frame],
    from: usize,
    throwable_class: ClassId,
) -> bool {
    let Some(above) = frames.get(from..) else {
        return false;
    };
    if above.is_empty() {
        return true;
    }
    let cm = shared.classes.class_manager.read_recursive();
    let is_a = |class_id: ClassId| {
        let mut id = Some(throwable_class);
        for _ in 0..64 {
            let Some(current) = id else {
                return false;
            };
            if current == class_id {
                return true;
            }
            id = cm
                .class_store
                .get(current)
                .and_then(|class| class.superclass);
        }
        false
    };
    above.iter().all(|frame| match frame.method_name() {
        "fillInStackTrace" => true,
        "<init>" => is_a(frame.class_id),
        _ => false,
    })
}

/// Interpreter round i2 wave 62, lane L3 (`VmConfig::served_raise_own_construction`;
/// `docs/feature-designs/real-jdk-mode.md`):
/// should the capture of a `throwable_class` -- the mark's class -- leave the
/// funnel's mark standing? Yes when the marked frame still stands at the
/// marked call but something other than this throwable's construction runs
/// above it: Java the native ran (a `<clinit>` that catches another class's
/// failed initialization), whose capture is not the raise the mark was set
/// for. Out of line: a marked capture only.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn served_raise_mark_left_for_its_own_raise(
    shared: &SharedVm,
    thread: &JvmThread,
    throwable_class: ClassId,
) -> bool {
    let Some(mark) = thread.served_native_raise.as_deref() else {
        return false;
    };
    let stands = mark
        .depth
        .checked_sub(1)
        .and_then(|index| thread.frames.get(index))
        .is_some_and(|frame| frame.last_instr_pc == mark.pc);
    let left =
        stands && !only_construction_above(shared, &thread.frames, mark.depth, throwable_class);
    if left && crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[SERVED_RAISE_MARK] left standing: {} frame(s) above the mark are not its construction",
            thread.frames.len().saturating_sub(mark.depth)
        );
    }
    left
}

/// The screen of the keyed families' door carrier, the door record
/// ([`served_door_key`]): a capture of a `throwable_class` keys on a door
/// opened at `(interp_depth, jit_depth)` when nothing but that throwable's
/// construction stands above the door's interpreter depth and no compiled
/// activation was entered since -- the throwable was built while the served
/// native ran, by the native or a callee that pushed no frame of its own.
/// Interpreter round i2 wave 63, lane L3 (split out of wave 62's
/// `reflective_row_key`, the same test; wave 64 retired the row carrier, so
/// the served `Method.invoke`'s door is screened here too).
pub(in crate::vm::vm_exec) fn door_key_believed(
    shared: &SharedVm,
    thread: &JvmThread,
    interp_depth: u32,
    jit_depth: u32,
    throwable_class: ClassId,
) -> bool {
    let Ok(base) = usize::try_from(interp_depth) else {
        return false;
    };
    let now = u32::try_from(crate::jit::conservative_roots::current_thread_jit_depth())
        .unwrap_or(u32::MAX);
    now <= jit_depth && only_construction_above(shared, &thread.frames, base, throwable_class)
}

/// Interpreter round i2 wave 63, lane L3 (`VmConfig::served_raise_door_key`):
/// the method the innermost static JDK door is entering
/// (`JvmThread::served_door_in_flight`), under [`door_key_believed`]. Out of
/// line: a capture inside a noted door only.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn served_door_key(
    shared: &SharedVm,
    thread: &JvmThread,
    door: &crate::runtime::stackwalker::DoorInFlight,
    throwable_class: ClassId,
) -> Option<(ClassId, u32)> {
    let key = door.key?;
    let keyed = door_key_believed(
        shared,
        thread,
        door.interp_depth,
        door.jit_depth,
        throwable_class,
    );
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[SERVED_RAISE_DOOR_KEY] door target {key:?} depth={} keyed={keyed}",
            door.interp_depth
        );
    }
    keyed.then_some(key)
}

/// What a static door's [`door_in_flight_enter`] replaced, for
/// [`door_in_flight_leave`]: `None` when the door noted nothing (the switch
/// off, an application class), else the outer door's record.
pub(in crate::vm::vm_exec) type DoorInFlightSave = Option<Option<crate::runtime::stackwalker::DoorInFlight>>;

/// Interpreter round i2 wave 63, lane L3 (`VmConfig::served_raise_door_key`;
/// proposal
/// `docs/feature-designs/real-jdk-mode.md`):
/// a by-name static door on a `java/` / `jdk/` class notes the method it is
/// about to enter on the thread (`JvmThread::served_door_in_flight`), so a
/// throwable a callee builds inside the served native -- with no funnel mark
/// and no call site naming the method (a method handle's `invokeExact`) --
/// lists the method's JDK frames. Switch off or an application class: one
/// branch, nothing stored.
#[inline(always)]
pub(in crate::vm::vm_exec) fn door_in_flight_enter(
    shared: &SharedVm,
    thread: &mut JvmThread,
    owner: DoorOwner<'_>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> DoorInFlightSave {
    if !crate::runtime::stackwalker::door_in_flight_screens(class_name)
    {
        return None;
    }
    Some(door_in_flight_note(
        shared,
        thread,
        owner,
        method_name,
        descriptor,
    ))
}

/// [`door_in_flight_enter`]'s note: the method the door selects, resolved
/// now from `owner` (a name as the innermost interpreted frame's loader
/// resolves it; an owner not loaded yet keys nothing), any kind
/// (`stackwalker::door_native_callee`). Returns the record it replaced.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn door_in_flight_note(
    shared: &SharedVm,
    thread: &mut JvmThread,
    owner: DoorOwner<'_>,
    method_name: &str,
    descriptor: &str,
) -> Option<crate::runtime::stackwalker::DoorInFlight> {
    use crate::runtime::stackwalker::{DoorKeyMemo, DoorKeyOwnerRef};
    let requester = thread.frames.last().map(|frame| frame.class_id);
    // Interpreter round i2 wave 64, lane L3 (`VmConfig::served_raise_door_memo`):
    // a door entered again takes its selection from the thread's memo
    // (`stackwalker::DoorKeyMemo`): no class-manager read, no method scan.
    let memo_owner = match owner {
        DoorOwner::Id(id) => DoorKeyOwnerRef::Id(id),
        DoorOwner::Name(name) => DoorKeyOwnerRef::Name(name, requester),
    };
    let redefinitions = if shared.config.served_raise_door_memo {
        Some(cratonvm_classloading::class_redefinition_count())
    } else {
        None
    };
    let memoized = redefinitions.and_then(|count| {
        thread
            .served_door_key_memo
            .as_deref_mut()
            .and_then(|memo| memo.lookup(count, memo_owner, method_name, descriptor))
    });
    let key = if let Some(key) = memoized {
        key
    } else {
        let (start, key) = {
            let cm = shared.classes.class_manager.read_recursive();
            let start = match owner {
                DoorOwner::Id(id) => Some(id),
                DoorOwner::Name(name) => {
                    requester.and_then(|requester| cm.find_class_by_name_for_class(name, requester))
                }
            };
            let key = start.and_then(|start| {
                crate::runtime::stackwalker::door_native_callee(
                    &cm.class_store,
                    start,
                    method_name,
                    descriptor,
                    true,
                )
            });
            (start, key)
        };
        // Only a selection whose owner resolved: an owner not loaded yet may
        // load before the next door.
        if let (Some(count), Some(_)) = (redefinitions, start) {
            thread
                .served_door_key_memo
                .get_or_insert_with(|| Box::new(DoorKeyMemo::new(count)))
                .insert(count, memo_owner, method_name, descriptor, key);
        }
        key
    };
    let record = crate::runtime::stackwalker::DoorInFlight {
        key,
        interp_depth: u32::try_from(thread.frames.len()).unwrap_or(u32::MAX),
        jit_depth: u32::try_from(crate::jit::conservative_roots::current_thread_jit_depth())
            .unwrap_or(u32::MAX),
    };
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[SERVED_RAISE_DOOR_KEY] enter {method_name}{descriptor} key={key:?} depth={} memo={}",
            record.interp_depth,
            match (redefinitions, memoized) {
                (None, _) => "off",
                (Some(_), None) => "miss",
                (Some(_), Some(_)) => "hit",
            }
        );
    }
    thread.served_door_in_flight.replace(record)
}

/// The door record a static door writes for `key`, at the thread's depths
/// now; returns the record it replaced. [`door_in_flight_note`]'s store, for
/// a door that already knows its key ([`settling_door_enter`]).
#[inline]
pub(in crate::vm::vm_exec) fn door_in_flight_store(
    thread: &mut JvmThread,
    key: Option<(ClassId, u32)>,
) -> Option<crate::runtime::stackwalker::DoorInFlight> {
    let record = crate::runtime::stackwalker::DoorInFlight {
        key,
        interp_depth: u32::try_from(thread.frames.len()).unwrap_or(u32::MAX),
        jit_depth: u32::try_from(crate::jit::conservative_roots::current_thread_jit_depth())
            .unwrap_or(u32::MAX),
    };
    thread.served_door_in_flight.replace(record)
}

/// Interpreter round i2 wave 65, lane L3
/// (`VmConfig::served_raise_settled_door_key`, proposal
/// `docs/feature-designs/real-jdk-mode.md`):
/// a settling handle's static door ([`door_in_flight_enter`] for
/// `NativeInvokeAccess::invoke_static_settling_keyed`). `door_owner` is the
/// owner the note resolves from (the handle's settled owner, else its
/// resolved one; `None`: a by-name door, never kept). With `door_key` (the
/// switch on) a slot filled for the same owner at the same
/// class-redefinition count writes the record directly; any other call
/// notes as [`door_in_flight_enter`] does and fills the slot from the record
/// it wrote. The slot is filled only once the screen passed, and a handle
/// record's class name never changes, so a hit needs no screen.
#[inline]
pub(in crate::vm::vm_exec) fn settling_door_enter(
    shared: &SharedVm,
    thread: &mut JvmThread,
    door_owner: Option<ClassId>,
    door_key: Option<&cratonvm_native_api::SettledDoorKey>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> DoorInFlightSave {
    let owner_ref = door_owner.map_or(DoorOwner::Name(class_name), DoorOwner::Id);
    let (Some(slot), Some(owner)) = (door_key, door_owner) else {
        return door_in_flight_enter(
            shared,
            thread,
            owner_ref,
            class_name,
            method_name,
            descriptor,
        );
    };
    // Read before any selection: a redefinition racing the scan leaves the
    // slot stale-marked, never fresh.
    let redefinitions = cratonvm_classloading::class_redefinition_count();
    if let Some(key) = slot.lookup(owner, redefinitions) {
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[SERVED_RAISE_DOOR_KEY] enter {method_name}{descriptor} key={key:?} depth={} settled-slot=hit",
                thread.frames.len()
            );
        }
        return Some(door_in_flight_store(thread, key));
    }
    let save = door_in_flight_enter(
        shared,
        thread,
        owner_ref,
        class_name,
        method_name,
        descriptor,
    );
    if save.is_some() {
        if let Some(record) = thread.served_door_in_flight {
            slot.set(Some(cratonvm_native_api::SettledDoorKeyEntry {
                owner,
                redefinitions,
                key: record.key,
            }));
        }
    }
    save
}

/// Interpreter round i2 wave 65, lane L3: the body of
/// `NativeInvokeAccess::invoke_static_settling` and its keyed variant
/// (`door_key`: the handle record's slot when
/// `VmConfig::served_raise_settled_door_key` is on, else `None`).
impl NativeContextImpl<'_> {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::vm::vm_exec) fn invoke_static_settling_door(
        &mut self,
        owner: Option<ClassId>,
        settled: Option<ClassId>,
        door_key: Option<&cratonvm_native_api::SettledDoorKey>,
        first_class: Option<ClassId>,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> (MethodCallResult, Option<ClassId>) {
        let mut fwd_buf: Vec<Value> = Vec::new();
        let args = forward_boundary_args(&self.shared.mem.heap, args, &mut fwd_buf);
        let before = served_raise_mark_addr(self.thread);
        // Interpreter round i2 wave 63, lane L3 (`VmConfig::served_raise_door_key`):
        // a `REF_invokeStatic` handle's door, whose caller's `invokeExact`
        // names no method. Wave 65 (`served_raise_settled_door_key`): the
        // handle record's slot, when it passed one.
        let in_flight = settling_door_enter(
            self.shared,
            self.thread,
            settled.or(owner),
            door_key,
            class_name,
            method_name,
            descriptor,
        );
        let (result, settled) = invoke_static_settling_with_first_class(
            self.shared,
            self.thread,
            owner,
            settled,
            class_name,
            method_name,
            descriptor,
            args,
            first_class.filter(|_| true),
        );
        door_in_flight_leave(self.thread, in_flight);
        let door = settled
            .or(owner)
            .map_or(DoorOwner::Name(class_name), DoorOwner::Id);
        let result = door_result_noted(
            self.shared,
            self.thread,
            before,
            door,
            method_name,
            descriptor,
            result,
        );
        (result, settled)
    }
}

/// The door's return: the outer door's record back (a door inside a door
/// restores, never clears).
#[inline(always)]
pub(in crate::vm::vm_exec) fn door_in_flight_leave(thread: &mut JvmThread, save: DoorInFlightSave) {
    if let Some(outer) = save {
        thread.served_door_in_flight = outer;
    }
}

/// The door record outside the outermost of `rows` (reflective calls being
/// left) whose door is still noted (`ReflectiveCallRow::door_save`): what
/// leaving them gives back. Interpreter round i2 wave 64, lane L3.
pub(in crate::vm::vm_exec) fn outermost_door_save(
    rows: &[crate::runtime::stackwalker::ReflectiveCallRow],
) -> DoorInFlightSave {
    rows.iter().find_map(|call| call.door_save)
}


/// The funnel with everything a **leaf** native cannot need taken out.
///
/// `callback` must have been registered through
/// [`NativeMethodRegistry::set_leaf`](cratonvm_native_api::NativeMethodRegistry::set_leaf),
/// whose doc comment states the four-part contract: no Java-heap allocation,
/// no safepoint or block, no collection, no JNI-pending exception. Callers
/// establish that by holding a `NativeMethodId` whose slot answers
/// `is_leaf_id`; they do not get to decide it themselves.
///
/// # What this drops, and why each is dead for a leaf
///
/// | funnel step | why a leaf does not need it |
/// |---|---|
/// | argument pinning into `native_pin_roots` | pins exist so a collection *during* the callback can remap the arguments. A leaf cannot reach a collection, so nothing can move under it. |
/// | the STW probe + `safepoint_check` | a leaf is a handful of instructions and returns to a mutator that polls safepoints itself; entering one here buys the collector nothing. |
/// | `native_array_gc_requested` / `young_spill_pressure` relief | both exist for natives that ALLOCATE. A leaf does not. |
/// | the remap-args-after-GC rebuild | unreachable once the two GC hooks above are gone. |
/// | `NativeRunning` / restore `record_transition` pair | the state exists so the STW census WAITS for a native holding raw `ObjectRef`s in Rust locals. A leaf holds them for a few instructions and cannot block, so the thread stays exactly what it was — running Java. |
/// | the JNI pending-exception drain | contract item 4. |
/// | the pin-watermark truncate + unpin ring | nothing was pinned. |
/// | `native_diag_pre_call` / `post_call` | these are already behind a mask that is zero on every production run; the leaf path simply does not offer them. A run that needs them can unset the leaf claim. |
///
/// # What it keeps
///
/// * **The argument forwarding barrier.** Object arguments have just left the
///   GC-visible operand stack, and a collection may have run between the frame
///   read and this call — that is the same window the funnel's own
///   `load_and_forward` covers, and it is two instructions.
/// * **`catch_unwind`.** A leaf must not panic, but "must not" is a contract,
///   not a proof, and a panic escaping into JIT-compiled or interpreter frames
///   is a process-level failure rather than a bad answer. It costs nothing
///   when nothing unwinds.
/// * **The return-value forwarding + `native_pending_return` handoff root**,
///   so an object-returning leaf hands its result over by the same rule every
///   other native does: rooted until its caller consumes it (the interpreter's
///   push, or the JIT helper's hand-off to compiled code, handoff-w36b).
///
/// Measured effect: `probes/NativeShapeProbe.java`, and
/// `native-call-funnel-is-the-per-call-floor-RETIRED-20260805.md`.
pub(crate) fn safe_native_call_leaf(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: NativeCallback,
    args: &[Value],
) -> MethodCallResult {
    safe_native_call_leaf_impl(shared, thread, callback, args, false)
}

/// [`safe_native_call_leaf`] for a caller that has already validated every
/// non-null `Value::Object` argument against this VM's heap with no safepoint
/// since (the lean JIT native door, `jit::helpers::try_lean_native_door`):
/// the forwarding barrier then skips the second heap-membership walk, as
/// [`safe_native_call_prevalidated_objects`] does for the full funnel.
pub(crate) fn safe_native_call_leaf_prevalidated(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: NativeCallback,
    args: &[Value],
) -> MethodCallResult {
    safe_native_call_leaf_impl(shared, thread, callback, args, true)
}

/// Where this thread is in a leaf native call, for
/// `NativeContext::decline_leaf_call` (the i53 residual): `LEAF_CALL_RUNNING`
/// while [`safe_native_call_leaf_impl`] runs a leaf's callback,
/// `LEAF_CALL_DECLINED` once the callback declined, `LEAF_CALL_NONE` outside.
/// A leaf calls no other native, so one cell per thread is enough.
pub(in crate::vm::vm_exec) const LEAF_CALL_NONE: u8 = 0;
pub(in crate::vm::vm_exec) const LEAF_CALL_RUNNING: u8 = 1;
pub(in crate::vm::vm_exec) const LEAF_CALL_DECLINED: u8 = 2;
thread_local! {
    pub(in crate::vm::vm_exec) static LEAF_CALL: std::cell::Cell<u8> = const { std::cell::Cell::new(LEAF_CALL_NONE) };
}

#[inline]
pub(in crate::vm::vm_exec) fn safe_native_call_leaf_impl(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: NativeCallback,
    args: &[Value],
    prevalidated_objects: bool,
) -> MethodCallResult {
    // Interpreter round i1 wave 26, lane L1: while a JDWP method event
    // request is in force a leaf takes the full funnel, which reports a
    // native method's entry and exit (`System.nanoTime`,
    // `System.currentTimeMillis`) and may park the thread for them. One
    // relaxed load, in a build with a JDWP server only; a leaf satisfies
    // every precondition of the full funnel.
    // (A JDWP session raises it on attach: `METHOD_EVENTS_BLOCKING`.)
    //
    // The i53 retirement: also while the funnel records the native a thread
    // runs for a C JVMTI env (`DebuggerGates::running_native_armed`, which a
    // C env counting running natives raises without a JDWP session), so its
    // `GetThreadState` of a thread inside a leaf answers in native as for any
    // other native -- a leaf is short, but not every leaf is instant (a
    // `RandomAccessFile` read or write is a system call).
    //
    // Interpreter round i3 wave 83, lane L1: `native_hooks_armed` (the same
    // word) also while the funnel posts a C env's `NativeMethodBind` of a
    // registered native's first run (`System.nanoTime` is a leaf).
    //
    // Interpreter round i6 wave 133, lane L1: `leaf_takes_full_funnel` is
    // `running_native_armed` plus a C env counting running natives from the
    // excuse word alone (`CRATONVM_JVMTI_C_FUNNEL_RECORD_FROM_EXCUSE`), which
    // only the full funnel raises. The same one load.
    #[cfg(feature = "experimental-debug")]
    if shared.debug.debugger_gates.native_hooks_armed()
        || shared.debug.debugger_gates.leaf_takes_full_funnel()
    {
        return safe_native_call_impl(shared, thread, callback, args, false, false, false);
    }
    // Copy nothing unless the barrier has something to rewrite — see the same
    // shape, and the measurement behind it, in `safe_native_call_impl`.
    const INLINE_NATIVE_ARGS: usize = crate::jit::helpers::INLINE_JIT_NATIVE_ARGS;
    // Declared, not initialised — see the same shape, and the measurement, in
    // `safe_native_call_impl`.
    let mut inline_forwarded: [Value; INLINE_NATIVE_ARGS];
    let mut heap_forwarded: Vec<Value>;
    let forwarded_args: &[Value] = if args.is_empty() {
        args
    } else {
        let mut moved = None;
        for (index, value) in args.iter().enumerate() {
            if let Value::Object(Some(obj)) = value {
                let forwarded = if prevalidated_objects {
                    shared.mem.heap.load_and_forward_validated(*obj)
                } else {
                    shared.mem.heap.load_and_forward(*obj)
                };
                if forwarded.as_ptr() != obj.as_ptr() {
                    moved = Some(index);
                    break;
                }
            }
        }
        match moved {
            None => args,
            Some(first_moved) => {
                let buf: &mut [Value] = if args.len() <= INLINE_NATIVE_ARGS {
                    inline_forwarded = [Value::Object(None); INLINE_NATIVE_ARGS];
                    inline_forwarded[..args.len()].copy_from_slice(args);
                    &mut inline_forwarded[..args.len()]
                } else {
                    heap_forwarded = args.to_vec();
                    &mut heap_forwarded[..]
                };
                for value in buf[first_moved..].iter_mut() {
                    if let Value::Object(Some(obj)) = value {
                        *obj = shared.mem.heap.load_and_forward(*obj);
                    }
                }
                buf
            }
        }
    };

    LEAF_CALL.with(|state| state.set(LEAF_CALL_RUNNING));
    let result = {
        let mut ctx = NativeContextImpl { shared, thread };
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            callback(&mut ctx, forwarded_args)
        }))
    };
    // The i53 residual: a leaf that found its call needs a blocking region
    // (`NativeContext::decline_leaf_call`, before any side effect) runs again
    // on the full funnel, which can give it one.
    if LEAF_CALL.with(|state| state.replace(LEAF_CALL_NONE)) == LEAF_CALL_DECLINED && result.is_ok()
    {
        return safe_native_call_impl(shared, thread, callback, args, false, false, false);
    }

    let mut out: MethodCallResult = match result {
        Ok(method_result) => method_result,
        Err(payload) => {
            // A leaf that panicked has broken its contract. Report it with the
            // same shape the funnel uses — the callee name plus the Java frame
            // that called it — rather than letting the payload fall on the
            // floor, because the whole point of the leaf list is that it is
            // auditable and a violation must be attributable.
            thread.native_pending_return = None;
            let msg = if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = payload.downcast_ref::<&str>() {
                s.to_string()
            } else {
                "unknown native method panic".to_string()
            };
            let callee = native_callee_name(callback);
            let top = thread
                .frames
                .last()
                .map(|f| {
                    format!(
                        "{}.{}{}",
                        f.class_name(),
                        f.method_name(),
                        f.method_descriptor()
                    )
                })
                .unwrap_or_default();
            tracing::error!(
                "LEAF native {} panicked: {} (invoked from {}). A leaf native must not \
                 panic — see NativeMethodRegistry::set_leaf; drop its leaf claim.",
                callee,
                msg,
                top,
            );
            return Err(MethodCallFailed::InternalError(VmError::Internal {
                message: format!("leaf native method panic: {msg}"),
            }));
        }
    };

    thread.native_pending_return = None;
    match &mut out {
        Ok(Some(v)) => {
            if let Value::Object(Some(o)) = v {
                *o = shared.mem.heap.load_and_forward(*o);
            }
            if let Some(o) = value_as_validated_object_ref(shared, *v) {
                thread.native_pending_return = Some(o);
            }
        }
        Err(MethodCallFailed::ExceptionThrown(exc)) => {
            thread.native_pending_return = Some(*exc);
        }
        // Interpreter round i2 wave 53, lane L3: as the full funnel. A leaf
        // reached no collection, so its forwarded receiver still stands.
        Err(MethodCallFailed::InternalError(VmError::Runtime(error))) => {
            let receiver = match forwarded_args.first() {
                Some(Value::Object(Some(object))) => Some(*object),
                _ => None,
            };
            note_served_native_raise(shared, thread, error, receiver);
        }
        _ => {}
    }
    out
}

/// DIAGNOSTIC-ONLY (cceres3): pin-stack underflow detector.
///
/// A native that returns with FEWER pins than it entered with truncated its
/// CALLER's pins (`unpin_native_roots` is a truncate) — every handle the caller
/// still holds now dangles and `read_native_pin` silently degrades to the raw,
/// possibly-stale fallback. The guard fires on every exit path (including
/// unwind) via `Drop` and names the culprit at the funnel.
///
/// Lifted out of `safe_native_call_impl` in ARCH-2026-08-04 A3 so the funnel
/// can construct it behind one mask test rather than its own gate call.
pub(in crate::vm::vm_exec) struct PinFloorGuard {
    pub(in crate::vm::vm_exec) floor: usize,
    pub(in crate::vm::vm_exec) thread: *const JvmThread,
    pub(in crate::vm::vm_exec) callee_addr: usize,
}

/// Report budget shared by the two pin-balance reporters ([`PinFloorGuard`]
/// and, in debug builds, [`check_native_pin_balance`]): a leaking native
/// leaks on every call, and the first reports are the ones that matter.
pub(in crate::vm::vm_exec) static PIN_IMBALANCE_REPORTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

impl Drop for PinFloorGuard {
    fn drop(&mut self) {
        // SAFETY: the guard lives strictly within one `safe_native_call_impl`
        // frame; the thread outlives it (debug-only read of a Vec length).
        let len = unsafe { (*self.thread).native_pin_roots.len() };
        if len < self.floor
            && PIN_IMBALANCE_REPORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8
        {
            let callee = cratonvm_native_api::native_ring::name_of(self.callee_addr)
                .unwrap_or_else(|| format!("<cb@{:#x}>", self.callee_addr));
            eprintln!(
                "[blockgc] PIN-UNDERFLOW callee={callee} entry_pins={} exit_pins={len} — this native truncated its caller's pins",
                self.floor,
            );
        }
    }
}

/// The debug-build half of C3W3-4: an underflow is an assertion failure (a
/// test that drives the native fails); a leak on a normal return is reported
/// (shared budget of eight per process) so a run over a test suite is its
/// census. Promote `Leaked` to an assertion once that census is clean.
#[cfg(debug_assertions)]
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn report_native_pin_imbalance(
    callback: NativeCallback,
    verdict: NativePinBalance,
    leaked_classes: impl FnOnce() -> String,
) {
    match verdict {
        // `debug_assert!`, not a panic site: this whole function is
        // debug-only, and `hot_files_have_no_production_panics` counts
        // literal panic sites in this file.
        NativePinBalance::Underflow(n) => debug_assert!(
            false,
            "native pin underflow: {} truncated {n} pin(s) below its entry floor \
             (its caller's roots dangle)",
            native_callee_name(callback),
        ),
        NativePinBalance::Leaked(n) => {
            if PIN_IMBALANCE_REPORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                eprintln!(
                    "[pin-balance] LEAK callee={} left {n} pin(s) on a normal return \
                     (truncated by the funnel; C3W3-4 census): [{}]",
                    native_callee_name(callback),
                    leaked_classes(),
                );
            }
        }
        NativePinBalance::Balanced | NativePinBalance::ExceptionHandoff(_) => {}
    }
}

/// Resolve a callback's human name, falling back to its address.
///
/// Every diagnostic below wants this and none of them are on the fast path, so
/// it lives here rather than being recomputed inline six times.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn native_callee_name(callback: NativeCallback) -> String {
    cratonvm_native_api::native_ring::name_of(callback as usize)
        .unwrap_or_else(|| format!("<cb@{:#x}>", callback as usize))
}

/// Format the top `n` Java frames, one per line, for a diagnostic dump.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn native_diag_java_stack(thread: &JvmThread, n: usize, prefix: &str) -> String {
    thread
        .frames
        .iter()
        .rev()
        .take(n)
        .map(|f| {
            format!(
                "{prefix}{}.{}{} pc={}",
                f.class_name(),
                f.method_name(),
                f.method_descriptor(),
                f.pc
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Everything the native funnel does *before* the callback when a diagnostic is
/// armed. Never reached on an all-off run.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn native_diag_pre_call(
    diag: u32,
    thread: &mut JvmThread,
    callback: NativeCallback,
) -> NativeDiagState {
    let mut st = NativeDiagState::default();

    // letsgo postmortem: record native dispatch with the caller frame's
    // identity so a SEGV inside a native callback leaves a breadcrumb of
    // *who* called it. The callback itself is an opaque fn-pointer, but
    // the top Java frame is the invokevirtual/invokestatic site.
    if diag & native_diag::DISPATCH_TRACE != 0 {
        let (cls, mth, des) = match thread.frames.last() {
            Some(f) => (
                f.class_name().to_string(),
                f.method_name().to_string(),
                f.method_descriptor().to_string(),
            ),
            None => (
                "<no-frame>".to_string(),
                "<native>".to_string(),
                String::new(),
            ),
        };
        // T19.H1 — also resolve the *callee* native's name from the
        // ring-buffer name map. Recording only the caller frame hides
        // which native is hung when a single bytecode method spins in a
        // tight loop calling natives (e.g. an `Enumeration` that never
        // exhausts). The `des` slot carries the native triple.
        let callee = native_callee_name(callback);
        crate::dispatch_trace::record_native(
            thread.thread_id.0 as usize,
            &cls,
            &mth,
            &format!("{des}  ->NATIVE {callee}"),
        );
    }

    // T19.H1 — record the native into the process-global ring buffer.
    // `safe_native_call` is the central choke point for nearly every native
    // dispatch path, so recording here (rather than only at the two interpreter
    // call sites) means a hang inside *any* native leaves a `STILL-IN-NATIVE`
    // breadcrumb the watchdog can dump.
    if diag & native_diag::VM_STATE != 0 {
        let name = native_callee_name(callback);
        thread.set_vm_state(format!("native:{name}"));
        st.vm_state_name = Some(name);
    }
    if diag & native_diag::RING != 0 {
        st.ring_idx = Some(cratonvm_native_api::native_ring::record_enter(
            callback as usize,
        ));
    }

    // DBG (CRATONVM_DBG_STRAYSTACK): track the innermost native name on a
    // thread-local stack so the stray-receiver dump can name the culprit.
    if diag & native_diag::STRAYSTACK != 0 {
        let nm = native_callee_name(callback);
        CURRENT_NATIVE_STACK.with(|s| s.borrow_mut().push((callback as usize, nm)));
        st.straystack_pushed = true;
    }

    st
}

/// Everything the native funnel does *after* the callback when a diagnostic is
/// armed, up to (but not including) result handling. Never reached on an
/// all-off run.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn native_diag_post_call(
    diag: u32,
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: NativeCallback,
    st: &NativeDiagState,
) {
    if st.straystack_pushed {
        CURRENT_NATIVE_STACK.with(|s| {
            s.borrow_mut().pop();
        });
    }
    if let Some(idx) = st.ring_idx {
        cratonvm_native_api::native_ring::record_exit(idx);
    }
    if st.vm_state_name.is_some() {
        thread.set_vm_state("native:return");
    }

    // DBG (bc math-ec, CRATONVM_DBG_ECWATCH_NATIVE): the native callback is the
    // suspected raw-writer of `0x4` into an EC reference field. Re-read every
    // watched EC ref cell now that this native has returned; any that flipped to
    // a non-zero `<0x1000` value was corrupted by THIS native. Gated separately
    // (expensive: O(watch-list) per native).
    if diag & native_diag::EC_WATCH != 0 {
        let hits = crate::runtime::ec_watch::detect(shared.vm_identity);
        if !hits.is_empty() {
            let native = native_callee_name(callback);
            for (holder, idx, expected, now) in hits {
                eprintln!(
                    "[ecwatch] CORRUPTED holder@0x{holder:x} fld[{idx}]: 0x{expected:x} -> 0x{now:x}  by NATIVE {native}"
                );
            }
            eprintln!("[ecwatch] Java stack at corruption (top first):");
            eprintln!("{}", native_diag_java_stack(thread, 24, "[ecwatch]   "));
        }
    }

    // bc math-ec 0x4 (CRATONVM_DBG_MEMWATCH): O(1) poll of the watched absolute
    // address after EVERY native return — a HIT here (vs at a bytecode
    // safepoint) blames the just-returned native directly.
    if diag & native_diag::MEMWATCH != 0 {
        crate::runtime::memwatch::poll_with(
            || format!("native:{}", native_callee_name(callback)),
            || native_diag_java_stack(thread, 28, "  "),
        );
    }

    // DBG (bc math-ec, CRATONVM_DBG_YOUNGSCAN): robust catch-all for the `0x4`
    // mutator write. We PROVED the `0x4` is written to a YOUNG object's
    // reference field between GCs (NOT by the GC: it is present in young at GC
    // entry — `[small4] PRE-GC YOUNG`). The victim CLASS varies run-to-run
    // (HexFormat, Level, EC types) so watching specific classes is unreliable;
    // instead scan the WHOLE young from-space after natives. ONE-SHOT: on the
    // first `0x4` found, dump the victim + the just-returned native + the full
    // Java stack (the corruptor is this native, or a bytecode in the top frame
    // just before it), then stop. STRIDE (CRATONVM_YOUNGSCAN_STRIDE, default 1)
    // throttles the whole-young walk on slow runs.
    if diag & native_diag::YOUNGSCAN != 0 && !youngscan_found() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        if n % youngscan_stride() == 0 {
            if let Some((addr, cid, fld, payload, nbr)) =
                shared.mem.heap.dbg_first_young_small_ref()
            {
                if !youngscan_mark_found() {
                    let native = native_callee_name(callback);
                    let is_arr = fld & 0x4000_0000 != 0;
                    eprintln!(
                        "[youngscan] FIRST 0x4 in YOUNG: holder@0x{addr:x} cid={cid} {}[{}] -> 0x{payload:x}  nbr_disc=0x{nbr:x}  (just-returned NATIVE {native})",
                        if is_arr { "arr" } else { "fld" }, fld & 0x3fff_ffff,
                    );
                    eprintln!("[youngscan] Java stack (top first):");
                    eprintln!("{}", native_diag_java_stack(thread, 30, "[youngscan]   "));
                }
            }
        }
    }
}

/// What [`enter_excusable_native`] raised: the word it raised and the one it
/// replaced (0 when none; a native the native called directly), and the
/// thread's plain copy before it.
#[derive(Clone, Copy)]
pub(in crate::vm::vm_exec) struct ExcusableNativeSpan {
    pub(in crate::vm::vm_exec) raised: usize,
    pub(in crate::vm::vm_exec) prior: usize,
    pub(in crate::vm::vm_exec) prior_copy: usize,
    /// The callback recorded before this native's (wave 62,
    /// `CRATONVM_HANDSHAKE_NATIVE_CALLBACK`), put back by the leave; `None`
    /// when the switch is off and nothing was recorded.
    pub(in crate::vm::vm_exec) prior_callback: Option<usize>,
}

/// Raise this thread's `GcBlockState::native_excuse` for the native it is
/// about to run (interpreter round i2 wave 58, lane L6,
/// `CRATONVM_HANDSHAKE_NATIVE_EXCUSE`; `jvm_thread::NativeExcuse`): a
/// handshake pause may excuse it while the native runs. Listable -- a
/// frame-trace pause may list the frames below the native -- when they are
/// all interpreter frames: no compiled activation (only the thread can walk
/// its JIT entry chain); since wave 62 a JNI native's row no longer counts
/// (the reader lists no such row), and since the i53 retirement neither does
/// a reflective call (its rows are recorded for the reader, `NX_REFLECTIVE`),
/// and a virtual thread is excusable too. Two loads, a store and, on a
/// thread's first native, the frame stack's learning of the word.
///
/// Interpreter round i2 wave 60, lane L6
/// (`CRATONVM_HANDSHAKE_NATIVE_EXCUSE_COMPILED`, `barrier`'s
/// `native_excuse_compiled_enabled`): compiled activations below the native
/// no longer make it unlisted -- the word carries `NX_COMPILED` and a
/// frame-trace pause walks them through the JIT cells the thread published
/// once, here, with the frame buffer
/// (`gc_and_alloc::publish_excused_native_traces`). The switch is read only
/// when compiled code is below the native; nothing more is stored per call.
#[inline]
pub(in crate::vm::vm_exec) fn enter_excusable_native(
    thread: &mut JvmThread,
    barrier: &crate::threading::GcBarrier,
    callback: usize,
) -> Option<ExcusableNativeSpan> {
    use crate::threading::jvm_thread::NativeExcuse;
    // The i53 retirement: a virtual thread too. Its continuation is frozen
    // only after a yielding native has returned (past the leave below), and
    // Java the native runs pins it (`vt_pinned_by_native`).
    let cell = &thread.gc_block_state.native_excuse;
    if !thread.frames.has_excuse_cell() {
        thread.frames.set_excuse_cell(cell);
        publish_native_excuse_jit_cells(cell);
    }
    // A native called directly by a native (no Java between them): lower
    // the outer one's word first, waiting while a pause holds it.
    let prior = cell.load() & !crate::threading::jvm_thread::NX_EXCUSED;
    if prior != 0 {
        cell.lower(prior);
    }
    // The thread's own chain depth, not the process-wide `any_thread_in_jit`
    // (a striped counter whose test reads every stripe).
    //
    // Interpreter round i2 wave 62, lane L6: rows of `jni_native_frames` (a
    // JNI native in the middle of the stack, and under a C JVMTI agent wave
    // 60's lambda proxy rows, which stand for a lambda-started thread's whole
    // life) no longer make the thread unlisted. The frame-trace reader
    // (`gc_and_alloc::publish_excused_native_traces`) never reads them: it
    // lists `frames[..depth]` as the thread's own published trace does
    // (`stackwalker::capture_published_trace`, which splices the reflective
    // calls but not those rows), and the rows' `Vec` is not touched by it.
    // `docs/feature-designs/real-jdk-mode.md`.
    //
    // The i53 retirement
    // (`docs/feature-designs/real-jdk-mode.md`):
    // a reflective call no longer makes it unlisted. The thread records its
    // reflective and served `Thread.run` calls (`NativeRowsSnapshot`, a copy
    // made only when they changed) before the raise, after the outer word's
    // lowering above (so no pause reads the record meanwhile), and the word
    // says so (`NX_REFLECTIVE`): the reader splices their JDK frames as the
    // thread's own published trace does. A served call without one used to
    // be listed with no `Thread.run` frame.
    let spliced = !thread.reflective_calls.is_empty() || !thread.served_thread_run.is_empty();
    if spliced {
        record_native_rows(thread);
    }
    let rows = if spliced {
        crate::threading::jvm_thread::NX_REFLECTIVE
    } else {
        0
    };
    let raised = if crate::jit::conservative_roots::current_thread_jit_depth() == 0 {
        NativeExcuse::raised(true, thread.frames.len())
    } else if barrier.native_excuse_compiled_enabled() && cell.has_jit_cells() {
        NativeExcuse::raised(true, thread.frames.len()) | crate::threading::jvm_thread::NX_COMPILED
    } else {
        NativeExcuse::raised(false, 0)
    };
    let raised = if raised & crate::threading::jvm_thread::NX_LISTABLE != 0 {
        raised | rows
    } else {
        raised
    };
    // Wave 62 (lane L6, `CRATONVM_HANDSHAKE_NATIVE_CALLBACK`): which native
    // the word is for, before the raise that publishes it. One branch on a
    // per-VM field next to the one the caller tested, and a store to the
    // cell's line the raise writes anyway.
    let prior_callback = barrier
        .excused_native_callback()
        .then(|| cell.replace_callback(callback));
    cell.raise(raised);
    let prior_copy = std::mem::replace(&mut thread.native_excuse_raised, raised);
    Some(ExcusableNativeSpan {
        raised,
        prior,
        prior_copy,
        prior_callback,
    })
}

/// Record the rows of `thread`'s stack a reader of its frames alone cannot
/// see, for the readers of the native it is entering
/// (`jvm_thread::NativeRowsSnapshot`; the i53 retirement). Out of line: only
/// a thread inside a reflective call, a served `Thread.run` or a JNI native
/// gets here.
#[cold]
#[inline(never)]
pub(crate) fn record_native_rows(thread: &JvmThread) {
    thread.gc_block_state.native_rows.lock().record(
        &thread.reflective_calls,
        &thread.reflective_frames_named,
        &thread.served_thread_run,
        &thread.jni_native_frames,
    );
}

/// Publish the calling thread's JIT cells into its native-excuse word, once
/// per thread (interpreter round i2 wave 60, lane L6): a pause that excuses
/// it in a native entered under compiled code reads its chain through them.
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn publish_native_excuse_jit_cells(cell: &crate::threading::jvm_thread::NativeExcuse) {
    let jit = crate::jit::conservative_roots::current_thread_jit_cells();
    cell.publish_jit_cells([
        jit.chain,
        jit.rbp_slot,
        jit.cm_slot,
        crate::runtime::interpreter::jit_bridge::osr_continuations_cell_addr(),
    ]);
}

/// Lower what [`enter_excusable_native`] raised, waiting while a pause holds
/// the excusal (the thread does not return to its caller while a pause
/// excuses it), and put back what it replaced.
#[inline]
pub(in crate::vm::vm_exec) fn leave_excusable_native(thread: &mut JvmThread, span: ExcusableNativeSpan) {
    let cell = &thread.gc_block_state.native_excuse;
    cell.lower(span.raised);
    if let Some(callback) = span.prior_callback {
        cell.restore_callback(callback);
    }
    if span.prior != 0 {
        cell.raise(span.prior);
    }
    thread.native_excuse_raised = span.prior_copy;
}

/// What [`enter_jni_native`] raised around a JNI native's C call.
pub(crate) struct JniNativeSpan {
    pub(in crate::vm::vm_exec) running: Option<crate::runtime::interpreter::RunningNativePrior>,
    pub(in crate::vm::vm_exec) excuse: Option<ExcusableNativeSpan>,
}

/// Around a JNI native's C call (the JNI arms of the invoke), what the
/// native-call funnel raises around a registered native's callback
/// (interpreter round i2, the i53 retirement;
/// `docs/feature-designs/real-jdk-mode.md`):
/// the debugger's running-native record with its acknowledgement and native
/// frames window (`interpreter::enter_running_native`), and the handshake
/// pause's excuse word ([`enter_excusable_native`]), both for
/// [`interpreter::C_NATIVE_CALLBACK`](crate::runtime::interpreter::C_NATIVE_CALLBACK).
/// A thread suspended, or asked for its stack by another thread, while its
/// JNI native's C code runs is listed at once, the native on top (named by
/// its `jni_native_frames` row, or by the top frame's invoke), as HotSpot
/// lists a thread in `_thread_in_native`; before, the JDWP `Frames` waited
/// and answered no frame, and a cross-thread trace waited for the C code to
/// return. One load each without a debugger and with the excuse switch off.
/// Balanced by [`leave_jni_native`] on every return of the C call (C code
/// does not unwind).
#[inline]
pub(crate) fn enter_jni_native(shared: &SharedVm, thread: &mut JvmThread) -> JniNativeSpan {
    let callback = crate::runtime::interpreter::C_NATIVE_CALLBACK;
    let running = crate::runtime::interpreter::enter_running_native(shared, thread, callback);
    let excuse = if shared.mem.gc_barrier.native_excuse_enabled() {
        enter_excusable_native(thread, &shared.mem.gc_barrier, callback)
    } else {
        None
    };
    JniNativeSpan { running, excuse }
}

/// End what [`enter_jni_native`] raised, in the funnel's order.
#[inline]
pub(crate) fn leave_jni_native(shared: &SharedVm, thread: &mut JvmThread, span: JniNativeSpan) {
    if let Some(excuse) = span.excuse {
        leave_excusable_native(thread, excuse);
    }
    crate::runtime::interpreter::leave_running_native_in(shared, thread, span.running);
}


/// The bytes and class file a `defineClass1` / `defineClass2` native parsed
/// for its supertype pre-pass ([`cratonvm_native_api::DefineClassFull::preparsed`],
/// `CRATONVM_DEFINE_ONE_PARSE`; interpreter round i3 wave 72, lane L5;
/// `native_builtins::lang_system::PreparsedDefine`), when the payload is
/// exactly that pair and this call owns it outright. `None` -- the define
/// parses the bytes itself, as without the switch -- for any other type or a
/// shared `Arc`.
pub(in crate::vm::vm_exec) fn define_preparsed_class_file(
    preparsed: std::sync::Arc<dyn std::any::Any + Send + Sync>,
) -> Option<(std::sync::Arc<[u8]>, cratonvm_reader::ClassFile)> {
    let typed = preparsed
        .downcast::<(std::sync::Arc<[u8]>, cratonvm_reader::ClassFile)>()
        .ok()?;
    std::sync::Arc::try_unwrap(typed).ok()
}

/// The funnel's arguments re-read from their pins after a collection: an
/// object argument takes its pin's current address; a pinned `long` is
/// rewritten only when it is a minted handle (a primitive that merely equals a
/// moved object's address stays as passed, the operand stack's rule).
///
/// Pins for one call are pushed in argument order and contiguously from
/// `pin_base`, so the *n*th set bit of `pinned_args` names the argument whose
/// root is `pin_base + n`.
pub(in crate::vm::vm_exec) fn native_args_from_pins(
    shared: &SharedVm,
    thread: &JvmThread,
    args: &[Value],
    pinned_args: u32,
    pin_base: usize,
) -> Vec<Value> {
    let mut fresh = args.to_vec();
    let mut root_idx = pin_base;
    for idx in 0..args.len().min(32) {
        if pinned_args & (1 << idx) == 0 {
            continue;
        }
        let remapped = thread.native_pin_roots[root_idx];
        root_idx += 1;
        match args[idx] {
            Value::Object(Some(_)) => fresh[idx] = Value::Object(Some(remapped)),
            Value::Long(bits)
                if crate::runtime::value_stack::smuggled_long_rewrite_admitted(
                    &shared.mem.heap,
                    bits as u64,
                    remapped.as_ptr() as u64,
                ) =>
            {
                fresh[idx] = Value::Long(remapped.as_ptr() as i64)
            }
            _ => {}
        }
    }
    fresh
}

/// zr z11/nd: is the ZGC native-OOM replay on? `CRATONVM_ZGC_NATIVE_OOM_REPLAY`,
/// default ON, `=0` = the one-attempt answer (the `OutOfMemoryError` the
/// refused allocation unwound with). Read on the refusal path only.
pub(crate) fn zgc_native_oom_replay_enabled() -> bool {
    true
}

/// gci i31/om (`gci-i31om-g1-boot-class-getname-oome-escapes-without-a-collection-FIXED-20261016`):
/// is the native-OOM replay on for Generational and G1?
/// `CRATONVM_GC_NATIVE_OOM_REPLAY`, default ON, `=0` = the one-attempt answer
/// those collectors had before (the `OutOfMemoryError` the refused allocation
/// unwound with, nothing collected). Read on the refusal path only.
pub(crate) fn gen_g1_native_oom_replay_enabled() -> bool {
    true
}

/// The collector half of [`native_oom_replay_admits`], pure over the two
/// switch readers: ZGC under `CRATONVM_ZGC_NATIVE_OOM_REPLAY` (zr z11/nd,
/// unchanged), Generational and G1 under `CRATONVM_GC_NATIVE_OOM_REPLAY`
/// (gci i31/om). A reader is called only for its own collector.
pub(in crate::vm::vm_exec) fn native_oom_replay_collector_admits(
    is_zgc: bool,
    zgc_on: impl FnOnce() -> bool,
    gen_g1_on: impl FnOnce() -> bool,
) -> bool {
    if is_zgc {
        zgc_on()
    } else {
        gen_g1_on()
    }
}

/// zr z11/nd: may the funnel collect and re-run `callback` after its
/// allocation refused? The collector's switch, and a body audited as
/// restartable (`cratonvm_native_builtins::oom_restartable`).
///
/// gci i31/om: Generational and G1 are admitted too (they kept the
/// one-attempt answer until then). A restartable native whose allocation
/// refused on a heap full of GARBAGE raised `OutOfMemoryError` there with
/// nothing collected, where HotSpot collects (clearing soft references)
/// before it raises a heap `OutOfMemoryError` from any VM allocation. Batch 3
/// rows `oo26_m_g1_fix_boot_r1` / `_r3` and `oo26_d_boot_g1`: the first
/// `Class.getName()` of a bootstrap class (`java.util.zip.Adler32`; no Java
/// loader ever made its name String) right after the probe dropped a full
/// heap minted the name in `native_class_get_name` through the one-attempt
/// intern door, which refused on G1 with no Free region left, and the
/// `OutOfMemoryError` escaped `main` outside the probe's guarded call.
#[cold]
pub(in crate::vm::vm_exec) fn native_oom_replay_admits(shared: &SharedVm, callback: NativeCallback) -> bool {
    if !native_oom_replay_collector_admits(
        shared.mem.heap.is_zgc(),
        zgc_native_oom_replay_enabled,
        gen_g1_native_oom_replay_enabled,
    ) {
        return false;
    }
    #[cfg(test)]
    if TEST_OOM_RESTARTABLE.with(|c| c.get()) == callback as usize {
        return true;
    }
    cratonvm_native_builtins::oom_restartable::native_is_oom_restartable(callback)
}

/// zr z11/nd (`zgc-zr8nr-one-attempt-native-doors-remaining-20261013.md`):
/// the collecting ladder for a native whose allocation unwound with
/// `NativeAllocOom`, run HERE because here nothing is held raw.
///
/// The native's one-attempt doors could not collect (its Rust locals are in
/// no root set), so they unwound. Now its frame is gone and every argument is
/// still pinned at `native_pin_roots[pin_base..native_pin_base]`: the native's
/// own pins and pending state are dropped, one rung of the shared native
/// ladder runs (the staged young door or the whole door with its clear-soft
/// rung, then a skipped clear-soft rung's repay -- `retry_after_reclaim`'s
/// order in `native-builtins`), and the native runs again on the re-read
/// arguments. A run that refuses again takes the next rung; when every rung
/// is spent (or a rung says a retry is not worth it: the overhead limit) the
/// last refused run's payload is the answer, i.e. the `OutOfMemoryError` the
/// caller saw before this existed, now after HotSpot's collections.
///
/// Only bodies listed as restartable reach this (re-running them is the same
/// as running them once; see that module for the rule).
#[cold]
#[inline(never)]
pub(in crate::vm::vm_exec) fn native_oom_replay(
    ctx: &mut NativeContextImpl<'_>,
    callback: NativeCallback,
    args: &[Value],
    pinned_args: u32,
    pin_base: usize,
    native_pin_base: usize,
    first: std::thread::Result<MethodCallResult>,
) -> std::thread::Result<MethodCallResult> {
    use std::sync::atomic::Ordering::Relaxed;
    let mut last = first;
    // 0: the staged young door, else the whole door; 1: the rest after the
    // young stage; 2: a skipped clear-soft rung's repay; 3: spent.
    let mut stage = 0u8;
    loop {
        // A native that released pins below its own base took the argument
        // pins with it: nothing left to re-read them from, so no re-run.
        if ctx.thread.native_pin_roots.len() < native_pin_base {
            return last;
        }
        // The refused run is gone: its own pins, its pending return and any
        // JNI exception it left die with it.
        ctx.thread.native_pin_roots.truncate(native_pin_base);
        ctx.thread.native_pending_return = None;
        let _ = crate::native::jni::take_jni_pending_exception();
        if stage > 2 {
            return last;
        }
        // The rung runs with the unwind permission still raised (it is
        // inside the funnel's `UnwindOkGuard`), so anything it allocates
        // that unwinds, or panics, is caught here and becomes the answer the
        // funnel's arms below map, exactly as it would inside a native.
        let rung = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match stage {
            0 => {
                if ctx.reclaim_young_before_alloc_retry() {
                    stage = 1;
                    true
                } else {
                    stage = 2;
                    ctx.reclaim_before_alloc_retry()
                }
            }
            1 => {
                stage = 2;
                ctx.reclaim_before_alloc_retry_after_young()
            }
            _ => {
                stage = 3;
                ctx.reclaim_after_failed_alloc_retry()
            }
        }));
        match rung {
            Ok(true) => {}
            Ok(false) => continue,
            Err(payload) => return Err(payload),
        }
        let census = &ctx.shared.mem.tlab_wedge.zgc_refusal;
        census.native_replays.fetch_add(1, Relaxed);
        let fresh = native_args_from_pins(ctx.shared, ctx.thread, args, pinned_args, pin_base);
        last =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(&mut *ctx, &fresh)));
        match &last {
            Err(payload) if crate::runtime::native_oom::is_native_alloc_oom_payload(&**payload) => {
            }
            // zs s1/nd: `served` counts a re-run that RETURNED a value. A re-run
            // that answered a Java error of its own (its own ladder's
            // `OutOfMemoryError`, say) is the answer too, but it was not served:
            // counting it made `native_replay_served == native_replays` read as
            // "every replay allocated" when the program still saw the OOME.
            Ok(Ok(_)) => {
                ctx.shared
                    .mem
                    .tlab_wedge
                    .zgc_refusal
                    .native_replay_served
                    .fetch_add(1, Relaxed);
                return last;
            }
            Ok(Err(_)) | Err(_) => return last,
        }
    }
}

pub(in crate::vm::vm_exec) fn safe_native_call_impl(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: NativeCallback,
    args: &[Value],
    prevalidated_objects: bool,
    // JIT round 17 wave 2 (lane funnel): `safe_native_call_jit_lean`'s route.
    // Implies `prevalidated_objects`; skips the argument re-forward and the
    // `disable_jit()`-gated native-array GC hook (see that wrapper's doc).
    jit_door_forwarded: bool,
    // JIT round 19 wave 3 (lane funnel3; FUNNEL2-19-3):
    // `safe_native_call_jit_fresh_result`'s route. The object result is healed
    // with `load_and_forward_validated` (no entry walk); see that wrapper.
    fresh_result: bool,
) -> MethodCallResult {
    let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::NativeCall);
    // Native nesting depth for the native->Java callback memo's witness: a
    // callback's tail reached from inside THIS native must not be mistaken
    // for the armed outer dispatch's. See `native_callee_memo::enter_native`.
    let _memo_native_frame = crate::runtime::native_callee_memo::enter_native();
    // The native this thread is inside, for `caller_loader` (which loader its
    // by-name class lookups use).
    let _caller_loader_native = cratonvm_native_api::caller_loader::enter_native(callback as usize);
    // ARCH-2026-08-04 A3 — the funnel's thirteen diagnostic gates, as one word.
    // Zero on every production run; see the `native_diag` module header.
    let diag = native_diag_mask();
    let _pin_floor_guard = (diag & native_diag::BLOCKGC != 0).then(|| PinFloorGuard {
        floor: thread.native_pin_roots.len(),
        thread: thread as *const JvmThread,
        callee_addr: callback as usize,
    });
    // Object arguments have just left the GC-visible operand stack. Refresh
    // forwarded addresses before pinning them: pinning a stale from-space
    // pointer preserves the bug rather than rooting the evacuated object.
    // This is the native-call counterpart to the interpreter getfield read
    // barrier and covers invokevirtual, invokeinterface, invokestatic, and
    // every re-entrant native dispatch through this common choke point.
    //
    // No VM allocation or safepoint can occur between extracting the stack
    // values and this barrier, so the forwarding header is still readable.
    // Hot path: native calls overwhelmingly carry <= 4 arguments (receiver
    // plus a couple of operands). The former unconditional `args.to_vec()` +
    // `Vec::with_capacity(args.len())` pair below allocated TWICE per native
    // call — measurable allocator traffic on autobox/map-heavy loops
    // (millions of calls). Keep both scratch buffers inline for the small-
    // arity case; longer slices (e.g. Set.of during Surefire bootstrap)
    // fall back to the original heap buffers with identical behavior.
    // Eight covers receiver + the full register-argument envelope of both
    // supported x64 ABIs and avoids heap scratch for common constructor and
    // reflection bridges with 5-7 Java arguments.
    const INLINE_NATIVE_ARGS: usize = crate::jit::helpers::INLINE_JIT_NATIVE_ARGS;
    // The scratch copy exists only for the arguments the barrier actually has
    // to rewrite, and a collection between the frame read and this line is the
    // exceptional case — the ordinary one is that every object argument
    // forwards to itself. So ask first, and copy nothing when the answer is
    // "none of them moved": initialising the buffer is eight 16-byte `Value`s
    // whether or not one is used, measured at 7.3 ns for the two scratch arrays
    // together in `native_funnel_profile` (`vm_exec.rs`), against a ~23 ns
    // one-argument funnel.
    // DECLARED, not initialised. The comment above prices the eight-`Value`
    // fill at 7.3 ns for the pair and says it is paid "whether or not one is
    // used" — and the ordinary case is that none is, because an argument that
    // forwards to itself needs no scratch at all. Definite-assignment lets the
    // fill move into the arm that actually reads it, so the common path stores
    // nothing. Measured at 2.7-3.8 ns for the remaining array on
    // `native_funnel_profile`'s scratch rung, against a ~26 ns funnel.
    let mut inline_forwarded: [Value; INLINE_NATIVE_ARGS];
    let mut heap_forwarded: Vec<Value>;
    let mut moved = None;
    // `safe_native_call_jit_lean`: the JIT door forwarded and validated every
    // argument with no GC point since, so none can have moved.
    let reforward: &[Value] = if jit_door_forwarded { &[] } else { args };
    for (index, value) in reforward.iter().enumerate() {
        if let Value::Object(Some(obj)) = value {
            // The caller already walked these pointers when it validated them;
            // `load_and_forward` would walk each one again. See
            // `VmHeap::load_and_forward_validated` for the contract, and
            // `safe_native_call_prevalidated_objects` for who establishes it.
            let forwarded = if prevalidated_objects {
                shared.mem.heap.load_and_forward_validated(*obj)
            } else {
                shared.mem.heap.load_and_forward(*obj)
            };
            if forwarded.as_ptr() != obj.as_ptr() {
                moved = Some(index);
                break;
            }
        }
    }
    let args: &[Value] = match moved {
        None => args,
        // At least one argument forwarded. Copy, then heal from the first one
        // that moved onward — everything before it was already checked and is
        // its own forwarding target.
        Some(first_moved) => {
            let forwarded_args: &mut [Value] = if args.len() <= INLINE_NATIVE_ARGS {
                inline_forwarded = [Value::Object(None); INLINE_NATIVE_ARGS];
                inline_forwarded[..args.len()].copy_from_slice(args);
                &mut inline_forwarded[..args.len()]
            } else {
                heap_forwarded = args.to_vec();
                &mut heap_forwarded[..]
            };
            for value in forwarded_args[first_moved..].iter_mut() {
                if let Value::Object(Some(obj)) = value {
                    *obj = shared.mem.heap.load_and_forward(*obj);
                }
            }
            forwarded_args
        }
    };
    // popped from the operand stack into this Rust slice and are otherwise
    // Pin object arguments for the duration of the native: they have been
    // invisible to `collect_roots` / frame scanning during a safepoint GC.
    let pin_base = thread.native_pin_roots.len();
    // Which arguments pinned, as a bitmask rather than an `[Option<usize>; 8]`.
    //
    // The indices this replaces are read in exactly one place: the
    // remap-arguments-after-GC rebuild below, which runs only when one of the
    // three GC hooks between here and there actually collected. The array was
    // therefore 128 bytes of stores per native call to serve a branch that
    // almost never runs. Pins for a given call are pushed in argument order and
    // contiguously from `pin_base`, so "argument *i* pinned" plus the count of
    // set bits below *i* reconstructs its index exactly.
    //
    // `INLINE_NATIVE_ARGS` is 8 and this is a `u32`, so an argument list longer
    // than 32 leaves the high arguments unrecorded — they are pinned either way
    // (the pin loop does not consult this), and only the post-GC remap of a
    // 33rd-or-later argument is missed. Both GC hooks are behind
    // `disable_jit()`/`young_spill_pressure()`, and a >32-argument native does
    // not exist in this tree; the alternative is a heap allocation on every
    // call to serve it.
    let mut pinned_args: u32 = 0;
    for (arg_index, a) in args.iter().enumerate() {
        let before = thread.native_pin_roots.len();
        match (prevalidated_objects, a) {
            (true, Value::Object(Some(object))) => thread.native_pin_roots.push(*object),
            _ => pin_value_for_native_call(shared, &mut thread.native_pin_roots, a),
        }
        if thread.native_pin_roots.len() > before && arg_index < 32 {
            pinned_args |= 1 << arg_index;
        }
    }
    let native_pin_base = thread.native_pin_roots.len();

    // Interpreter round i1 wave 26, lane L1: a JDWP `MethodEntry` of the
    // native method this call runs, for an interpreted caller, while a method
    // event request is in force (wave 27: or a JVMTI `MethodEntry` while an
    // agent listens) — one relaxed load otherwise, in a build with a JDWP
    // server only. Here, with the arguments pinned and the thread still a
    // mutator: a suspending request parks it, polling safepoints, an agent's
    // callback may collect, and the arguments are re-read from their pins
    // below.
    //
    // Wave 28 (lane L1): behind the same load, a callback standing in for a
    // Java method with bytecode that a debugger or an agent needs to see run
    // in a frame (method events, a breakpoint in it, a step) is not called:
    // the method's bytecode runs instead and its result is this call's.
    //
    // Interpreter round i3 wave 83, lane L1: behind the same load
    // (`native_hooks_armed`, the word `method_events_armed` reads, with the
    // `NativeMethodBind` bit), a C JVMTI env's `NativeMethodBind` of a
    // registered native the first time it runs, after its `MethodEntry`, as
    // HotSpot's native entry posts the entry before it links the native.
    #[cfg(feature = "experimental-debug")]
    let mut bind_posted = false;
    #[cfg(feature = "experimental-debug")]
    let debugged_native = if shared.debug.debugger_gates.native_hooks_armed() {
        if let Some(out) = crate::runtime::interpreter::run_stood_in_java_method(
            shared,
            thread,
            callback as usize,
            args,
        ) {
            thread.native_pin_roots.truncate(pin_base);
            // The funnel's return contract: an object result or a thrown
            // exception stays rooted in the pending-return slot until the
            // caller consumes it.
            thread.native_pending_return = match &out {
                Ok(Some(value)) => value_as_validated_object_ref(shared, *value),
                Err(MethodCallFailed::ExceptionThrown(exc)) => Some(*exc),
                _ => None,
            };
            return out;
        }
        let native = crate::runtime::interpreter::report_native_method_entry(
            shared,
            thread,
            callback as usize,
        );
        if shared.debug.debugger_gates.native_bind_hook_armed() {
            bind_posted = crate::jvmti::native_env::post_rust_native_method_bind(
                shared,
                thread,
                callback as usize, // Cast: fn address
            );
        }
        native
    } else {
        None
    };
    // An agent's `NativeMethodBind` callback ran (it may have collected):
    // the arguments are re-read from their pins below, as after a park.
    #[cfg(feature = "experimental-debug")]
    let parked_for_debugger =
        debugged_native.as_ref().is_some_and(|native| native.parked) || bind_posted;
    #[cfg(not(feature = "experimental-debug"))]
    let parked_for_debugger = false;

    let mut remapped_args = None;
    let stw_pending = shared
        .mem
        .gc_barrier
        .stw_requested
        .load(std::sync::atomic::Ordering::Acquire);
    if stw_pending {
        crate::runtime::interpreter::safepoint_check(shared, thread);
    }
    // A native callback can allocate through `NativeContext::new_array`, whose
    // contract cannot surface an allocation failure and therefore must not
    // initiate a moving collection mid-callback: native locals are not all
    // rooted yet. This funnel is the safe pre-callback point — every Java
    // argument is pinned above and refreshed below. The allocation wrapper
    // records the threshold crossing; only that request is consumed here.
    // A general `needs_gc()` check is intentionally not enough: it would force
    // a collection for every native dispatch on allocation-heavy JIT paths.
    // `safe_native_call_jit_lean`: compiled code is running, so
    // `disable_jit()` is false and this is dead on that route.
    let native_array_gc = !jit_door_forwarded
        && crate::runtime::env_cache::disable_jit()
        && shared
            .mem
            .native_array_gc_requested
            .swap(false, std::sync::atomic::Ordering::Relaxed);
    let mut requested_gc = false;
    if native_array_gc && !crate::runtime::interpreter::gc_overhead_limit_exceeded(shared) {
        // zr z3/t2: its own site name -- an occupancy request, not a failed
        // allocation (`gc_and_alloc::ZGC_OCCUPANCY_FORCED_SITES`).
        crate::runtime::interpreter::maybe_gc_forced_pub_at(shared, thread, "vm-exec-native-array");
        requested_gc = true;
    }
    // Native-alloc young-pressure relief: when a native allocation wrapper
    // had to spill into old gen because young was exhausted (the wrappers —
    // `ctx.alloc_object`, `new_array`, … — must stay GC-free mid-callback,
    // since their callers hold unrooted local `ObjectRef`s), run the same
    // orchestrated GC the interpreter's `gc_alloc_*` slow path would, HERE:
    // the one point on the native dispatch path where every argument is
    // pinned in `native_pin_roots` and remapped below, exactly like the
    // peer-STW `safepoint_check` above (so this adds no new hazard class for
    // callers). Without this, a workload whose allocations all happen inside
    // natives (e.g. a JIT'd HashMap<Integer,Integer> put loop: boxing + node
    // allocs are both native) never initiates ANY collection — young fills
    // once with mostly-dead wrappers, old gen absorbs every later allocation,
    // and `gen_heap::alloc_young_initialized` hard-aborts a process whose
    // heap is almost entirely garbage (HashMapOnly 30M at default -Xmx).
    // Gated on the heap's own occupancy trigger (`needs_gc`, live-metric) +
    // the GC-overhead limit so an all-live young cannot thrash boundary GCs;
    // the flag check itself is one relaxed load on the hot path.
    let mut pressure_gc = false;
    if shared.mem.heap.young_spill_pressure() {
        // `|| old_gen_needs_gc()` — SB-LOADER-ZIPCONTENT (2026-08-04).
        // `needs_gc()` asks about the YOUNG generation, and this hook fires
        // precisely when young could NOT serve an allocation and the request
        // went to old gen instead. Young's live set is small in exactly that
        // situation — that is why the allocation spilled — so the young trigger
        // answers "no" and the relief this hook exists to provide never runs.
        // Old gen then absorbs every subsequent spill with nothing watching it:
        // measured on `ZipContentTests`, the heap reached live=1,555,123,224 of
        // a 1,610,612,736-byte capacity before any collection ran, and a native
        // allocation — which cannot initiate a GC of its own, by design —
        // raised `OutOfMemoryError` in the gap. The collection that eventually
        // ran freed 1.49 GB, so nothing was leaking; the trigger was blind to
        // where the bytes had gone. `old_gen_needs_gc` is the same 75 %
        // threshold both major-GC branches use, so the collection this admits
        // is exactly the one that reclaims old.
        // `|| hard_alloc_failure()` — ZGC Phase 2.4 (2026-08-13). The two
        // predicates above are OCCUPANCY questions, and on a non-compacting
        // heap occupancy is not what binds: an arena refuses a 2 MB array
        // because no single hole is 2 MB, which it can do with `allocated` at
        // 7% of capacity. `ZgcRealHeap::alloc_raw` latches on that refusal and
        // its comment says exactly why the re-check below it is wrong — "a
        // request that just failed is stronger evidence that a cycle is due
        // than the `allocated >= gc_threshold` predicate, which counts LIVE
        // bytes and therefore cannot see the bump space this heap never
        // rewinds". It was right, and until this line the consumer overruled
        // it: `needs_gc()` answered no, the latch was cleared a few lines
        // below without collecting, and the signal was dropped. That is the
        // 2026-08-13 `TestNonBlockingAPI` shape, where the arena was full of
        // TLAB *reservations* the live-byte counter cannot see.
        //
        // A collection is worth running there even though the failing request
        // has already raised its `OutOfMemoryError`: this sweep coalesces the
        // free list and retracts the bump cursor into the freed tail, so it
        // restores CONTIGUITY, which is the resource that was missing. The
        // next request is the one it saves.
        //
        // It cannot storm — the bit is set only by a genuine refusal, is
        // cleared unconditionally below, and `gc_overhead_limit_exceeded`
        // still gates it, which is the same bound the soft path relies on.
        // zr z5/cr: the hard latch alone does not earn a collection of a heap
        // that has allocated under 4 KiB since its last one (ZGC only; the
        // latch is never raised on Generational or G1, and `=0` on
        // `CRATONVM_ZGC_REFUSAL_SKIP_FUTILE` restores the relief) -- see
        // `gc_and_alloc::zgc_hard_failure_relief_is_futile`.
        let occupancy_wants_gc =
            shared.mem.heap.needs_gc() || shared.mem.heap.old_gen_needs_gc();
        let refusal_wants_gc = !occupancy_wants_gc
            && shared.mem.heap.hard_alloc_failure()
            && !crate::runtime::interpreter::gc_and_alloc::zgc_hard_failure_relief_is_futile(
                shared,
            );
        if !crate::runtime::interpreter::gc_overhead_limit_exceeded(shared)
            && (occupancy_wants_gc || refusal_wants_gc)
        {
            // `maybe_gc_forced` retires this thread's TLAB itself.
            // zr z3/t2: its own site name -- a pressure verdict, not a failed
            // allocation (`gc_and_alloc::ZGC_OCCUPANCY_FORCED_SITES`).
            // zt t21/tg10 (`CRATONVM_ZGC_PRESSURE_REFUSAL_IS_STALL`, default
            // ON, `=0` kill): when the hard latch alone asked -- an allocation
            // the arena REFUSED, no occupancy verdict -- the collection follows
            // a failed allocation, which HotSpot's ZGC serves as an allocation
            // stall: site `vm-exec-pressure-refused`, outside the occupancy
            // list, so its cause is `Allocation Stall` and it counts as a
            // stall. It carried the pending rule / `Allocation Rate`.
            let site = if refusal_wants_gc {
                "vm-exec-pressure-refused"
            } else {
                "vm-exec-pressure"
            };
            crate::runtime::interpreter::maybe_gc_forced_pub_at(shared, thread, site);
            pressure_gc = true;
        }
        // gce e2/o: the opt-in latched-streak major (`CRATONVM_GC_NATIVE_LATCHED_MAJOR`,
        // gcd d3/o) is removed -- unbounded per latched episode and superseded
        // by the native OOME debt below and the shared native ladder
        // (`native_reclaim_before_alloc_retry`). A latched streak skips this
        // relief, as it always did by default.
        // Clear even when the gates said no: the flag was stale (another
        // thread's GC already relieved young) or the heap is genuinely full
        // of live data (overhead limit) — the next spill re-sets it. Same for
        // the hard latch, which `collect_garbage` also lowers; clearing it
        // here covers the overhead-limited path, where no cycle ran to do it.
        shared.mem.heap.clear_young_spill_pressure();
        shared.mem.heap.clear_hard_alloc_failure();
    }
    // gcd d4/j (default on; `CRATONVM_GC_OVERHEAD_PROGRESS=0` off): a heap
    // `OutOfMemoryError` was raised since this funnel last collected for one,
    // and a native's allocation never collects on its own -- so after a
    // program catches the error and drops its data, the first native that
    // allocates met the pre-drop heap and threw
    // (`gcd-d3o-native-door-first-allocation-throws-without-a-collection`,
    // shapes A and B). Collect here, where every argument is pinned and is
    // re-read below, as the relief above does. At most once per raised error
    // (`native_call_owes_oome_major` takes the debt).
    // gcd d5/q: only the raising thread pays, while the heap is still short
    // (`gc_and_alloc::oome_debt_decision`).
    // gcf f1/o: the payment dispatches per collector
    // (`pay_native_oome_debt`; Generational unchanged: `majors_to_decide_oome`
    // at `native-after-oome-major`). G1 and ZGC are never armed (gcf f5/o
    // deleted their opt-in).
    if crate::runtime::interpreter::native_call_owes_oome_major(shared, thread) {
        crate::runtime::interpreter::pay_native_oome_debt(shared, thread);
        pressure_gc = true;
    }
    if stw_pending || requested_gc || pressure_gc || parked_for_debugger {
        remapped_args = Some(native_args_from_pins(
            shared,
            thread,
            args,
            pinned_args,
            pin_base,
        ));
    }
    let native_args = remapped_args.as_deref().unwrap_or(args);

    // Dispatch tracing, VM-state publication, the native ring and the
    // straystack thread-local all live behind this one test (A3).
    let diag_state = if diag != 0 {
        native_diag_pre_call(diag, thread, callback)
    } else {
        NativeDiagState::default()
    };

    // P1 shadow record (`docs/threading/thread-transition-states.md` §7.2):
    // `NativeRunning` is the state the STW census deliberately WAITS for — "a
    // *running* native still holds raw `ObjectRef`s in Rust locals and must be
    // waited for so the copying collector does not relocate objects under it"
    // (`gc_barrier.rs`). This is the funnel every native dispatch passes
    // through, so recording here covers them all.
    //
    // The prior state is restored rather than assumed: this call site is
    // reached from the interpreter (`JavaRunning`), from compiled code
    // (`CompiledUninterruptible`) and from a re-entrant native, and inventing
    // an edge the code does not perform is what the tripwire is meant to
    // catch. Restoring on `Drop` covers the `catch_unwind`-caught panic and
    // the early returns below it. A native that opened a blocking region and
    // came back is left in whatever `end_blocking_region` recorded until this
    // guard restores the caller's state — both are tabled edges.
    //
    // ONE thread-local access for the pair, not three. This was
    // `current_state()` + `record_transition(NativeRunning)` + a `Drop` that
    // recorded the prior state, and `native_funnel_profile::funnel_cost_
    // breakdown` prices that trio at 10.8-14.4 ns of a 29-36 ns funnel — the
    // largest single component, with `current_state()` alone at 0.9 ns, which
    // is what says the cost was the repetition rather than the read.
    // `NativeStateSpan` takes the cell once and restores through a raw pointer;
    // the `Starting` correction the old code did here moved into
    // `enter_native_state`, where every caller gets it.
    struct NativeStateGuard(Option<thread_state::NativeStateSpan>);
    impl Drop for NativeStateGuard {
        fn drop(&mut self) {
            if let Some(span) = self.0.take() {
                span.restore("vm_exec::safe_native_call_impl:return");
            }
        }
    }
    #[cfg_attr(not(feature = "experimental-debug"), allow(unused_mut))]
    let mut _native_state_guard = NativeStateGuard(Some(thread_state::enter_native_state(
        "vm_exec::safe_native_call_impl",
    )));

    let result = {
        // Heap-exhaustion unwind permission. The callback below runs directly
        // under this `catch_unwind` with no JIT-compiled frame in between, so
        // the native array/object allocators may unwind from here to surface
        // the catchable `java.lang.OutOfMemoryError` HotSpot throws instead of
        // `std::process::abort()`-ing the process. The guard restores the
        // previous depth on every exit path, unwind included. See
        // `crate::runtime::native_oom`.
        struct UnwindOkGuard(u32);
        impl Drop for UnwindOkGuard {
            fn drop(&mut self) {
                crate::runtime::native_oom::restore(self.0);
            }
        }
        let _unwind_ok = UnwindOkGuard(crate::runtime::native_oom::enter_native_call());
        // The native this thread runs, for a debugger (interpreter round i1
        // wave 17): one load with none attached; restored after the
        // `catch_unwind`, which every exit of the callback passes.
        let running_native =
            crate::runtime::interpreter::enter_running_native(shared, thread, callback as usize);
        // Interpreter round i2 wave 58, lane L6 (`CRATONVM_HANDSHAKE_NATIVE_EXCUSE`):
        // a handshake pause may excuse this thread while the native runs
        // instead of waiting for it. One load of a per-VM field otherwise.
        let native_excuse = if shared.mem.gc_barrier.native_excuse_enabled() {
            enter_excusable_native(thread, &shared.mem.gc_barrier, callback as usize)
        } else {
            None
        };
        let mut ctx = NativeContextImpl { shared, thread };
        let mut result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            callback(&mut ctx, native_args)
        }));
        // zr z11/nd: a restartable native whose allocation refused is re-run
        // after a collection (`native_oom_replay`; ZGC, and Generational /
        // G1 since gci i31/om). Refusal path only:
        // one payload type test when the native unwound, nothing otherwise.
        if let Err(payload) = &result {
            if crate::runtime::native_oom::is_native_alloc_oom_payload(&**payload)
                && native_oom_replay_admits(shared, callback)
            {
                result = native_oom_replay(
                    &mut ctx,
                    callback,
                    args,
                    pinned_args,
                    pin_base,
                    native_pin_base,
                    result,
                );
            }
        }
        if let Some(span) = native_excuse {
            leave_excusable_native(thread, span);
        }
        crate::runtime::interpreter::leave_running_native_in(shared, thread, running_native);
        result
    };
    // Round 16 wave 9 (lane satb), C3W3-4: the native's pin balance, read
    // before any arm below truncates to `pin_base`. Debug builds only; an
    // unwound native (`Err` payload) skipped its unpins by construction and is
    // not judged.
    #[cfg(debug_assertions)]
    if let Ok(method_result) = &result {
        report_native_pin_imbalance(
            callback,
            native_pin_balance(
                native_pin_base,
                thread.native_pin_roots.len(),
                method_result.is_err(),
            ),
            // The classes of the leaked roots, which name the code that
            // pinned them (interpreter round i2 wave 62, lane L4). Asked only
            // for a reported leak.
            || {
                let roots = &thread.native_pin_roots;
                let cm = shared.classes.class_manager.read();
                roots[native_pin_base.min(roots.len())..]
                    .iter()
                    .map(|o| {
                        let cid = shared.mem.heap.class_id_of(*o);
                        cm.get_class(cid)
                            .map(|c| c.name.to_string())
                            .unwrap_or_else(|| format!("cid={}", cid.as_u32()))
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        );
    }
    // Straystack pop, ring exit, VM-state reset, EC watch, memwatch and the
    // young scan all live behind this one test (A3).
    if diag != 0 {
        native_diag_post_call(diag, shared, thread, callback, &diag_state);
    }

    let mut out: MethodCallResult = match result {
        Ok(method_result) => {
            if let Some(exc_handle) = crate::native::jni::take_jni_pending_exception() {
                if exc_handle == u64::MAX {
                    thread.native_pending_return = None;
                    thread.native_pin_roots.truncate(pin_base);
                    return Err(crate::runtime::exceptions::throw_runtime_error(
                        shared,
                        thread,
                        RuntimeError::IllegalStateException {
                            message: "JNI ThrowNew pending exception".to_string(),
                        },
                    ));
                }
                let ptr = exc_handle as *mut u8;
                if !ptr.is_null() && (ptr as usize) % 8 == 0 {
                    // JNI `Throw`/`ThrowNew` publish the throwable through
                    // `native_pending_return` before returning to native code.
                    // That slot is a GC root and is remapped in place, unlike
                    // the JNI ABI's raw handle.  Fall back to the raw handle
                    // for legacy/no-context producers.
                    let exc_ref = thread
                        .native_pending_return
                        .unwrap_or_else(|| unsafe { crate::types::ObjectRef::from_raw(ptr) });
                    thread.native_pending_return = Some(exc_ref);
                    thread.native_pin_roots.truncate(pin_base);
                    // The exception router runs while this thread is still runnable.
                    // Publish its pending return only at the next collector-visible
                    // boundary, just like an ordinary object return.
                    return Err(MethodCallFailed::ExceptionThrown(exc_ref));
                }
                thread.native_pin_roots.truncate(pin_base);
            }
            method_result
        }
        Err(payload) => {
            thread.native_pin_roots.truncate(pin_base);
            thread.native_pending_return = None;
            let _ = crate::native::jni::take_jni_pending_exception();
            // Set when the unwind was the allocators' heap-exhaustion signal;
            // consumed by this arm's tail expression below.
            let mut return_oom: Option<RuntimeError> = None;
            // Heap exhaustion raised by the native allocators (see
            // `crate::runtime::native_oom`): this is a HANDLED condition, not a
            // native bug. Surface the same catchable `java.lang.OutOfMemoryError`
            // the interpreter's `gc_alloc_*` slow path raises — the interpreter
            // materialises `RuntimeError::OutOfMemoryError` as a real
            // `java/lang/OutOfMemoryError` throwable (falling back to the
            // pre-allocated `singleton_oom` when the heap is too full to build
            // one), so application `catch (OutOfMemoryError)` observes it.
            if let Some(oom) = payload.downcast_ref::<crate::runtime::native_oom::NativeAllocOom>()
            {
                // Produced as this arm's value (not an early `return`) so the
                // funnel's shared return path — pin-watermark truncation, the
                // `native_pending_return` clear, the unpin ring — still runs.
                return_oom = Some(RuntimeError::OutOfMemoryError {
                    // Heap exhaustion or HotSpot's array-length refusal; the
                    // payload knows which (gc-common w3-c / w4-c).
                    message: oom.java_message(),
                });
            }
            let msg = if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = payload.downcast_ref::<&str>() {
                s.to_string()
            } else {
                "unknown native method panic".to_string()
            };
            let in_bootstrap = shared.get_init_level() < 4;
            if let Some(oom) = &return_oom {
                // A heap-exhaustion unwind is NOT a native bug and must not be
                // reported as one. `NativeAllocOom` is neither a `String` nor a
                // `&str`, so the ladder above degrades it to "unknown native
                // method panic" and the final arm below logged that at ERROR,
                // naming a callback address and a Java frame — which is how a
                // working, catchable `OutOfMemoryError` came to read as a VM
                // crash, and why the Tomcat `TestNonBlockingAPI` page filed
                // "root cause of the native method panic" as an open question.
                // There was no panic to root-cause. Say what actually happened,
                // at the severity it actually has.
                let top = thread
                    .frames
                    .last()
                    .map(|f| {
                        format!(
                            "{}.{}{}",
                            f.class_name(),
                            f.method_name(),
                            f.method_descriptor()
                        )
                    })
                    .unwrap_or_default();
                tracing::warn!(
                    target: "cratonvm::gc::guard",
                    java_frame = %top,
                    "a native allocation could not be served and unwound to the \
                     native-call boundary, where it becomes a catchable \
                     java.lang.OutOfMemoryError ({oom:?}). This is the handled \
                     heap-exhaustion path, not a native method fault.",
                );
                // gen r4w4/oom (2026-09-24): `-XX:+HeapDumpOnOutOfMemoryError`
                // on this door too. HotSpot dumps on the first
                // `OutOfMemoryError` whichever code raised it
                // (`report_java_out_of_memory`, which also covers the
                // array-length refusal); the interpreter ladders and the JIT
                // helpers dump, and a native allocation's OOME did not. Once
                // per VM, a no-op without the flag, and a NON-moving pause:
                // the native's pins were dropped above, and nothing it held is
                // reachable any more.
                let java_message = match oom {
                    RuntimeError::OutOfMemoryError { message } => message.as_str(),
                    _ => "Java heap space",
                };
                crate::runtime::interpreter::maybe_dump_heap_on_oom_for(
                    shared,
                    thread,
                    java_message,
                );
            } else if (msg.contains("unaligned pointer") || msg.contains("null pointer"))
                && in_bootstrap
            {
                shared
                    .debug
                    .swallow_counter
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::debug!("Native method panic (bootstrap): {}", msg);
                if crate::runtime::env_cache::strict_swallows() {
                    tracing::error!(
                        "CRATONVM_STRICT_SWALLOWS=1: safe_native_call bootstrap panic: {}",
                        msg,
                    );
                    std::process::abort();
                }
            } else {
                let top = thread
                    .frames
                    .last()
                    .map(|f| {
                        format!(
                            "{}.{}{}",
                            f.class_name(),
                            f.method_name(),
                            f.method_descriptor()
                        )
                    })
                    .unwrap_or_default();
                let callee = cratonvm_native_api::native_ring::name_of(callback as usize)
                    .unwrap_or_else(|| format!("<cb@{:#x}>", callback as usize));
                tracing::error!(
                    "Native method panic caught in {}: {} (native invoked from {})",
                    callee,
                    msg,
                    top
                );
            }
            match return_oom {
                Some(oom) => Err(MethodCallFailed::InternalError(VmError::Runtime(oom))),
                None => Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: format!("native method panic: {msg}"),
                })),
            }
        }
    };

    // `CRATONVM_DBG_LINKAGE=1` — a native that returns a `VmError::Linkage`
    // hands its caller something that is only catchable if the caller's
    // dispatch arm converts it. Name the native and the Java call site here,
    // at the boundary, so the routing can be followed from its origin.
    if diag & native_diag::LINKAGE != 0 {
        if let Err(MethodCallFailed::InternalError(VmError::Linkage(l))) = &out {
            let callee = native_callee_name(callback);
            crate::runtime::interpreter::dbg_linkage_dump(
                thread,
                "native return",
                &format!("{callee} -> {l:?}"),
            );
        }
    }

    // Interpreter round i1 wave 25, lane L1 (L1c): a `ThreadReference.Stop`
    // pending for this thread is thrown as the native returns, in place of
    // what it returned or threw — HotSpot delivers an asynchronous exception
    // at the native-to-Java transition. The stop woke a park, a wait or a
    // sleep; without this, the thread threw it only at its next interpreted
    // bytecode after the dispatch loop re-read its gate (a frame switch or a
    // back edge), which for a registered `LockSupport.park` (`--compatible`)
    // was after the caller had already run on (`L1W25JdiStopMonitors`,
    // `parker saw unparked`). One load while no stop is pending, in a build
    // with a JDWP server only.
    #[cfg(feature = "experimental-debug")]
    if shared.debug.debugger_gates.stop_pending()
        && matches!(out, Ok(_) | Err(MethodCallFailed::ExceptionThrown(_)))
    {
        if let Some(stop) = crate::debug::take_stop_for_blocking_call(shared, thread.thread_id.0) {
            out = Err(MethodCallFailed::ExceptionThrown(stop));
        }
    }
    thread.native_pending_return = None;
    match &mut out {
        Ok(Some(v)) => {
            // cce0079: RETURN-value counterpart to the argument
            // `load_and_forward` barrier at the top of this function — a
            // native whose last GC-capable call preceded its final read can
            // return an already-evacuated ref (the long-tail Family-1
            // stale-at-return shape); heal it here exactly like arguments
            // are healed on entry (the forwarding marker in from-space
            // stays readable until the memory is actually reused). Under
            // CRATONVM_DBG_STALE_OBJREF, name the producing native so the
            // site can be fixed at the source.
            if let Value::Object(Some(o)) = v {
                // FUNNEL2-19-3: a fresh-result native's object is a live base
                // (the caller's claim, see `safe_native_call_jit_fresh_result`),
                // so only the forwarding read runs. Under a diagnostic mask the
                // ordinary walk keeps its reports exact.
                let healed = if fresh_result && diag == 0 {
                    shared.mem.heap.load_and_forward_validated(*o)
                } else {
                    shared.mem.heap.load_and_forward(*o)
                };
                if healed.as_ptr() != o.as_ptr() && diag & native_diag::STALE_OBJREF != 0 {
                    let callee = native_callee_name(callback);
                    // The cached-dispatch callback pointer often has no ring
                    // name; the top Java frame names the method this native
                    // implements, which is the actionable identity.
                    let java_site = thread
                        .frames
                        .last()
                        .map(|f| {
                            format!(
                                "{}.{}{}",
                                f.class_name(),
                                f.method_name(),
                                f.method_descriptor()
                            )
                        })
                        .unwrap_or_default();
                    tracing::warn!(
                        "CRATONVM_DBG_STALE_OBJREF: native {} (invoked from {}) returned a \
                         stale (already-evacuated) ref 0x{:x} — healed to 0x{:x}",
                        callee,
                        java_site,
                        o.as_ptr() as usize,
                        healed.as_ptr() as usize,
                    );
                }
                *o = healed;
                // `CRATONVM_DBG_DEADREF_STORE`: did the heal actually heal it?
                //
                // `load_and_forward` reads the forwarding marker at the old
                // address, and the comment above is careful to say that marker
                // "stays readable until the memory is actually reused". Once the
                // allocator has re-served the span there is nothing left to
                // read, and the heal silently returns the dead address it was
                // given. That is not a rare corner under GC stress: it is the
                // normal case, because a stale reference is only USED some
                // cycles after it goes stale.
                //
                // So this is the boundary at which a native's stale return
                // becomes the interpreter's problem, and it is the only place
                // that can name the native. Everything downstream sees an
                // ordinary operand-stack value.
                {
                    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                    if *ON.get_or_init(|| cratonvm_types::flags().gc.dbg_deadref_store) {
                        if let Some(reason) =
                            cratonvm_gc::gen_heap::dead_young_ref_reason_global(o.as_ptr() as usize)
                        {
                            static N: std::sync::atomic::AtomicU64 =
                                std::sync::atomic::AtomicU64::new(0);
                            if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                                let callee = native_callee_name(callback);
                                let java_site = thread
                                    .frames
                                    .last()
                                    .map(|f| {
                                        format!(
                                            "{}.{}{}",
                                            f.class_name(),
                                            f.method_name(),
                                            f.method_descriptor()
                                        )
                                    })
                                    .unwrap_or_default();
                                eprintln!(
                                    "[deadref-nret] {reason} native {callee} (invoked from \
                                     {java_site}) returned 0x{:x}, which names no live object, \
                                     and `load_and_forward` could not heal it — the forwarding \
                                     marker is gone because the span was re-served. The defect \
                                     is in the native: it held a reference across an allocation.",
                                    o.as_ptr() as usize,
                                );
                            }
                        }
                    }
                }
            }
            if let Some(o) = value_as_validated_object_ref(shared, *v) {
                thread.native_pending_return = Some(o);
                if diag & native_diag::REMAP_TRACE != 0 {
                    let site = thread
                        .frames
                        .last()
                        .map(|f| format!("{}.{} pc={}", f.class_name(), f.method_name(), f.pc))
                        .unwrap_or_default();
                    crate::runtime::interpreter::nret_record(
                        callback as usize,
                        o.as_ptr() as usize,
                        &site,
                    );
                }
            }
            if diag & native_diag::ALTRACE != 0 {
                if let Value::Object(Some(o)) = v {
                    let callee = native_callee_name(callback);
                    let cid = shared.mem.heap.class_id_of(*o);
                    let cname = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| format!("<cid={cid:?}>"));
                    eprintln!(
                        "[altrace NRET] callee={callee} v={:p} cls={cname}",
                        o.as_ptr()
                    );
                }
            }
        }
        Err(MethodCallFailed::ExceptionThrown(exc)) => {
            let exc_is_current = shared
                .mem
                .heap
                .is_object_address(exc.as_ptr() as usize)
                .is_some();
            if !exc_is_current {
                if let Some(obj) = thread
                    .native_pin_roots
                    .get(native_pin_base..)
                    .and_then(|pins| pins.last())
                    .copied()
                {
                    *exc = obj;
                }
            }
            thread.native_pending_return = Some(*exc);
        }
        // Interpreter round i2 wave 53, lane L3: the funnel's mark of a raise
        // (`stackwalker::served_raise_frames`). The error path only.
        Err(MethodCallFailed::InternalError(VmError::Runtime(error))) => {
            // Wave 54: the receiver through its pin (a collection in the
            // callback may have moved it); none when it was not pinned.
            let receiver = (pinned_args & 1 != 0)
                .then(|| thread.native_pin_roots.get(pin_base).copied())
                .flatten();
            note_served_native_raise(shared, thread, error, receiver);
        }
        _ => {}
    }
    // Interpreter round i1 wave 26, lane L1: the JDWP `MethodExit` of the
    // native whose entry was reported above, for a normal return only (the
    // result is rooted in `native_pending_return` and the arguments are still
    // pinned); wave 27: also the JVMTI `MethodExit`, which an exceptional
    // return posts too (`DebuggedNative::reports_exit`). The native thread
    // state is closed first: a suspending request parks the thread, and a
    // parked thread, like one running an agent's callback, is a mutator.
    #[cfg(feature = "experimental-debug")]
    if let Some(native) = debugged_native.as_ref() {
        if native.reports_exit(&out) {
            if let Some(span) = _native_state_guard.0.take() {
                span.restore("vm_exec::safe_native_call_impl:return");
            }
            crate::runtime::interpreter::report_native_method_exit(
                shared, thread, native, &mut out,
            );
        }
    }
    // Interpreter round i1 wave 45, lane L1: a thread the debugger suspended
    // while this native ran parks here, the native listed on top, as HotSpot
    // holds it at its transition back to Java. One load while no JDWP
    // suspension is in force, in a build with a JDWP server only.
    #[cfg(feature = "experimental-debug")]
    if shared.debug.debugger_gates.suspension_in_force() {
        if let Some(span) = _native_state_guard.0.take() {
            span.restore("vm_exec::safe_native_call_impl:return");
        }
        crate::runtime::interpreter::park_if_suspended_at_native_exit(
            shared,
            thread,
            crate::runtime::interpreter::ReturningNative::Callback(callback as usize), // Cast: fn address
            &mut out,
        );
    }
    // Install the post-native object root before unwinding native pins. This
    // covers both object returns and Java exceptions thrown by the native. When
    // an exception ref has already gone stale, a native may leave the live
    // exception as a handoff pin above the argument-root watermark; do not use
    // those temporary pins to reinterpret normal object returns.
    if diag & native_diag::UNPIN_RING != 0 && pin_base < thread.native_pin_roots.len() {
        let callee = native_callee_name(callback);
        UNPIN_RING.with(|r| {
            let mut r = r.borrow_mut();
            if r.len() >= 6 {
                r.remove(0);
            }
            r.push((
                pin_base,
                thread.native_pin_roots.len(),
                format!("funnel-return callee={callee}\n"),
            ));
        });
    }
    thread.native_pin_roots.truncate(pin_base);
    // A running thread cannot be collected at this ordinary, non-blocking
    // return boundary. A peer-requested STW waits for this thread to reach its
    // next safepoint, and that safepoint refreshes the authoritative snapshot
    // before the collector reads it. A blocking native takes the separate
    // `deposit_root_snapshot` path before it parks. Keep object returns and
    // native-thrown exceptions in `native_pending_return` across this short
    // handoff, but do not rebuild the full O(stack-depth) snapshot here.
    //
    // This applies equally to interpreted and JIT callers: publishing on every
    // native return was pure running-thread overhead, while safepoints and
    // blocking transitions remain the only collector-visible publication sites.
    //
    // The slot is the caller's to consume: the interpreter drops it once the
    // value is on its operand stack (`native_return_pushed_to_stack`), and a
    // JIT helper once it hands the value back to compiled code
    // (`jit::helpers::drop_native_return_handed_to_compiled_code`,
    // handoff-w36b). Left set, it roots the value until the next native call.

    out
}

/// Clear [`JvmThread::native_pending_return`] after its object has been pushed
/// onto the caller operand stack (stackless native invoke path).
#[inline]
pub fn native_return_pushed_to_stack(_shared: &SharedVm, thread: &mut JvmThread) {
    thread.native_pending_return = None;
    // The caller has already placed the return value on an interpreter operand
    // stack. It is published by the next safepoint (or by a blocking deposit),
    // so rebuilding a snapshot while this thread remains runnable is redundant.
}

/// Acquire a Java monitor, blocking GC-SAFELY on contention. Returns the
/// (possibly GC-relocated) object ref — the caller MUST use the returned
/// ref for any later monitor operation (exit pairing).
///
/// The uncontended/re-entrant paths are a couple of CASes
/// (`enter_or_contend` → `None`). On contention the current owner may be
/// parked at a GC safepoint waiting for `gc_complete`; a contender that
/// blocks while still counted in the STW barrier's `expected` then wedges
/// the whole VM (owner waits GC, contender waits owner, GC initiator waits
/// contender — the H2 TestScript three-way deadlock). So before blocking we
/// run the full blocking-site protocol: PIN `obj` (the popped Rust copy is
/// otherwise invisible to the root scan and un-remapped — feeding a stale
/// ref to `header_of` after a moving GC is what tripped the "monitor
/// inflation invariant" panic), deposit roots, mark GC-blocked, block, then
/// re-sync via `check_post_block_gc` (which remaps the pin) and pop the
/// updated ref back.
///
/// SAFETY CONTRACT: only call from sites where every OTHER raw heap ref
/// the caller still needs is rooted AND remapped across the block (frame
/// slots are; immutable `&[Value]` arg slices are NOT — see the
/// synchronized-method prologue, which deliberately does not use this).
/// cceres3 FIX (leaked blocked-region exit self-heal): apply any pending
/// blocked-window fixup chain + exact slot-origin write-backs to this
/// thread's frames and thread-level refs, WITHOUT touching the blocked flag
/// or barrier accounting. Normally `check_post_block_gc` consumes these on
/// wake; a blocked-region exit path that skips it (observed live: an EQE
/// worker ran 43 frames -> 1 frame with fixup_pending=44 across four
/// deposits) leaves the thread executing on stale frames and poisons every
/// object graph it touches. Calling this from the safepoint publish bounds
/// that damage to one safepoint interval. Returns the number of chain
/// entries + write-backs applied.
/// The JIT half of the blocked-region wake: remap this thread's active
/// compiled frames, register images and shadow stack through the fixup chain
/// composed while it slept.
///
/// **Default-ON as of 2026-09-08; `CRATONVM_NO_BLOCKED_WAKE_JIT_REMAP=1` is the
/// kill switch.** It shipped opt-in and wired into
/// `apply_pending_blocked_fixups` only -- the LEAKED-region fallback -- so the
/// path that actually runs, `check_post_block_gc_refs`, remapped interpreter
/// frames and thread-local refs and nothing compiled. A thread that blocked in
/// a native with compiled frames below it therefore resumed with every
/// JIT-frame oop, register-image word and shadow-stack entry still at its
/// pre-move address, which is a use-after-free whatever else is true.
///
/// That omission is the one `xt_jit_coverage_assume`'s doc names as the price
/// of crediting blocked peers -- *"giving them a deposit means teaching the
/// blocked-region WAKE to remap JIT frames (it currently remaps only
/// interpreter frames)"* -- and it is what
/// `internal/fixed-suite-bugs/netty/bytebuf-multiplethreads-npe-generational-blocked-wake-jit-remap-FIXED-20260908.md`
/// spent §6-§12 narrowing to "the stale reference is outside the heap, in a
/// peer". Measured on that page's own repro at
/// `CRATONVM_GC_YOUNG_TRIGGER_PERCENT=1 CRATONVM_XT_JIT_COVERAGE_ASSUME=1
/// CRATONVM_GC_NO_PEER_PIN_DIVERT=1`: SIGSEGV in compiled code reading a
/// decommitted heap span, **6/6 with this disabled and 0/10 with it enabled on
/// one binary** (10/10 on the dev tip that predates it). Across the whole
/// 19-class family at that engagement, 18 of 19 classes crashed before and
/// none after.
pub(in crate::vm::vm_exec) fn blocked_wake_jit_remap_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        None::<std::ffi::OsString>.is_none()
    })
}

/// The JIT half of a blocked-region wake, shared by the ordinary path
/// (`check_post_block_gc_refs`) and the leaked-region fallback
/// (`apply_pending_blocked_fixups`).
///
/// Sound on both because each runs ON the waking thread, after
/// `leave_blocked_region_flagged` and before it can re-enter Java or compiled
/// code: `JIT_ENTRY_CHAIN`, the cached top RBP and the shadow stack are all
/// thread-local, so nothing here reads another thread's state, and no pause can
/// complete concurrently (this thread now counts in `expected`).
///
/// Re-remapping an already-rewritten slot is NOT harmless (corrected
/// gc-common w2-g, 2026-09-23; this doc used to say "a second lookup of a
/// to-space address misses"). A sliding compactor's map routinely holds
/// `A -> B` and `B -> C` together, so a second lookup of `B` hits and moves the
/// slot onto the object that used to live at `B`. Both callers therefore run
/// this BEFORE `apply_native_slot_fixups`, whose `still reads orig` guard is
/// what makes an overlap between the two word sets safe in that order and
/// only in that order.
pub(in crate::vm::vm_exec) fn apply_blocked_wake_jit_remap(
    shared: &SharedVm,
    thread: &mut JvmThread,
    fixup: &cratonvm_types::PointerMap,
) {
    if !blocked_wake_jit_remap_enabled() || fixup.is_empty() {
        return;
    }
    crate::jit::conservative_roots::remap_active_jit_frames(
        fixup,
        &crate::jit::conservative_roots::shadow_owned_slots(&thread.shadow_stack),
    );
    crate::jit::conservative_roots::remap_register_image_words(fixup, Some(shared));
    if crate::jit::conservative_roots::shadow_stack_enabled() {
        thread.shadow_stack.remap(fixup);
    }
}

/// Write back the native-stack words a peer collection scanned out of this
/// thread while it was blocked. Returns how many were stored.
///
/// Called from BOTH wake paths. `check_post_block_gc_refs` is the ordinary one;
/// `apply_pending_blocked_fixups` is the leaked-blocked-region fallback. The
/// first cut of this repair lived only in the fallback, and the engagement
/// census said so plainly -- `captured=49916 written=0 skipped=0`.
///
/// ORDER (gc-common w2-g, 2026-09-23): both callers run this AFTER
/// `apply_blocked_wake_jit_remap`. A word that is both a captured native slot
/// and a precise JIT slot / register-image word would otherwise be rewritten
/// twice -- here to `cur`, then by the JIT remap looking `cur` up in the
/// composed chain, which under a sliding compactor's `A -> B, B -> C` map
/// moves it onto the wrong object. In this order the JIT remap writes first and
/// the `p.read() == ns.orig` guard below skips the word.
///
/// `wake_floor` (gcj-j1pn-raw-root-write-owner-findings item 2): an address in
/// the frame that STARTED this wake, taken by the caller before it called
/// anything; `0` for none. Every frame below it was pushed by the wake itself
/// and did not exist when the words were captured, so no captured word can
/// name one of its slots; see [`native_fixup_store_band`].
pub(crate) fn apply_native_slot_fixups(thread: &mut JvmThread, wake_floor: usize) -> usize {
    let slots = {
        let mut n = thread.gc_block_state.native_slots.lock();
        std::mem::take(&mut *n)
    };
    if slots.is_empty() {
        return 0;
    }
    let mut n = 0usize;
    // THE BLOCKED-PEER NATIVE-STACK WRITE-BACK (2026-09-07).
    //
    // These are raw words in THIS thread's own machine stack that a peer
    // collection scanned conservatively while we were blocked: the objects were
    // kept alive and RELOCATED, and nothing rewrote the words, because a
    // blocked thread skips the safepoint-resume `apply_pointer_map_to_thread`
    // and the pin that was supposed to protect it is a no-op on a Cheney copy
    // (`VmHeap::Generational::honours_conservative_pins()` is false).
    //
    // Running here is what makes the write safe from concurrency: this is the
    // owning thread, after `leave_blocked_region_flagged` and before it can
    // re-enter Java or compiled code.
    //
    // THE GUARD IS LOAD-BEARING. The scanned band spans the peer's actively
    // running NATIVE frames, whose C locals churn while it is blocked, so a
    // word may have been reused since the capture. Only a word that still reads
    // `orig` is stored into; anything else is left alone. What remains is the
    // residual every conservative scan carries -- a C value bit-identical to a
    // young object base that moved -- and it is the same residual
    // `remap_one_frame_register_images` accepted when it chose to WRITE the
    // callee-saved GPR image, on the same grounds: `is_object_address` vetted
    // the word against the arena bounds and the object-start bitmap.
    //
    // THE OWN-STACK BOUND (gen r5w1/crash5, 2026-09-26). "This thread's own
    // stack" was an assumption, not a check, and two routes broke it:
    //
    // * the fold routes a capture by OS tid, and every alive blocked entry
    //   whose `os_tid` equals the scanned tid adopted it. Several entries claim
    //   one OS tid (every virtual thread that last mounted on a carrier keeps
    //   the carrier's tid while parked), so a parked continuation adopted words
    //   of its old CARRIER's stack and, remounted elsewhere, wrote them into a
    //   stack another thread was running on. The fold now routes to the newest
    //   claimant only (`ThreadRegistry::fold_pointer_map_into_blocked_audited`);
    // * the helper-window band ends where the `/proc/self/maps` line holding
    //   the peer's `rsp` ends, and glibc's cached thread stacks are reported
    //   merged with their neighbours (`xt_root_scan::read_self_memory`), so a
    //   band can run past the peer's own stack into a block glibc later gives
    //   to a new thread -- its stack, static TLS and TCB.
    //
    // Either way the store below was a read-compare-write on memory another
    // thread owns, racing it. This bound makes every route safe: a word outside
    // `[sp, stack top)` of the CALLING thread is never written. A word below
    // `sp` belongs to a frame that has returned, so declining it loses nothing.
    // `CRATONVM_DBG_ROOT_WRITE_AUDIT` reports each store and each refusal
    // (`cratonvm_gc::root_write_audit`), classified against the same band.
    //
    // THE WAKE FLOOR (gcj j1pn raw-write item 2): `[sp, stack top)` still
    // admits every frame between the wake's entry and this store -- the wake
    // path itself, none of which existed at capture -- and only the value
    // check protected such a word. `wake_floor` cuts them off.
    let own = cratonvm_gc::shadow_stack::SlotBand::current_thread();
    let band = native_fixup_store_band(own, wake_floor);
    let audit = cratonvm_gc::root_write_audit::enabled();
    for ns in &slots {
        if ns.cur == ns.orig || ns.addr == 0 || ns.addr & 0x7 != 0 {
            continue;
        }
        if !own.contains_slot(ns.addr) {
            crate::threading::thread_registry::NATIVE_SLOTS_REFUSED_OFF_STACK
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if audit {
                cratonvm_gc::root_write_audit::note_write(
                    cratonvm_gc::root_write_audit::RootWriteKind::BlockedPeerNativeSlot,
                    band,
                    ns.addr,
                    ns.orig,
                    ns.cur,
                    false,
                );
            }
            continue;
        }
        // Read only for a word the two bounds disagree on (no latch: the vm
        // statics ratchet), so the common store pays no flag lookup.
        if !band.contains_slot(ns.addr) {
            cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_REFUSED_BELOW_WAKE
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if audit {
                cratonvm_gc::root_write_audit::note_write(
                    cratonvm_gc::root_write_audit::RootWriteKind::BlockedPeerNativeSlot,
                    band,
                    ns.addr,
                    ns.orig,
                    ns.cur,
                    false,
                );
            }
            continue;
        }
        // The bound this store was made under (the floor's kill switch may
        // have let a word below the floor through).
        let guard = if band.contains_slot(ns.addr) {
            band
        } else {
            own
        };
        // SAFETY: `ns.addr` is an aligned word inside `[sp, stack top)` of the
        // calling thread (checked just above), so it is this thread's own live
        // stack, and this code runs ON that thread. The read-compare-write is
        // not racing anything: no other thread writes this stack, and we have
        // not resumed Java yet.
        unsafe {
            let p = ns.addr as *mut usize;
            if p.read() == ns.orig {
                if audit {
                    cratonvm_gc::root_write_audit::note_write(
                        cratonvm_gc::root_write_audit::RootWriteKind::BlockedPeerNativeSlot,
                        guard,
                        ns.addr,
                        ns.orig,
                        ns.cur,
                        true,
                    );
                }
                p.write(ns.cur);
                cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_WRITTEN
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                n += 1;
            } else {
                // The native call reused this word since the capture. Leaving
                // it alone is the whole safety argument -- see the block
                // comment above.
                cratonvm_gc::gc_quiescence::PEER_STACK_SLOTS_SKIPPED
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                // gcj j2/pn2: audited as a declined store, as the module doc
                // of `root_write_audit` says ("a word that no longer reads its
                // captured value logs `written=false`"); it was only counted.
                if audit {
                    cratonvm_gc::root_write_audit::note_write(
                        cratonvm_gc::root_write_audit::RootWriteKind::BlockedPeerNativeSlot,
                        guard,
                        ns.addr,
                        ns.orig,
                        ns.cur,
                        false,
                    );
                }
            }
        }
    }
    n
}

/// The words the blocked-peer native-stack write-back may store into: `own`
/// (`[sp, stack top)` of the calling thread) cut below at `wake_floor` (gcj
/// j1pn raw-write item 2). `0`, or a floor outside `own`, cuts nothing.
///
/// Why the floor is the right bound, and why it is taken at the WAKE and not
/// at the blocked-region entry (the page's first suggestion): a captured word
/// that still needs its store lies in a frame that existed at the capture and
/// still exists now. Every frame alive now is either an ancestor of the
/// wake's entry -- above the floor -- or a frame the wake pushed -- below it,
/// and new. An entry-time bound would be wrong for a thread whose wake runs
/// deeper than its entry did (a wait loop that left the region from a callee
/// of the frame that entered it): that callee's frames are live, were
/// captured, and lie below the entry's SP. The floor is exact for the frames
/// below it; the floor frame's own words above the probe remain value-gated.
/// Pure: testable.
#[inline]
pub(in crate::vm::vm_exec) fn native_fixup_store_band(
    own: cratonvm_gc::shadow_stack::SlotBand,
    wake_floor: usize,
) -> cratonvm_gc::shadow_stack::SlotBand {
    let floor = wake_floor & !7usize;
    if floor > own.lo && floor < own.hi {
        cratonvm_gc::shadow_stack::SlotBand::new(floor, own.hi)
    } else {
        own
    }
}


/// Drop `thread`'s frozen-frame root cache (`update_root_snapshot`'s
/// `rs_cache`) after a blocked-region wake rewrote its frames.
///
/// Only `remap_rs_cache_after_gc` rewrites that cache, and only the safepoint
/// and initiator paths call it. Both wake paths (`check_post_block_gc_refs`
/// and the leaked-region fallback `apply_pending_blocked_fixups`) rewrite the
/// frames through the composed fixup instead, so a cache entry built from them
/// before the rewrite names the PRE-move addresses -- and its tag is only a
/// collection count, which a cache rebuilt while this thread was still blocked
/// already carries. The next object-returning native then publishes those
/// cached addresses for the deep frames instead of what the frames hold, the
/// following pause finds no root for an object only those frames name, and
/// frees it under them. Measured on `ObjectStreamLookupGcRaceProbe` (G1, JIT
/// on): a pool worker's `runWorker` / `FutureTask.run` locals named a freed
/// FutureTask in about one run in four (`ClassCastException: Cannot cast
/// java.lang.Object to java.util.concurrent.FutureTask`),
/// `CRATONVM_DBG_ROOTSNAP_VERIFY` reported the missing deep-frame roots, and
/// `CRATONVM_ROOTSNAP_CACHE=0` ran 20/20 clean. A relocating fold is rare
/// next to a wake, so this costs one rebuild per relocation slept through.
/// `CRATONVM_ROOTSNAP_CACHE_WAKE_INVALIDATE=0` restores the old wake.
pub(crate) fn invalidate_root_cache_after_wake_remap(thread: &mut JvmThread) {
    {
        thread.rs_cache.clear();
        thread.rs_cache_gen = u64::MAX;
    }
}

#[inline(never)] // gcj j1pn raw-write item 2: the wake floor is this frame's (see the first line)
pub(crate) fn apply_pending_blocked_fixups(shared: &SharedVm, thread: &mut JvmThread) -> usize {
    // gcj j1pn raw-write item 2: the wake floor, FIRST, in this frame (see
    // `native_fixup_store_band`); every callee below is newer than any
    // capture. Item 7: and the audit's VM tag.
    let wake_floor = crate::jit::conservative_roots::current_stack_pointer();
    let _audit_vm = cratonvm_gc::root_write_audit::VmTagScope::enter(shared.vm_identity as u64);
    let fixup = {
        let mut f = thread.gc_block_state.fixup.lock();
        std::mem::take(&mut *f)
    };
    let origins = {
        let mut o = thread.gc_block_state.slot_origins.lock();
        std::mem::take(&mut *o)
    };
    let mut applied = 0usize;
    if !fixup.is_empty() {
        applied += fixup.len();
        // THE JIT HALF, and it was missing entirely.
        //
        // This function remapped a blocked thread's INTERPRETER frames,
        // `printed`, `java_thread_obj` and the native root slots -- and nothing
        // compiled. A peer that entered a blocking region with compiled frames
        // BELOW it therefore resumed with every JIT-frame oop, register image
        // and shadow-stack entry still at its pre-move address.
        //
        // `apply_pointer_map_to_thread` (the STW-resume path) already carries
        // exactly this block, and its comment describes the identical defect
        // for the thread that PARKED at the barrier: "this stranded a
        // non-initiator's JIT-frame oops at their old addresses after a
        // relocation -- a use-after-free". Blocked peers are the same bug one
        // path over, and they are the population the ZGC pinned-peer credit
        // makes relocation possible under.
        //
        // ON ITS OWN MERITS, and the elimination that first argued for it is
        // withdrawn. That argument ran: three repairs aimed at the CURRENT
        // thread's frames changed nothing, the blanket guard -- the only
        // remedy that also covers PEERS -- was 0 SIGSEGV in 4, so the defect
        // must be a peer resuming un-remapped. Adding this block REFUTED it:
        // the crash stayed at 3 of 4 against a control at 2 of 2. The guard
        // worked because it suppresses COMPACTION, and the real fault was
        // `relocate_stw` sliding into a decommitted arena granule
        // (`fixed-bugs/zgc-relocation-slides-wrote-into-decommitted-granules-FIXED-20260904`).
        //
        // The omission this closes is still real and still free: a peer that
        // blocked with compiled frames below it resumed with every JIT oop at
        // its pre-move address, which is a use-after-free whatever else was
        // true. See the retired
        // `bug-box-unbox-intrinsic-segv-under-relocation-20260902` for the
        // whole elimination and its retraction.
        //
        // Sound here for the same reason it is sound there: these walks are
        // thread-local (`JIT_ENTRY_CHAIN`, this thread's shadow stack) and this
        // function runs ON the waking thread, before it can re-enter compiled
        // code.
        //
        // Each slot must be looked up in `fixup` EXACTLY ONCE. The composed
        // chain is keyed by the addresses this thread held when it blocked and
        // valued by where those objects are NOW, and a sliding compactor's map
        // routinely holds `A -> B` and `B -> C` at once (ZGC's slide, the
        // Generational young+compact composition) -- so a second lookup of an
        // already-rewritten `B` HITS and moves the slot onto the object that
        // used to live at `B`. That is why the native-stack write-back below
        // runs AFTER this JIT remap and not before: its `still reads orig`
        // guard notices a word the JIT remap already rewrote, whereas the JIT
        // remap cannot notice a word the write-back already rewrote.
        // Attribution: this thread applied a relocation map through the
        // LEAKED-REGION FALLBACK.
        cratonvm_gc::gc_quiescence::note_pointer_map_applied(3);
        apply_blocked_wake_jit_remap(shared, thread, &fixup);
        for frame in &mut thread.frames {
            frame.update_frame_refs_mapped(&fixup, &shared.mem.heap, &shared.classes.type_maps);
            if let Some(ref mut obj_ref) = frame.monitor_on_exit {
                let old_addr = obj_ref.as_ptr() as usize;
                if let Some(&new_addr) = fixup.get(&old_addr) {
                    *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
            // Block monitors (`Frame::held_monitors`, wave 23 lane L7): one
            // lookup per entry, as the chain requires.
            frame.held_monitors.remap_with(|a| fixup.get(&a).copied());
        }
        // Every off-frame per-thread reference, through the ONE list the four
        // per-thread remap paths share (gc-common w2-g, 2026-09-23). This
        // fallback's own copy had drifted: it forwarded neither JIT cache
        // (`jit_hashmap_string_node_cache`, `string_case_cache`) nor either
        // parked throwable (`jit_pending_exception`,
        // `uncaught_exception_pending`), all of which the blocked deposit
        // publishes -- so they were kept alive and seeded into this chain, and
        // then never written back.
        crate::memory::gc::remap_thread_off_frame_refs(thread, &fixup);
        // The blocked deposit offered this thread's JIT deopt / exceptional
        // stash to the scan (`deposit_root_snapshot_inner`), so the remap half
        // is paired here exactly as on the ordinary wake. Thread-local, and
        // this function always runs on the owning thread.
        cratonvm_jit::deopt::remap_stashed_deopt_objects(|addr| {
            fixup.get(&(addr as usize)).map(|&to| to as u64)
        });
        crate::native::jni::update_local_refs_after_gc(&fixup);
    }
    // The native-stack write-back, AFTER the JIT remap above (see there).
    applied += apply_native_slot_fixups(thread, wake_floor);
    for so in &origins {
        if so.cur == so.orig {
            continue;
        }
        let fi = so.frame as usize;
        let Some(fr) = thread.frames.get_mut(fi) else {
            continue;
        };
        if so.is_stack {
            if fr.stack.rewrite_object_at(so.idx as usize, so.orig, so.cur) {
                applied += 1;
            }
        } else if let Value::Object(Some(o)) = fr.get_local(so.idx as u16) {
            if o.as_ptr() as usize == so.orig {
                fr.set_local(
                    so.idx as u16,
                    Value::Object(Some(unsafe { ObjectRef::from_raw(so.cur as *mut u8) })),
                );
                applied += 1;
            }
        }
    }
    if applied > 0 {
        invalidate_root_cache_after_wake_remap(thread);
    }
    applied
}

/// Release a monitor whose thin-lock owner died before anyone contended it.
///
/// `ThreadRegistry::mark_dead`'s sweep (`MonitorTable::release_monitors_held_by`)
/// only reaches monitors that were ALREADY inflated when the thread died. A lock
/// that was still an uncontended thin lock is inflated later, by the next
/// contender, and inflation seeds the new `Monitor`'s owner from the stale mark
/// word — so without this every later `monitorenter` on that object parks
/// forever behind an owner that will never exit. That was the Jetty
/// `Deflater.end()` hang (`16eaae90c`); the check was lost when a merge of
/// `origin/dev` (`27aa91068`, 2026-07-20) took the side without it, while
/// `monitor.rs` kept the test and the comments describing it.
///
/// Only an owner the monitor INHERITED from a thin lock and has held without a
/// break since (`Monitor::thin_seed_owner`) qualifies: an owner that died
/// holding an already-inflated monitor is the sweep's job, and a thread in its
/// termination handshake (which holds its own `Thread` mirror's monitor across
/// `mark_dead`, inflated before it is acquired — see
/// `release_monitors_held_by_except`) must not be robbed of it. The seed lives
/// on the monitor rather than being returned by the inflating call, so it does
/// not matter WHICH path inflated the lock (a declining `invoke_fast` door, JNI,
/// `Object.wait`): every later contender that reaches here sees it. The owner
/// must also be a thread the registry REGISTERED and has marked dead: an owner
/// id it never registered (the JNI `ThreadId(0)` placeholder) proves nothing.
pub(in crate::vm::vm_exec) fn release_dead_thin_owner(
    shared: &SharedVm,
    monitor: &crate::threading::monitor::Monitor,
    me: ThreadId,
) {
    let Some(owner) = monitor.thin_seed_owner() else {
        return;
    };
    if owner != me && shared.threads.thread_registry.is_registered_dead(owner) {
        monitor.force_release_if_owned_by(owner);
        // The dead owner kept its lock-slot lease at death because this lock
        // was still counted under it; the inflation that led here uncounted
        // it, so the lease goes back now unless another of its thin locks is
        // still counted (round 11 wave 15; `reclaim_dead_thread_lease`).
        shared.threads.monitors.reclaim_dead_thread_lease(owner);
    }
}

/// `CRATONVM_DBG_JITC` monitor census: how often this thread's compiled
/// monitor sites took the inline thin-lock path against how often they fell to
/// the helper, plus what THIS helper-path acquisition looks like to the inline
/// path's screens (armed? the mark word's state? the lock stack's `top` and
/// `spilled`?). The counters are `JitMonitorBlock`'s, written by sites compiled
/// under the same flag (`jit/src/runtime_lowering.rs`, `emit_inline_thin_lock`,
/// which both tiers emit through).
///
/// One line per doubling of the counters' total, so a hot loop costs O(log n)
/// lines; `JitMonitorBlock`'s `Drop` prints a thread's final totals. A thread
/// whose compiled sites never ran prints nothing here.
#[cold]
pub(in crate::vm::vm_exec) fn report_monitor_inline_census(shared: &SharedVm, thread: &mut JvmThread, obj: ObjectRef) {
    use cratonvm_types::{ObjectHeader, MARK_INFLATED, MARK_NEUTRAL, MARK_THIN_LOCKED};
    if !thread.jit_monitor_block.census_due() {
        return;
    }
    let (ie, ix, se, sx) = thread.jit_monitor_block.census_counts();
    let armed = thread.jit_monitor_block.is_armed();
    let (top, spilled) = shared
        .threads
        .thread_registry
        .jmx_lock_stack_shape(thread.thread_id)
        .unwrap_or((usize::MAX, false));
    // The 8-byte header's mark word is a `u32` dword (dev, 2026-09-24): a thin
    // owner is a per-VM lock SLOT, not a thread id, so it is printed as one.
    let shape = if obj.as_ptr().is_null() {
        "null".to_string()
    } else {
        // SAFETY: `obj` is the live, non-null receiver this `monitorenter` is
        // about to lock, so it starts with a live `ObjectHeader`; the mark word
        // is read the way the monitor code reads it.
        let mark = unsafe { &*(obj.as_ptr() as *const ObjectHeader) }
            .mark_word
            .load(std::sync::atomic::Ordering::Relaxed);
        match ObjectHeader::mark_state(mark) {
            s if s == MARK_NEUTRAL && ObjectHeader::thin_lockable(mark) => "neutral".to_string(),
            s if s == MARK_NEUTRAL => "hashed".to_string(),
            s if s == MARK_THIN_LOCKED => format!(
                "thin-slot{}-rec{}",
                ObjectHeader::thin_lock_owner(mark),
                ObjectHeader::thin_lock_recursion(mark)
            ),
            s if s == MARK_INFLATED => "inflated".to_string(),
            s => format!("state{s}"),
        }
    };
    eprintln!(
        "[monitor-inline census] tid={} name={} armed={armed} inline_enters={ie} \
         inline_exits={ix} helper_enters={se} helper_exits={sx} | this helper \
         monitorenter: mark={shape} lock_stack_top={top} spilled={spilled}",
        thread.thread_id.0, thread.name,
    );
}

/// The opt-in running park in front of the GC-blocked one
/// (`CRATONVM_MONITOR_LAZY_PARK_US`, [`crate::threading::monitor::Monitor::park_enter_while`]):
/// `true` when it acquired `monitor` for `tid`. Skipped (`false`) when the arm
/// is off or a pause is already requested -- a running parker would only
/// delay that pause -- and left (`false`) when one is requested by the time a
/// wake-up it lost would send it back to sleep (round 12 wave 1).
///
/// The caller has published the JMX contention and must NOT be GC-blocked;
/// on `true` it completes the JMX enter with the unmoved object.
pub(in crate::vm::vm_exec) fn lazy_monitor_park(
    shared: &SharedVm,
    monitor: &std::sync::Arc<crate::threading::monitor::Monitor>,
    tid: ThreadId,
) -> bool {
    let Some(budget) = crate::threading::monitor::monitor_lazy_park_budget() else {
        return false;
    };
    let no_pause_requested = || {
        !shared
            .mem
            .gc_barrier
            .stw_requested
            .load(std::sync::atomic::Ordering::Acquire)
    };
    // Counted as a running parker, so a pause initiator can wake it
    // (`MonitorTable::wake_lazy_parkers`, round 12 wave 1); listed by its
    // monitor under `CRATONVM_MONITOR_LAZY_WAKE_LIST` (gcg g3/k).
    no_pause_requested()
        && shared.threads.monitors.lazily_parked_on(monitor, || {
            monitor.park_enter_while(tid, budget, no_pause_requested)
        })
}

/// The GC-blocked contended entry of a site with a debugger story
/// (`monitor_enter_blocking`, `monitor_enter_synchronized_method`, JNI
/// `MonitorEnter`): `Monitor::block_enter` (labeled when `label` is given),
/// and a hold won while the debugger or a C JVMTI agent holds `tid`
/// suspended is given back until its resume. With
/// `CRATONVM_MONITOR_SUSPENSION_AWARE_ENTRY` (interpreter round i2 wave 56,
/// lane L6) the give-back is the monitor's own loop
/// (`Monitor::block_enter_with`, the VM's `debug::DebuggerEntryHold`);
/// without it, the wave-54 give-back after the enter
/// (`debug::exit_monitor_while_suspended`). Without `experimental-debug`,
/// the bare enter. `CRATONVM_FRAME_TRACE=1` prints
/// `[MONITOR_ENTRY_HOLD] tid=<n> given_back=<n>` when the loop gave one back.
#[inline]
pub(crate) fn block_enter_suspension_aware(
    shared: &SharedVm,
    tid: ThreadId,
    monitor: &crate::threading::monitor::Monitor,
    label: Option<&str>,
) {
    #[cfg(feature = "experimental-debug")]
    {
        if shared.threads.monitors.suspension_aware_entry() {
            let hold = crate::debug::DebuggerEntryHold { shared };
            let given_back = monitor.block_enter_with(tid, label, &hold);
            if given_back > 0 && crate::runtime::env_cache::frame_trace() {
                eprintln!("[MONITOR_ENTRY_HOLD] tid={} given_back={given_back}", tid.0);
            }
            return;
        }
        monitor.block_enter_labeled(tid, label);
        crate::debug::exit_monitor_while_suspended(shared, tid, monitor);
    }
    #[cfg(not(feature = "experimental-debug"))]
    {
        let _ = shared;
        monitor.block_enter_labeled(tid, label);
    }
}

/// After [`lazy_monitor_park`] won `monitor` for `tid`: give it back
/// (`true`) when the debugger holds `tid` suspended, so the caller takes the
/// GC-blocked park, which waits for the resume without the monitor
/// (interpreter round i2 wave 54, lane L6;
/// `debug::exit_monitor_while_suspended`). The running park won as a mutator
/// that would otherwise park at its next suspend point holding the lock.
/// One load without a suspension; always `false` without the debugger.
#[inline]
pub(in crate::vm::vm_exec) fn lazy_win_given_back(
    shared: &SharedVm,
    monitor: &crate::threading::monitor::Monitor,
    tid: ThreadId,
) -> bool {
    #[cfg(feature = "experimental-debug")]
    if crate::debug::monitor_entry_held_off(shared, tid) {
        return monitor.exit(tid).is_ok();
    }
    #[cfg(not(feature = "experimental-debug"))]
    let _ = (shared, monitor, tid);
    false
}

/// Should a contended monitor entry of `thread` that is about to block have
/// the virtual-thread scheduler compensate for the carrier it holds?
/// (interpreter round i4 wave 84, lane L6,
/// `CRATONVM_MONITOR_VTHREAD_ENTRY_COMPENSATES`; page
/// `interpreter-i84-L6-vthread-monitor-entry-does-not-unmount-FIXED-20261123`.)
///
/// Yes for a MOUNTED JDK virtual thread (`blocks_jdk_carrier`'s test): this
/// VM never unmounts a virtual thread blocked on a monitor, where HotSpot 25
/// does (JEP 491), so the block holds the carrier and a virtual thread the
/// owner waits for may never get one. `Object.wait` and a pinned park take
/// the same `Blocker.begin()` already (`monitor_wait_compensated`,
/// `native_unsafe_park_with_blocker`). Not while the compiled helper has
/// armed its blocked-monitor proof: the compensation runs Java, whose own
/// blocking deposit would settle the arm.
#[inline]
pub(in crate::vm::vm_exec) fn monitor_entry_compensates(shared: &SharedVm, thread: &JvmThread) -> bool {
    thread.jdk_continuation.jdk()
        && thread.jdk_continuation.carrier() != 0
        && shared.threads.monitors.vthread_entry_compensates()
        && thread
            .gc_block_state
            .monitor_block_proven_jit_depth
            .load(std::sync::atomic::Ordering::Relaxed)
            != crate::threading::jvm_thread::MONITOR_BLOCK_PROOF_ARMED
}

/// Rewrite `args` from the native pins `pin_value_for_native_call` pushed for
/// them (`arg_root_indices[i]` is `args[i]`'s pin, if it got one): the GC
/// remap half of a pinned window, shared by the contended synchronized-method
/// park and its carrier compensation.
pub(in crate::vm::vm_exec) fn remap_args_from_pins(
    shared: &SharedVm,
    thread: &JvmThread,
    args: &mut [Value],
    arg_root_indices: Vec<Option<usize>>,
) {
    for (idx, root_idx) in arg_root_indices.into_iter().enumerate() {
        let Some(root_idx) = root_idx else {
            continue;
        };
        let Some(remapped) = thread.native_pin_roots.get(root_idx).copied() else {
            continue;
        };
        match args[idx] {
            Value::Object(Some(_)) => args[idx] = Value::Object(Some(remapped)),
            // Minted handles only — see the twin in the native-call remap.
            Value::Long(bits)
                if crate::runtime::value_stack::smuggled_long_rewrite_admitted(
                    &shared.mem.heap,
                    bits as u64,
                    remapped.as_ptr() as u64,
                ) =>
            {
                args[idx] = Value::Long(remapped.as_ptr() as i64)
            }
            _ => {}
        }
    }
}

/// `Blocker.begin()` (`begin`) or `Blocker.end(true)` (`!begin`) for a
/// contended monitor entry ([`monitor_entry_compensates`]), with the monitor
/// object and every reference in `args` pinned across the Java call and
/// rewritten in place. Answers the (possibly moved) object and, for a begin,
/// whether a compensation was attempted (the matching end is owed).
pub(in crate::vm::vm_exec) fn monitor_entry_compensation(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
    args: &mut [Value],
    begin: bool,
) -> (ObjectRef, bool) {
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(obj);
    let mut arg_root_indices = Vec::with_capacity(args.len());
    for arg in args.iter() {
        let before = thread.native_pin_roots.len();
        pin_value_for_native_call(shared, &mut thread.native_pin_roots, arg);
        arg_root_indices.push((thread.native_pin_roots.len() > before).then_some(before));
    }
    let attempted = {
        let mut ctx = NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        if begin {
            super::jdk_continuation::begin_carrier_compensation(&mut ctx)
        } else {
            super::jdk_continuation::end_carrier_compensation(&mut ctx, true);
            false
        }
    };
    let fixed = thread
        .native_pin_roots
        .get(pin_base)
        .copied()
        .unwrap_or(obj);
    remap_args_from_pins(shared, thread, args, arg_root_indices);
    thread.native_pin_roots.truncate(pin_base);
    (fixed, attempted)
}

pub(crate) fn monitor_enter_blocking(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
) -> ObjectRef {
    match monitor_enter_blocking_or_unmount(shared, thread, obj, false) {
        VthreadMonitorEntry::Entered(obj) => obj,
        // `may_unmount == false` never answers it.
        VthreadMonitorEntry::Unmount => obj,
    }
}

/// Is `class` value-based for HotSpot: `JVM_ACC_IS_VALUE_BASED_CLASS`, which
/// `ClassFileParser` sets from a runtime-visible `@jdk.internal.ValueBased`
/// on a class the boot or platform loader defines (a privileged loader; the
/// annotation is ignored elsewhere).
pub(in crate::vm::vm_exec) fn class_is_value_based(class: &crate::classloading::Class) -> bool {
    use crate::classloading::ClassLoaderId;
    matches!(
        class.loader_id,
        ClassLoaderId::Bootstrap | ClassLoaderId::Extension
    ) && class
        .annotations
        .iter()
        .any(|a| class.constant_pool.get_utf8(a.type_index) == Some("Ljdk/internal/ValueBased;"))
}

