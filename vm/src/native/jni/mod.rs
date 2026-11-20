// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JNI (Java Native Interface) implementation.
//!
//! Provides the standard C API for native code to interact with the JVM.
//! The function table is represented as a flat `[usize; 229]` array matching
//! the JNI spec layout. Native code accesses it via `(*env)[index](env, ...)`.
//!
//! JNI functions access the VM via thread-local storage. Before calling into
//! native code, the interpreter installs the TLS context with `replace_jni_context()`,
//! and clears it on return with `clear_jni_context()`.

#![allow(dead_code)]

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_char;
use std::sync::{Arc, Weak};

use crate::classloading::{find_field_recursive, find_method_recursive, ClassId};
use crate::memory::heap::ArrayElementType;
use crate::threading::{JvmThread, ThreadId};
use crate::types::{ObjectRef, Value};
use crate::vm::{invoke_on_class_shared, SharedVm};
/// Build, register, and install a foreign (host-created) OS thread as a
/// first-class, GC-safe Java thread, returning the stable `*mut JvmThread` the
/// caller installs into `JNI_THREAD`.
///
/// This mirrors the per-thread wiring the main thread gets in `Vm::new`
/// (`vm_init.rs`) and that `Thread.start` workers get (`vm_exec.rs`): a
/// heap-boxed `JvmThread` with a fresh `ThreadId`, registered in the
/// `ThreadRegistry`, with its shared `Arc` fields (interrupted / park_state /
/// root_snapshot / frame_trace / gc_block_state) mirrored into the registry so a
/// GC initiator on another thread can scan and maintain this thread's roots.
///
/// Ordering is deliberate (see §3.2 / §5 of the design): the thread is
/// registered and its `root_snapshot` / `gc_block_state` are shared with the
/// registry BEFORE the caller publishes `JNI_SHARED_VM` / `JNI_THREAD`. So the
/// first instant this thread can run Java (and trip a safepoint) it is already
/// stop-the-world-visible with a (currently empty) deposited snapshot — there is
/// no window where it holds live oops but is invisible to `request_stw`.
///
/// The entry is COUNTED (`stw_ready`) from registration on, so a caller that
/// then models the thread as idle must not leave a gap before it raises
/// `in_blocked_region`: [`attach_foreign_thread_idle`] uses the starting form
/// instead (gcd d2/i). Test-only since then: production attaches go
/// through [`attach_foreign_thread_idle`].
#[cfg(test)]
pub(crate) fn attach_foreign_thread(
    shared: &SharedVm,
    daemon: bool,
    name: Option<&str>,
) -> *mut JvmThread {
    attach_foreign_thread_registered(shared, daemon, name, true)
}

/// [`attach_foreign_thread`], registering the entry counted (`stw_ready`) or,
/// with `stw_ready == false`, as STARTING: alive but in no pause's `expected`
/// until the caller runs `ThreadRegistry::mark_stw_ready`. Test-only since
/// zt t11/tg4: production goes through [`attach_foreign_thread_registered_as`].
#[cfg(test)]
fn attach_foreign_thread_registered(
    shared: &SharedVm,
    daemon: bool,
    name: Option<&str>,
    stw_ready: bool,
) -> *mut JvmThread {
    attach_foreign_thread_registered_as(shared, daemon, name, stw_ready, true)
}

/// Unit tests switch [`foreign_tid_publish_active`]'s own half per thread
/// ([`NativeCallTls::tid_publish_override`]): the switch is latched per process.
#[cfg(test)]
fn foreign_tid_publish_test_override() -> Option<bool> {
    JNI_NATIVE_CALL
        .try_with(|t| t.tid_publish_override.get())
        .ok()
        .flatten()
}

#[cfg(not(test))]
#[inline(always)]
fn foreign_tid_publish_test_override() -> Option<bool> {
    None
}

/// Unit tests switch [`foreign_quiet_idle_active`] per thread
/// ([`NativeCallTls::quiet_idle_override`]): the switch is latched per process.
#[cfg(test)]
fn foreign_quiet_idle_test_override() -> Option<bool> {
    JNI_NATIVE_CALL
        .try_with(|t| t.quiet_idle_override.get())
        .ok()
        .flatten()
}

#[cfg(not(test))]
#[inline(always)]
fn foreign_quiet_idle_test_override() -> Option<bool> {
    None
}

#[cfg(test)]
thread_local! {
    /// Unit tests switch the transitions per thread: the flag is latched per
    /// process, and tests share one.
    static FOREIGN_TRANSITIONS_TEST_OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
}

#[cfg(test)]
fn foreign_transitions_test_override() -> Option<bool> {
    FOREIGN_TRANSITIONS_TEST_OVERRIDE
        .try_with(Cell::get)
        .ok()
        .flatten()
}

#[cfg(not(test))]
#[inline(always)]
fn foreign_transitions_test_override() -> Option<bool> {
    None
}

/// Unit tests switch the native transitions per thread (the flag is latched
/// per process): [`NativeCallTls::test_override`].
#[cfg(test)]
fn native_transitions_test_override() -> Option<bool> {
    JNI_NATIVE_CALL
        .try_with(|t| t.test_override.get())
        .ok()
        .flatten()
}

#[cfg(not(test))]
#[inline(always)]
fn native_transitions_test_override() -> Option<bool> {
    None
}

/// Remap or clear every weak global ref after a STOP-THE-WORLD collection
/// (gc-common w8-c, `common-w7c-jni-weak-global-refs-are-strong`).
///
/// For each uncleared weak entry, with the PRE-collection address:
///
/// * in `pointer_map`: the referent moved (or is an identity survivor); the
///   entry now names the new address;
/// * else survived in place: kept. The verdict is the reference processor's
///   (`VmHeap::watched_pre_gc_addr_survived`, see
///   `process_references_after_gc`) AND `addr_keyed::survived_in_place`, so a
///   header that still parses in reclaimed space never keeps an entry. On G1
///   the pause's `survived` (`addr_keyed::InPlaceVerdict::survived`) also
///   refuses a referent G1's per-object verdict found unheld in a young region
///   the pause kept in place (gci i28/wt; `addr_keyed::RelocationTargets::with_g1_unheld`):
///   the region-granular G1 arm alone kept every such weak global;
/// * else: the referent died, the entry is CLEARED. From here on
///   `IsSameObject(wref, NULL)` answers `JNI_TRUE` and `NewLocalRef(wref)`
///   answers NULL; the handle itself stays valid until `DeleteWeakGlobalRef`.
///
/// Finalization: a finalizable referent is resurrected by the collection
/// (`finalizable_roots`), so it survives this verdict and the weak global
/// keeps naming it until the object is really gone. That is the JNI spec's
/// phantom strength for weak globals (JDK 9+), not the weak strength of a
/// `java.lang.ref.WeakReference`.
///
/// Where it must run: inside the pause, after the collection and before the
/// world is released (the verdict is only meaningful before an allocation can
/// reuse a reclaimed address), on EVERY stop-the-world collection of every
/// backend -- `run_collection_pause`, which also calls
/// [`watch_weak_global_referents`] before the collection (gc-common w8-c
/// handoff `handoff-w8c-jni-weak-global-sweep-call-sites`).
/// Reclamation outside a collection pause is covered elsewhere: the
/// concurrent remarks by [`clear_weak_global_refs_unmarked`], and the
/// Generational concurrent sweep and G1 cleanup by the address-keyed sweeps
/// (`addr_keyed::sweep_address_keyed_tables`,
/// `addr_keyed::drop_address_keyed_rows_in`).
///
/// No table lock is held while the heap is asked (the survival predicates
/// take the old-gen lock, and the span drop takes this table's lock UNDER
/// the old-gen lock); the verdicts are applied afterwards, per entry, only
/// if the entry still holds the address that was judged.
///
/// Test-only since gc-common w18 (`handoff-w18a-in-pause-sweeps-take-the-in-place-verdict`):
/// the pause calls [`sweep_weak_global_refs_with`] with its batched
/// `InPlaceVerdict`; this per-address form stays for the tests.
#[cfg(test)]
pub(crate) fn sweep_weak_global_refs(
    shared: &SharedVm,
    pointer_map: &cratonvm_types::PointerMap,
) -> WeakGlobalSweepStats {
    let heap = &shared.mem.heap;
    sweep_weak_global_refs_with(shared, pointer_map, &|addr| {
        crate::memory::addr_keyed::survived_in_place(heap, addr)
    })
}

#[cfg(test)]
thread_local! {
    /// Unit tests switch the indirection per thread: the flag is latched per
    /// process, and tests share one.
    static INDIRECT_LOCALS_TEST_OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
}

#[cfg(test)]
fn indirect_locals_test_override() -> Option<bool> {
    INDIRECT_LOCALS_TEST_OVERRIDE.with(Cell::get)
}

#[cfg(not(test))]
#[inline(always)]
fn indirect_locals_test_override() -> Option<bool> {
    None
}

// ---- Indices 175-181: New<Type>Array ----
macro_rules! new_prim_array {
    ($name:ident, $elem_type:expr) => {
        pub(in crate::native::jni) extern "C" fn $name(_env: JNIEnv, length: JSize) -> JArray {
            // gcd d10/j: allocates without a collection and runs no Java; a
            // raise escalates through `with_jni_context` (see
            // `ForeignJniEntry::enter_vm_only`).
            let _fx = ForeignJniEntry::enter_vm_only();
            if length < 0 {
                raise_jni_negative_array_size(length);
                return 0;
            }
            with_shared_vm(|shared| {
                // gcj j1/jn: HotSpot's length refusal comes first, and runs no
                // collection.
                if jni_array_refused_for_its_length(shared, length, "a primitive array") {
                    return 0;
                }
                // gci i13/oa item 4: sized (G1 humongous label).
                let bytes = crate::runtime::interpreter::gc_and_alloc::native_array_footprint(
                    $elem_type,
                    length as usize,
                );
                match jni_alloc_or_oom_sized(shared, "a primitive array", bytes, |heap| {
                    heap.try_alloc_array_full(ClassId::new(0), $elem_type, length as usize)
                }) {
                    Some(arr) => new_local_handle(arr),
                    None => 0,
                }
            })
            .unwrap_or(0)
        }
    };
}

macro_rules! get_array_elements {
    ($name:ident, $rust_type:ty, $value_variant:ident, $default:expr) => {
        pub(in crate::native::jni) extern "C" fn $name(
            _env: JNIEnv,
            array: JArray,
            is_copy: *mut JBoolean,
        ) -> *mut $rust_type {
            let _fx = ForeignJniEntry::enter();
            if array == 0 {
                return std::ptr::null_mut();
            }
            let result = with_shared_vm(|shared| {
                let oref = jobject_to_obj(array)?;
                // Materialise a detached, native-endian copy via the region-safe
                // per-element accessor (works for ordinary AND G1-humongous
                // arrays). See the module comment above for why a copy — not a
                // direct heap pointer — is what we hand to native code.
                let len = shared.mem.heap.array_length(oref);
                // j27 wave 1 (lane JNINAT, `jr21-jni-empty-array-elements-
                // iscopy-20261014.md`): an EMPTY array answers what HotSpot 25's
                // `DEFINE_GETSCALARARRAYELEMENTS` answers -- a shared non-NULL
                // dummy address and `isCopy = JNI_FALSE` -- with no buffer and no
                // keep-alive global ref. `Release<Type>ArrayElements` finds no
                // record for that address and does nothing, as HotSpot's release
                // does for `length == 0`, so a native that skips the release of a
                // non-copy leaks nothing.
                if len == 0 {
                    return Some((jni_empty_array_elements::<$rust_type>(), JNI_FALSE));
                }
                // `len.max(1)` is kept from when an empty array also got a
                // buffer (harmless now that `len >= 1` here).
                //
                // gc-common w10-c: reserved FALLIBLY (`Vec::with_capacity`
                // aborts the process when the C heap cannot hold the copy).
                // j27 wave 2 (lane NATMISC): a failed reserve answers NULL with
                // NO exception pending, as HotSpot 25's
                // `DEFINE_GETSCALARARRAYELEMENTS` does
                // (`NEW_C_HEAP_ARRAY_RETURN_NULL`, "JNI Specification states
                // return null on OOM", no THROW).
                let mut buf: Vec<$rust_type> = reserve_elements_copy(len.max(1))?;
                for i in 0..len {
                    let val = match shared.mem.heap.get_array_element(oref, i) {
                        Ok(v) => v,
                        Err(_) => break,
                    };
                    let elem = match val {
                        Value::$value_variant(v) => v as $rust_type,
                        _ => $default,
                    };
                    buf.push(elem);
                }
                let ptr = buf.as_mut_ptr();
                // Record (ptr -> (len, cap)) so Release uses the EXACT layout we
                // allocated. `buf.len()` bounds the copy-back (never read an
                // uninitialised tail) and `buf.capacity()` is the size
                // `Vec::from_raw_parts` requires for a sound free.
                let buf_len = buf.len();
                let buf_cap = buf.capacity();
                // Mint a REMAPPABLE keep-alive handle for the source array: a JNI
                // global ref. It keeps the array alive for the whole Get/Release
                // window AND is rewritten by the GC on every relocating
                // collection, so the copy-back at Release follows the array to
                // its current address (see the module comment above). Deleted on
                // the final Release. Recorded in the VM's table, under the same
                // lock, so a Release on ANY thread finds it (gc-common w10-c).
                shared.natives.jni_global_refs.lock().open_elem_copy(
                    ptr as usize,
                    buf_len,
                    buf_cap,
                    oref,
                );
                std::mem::forget(buf); // OWNERSHIP: buffer transferred to native caller, freed by Release<Type>ArrayElements via Vec::from_raw_parts
                Some((ptr, JNI_TRUE))
            })
            .flatten();
            match result {
                Some((ptr, copy_flag)) => {
                    if !is_copy.is_null() {
                        unsafe {
                            *is_copy = copy_flag;
                        }
                    }
                    ptr
                }
                None => std::ptr::null_mut(),
            }
        }
    };
}

// ---- Indices 191-198: Release<Type>ArrayElements ----
// Frees the buffer allocated by Get<Type>ArrayElements and optionally copies back.
macro_rules! release_array_elements {
    ($name:ident, $rust_type:ty, $value_constructor:expr) => {
        pub(in crate::native::jni) extern "C" fn $name(_env: JNIEnv, array: JArray, elems: *mut $rust_type, mode: JInt) {
            let _fx = ForeignJniEntry::enter();
            // The raw `array` handle the caller passes back is deliberately NOT
            // trusted to locate the array: it is a from-space pointer that a
            // moving GC may have invalidated during the Get/Release window. We
            // resolve the array through the remappable global ref recorded at Get
            // (see below), which the GC keeps current across relocations.
            let _ = array;
            if elems.is_null() {
                return;
            }
            // BUG FIX (vm-jni-roots #2): look up the `ArrayElemBuffer` recorded
            // for THIS buffer at Get time. Never re-derive the length from the
            // array handle — the array may have moved/realloc'd under a moving
            // GC, the handle may be aliased/stale, or `array_length` may read 0,
            // any of which made the old code build the copy-back loop and
            // `Vec::from_raw_parts` with a wrong length → heap corruption.
            //
            // For mode != 1 (i.e. modes that free) we `remove` the entry so the
            // pointer can never be double-freed; for JNI_COMMIT (1, no free) we
            // only `get` so a later release can still find it (and its global
            // ref stays alive for that later release).
            //
            // gc-common w10-c: the record is the VM's, not this thread's, so a
            // release on another thread than the Get's finds it. No context
            // (no VM to look in) is a miss, as an unknown pointer is.
            //
            // j27 wave 1 (lane JNINAT): the two effects are HotSpot 25's
            // `DEFINE_RELEASESCALARARRAYELEMENTS` tests, `mode == 0 ||
            // mode == JNI_COMMIT` copies back and `mode == 0 || mode ==
            // JNI_ABORT` frees. A mode outside the three does NEITHER there (the
            // buffer stays outstanding); here it used to copy back AND free.
            let (copy_back, free) = release_elements_effects(mode);
            if !copy_back && !free {
                return;
            }
            let entry = with_shared_vm(|shared| {
                shared
                    .natives
                    .jni_global_refs
                    .lock()
                    .elem_copy(elems as usize, free)
            })
            .flatten();
            let ArrayElemBuffer {
                len: stored_len,
                cap: stored_cap,
                array_gref,
            } = match entry {
                Some(v) => v,
                // Unknown pointer — not one we handed out (or already released).
                // Do nothing rather than risk a wrong-length free.
                None => return,
            };
            // mode 0 = copy back and free, JNI_COMMIT = copy back don't free,
            // JNI_ABORT = free without copy back
            if copy_back {
                // Copy back to the array (mode 0 or JNI_COMMIT=1). Resolve the
                // array's CURRENT address through the remappable global ref — the
                // GC rewrote it to follow any relocation since Get, so the write
                // lands in the live array, not a freed/recycled region. Bound the
                // loop by BOTH our initialised length and the live array length so
                // we neither read past the end of our buffer nor write out of the
                // array's bounds if it has since shrunk.
                with_shared_vm(|shared| {
                    let oref = shared.natives.jni_global_refs.lock().resolve(array_gref)?;
                    let arr_len = shared.mem.heap.array_length(oref);
                    let copy_len = stored_len.min(arr_len);
                    for i in 0..copy_len {
                        let val = unsafe { *elems.add(i) };
                        let value = $value_constructor(val);
                        // Mint chokepoint for the `jlong[]` instantiation
                        // (`SetLongArrayRegion`'s twin); a no-op for the rest.
                        mint_if_smuggled_long_value(&shared.mem.heap, &value);
                        let _ = shared.mem.heap.set_array_element(oref, i, value);
                    }
                    Some(())
                });
            }
            if free {
                // Final (freeing) release (mode 0 or JNI_ABORT=2): delete the
                // remappable global ref (drops the keep-alive root for the array)
                // and free the buffer using the EXACT length and capacity we
                // recorded at allocation time. On JNI_COMMIT (1) both the entry
                // and its global ref intentionally persist for a later release.
                with_shared_vm(|shared| {
                    shared.natives.jni_global_refs.lock().remove(array_gref);
                    Some(())
                });
                unsafe {
                    drop(Vec::from_raw_parts(elems, stored_len, stored_cap));
                }
            }
        }
    };
}

// ---- Indices 199-206: Get<Type>ArrayRegion ----
macro_rules! get_array_region {
    ($name:ident, $rust_type:ty, $value_variant:ident, $default:expr, $elem_type:expr) => {
        pub(in crate::native::jni) extern "C" fn $name(
            _env: JNIEnv,
            array: JArray,
            start: JSize,
            len: JSize,
            buf: *mut $rust_type,
        ) {
            // gcd d5/f: a leaf (primitive reads; an out-of-range window
            // escalates before it raises); see `ForeignJniEntry::enter_leaf`.
            let _fx = ForeignJniEntry::enter_leaf(array);
            if array == 0 || buf.is_null() {
                return;
            }
            with_shared_vm(|shared| {
                let oref = jobject_to_obj(array)?;
                // JNI contract: validate the requested window against the array
                // length BEFORE touching memory; raise AIOOBE on a bad range.
                if !region_bounds_ok(start, len, shared.mem.heap.array_length(oref)) {
                    return None;
                }
                // gcf f2/j: one copy (see `primitive_region_ptr`).
                let stride = std::mem::size_of::<$rust_type>();
                if bulk_array_copies() {
                    if let Some(src) =
                        primitive_region_ptr(shared, oref, $elem_type, start as usize, stride)
                    {
                        // SAFETY: `src` spans `len` elements of the array's
                        // payload (bounds checked above); `buf` is the caller's
                        // buffer of at least `len` elements (JNI contract); the
                        // two cannot overlap (heap vs native memory).
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                src as *const u8,
                                buf as *mut u8,
                                len as usize * stride,
                            );
                        }
                        return Some(());
                    }
                }
                for i in 0..len as usize {
                    let val = shared
                        .mem
                        .heap
                        .get_array_element(oref, start as usize + i)
                        .ok()?;
                    let elem = match val {
                        Value::$value_variant(v) => v as $rust_type,
                        _ => $default,
                    };
                    unsafe {
                        *buf.add(i) = elem;
                    }
                }
                Some(())
            });
        }
    };
}

// ---- Indices 207-214: Set<Type>ArrayRegion ----
//
// gcf f2/j: `$elem_type` `None` keeps the element walk always (`boolean[]`).
macro_rules! set_array_region {
    ($name:ident, $rust_type:ty, $value_constructor:expr, $elem_type:expr) => {
        pub(in crate::native::jni) extern "C" fn $name(
            _env: JNIEnv,
            array: JArray,
            start: JSize,
            len: JSize,
            buf: *const $rust_type,
        ) {
            // gcd d5/f: a leaf (primitive stores, which need no barrier; an
            // out-of-range window escalates before it raises); see
            // `ForeignJniEntry::enter_leaf`.
            let _fx = ForeignJniEntry::enter_leaf(array);
            if array == 0 || buf.is_null() {
                return;
            }
            with_shared_vm(|shared| {
                let oref = jobject_to_obj(array)?;
                // JNI contract: validate the requested window against the array
                // length BEFORE writing; raise AIOOBE on a bad range so no
                // partial / out-of-range store is performed.
                if !region_bounds_ok(start, len, shared.mem.heap.array_length(oref)) {
                    return None;
                }
                // gcf f2/j: one copy (see `primitive_region_ptr`).
                let stride = std::mem::size_of::<$rust_type>();
                let want: Option<ArrayElementType> = $elem_type;
                if let Some(want) = want.filter(|_| bulk_array_copies()) {
                    if let Some(dst) =
                        primitive_region_ptr(shared, oref, want, start as usize, stride)
                    {
                        // SAFETY: as in `get_array_region!`, the other way.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                buf as *const u8,
                                dst,
                                len as usize * stride,
                            );
                        }
                        return Some(());
                    }
                }
                for i in 0..len as usize {
                    let val = unsafe { *buf.add(i) };
                    let _ = shared.mem.heap.set_array_element(
                        oref,
                        start as usize + i,
                        $value_constructor(val),
                    );
                }
                Some(())
            });
        }
    };
}

// ---------------------------------------------------------------------------
// Bare-varargs Call<Type>Method(...) trampolines
// ---------------------------------------------------------------------------
//
// The `...` forms are a third of the JNI call surface and cannot be WRITTEN in
// stable Rust — defining a C-variadic function is still unstable (rust-lang
// #44930). They can, however, be ENTERED in assembly: a variadic callee's only
// extra obligation over an ordinary one is to materialise the `va_list` its ABI
// describes, and once it has, the `...MethodV` implementation already in this
// file does the rest. So every `...` slot gets a small trampoline that builds
// the platform `va_list` and hands off to its `V` sibling.
//
// `call` then `leave`/`ret`, not a tail `jmp`: the `va_list` and its register
// save area live in THIS frame and must outlive the callee. The return value
// needs no handling — RAX and XMM0 pass straight through — which is also why
// one trampoline shape serves `jint`, `jlong`, `jobject`, `jdouble` and `void`
// alike.
//
// Two named-argument counts cover every slot:
//   3 named — `(env, obj|clazz, methodID, ...)`: NewObject, Call<T>Method,
//             CallStatic<T>Method
//   4 named — `(env, obj, clazz, methodID, ...)`: CallNonvirtual<T>Method
//
// Anything that is not x86-64 keeps `jni_varargs_unsupported`, which raises
// UnsatisfiedLinkError rather than fabricating a 0/null.

/// System V x86-64: spill the six GP and eight SSE argument registers into a
/// 176-byte register-save area, then hand the callee a `__va_list_tag` whose
/// `gp_offset` already skips the named arguments.
///
/// Frame after `push rbp; sub rsp,224` — RSP stays 16-aligned, which `movaps`
/// requires:
///   `[rsp    .. rsp+23 ]` the `__va_list_tag`
///   `[rsp+32 .. rsp+207]` the register-save area (6x8 GP, then 8x16 SSE)
///
/// `AL` carries the caller's count of SSE argument registers used, so the
/// `movaps` block is skipped entirely for the common all-integer call.
#[cfg(all(target_arch = "x86_64", not(target_os = "windows")))]
macro_rules! jni_varargs_trampoline {
    (@n 3, $sym:ident, $target:path) => {
        jni_varargs_trampoline!(@emit $sym, $target, "24", "lea rcx, [rsp]", "");
    };
    (@n 4, $sym:ident, $target:path) => {
        jni_varargs_trampoline!(@emit $sym, $target, "32", "mov rcx, [rsp+56]", "lea r8, [rsp]");
    };
    (@emit $sym:ident, $target:path, $gp_off:literal, $tail1:literal, $tail2:literal) => {
        core::arch::global_asm!(
            ".p2align 4",
            concat!(".globl ", stringify!($sym)),
            concat!(stringify!($sym), ":"),
            "push rbp",
            "mov rbp, rsp",
            "sub rsp, 224",
            "mov [rsp+32], rdi",
            "mov [rsp+40], rsi",
            "mov [rsp+48], rdx",
            "mov [rsp+56], rcx",
            "mov [rsp+64], r8",
            "mov [rsp+72], r9",
            "test al, al",
            "je 1f",
            "movaps [rsp+80], xmm0",
            "movaps [rsp+96], xmm1",
            "movaps [rsp+112], xmm2",
            "movaps [rsp+128], xmm3",
            "movaps [rsp+144], xmm4",
            "movaps [rsp+160], xmm5",
            "movaps [rsp+176], xmm6",
            "movaps [rsp+192], xmm7",
            "1:",
            concat!("mov dword ptr [rsp], ", $gp_off), // gp_offset skips the named args
            "mov dword ptr [rsp+4], 48",               // fp_offset: start of the SSE area
            "lea rax, [rbp+16]",                       // overflow_arg_area = 1st stack arg
            "mov [rsp+8], rax",
            "lea rax, [rsp+32]",                       // reg_save_area
            "mov [rsp+16], rax",
            "mov rdi, [rsp+32]",                       // re-load the named args
            "mov rsi, [rsp+40]",
            "mov rdx, [rsp+48]",
            $tail1,
            $tail2,
            "call {target}",
            "leave",
            "ret",
            target = sym $target,
        );
    };
}

/// Windows x64: every argument, named or variadic, arrives in RCX/RDX/R8/R9
/// then on the stack, and the caller always reserved the 32-byte home area at
/// `[rsp+8]`. Spilling the four register arguments into their home slots makes
/// the whole argument list one contiguous run of 8-byte slots — which IS the
/// Windows `va_list`. A `double` vararg is passed in both its GP register and
/// its XMM register, so the GP spill carries the bits.
#[cfg(all(target_arch = "x86_64", target_os = "windows"))]
macro_rules! jni_varargs_trampoline {
    (@n 3, $sym:ident, $target:path) => {
        // 3 named: RCX/RDX/R8 already hold them; the va_list is the 4th arg.
        jni_varargs_trampoline!(@emit $sym, $target, "lea rax, [rsp+32]",
                                "sub rsp, 32", "mov r9, rax");
    };
    (@n 4, $sym:ident, $target:path) => {
        // 4 named: all four registers are named, so the va_list is the 5th arg
        // and goes on the stack just above the 32-byte shadow space.
        jni_varargs_trampoline!(@emit $sym, $target, "lea rax, [rsp+40]",
                                "sub rsp, 48", "mov [rsp+32], rax");
    };
    (@emit $sym:ident, $target:path, $lea:literal, $tail1:literal, $tail2:literal) => {
        core::arch::global_asm!(
            ".p2align 4",
            concat!(".globl ", stringify!($sym)),
            concat!(stringify!($sym), ":"),
            "mov [rsp+8], rcx",  // home slots, in the caller's shadow space
            "mov [rsp+16], rdx",
            "mov [rsp+24], r8",
            "mov [rsp+32], r9",
            $lea,                // rax = &first variadic argument
            "push rbp",
            "mov rbp, rsp",
            $tail1,              // shadow space (+ the 5th arg slot when needed)
            $tail2,
            "call {target}",
            "mov rsp, rbp",
            "pop rbp",
            "ret",
            target = sym $target,
        );
    };
}

#[cfg(target_arch = "x86_64")]
macro_rules! jni_varargs_trampolines {
    ($( $sym:ident => ($target:path, $named:tt) ),* $(,)?) => {
        $( jni_varargs_trampoline!(@n $named, $sym, $target); )*
        extern "C" {
            $(
                /// Assembly trampoline; only ever used for its ADDRESS.
                fn $sym();
            )*
        }
    };
}

// ---- Index 215: RegisterNatives ----

mod jni_platform_rebind;
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------



#[cfg(test)]
mod gci_i47w_oz47_delete_local_ref_leaf_tests;

// JIT round 21 wave 20 (lane jniharness, JNI6-1): the in-process JNI
// conformance harness -- fixture classes assembled in Rust, natives bound with
// `RegisterNatives`, and the function table driven through the `JNIEnv` a
// native is handed, under each in-native arm.
#[cfg(test)]
mod jni_conformance_tests;

mod j_boolean;
#[allow(unused_imports)]
pub use j_boolean::*;
#[allow(unused_imports)]
pub(in crate::native::jni) use j_boolean::*;
mod raw_locals_counted;
#[allow(unused_imports)]
pub use raw_locals_counted::*;
#[allow(unused_imports)]
pub(in crate::native::jni) use raw_locals_counted::*;
mod truncate_local_frames;
#[allow(unused_imports)]
pub use truncate_local_frames::*;
#[allow(unused_imports)]
pub(in crate::native::jni) use truncate_local_frames::*;
mod jni_empty_array_elements;
#[allow(unused_imports)]
pub use jni_empty_array_elements::*;
#[allow(unused_imports)]
pub(in crate::native::jni) use jni_empty_array_elements::*;
mod lambda_proxy_jni_construct;
#[allow(unused_imports)]
pub use lambda_proxy_jni_construct::*;
#[allow(unused_imports)]
pub(in crate::native::jni) use lambda_proxy_jni_construct::*;
