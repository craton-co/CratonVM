#[allow(unused_imports)]
use super::*;

impl<'a> NativeContextImpl<'a> {
    /// Shared body for the `redefine_class` / `retransform_class` trait
    /// methods. `preserve_original` controls whether the class's cached
    /// "original" bytes are kept (retransform) or replaced (redefine) — see
    /// [`cratonvm_classloading::RedefineOptions::preserve_original_bytes`].
    pub(in crate::vm::vm_exec) fn redefine_class_with(
        &mut self,
        class_id: ClassId,
        new_bytes: &[u8],
        preserve_original: bool,
    ) -> Result<(), cratonvm_classloading::RedefinitionRefused> {
        use cratonvm_classloading::{RedefineOptions, RedefinitionRefusal, RedefinitionRefused};
        let name = {
            let cm = self.shared.classes.class_manager.read();
            let cls = cm.class_store.get(class_id).ok_or_else(|| {
                RedefinitionRefused::new(
                    RedefinitionRefusal::InvalidClass,
                    class_id.as_u32().to_string(),
                    "class not loaded",
                )
            })?;
            cls.name.to_string()
        };
        // Interpreter round i6 wave 116, lane L1
        // (`CRATONVM_JDWP_SCOPED_FIELD_WATCHES`): a class whose NEW constant
        // pool names a watched field joins the field watch's scope before
        // its new code is installed, on every redefinition route (JDWP,
        // JVMTI, `java.lang.instrument`, the JFR retransform). One load with
        // no such watch; no lock held here.
        #[cfg(feature = "experimental-debug")]
        crate::debug::field_watch_scope::join_redefinitions(
            self.shared,
            &[(class_id, new_bytes)],
        );
        // Interpreter round i6 wave 117, lane L3 (the wave-28 note of
        // `i14-L3-proposal-class-scoped-redefinition-invalidation`;
        // `CRATONVM_REDEFINE_IDENTICAL_FAST_PATH`, default ON since the i6 wave-126 flip, `=0` off, performance):
        // a RETRANSFORM whose class file is byte-identical to the one this
        // path last installed for the class, at an unchanged redefine
        // generation, installs nothing. Mockito's inline mock maker
        // retransforms a mocked type's whole hierarchy (`java/lang/Object`
        // included, which takes the full code-cache flush) on every `mock()`
        // of a new type, weaving each class the same way every time. The code
        // that would run after the swap is the bytecode running now, so only
        // `classRedefinedCount` moves (HotSpot counts the redefinition and
        // reflection caches key on it). Restricted to retransforms: a
        // `redefineClasses` also replaces the retransformation base. A
        // generation moved by any other route (JDWP, a redefinition) misses.
        let identical_fast_path = preserve_original;
        if identical_fast_path {
            let generation = self
                .shared
                .classes
                .class_manager
                .read()
                .class_redefine_generation(class_id);
            let same = self
                .shared
                .classes
                .installed_retransform_files
                .lock()
                .get(&class_id)
                .is_some_and(|(bytes, at)| *at == generation && **bytes == *new_bytes);
            if same {
                if false {
                    eprintln!("[redefine] identical retransform of {name}: nothing installed");
                }
                super::vm_object::note_class_redefined_for_reflection(self.shared, class_id);
                // Wave 119 (lane L9's review): HotSpot clears the class's
                // breakpoints on an identical retransform too, as the slow
                // path below does.
                clear_breakpoints_of_redefined_class(self.shared, class_id);
                return Ok(());
            }
        }
        // Interpreter round i1 wave 23, lane L6: whether this redefinition
        // withdrew compiled bodies (below), whose running frames must then
        // leave at their next back-edge poll.
        let withdrawn_before = self.shared.jit.jit_cache.bodies_withdrawn_by_redefinition();
        // JEP 109 obsolete methods, window 1: when the new pool renumbers the
        // class's constants, no other thread runs an instruction of the old
        // body across the swap -- their loops wait at their top until the
        // fence drops below (interpreter round i1 wave 37, lane L3). Taken
        // before the writer; dropped after it, or on a refusal's return.
        let fence = crate::runtime::interpreter::obsolete_frames::RedefinitionFence::raise(
            self.shared,
            self.thread.thread_id,
            class_id,
            new_bytes,
        );
        let evicted = {
            let mut cm = self.shared.classes.class_manager_write();
            let options = RedefineOptions {
                preserve_original_bytes: preserve_original,
                ..RedefineOptions::default()
            };
            // The per-thread site caches (`interpreter::site_cache`) survive a
            // redefinition since interpreter round i1 wave 17
            // (`SITE_CACHES_SURVIVE_REDEFINITION`): what retires their entries
            // is the resolution epoch in every entry's tag. Advanced here,
            // under the writer and BEFORE the class is replaced, so no entry
            // filled from the old constant pool is served once the swap
            // begins; and again below, after `redefine_class` has swept the
            // shared resolution memo (its invalidate hook advances the epoch
            // BEFORE the sweep, so a fill could snapshot the new epoch and
            // still read an unswept row).
            crate::runtime::interpreter::bump_resolution_epoch_in(&self.shared);
            // The JIT invalidate hook `redefine_class` fires for this class
            // (step 8) leaves the code-cache flush to the call below instead
            // of flushing every live VM (`InPlaceRedefinitionScope`).
            use crate::vm::realms::jit_realm::InPlaceRedefinitionScope;
            // JEP 109: the threads' published frame snapshots (a blocked
            // thread's, for `Thread.getStackTrace`) take their lines in this
            // class's methods from the bodies they ran, before those bodies'
            // line tables are replaced (interpreter round i1 wave 22, lane
            // L3). Stored Throwable backtraces carry their capture count
            // instead (`obsolete_frames::resolve_lines_as_captured`).
            crate::runtime::interpreter::obsolete_frames::resolve_captured_lines_of_class(
                self.shared,
                &cm.class_store,
                class_id,
            );
            // The stale-frame census: history steps no thread's frames need
            // any more are dropped, and the steps a parked thread still needs
            // outlive the soft caps (interpreter round i1 wave 23, lane L3;
            // per class since wave 24). Lock order as above: class manager,
            // then the registry.
            crate::runtime::interpreter::obsolete_frames::prune_histories_by_census(
                self.shared,
                &mut cm,
            );
            let in_place = InPlaceRedefinitionScope::enter(class_id);
            cm.redefine_class_typed(class_id, new_bytes.to_vec(), options)?;
            drop(in_place);
            crate::runtime::interpreter::bump_resolution_epoch_in(&self.shared);
            // The compiled-`ldc` slots minted for this class's `(class, cp
            // index)` pairs hold the OLD pool's constants. Dropped from the
            // slot table here, after the sweep and BEFORE the code-cache flush
            // below, so no compile that starts after the flush can bake one
            // (interpreter round i1 wave 18, lane L4).
            crate::jit::helpers::retire_ldc_slots_of_class(&self.shared, class_id);
            // r23 w4b review: and the condy-handle members keyed by the same
            // `(class, cp index)` pairs (`CRATONVM_JIT_MH_CONSTANT_CONDY`).
            crate::runtime::interpreter::jit_bridge::retire_mh_condy_members_of_class(
                &self.shared,
                class_id,
            );
            // JVMTI redefinition can stale caller-side direct calls and inline
            // dispatch caches, not just compiled bodies declared by `name`.
            // Since interpreter round i1 wave 20 (lane L1) only the bodies
            // that depend on the class are withdrawn when nothing unrecorded
            // can (`JitRealm::scoped_redefinition_admitted`); a class with a
            // registered native keeps the full flush, because its bytecode now
            // wins over the native a compiled caller may have bound.
            // Invalidate every inline cache exactly once. Slots compare their
            // stamp against this counter on their next dispatch and flush then;
            // nothing pays a per-call cost afterwards. Must be bumped alongside
            // the cache clear so no slot can validate against a pre-clear epoch.
            let has_registered_natives = cm.get_class(class_id).is_some_and(|class| {
                class.methods.iter().any(|m| {
                    self.shared
                        .natives
                        .native_methods
                        .find_with_kind(&name, &m.name, &m.descriptor)
                        .is_some()
                })
            });
            // The stale compiled bodies the call below withdraws are made NOT ENTRANT: their
            // entries jump to a stub that re-dispatches through this VM
            // (`jit_not_entrant_entry`; interpreter round i1 wave 22, lane L6).
            // Arming is idempotent; the address is this VM's own.
            self.shared
                .jit
                .jit_cache
                .arm_not_entrant(cratonvm_jit::NotEntrantHook {
                    // Cast: a helper's address, baked into generated code.
                    helper: crate::jit::helpers::jit_not_entrant_entry as *const () as usize,
                    // Cast: this VM's address, the helpers' `vm_ptr` word.
                    vm_ptr: self.shared as *const crate::vm::SharedVm as usize,
                });
            // `note_class_redefinition_of` does `bump_redefine_epoch` and
            // the eviction, then moves THIS VM's own epochs: its compile verdicts
            // (bail list, OSR entry rejects), runtime de-speculation, and the
            // code-state epoch the tiered manager, the OSR refusal budget and
            // the optimizing OSR memo read — so another VM's redefinition
            // expires none of them — and feeds the redefinition census on the
            // `jit-method-stats` line. Still under the writer, where the
            // invalidate hook's flush used to run.
            // Interpreter round i1 wave 45, lane L2: whether this redefinition
            // left some old constant-pool index naming another constant. An
            // OSR body of the class's own old bytecode then leaves for the
            // interpreter instead of translating its constant-pool sites
            // (`cratonvm_jit::not_entrant::OWN_CLASS_RENUMBERED_OSR_BODIES_LEAVE`).
            let renumbered = cm
                .redefinition_history(class_id)
                .is_some_and(|history| history.last_redefinition_moved_constants());
            let evicted = self.shared.jit.note_class_redefinition_renumbering(
                class_id,
                &name,
                has_registered_natives,
                renumbered,
            );
            // Once more after the flush: a cache keyed on a caller's `(class,
            // cp index)` that filled between `redefine_class`'s last advance
            // and the flush (a compiled body of the old class was still
            // published then) is retired on its next lookup. Recorded in this
            // VM's own redefinition log too (nio21 wave 3 L1), which is what
            // the invoke caches bound to it read.
            cm.bump_redefinition_count_of(class_id);
            evicted
        };
        // The writer is released and the redefinition recorded: the held
        // loops move their frames, and every other running loop at its next
        // top (the drop arms every loop's poll word).
        drop(fence);
        // PERF-cache correctness: `resolve_field_descriptor_byte_cached`
        // memoizes per-(ClassId, slot) field descriptors (incl. negative
        // results). JEP 109 redefinition is required to preserve field layout
        // so cached entries SHOULD stay valid, but clear the cache here as a
        // conservative guard against any future relaxation of that invariant.
        // Redefinition is rare, so the rebuild cost is negligible.
        self.shared.classes.field_descriptor_cache.write().clear();
        // `Class.classRedefinedCount` of the class and its subclasses, and the
        // annotation natives' proxy cache: the reflection caches keyed on the
        // class start over, as on HotSpot (interpreter round i1 wave 27, lane
        // L3).
        super::vm_object::note_class_redefined_for_reflection(self.shared, class_id);
        // Interpreter round i6 wave 119, lane L3: the class's breakpoints, as
        // HotSpot's `VM_RedefineClasses` clears them on every route.
        clear_breakpoints_of_redefined_class(self.shared, class_id);
        // JIT round 21 wave 17, lane innerclasses: the redefinition may have
        // installed new `InnerClasses` / `EnclosingMethod` tables
        // (`CRATONVM_REDEFINE_SWAPS_INNER_CLASSES`), so the natives' answers
        // built from the old ones go: the spoof verdicts, the simple and
        // canonical names, the class's modifiers. After the writer is
        // released: the memos' locks are leaves.
        cratonvm_native_builtins::lang_class::forget_redefined_inner_class_answers(&*self, class_id);
        if evicted > 0 {
            tracing::debug!("JIT: invalidated {evicted} method(s) due to redefineClass: {name}");
        }
        // Every tiering verdict recorded against the old bytecode (ineligible,
        // c2_bailout, trap counts, OSR denials) says nothing about the new one.
        // r10 lane `tier2`: the profile store is keyed by (class_id, method,
        // descriptor) and `class_id` is STABLE across a redefine, so without this the
        // new bytecode's execution blends into branch ratios and receiver tables
        // gathered from the OLD bytecode. The unload path in `memory/gc.rs` has always
        // invalidated both; redefine invalidated only the tiering verdicts.
        let profile_store = &self.shared.jit.profile_store;
        profile_store.invalidate_class(class_id.as_u32());
        self.shared
            .jit
            .tiered_manager
            .on_class_redefined(class_id, &name);
        crate::runtime::frozen_pages::invalidate_class(self.shared, class_id.as_u32());
        // JEP 109 obsolete methods: a frame still running a body this
        // redefinition replaced moves onto its copy translated into the merged
        // pool -- this thread's now, every other running thread's at the
        // handshake pause this takes, a blocked thread's when it leaves its
        // blocking region (interpreter round i1 wave 19, lane L3).
        let every_peer_polled = crate::runtime::interpreter::obsolete_frames::after_redefinition(
            self.shared,
            self.thread,
            class_id,
        );
        // A compiled frame already running a body this redefinition withdrew
        // -- one that spliced the redefined class's old bytecode -- leaves for
        // the interpreter at its next back-edge poll; this pause makes every
        // running body take its poll's slow path once (interpreter round i1
        // wave 23, lane L6; `jvmti_events::withdrawn_body_may_leave`). Not a
        // second pause when the one above already made every peer poll after
        // the withdrawal (wave 24, lane L6).
        // Wave 25 (lane L6): inside a batch (one `retransformClasses` call),
        // deferred to the batch's end, which takes one pause for all of its
        // classes (`end_redefinition_batch`); a call on N classes used to take
        // up to N pauses (2N until wave 26, when each class was also redefined
        // back to its retransformation base first). A frame running a
        // withdrawn body leaves at its next exit-capable back edge meanwhile
        // anyway (its polls are forced, `JitCache::force_withdrawn_exit_polls`).
        if !every_peer_polled
            && self.thread.redefinition_batch.is_none()
            && self.shared.jit.jit_cache.bodies_withdrawn_by_redefinition() != withdrawn_before
        {
            crate::runtime::interpreter::request_withdrawn_body_exits(
                self.shared,
                self.thread.thread_id,
                withdrawn_before,
            );
        }
        // Wave 117 (`CRATONVM_REDEFINE_IDENTICAL_FAST_PATH`): what this
        // retransform installed, for the next one's comparison.
        if identical_fast_path {
            let generation = self
                .shared
                .classes
                .class_manager
                .read()
                .class_redefine_generation(class_id);
            self.shared
                .classes
                .installed_retransform_files
                .lock()
                .insert(class_id, (std::sync::Arc::from(new_bytes), generation));
        }
        Ok(())
    }
}

/// What HotSpot's `VM_RedefineClasses::redefine_single_class` does to the
/// breakpoints of a class it redefines, whatever the route and even for
/// byte-identical class files (`JvmtiBreakpoints::clearall_in_class_at_safepoint`):
/// every JVMTI breakpoint in the class goes, and so a JDWP breakpoint
/// request located there stops firing while its request stays registered
/// (the back end is not told; only JDWP's own `RedefineClasses` also frees
/// the requests, which `debug::redefine_classes` does after this). MEASURED,
/// HotSpot 25.0.3 (`L3W119RetransformClearsBreakpoints.java`):
/// one hit, not three. Before wave 119 this VM kept both kinds across an
/// agent's redefinition or retransformation. `CRATONVM_REDEFINE_CLEARS_BREAKPOINTS=0`
/// keeps them (parity, default on; read per redefinition, a rare event).
/// Nothing without a debugger build. Interpreter round i6 wave 119, lane L3.
pub(in crate::vm::vm_exec) fn clear_breakpoints_of_redefined_class(shared: &SharedVm, class_id: ClassId) {
    #[cfg(feature = "experimental-debug")]
    {
        let raw = u64::from(class_id.as_u32());
        let jvmti = crate::runtime::jvmti::clear_class_breakpoints_for_vm(shared, raw);
        let mut ds = shared.debug.debug_state.lock();
        let jdwp = ds.events.disarm_class_breakpoints(raw);
        if jvmti + jdwp > 0 {
            crate::debug::publish_debugger_gates(shared, &ds);
        }
    }
    #[cfg(not(feature = "experimental-debug"))]
    {
        let _ = (shared, class_id);
    }
}



/// A refused redefinition as `java.lang.instrument` reports it: libinstrument's
/// throwable for the refusal (`RedefinitionRefusal::instrument_throwable`) and
/// the class manager's account of it (interpreter round i1 wave 27, lane L3).
#[cold]
pub(in crate::vm::vm_exec) fn instrument_refusal_of(
    refused: cratonvm_classloading::RedefinitionRefused,
) -> cratonvm_native_api::InstrumentRefusal {
    let (throwable, message) = refused.kind.instrument_throwable();
    cratonvm_native_api::InstrumentRefusal {
        throwable,
        message,
        detail: format!("{:?}", refused.into_linkage_error()),
    }
}

/// CRATONVM_DBG_BLOCKED_ACCESS: see `pin_native_root` — pins pushed while
/// the thread's `in_blocked_region` flag is raised are invisible to the root
/// scan and the blocked fold. One relaxed load when the gate is unarmed.
#[inline]
pub(in crate::vm::vm_exec) fn check_blocked_pin_push(thread: &JvmThread, values: &[Value]) {
    if cratonvm_gc::blocked_access_debug::enabled()
        && thread
            .gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire)
        && values.iter().any(|v| matches!(v, Value::Object(Some(_))))
    {
        cratonvm_gc::blocked_access_debug::report_blocked_violation(
            "pin_native_object_values push on a census-excluded thread",
            0,
        );
    }
}

pub(in crate::vm::vm_exec) fn pin_native_object_values(thread: &mut JvmThread, values: &[Value]) -> Vec<Option<usize>> {
    check_blocked_pin_push(thread, values);
    let mut handles = Vec::with_capacity(values.len());
    for value in values {
        if let Value::Object(Some(obj)) = value {
            let handle = thread.native_pin_roots.len();
            thread.native_pin_roots.push(*obj);
            handles.push(Some(handle));
        } else {
            handles.push(None);
        }
    }
    handles
}

/// Keeps the argument vector of a recursive VM dispatch rooted until the
/// dispatch either reaches a scanned Java frame or returns to its caller.
///
/// Unlike `safe_native_call`, class/method resolution happens before a native
/// or bytecode target is known. It can re-enter Java and collect while its
/// `Vec<Value>` argument copy is otherwise invisible to the collector. The
/// guard pins every object entry and can restore the collector-forwarded value
/// before the terminal receiver-based retry.
///
/// The `k`-th pinned object sits at `pin_base + k` (the pins are pushed
/// consecutively), so the guard records only which argument positions were
/// pinned — inline for up to eight object arguments, no heap allocation per
/// dispatch (wave 15; it used to allocate a `Vec<Option<usize>>` of handles).
pub(in crate::vm::vm_exec) struct PinnedDispatchArgs {
    pub(in crate::vm::vm_exec) thread: *mut JvmThread,
    pub(in crate::vm::vm_exec) pin_base: usize,
    pub(in crate::vm::vm_exec) positions: smallvec::SmallVec<[u32; 8]>,
    pub(in crate::vm::vm_exec) previous_window: Option<(usize, usize)>,
}

impl PinnedDispatchArgs {
    pub(in crate::vm::vm_exec) fn new(thread: &mut JvmThread, values: &[Value]) -> Self {
        check_blocked_pin_push(thread, values);
        let pin_base = thread.native_pin_roots.len();
        let mut positions = smallvec::SmallVec::new();
        for (index, value) in values.iter().enumerate() {
            if let Value::Object(Some(obj)) = value {
                thread.native_pin_roots.push(*obj);
                positions.push(index as u32);
            }
        }
        let previous_window = thread
            .by_name_dispatch_pin_window
            .replace((pin_base, positions.len()));
        Self {
            thread: thread as *mut JvmThread,
            pin_base,
            positions,
            previous_window,
        }
    }

    pub(in crate::vm::vm_exec) fn refresh(&self, values: &mut [Value]) {
        // SAFETY: the guard is lexically scoped to its dispatch invocation;
        // the owning JvmThread outlives it and remains exclusively driven by
        // that invocation between stop-the-world collections.
        let thread = unsafe { &*self.thread };
        for (k, &position) in self.positions.iter().enumerate() {
            if let Some(value) = values.get_mut(position as usize) {
                if matches!(&*value, Value::Object(Some(_))) {
                    if let Some(forwarded) = thread.native_pin_roots.get(self.pin_base + k).copied()
                    {
                        *value = Value::Object(Some(forwarded));
                    }
                }
            }
        }
    }

    /// gch h1/fr (2026-10-08, `gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes`):
    /// release this dispatch's pins before a tail that roots its own copy of
    /// the arguments (`interpreter::execute` pins at entry, before any
    /// safepoint, and drops its pins once the callee's frame holds them).
    /// Only when they are the top of the pin stack (nothing nested still
    /// needs a slot above them) and the thread runs no native (an excused
    /// thread can meet a collection between this release and `execute`'s
    /// pin). `Drop` then truncates to the same watermark, a no-op. Answers
    /// whether it released.
    pub(in crate::vm::vm_exec) fn release_before_rooting_tail(&self, thread: &mut JvmThread) -> bool {
        if thread.native_pin_roots.len() != self.pin_base + self.positions.len()
            || thread.running_native != 0
            || thread.native_excuse_raised != 0
        {
            return false;
        }
        thread.native_pin_roots.truncate(self.pin_base);
        true
    }
}

impl Drop for PinnedDispatchArgs {
    fn drop(&mut self) {
        // SAFETY: see `refresh`; all nested pin scopes have returned before
        // this lexical owner is dropped.
        unsafe {
            let thread = &mut *self.thread;
            thread.by_name_dispatch_pin_window = self.previous_window;
            thread.native_pin_roots.truncate(self.pin_base);
        }
    }
}

pub(in crate::vm::vm_exec) fn reread_native_object_values(
    thread: &JvmThread,
    values: &[Value],
    handles: &[Option<usize>],
) -> Vec<Value> {
    values
        .iter()
        .zip(handles.iter())
        .map(|(value, handle)| match (value, handle) {
            (Value::Object(Some(fallback)), Some(handle)) => Value::Object(Some(
                thread
                    .native_pin_roots
                    .get(*handle)
                    .copied()
                    .unwrap_or(*fallback),
            )),
            _ => *value,
        })
        .collect()
}

thread_local! {
    /// DIAGNOSTIC-ONLY (cceres3, CRATONVM_DBG_UNPIN_RING): last few pin-stack
    /// truncations with backtraces; dumped by the PIN-DANGLING canary.
    pub(in crate::vm::vm_exec) static UNPIN_RING: std::cell::RefCell<Vec<(usize, usize, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

pub(in crate::vm::vm_exec) fn unpin_ring_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

pub(in crate::vm::vm_exec) struct VmNativeThreadBlocker {
    pub(in crate::vm::vm_exec) shared: std::sync::Arc<SharedVm>,
    pub(in crate::vm::vm_exec) thread_id: ThreadId,
}

/// Payload size in bytes of a primitive array, or `None` when `arr` is
/// not one.
///
/// Reference arrays return `None` deliberately: the byte-level bulk
/// accessors must never touch them, because writing raw bytes over
/// object references would hand the collector pointers it never issued.
#[allow(dead_code)]
pub(in crate::vm::vm_exec) fn primitive_array_byte_capacity_of(
    heap: &cratonvm_gc::vm_heap::VmHeap,
    arr: cratonvm_types::ObjectRef,
) -> Option<usize> {
    use cratonvm_types::{ArrayElementType, ObjectKind};
    if heap.kind_of(arr) != ObjectKind::Array {
        return None;
    }
    let et = heap.element_type_of_validated(arr);
    if et == ArrayElementType::Reference {
        return None;
    }
    let width = cratonvm_types::element_byte_size(et);
    heap.array_length(arr).checked_mul(width)
}

/// nio21 wave 6 L3 (`docs/feature-designs/real-jdk-mode.md`):
/// a read-modify-write of `width` (2, 4 or 8) bytes at a MISALIGNED payload
/// address as x86-64 `LOCK CMPXCHG` -- what HotSpot emits for the same
/// `Unsafe` call -- so it is atomic against plain element stores to those bytes
/// and against an overlapping aligned hardware CAS, which the process lock it
/// replaces was not. `f` as in `primitive_array_atomic_update`.
///
/// A misaligned locked op inside one cache line is an ordinary locked op. One
/// that crosses a 64-byte line is a SPLIT LOCK: still atomic, but a Linux
/// kernel with `split_lock_detect` may throttle the thread (the default
/// `warn` mode) or send `SIGBUS` (`fatal`), which would kill the VM where the
/// process lock merely serialises. That shape stays on the caller's lock only
/// under `CRATONVM_UNSAFE_SPLIT_LOCK_CAS=0` (default ON since JIT round 20: the
/// process-lock arm lost an update, `R20AtomSplitLock`). `None` (the caller keeps
/// its lock) also under `CRATONVM_UNSAFE_MISALIGNED_HW_CAS=0` and on every
/// other architecture.
///
/// # Safety
/// `addr .. addr + width` must be live, writable heap payload that cannot move
/// for the duration of the call.
#[cfg(target_arch = "x86_64")]
pub(in crate::vm::vm_exec) unsafe fn misaligned_locked_update(
    addr: *mut u8,
    width: usize,
    f: &dyn Fn(u64) -> Option<u64>,
) -> Option<(u64, bool)> {
    {
        if !matches!(width, 2 | 4 | 8)
            || !crate::runtime::env_cache::unsafe_misaligned_hw_cas_enabled()
        {
            return None;
        }
        if misaligned_access_splits_a_line(addr as usize, width)
            && !crate::runtime::env_cache::unsafe_split_lock_cas_enabled()
        {
            return None;
        }
        let mask = if width == 8 {
            u64::MAX
        } else {
            (1u64 << (width * 8)) - 1
        };
        // SAFETY: the caller's contract; every access below is one locked
        // instruction of `width` bytes at `addr`.
        let mut cur = unsafe { x86_locked_load(addr, width) } & mask;
        loop {
            let Some(new) = f(cur) else {
                return Some((cur, false));
            };
            // SAFETY: as above.
            let seen = unsafe { x86_locked_cas(addr, width, cur, new & mask) } & mask;
            if seen == cur {
                return Some((cur, true));
            }
            cur = seen;
        }
    }
}

/// [`misaligned_locked_update`] off x86-64: the caller keeps its lock.
///
/// # Safety
/// None needed; nothing is accessed.
#[cfg(not(target_arch = "x86_64"))]
pub(in crate::vm::vm_exec) unsafe fn misaligned_locked_update(
    _addr: *mut u8,
    _width: usize,
    _f: &dyn Fn(u64) -> Option<u64>,
) -> Option<(u64, bool)> {
    None
}

/// Whether a `width`-byte access at `addr` crosses a 64-byte cache line (a
/// split lock when issued with `LOCK`).
#[inline]
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(in crate::vm::vm_exec) fn misaligned_access_splits_a_line(addr: usize, width: usize) -> bool {
    (addr % 64) + width > 64
}

/// An atomic load of `width` bytes at `addr` of any alignment: `LOCK XADD`
/// of zero, which writes back the value it read.
///
/// # Safety
/// As [`misaligned_locked_update`].
#[cfg(target_arch = "x86_64")]
pub(in crate::vm::vm_exec) unsafe fn x86_locked_load(addr: *mut u8, width: usize) -> u64 {
    let mut v: u64 = 0;
    // SAFETY: the caller's contract.
    unsafe {
        match width {
            2 => core::arch::asm!(
                "lock xadd word ptr [{p}], {v:x}",
                p = in(reg) addr,
                v = inout(reg) v,
                options(nostack)
            ),
            4 => core::arch::asm!(
                "lock xadd dword ptr [{p}], {v:e}",
                p = in(reg) addr,
                v = inout(reg) v,
                options(nostack)
            ),
            _ => core::arch::asm!(
                "lock xadd qword ptr [{p}], {v:r}",
                p = in(reg) addr,
                v = inout(reg) v,
                options(nostack)
            ),
        }
    }
    v
}

/// `LOCK CMPXCHG` of `width` bytes at `addr` of any alignment: answers the
/// value witnessed (equal to `expected` exactly when it stored `new`). Upper
/// bits above `width` are unspecified; the caller masks.
///
/// # Safety
/// As [`misaligned_locked_update`].
#[cfg(target_arch = "x86_64")]
pub(in crate::vm::vm_exec) unsafe fn x86_locked_cas(addr: *mut u8, width: usize, expected: u64, new: u64) -> u64 {
    let mut w: u64 = expected;
    // SAFETY: the caller's contract.
    unsafe {
        match width {
            2 => core::arch::asm!(
                "lock cmpxchg word ptr [{p}], {n:x}",
                p = in(reg) addr,
                n = in(reg) new,
                inout("rax") w,
                options(nostack)
            ),
            4 => core::arch::asm!(
                "lock cmpxchg dword ptr [{p}], {n:e}",
                p = in(reg) addr,
                n = in(reg) new,
                inout("rax") w,
                options(nostack)
            ),
            _ => core::arch::asm!(
                "lock cmpxchg qword ptr [{p}], {n:r}",
                p = in(reg) addr,
                n = in(reg) new,
                inout("rax") w,
                options(nostack)
            ),
        }
    }
    w
}


impl NativeThreadBlocker for VmNativeThreadBlocker {
    /// gcj j3/x2 R7: also the moment the thread becomes STW-countable. Under
    /// `CRATONVM_NATIVE_THREAD_TERMINAL_BLOCK` the spawner registered it
    /// STARTING (`register_native_thread`), because it allocates the carrier
    /// mirror -- which may collect -- before its OS thread exists: an entry
    /// counted from the registration was waited for by a pause in that window
    /// (even one the spawner itself started) while its spawn sat behind the
    /// pause. Its first act on its own thread, here, marks it ready exactly as
    /// a `Thread.start()` worker does: only while no pause is requested, and
    /// waiting each one out as an excluded thread.
    fn publish_os_tid(&self) {
        let registry = &self.shared.threads.thread_registry;
        registry.set_os_tid_current(self.thread_id);
        if !registry.is_alive(self.thread_id) || registry.is_stw_ready(self.thread_id) {
            return;
        }
        let barrier = &self.shared.mem.gc_barrier;
        while !barrier.run_if_no_stw_requested(|| registry.mark_stw_ready(self.thread_id)) {
            // No frames and no TLAB yet: the pause's pointer map has nothing
            // of this thread's to rewrite.
            let _ = barrier.arrive_and_wait_excluded(self.thread_id);
        }
    }

    fn enter_blocked(&self) {
        self.shared
            .threads
            .thread_registry
            .mark_native_thread_blocked(self.thread_id);
        let pre_stw = self.shared.mem.gc_barrier.mark_blocked_region_enter();
        if pre_stw {
            // GCAUDIT-0711-FIX (finding 1a): `mark_native_thread_blocked`
            // above already raised `in_blocked_region`, so `_auto`.
            let _ = self
                .shared
                .mem
                .gc_barrier
                .arrive_and_wait_auto(self.thread_id);
        }
    }

    fn leave_blocked(&self) {
        // JDWP (interpreter round i1 wave 18, lane L3): a suspended native
        // thread stops at its return from the blocking call, still GC-blocked,
        // as HotSpot stops a thread in native at its next transition. One load
        // while no suspension is in force.
        #[cfg(feature = "experimental-debug")]
        crate::debug::hold_native_thread_while_suspended(&self.shared, self.thread_id.0);
        self.shared.mem.gc_barrier.mark_blocked_region_leave();
        // CENSUS-RECONCILE (2026-07-31): clear the IDENTITY flag through the
        // barrier, not with `mark_native_thread_unblocked`'s bare
        // `store(false)`. `mark_blocked_region_leave` above only waits out the
        // pause that was active when it ran; a pause requested in the window
        // between that and the store would both EXCLUDE this thread (its
        // census reads the still-raised flag) and let it run — finding 1(c)'s
        // "excluded but running mutator" corruption window.
        // `leave_blocked_region_flagged` closes it by clearing under the same
        // lock hold that proved no pause is active. The
        // `mark_native_thread_unblocked` call is kept for its side-table drain
        // (fixup / slot_origins / snapshot); its own store is then a no-op.
        if let Some(gc_block_state) = self
            .shared
            .threads
            .thread_registry
            .gc_block_state_of(self.thread_id)
        {
            self.shared
                .mem
                .gc_barrier
                .leave_blocked_region_flagged(self.thread_id, &gc_block_state.in_blocked_region);
        }
        self.shared
            .threads
            .thread_registry
            .mark_native_thread_unblocked(self.thread_id);
    }

    /// gcj j2/cm2 (`CRATONVM_NATIVE_THREAD_TERMINAL_BLOCK`, default ON;
    /// `gcj-j1cm-safepoint-barrier-and-registry-review-residuals` item 1).
    /// gcj j3/rt3 residual 5 (proposal
    /// `gcj-j2cm2-proposal-native-threads-own-their-death`, steps 1 and 2):
    /// every backend, and the only death path -- the process-wide dead queues
    /// and their drains are deleted.
    ///
    /// A registered native-builtins thread (XNIO I/O thread, Vert.x event loop)
    /// leaves its last blocked region when its loop ends, then queued its death
    /// and its OS thread finished; the VM marked it dead only when some later
    /// native call drained that queue (`unregister_native_thread`). A census
    /// taken between the loop's end and the OS thread's finish
    /// (`entry_is_stw_live` then still reads it live) COUNTED it, and it never
    /// arrives: it runs no Java, polls no safepoint, the take-over keeps only a
    /// peer whose `Rip` is in compiled code (`take_over_pass_split`,
    /// `Take::NotJit`), and once the OS thread has finished the roster omits
    /// it, so nothing reduces `expected` -- the pause waited for it forever.
    ///
    /// Now the thread retires itself here, as a dying platform thread does
    /// (`vm_exec`'s thread-termination `term_blk.finish_after`): it enters
    /// the blocked population (identity flag first, so every later census
    /// excludes it; arriving if a pause is already up), then leaves it and
    /// goes dead in one step under the barrier lock, after waiting out any
    /// pause in progress (`mark_blocked_region_leave_after`), and releases the
    /// monitors it holds -- all under the VM that registered it, so no other
    /// VM can be handed its per-VM id (the defect of the process-wide queues
    /// this replaced, which also left `Thread.isAlive()` true for a finished
    /// carrier until some later native call drained them; HotSpot ends a
    /// thread when it terminates).
    ///
    /// `=0`: the thread still dies here, but without the blocked-region
    /// protocol (a plain `mark_dead` and the monitor release, what the old
    /// drain did) -- a census racing it can count it, as before the switch.
    fn retire(&self) -> bool {
        let registry = &self.shared.threads.thread_registry;
        if !registry.is_alive(self.thread_id) {
            return false;
        }
        self.enter_blocked();
        self.shared
            .mem
            .gc_barrier
            .mark_blocked_region_leave_after(|| registry.mark_dead(self.thread_id));
        self.shared
            .threads
            .monitors
            .release_monitors_held_by(self.thread_id);
        true
    }
}



/// gcj j2/cm2: the retirement of a registered native thread at its loop's end.


/// Read the real-JDK `java.lang.Thread.tid` field from a Thread mirror.
///
/// `tid` is `final`, assigned once in the constructor from a process-wide
/// monotonic counter, and never reused across Thread objects — making it an
/// aliasing-proof identity key where the mirror's heap address is not (the
/// collector recycles a dead mirror's address for new allocations; see the
/// DoHead engine-start `IllegalThreadStateException` incident). Returns
/// `None` for synthetic-layout threads (no `tid` field in the hierarchy),
/// for mirrors whose constructor hasn't assigned it yet (reads 0), or when
/// the slot doesn't read back as a `Long` (don't trust a corrupt layout).
pub(crate) fn read_java_thread_tid(shared: &SharedVm, thread_obj: ObjectRef) -> Option<u64> {
    // Callers pass mirrors that can be stale (an unpark racing a moving GC);
    // don't dereference anything that isn't a live heap address.
    shared.mem.heap.is_heap_addr(thread_obj.as_ptr() as usize)?;
    let header = shared.mem.heap.get_header(thread_obj);
    // Round 15 wave 4 (lane threadvm): `java.lang.Thread`'s own `tid`
    // (`thread_field_owner`), not a subclass's `long tid`, which keyed the
    // registry's identity index and the JMX ids on an application value.
    // Round 15 wave 5 (lane threadvm2; TVM-4): through the layout memo, so an
    // unpark or interrupt no longer takes the class-manager lock.
    let slot = thread_field_slot(shared, header.class_id, "tid")?;
    match shared.mem.heap.get_field(thread_obj, slot) {
        Value::Long(v) if v > 0 => Some(v as u64),
        _ => None,
    }
}

/// Is the JMX ownable-synchronizer index being maintained?
///
/// **Default yes.** `CRATONVM_JMX_OWNED_SYNCHRONIZERS=off` stops the writes —
/// see [`NativeContextImpl::record_jmx_owned_synchronizer`] for what that costs
/// in fidelity and why the lever exists. Latched once: the answer must not be
/// able to change mid-run, or the index would hold a partial history that no
/// reader could interpret.
pub(in crate::vm::vm_exec) fn jmx_owned_synchronizer_index_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        match cratonvm_types::flags::runtime_var("CRATONVM_JMX_OWNED_SYNCHRONIZERS") {
            Ok(v) => {
                let v = v.trim();
                !(v.eq_ignore_ascii_case("off") || v == "0" || v.eq_ignore_ascii_case("false"))
            }
            Err(_) => true,
        }
    })
}

/// `Unsafe.unpark` of a PLATFORM thread in one registry lookup (interpreter
/// round i3 wave 72, lane L6, `CRATONVM_UNPARK_ONE_LOOKUP`; page
/// `interpreter-i71-L1-proposal-a-running-phase-before-a-blocking-park-DONE-20261224`,
/// follow-up (c)). The default path reads the mirror's `tid` twice and takes
/// the `java_tid` index twice, the virtual-thread manager's `threads` lock
/// and the registry's `threads` lock (`resolve_thread_id_from_thread_obj`,
/// `is_virtual`, `find_park_state_for_thread_obj`); this reads `tid` once,
/// asks `ThreadRegistry::unpark_target_by_java_tid` once and the manager only
/// once a virtual thread exists. `true`: unparked. `false` (no `tid`, not in
/// the `tid` index, or a virtual thread): nothing was done and the caller
/// takes the full path, whose answer for these cases is unchanged.
#[inline(never)]
pub(in crate::vm::vm_exec) fn unpark_platform_in_one_lookup(shared: &SharedVm, thread_obj: ObjectRef) -> bool {
    let Some(java_tid) = read_java_thread_tid(shared, thread_obj) else {
        return false;
    };
    let Some((tid, park_state)) = shared
        .threads
        .thread_registry
        .unpark_target_by_java_tid(java_tid)
    else {
        return false;
    };
    let vtm = &shared.threads.virtual_thread_manager;
    if vtm.may_have_virtual_threads() && vtm.is_virtual(tid.0) {
        return false;
    }
    unpark_park_state(shared, &park_state);
    true
}

/// The registry id a live mirror's `Thread.eetop` names
/// ([`live_eetop`]'s inverse), or `None`: no real `eetop` field (a
/// synthetic `Thread`), a mirror that is not a live heap address (an unpark
/// racing a moving GC), or `eetop` not a live value (`0`: not started, or
/// terminated).
pub(in crate::vm::vm_exec) fn thread_id_from_eetop(shared: &SharedVm, thread_obj: ObjectRef) -> Option<ThreadId> {
    shared.mem.heap.is_heap_addr(thread_obj.as_ptr() as usize)?;
    let header = shared.mem.heap.get_header(thread_obj);
    let slot = thread_field_slot(shared, header.class_id, "eetop")?;
    match shared.mem.heap.get_field(thread_obj, slot) {
        Value::Long(v) if v >= live_eetop(ThreadId(0)) => {
            Some(ThreadId((v - live_eetop(ThreadId(0))) as u64))
        }
        _ => None,
    }
}

/// `Unsafe.unpark` of a PLATFORM thread through `Thread.eetop` (interpreter
/// round i3 wave 73, lane L6, `CRATONVM_UNPARK_EETOP`; page
/// `i72-L6-proposal-unpark-through-eetop-20261105`). HotSpot's
/// `Unsafe_Unpark` reads `java_lang_Thread::thread` (the `eetop` word) and
/// unparks that thread's `Parker`; the VM's `eetop` is `0x1000 + ThreadId`
/// for a live thread ([`set_thread_state_fields`]), so the registry id comes
/// from one field load and only the registry's `threads` READ lock is taken
/// (`ThreadRegistry::get_park_state`), not the VM-global `java_tid` index
/// mutex [`unpark_platform_in_one_lookup`] takes. The id is per thread, not
/// per address, so a recycled mirror address cannot misroute it. `true`:
/// unparked. `false` (no live `eetop`, no registry entry, or a virtual
/// thread): nothing was done and the caller takes the next path, whose
/// answer for these cases is unchanged.
#[inline(never)]
pub(crate) fn unpark_platform_through_eetop(shared: &SharedVm, thread_obj: ObjectRef) -> bool {
    let Some(tid) = thread_id_from_eetop(shared, thread_obj) else {
        return false;
    };
    let vtm = &shared.threads.virtual_thread_manager;
    if vtm.may_have_virtual_threads() && vtm.is_virtual(tid.0) {
        return false;
    }
    let Some(park_state) = shared.threads.thread_registry.get_park_state(tid) else {
        return false;
    };
    unpark_park_state(shared, &park_state);
    true
}

/// May a park's `jdk.ThreadPark` event be wanted at its entry? A recording on
/// the `jdk.jfr`-driven backend (`jfr_vm_events`): one relaxed load, read
/// once at a park's entry, as HotSpot's `EventThreadPark` constructor tests
/// `is_enabled` once (interpreter round i3 wave 73, lane L6). The legacy
/// recorder (`cratonvm_jfr::is_enabled`) is asked only once the park has
/// passed its permit fast path ([`legacy_park_event_wanted`]), so the fast
/// path pays this one load and nothing else.
#[inline(always)]
pub(in crate::vm::vm_exec) fn park_event_wanted() -> bool {
    cratonvm_native_builtins_jfr::jfr_vm_events::any_recording()
}

/// The legacy recorder's half of [`park_event_wanted`], for a park that
/// waits (its running phase or its blocking path).
#[inline(always)]
pub(in crate::vm::vm_exec) fn legacy_park_event_wanted() -> bool {
    cratonvm_jfr::is_enabled()
}

/// The wait of a relative Java-level park of `t` on Windows, as HotSpot 25's
/// `Parker::park` (os_windows.cpp) makes it: whole milliseconds, truncated
/// (`time /= 1000000`), and at least one (`if (time == 0) time = 1`).
/// Zero (an already-elapsed park) stays zero: HotSpot returns at once for
/// a negative time, the native's zero. Interpreter round i4 wave 98, lane
/// L6 (page `i98-L6-windows-parknanos-whole-milliseconds`).
#[cfg(windows)]
pub(in crate::vm::vm_exec) fn windows_parker_relative_wait(t: std::time::Duration) -> std::time::Duration {
    if t.is_zero() {
        return t;
    }
    let ms = u64::try_from(t.as_millis()).unwrap_or(u64::MAX).max(1);
    std::time::Duration::from_millis(ms)
}

/// Set a platform thread's permit for `Unsafe.unpark`: under the
/// `ParkState` mutex (the default), or with `CRATONVM_PARK_ATOMIC_PERMIT`
/// (interpreter round i3 wave 73, lane L6; page
/// `i72-L6-proposal-lock-free-park-permit-20261105`) with one swap, taking
/// the mutex and the wake-up only when the target waits.
///
/// With `CRATONVM_PARK_UNTIMED_NO_POLL` (interpreter round i3 wave 75, lane
/// L6) the wake is a `notify_all` (`ParkState::unpark_waking_all`): without
/// the 5 ms poll, a `notify_one` that lands on another waiter of the state's
/// condvar is not rescued by the owner's next slice.
///
/// Interpreter round i3 wave 76, lane L6 (`CRATONVM_PARK_WAKE_AFTER_UNLOCK`,
/// default off): HotSpot's `Parker::unpark` -- the permit under the mutex,
/// the wake (of every waiter) after it is released, and none when the permit
/// was already set. Polled or not.
///
/// Wave 77 (lane L6): the three switches are one `UnparkShape` and one call
/// (`ParkState::unpark_with`), each combination the body it chose before.
#[inline]
pub(in crate::vm::vm_exec) fn unpark_park_state(shared: &SharedVm, park_state: &crate::threading::jvm_thread::ParkState) {
    park_state.unpark_with(shared.threads.thread_registry.unpark_shape());
}

pub(crate) fn resolve_thread_id_from_thread_obj(
    shared: &SharedVm,
    thread_obj: ObjectRef,
) -> Option<ThreadId> {
    // Real-JDK mirrors: resolve by the unique Java `Thread.tid` first — immune
    // to the mirror-address recycling that makes the pointer walk misreport a
    // freshly constructed Thread as RUNNABLE/TERMINATED (a dead thread's
    // retained entry at the same recycled address) and misroute
    // interrupt/unpark wakeups. The tid-guarded pointer walk below covers
    // mirrors registered mid-construction (entry recorded before the ctor
    // assigned `tid`), backfilling the index for subsequent O(1) hits.
    if let Some(java_tid) = read_java_thread_tid(shared, thread_obj) {
        if let Some(id) = shared
            .threads
            .thread_registry
            .find_thread_id_by_java_tid(java_tid)
        {
            return Some(id);
        }
        return shared
            .threads
            .thread_registry
            .find_thread_id_by_thread_obj_tid_checked(thread_obj, java_tid);
    }
    // Synthetic-layout / pre-ctor mirrors: legacy pointer walk, then the
    // synthetic convention of the registry id stored as a Long at slot 2.
    shared
        .threads
        .thread_registry
        .find_thread_id_by_thread_obj(thread_obj)
        .or_else(|| match shared.mem.heap.get_field(thread_obj, 2) {
            Value::Long(id) => Some(ThreadId(id as u64)),
            _ => None,
        })
}

/// Whether an object is the real bootstrap `java/lang/String` class.
pub(in crate::vm::vm_exec) fn is_real_java_string(shared: &SharedVm, object: ObjectRef) -> bool {
    let object = shared.mem.heap.load_and_forward(object);
    if shared.mem.heap.kind_of(object) != ObjectKind::Object {
        return false;
    }
    shared
        .classes
        .class_manager
        .read()
        .get_class(shared.mem.heap.class_id_of(object))
        .is_some_and(|class| class.name.as_ref() == "java/lang/String")
}

/// How a `java.lang.String` instance stores its characters.
///
/// The first two match the JDK's own `String.LATIN1` / `String.UTF16` coder
/// constants and describe a `byte[]` payload. [`STRING_STORAGE_CHARS`] is the
/// pre-JDK-9 `char[]` payload, which CratonVM's own fabricated
/// `java/lang/String` still uses — see [`java_string_storage`].
pub(in crate::vm::vm_exec) const STRING_STORAGE_LATIN1: u8 = 0;
pub(in crate::vm::vm_exec) const STRING_STORAGE_UTF16: u8 = 1;
pub(in crate::vm::vm_exec) const STRING_STORAGE_CHARS: u8 = 2;

/// The element type of `object`, or `None` when `object` is not an array at
/// all. `VmHeap::array_element_type` reads a header word that is only
/// meaningful on an array, so every layout probe below goes through here.
pub(in crate::vm::vm_exec) fn array_element_type_of(shared: &SharedVm, object: ObjectRef) -> Option<ArrayElementType> {
    let object = shared.mem.heap.load_and_forward(object);
    if shared.mem.heap.kind_of(object) != ObjectKind::Array {
        return None;
    }
    shared.mem.heap.array_element_type(object)
}

/// Locate a String's character array and how that array is encoded.
///
/// Returning `None` means *"this object's characters could not be located"* —
/// never *"the string is empty"* and never anything about a comparison. Every
/// caller must treat `None` as "fall back to running `String.hashCode()` /
/// `String.equals()`".
///
/// # Why this is not just slots 0 and 1
///
/// The JDK-9+ compact layout (`value:[B` at slot 0, `coder:B` at slot 1) is
/// probed first and answered without touching class metadata: it is the layout
/// of every String in a real-JDK run and these readers sit under the hottest
/// comparison in the map natives.
///
/// But it is not the only layout this VM runs. CratonVM's fabricated
/// `java/lang/String` (booted whenever no real JDK image is in play — every
/// in-process `VmConfig::new()` test) declares `value` + `hash` and has **no
/// `coder` field at all**, so its slot 1 holds the *cached hash*. Reading that
/// as a coder is what made `compact_java_strings_equal` answer "not equal" for
/// two identical Strings, which in turn made `ConcurrentHashMap.get` miss
/// every String key the same map had just stored
/// (`chm-get-misses-stored-key-in-process-RETIRED-20260804.md`).
/// So when the positional probe does not describe a String, resolve `value`
/// and `coder` by NAME off the receiver's own class before giving up.
pub(in crate::vm::vm_exec) fn java_string_storage(shared: &SharedVm, object: ObjectRef) -> Option<(ObjectRef, u8)> {
    let heap = &shared.mem.heap;
    let object = heap.load_and_forward(object);
    if let Value::Object(Some(value)) = heap.get_field(object, 0) {
        let value = heap.load_and_forward(value);
        // `array_element_type` / `array_length` read header words that only
        // mean anything on an array, so the kind check comes first — same
        // order, and for the same reason, as `read_java_string`'s.
        match array_element_type_of(shared, value) {
            Some(ArrayElementType::Char) => return Some((value, STRING_STORAGE_CHARS)),
            Some(ArrayElementType::Byte) => match heap.get_field(object, 1) {
                Value::Int(0) => return Some((value, STRING_STORAGE_LATIN1)),
                Value::Int(1) => return Some((value, STRING_STORAGE_UTF16)),
                _ => {}
            },
            _ => {}
        }
    }
    java_string_storage_by_name(shared, object)
}

/// Metadata-driven half of [`java_string_storage`], for any `java/lang/String`
/// whose layout is not the JDK-9+ positional one. A missing `coder` field means
/// a `byte[]` payload is LATIN1 — the same default `NativeContextImpl
/// ::read_string` applies when it takes this route.
pub(in crate::vm::vm_exec) fn java_string_storage_by_name(shared: &SharedVm, object: ObjectRef) -> Option<(ObjectRef, u8)> {
    let object = shared.mem.heap.load_and_forward(object);
    let class_id = shared.mem.heap.class_id_of(object);
    let (value_index, coder_index) = {
        let cm = shared.classes.class_manager.read();
        (
            resolve_field_index_in_hierarchy(class_id, "value", &cm.class_store)?,
            resolve_field_index_in_hierarchy(class_id, "coder", &cm.class_store),
        )
    };
    let Value::Object(Some(value)) = shared.mem.heap.get_field(object, value_index) else {
        return None;
    };
    let value = shared.mem.heap.load_and_forward(value);
    match array_element_type_of(shared, value) {
        Some(ArrayElementType::Char) => Some((value, STRING_STORAGE_CHARS)),
        Some(ArrayElementType::Byte) => {
            match coder_index.map(|index| shared.mem.heap.get_field(object, index)) {
                None | Some(Value::Int(0)) => Some((value, STRING_STORAGE_LATIN1)),
                Some(Value::Int(1)) => Some((value, STRING_STORAGE_UTF16)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Number of UTF-16 code units in a character array of `element_count`
/// elements under `storage`. `None` for a UTF16 `byte[]` of odd length, which
/// is not a decodable String payload.
pub(in crate::vm::vm_exec) fn string_unit_count(element_count: usize, storage: u8) -> Option<usize> {
    match storage {
        STRING_STORAGE_LATIN1 => Some(element_count),
        STRING_STORAGE_UTF16 => (element_count % 2 == 0).then(|| element_count / 2),
        _ => Some(element_count),
    }
}

/// Read one UTF-16 code unit out of a String's character array.
///
/// # Safety
/// `data` must point at `storage`'s payload and `index` must be less than the
/// [`string_unit_count`] computed for that same payload.
pub(in crate::vm::vm_exec) unsafe fn string_unit_at(data: *const u8, storage: u8, index: usize) -> u16 {
    if storage == STRING_STORAGE_LATIN1 {
        return unsafe { *data.add(index) } as u16;
    }
    // Both the UTF16 `byte[]` payload and a `char[]` payload are pairs of
    // little-endian bytes (`StringUTF16.isBigEndian() == false`, and `char[]`
    // elements are stored as native-endian `u16` on the little-endian targets
    // this VM builds for).
    unsafe { u16::from_le_bytes([*data.add(index * 2), *data.add(index * 2 + 1)]) }
}

/// Return the raw Java String hash directly from the receiver's character
/// storage, without materialising a host `String`.
///
/// `None` means the storage could not be read, not that the hash is 0.
pub(in crate::vm::vm_exec) fn compact_java_string_hash(shared: &SharedVm, object: ObjectRef) -> Option<i32> {
    let object = shared.mem.heap.load_and_forward(object);
    if !is_real_java_string(shared, object) {
        return None;
    }
    let (value, storage) = java_string_storage(shared, object)?;
    let data = shared.mem.heap.array_data_ptr(value)?;
    let units = string_unit_count(shared.mem.heap.array_length(value), storage)?;
    let mut hash = 0i32;
    for index in 0..units {
        // SAFETY: `data` is `value`'s payload and `index < units`, the unit
        // count computed for that payload.
        let unit = unsafe { string_unit_at(data, storage, index) };
        hash = hash.wrapping_mul(31).wrapping_add(unit as i32);
    }
    Some(hash)
}

/// Compare two `java.lang.String` instances without creating a host `String`.
///
/// `Some(_)` is an ANSWER; `None` means "this comparison was not made" — the
/// caller must fall back to dispatching `String.equals`. Returning `false` for
/// a String whose storage this function does not understand is what made
/// `ConcurrentHashMap.get` miss keys the map held (see [`java_string_storage`]
/// and the known-issue file it names): a "not equal" verdict is only ever
/// correct once BOTH operands have actually been read.
pub(in crate::vm::vm_exec) fn compact_java_strings_equal(
    shared: &SharedVm,
    left: ObjectRef,
    right: ObjectRef,
) -> Option<bool> {
    let left = shared.mem.heap.load_and_forward(left);
    let right = shared.mem.heap.load_and_forward(right);
    if left == right {
        return Some(true);
    }
    if shared.mem.heap.class_id_of(left) != shared.mem.heap.class_id_of(right) {
        return Some(false);
    }
    let (left_value, left_storage) = java_string_storage(shared, left)?;
    let (right_value, right_storage) = java_string_storage(shared, right)?;
    let (Some(left_data), Some(right_data)) = (
        shared.mem.heap.array_data_ptr(left_value),
        shared.mem.heap.array_data_ptr(right_value),
    ) else {
        return None;
    };
    let left_elements = shared.mem.heap.array_length(left_value);
    let right_elements = shared.mem.heap.array_length(right_value);
    if left_storage == right_storage {
        // Same representation: one raw payload compare, no decoding at all.
        //
        // Bytes per ELEMENT, not per code unit: `array_length` already counts
        // elements, and a UTF16 String's value array is a `byte[]` whose length
        // is the byte count. Only a `char[]` has 2-byte elements. (Deriving the
        // span from the unit count instead would read twice the payload of
        // every UTF16 String.)
        let element_bytes = if left_storage == STRING_STORAGE_CHARS {
            2
        } else {
            1
        };
        let left_raw =
            unsafe { std::slice::from_raw_parts(left_data, left_elements * element_bytes) };
        let right_raw =
            unsafe { std::slice::from_raw_parts(right_data, right_elements * element_bytes) };
        return Some(left_raw == right_raw);
    }
    let left_units = string_unit_count(left_elements, left_storage)?;
    let right_units = string_unit_count(right_elements, right_storage)?;
    if left_units != right_units {
        return Some(false);
    }
    for index in 0..left_units {
        // SAFETY: each pointer is its own value array's payload and `index` is
        // below that array's unit count.
        let (l, r) = unsafe {
            (
                string_unit_at(left_data, left_storage, index),
                string_unit_at(right_data, right_storage, index),
            )
        };
        if l != r {
            return Some(false);
        }
    }
    Some(true)
}

impl<'a> NativeContextImpl<'a> {
    /// Promote a reference READ THROUGH a trusted region to a trusted cursor.
    ///
    /// The shared tail of the three fused read-and-promote accessors and of
    /// [`cratonvm_native_api::NativeHeapAccess::reached`]. `None` for a null or
    /// a non-reference value.
    ///
    /// No membership probe: a reference slot of a live object holds a live
    /// object, and inside a region no safepoint has intervened since the entry
    /// was validated — so the probe would be re-answering a settled question.
    /// The DEBUG build asks it anyway, so every `--lib` and regression run
    /// polices the premise instead of trusting it silently.
    #[inline]
    pub(in crate::vm::vm_exec) fn promote_reached(&self, value: Value) -> Option<cratonvm_native_api::ReadCursor<'_>> {
        let Value::Object(Some(obj)) = value else {
            return None;
        };
        debug_assert!(
            self.shared
                .mem
                .heap
                .is_object_address(obj.as_ptr() as usize)
                .is_some(),
            "a region read produced a reference that is not a live object base -- \
             either the region reached a safepoint (it must not be able to) or \
             the value did not come from a read through this region"
        );
        Some(cratonvm_native_api::ReadCursor::trusted(
            obj,
            READ_CURSOR_TRUSTED,
        ))
    }

    /// JNI-`FindClass`-style loader fidelity for name-based class lookups made
    /// from *native* code.
    ///
    /// [`SharedVm::load_class_concurrent`] only consults the three built-in
    /// loader levels (bootstrap -> extension -> application) and, for a name
    /// under a recognized enterprise prefix (`io/quarkus/`, `org/jboss/`,
    /// `io/smallrye/`, `org/infinispan/`, ...), *fabricates a synthetic stub*
    /// when none of them has the bytes. A stub has no `Code` attribute on any
    /// method, so the first real use of that answer dies with a `VerifyError`
    /// ("non-abstract non-native method must have Code attribute").
    ///
    /// That is exactly wrong whenever the class genuinely exists but lives
    /// only behind a custom `ClassLoader` invisible to the process's own
    /// `-cp` -- Quarkus's fast-jar `RunnerClassLoader` (`lib/main/*.jar`),
    /// Tomcat's `WebappClassLoader` (`WEB-INF/lib`), JBoss Modules, ... .
    /// HotSpot has no such failure mode: JNI `FindClass` resolves through the
    /// loader associated with the *calling* class, and the reflective
    /// signature/generics resolvers use the declaring class's own loader.
    ///
    /// Worse, the mis-resolution is *destructive*: the fabricated stub is
    /// registered globally under `Application`, so a later, correct
    /// `RunnerClassLoader.loadClass` for the same binary name finds the stub
    /// through parent delegation and never defines the real class. The
    /// consultation therefore has to happen BEFORE the fabrication, which is
    /// what `ClassManager::would_fabricate_synthetic_stub` is for.
    ///
    /// Strictly additive: it only ever fires where the alternative answer is a
    /// fabricated stub -- never where any loader actually defined a class --
    /// so deployments that legitimately rely on stubs (no user loader on the
    /// stack, or that loader cannot find the name either) are unaffected.
    ///
    /// Found via Keycloak 26.6.1 boot: `Class.getGenericInterfaces()` on a
    /// SmallRye Config mapping interface resolved the type argument
    /// `io.quarkus.runtime.configuration.MemorySize` -- present only in
    /// `lib/main/io.quarkus.quarkus-core-*.jar` behind `RunnerClassLoader` --
    /// to a synthetic stub, and the `MethodHandle` constructor call SmallRye
    /// then made against that mirror threw `VerifyError` out of
    /// `io.quarkus.runtime.generated.SharedConfig.<clinit>`.
    /// Resolve `name` the way a native's *calling class* would: try the
    /// built-in loaders, but let the calling class's own `ClassLoader` answer
    /// (a) before a synthetic stub would be fabricated for the name, and
    /// (b) after a genuine miss. Both are cases where the built-in answer is
    /// either fake or absent, so this can only ever resolve MORE classes --
    /// it never overrides a class a built-in loader really defined.
    pub(in crate::vm::vm_exec) fn resolve_class_loader_faithful(&mut self, name: &str) -> Result<ClassId, MethodCallFailed> {
        self.resolve_class_loader_faithful_driving(name, true)
    }

    /// [`Self::resolve_class_loader_faithful`]; `drive_after_miss` false
    /// skips the calling frame's loader after a global miss (the base
    /// `ClassLoader.loadClass` native's own lookup,
    /// `NativeClassAccess::load_class_for_loader_receiver`; interpreter round
    /// i2 wave 53, lane L5). The before-stub drive is kept: that step skips
    /// stub names anyway unless `CRATONVM_CL_STUB_DELEGATION=1`, and then the
    /// drive is what keeps the stub from being minted.
    pub(in crate::vm::vm_exec) fn resolve_class_loader_faithful_driving(
        &mut self,
        name: &str,
        drive_after_miss: bool,
    ) -> Result<ClassId, MethodCallFailed> {
        // The native-side twin of the constant-pool hook in
        // `resolve_class_loader_aware`: `Class.forName`, JNI `FindClass` and
        // every native that resolves a class by name land here, and this is the
        // last point that has a Java thread and no class-manager lock. See
        // `runtime::instrument::pre_transform_for_load`.
        if crate::runtime::instrument::load_hook_armed(self.shared) {
            crate::runtime::instrument::pre_transform_for_load(self.shared, self.thread, name, 0);
        }
        // JNI `FindClass`: a native running on behalf of a class a user-defined
        // loader defined resolves its names through THAT loader.
        if let Some(anchor) = self.native_caller_loader_anchor(name) {
            if let Ok(cid) = crate::runtime::interpreter::resolve_class_or_array_loader_aware(
                self.shared,
                self.thread,
                anchor,
                name,
            ) {
                return Ok(cid);
            }
        }
        if let Some(cid) = self.class_via_caller_loader_before_stub(name) {
            return Ok(cid);
        }
        match self.shared.load_class_concurrent(name) {
            Ok(cid) => Ok(cid),
            Err(e) if !drive_after_miss => {
                if cratonvm_native_builtins::classloader::any_defining_loader_registered(&self.shared.loader_identity) {
                    self.shared
                        .classes
                        .load_class_caller_drives_skipped
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e.into())
            }
            Err(e) => match self.class_via_caller_loader_noted(name, "after-miss") {
                Some(cid) => Ok(cid),
                None => Err(e.into()),
            },
        }
    }

    /// `set_fields_typed`'s mixed batch (`VmConfig::native_batch_reference_stores`;
    /// the stack-walker residual-costs page, item 2): each reference value
    /// forwarded at the boundary, as the per-slot path forwards it, then
    /// `VmHeap::try_set_compact_fields` -- one plan and one card for the
    /// batch, where the per-slot path validated, forwarded and resolved the
    /// layout once per slot. `None` (the switch off, a batch over 16 slots, or
    /// the heap declining) performs no writes.
    pub(in crate::vm::vm_exec) fn try_set_compact_fields_forwarded(
        &self,
        obj: ObjectRef,
        slots: &[(usize, u8)],
        values: &[cratonvm_types::Value],
    ) -> Option<ObjectRef> {
        if values.len() > 16 {
            return None;
        }
        let mut forwarded = [Value::Object(None); 16];
        for (dst, value) in forwarded.iter_mut().zip(values) {
            *dst = forward_boundary_value(&self.shared.mem.heap, *value);
        }
        self.shared
            .mem
            .heap
            .try_set_compact_fields(obj, slots, &forwarded[..values.len()])
    }

    /// [`NativeInvokeAccess::invoke`]'s memoised route
    /// (`VmConfig::native_invoke_memo`; the stack-walker residual-costs page,
    /// item 1): a [`native_callee_memo::lookup_invoke`] hit runs the declaring
    /// class's bytecode exactly as the ordinary tail of
    /// `invoke_on_class_shared_inner` would; a miss resolves the class as
    /// the door does (so the witness can name it), dispatches through
    /// `native_context_invoke` with the witness armed, and fills the memo only
    /// if that dispatch reached the ordinary tail.
    ///
    /// [`native_callee_memo::lookup_invoke`]: crate::runtime::native_callee_memo::lookup_invoke
    pub(in crate::vm::vm_exec) fn invoke_memoized(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
        first_key: u32,
    ) -> MethodCallResult {
        use crate::runtime::native_callee_memo;
        if let Some(declaring) =
            native_callee_memo::lookup_invoke(self.shared, class_name, first_key, method_name, descriptor)
        {
            return crate::runtime::interpreter::execute(
                self.shared,
                self.thread,
                declaring,
                method_name,
                descriptor,
                args,
            );
        }
        let (owner, fresh) = load_class_pinning_args(self.shared, self.thread, class_name, args);
        let owner = owner?;
        let args = fresh.as_deref().unwrap_or(args);
        let save = native_callee_memo::arm_invoke(
            owner,
            first_key,
            method_name,
            descriptor,
            self.thread.frames.len(),
        );
        let result = native_context_invoke(
            self.shared,
            self.thread,
            class_name,
            method_name,
            descriptor,
            args,
            Some(owner),
        );
        if let Some(declaring) = native_callee_memo::disarm(save) {
            native_callee_memo::fill_invoke(
                self.shared,
                class_name,
                first_key,
                method_name,
                descriptor,
                declaring,
            );
        }
        result
    }

    // The Java frame class a native's by-name class lookup is made on behalf
    // of, when a user-defined `ClassLoader` defined it (JNI `FindClass`: "the
    // loader associated with the calling class"). The innermost frame outside
    // the JDK's own plumbing (`java/`, `jdk/`, `sun/`) is the caller: an
    // intrinsic replaces a Java method, so its frame is the caller's. `None`
    // when no user-defined loader exists, when that frame's class came from a
    // built-in loader, or for names that only bootstrap can answer.
    //
    // Without it a by-name lookup answers the built-in chain's copy -- the
    // APPLICATION loader's -- even when the running code is a
    // `ModifiedClassPathClassLoader` fork's: `H2 ValueBigint.get` returned the
    // application's `ValueBigint` into the fork's `Value[]` (`ArrayStoreException`
    // in Flyway/Liquibase tests), and reactor's `Mono.just` bridge returned the
    // application's `MonoJust`.
    // rt22 (open): this walks `thread.frames`, which holds no compiled activation, so a
    // compiled caller anchors on the nearest INTERPRETED non-JDK frame. See
    // `rt22-compiled-callers-invisible-to-interpreter-frame-readers-20261102.md`.
    pub(in crate::vm::vm_exec) fn native_caller_loader_anchor(&self, name: &str) -> Option<ClassId> {
        if !cratonvm_native_builtins::classloader::any_defining_loader_registered(&self.shared.loader_identity)
            || cratonvm_native_api::caller_loader::suppressed()
            || !cratonvm_native_api::caller_loader::current_native_is_application()
        {
            return None;
        }
        if name.starts_with('[')
            || name.starts_with("java/")
            || name.starts_with("jdk/")
            || name.starts_with("sun/")
            || name.starts_with("cratonvm/")
        {
            return None;
        }
        let cm = self.shared.classes.class_manager.read();
        for frame in self.thread.frames.iter().rev() {
            let Some(class) = cm.get_class(frame.class_id) else {
                continue;
            };
            let class_name: &str = &class.name;
            if class_name.starts_with("java/")
                || class_name.starts_with("jdk/")
                || class_name.starts_with("sun/")
                || class_name.starts_with("cratonvm/")
            {
                continue;
            }
            let cid = frame.class_id;
            drop(cm);
            return cratonvm_native_builtins::classloader::defining_loader_for(
                &self.shared.loader_identity,
                cid.as_u32(),
            )
            .map(|_| cid);
        }
        None
    }

    pub(in crate::vm::vm_exec) fn class_via_caller_loader_before_stub(&mut self, name: &str) -> Option<ClassId> {
        // Cheap bail-out: no user-defined loader has ever defined a class in
        // this process, so no better answer can exist. Single atomic load.
        if !cratonvm_native_builtins::classloader::any_defining_loader_registered(&self.shared.loader_identity) {
            return None;
        }
        if !self
            .shared
            .classes
            .class_manager
            .read()
            .would_fabricate_synthetic_stub(name)
        {
            return None;
        }
        self.class_via_caller_loader_noted(name, "before-stub")
    }

    /// [`Self::class_via_caller_loader`], tallied in
    /// `ClassRealm::caller_drive_census` under `CRATONVM_DBG=access`
    /// (interpreter round i6 wave 116, lane L5): `when` names the door.
    pub(in crate::vm::vm_exec) fn class_via_caller_loader_noted(&mut self, name: &str, when: &'static str) -> Option<ClassId> {
        // The request prologue's door for this drive (interpreter round i6
        // wave 123, lane L5, `i39-L5`): one `Lookup` row per `when`.
        let door = if when == "before-stub" {
            "caller-loader-before-stub"
        } else {
            "caller-loader-after-miss"
        };
        if !cratonvm_types::flags().loader.dbg_access {
            return self.class_via_caller_loader(name, door);
        }
        let (answer, drove) = self.class_via_caller_loader_drove(name, door);
        if drove {
            let site = {
                let cm = self.shared.classes.class_manager.read();
                self.thread
                    .frames
                    .last()
                    .map(|f| {
                        let class = cm
                            .get_class(f.class_id)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default();
                        format!("{when} {class}.{}", f.method_name())
                    })
                    .unwrap_or_else(|| format!("{when} <no Java frame>"))
            };
            crate::runtime::resolve::loader_throw::note_caller_drive(
                self.shared,
                site,
                answer.is_some(),
            );
        }
        answer
    }

    /// Drive the `loadClass` of the nearest Java frame whose defining class
    /// came from a real user `ClassLoader` -- the native's "calling class" in
    /// JNI terms. Only the innermost such frame is consulted; walking further
    /// out would cross loader namespaces. `None` when there is no such frame,
    /// or that loader cannot supply `name` either.
    pub(in crate::vm::vm_exec) fn class_via_caller_loader(&mut self, name: &str, door: &'static str) -> Option<ClassId> {
        self.class_via_caller_loader_drove(name, door).0
    }

    /// [`Self::class_via_caller_loader`], and whether a loader was driven
    /// (the census of [`Self::class_via_caller_loader_noted`]).
    pub(in crate::vm::vm_exec) fn class_via_caller_loader_drove(
        &mut self,
        name: &str,
        door: &'static str,
    ) -> (Option<ClassId>, bool) {
        if !cratonvm_native_builtins::classloader::any_defining_loader_registered(&self.shared.loader_identity) {
            return (None, false);
        }
        // `cratonvm/...` is CratonVM's own internal helper namespace (stream
        // ops, collector shims). No application ClassLoader can supply those,
        // and the stub IS the intended answer -- don't spend a Java
        // `loadClass` call per lookup asking.
        if name.starts_with("cratonvm/") {
            return (None, false);
        }
        let frame_classes: Vec<ClassId> = self
            .thread
            .frames
            .iter()
            .rev()
            .map(|f| f.class_id)
            .collect();
        let dbg = crate::runtime::env_cache::dbg_stub_loader();
        for cid in frame_classes {
            if cratonvm_native_builtins::classloader::defining_loader_for(
                &self.shared.loader_identity,
                cid.as_u32(),
            )
            .is_none()
            {
                continue;
            }
            let driven = crate::runtime::interpreter::drive_defining_loader_load_door(
                self.shared,
                self.thread,
                cid,
                name,
                door,
            );
            if dbg {
                let cm = self.shared.classes.class_manager.read();
                eprintln!(
                    "[DBG_STUBLOADER] {name}: drove {} -> {:?}",
                    cm.get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default(),
                    driven.and_then(|d| cm.get_class(d).map(|c| c.name.to_string())),
                );
            }
            return (driven, true);
        }
        (None, false)
    }

    /// The `ClassId` a native call should resolve *array* descriptors against:
    /// the innermost Java frame that is not part of the reflection plumbing
    /// that got us here.
    ///
    /// `Class.forName` / `Array.newInstance` / `Class.arrayType` are all
    /// caller-sensitive, and their own declaring classes are bootstrap-defined,
    /// so taking `frames.last()` blindly would always answer "bootstrap" and
    /// defeat the whole point. Mirrors the frame walk
    /// `lang_class::class_for_name_one_arg_caller_loader` already does for the
    /// one-argument `Class.forName` overload; only the innermost non-plumbing
    /// frame is consulted, because walking further out crosses loader
    /// namespaces.
    pub(in crate::vm::vm_exec) fn array_resolution_referencing_class(&self) -> Option<ClassId> {
        const REFLECTION_PLUMBING: &[&str] = &[
            "java/lang/Class",
            "java/lang/reflect/Array",
            "java/lang/invoke/MethodType",
        ];
        // JIT round 22 wave 1, lane runtime (page rt22): a compiled activation
        // pushes no interpreter `Frame`, so walking `thread.frames` alone made a
        // compiled caller's array-descriptor load (`Class.forName("[LX;")`, the
        // `Array.newInstance` family) resolve against the nearest INTERPRETED
        // frame outward -- possibly another loader's class -- where the
        // interpreter answers the caller's own. The compiled-aware walk
        // (`stackwalker::frame_class_ids_tail`, the one `frame_class_ids` and
        // the reflection caller rule use) lists compiled activations and their
        // exact inlined levels in order; with no compiled frame live it is the
        // interpreter frames reversed, as before. A window first (the plumbing
        // is one to three frames), the whole stack only if the window held none.
        let first_outside_plumbing = |ids: &[ClassId]| {
            let cm = self.shared.classes.class_manager.read();
            ids.iter().copied().find(|cid| {
                cm.get_class(*cid)
                    .is_some_and(|class| !REFLECTION_PLUMBING.contains(&&*class.name))
            })
        };
        let (window, whole) =
            crate::runtime::stackwalker::frame_class_ids_tail(&self.thread.frames, 8);
        if let Some(found) = first_outside_plumbing(&window) {
            return Some(found);
        }
        if whole {
            return None;
        }
        let all = crate::runtime::stackwalker::frame_class_ids_with_compiled(&self.thread.frames);
        first_outside_plumbing(&all)
    }

    /// `NativeClassAccess::load_class`; `drive_after_miss` as in
    /// [`Self::resolve_class_loader_faithful_driving`].
    pub(in crate::vm::vm_exec) fn load_class_driving(&mut self, name: &str, drive_after_miss: bool) -> MethodCallResult {
        // JVMS §5.3.3: an array class is defined by the defining loader of its
        // component type. `resolve_class_loader_faithful` below cannot express
        // that — it resolves the array descriptor as a flat global name, so
        // `[Lp/X;` came out bootstrap-keyed and two isolating loaders' arrays
        // collapsed onto one runtime class. Try the loader-faithful array path
        // first; it returns `None` for every non-array name and for a
        // built-in calling loader, so this is strictly additive.
        if name.starts_with('[') {
            // Bind before the `if let`: an `if let` scrutinee's temporaries
            // (here the autoref `&*self`) live for the whole body, which would
            // collide with the `&mut self` reborrows inside it.
            let referencing = self.array_resolution_referencing_class();
            if let Some(referencing_class_id) = referencing {
                let resolved = crate::runtime::interpreter::resolve_array_class_loader_aware(
                    self.shared,
                    self.thread,
                    referencing_class_id,
                    name,
                );
                if let Some(class_id) = resolved {
                    let mirror = super::get_or_create_class_mirror(self.shared, class_id);
                    return Ok(Some(Value::Object(Some(mirror))));
                }
            }
        }
        // See `class_via_caller_loader_before_stub`: a name that would only
        // resolve to a fabricated synthetic stub must first be offered to the
        // calling class's own ClassLoader, before the stub is minted and
        // globally registered under `Application`.
        let class_id = self.resolve_class_loader_faithful_driving(name, drive_after_miss)?;
        let mirror = super::get_or_create_class_mirror(self.shared, class_id);
        Ok(Some(Value::Object(Some(mirror))))
    }
}

impl NativeContextImpl<'_> {
    /// HotSpot's spelling of a define-time JPMS refusal of the class being
    /// defined by `cl_id` (`ClassManager`'s `supertype_access_violation`,
    /// JIT round 20 lane superAcc): `verify_class_access_msg` names an
    /// unnamed accessor module `unnamed module @0x<identity hash>` -- the
    /// defining loader's `ClassLoader.unnamedModule` -- where the class
    /// manager, which cannot read that object, wrote `unnamed module`. Every
    /// other error, and a refusal whose loader's unnamed module cannot be
    /// read (the bootstrap and platform loaders, a loader shape without the
    /// field), is returned unchanged.
    #[cold]
    pub(in crate::vm::vm_exec) fn define_refusal_with_unnamed_module_hash(
        &self,
        error: VmError,
        cl_id: cratonvm_types::ClassLoaderId,
    ) -> VmError {
        use cratonvm_types::ClassLoaderId;
        let VmError::Linkage(LinkageError::IllegalAccessError { message }) = &error else {
            return error;
        };
        let accessor_unnamed = "(in unnamed module) cannot access class ";
        let exported_to_unnamed = " to unnamed module";
        if !(message.starts_with("superclass access check failed: class ")
            || message.starts_with("superinterface check failed: class "))
            || !message.contains(accessor_unnamed)
            || !message.ends_with(exported_to_unnamed)
        {
            return error;
        }
        let loader = match cl_id {
            ClassLoaderId::UserDefined(_) => {
                cratonvm_native_builtins::classloader::user_loader_object(
                    self,
                    cl_id.to_native_id(),
                )
            }
            ClassLoaderId::Application => {
                cratonvm_native_builtins::classloader::adopted_builtin_loader_for_tag(
                    &self.shared.loader_identity,
                    cl_id.to_native_id(),
                    "",
                )
            }
            ClassLoaderId::Bootstrap | ClassLoaderId::Extension => None,
        };
        let Some(loader) = loader else {
            return error;
        };
        let heap = &self.shared.mem.heap;
        let (loader, ok) = heap.load_and_forward_checked(loader);
        if !ok {
            return error;
        }
        let Some(slot) = resolve_field_slot_by_name_cached(
            self.shared,
            heap.class_id_of_validated(loader),
            "unnamedModule",
        ) else {
            return error;
        };
        let Value::Object(Some(module)) = heap.get_field(loader, slot) else {
            return error;
        };
        // Interpreter round i5 wave 100, lane L4: the one copy of the rewrite
        // (`vm_object.rs` `unnamed_module_access_text`).
        let text = super::unnamed_module_access_text(self.shared, module, message.clone());
        VmError::Linkage(LinkageError::IllegalAccessError { message: text })
    }
}

impl NativeContextImpl<'_> {
    /// [`NativeClassAccess::declared_methods`] (`name` `None`) or
    /// [`NativeClassAccess::declared_methods_named`]: the metadata of the class's
    /// declared methods named `name` (all of them for `None`), and whether it
    /// declares any method. One read of the class manager; the metadata
    /// (strings, decoded `Exceptions` / `Signature`) is built for the matches only.
    pub(in crate::vm::vm_exec) fn declared_methods_matching(
        &self,
        class_id: ClassId,
        name: Option<&str>,
    ) -> (Vec<MethodMetadata>, bool) {
        let cm = self.shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return (Vec::new(), false);
        };
        let any = !class.methods.is_empty();
        let found = class
            .methods
            .iter()
            .filter(|m| name.is_none_or(|n| &*m.name == n))
            .map(|m| {
                // WP2.5 v3 — populate `exceptions` from the JVMS §4.7.5
                // `Exceptions` attribute when present. Used by the proxy
                // generator to thread the declared throws set into
                // `<clinit>` so the dispatch helper's UTE wrap can match
                // thrown exceptions against the method's declared set.
                let exceptions: Vec<String> = m
                    .attributes
                    .iter()
                    .find_map(|a| match a.as_decoded() {
                        Some(cratonvm_reader::attribute::Attribute::Exceptions {
                            exception_indices,
                        }) => Some(
                            exception_indices
                                .iter()
                                .filter_map(|idx| {
                                    class.constant_pool.get_class_name(*idx).map(str::to_string)
                                })
                                .collect::<Vec<String>>(),
                        ),
                        _ => None,
                    })
                    .unwrap_or_default();
                // Same one pass, for the same reason: `create_method_object`
                // used to re-find this method by name+descriptor to read it.
                let signature: Option<String> =
                    m.attributes.iter().find_map(|a| match a.as_decoded() {
                        Some(cratonvm_reader::attribute::Attribute::Signature(sig)) => {
                            Some(sig.to_string())
                        }
                        _ => None,
                    });
                MethodMetadata {
                    name: m.name.to_string(),
                    descriptor: m.descriptor.to_string(),
                    access_flags: m.access_flags.bits(),
                    declaring_class_id: class_id,
                    exceptions,
                    signature,
                }
            })
            .collect();
        (found, any)
    }

    /// [`NativeClassAccess::declared_fields`] (`name` `None`) or
    /// [`NativeClassAccess::declared_fields_named`]: the metadata of the class's
    /// declared fields named `name` (all of them for `None`), and whether it
    /// declares any field. Slot indices count every field in declaration order
    /// (statics from 0, instance fields from `first_field_index`), matched or not.
    pub(in crate::vm::vm_exec) fn declared_fields_matching(
        &self,
        class_id: ClassId,
        name: Option<&str>,
    ) -> (Vec<FieldMetadata>, bool) {
        let cm = self.shared.classes.class_manager.read();
        let Some(class) = cm.get_class(class_id) else {
            return (Vec::new(), false);
        };
        let any = !class.fields.is_empty();
        let mut static_idx = 0usize;
        let mut instance_idx = 0usize;
        let mut found = Vec::new();
        for f in class.fields.iter() {
            let slot = if f.is_static() {
                let idx = static_idx;
                static_idx += 1;
                idx
            } else {
                let idx = class.first_field_index + instance_idx;
                instance_idx += 1;
                idx
            };
            if name.is_some_and(|n| &*f.name != n) {
                continue;
            }
            found.push(FieldMetadata {
                name: f.name.to_string(),
                descriptor: f.descriptor.to_string(),
                access_flags: f.access_flags.bits(),
                slot_index: slot,
                declaring_class_id: class_id,
                is_static: f.is_static(),
            });
        }
        (found, any)
    }
}

