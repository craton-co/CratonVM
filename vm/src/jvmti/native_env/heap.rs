// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Object tags and the heap walks of the C table (interpreter round i2 wave
//! 63, lane L1; the i51-L1 page's "Next step (from wave 61)", item 4): 106
//! `GetTag`, 107 `SetTag`, 114 `GetObjectsWithTags`, 108
//! `ForceGarbageCollection`, 116 `IterateThroughHeap`, 115
//! `FollowReferences` and the JVMTI 1.0 walks 109-112, all with
//! `can_tag_objects`, each with HotSpot 25's checks and answers (MEASURED,
//! `tools/probes/interp/L1/L1W63JvmtiTagsAndHeap.java`).
//!
//! A tag is per env. It lives in the env's [`TagTable`] as a JNI weak global
//! reference, which the collectors keep current and clear when the object
//! dies; the table's address index is rebuilt after each collection, and the
//! tags of the objects that died are handed back for `ObjectFree`
//! (`events_more::post_object_free`).
//!
//! The walks run in a non-moving stop-the-world pause
//! (`interpreter::NonMovingPause`, the heap dump's), so the agent's callbacks
//! run with every other thread stopped and nothing moving, as HotSpot runs
//! them in a VM operation: they may call no JNI function, as the
//! specification says. A tag a callback writes through its `tag_ptr` is
//! stored when it returns.

use std::collections::{HashMap, HashSet};

use super::*;
use crate::memory::heap::{ArrayElementType, ObjectKind};

/// Tags keyed by JNI weak global references (see the module doc).
#[derive(Default)]
pub(crate) struct TagTable {
    entries: Vec<(JObject, i64)>,
    /// Object address to its entry, valid for address-memo `epoch`
    /// (`HeapRealm::address_epoch`).
    // addr-keyed: rebuilt from the weak refs per address epoch; verified per lookup when `exact`.
    index: HashMap<usize, usize>,
    epoch: Option<u64>,
    /// The heap may move an object without its collection count changing
    /// (i87-L1, interpreter round i4 wave 92): the index is then a hint, an
    /// index hit is confirmed against its weak reference and a miss (or a
    /// refuted hit) rebuilds the index before it answers. Set at each
    /// [`Self::refresh`] from [`objects_move_between_counts`].
    exact: bool,
}

/// Can `heap` move an object while the collection count it reports stays the
/// same (i87-L1, wave 92)? The generational backend and G1 move objects only
/// inside a pause that ends with the count incremented; every other backend
/// (ZGC, whose relocation is meant to run off the safepoint and whose count
/// is bumped only at the end of a cycle) is not trusted to, so its tag
/// lookups resolve the weak references instead of trusting the index.
fn objects_move_between_counts(heap: &crate::memory::vm_heap::VmHeap) -> bool {
    !heap.is_g1() && !heap.is_generational()
}

/// Where the object a tag's weak reference names lives now; `None` once it
/// was collected.
fn resolve_addr(shared: &SharedVm, weak: JObject) -> Option<usize> {
    shared
        .natives
        .jni_global_refs
        .lock()
        .resolve(weak)
        .map(|obj| obj.as_ptr() as usize)
}

impl TagTable {
    /// Bring the address index up to date with the heap's collections: an
    /// entry whose object died is dropped, and its tag returned (for
    /// `ObjectFree`).
    ///
    /// gci i3/wr: keyed on the address-memo epoch, not the collection count.
    /// G1's `cleanup` and every Generational concurrent sweep slice free
    /// blocks without booking a collection, so a count-keyed index answered a
    /// dead object's tag for a new object placed at its address and delayed
    /// its `ObjectFree` to the next collection.
    fn refresh(&mut self, shared: &SharedVm) -> Vec<i64> {
        self.exact = objects_move_between_counts(&shared.mem.heap);
        let epoch = shared.mem.address_epoch();
        if self.epoch == Some(epoch) {
            return Vec::new();
        }
        let mut freed = Vec::new();
        let mut refs = shared.natives.jni_global_refs.lock();
        let mut kept = Vec::with_capacity(self.entries.len());
        self.index.clear();
        for (weak, tag) in self.entries.drain(..) {
            match refs.resolve(weak) {
                Some(obj) => {
                    self.index.insert(obj.as_ptr() as usize, kept.len());
                    kept.push((weak, tag));
                }
                None => {
                    refs.remove(weak);
                    freed.push(tag);
                }
            }
        }
        self.entries = kept;
        self.epoch = Some(epoch);
        freed
    }

    /// The entry of the object at `addr`. When the heap may move objects
    /// between collection counts ([`Self::exact`]) an index hit is confirmed
    /// by resolving its weak reference, and a miss or a refuted hit rebuilds
    /// the index from the weak references ([`Self::reindex`]) before
    /// answering: O(tags) per untagged lookup on such a heap only.
    fn entry_of(
        &mut self,
        addr: usize,
        mut resolve: impl FnMut(JObject) -> Option<usize>,
    ) -> Option<usize> {
        // gci i20/mr finding 8: an index entry past the end is stale (`set`'s
        // untag moved the last entry and could not resolve its address to
        // re-key it, which only a heap that frees between epochs allows); it
        // is a miss, never an index into `entries`.
        let hit = self
            .index
            .get(&addr)
            .copied()
            .filter(|&i| i < self.entries.len());
        if !self.exact {
            return hit;
        }
        if let Some(i) = hit {
            if resolve(self.entries[i].0) == Some(addr) {
                return Some(i);
            }
        }
        self.reindex(&mut resolve);
        self.index.get(&addr).copied()
    }

    /// Rebuild the address index from the weak references, without dropping
    /// an entry (a cleared one is left for [`Self::refresh`] to free, so its
    /// `ObjectFree` is posted once).
    fn reindex(&mut self, resolve: &mut impl FnMut(JObject) -> Option<usize>) {
        self.index.clear();
        for (i, &(weak, _)) in self.entries.iter().enumerate() {
            if let Some(addr) = resolve(weak) {
                self.index.insert(addr, i);
            }
        }
    }

    /// [`Self::refresh`], then, inside a walk's non-moving pause on a heap
    /// that moves objects between counts, the index rebuilt so the walk's
    /// [`Self::snapshot`] names where the objects are now.
    fn refresh_in_pause(&mut self, shared: &SharedVm) -> Vec<i64> {
        let freed = self.refresh(shared);
        if self.exact {
            self.reindex(&mut |weak| resolve_addr(shared, weak));
        }
        freed
    }

    fn get(&mut self, shared: &SharedVm, obj: ObjectRef) -> i64 {
        self.entry_of(obj.as_ptr() as usize, |weak| resolve_addr(shared, weak))
            .map_or(0, |i| self.entries[i].1)
    }

    fn set(&mut self, shared: &SharedVm, obj: ObjectRef, tag: i64) {
        let addr = obj.as_ptr() as usize;
        match self.entry_of(addr, |weak| resolve_addr(shared, weak)) {
            Some(i) if tag != 0 => self.entries[i].1 = tag,
            Some(i) => {
                let (weak, _) = self.entries.swap_remove(i);
                shared.natives.jni_global_refs.lock().remove(weak);
                self.index.remove(&addr);
                if let Some((moved_weak, _)) = self.entries.get(i) {
                    if let Some(moved) = shared.natives.jni_global_refs.lock().resolve(*moved_weak)
                    {
                        self.index.insert(moved.as_ptr() as usize, i);
                    }
                }
            }
            None if tag != 0 => {
                let weak = shared.natives.jni_global_refs.lock().add_weak(obj);
                self.index.insert(addr, self.entries.len());
                self.entries.push((weak, tag));
            }
            None => {}
        }
    }

    /// The live objects tagged with one of `tags`, with their tags.
    fn with_tags(&self, shared: &SharedVm, tags: &[i64]) -> Vec<(ObjectRef, i64)> {
        let refs = shared.natives.jni_global_refs.lock();
        self.entries
            .iter()
            .filter(|(_, tag)| tags.contains(tag))
            .filter_map(|&(weak, tag)| refs.resolve(weak).map(|obj| (obj, tag)))
            .collect()
    }

    /// Address to tag, for a walk.
    fn snapshot(&self) -> HashMap<usize, i64> {
        // gci i20/mr finding 8: a stale entry past the end is skipped.
        self.index
            .iter()
            .filter_map(|(&addr, &i)| self.entries.get(i).map(|&(_, tag)| (addr, tag)))
            .collect()
    }
}

/// The env's tag table brought up to date (see [`TagTable::refresh`]), the
/// freed tags posted as `ObjectFree`, then `work` on it.
fn with_tags<R>(
    env: &JvmtiNativeEnv,
    shared: &SharedVm,
    work: impl FnOnce(&mut TagTable) -> R,
) -> R {
    let (freed, out) = refreshed_tags(env, shared, work);
    post_freed_tags(env, &freed);
    out
}

/// gci i9/hj: [`with_tags`] without the `ObjectFree` posting: the table
/// brought up to date and `work` on it, under the env's lock, and the freed
/// tags handed back for the caller to post once it is back in native
/// ([`post_freed_tags`]). The caller may hold a JNI entry around it.
fn refreshed_tags<R>(
    env: &JvmtiNativeEnv,
    shared: &SharedVm,
    work: impl FnOnce(&mut TagTable) -> R,
) -> (Vec<i64>, R) {
    let mut state = env.state.lock();
    let freed = state.tags.refresh(shared);
    let out = work(&mut state.tags);
    (freed, out)
}

/// `ObjectFree` for `freed`, if any (see [`refreshed_tags`]).
fn post_freed_tags(env: &JvmtiNativeEnv, freed: &[i64]) {
    if !freed.is_empty() {
        super::events_more::post_object_free(env.addr(), freed);
    }
}

/// gci i9/hj (`gci-i8hd-jvmti-tag-functions-and-oome-dump-outside-a-pause-FIXED-20261017`
/// item 1): `work` on the env's up-to-date tag table and the object `handle`
/// names, with the thread out of native ([`jni::ForeignJniEntry`]) across the
/// decode AND the table operation, so no collection completes between them.
/// The decode used to drop its entry on return: a moving collection that ran
/// before `with_tags` left `GetTag` looking up (and `SetTag` adding a weak
/// global for) the vacated address. `None` for NULL or a handle naming
/// nothing. The tags the refresh freed are posted as `ObjectFree` after the
/// entry is dropped, back where the caller was, as before (the `ObjectFree`
/// callback does not run inside the entry).
///
/// gci i48 lane jl48 (`ja11_w_tag`, i10al item 4): the entry is
/// [`jni::ForeignJniEntry::enter_vm_only`]. `GetTag` / `SetTag` run no Java,
/// cannot collect, write no `JvmThread` field and mint no local: they decode
/// a handle and read or edit the env's table (a weak global added or
/// removed; weak globals are not roots). From a JNI native in native the call
/// then goes back into native incrementally, not with a full deposit per tag.
fn with_object_tags<R>(
    env: &JvmtiNativeEnv,
    shared: &SharedVm,
    handle: JObject,
    work: impl FnOnce(&mut TagTable, ObjectRef) -> R,
) -> Option<R> {
    if handle == 0 {
        return None;
    }
    let (freed, out) = {
        let _fx = jni::ForeignJniEntry::enter_vm_only();
        let prev = jni::replace_jni_context(shared);
        let obj = jni::jobject_to_obj(handle);
        jni::restore_jni_context(prev);
        let obj = obj?;
        refreshed_tags(env, shared, |tags| work(tags, obj))
    };
    post_freed_tags(env, &freed);
    Some(out)
}

/// Bring every C env of `shared` up to date after a collection, posting
/// `ObjectFree` for the objects that died (`ForceGarbageCollection`, and the
/// collector's finish hook through `events_more`).
pub(super) fn sweep_all_tags(shared: &SharedVm) {
    let envs = shared.debug.jvmti_env.lock().c_table.native_envs.clone();
    for addr in envs {
        // SAFETY: an address `new_env_for_version` handed out.
        let env = unsafe { env_at(addr) };
        if env.state.lock().disposed {
            continue;
        }
        with_tags(env, shared, |_| ());
    }
}

/// The checks of the tag functions: the env, the start or live phase,
/// `can_tag_objects`.
fn tag_env<'a>(env: *mut JvmtiNativeEnv) -> Result<(&'a JvmtiNativeEnv, Arc<SharedVm>), JInt> {
    let e = live_env(env)?;
    let shared = e.vm().ok_or(ERR_WRONG_PHASE)?;
    let gates = &shared.debug.debugger_gates;
    if gates.jvmti_outside_live() && !gates.jvmti_start_phase() {
        return Err(ERR_WRONG_PHASE);
    }
    if !has_cap(&e.state.lock().capabilities, CAP_TAG_OBJECTS) {
        return Err(ERR_MUST_POSSESS_CAPABILITY);
    }
    Ok((e, shared))
}

/// The object a handle names, in `shared`'s JNI context; `None` for NULL or
/// a handle that names nothing.
fn object_of(shared: &SharedVm, handle: JObject) -> Option<ObjectRef> {
    if handle == 0 {
        return None;
    }
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(shared);
    let obj = jni::jobject_to_obj(handle);
    jni::restore_jni_context(prev);
    obj
}

// Interpreter round i4 wave 87, lane L1 (dev) fixed the same GetTag / SetTag
// decode-then-lookup race with a `with_object_of` helper; the gci i9/hj
// `with_object_tags` above covers it (one entry across decode and lookup) and
// also posts `ObjectFree` outside the entry, so the merge of 2026-10-15 keeps
// that one.

/// The object `handle` names, read inside a walk's non-moving pause (wave
/// 87): the address [`object_of`] answered before the pause may have moved
/// by the time the pause began.
fn object_in_pause(shared: &SharedVm, handle: JObject) -> Option<ObjectRef> {
    if handle == 0 {
        return None;
    }
    let prev = jni::replace_jni_context(shared);
    let obj = jni::jobject_to_obj(handle);
    jni::restore_jni_context(prev);
    obj
}

/// Slot 106: `GetTag(env, object, tag_ptr)`: 0 for an untagged object
/// (MEASURED). A NULL `tag_ptr` is checked before the object (MEASURED).
pub(super) extern "C" fn get_tag(
    env: *mut JvmtiNativeEnv,
    object: JObject,
    tag_ptr: *mut i64,
) -> JInt {
    let (e, shared) = match tag_env(env) {
        Ok(found) => found,
        Err(code) => return code,
    };
    if tag_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some(tag) = with_object_tags(e, &shared, object, |tags, obj| tags.get(&shared, obj)) else {
        return ERR_INVALID_OBJECT;
    };
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *tag_ptr = tag };
    ERR_NONE
}

/// Slot 107: `SetTag(env, object, tag)`: 0 removes the tag.
pub(super) extern "C" fn set_tag(env: *mut JvmtiNativeEnv, object: JObject, tag: i64) -> JInt {
    let (e, shared) = match tag_env(env) {
        Ok(found) => found,
        Err(code) => return code,
    };
    if with_object_tags(e, &shared, object, |tags, obj| tags.set(&shared, obj, tag)).is_none() {
        return ERR_INVALID_OBJECT;
    }
    ERR_NONE
}

/// Slot 114: `GetObjectsWithTags(env, tag_count, tags, count_ptr,
/// object_result_ptr, tag_result_ptr)`: the live objects carrying one of
/// the tags, each result array optional. HotSpot's checks in its order
/// (MEASURED): a negative count 103, NULL tags 100, a NULL count 100; a tag
/// of 0 is accepted (no object carries it).
pub(super) extern "C" fn get_objects_with_tags(
    env: *mut JvmtiNativeEnv,
    tag_count: JInt,
    tags: *const i64,
    count_ptr: *mut JInt,
    object_result_ptr: *mut *mut JObject,
    tag_result_ptr: *mut *mut i64,
) -> JInt {
    let (e, shared) = match tag_env(env) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let Ok(n) = usize::try_from(tag_count) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if tags.is_null() || count_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's array of `tag_count` tags, non-null.
    let wanted = unsafe { std::slice::from_raw_parts(tags, n) }.to_vec();
    // gci i9/hj (`gci-i8hd-jvmti-tag-functions-and-oome-dump-outside-a-pause-FIXED-20261017`
    // item 2): out of native FIRST, then resolve the weak globals and mint the
    // local handles under the same entry. The weak globals used to be resolved
    // in native and the entry taken after: a collection the entry waited out
    // left every handle naming a vacated address. `ObjectFree` for what the
    // refresh freed is posted after the entry, back in native. (Wave 87, lane
    // L1, made the same entry-first fix on dev.)
    let (freed, (found, handles)) = {
        let _fx = jni::ForeignJniEntry::enter();
        let resolved = refreshed_tags(e, &shared, |table| table.with_tags(&shared, &wanted));
        let prev = jni::replace_jni_context(&shared);
        let (freed, found) = resolved;
        let handles: Vec<JObject> = found
            .iter()
            .map(|&(obj, _)| jni::new_local_handle(obj))
            .collect();
        jni::restore_jni_context(prev);
        (freed, (found, handles))
    };
    post_freed_tags(e, &freed);
    let tag_list: Vec<i64> = found.iter().map(|&(_, tag)| tag).collect();
    let Ok(count) = JInt::try_from(found.len()) else {
        return ERR_INTERNAL;
    };
    let mut ignored: JInt = 0;
    if !object_result_ptr.is_null() {
        let code = write_id_array(&mut ignored, object_result_ptr, &handles);
        if code != ERR_NONE {
            return code;
        }
    }
    if !tag_result_ptr.is_null() {
        let code = write_id_array(&mut ignored, tag_result_ptr, &tag_list);
        if code != ERR_NONE {
            return code;
        }
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *count_ptr = count };
    ERR_NONE
}

/// Slot 108: `ForceGarbageCollection(env)` (live phase): a full collection
/// (the VM's own, not the `System.gc()` door: gci i15/rb), then every env's tags brought up to date
/// (MEASURED: a tag of an object no longer reachable is gone afterwards).
pub(super) extern "C" fn force_garbage_collection(env: *mut JvmtiNativeEnv) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    // gci i15/rb (`gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`
    // finding 1): the collection runs out of native, as every other JVMTI
    // function that builds a context does (`on_calling_thread`: the JNI entry
    // first), and as HotSpot's `ThreadInVMfromNative` runs it. A caller in
    // native -- a `RunAgentThread` start function since gci i15/hx, a JNI
    // native's C code with `CRATONVM_JNI_NATIVE_TRANSITIONS` -- used to request
    // the pause with its `in_blocked_region` up and then run the forced
    // finalizer and cleaner drains (Java) still flagged in native, i.e.
    // excluded from every other thread's pause with only its stale deposit as
    // its roots. Inert for a counted caller (a JVMTI callback on a Java
    // thread): the entry does nothing there. ObjectFree is posted after, in
    // native, as before.
    //
    // Finding 4: and the VM's own full collection (`force_gc_for_vm`), not the
    // `System.gc()` door: HotSpot's `JvmtiEnv::ForceGarbageCollection` is
    // `collect(GCCause::_jvmti_force_gc)`, which `-XX:+DisableExplicitGC` does
    // not turn off (only `JVM_GC` reads it), which commits no `jdk.SystemGC`
    // event, and which G1 runs as a full STW collection even under
    // `-XX:+ExplicitGCInvokesConcurrent` (not a user-requested cause).
    // gci i27/ob (finding 4's residual): on its own door, so the collection
    // reports HotSpot's cause, `JvmtiEnv ForceGarbageCollection`.
    //
    // zs s8/jf2 (merge of origin/dev, 2026-10-18): ONE door for both sides.
    // `ctx.force_gc_for_jvmti()` is `gc_and_alloc::force_gc_for_jvmti`, which
    // also carries the round's ZGC behaviour (HotSpot's `_jvmti_force_gc`
    // cause on the collection only, no `jdk.SystemGC`, not gated by
    // `-XX:+DisableExplicitGC`) and its kill switch
    // `CRATONVM_ZGC_JVMTI_FORCE_GC_DOOR=0` (ZGC: the `System.gc()` door).
    if let Err(code) = on_calling_thread(&shared, |ctx| ctx.force_gc_for_jvmti()) {
        return code;
    }
    sweep_all_tags(&shared);
    ERR_NONE
}

// --- The walk ---------------------------------------------------------------

/// `jvmtiHeapCallbacks`.
#[repr(C)]
pub(super) struct HeapCallbacks {
    heap_iteration: usize,
    heap_reference: usize,
    primitive_field: usize,
    array_primitive_value: usize,
    string_primitive_value: usize,
    reserved: [usize; 10],
}

type HeapIterationCallback = unsafe extern "C" fn(i64, i64, *mut i64, JInt, usize) -> JInt;
type HeapReferenceCallback = unsafe extern "C" fn(
    JInt,
    *const ReferenceInfo,
    i64,
    i64,
    i64,
    *mut i64,
    *mut i64,
    JInt,
    usize,
) -> JInt;
type PrimitiveFieldCallback =
    unsafe extern "C" fn(JInt, *const ReferenceInfo, i64, *mut i64, u64, JInt, usize) -> JInt;
type ArrayPrimitiveValueCallback =
    unsafe extern "C" fn(i64, i64, *mut i64, JInt, JInt, *const u8, usize) -> JInt;
type StringPrimitiveValueCallback =
    unsafe extern "C" fn(i64, i64, *mut i64, *const u16, JInt, usize) -> JInt;

/// `jvmtiHeapReferenceInfo`, the 64-byte union: the field, array and
/// constant pool members are its first `jint`; the stack members fill it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct ReferenceInfo {
    words: [i64; 8],
}

impl ReferenceInfo {
    fn index(index: JInt) -> Self {
        let mut info = Self::default();
        info.words[0] = i64::from(index) & 0xffff_ffff;
        info
    }

    /// `jvmtiHeapReferenceInfoStackLocal`: `{ jlong thread_tag; jlong
    /// thread_id; jint depth; jmethodID method; jlocation location; jint
    /// slot; }`.
    fn stack(
        thread_tag: i64,
        thread_id: i64,
        depth: JInt,
        method: u64,
        location: i64,
        slot: JInt,
    ) -> Self {
        let mut info = Self::default();
        info.words[0] = thread_tag;
        info.words[1] = thread_id;
        info.words[2] = i64::from(depth) & 0xffff_ffff;
        info.words[3] = method as i64; // Cast: the `jmethodID`'s bits
        info.words[4] = location;
        info.words[5] = i64::from(slot) & 0xffff_ffff;
        info
    }
}

// `jvmtiHeapReferenceKind`.
const REF_CLASS: JInt = 1;
const REF_FIELD: JInt = 2;
const REF_ARRAY_ELEMENT: JInt = 3;
const REF_CLASS_LOADER: JInt = 4;
const REF_SIGNERS: JInt = 5;
const REF_PROTECTION_DOMAIN: JInt = 6;
const REF_INTERFACE: JInt = 7;
const REF_STATIC_FIELD: JInt = 8;
const REF_CONSTANT_POOL: JInt = 9;
const REF_SUPERCLASS: JInt = 10;
const REF_JNI_GLOBAL: JInt = 21;
const REF_SYSTEM_CLASS: JInt = 22;
const REF_STACK_LOCAL: JInt = 24;
const REF_THREAD: JInt = 26;
const REF_OTHER: JInt = 27;

// Visit control and filters.
const VISIT_OBJECTS: JInt = 0x100;
const VISIT_ABORT: JInt = 0x8000;
const FILTER_TAGGED: JInt = 0x4;
const FILTER_UNTAGGED: JInt = 0x8;
const FILTER_CLASS_TAGGED: JInt = 0x10;
const FILTER_CLASS_UNTAGGED: JInt = 0x20;

/// One reference a walk reports: its kind, its info, the referee.
struct Reference {
    kind: JInt,
    info: ReferenceInfo,
    target: ObjectRef,
}

/// A class filter of a walk, as HotSpot reads the `jclass`: none (NULL), a
/// class, or a handle that names no class (MEASURED: an object handed as
/// the class is not refused; nothing is reported).
#[derive(Clone, Copy, PartialEq, Eq)]
enum KlassFilter {
    Any,
    Exactly(ClassId),
    Nothing,
}

/// What a walk needs, inside its pause.
struct Walk<'a> {
    shared: &'a SharedVm,
    cm: &'a crate::classloading::ClassManager,
    tags: HashMap<usize, i64>,
    /// Array class ids by (component class or `None` for a primitive, the
    /// element type).
    array_classes: HashMap<(Option<ClassId>, u8), Option<ClassId>>,
    /// The `jvmtiHeapReferenceInfoField` index base of each class's fields.
    field_bases: HashMap<ClassId, usize>,
    /// Class loader object address to the mirrors of the classes it
    /// defined, made the first time the walk needs it.
    loaders: Option<HashMap<usize, Vec<ObjectRef>>>,
    sizer: &'a dyn Fn(ObjectRef) -> i64,
}

impl<'a> Walk<'a> {
    fn tag(&self, obj: ObjectRef) -> i64 {
        self.tags
            .get(&(obj.as_ptr() as usize))
            .copied()
            .unwrap_or(0)
    }

    fn set_tag(&mut self, obj: ObjectRef, tag: i64) {
        let addr = obj.as_ptr() as usize;
        if tag == 0 {
            self.tags.remove(&addr);
        } else {
            self.tags.insert(addr, tag);
        }
    }

    /// The class of `obj`: its own for an instance, the array class for an
    /// array (`None` when this VM never made it).
    fn class_of_object(&mut self, obj: ObjectRef) -> Option<ClassId> {
        let header = self.shared.mem.heap.get_header(obj);
        if header.kind() != ObjectKind::Array {
            return Some(header.class_id);
        }
        let element = self.shared.mem.heap.array_element_type(obj)?;
        let component = (element == ArrayElementType::Reference).then_some(header.class_id);
        let key = (component, element as u8); // Cast: the enum's discriminant
        if let Some(&known) = self.array_classes.get(&key) {
            return known;
        }
        let found = crate::debug::inspect::array_descriptor(self.shared, &self.cm, obj).and_then(
            |(desc, comp)| {
                let loader = comp
                    .and_then(|c| self.cm.get_class(c))
                    .map_or(crate::classloading::ClassLoaderId::Bootstrap, |c| {
                        c.loader_id
                    });
                self.cm.find_class_by_name_for_loader(&desc, loader)
            },
        );
        self.array_classes.insert(key, found);
        found
    }

    /// The mirror of class `class_id`, when this VM made one.
    fn mirror_of(&self, class_id: ClassId) -> Option<ObjectRef> {
        self.shared
            .classes
            .class_mirrors
            .read()
            .get(&class_id)
            .copied()
    }

    /// The tag of `obj`'s class's mirror.
    fn class_tag(&mut self, obj: ObjectRef) -> i64 {
        self.class_of_object(obj)
            .and_then(|c| self.mirror_of(c))
            .map_or(0, |m| self.tag(m))
    }

    /// The class a mirror stands for, `None` for an object that is no
    /// mirror or a primitive class's.
    fn mirrored_class(&self, obj: ObjectRef) -> Option<ClassId> {
        self.shared
            .classes
            .class_mirrors_reverse
            .read()
            .get(&obj)
            .copied()
    }

    /// Is `obj` filtered out by `heap_filter` and the class filter?
    fn filtered(&mut self, obj: ObjectRef, heap_filter: JInt, klass: KlassFilter) -> bool {
        match klass {
            KlassFilter::Any => {}
            KlassFilter::Nothing => return true,
            KlassFilter::Exactly(k) => {
                if self.class_of_object(obj) != Some(k) {
                    return true;
                }
            }
        }
        let tagged = self.tag(obj) != 0;
        let class_tagged = self.class_tag(obj) != 0;
        (heap_filter & FILTER_TAGGED != 0 && tagged)
            || (heap_filter & FILTER_UNTAGGED != 0 && !tagged)
            || (heap_filter & FILTER_CLASS_TAGGED != 0 && class_tagged)
            || (heap_filter & FILTER_CLASS_UNTAGGED != 0 && !class_tagged)
    }

    fn length_of(&self, obj: ObjectRef) -> JInt {
        if self.shared.mem.heap.get_header(obj).kind() == ObjectKind::Array {
            JInt::try_from(self.shared.mem.heap.array_length(obj)).unwrap_or(JInt::MAX)
        } else {
            -1
        }
    }

    /// The index HotSpot gives the first field `class_id` declares
    /// (`ClassFieldMap`): its transitive interfaces' fields, then every
    /// superclass's own and their transitive interfaces' fields, from
    /// `Object` down. Statics and instance fields both count.
    fn field_base(&mut self, class_id: ClassId) -> usize {
        if let Some(&base) = self.field_bases.get(&class_id) {
            return base;
        }
        let mut chain = Vec::new();
        let mut at = Some(class_id);
        while let Some(c) = at.and_then(|c| self.cm.get_class(c)) {
            chain.push(c.id);
            at = c.superclass;
        }
        let mut index = 0;
        for &c in chain.iter().rev() {
            index += self.transitive_interface_fields(c);
            if c == class_id {
                break;
            }
            index += self.cm.get_class(c).map_or(0, |k| k.fields.len());
        }
        self.field_bases.insert(class_id, index);
        index
    }

    /// The fields of every interface `class_id` implements, transitively
    /// (through its superclasses too), each interface once.
    fn transitive_interface_fields(&self, class_id: ClassId) -> usize {
        let mut seen: Vec<ClassId> = Vec::new();
        let mut pending: Vec<ClassId> = Vec::new();
        let mut at = Some(class_id);
        while let Some(c) = at.and_then(|c| self.cm.get_class(c)) {
            pending.extend(c.interfaces.iter().copied());
            at = c.superclass;
        }
        while let Some(i) = pending.pop() {
            if seen.contains(&i) {
                continue;
            }
            seen.push(i);
            if let Some(k) = self.cm.get_class(i) {
                pending.extend(k.interfaces.iter().copied());
            }
        }
        seen.iter()
            .filter_map(|&i| self.cm.get_class(i))
            .map(|k| k.fields.len())
            .sum()
    }

    /// The instance fields of `obj`'s class and superclasses: `(index,
    /// descriptor, value)`, from `Object` down.
    fn instance_fields(&mut self, obj: ObjectRef) -> Vec<(JInt, Arc<str>, Value)> {
        let class_id = self.shared.mem.heap.class_id_of(obj);
        let mut chain = Vec::new();
        let mut at = Some(class_id);
        while let Some(c) = at.and_then(|c| self.cm.get_class(c)) {
            chain.push(c.id);
            at = c.superclass;
        }
        let num_slots = self.shared.mem.heap.get_header(obj).num_slots() as usize;
        let mut out = Vec::new();
        let mut slot = 0usize;
        for &c in chain.iter().rev() {
            let base = self.field_base(c);
            let fields: Vec<(usize, Arc<str>, bool)> =
                self.cm.get_class(c).map_or_else(Vec::new, |k| {
                    k.fields
                        .iter()
                        .enumerate()
                        .map(|(i, f)| (i, Arc::clone(&f.descriptor), f.is_static()))
                        .collect()
                });
            for (i, descriptor, is_static) in fields {
                if is_static {
                    continue;
                }
                if slot < num_slots {
                    let value = self.shared.mem.heap.get_field(obj, slot);
                    out.push((
                        JInt::try_from(base + i).unwrap_or(JInt::MAX),
                        descriptor,
                        value,
                    ));
                }
                slot += 1;
            }
        }
        out
    }

    /// The static fields of class `class_id`: `(index, descriptor, value)`.
    fn static_fields(&mut self, class_id: ClassId) -> Vec<(JInt, Arc<str>, Value)> {
        let base = self.field_base(class_id);
        let fields: Vec<(usize, Arc<str>, bool)> =
            self.cm.get_class(class_id).map_or_else(Vec::new, |k| {
                k.fields
                    .iter()
                    .enumerate()
                    .map(|(i, f)| (i, Arc::clone(&f.descriptor), f.is_static()))
                    .collect()
            });
        let statics = self.shared.classes.statics.read();
        let block = statics.get(&class_id);
        let mut out = Vec::new();
        let mut static_index = 0usize;
        for (i, descriptor, is_static) in fields {
            if !is_static {
                continue;
            }
            if let Some(value) = block.and_then(|b| b.get(static_index)).copied() {
                out.push((
                    JInt::try_from(base + i).unwrap_or(JInt::MAX),
                    descriptor,
                    value,
                ));
            }
            static_index += 1;
        }
        out
    }

    /// The references `obj` holds, in HotSpot's order
    /// (`VM_HeapWalkOperation::visit`): a mirror's class references
    /// ([`Self::class_references`]); an instance's class, then its reference
    /// fields; an array's class, then its non-null elements.
    fn references_of(&mut self, obj: ObjectRef) -> Vec<Reference> {
        if let Some(class_id) = self.mirrored_class(obj) {
            return self.class_references(class_id);
        }
        let mut out = Vec::new();
        if let Some(mirror) = self.class_of_object(obj).and_then(|c| self.mirror_of(c)) {
            out.push(Reference {
                kind: REF_CLASS,
                info: ReferenceInfo::default(),
                target: mirror,
            });
        }
        let header = self.shared.mem.heap.get_header(obj);
        if header.kind() == ObjectKind::Array {
            if self.shared.mem.heap.array_element_type(obj) == Some(ArrayElementType::Reference) {
                for i in 0..self.shared.mem.heap.array_length(obj) {
                    if let Ok(Value::Object(Some(target))) =
                        self.shared.mem.heap.get_array_element(obj, i)
                    {
                        out.push(Reference {
                            kind: REF_ARRAY_ELEMENT,
                            info: ReferenceInfo::index(JInt::try_from(i).unwrap_or(JInt::MAX)),
                            target,
                        });
                    }
                }
            }
            return out;
        }
        for (index, _, value) in self.instance_fields(obj) {
            if let Value::Object(Some(target)) = value {
                out.push(Reference {
                    kind: REF_FIELD,
                    info: ReferenceInfo::index(index),
                    target,
                });
            }
        }
        // A class loader keeps the classes it defined in its `classes` list
        // (`ClassLoader.addClass`, wave 63), which the field references above
        // reach. A class this VM defined for the loader without going through
        // `addClass` (a VM-side define) is followed here instead, as a
        // reference of kind OTHER, which no 1.0 callback is handed.
        out.extend(self.defined_classes_of(obj));
        out
    }

    /// The mirrors in class loader object `loader`'s `classes` list (an
    /// `ArrayList`), by address; empty for an object that is no loader.
    fn listed_classes_of(&self, loader: ObjectRef) -> HashSet<usize> {
        let mut out = HashSet::new();
        let heap = &self.shared.mem.heap;
        let field = |obj: ObjectRef, owner: &str, name: &str| {
            let class = self.cm.find_class_by_name_for_loader(
                owner,
                crate::classloading::ClassLoaderId::Bootstrap,
            )?;
            let slot = crate::vm::vm_exec::resolve_field_index_in_hierarchy(
                class,
                name,
                &self.cm.class_store,
            )?;
            Some(heap.get_field(obj, slot))
        };
        let Some(Value::Object(Some(list))) = field(loader, "java/lang/ClassLoader", "classes")
        else {
            return out;
        };
        let size = match field(list, "java/util/ArrayList", "size") {
            Some(Value::Int(n)) => usize::try_from(n).unwrap_or(0),
            _ => 0,
        };
        if let Some(Value::Object(Some(data))) = field(list, "java/util/ArrayList", "elementData") {
            for i in 0..size.min(heap.array_length(data)) {
                if let Ok(Value::Object(Some(mirror))) = heap.get_array_element(data, i) {
                    out.insert(mirror.as_ptr() as usize);
                }
            }
        }
        out
    }

    /// The mirrors of the classes class loader object `loader` defined that
    /// its `classes` list does not hold, as references; none for an object
    /// that is no loader.
    fn defined_classes_of(&mut self, loader: ObjectRef) -> Vec<Reference> {
        if self.loaders.is_none() {
            let mut by_loader: HashMap<usize, Vec<ObjectRef>> = HashMap::new();
            // zt t18/df7 (x13 item 21a): not an unbound or withdrawn define.
            let ids: Vec<ClassId> = self.cm.class_store.iter_named().map(|c| c.id).collect();
            for id in ids {
                if let (Some(l), Some(m)) = (self.class_loader_object(id), self.mirror_of(id)) {
                    by_loader.entry(l.as_ptr() as usize).or_default().push(m);
                }
            }
            self.loaders = Some(by_loader);
        }
        let Some(defined) = self
            .loaders
            .as_ref()
            .and_then(|m| m.get(&(loader.as_ptr() as usize)))
            .cloned()
        else {
            return Vec::new();
        };
        let listed = self.listed_classes_of(loader);
        defined
            .into_iter()
            .filter(|m| !listed.contains(&(m.as_ptr() as usize)))
            .map(|target| Reference {
                kind: REF_OTHER,
                info: ReferenceInfo::default(),
                target,
            })
            .collect()
    }

    /// The class loader object of class `class_id` (`None`: the bootstrap
    /// loader), read from its mirror's `classLoader` field.
    fn class_loader_object(&self, class_id: ClassId) -> Option<ObjectRef> {
        let mirror = self.mirror_of(class_id)?;
        let slot = self.mirror_field_slot("classLoader")?;
        match self.shared.mem.heap.get_field(mirror, slot) {
            Value::Object(loader) => loader,
            _ => None,
        }
    }

    /// The slot of `java.lang.Class`'s instance field `name`.
    fn mirror_field_slot(&self, name: &str) -> Option<usize> {
        let class = self.cm.find_class_by_name_for_loader(
            "java/lang/Class",
            crate::classloading::ClassLoaderId::Bootstrap,
        )?;
        crate::vm::vm_exec::resolve_field_index_in_hierarchy(class, name, &self.cm.class_store)
    }

    /// A mirror's references, as HotSpot's `iterate_over_class` reports
    /// them: none for a class not linked yet; the superclass (not
    /// `Object`), the class loader, the protection domain, the signers, the
    /// constant pool's class entries whose class is loaded (by pool index),
    /// the direct interfaces, the static reference fields.
    fn class_references(&mut self, class_id: ClassId) -> Vec<Reference> {
        let mut out = Vec::new();
        let Some((linked, superclass, interfaces, pool_classes, loader_id)) =
            self.cm.get_class(class_id).map(|c| {
                let pool: Vec<(usize, String)> = (1..c.constant_pool.len())
                    .filter_map(|i| {
                        c.constant_pool
                            .get_class_name(i as u16)
                            .map(|n| (i, n.to_string()))
                    })
                    .collect();
                (
                    !matches!(c.state, crate::classloading::ClassState::Loaded),
                    c.superclass,
                    c.interfaces.clone(),
                    pool,
                    c.loader_id,
                )
            })
        else {
            return out;
        };
        if !linked {
            return out;
        }
        let object = self.cm.find_class_by_name_for_loader(
            "java/lang/Object",
            crate::classloading::ClassLoaderId::Bootstrap,
        );
        if let Some(target) = superclass
            .filter(|&s| Some(s) != object)
            .and_then(|s| self.mirror_of(s))
        {
            out.push(Reference {
                kind: REF_SUPERCLASS,
                info: ReferenceInfo::default(),
                target,
            });
        }
        if let Some(target) = self.class_loader_object(class_id) {
            out.push(Reference {
                kind: REF_CLASS_LOADER,
                info: ReferenceInfo::default(),
                target,
            });
        }
        let mirror = self.mirror_of(class_id);
        for (name, kind) in [
            ("protectionDomain", REF_PROTECTION_DOMAIN),
            ("signers", REF_SIGNERS),
        ] {
            let target = mirror
                .zip(self.mirror_field_slot(name))
                .and_then(|(m, slot)| match self.shared.mem.heap.get_field(m, slot) {
                    Value::Object(t) => t,
                    _ => None,
                });
            if let Some(target) = target {
                out.push(Reference {
                    kind,
                    info: ReferenceInfo::default(),
                    target,
                });
            }
        }
        for (index, name) in pool_classes {
            let found = self.cm.find_class_by_name_for_loader(&name, loader_id);
            if let Some(target) = found.and_then(|c| self.mirror_of(c)) {
                out.push(Reference {
                    kind: REF_CONSTANT_POOL,
                    info: ReferenceInfo::index(JInt::try_from(index).unwrap_or(JInt::MAX)),
                    target,
                });
            }
        }
        for target in interfaces.iter().filter_map(|&i| self.mirror_of(i)) {
            out.push(Reference {
                kind: REF_INTERFACE,
                info: ReferenceInfo::default(),
                target,
            });
        }
        for (index, _, value) in self.static_fields(class_id) {
            if let Value::Object(Some(target)) = value {
                out.push(Reference {
                    kind: REF_STATIC_FIELD,
                    info: ReferenceInfo::index(index),
                    target,
                });
            }
        }
        out
    }

    /// The heap roots, as HotSpot collects them: the JNI global references,
    /// the classes of the built-in loaders (`SYSTEM_CLASS`), every live
    /// thread's `Thread` (`THREAD`) and the objects its frames hold
    /// (`STACK_LOCAL`, the calling thread's interpreter frames slot by slot,
    /// the other threads' from their published roots).
    fn roots(&mut self, current: &JvmThread) -> Vec<Reference> {
        let mut out = Vec::new();
        let mut globals = Vec::new();
        self.shared
            .natives
            .jni_global_refs
            .lock()
            .collect_roots(&mut globals);
        for target in globals {
            out.push(Reference {
                kind: REF_JNI_GLOBAL,
                info: ReferenceInfo::default(),
                target,
            });
        }
        {
            let mirrors = self.shared.classes.class_mirrors.read();
            for class in self.cm.class_store.iter() {
                let builtin = matches!(
                    class.loader_id,
                    crate::classloading::ClassLoaderId::Bootstrap
                        | crate::classloading::ClassLoaderId::Extension
                        | crate::classloading::ClassLoaderId::Application
                );
                if let Some(&target) = mirrors.get(&class.id).filter(|_| builtin) {
                    out.push(Reference {
                        kind: REF_SYSTEM_CLASS,
                        info: ReferenceInfo::default(),
                        target,
                    });
                }
            }
        }
        let registry = &self.shared.threads.thread_registry;
        for (tid, _) in registry.all_thread_names() {
            if !registry.is_alive(tid) {
                continue;
            }
            let Some(thread_obj) = registry.java_thread_obj(tid) else {
                continue;
            };
            out.push(Reference {
                kind: REF_THREAD,
                info: ReferenceInfo::default(),
                target: thread_obj,
            });
            let thread_tag = self.tag(thread_obj);
            let java_tid = crate::vm::vm_exec::read_java_thread_tid(self.shared, thread_obj)
                .map_or(0, |t| t as i64);
            if tid == current.thread_id {
                for (depth, frame) in current.frames.iter().rev().enumerate() {
                    let depth = JInt::try_from(depth).unwrap_or(JInt::MAX);
                    for slot in 0..frame.locals_len() {
                        if let Value::Object(Some(target)) = frame.get_local_unchecked(slot) {
                            out.push(Reference {
                                kind: REF_STACK_LOCAL,
                                info: ReferenceInfo::stack(
                                    thread_tag,
                                    java_tid,
                                    depth,
                                    0,
                                    frame.last_instr_pc as i64, // Cast: a bytecode index
                                    JInt::try_from(slot).unwrap_or(JInt::MAX),
                                ),
                                target,
                            });
                        }
                    }
                }
            } else {
                for target in registry.root_snapshot_of(tid) {
                    out.push(Reference {
                        kind: REF_STACK_LOCAL,
                        info: ReferenceInfo::stack(thread_tag, java_tid, -1, 0, -1, -1),
                        target,
                    });
                }
            }
        }
        out
    }

    fn size(&self, obj: ObjectRef) -> i64 {
        (self.sizer)(obj)
    }
}

/// Run `work` on a [`Walk`] of `env`'s tags inside a non-moving pause; the
/// tags the callbacks wrote are stored before the pause ends.
fn walking<R>(
    e: &JvmtiNativeEnv,
    shared: &SharedVm,
    work: impl FnOnce(&mut Walk<'_>, &JvmThread) -> R,
) -> Result<R, JInt> {
    walking_from(
        e,
        shared,
        || Ok(()),
        |walk, current, ()| work(walk, current),
    )
}

/// gci i8/hd: how long a walk keeps asking for its pause while another
/// stop-the-world owns the world, one GC-safe slice of
/// [`DECLINED_PAUSE_SLICE`] per refusal, before it answers
/// `JVMTI_ERROR_INTERNAL`.
const DECLINED_PAUSE_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// gci i8/hd: one wait between two requests of a walk's pause.
const DECLINED_PAUSE_SLICE: std::time::Duration = std::time::Duration::from_millis(1);

/// [`walking`] with a `prepare` step run right before each request of the
/// pause -- the decode of an agent's handle, whose object a collection run
/// between two requests may have moved -- whose answer `work` receives.
///
/// gci i8/hd (`gci-i3hd-heap-walks-and-diagnostics-review-findings-FIXED-20261017`
/// item 7): a declined request (another stop-the-world owns the world) is no
/// longer ignored. The walk used to proceed unpaused: the agent's callbacks
/// read a running heap that the other pause could move under them. And the
/// caller is a mutator that pause may be waiting for, so it must not spin on
/// the request either: each refusal waits [`DECLINED_PAUSE_SLICE`] GC-safe
/// ([`super::gc_safe`]: a deposited, blocked region, or a plain sleep for a
/// thread already in one, such as a JNI native's C code), which lets the
/// other pause count it and finish, then asks again. After
/// [`DECLINED_PAUSE_LIMIT`] the walk answers `JVMTI_ERROR_INTERNAL` rather
/// than walk unpaused. The tag index is brought up to date again inside the
/// pause (a collection between the first refresh and the pause stales its
/// addresses); the tags freed there are posted after the pause.
fn walking_from<P, R>(
    e: &JvmtiNativeEnv,
    shared: &SharedVm,
    mut prepare: impl FnMut() -> Result<P, JInt>,
    work: impl FnOnce(&mut Walk<'_>, &JvmThread, P) -> R,
) -> Result<R, JInt> {
    let current = jni::current_jvm_thread_of(shared).ok_or(ERR_UNATTACHED_THREAD)?;
    // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
    let initiator = unsafe { (*current).thread_id };
    let freed = e.state.lock().tags.refresh(shared);
    if !freed.is_empty() {
        super::events_more::post_object_free(e.addr(), &freed);
    }
    let deadline = std::time::Instant::now() + DECLINED_PAUSE_LIMIT;
    let (pause, prepared) = loop {
        let prepared = prepare()?;
        if let Some(pause) = crate::runtime::interpreter::NonMovingPause::request(shared, initiator)
        {
            break (pause, prepared);
        }
        if std::time::Instant::now() >= deadline {
            return Err(ERR_INTERNAL);
        }
        super::gc_safe(shared, || std::thread::sleep(DECLINED_PAUSE_SLICE));
    };
    // gci i8/hd + wave 87 (lane L1): refreshed again inside the pause. A
    // collection between the refresh above and the pause left the address
    // index of the epoch before it; on a heap that moves objects between
    // counts the index is also rebuilt (`refresh_in_pause`). Its freed tags
    // are posted after the pause.
    let late_freed = e.state.lock().tags.refresh_in_pause(shared);
    let before = e.state.lock().tags.snapshot();
    // gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
    // item 4): ONE exclusive borrow of the calling thread for the whole walk,
    // and both views of it -- the sizer's context and `work`'s thread --
    // shared from it. The sizer used to build a fresh `&mut` per object while
    // `work`'s `&JvmThread` (a second borrow of the same pointer) was live.
    let ctx = crate::vm::vm_exec::NativeContextImpl {
        shared,
        // SAFETY: the calling thread's own, live `JvmThread`; the walk runs
        // on it and only reads it, through this one borrow, until it ends.
        thread: unsafe { &mut *current },
    };
    let sizer = |obj: ObjectRef| -> i64 { crate::runtime::instrument::object_size_of(&ctx, obj) };
    let cm = shared.classes.class_manager.read();
    let mut walk = Walk {
        shared,
        cm: &cm,
        tags: before.clone(),
        array_classes: HashMap::new(),
        field_bases: HashMap::new(),
        loaders: None,
        sizer: &sizer,
    };
    let out = work(&mut walk, &*ctx.thread, prepared);
    let after = std::mem::take(&mut walk.tags);
    drop(walk);
    drop(cm);
    {
        let mut state = e.state.lock();
        for (&addr, &tag) in &after {
            if before.get(&addr) != Some(&tag) {
                // SAFETY: an address the walk saw as an object, unmoved (the
                // pause holds).
                state
                    .tags
                    .set(shared, unsafe { ObjectRef::from_raw(addr as *mut u8) }, tag);
            }
        }
        for &addr in before.keys().filter(|a| !after.contains_key(a)) {
            // SAFETY: as above.
            state
                .tags
                .set(shared, unsafe { ObjectRef::from_raw(addr as *mut u8) }, 0);
        }
    }
    drop(pause);
    // gci i8/hd: what the in-pause refresh dropped, posted outside the pause
    // as the first refresh's are.
    if !late_freed.is_empty() {
        super::events_more::post_object_free(e.addr(), &late_freed);
    }
    Ok(out)
}

/// Every object of the heap, in the walk's pause: what `walk_objects`
/// yields but the reserved TLAB tails and the humongous fillers.
///
/// gci i14/hd (`gci-i14hd-g1-heap-diagnostics-review-findings-FIXED-20261016`
/// item 8): visited in place (`VmHeap::for_each_object`), the tails searched
/// sorted (a linear scan per object before), and a retired TLAB's `int[]`
/// filler or an object (reference array) whose class the store does not hold
/// -- a dead instance of an unloaded class in an uncollected G1 region, a VM
/// wrapper -- is not handed to the agent: it has no `jclass`.
fn heap_objects(shared: &SharedVm) -> Vec<ObjectRef> {
    let mut tails = shared.threads.thread_registry.collect_reserved_tlab_tails();
    tails.sort_unstable();
    let mut known: HashSet<u32> = HashSet::new();
    {
        let cm = shared.classes.class_manager.read();
        known.extend(cm.class_store.iter_named().map(|c| c.id.as_u32()));
    }
    let heap = &shared.mem.heap;
    let mut out = Vec::new();
    heap.for_each_object(&mut |p: *mut u8, _size: usize| {
        let a = p as usize;
        let after = tails.partition_point(|&(s, _)| s <= a);
        if after > 0 && a < tails[after - 1].1 {
            return;
        }
        // SAFETY: the walk yields object starts, in the walk's pause.
        let o = unsafe { ObjectRef::from_raw(p) };
        let h = heap.get_header(o);
        if heap_object_listed(h.kind(), h.class_id.as_u32(), h.element_type(), &known) {
            out.push(o);
        }
    });
    out
}

/// gci i14/hd (item 8): may the JVMTI walk hand this object to an agent?
/// Not a humongous filler, not a TLAB `int[]` filler, and an instance or a
/// reference array only when its class id (a reference array's: the
/// component's) is in `known`. `element` is read only for an array. Pure.
fn heap_object_listed(
    kind: ObjectKind,
    class_id: u32,
    element: ArrayElementType,
    known: &HashSet<u32>,
) -> bool {
    match kind {
        ObjectKind::HumongousFiller => false,
        ObjectKind::Object => known.contains(&class_id),
        ObjectKind::Array if class_id == cratonvm_gc::tlab::TLAB_FILLER_CLASS_ID.as_u32() => false,
        ObjectKind::Array => element != ArrayElementType::Reference || known.contains(&class_id),
    }
}

/// A JDWP heap walk (interpreter round i5 wave 103, lane L1;
/// `canGetInstanceInfo`), answered by [`debugger_heap_query`].
pub(crate) enum DebuggerHeapQuery {
    /// `VirtualMachine.InstanceCounts`: the instances of each class exactly
    /// (not of its subclasses), as HotSpot's `classInstanceCounts` counts
    /// them with `IterateThroughHeap`.
    InstanceCounts(Vec<ClassId>),
    /// `ReferenceType.Instances`: up to `max` (0: every one) instances of the
    /// class exactly.
    Instances(ClassId, usize),
    /// `ObjectReference.ReferringObjects`: up to `max` (0: every one)
    /// objects that hold a reference to the object -- a field, an array
    /// element, a class's static field (its mirror), an object's class (the
    /// referee is a mirror) -- as JVMTI reports references
    /// ([`Walk::references_of`]).
    Referrers(ObjectRef, usize),
}

/// What a [`DebuggerHeapQuery`] answers.
pub(crate) enum DebuggerHeapAnswer {
    Counts(Vec<u64>),
    Objects(Vec<ObjectRef>),
}

/// Answer the JDWP heap walk `query` on `thread`, a mutator the JDWP server
/// parked, or its heap service (interpreter round i5 wave 103, lane L1): the
/// heap is read in a non-moving pause that `thread` requests, as this
/// table's walks are; the objects answered stay where they are until
/// `thread` next reaches a safepoint (no collection completes while it runs
/// this), so its caller exports them right after.
///
/// Which objects count (interpreter round i5 wave 104, lane L1;
/// `from_roots`, `CRATONVM_JDWP_HEAP_WALK_FROM_ROOTS`, default on): the
/// objects reachable from the heap roots ([`Walk::roots`], followed through
/// [`Walk::references_of`] as `FollowReferences` follows them), as HotSpot's
/// back end counts all three commands with `FollowReferences` (`util.c`
/// `classInstanceCounts`, `classInstances`, `getReferrers`), so garbage not
/// yet collected is not answered. Without it (`=0`), every object of the
/// heap, as wave 103 answered. A peer frozen in compiled code by the pause's
/// take-over contributes the roots it last published (`root_snapshot_of`),
/// which the walk reads as JVMTI `FollowReferences` does here.
pub(crate) fn debugger_heap_query(
    shared: &SharedVm,
    thread: &JvmThread,
    query: DebuggerHeapQuery,
    from_roots: bool,
) -> DebuggerHeapAnswer {
    let pause = crate::runtime::interpreter::NonMovingPause::request(shared, thread.thread_id);
    let cm = shared.classes.class_manager.read();
    let sizer = |_: ObjectRef| -> i64 { 0 };
    let mut walk = Walk {
        shared,
        cm: &cm,
        tags: HashMap::new(),
        array_classes: HashMap::new(),
        field_bases: HashMap::new(),
        loaders: None,
        sizer: &sizer,
    };
    // The referee a `Referrers` query looks for, and the reachable objects
    // that hold a reference to it (found during the traversal, so each
    // object's references are read once).
    let referee = match &query {
        DebuggerHeapQuery::Referrers(target, _) => Some(*target),
        _ => None,
    };
    let mut referrers: Vec<ObjectRef> = Vec::new();
    let objects = if from_roots {
        reachable_objects(&mut walk, thread, |obj, references| {
            if let Some(target) = referee {
                if references.iter().any(|r| r.target == target) {
                    referrers.push(obj);
                }
            }
        })
    } else {
        heap_objects(shared)
    };
    let answer = match query {
        DebuggerHeapQuery::InstanceCounts(classes) => {
            let mut counts = vec![0u64; classes.len()];
            if !classes.is_empty() {
                for &obj in &objects {
                    if let Some(class) = walk.class_of_object(obj) {
                        for (i, &wanted) in classes.iter().enumerate() {
                            if wanted == class {
                                counts[i] += 1;
                            }
                        }
                    }
                }
            }
            DebuggerHeapAnswer::Counts(counts)
        }
        DebuggerHeapQuery::Instances(class, max) => {
            let mut out = Vec::new();
            for &obj in &objects {
                if max != 0 && out.len() >= max {
                    break;
                }
                if walk.class_of_object(obj) == Some(class) {
                    out.push(obj);
                }
            }
            DebuggerHeapAnswer::Objects(out)
        }
        DebuggerHeapQuery::Referrers(target, max) if from_roots => {
            let _ = target;
            if max != 0 {
                referrers.truncate(max);
            }
            DebuggerHeapAnswer::Objects(referrers)
        }
        DebuggerHeapQuery::Referrers(target, max) => {
            let mut out = Vec::new();
            for &obj in &objects {
                if max != 0 && out.len() >= max {
                    break;
                }
                if walk.references_of(obj).iter().any(|r| r.target == target) {
                    out.push(obj);
                }
            }
            DebuggerHeapAnswer::Objects(out)
        }
    };
    drop(walk);
    drop(cm);
    drop(pause);
    answer
}

/// Run `read` over every object of the heap inside a non-moving pause that
/// `thread`, a mutator the JDWP server parked (or its heap service),
/// requests (interpreter round i5 wave 104, lane L1: does an array of a
/// signature exist, before `ClassesBySignature` makes its class,
/// `debug::inspect::materialize_array_classes`). The objects are valid only
/// inside `read`, which must not allocate or reach a safepoint.
pub(crate) fn debugger_heap_scan<R>(
    shared: &SharedVm,
    thread: &JvmThread,
    read: impl FnOnce(&[ObjectRef]) -> R,
) -> R {
    let pause = crate::runtime::interpreter::NonMovingPause::request(shared, thread.thread_id);
    let objects = heap_objects(shared);
    let out = read(&objects);
    drop(pause);
    out
}

/// Every object reachable from the heap roots ([`Walk::roots`] of
/// `current`), each once, following [`Walk::references_of`] depth first, as
/// [`follow_graph`] does for `FollowReferences` (interpreter round i5 wave
/// 104, lane L1): `visit` is handed each object with its references, the
/// object's referrers being the visited objects whose references name it.
/// Inside the caller's pause.
fn reachable_objects(
    walk: &mut Walk<'_>,
    current: &JvmThread,
    mut visit: impl FnMut(ObjectRef, &[Reference]),
) -> Vec<ObjectRef> {
    let mut visited: HashSet<usize> = HashSet::new();
    let mut stack: Vec<ObjectRef> = Vec::new();
    for reference in walk.roots(current) {
        if visited.insert(reference.target.as_ptr() as usize) {
            stack.push(reference.target);
        }
    }
    let mut out = Vec::new();
    while let Some(obj) = stack.pop() {
        let references = walk.references_of(obj);
        visit(obj, &references);
        for reference in &references {
            if visited.insert(reference.target.as_ptr() as usize) {
                stack.push(reference.target);
            }
        }
        out.push(obj);
    }
    out
}

/// The class filter a `jclass` makes (see [`KlassFilter`]).
fn klass_filter(shared: &SharedVm, klass: JObject) -> KlassFilter {
    if klass == 0 {
        return KlassFilter::Any;
    }
    match decode_class(shared, klass) {
        Some(DecodedClass::Stored(id)) => KlassFilter::Exactly(id),
        _ => KlassFilter::Nothing,
    }
}

/// `jvmtiPrimitiveType` of a field descriptor, `None` for a reference.
fn primitive_type(descriptor: &str) -> Option<JInt> {
    let c = *descriptor.as_bytes().first()?;
    matches!(c, b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D').then_some(JInt::from(c))
}

/// A field value as the `jvalue` a primitive field callback is handed.
fn jvalue_of(value: &Value, ty: JInt) -> u64 {
    match (ty as u8, value) {
        // Cast: `ty` is an ASCII type letter
        (b'J', Value::Long(v)) => *v as u64,
        (b'F', Value::Float(v)) => u64::from(v.to_bits()),
        (b'D', Value::Double(v)) => v.to_bits(),
        (b'Z' | b'B', Value::Int(v)) => u64::from(*v as u8),
        (b'C' | b'S', Value::Int(v)) => u64::from(*v as u16),
        (_, Value::Int(v)) => u64::from(*v as u32),
        _ => 0,
    }
}

/// `jvmtiPrimitiveType` of an array element type.
fn element_primitive(element: ArrayElementType) -> Option<JInt> {
    let c = match element {
        ArrayElementType::Boolean => b'Z',
        ArrayElementType::Byte => b'B',
        ArrayElementType::Char => b'C',
        ArrayElementType::Short => b'S',
        ArrayElementType::Int => b'I',
        ArrayElementType::Long => b'J',
        ArrayElementType::Float => b'F',
        ArrayElementType::Double => b'D',
        ArrayElementType::Reference => return None,
    };
    Some(JInt::from(c))
}

/// A primitive array's elements in their C layout.
fn primitive_elements(shared: &SharedVm, arr: ObjectRef, element: ArrayElementType) -> Vec<u8> {
    let len = shared.mem.heap.array_length(arr);
    let mut out = Vec::new();
    for i in 0..len {
        let value = shared
            .mem
            .heap
            .get_array_element_unboxing(arr, i)
            .unwrap_or(Value::Int(0));
        match (element, value) {
            (ArrayElementType::Boolean | ArrayElementType::Byte, Value::Int(v)) => {
                out.push(v as u8)
            }
            (ArrayElementType::Char | ArrayElementType::Short, Value::Int(v)) => {
                out.extend_from_slice(&(v as u16).to_ne_bytes())
            }
            (ArrayElementType::Int, Value::Int(v)) => out.extend_from_slice(&v.to_ne_bytes()),
            (ArrayElementType::Long, Value::Long(v)) => out.extend_from_slice(&v.to_ne_bytes()),
            (ArrayElementType::Float, Value::Float(v)) => out.extend_from_slice(&v.to_ne_bytes()),
            (ArrayElementType::Double, Value::Double(v)) => out.extend_from_slice(&v.to_ne_bytes()),
            _ => out.extend(std::iter::repeat_n(0u8, element_width(element))),
        }
    }
    out
}

fn element_width(element: ArrayElementType) -> usize {
    match element {
        ArrayElementType::Boolean | ArrayElementType::Byte => 1,
        ArrayElementType::Char | ArrayElementType::Short => 2,
        ArrayElementType::Int | ArrayElementType::Float => 4,
        _ => 8,
    }
}

/// Is `obj` a `java.lang.String`?
fn is_string(walk: &Walk<'_>, obj: ObjectRef) -> bool {
    let class_id = walk.shared.mem.heap.class_id_of(obj);
    walk.cm
        .get_class(class_id)
        .is_some_and(|c| &*c.name == "java/lang/String")
}

/// The callbacks an object gets after its iteration or reference callback:
/// its primitive fields (an instance's, or a mirror's static ones), a
/// `String`'s value, a primitive array's elements. `false` to abort.
fn report_values(
    walk: &mut Walk<'_>,
    callbacks: &HeapCallbacks,
    obj: ObjectRef,
    user: usize,
) -> bool {
    let header_kind = walk.shared.mem.heap.get_header(obj).kind();
    if callbacks.primitive_field != 0 && header_kind != ObjectKind::Array {
        // SAFETY: the agent's `jvmtiPrimitiveFieldCallback`.
        let cb: PrimitiveFieldCallback = unsafe { std::mem::transmute(callbacks.primitive_field) };
        let (kind, fields) = match walk.mirrored_class(obj) {
            Some(class_id) => (REF_STATIC_FIELD, walk.static_fields(class_id)),
            None => (REF_FIELD, walk.instance_fields(obj)),
        };
        let class_tag = walk.class_tag(obj);
        for (index, descriptor, value) in fields {
            let Some(ty) = primitive_type(&descriptor) else {
                continue;
            };
            let info = ReferenceInfo::index(index);
            let mut tag = walk.tag(obj);
            // SAFETY: a call of the agent's callback with live arguments.
            let res = unsafe {
                cb(
                    kind,
                    &info,
                    class_tag,
                    &mut tag,
                    jvalue_of(&value, ty),
                    ty,
                    user,
                )
            };
            walk.set_tag(obj, tag);
            if res & VISIT_ABORT != 0 {
                return false;
            }
        }
    }
    if callbacks.string_primitive_value != 0
        && header_kind != ObjectKind::Array
        && is_string(walk, obj)
    {
        // SAFETY: the agent's `jvmtiStringPrimitiveValueCallback`.
        let cb: StringPrimitiveValueCallback =
            unsafe { std::mem::transmute(callbacks.string_primitive_value) };
        let text = crate::vm::read_java_string(&walk.shared.mem.heap, obj).unwrap_or_default();
        let chars: Vec<u16> = text.encode_utf16().collect();
        let class_tag = walk.class_tag(obj);
        let size = walk.size(obj);
        let mut tag = walk.tag(obj);
        // SAFETY: as above.
        let res = unsafe {
            cb(
                class_tag,
                size,
                &mut tag,
                chars.as_ptr(),
                JInt::try_from(chars.len()).unwrap_or(JInt::MAX),
                user,
            )
        };
        walk.set_tag(obj, tag);
        if res & VISIT_ABORT != 0 {
            return false;
        }
    }
    if callbacks.array_primitive_value != 0 && header_kind == ObjectKind::Array {
        if let Some((element, ty)) = walk
            .shared
            .mem
            .heap
            .array_element_type(obj)
            .and_then(|e| element_primitive(e).map(|t| (e, t)))
        {
            // SAFETY: the agent's `jvmtiArrayPrimitiveValueCallback`.
            let cb: ArrayPrimitiveValueCallback =
                unsafe { std::mem::transmute(callbacks.array_primitive_value) };
            let elements = primitive_elements(walk.shared, obj, element);
            let class_tag = walk.class_tag(obj);
            let size = walk.size(obj);
            let length = walk.length_of(obj);
            let mut tag = walk.tag(obj);
            // SAFETY: as above.
            let res = unsafe {
                cb(
                    class_tag,
                    size,
                    &mut tag,
                    length,
                    ty,
                    elements.as_ptr(),
                    user,
                )
            };
            walk.set_tag(obj, tag);
            if res & VISIT_ABORT != 0 {
                return false;
            }
        }
    }
    true
}

/// Slot 116: `IterateThroughHeap(env, heap_filter, klass, callbacks,
/// user_data)`: every object of the heap, reachable or not, that passes the
/// filters -- the class filter is the object's exact class (MEASURED: a
/// `Leaf` is not a `Node` here) -- to the iteration callback, then its
/// primitive values to theirs.
pub(super) extern "C" fn iterate_through_heap(
    env: *mut JvmtiNativeEnv,
    heap_filter: JInt,
    klass: JObject,
    callbacks: *const HeapCallbacks,
    user_data: usize,
) -> JInt {
    let (e, shared) = match tag_env(env) {
        Ok(found) => found,
        Err(code) => return code,
    };
    if klass != 0 && decode_class(&shared, klass).is_none() && object_of(&shared, klass).is_none() {
        return ERR_INVALID_CLASS;
    }
    if callbacks.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's `jvmtiHeapCallbacks`.
    let callbacks = unsafe { &*callbacks };
    let filter = klass_filter(&shared, klass);
    let walked = walking(e, &shared, |walk, _| {
        for obj in heap_objects(walk.shared) {
            if walk.filtered(obj, heap_filter, filter) {
                continue;
            }
            if callbacks.heap_iteration != 0 {
                // SAFETY: the agent's `jvmtiHeapIterationCallback`.
                let cb: HeapIterationCallback =
                    unsafe { std::mem::transmute(callbacks.heap_iteration) };
                let class_tag = walk.class_tag(obj);
                let size = walk.size(obj);
                let length = walk.length_of(obj);
                let mut tag = walk.tag(obj);
                // SAFETY: a call of the agent's callback with live arguments.
                let res = unsafe { cb(class_tag, size, &mut tag, length, user_data) };
                walk.set_tag(obj, tag);
                if res & VISIT_ABORT != 0 {
                    return;
                }
            }
            if !report_values(walk, callbacks, obj, user_data) {
                return;
            }
        }
    });
    match walked {
        Ok(()) => ERR_NONE,
        Err(code) => code,
    }
}

/// The reference callbacks of a graph walk: the advanced ones of
/// `FollowReferences`, or the JVMTI 1.0 ones.
enum Reporter<'c> {
    Advanced {
        callbacks: &'c HeapCallbacks,
        heap_filter: JInt,
        klass: KlassFilter,
    },
    Basic {
        root: usize,
        stack: usize,
        object: usize,
    },
}

/// What a reported reference asks of the walk.
enum Next {
    Follow,
    Skip,
    Abort,
}

/// The JVMTI 1.0 `jvmtiObjectReferenceKind` of a reference kind: a
/// superclass is reported as `CLASS`, as HotSpot reports it; the roots have
/// none.
fn basic_kind(kind: JInt) -> JInt {
    if kind == REF_SUPERCLASS {
        REF_CLASS
    } else {
        kind
    }
}

/// `jvmtiHeapRootKind` of a root reference kind.
fn basic_root_kind(kind: JInt) -> JInt {
    match kind {
        REF_JNI_GLOBAL => 1,
        REF_SYSTEM_CLASS => 2,
        REF_STACK_LOCAL => 4,
        REF_THREAD => 6,
        _ => 7,
    }
}

type BasicRootCallback = unsafe extern "C" fn(JInt, i64, i64, *mut i64, usize) -> JInt;
type BasicStackCallback =
    unsafe extern "C" fn(JInt, i64, i64, *mut i64, i64, JInt, u64, JInt, usize) -> JInt;
type BasicObjectCallback = unsafe extern "C" fn(JInt, i64, i64, *mut i64, i64, JInt, usize) -> JInt;

impl Reporter<'_> {
    /// Report `reference` from `referrer` (`None`: a root).
    fn report(
        &self,
        walk: &mut Walk<'_>,
        referrer: Option<ObjectRef>,
        reference: &Reference,
        user: usize,
    ) -> Next {
        let target = reference.target;
        match *self {
            Reporter::Advanced {
                callbacks,
                heap_filter,
                klass,
            } => {
                if walk.filtered(target, heap_filter, klass) || callbacks.heap_reference == 0 {
                    return Next::Follow;
                }
                // SAFETY: the agent's `jvmtiHeapReferenceCallback`.
                let cb: HeapReferenceCallback =
                    unsafe { std::mem::transmute(callbacks.heap_reference) };
                let class_tag = walk.class_tag(target);
                let referrer_class_tag = referrer.map_or(0, |r| walk.class_tag(r));
                let size = walk.size(target);
                let length = walk.length_of(target);
                let mut tag = walk.tag(target);
                let mut referrer_tag = referrer.map_or(0, |r| walk.tag(r));
                let same = referrer == Some(target);
                let referrer_ptr: *mut i64 = match referrer {
                    None => std::ptr::null_mut(),
                    Some(_) if same => &mut tag,
                    Some(_) => &mut referrer_tag,
                };
                // SAFETY: a call of the agent's callback with live arguments.
                let res = unsafe {
                    cb(
                        reference.kind,
                        &reference.info,
                        class_tag,
                        referrer_class_tag,
                        size,
                        &mut tag,
                        referrer_ptr,
                        length,
                        user,
                    )
                };
                walk.set_tag(target, tag);
                if let Some(r) = referrer.filter(|_| !same) {
                    walk.set_tag(r, referrer_tag);
                }
                if res & VISIT_ABORT != 0 {
                    Next::Abort
                } else if res & VISIT_OBJECTS != 0 {
                    Next::Follow
                } else {
                    Next::Skip
                }
            }
            Reporter::Basic {
                root,
                stack,
                object,
            } => {
                let class_tag = walk.class_tag(target);
                let size = walk.size(target);
                let mut tag = walk.tag(target);
                let res = match referrer {
                    None if reference.kind == REF_STACK_LOCAL => {
                        if stack == 0 {
                            return Next::Follow;
                        }
                        // SAFETY: the agent's `jvmtiStackReferenceCallback`.
                        let cb: BasicStackCallback = unsafe { std::mem::transmute(stack) };
                        let w = reference.info.words;
                        // SAFETY: a call of the agent's callback.
                        unsafe {
                            cb(
                                4,
                                class_tag,
                                size,
                                &mut tag,
                                w[0],
                                w[2] as JInt,
                                w[3] as u64,
                                w[5] as JInt,
                                user,
                            )
                        } // Cast: the info words this walk wrote
                    }
                    None => {
                        if root == 0 {
                            return Next::Follow;
                        }
                        // SAFETY: the agent's `jvmtiHeapRootCallback`.
                        let cb: BasicRootCallback = unsafe { std::mem::transmute(root) };
                        // SAFETY: a call of the agent's callback.
                        unsafe {
                            cb(
                                basic_root_kind(reference.kind),
                                class_tag,
                                size,
                                &mut tag,
                                user,
                            )
                        }
                    }
                    Some(r) => {
                        if object == 0 || reference.kind == REF_OTHER {
                            return Next::Follow;
                        }
                        // SAFETY: the agent's `jvmtiObjectReferenceCallback`.
                        let cb: BasicObjectCallback = unsafe { std::mem::transmute(object) };
                        let referrer_tag = walk.tag(r);
                        let index = match reference.kind {
                            REF_FIELD | REF_ARRAY_ELEMENT | REF_STATIC_FIELD
                            | REF_CONSTANT_POOL => {
                                reference.info.words[0] as JInt // Cast: the `jint` index this walk wrote
                            }
                            _ => -1,
                        };
                        // SAFETY: a call of the agent's callback.
                        unsafe {
                            cb(
                                basic_kind(reference.kind),
                                class_tag,
                                size,
                                &mut tag,
                                referrer_tag,
                                index,
                                user,
                            )
                        }
                    }
                };
                walk.set_tag(target, tag);
                match res {
                    0 => Next::Abort,
                    2 => Next::Skip,
                    _ => Next::Follow,
                }
            }
        }
    }
}

/// The graph walk of `FollowReferences` and the JVMTI 1.0 reachable walks:
/// from `initial` (not reported itself) or from the roots, each reference
/// reported, each object visited once, depth first as HotSpot's visit stack
/// runs; an object a filter hides is visited without a callback.
fn follow_graph(
    walk: &mut Walk<'_>,
    current: &JvmThread,
    reporter: &Reporter<'_>,
    initial: Option<ObjectRef>,
    user: usize,
) {
    let mut visited: HashSet<usize> = HashSet::new();
    let mut stack: Vec<ObjectRef> = Vec::new();
    let mut push = |obj: ObjectRef, visited: &mut HashSet<usize>, stack: &mut Vec<ObjectRef>| {
        if visited.insert(obj.as_ptr() as usize) {
            stack.push(obj);
        }
    };
    match initial {
        Some(obj) => push(obj, &mut visited, &mut stack),
        None => {
            for reference in walk.roots(current) {
                match reporter.report(walk, None, &reference, user) {
                    Next::Abort => return,
                    Next::Follow => push(reference.target, &mut visited, &mut stack),
                    Next::Skip => {}
                }
            }
        }
    }
    while let Some(obj) = stack.pop() {
        if let Reporter::Advanced {
            callbacks,
            heap_filter,
            klass,
        } = reporter
        {
            if !walk.filtered(obj, *heap_filter, *klass)
                && !report_values(walk, callbacks, obj, user)
            {
                return;
            }
        }
        for reference in walk.references_of(obj) {
            match reporter.report(walk, Some(obj), &reference, user) {
                Next::Abort => return,
                Next::Follow => push(reference.target, &mut visited, &mut stack),
                Next::Skip => {}
            }
        }
    }
}

/// Slot 115: `FollowReferences(env, heap_filter, klass, initial_object,
/// callbacks, user_data)`: the references from `initial_object` (or from the
/// heap roots), the objects whose callback asks for `JVMTI_VISIT_OBJECTS`
/// followed; an object the filters hide gets no callback and is followed
/// (MEASURED: with a `Node` filter a static field's reference to the root
/// node is reported, found through classes the filter hides).
pub(super) extern "C" fn follow_references(
    env: *mut JvmtiNativeEnv,
    heap_filter: JInt,
    klass: JObject,
    initial_object: JObject,
    callbacks: *const HeapCallbacks,
    user_data: usize,
) -> JInt {
    let (e, shared) = match tag_env(env) {
        Ok(found) => found,
        Err(code) => return code,
    };
    if callbacks.is_null() {
        return ERR_NULL_POINTER;
    }
    let initial = if initial_object == 0 {
        None
    } else {
        match object_of(&shared, initial_object) {
            Some(obj) => Some(obj),
            None => return ERR_INVALID_OBJECT,
        }
    };
    // SAFETY: the agent's `jvmtiHeapCallbacks`.
    let callbacks = unsafe { &*callbacks };
    let reporter = Reporter::Advanced {
        callbacks,
        heap_filter,
        klass: klass_filter(&shared, klass),
    };
    // gci i8/hd: the handle decoded again before each request of the pause
    // (`walking_from`), so a collection the walk waited out between two
    // requests cannot leave it a vacated address. Wave 87 (lane L1): and read
    // again inside the pause, which is where the walk uses it.
    let reinitial = || match initial {
        None => Ok(None),
        Some(_) => object_of(&shared, initial_object)
            .map(Some)
            .ok_or(ERR_INVALID_OBJECT),
    };
    match walking_from(e, &shared, reinitial, |walk, current, initial| {
        let initial = initial.and_then(|_| object_in_pause(walk.shared, initial_object));
        follow_graph(walk, current, &reporter, initial, user_data)
    }) {
        Ok(()) => ERR_NONE,
        Err(code) => code,
    }
}

/// `jvmtiHeapObjectFilter` of the JVMTI 1.0 walks: `TAGGED` (1),
/// `UNTAGGED` (2), any other value every object (MEASURED: 9 is accepted).
fn basic_filter_passes(filter: JInt, tag: i64) -> bool {
    match filter {
        1 => tag != 0,
        2 => tag == 0,
        _ => true,
    }
}

type BasicHeapCallback = unsafe extern "C" fn(i64, i64, *mut i64, usize) -> JInt;

/// The JVMTI 1.0 heap walk of [`iterate_over_heap`] and
/// [`iterate_over_instances_of_class`]: objects of `class` and its
/// subclasses (`None`: every object) passing `filter`, until the callback
/// answers `JVMTI_ITERATION_ABORT` (0).
fn basic_heap_walk(
    env: *mut JvmtiNativeEnv,
    class: Option<ClassId>,
    filter: JInt,
    callback: usize,
    user: usize,
) -> JInt {
    let (e, shared) = match tag_env(env) {
        Ok(found) => found,
        Err(code) => return code,
    };
    // SAFETY: the agent's `jvmtiHeapObjectCallback`, checked non-null.
    let cb: BasicHeapCallback = unsafe { std::mem::transmute(callback) };
    let walked = walking(e, &shared, |walk, _| {
        for obj in heap_objects(walk.shared) {
            if let Some(class) = class {
                let Some(own) = walk.class_of_object(obj) else {
                    continue;
                };
                if !walk.cm.is_subclass_of(own, class) {
                    continue;
                }
            }
            let mut tag = walk.tag(obj);
            if !basic_filter_passes(filter, tag) {
                continue;
            }
            let class_tag = walk.class_tag(obj);
            let size = walk.size(obj);
            // SAFETY: a call of the agent's callback with live arguments.
            let res = unsafe { cb(class_tag, size, &mut tag, user) };
            walk.set_tag(obj, tag);
            if res == 0 {
                return;
            }
        }
    });
    match walked {
        Ok(()) => ERR_NONE,
        Err(code) => code,
    }
}

/// Slot 111: `IterateOverHeap(env, object_filter, heap_object_callback,
/// user_data)`.
pub(super) extern "C" fn iterate_over_heap(
    env: *mut JvmtiNativeEnv,
    filter: JInt,
    callback: usize,
    user: usize,
) -> JInt {
    if let Err(code) = tag_env(env) {
        return code;
    }
    if callback == 0 {
        return ERR_NULL_POINTER;
    }
    basic_heap_walk(env, None, filter, callback, user)
}

/// Slot 112: `IterateOverInstancesOfClass(env, klass, object_filter,
/// heap_object_callback, user_data)`: the class and its subclasses
/// (MEASURED: the `Leaf`s among the `Node`s).
pub(super) extern "C" fn iterate_over_instances_of_class(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    filter: JInt,
    callback: usize,
    user: usize,
) -> JInt {
    let shared = match tag_env(env) {
        Ok((_, shared)) => shared,
        Err(code) => return code,
    };
    let class = match decode_class(&shared, klass) {
        Some(DecodedClass::Stored(id)) => Some(id),
        Some(_) => None,
        None => return ERR_INVALID_CLASS,
    };
    if callback == 0 {
        return ERR_NULL_POINTER;
    }
    match class {
        Some(class) => basic_heap_walk(env, Some(class), filter, callback, user),
        None => ERR_NONE,
    }
}

/// Slot 109: `IterateOverObjectsReachableFromObject(env, object,
/// object_reference_callback, user_data)`.
pub(super) extern "C" fn iterate_over_objects_reachable_from_object(
    env: *mut JvmtiNativeEnv,
    object: JObject,
    callback: usize,
    user: usize,
) -> JInt {
    let (e, shared) = match tag_env(env) {
        Ok(found) => found,
        Err(code) => return code,
    };
    if object_of(&shared, object).is_none() {
        return ERR_INVALID_OBJECT;
    }
    if callback == 0 {
        return ERR_NULL_POINTER;
    }
    let reporter = Reporter::Basic {
        root: 0,
        stack: 0,
        object: callback,
    };
    // gci i8/hd: decoded again before each request of the pause, as in
    // `follow_references`; the first decode above keeps the error order.
    // Wave 87 (lane L1): and read again inside the pause.
    let reinitial = || {
        object_of(&shared, object)
            .map(Some)
            .ok_or(ERR_INVALID_OBJECT)
    };
    match walking_from(e, &shared, reinitial, |walk, current, initial| {
        let initial = initial.and_then(|_| object_in_pause(walk.shared, object));
        follow_graph(walk, current, &reporter, initial, user)
    }) {
        Ok(()) => ERR_NONE,
        Err(code) => code,
    }
}

/// Slot 110: `IterateOverReachableObjects(env, heap_root_callback,
/// stack_ref_callback, object_ref_callback, user_data)`: each callback may
/// be NULL (MEASURED: all three NULL is 0).
pub(super) extern "C" fn iterate_over_reachable_objects(
    env: *mut JvmtiNativeEnv,
    root: usize,
    stack: usize,
    object: usize,
    user: usize,
) -> JInt {
    let (e, shared) = match tag_env(env) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let reporter = Reporter::Basic {
        root,
        stack,
        object,
    };
    match walking(e, &shared, |walk, current| {
        follow_graph(walk, current, &reporter, None, user)
    }) {
        Ok(()) => ERR_NONE,
        Err(code) => code,
    }
}

#[cfg(test)]
mod gci_i8hd_walk_pause_tests {
    /// The body of `fn <name>` in this file, up to the next item at column 0.
    fn body<'a>(src: &'a str, name: &str) -> &'a str {
        let start = src.find(&format!("\nfn {name}")).expect("the function");
        let rest = &src[start + 1..];
        &rest[..rest.find("\n}\n").expect("its end")]
    }

    /// gci i8/hd (`gci-i3hd-heap-walks-and-diagnostics-review-findings-FIXED-20261017`
    /// item 7): every walk's pause is asked for in the retry loop of
    /// `walking_from`, a refusal waits GC-safe and the limit answers
    /// `JVMTI_ERROR_INTERNAL`; no walk takes the pause's `Option` and proceeds
    /// on `None`. (Source pin: the JVMTI C table cannot be driven against a
    /// competing pause in a unit test.)
    #[test]
    fn gci_i8hd_a_declined_walk_pause_is_retried_gc_safe() {
        let src = include_str!("heap.rs").replace("\r\n", "\n");
        let from = body(&src, "walking_from<");
        assert_eq!(from.matches("NonMovingPause::request(").count(), 1);
        assert!(from.contains(
            "if let Some(pause) = crate::runtime::interpreter::NonMovingPause::request("
        ));
        assert!(
            from.contains("super::gc_safe(shared, || std::thread::sleep(DECLINED_PAUSE_SLICE))")
        );
        assert!(from.contains("return Err(ERR_INTERNAL);"));
        // The tag index is refreshed again under the pause, its frees posted after.
        let late = from.find("let late_freed").expect("in-pause refresh");
        assert!(late > from.find("break (pause, prepared)").expect("the pause"));
        assert!(
            from.rfind("post_object_free(e.addr(), &late_freed)")
                .expect("posted")
                > from.find("drop(pause);").expect("end")
        );
        // `walking` is `walking_from` with nothing to prepare.
        let walking = body(&src, "walking<");
        assert!(walking.contains("walking_from("));
        assert!(!walking.contains("NonMovingPause"));
        // The handle-taking walks decode again per request.
        for f in [
            "follow_references(",
            "iterate_over_objects_reachable_from_object(",
        ] {
            let start = src
                .find(&format!("pub(super) extern \"C\" fn {f}"))
                .expect(f);
            let rest = &src[start..];
            let b = &rest[..rest.find("\n}\n").expect("end")];
            assert!(b.contains("walking_from(e, &shared, reinitial,"), "{f}");
        }
    }
}

#[cfg(test)]
mod gci_i9hj_tag_entry_tests {
    /// The text of `name`'s item, from its line up to the next `}` at column 0.
    fn item<'a>(src: &'a str, name: &str) -> &'a str {
        let start = src.find(name).expect(name);
        let rest = &src[start..];
        &rest[..rest.find("\n}\n").expect("its end")]
    }

    /// gci i9/hj (`gci-i8hd-jvmti-tag-functions-and-oome-dump-outside-a-pause-FIXED-20261017`
    /// items 1 and 2): `GetTag` / `SetTag` decode and use the object under one
    /// JNI entry; `GetObjectsWithTags` takes the entry before it resolves the
    /// weak globals and mints the handles under it; every `ObjectFree` is
    /// posted after the entry is dropped. (Source pin: a collection cannot be
    /// staged between the decode and the lookup in a unit test.)
    #[test]
    fn gci_i9hj_tag_functions_hold_the_entry_across_the_lookup() {
        let src = include_str!("heap.rs").replace("\r\n", "\n");
        let wot = item(&src, "fn with_object_tags<");
        // gci i48 lane jl48: the vm-only entry (the tag functions run no Java).
        let enter = wot
            .find("jni::ForeignJniEntry::enter_vm_only()")
            .expect("entry");
        let decode = wot.find("jni::jobject_to_obj(handle)").expect("decode");
        let table = wot.find("refreshed_tags(env, shared,").expect("table op");
        let closed = wot.find("\n    };\n").expect("entry scope ends");
        let posted = wot.find("post_freed_tags(env, &freed)").expect("posted");
        assert!(enter < decode && decode < table && table < closed && closed < posted);
        for f in [
            "pub(super) extern \"C\" fn get_tag(",
            "pub(super) extern \"C\" fn set_tag(",
        ] {
            let b = item(&src, f);
            assert!(b.contains("with_object_tags(e, &shared, object,"), "{f}");
            assert!(!b.contains("object_of("), "{f}");
            assert!(!b.contains("with_tags("), "{f}");
        }
        let gowt = item(&src, "pub(super) extern \"C\" fn get_objects_with_tags(");
        let enter = gowt.find("jni::ForeignJniEntry::enter()").expect("entry");
        let resolve = gowt.find("refreshed_tags(e, &shared,").expect("resolve");
        let mint = gowt.find("jni::new_local_handle(obj)").expect("mint");
        let posted = gowt.find("post_freed_tags(e, &freed)").expect("posted");
        assert!(enter < resolve && resolve < mint && mint < posted);
        assert!(
            !gowt.contains("with_tags(e,"),
            "no resolve before the entry"
        );
        // `with_tags` itself never takes an entry: it may run in native.
        assert!(!item(&src, "fn refreshed_tags<").contains("ForeignJniEntry"));
    }
}

#[cfg(test)]
mod gci_i11ja_walk_borrow_tests {
    /// gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
    /// item 4): `walking_from` takes ONE `&mut` of the calling thread, after
    /// the pause is held, and the sizer and `work` both share it; no second
    /// borrow is built from the raw pointer inside the sizer or for `work`.
    /// (Source pin: Stacked Borrows cannot be checked in a unit test.)
    #[test]
    fn gci_i11ja_heap_walk_borrows_the_thread_once() {
        let src = include_str!("heap.rs").replace("\r\n", "\n");
        let start = src.find("\nfn walking_from<").expect("walking_from");
        let rest = &src[start + 1..];
        let from = &rest[..rest.find("\n}\n").expect("its end")];
        assert_eq!(
            from.matches("&mut *current").count(),
            1,
            "one exclusive borrow"
        );
        assert_eq!(
            from.matches("NativeContextImpl {").count(),
            1,
            "one context"
        );
        assert!(
            !from.contains("&*current"),
            "no shared re-derive for `work`"
        );
        assert!(from.contains("work(&mut walk, &*ctx.thread, prepared)"));
        let sizer = from.find("let sizer =").expect("sizer");
        let ctx = from.find("let ctx =").expect("ctx");
        let pause = from.find("break (pause, prepared)").expect("pause");
        assert!(
            pause < ctx && ctx < sizer,
            "borrow taken after the pause, before the sizer"
        );
        let sizer_body = &from[sizer..sizer + from[sizer..].find("};").expect("its end")];
        assert!(
            sizer_body.contains("object_size_of(&ctx, obj)"),
            "{sizer_body}"
        );
        assert!(!sizer_body.contains("unsafe"), "{sizer_body}");
    }
}

#[cfg(test)]
mod gci_i14hd_heap_objects_tests {
    use super::*;

    /// gci i14/hd item 8: the JVMTI walk lists no humongous filler, no TLAB
    /// `int[]` filler and no instance / reference array whose class the store
    /// does not hold; a primitive array and a known class's objects stay.
    #[test]
    fn gci_i14hd_jvmti_walk_lists_only_classed_objects() {
        let known: HashSet<u32> = [7u32].into_iter().collect();
        let filler = cratonvm_gc::tlab::TLAB_FILLER_CLASS_ID.as_u32();
        assert!(heap_object_listed(
            ObjectKind::Object,
            7,
            ArrayElementType::Reference,
            &known
        ));
        assert!(!heap_object_listed(
            ObjectKind::Object,
            8,
            ArrayElementType::Reference,
            &known
        ));
        assert!(!heap_object_listed(
            ObjectKind::HumongousFiller,
            7,
            ArrayElementType::Int,
            &known
        ));
        assert!(!heap_object_listed(
            ObjectKind::Array,
            filler,
            ArrayElementType::Int,
            &known
        ));
        assert!(heap_object_listed(
            ObjectKind::Array,
            8,
            ArrayElementType::Int,
            &known
        ));
        assert!(heap_object_listed(
            ObjectKind::Array,
            7,
            ArrayElementType::Reference,
            &known
        ));
        assert!(!heap_object_listed(
            ObjectKind::Array,
            8,
            ArrayElementType::Reference,
            &known
        ));
    }
}

#[cfg(test)]
mod gci_i15rb_force_gc_entry_tests {
    /// gci i15/rb (`gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`
    /// findings 1 and 4): `ForceGarbageCollection` runs its collection (and
    /// the forced finalizer / cleaner drains behind it) through
    /// `on_calling_thread`, i.e. out of native under the JNI entry, and builds
    /// no context of its own; it is the VM's own full collection
    /// (`force_gc_for_vm`: no `-XX:+DisableExplicitGC`, no `jdk.SystemGC`, as
    /// HotSpot's `_jvmti_force_gc`); the tag sweep (which posts `ObjectFree`)
    /// comes after. (Source pin: a JNI in-native caller cannot be staged in a
    /// unit test of this module.)
    #[test]
    fn gci_i15rb_force_garbage_collection_leaves_native_first() {
        let src = include_str!("heap.rs").replace("\r\n", "\n");
        let start = src
            .find("pub(super) extern \"C\" fn force_garbage_collection(")
            .expect("force_garbage_collection");
        let rest = &src[start..];
        let body = &rest[..rest.find("\n}\n").expect("its end")];
        let gc = body
            .find("on_calling_thread(&shared, |ctx| ctx.force_gc_for_jvmti())")
            .expect("the collection runs under the calling-thread entry");
        let sweep = body.find("sweep_all_tags(&shared)").expect("the tag sweep");
        assert!(gc < sweep, "the sweep follows the collection");
        assert!(
            !body.contains("NativeContextImpl"),
            "no context outside the entry"
        );
        assert!(
            !body.contains("current_jvm_thread_of"),
            "no raw thread borrow"
        );
    }
}

#[cfg(test)]
mod tag_index_tests {
    //! i87-L1 (interpreter round i4 wave 92): the tag table's address index
    //! under a heap that moves objects without bumping its collection count.
    use super::*;

    /// A table of `tags` (weak handle, tag), indexed at `addrs` (handle to
    /// address) as a refresh at an unchanged count would have left it.
    fn table(exact: bool, tags: &[(JObject, i64)], addrs: &HashMap<JObject, usize>) -> TagTable {
        let mut t = TagTable {
            entries: tags.to_vec(),
            index: HashMap::new(),
            epoch: Some(7),
            exact,
        };
        t.reindex(&mut |w| addrs.get(&w).copied());
        t
    }

    #[test]
    fn a_relocation_between_counts_is_seen_when_exact() {
        let mut heap: HashMap<JObject, usize> = HashMap::from([(0x11, 0x1000), (0x21, 0x2000)]);
        let mut t = table(true, &[(0x11, 5), (0x21, 9)], &heap);
        // The object tagged 5 moves onto 0x3000, and the one tagged 9 onto
        // 0x1000 (the first's old address); the count does not change.
        heap.insert(0x11, 0x3000);
        heap.insert(0x21, 0x1000);
        let tag_at = |t: &mut TagTable, addr: usize, heap: &HashMap<JObject, usize>| {
            t.entry_of(addr, |w| heap.get(&w).copied())
                .map_or(0, |i| t.entries[i].1)
        };
        assert_eq!(
            tag_at(&mut t, 0x3000, &heap),
            5,
            "a moved object keeps its tag"
        );
        assert_eq!(
            tag_at(&mut t, 0x1000, &heap),
            9,
            "an old address names its new tenant"
        );
        assert_eq!(
            tag_at(&mut t, 0x2000, &heap),
            0,
            "a vacated address is untagged"
        );
        // A SetTag on the moved object finds its entry: no second entry.
        assert!(t.entry_of(0x3000, |w| heap.get(&w).copied()).is_some());
        assert_eq!(t.entries.len(), 2);
    }

    #[test]
    fn a_pause_collector_trusts_its_index() {
        let heap: HashMap<JObject, usize> = HashMap::from([(0x11, 0x1000)]);
        let mut t = table(false, &[(0x11, 5)], &heap);
        let mut resolved = 0;
        let hit = t.entry_of(0x1000, |w| {
            resolved += 1;
            heap.get(&w).copied()
        });
        assert_eq!(hit, Some(0));
        let miss = t.entry_of(0x2000, |w| {
            resolved += 1;
            heap.get(&w).copied()
        });
        assert_eq!(miss, None);
        assert_eq!(
            resolved, 0,
            "no weak reference is resolved off the exact path"
        );
    }

    #[test]
    fn a_cleared_reference_is_left_for_refresh() {
        let mut heap: HashMap<JObject, usize> = HashMap::from([(0x11, 0x1000), (0x21, 0x2000)]);
        let mut t = table(true, &[(0x11, 5), (0x21, 9)], &heap);
        heap.remove(&0x11);
        assert_eq!(t.entry_of(0x1000, |w| heap.get(&w).copied()), None);
        // Still held, so the next refresh frees it and posts its ObjectFree.
        assert_eq!(t.entries.len(), 2);
        assert_eq!(t.entry_of(0x2000, |w| heap.get(&w).copied()), Some(1));
    }

    /// gci i20/mr finding 8: an index entry past the end of `entries` (left
    /// by an untag that could not re-key the entry it moved) is a miss on
    /// both paths and is skipped by the walk's snapshot -- never an index
    /// out of bounds.
    #[test]
    fn gci_i20mr_a_stale_index_past_the_end_is_a_miss() {
        let heap: HashMap<JObject, usize> = HashMap::from([(0x11, 0x1000)]);
        for exact in [false, true] {
            let mut t = table(exact, &[(0x11, 5)], &heap);
            t.index.insert(0x4000, 1);
            assert_eq!(
                t.entry_of(0x4000, |w| heap.get(&w).copied()),
                None,
                "{exact}"
            );
            assert_eq!(
                t.entry_of(0x1000, |w| heap.get(&w).copied()),
                Some(0),
                "{exact}"
            );
            let mut t = table(exact, &[(0x11, 5)], &heap);
            t.index.insert(0x4000, 3);
            let snap = t.snapshot();
            assert_eq!(snap.get(&0x1000), Some(&5));
            assert!(!snap.contains_key(&0x4000));
        }
    }
}
