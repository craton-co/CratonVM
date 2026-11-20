#[allow(unused_imports)]
use super::*;

/// Whether ZGC **relocation** may run in this configuration — a refusal, not a
/// warning.
///
/// `requested` is the eventual relocation switch. There is none today: no
/// `-XX:` option, no `CRATONVM_*` name in `types/src/flag_groups.rs`, no field
/// on `crate::config::VmConfig`, and `gc/src/zgc/relocate.rs` has no caller —
/// `ZgcRealHeap` is still the non-moving stop-the-world mark-sweep it has
/// always been. So the one call site passes a `false` constant and this gate is
/// a no-op. It exists anyway because a gate added *after* the capability it
/// guards is a gate that shipped one release too late.
///
/// # Why a refusal
///
/// Under a relocating ZGC a reference slot holds a *colored* word, not a
/// machine pointer: a heap offset plus metadata bits plus `Z_COLORED_TAG` at
/// bit 63 (`gc/src/zgc/vaddr.rs`). The load barrier
/// (`gc/src/zgc/barrier.rs`) is what turns that word into a live address and
/// heals the slot, and the interpreter takes it on every read. JIT-compiled
/// code does not: there are nine raw reference-load emission points across the
/// baseline and optimizing x64 tiers — `getfield`, `getstatic`, `aaload`, the
/// `String.value` intrinsic, the LICM hoist, the SIMD row load — and the helper
/// arms that do reach Rust run the loaded word through `plausible_heap_pointer`
/// and answer `0` when it fails, which a colored word is designed to do.
///
/// So compiled code reads either a stale from-space address into an evacuated
/// object (use-after-free) or a spurious `null` for a live one (a wrong answer,
/// silently). Unlike the compressed-oops gate in `SharedVm::new`, whose
/// degraded mode is "slower but correct", there is no correct degraded mode
/// here, so this returns `false` rather than logging and continuing.
///
/// Refusing *relocation* rather than the *JIT* is deliberate: it degrades ZGC
/// to the non-moving collector it already is, instead of degrading the whole VM
/// to the interpreter.
///
/// # What lifts the gate
///
/// Stage (a) of `docs/feature-designs/zgc-jit-load-barrier.md`: the barrier
/// inside the `jit_getfield` / `jit_aaload` / `jit_getstatic` helpers, the
/// inline arms routed to those helpers behind the JIT-side kill switch, and the
/// seven value-degrading plausibility filters removed from the load path. Until
/// then the permitted relocating configuration is "JIT off".
#[cfg(feature = "zgc")]
pub fn zgc_relocation_permitted(requested: bool) -> bool {
    if !requested {
        return false;
    }
    if crate::runtime::env_cache::disable_jit() {
        return true;
    }
    // Stage (a) of `zgc-jit-load-barrier.md` LANDED 2026-08-13, so the JIT is
    // no longer automatically disqualifying.
    //
    // The refusal below exists because JIT-compiled code baked raw 8-byte
    // reference loads at compile-time offsets and would read a coloured word
    // as a pointer. `x64::zgc_read_barrier_blocks_inline_fields` now routes
    // every compact-field access through `jit_getfield` / `jit_putfield_object`
    // whenever the read barrier is armed, and those go through the heap's own
    // accessors, which barrier. That is the same mechanism compressed oops has
    // used for the same reason since before this gate existed.
    //
    // The CAPABILITY, not the runtime state: this runs at VM init, long
    // before any cycle arms a barrier, so asking "is the barrier armed" here
    // would answer no forever and refuse relocation permanently. The question
    // is whether the code this JIT emits will respect a barrier armed later.
    if cratonvm_jit::x64::zgc_codegen_honours_read_barrier() {
        return true;
    }
    // Reported on stderr, not just through `tracing`, for the reason the
    // compressed-oops gate states: a silent fallback would look identical to a
    // successful run, and the operator must see which one they got. The stakes
    // are higher here — the un-refused configuration corrupts the heap rather
    // than merely using more of it.
    eprintln!(
        "[cratonvm] ZGC relocation requested but the JIT is enabled - relocation \
         REFUSED, running the non-moving mark-sweep instead. JIT-compiled code \
         loads reference fields without the ZGC load barrier, so a relocating \
         cycle would hand it stale pointers into evacuated objects \
         (use-after-free) or a spurious null for a live object, with no error \
         path. Re-run with --nojit (CRATONVM_DISABLE_JIT=1) to get relocation, \
         or wait for the JIT-side load barrier - stage (a) of \
         docs/feature-designs/zgc-jit-load-barrier.md."
    );
    false
}

// ---------------------------------------------------------------------------
// Round 4 audit fix (CRIT) — ResolutionCache invalidation on redefine
// ---------------------------------------------------------------------------
//
// `cratonvm_classloading::ClassManager::redefine_class` fires a plain
// `fn(u32, u32)` hook (see `install_resolution_invalidate_hook`) when the
// bytecode of a class is replaced in place. The hook has no captured
// state, so we bridge it to the VM-owned `SharedVm::resolution_cache`
// through this module-private registry.
//
// PER-VM STATE (P0, `docs/architecture/per-vm-state.md`). This used to be a
// `OnceLock<Weak<SharedVm>>` — **first VM wins forever**. That was wrong even
// for the SEQUENTIAL embedding case: create VM A, drop it, create VM B, and
// every `redefine_class` in VM B fired a hook that upgraded VM A's dead
// `Weak`, returned early, and left VM B's `ResolutionCache` / `LinkResolver`
// holding pre-redefine `(declaring_class_id, index)` pairs. Stale resolution
// after a redefine is silent wrong behaviour, not a perf loss.
//
// The registry holds every live VM. The JIT and resolution hooks carry the
// firing store's layout domain (waves 18 and 19 of interpreter round i1) and
// reach only the owning VM ([`live_hook_vms_for_domain`]); an unknown token,
// and the hooks that carry no token, fan out to all of them. Invalidation,
// unlike installation, is safe to over-apply: dropping a cache entry only
// costs a re-resolve. In the single-VM case (the overwhelmingly common one)
// the registry has exactly one entry and behaviour is bit-identical to the
// old `OnceLock`.
//
// Vtable *installation*, which is not idempotent across VMs and so cannot fan
// out, is routed by the firing class handle's VM instead
// (`runtime::vtable::vtable_install_adapter_scoped`, interpreter round i1
// wave 19).
//
// KEYED by the owner token (interpreter round i1 wave 21, lane L5, stage 2 of
// `i18-L5-proposal-one-owner-token-for-every-classloading-hook`): the map goes
// from a VM's class-store layout domain (`JitRealm::class_layout_domain`,
// unique per store and so per VM) to the VM. A scoped fire
// ([`hook_vm_for_domain`]) is one read-locked probe and one `Weak` upgrade,
// with no `Vec` allocated; it used to lock a mutex, upgrade every entry and
// copy them all into a fresh `Vec` on every class-manager write-guard release.

// gci i5/hd (`gch-h4fl-xlog-class-load-residuals` items 1-2): each entry also
// carries the VM's `-Xlog:class+load` walk cutoff -- the first `ClassId` its
// registration did NOT print (`set_global_shared_vm_for_hooks`); 0 when the
// tag was off then. Per VM, in the existing registry: no new static.
pub(in crate::vm::vm_init) type SharedVmRegistry = parking_lot::RwLock<rustc_hash::FxHashMap<u32, (Weak<SharedVm>, u32)>>;

pub(in crate::vm::vm_init) static RESOLUTION_INVALIDATE_VMS: OnceLock<SharedVmRegistry> = OnceLock::new();

pub(in crate::vm::vm_init) fn shared_vm_hook_registry() -> &'static SharedVmRegistry {
    RESOLUTION_INVALIDATE_VMS
        .get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()))
}

/// Snapshot the live VMs registered for captureless-`fn` hook bridging.
///
/// Returns owning `Arc`s so callers can drop the registry lock before doing
/// any VM work — the adapters below take VM-internal locks
/// (`resolution_cache`, `jit_cache`, `class_manager`), and holding the
/// registry lock across those would invert the lock order. Dead entries are
/// skipped here and pruned by the next registration.
/// `live_hook_vms` for callers outside this module (the deferred-`new` and
/// profile-replay sweeps given no owner token). Same registry, same lock
/// discipline.
pub(crate) fn live_hook_vms_for_jit() -> Vec<Arc<SharedVm>> {
    live_hook_vms()
}

/// Every live registered VM, in layout-domain order (the order their class
/// stores were created in), so "the first VM that has the id" stays the
/// oldest one, as it was when the registry was a `Vec`. For the hooks that
/// carry no owner token; cold.
pub(in crate::vm::vm_init) fn live_hook_vms() -> Vec<Arc<SharedVm>> {
    let mut live: Vec<(u32, Arc<SharedVm>)> = {
        let reg = shared_vm_hook_registry().read();
        reg.iter()
            .filter_map(|(&domain, (weak, _))| weak.upgrade().map(|shared| (domain, shared)))
            .collect()
    };
    live.sort_unstable_by_key(|&(domain, _)| domain);
    live.into_iter().map(|(_, shared)| shared).collect()
}

/// The live VM whose class store has layout domain `owner_domain`, if one is
/// registered: the target of a scoped hook fire. One read-locked map probe
/// and one `Weak` upgrade; the lock is released before the caller touches the
/// VM.
pub(crate) fn hook_vm_for_domain(owner_domain: u32) -> Option<Arc<SharedVm>> {
    let weak = shared_vm_hook_registry()
        .read()
        .get(&owner_domain)
        .map(|(weak, _)| weak.clone());
    weak.and_then(|weak| weak.upgrade())
}

/// [`hook_vm_for_domain`] plus the VM's `-Xlog:class+load` walk cutoff (gci
/// i5/hd): the class-load hook prints only ids at or above it
/// (`unified_logging::class_load_hook_prints`).
pub(crate) fn hook_vm_and_class_log_cutoff(owner_domain: u32) -> Option<(Arc<SharedVm>, u32)> {
    let entry = shared_vm_hook_registry().read().get(&owner_domain).cloned();
    entry.and_then(|(weak, cutoff)| weak.upgrade().map(|shared| (shared, cutoff)))
}

/// zr z4/un: `jdk.ZUncommit` for one uncommitter pass (`bytes` returned to the
/// OS over `dur_ns` from `start_ns`, epoch nanoseconds), on both JFR backends,
/// from the heap's uncommitter thread -- HotSpot emits it from its
/// `ZUncommitter` thread, outside any collection. The legacy recorder lives on
/// the `SharedVm`, found through the hook registry by layout domain (`None`
/// before `Vm::new` published it, or after the VM is gone: then only the
/// native backend records). Gated by zr z4/jf's `CRATONVM_ZGC_JFR_EVENTS`.
#[cfg(feature = "zgc")]
pub(in crate::vm::vm_init) fn zgc_uncommit_jfr(vm_identity: usize, domain: u32, bytes: usize, start_ns: u64, dur_ns: u64) {
    // gci i20/ob: per call (once per uncommitter pass), as `gc_events.rs`'s
    // `zgc_jfr_events_enabled` reads the same switch; no process latch.
    if !true {
        return;
    }
    cratonvm_native_builtins_jfr::jfr_vm_more::zgc_uncommit(
        vm_identity,
        start_ns,
        dur_ns,
        bytes as u64,
    );
    if cratonvm_jfr::is_enabled() {
        if let Some(shared) = hook_vm_for_domain(domain) {
            let mut jfr = shared.debug.flight_recorder.lock();
            cratonvm_jfr::builtin::emit_zgc_uncommit_event(
                &mut jfr,
                i64::try_from(bytes).unwrap_or(i64::MAX),
                start_ns,
                dur_ns,
            );
        }
    }
}

/// Publish the `Weak<SharedVm>` handle that `resolution_invalidate_adapter`
/// upgrades to invalidate the per-VM `ResolutionCache` on redefine.
///
/// Called from `Vm::new` right after `self_arc` is populated, keyed by the
/// VM's layout domain. Registering the same VM twice is a no-op (its entry is
/// replaced by an equal one); dead entries from previously-dropped VMs are
/// pruned on every call, so a long-lived process that creates and destroys
/// many VMs does not accumulate them.
///
/// gci i5/hd (`gch-h4fl-xlog-class-load-residuals` items 1-2): with
/// `-Xlog:class+load` on, the FIRST registration of a VM prints every class
/// its store already holds, in id (define) order -- the primordial classes
/// `bootstrap_core_classes` defined before the hook existed, and the classes
/// whose hook fired before this registration (they printed their bare name,
/// now nothing) -- and records the store's `next_id` as the entry's cutoff, so
/// a hook event of one of those classes still queued fires silently. The
/// cutoff is read and the entry inserted under one class-manager read guard:
/// no class can be defined between the two. The lines are written after the
/// guard is dropped. With the tag off: cutoff 0, no walk, no class-manager
/// lock (the registration is as it was).
pub fn set_global_shared_vm_for_hooks(weak: Weak<SharedVm>) {
    let Some(shared) = weak.upgrade() else {
        return; // already dead; nothing to bridge
    };
    let domain = shared.jit.class_layout_domain;
    if !crate::runtime::unified_logging::class_load_logging_enabled() {
        let mut reg = shared_vm_hook_registry().write();
        reg.retain(|_, (existing, _)| existing.strong_count() > 0);
        let kept = reg
            .get(&domain)
            .filter(|(existing, _)| Weak::ptr_eq(existing, &weak))
            .map_or(0, |&(_, cutoff)| cutoff);
        reg.insert(domain, (weak, kept));
        return;
    }
    let walked = {
        let cm = shared.classes.class_manager.read();
        let cutoff = cm.class_store().next_id().as_u32();
        let fresh = {
            let mut reg = shared_vm_hook_registry().write();
            reg.retain(|_, (existing, _)| existing.strong_count() > 0);
            let again = reg
                .get(&domain)
                .is_some_and(|(existing, _)| Weak::ptr_eq(existing, &weak));
            if !again {
                reg.insert(domain, (weak, cutoff));
            }
            !again
        };
        if fresh {
            crate::runtime::unified_logging::class_load_walk_messages(&cm, cutoff)
        } else {
            Vec::new()
        }
    };
    crate::runtime::unified_logging::log_class_load_walk(&walked);
}

/// The `set_class_namer` adapter: `ClassId` -> binary name, for GC
/// diagnostics only.
///
/// `try_read` rather than `read`, and this is load-bearing. Every caller is a
/// failure-path report, and at least one of them (the ZGC fragmentation
/// report) runs on a thread that has just failed an allocation — a thread that
/// may well be the one holding the class-manager write lock further up its own
/// stack. Blocking there would convert a diagnostic into a hang, which is
/// strictly worse than an unnamed class id. A contended lock therefore falls
/// back to the id, which is exactly what the caller prints when no namer is
/// installed at all.
///
/// `try_read_untracked`, not `try_read` (interpreter round i1 wave 19): the
/// tracked form still runs the lock-order check, which panics (debug builds,
/// `CRATONVM_LOCK_ORDER_CHECK`) when this thread holds a lock at the class
/// manager's level or below — for instance ITS VM's class-manager writer
/// while another live VM's class manager is free. A non-blocking acquire adds
/// no wait-for edge, so skipping the check is sound here.
pub(in crate::vm::vm_init) fn class_name_adapter(class_id: u32) -> Option<String> {
    let cid = crate::classloading::ClassId::new(class_id);
    for shared in live_hook_vms() {
        let Some(cm) = shared.classes.class_manager.try_read_untracked() else {
            continue;
        };
        if let Some(class) = cm.get_class(cid) {
            return Some(class.name.to_string());
        }
    }
    None
}

/// The `JvmtiClassHook` adapter handed to `install_class_load_hook`: posts
/// ClassLoad into the VM whose class store defined the class.
///
/// `owner_domain` is that store's layout domain, and a VM's identity is its
/// store's domain plus one ([`vm_identity_for_layout_domain`]), so naming the
/// VM needs no registry lookup. Until interpreter round i1 wave 22 (lane L5)
/// the hook carried no owner and the adapter posted through the unattributed
/// manager, whose real-env bridge refuses to guess with two VMs live: no
/// ClassLoad reached any native agent in a two-VM process, and a Rust
/// listener on the unattributed row saw both VMs' class ids.
pub(in crate::vm::vm_init) fn class_load_adapter(class_id: u32, class_name: &str, thread_id: u64, owner_domain: u32) {
    // gch h4/fl: `-Xlog:class+load` (one logger probe when the tag is off),
    // before the agent's `ClassLoad` as HotSpot logs at parse time.
    crate::runtime::unified_logging::log_class_load(owner_domain, class_id, class_name);
    // i53-L1 wave 124 (lane L1): `CRATONVM_JVMTI_C_CLASS_LOAD_AT_ADAPTER`
    // (default OFF) posts the C table's `ClassLoad` here, by the defining
    // store's VM, and the D14 bridge below skips its C half; same order.
    #[cfg(feature = "experimental-debug")]
    let c_at_adapter = crate::jvmti::native_env::class_load_at_adapter_enabled();
    #[cfg(not(feature = "experimental-debug"))]
    let c_at_adapter = false;
    #[cfg(feature = "experimental-debug")]
    if c_at_adapter {
        crate::jvmti::native_env::post_class_load_at_adapter(owner_domain, thread_id, class_id);
    }
    crate::runtime::jvmti::fire_class_load_for_vm_after_adapter(
        vm_identity_for_layout_domain(owner_domain),
        thread_id,
        class_id as u64,
        c_at_adapter,
    );
}

/// The `ResolutionInvalidateHook` adapter handed to
/// `install_resolution_invalidate_hook`. Plain `fn` pointer (no captures)
/// so the classloading crate can store it in a `OnceLock`.
///
/// Drops every cached resolution that refers to (or was resolved into)
/// the redefined class. The classloading crate's `RedefineGate` already
/// auto-evicts the per-thread invoke caches via the generation counter;
/// this closes the loop for the slower symbolic-reference cache.
///
/// `owner_domain` is the firing class store's layout domain; the sweep
/// reaches only the VM that owns it ([`live_hook_vms_for_domain`]).
///
/// `what` (interpreter round i1 wave 24, lane L5): a class whose constant pool
/// was replaced loses every record keyed on it; a descendant whose field
/// layout merely shifted keeps its recorded failures, permanent constants and
/// call sites (`ResolutionCache::invalidate_class_field_layout`), which JVMS
/// §5.4.3 / §5.4.3.6 make permanent for an unchanged pool.
pub(in crate::vm::vm_init) fn resolution_invalidate_adapter(
    class_id: u32,
    owner_domain: u32,
    what: cratonvm_classloading::ResolutionInvalidation,
) {
    let cid = crate::classloading::ClassId::new(class_id);
    // Advance the resolution generation FIRST, so no thread can publish a new
    // per-thread site-cache entry that snapshots the pre-invalidation epoch
    // after the authoritative maps below have already been swept. Two of the
    // four firing sites (`upgrade_synthetic_class`, `recompute_subclass_layouts`)
    // move a class's field layout while leaving its `ClassId`, its name and the
    // redefine latch alone — this bump is the only signal a resolved-field
    // cache gets for them. See `runtime::interpreter::constants`'s `RESOLUTION_EPOCH`.
    // The firing store's own copy moves (and the process counter), so it is
    // exact for the owner even when no live VM claims the token (wave 20).
    crate::runtime::interpreter::bump_resolution_epoch_for_domain(owner_domain);
    // Only the VM whose class store fired the hook: `ClassId`s are per store,
    // so another VM's class with this id is a different class, and sweeping
    // it dropped that VM's resolutions for nothing under its write lock
    // (interpreter round i1 wave 19, the twin of `jit_invalidate_adapter`'s
    // wave-18 token). A token no live VM owns (a class manager built outside
    // a VM, or a VM not yet registered) keeps the fan-out: under-invalidating
    // the owner is a stale-resolution correctness bug, over-invalidating
    // another VM a re-resolve. The resolution-epoch bump above is the
    // store's (interpreter round i1 wave 20, lane L2).
    let targets = live_hook_vms_for_domain(owner_domain);
    // The `multianewarray` per-site plans are keyed by VM, the referencing
    // class's id and a constant-pool index; a redefine can renumber that pool
    // under the same id. For the owning VM, its plans that name the class on
    // either side (the unload path's rule in `memory::gc`, which is a superset
    // of what a renumbered pool invalidates); for an unknown token,
    // wholesale, which also reaches a VM that is not in the hook registry.
    match targets.as_slice() {
        [owner] if owner.jit.class_layout_domain == owner_domain => {
            crate::runtime::interpreter::invalidate_multianewarray_plans_naming(
                owner.vm_identity,
                &|id| id == cid,
            );
        }
        _ => crate::runtime::interpreter::invalidate_multianewarray_plans(),
    }
    for shared in targets {
        match what {
            cratonvm_classloading::ResolutionInvalidation::ConstantPoolReplaced => {
                shared
                    .classes
                    .resolution_cache
                    .write()
                    .invalidate_class(cid);
            }
            cratonvm_classloading::ResolutionInvalidation::FieldLayoutOnly => {
                shared
                    .classes
                    .resolution_cache
                    .write()
                    .invalidate_class_field_layout(cid);
            }
        }
        // Round 8 audit fix (CRIT #3): the `LinkResolver` reflective cache
        // was missing from the redefine-invalidation cascade. Without
        // this, any cached `(class, name, descriptor)` triple resolved
        // before a `RedefineClasses` continues to return the
        // pre-redefine `(declaring_class_id, index)` pair — pointing at
        // a method index that may now refer to a different method body
        // (or, after a field shape change, a stale field slot).
        // `LinkResolver::invalidate_class` mirrors
        // `ResolutionCache::invalidate_class` (drops by key-class OR
        // resolved-declaring-class match).
        shared.classes.link_resolver.invalidate_class(cid);
    }
}

/// The `JitInvalidateHook` adapter handed to
/// `cratonvm_classloading::install_jit_invalidate_hook`.
///
/// Fired whenever a class's field layout may have changed in a way that
/// already-compiled JIT code cannot safely observe: JVMTI `redefine_class`
/// (Step 8), and — critically — `upgrade_synthetic_class` /
/// `recompute_subclass_layouts`, which (unlike JEP 109 redefine) really can
/// change instance field count/order/offsets when a synthetic JDK stub is
/// later replaced by its real `.class` bytecode. A method JIT-compiled
/// against the stub's layout bakes the stub's field offsets
/// (`compact_field_off`, or the legacy `field_index * SLOT_SIZE` cell
/// offset) directly into its machine code as immediates; nothing else in
/// the VM invalidated that code when the layout later grew/reordered, so it
/// kept reading/writing the WRONG byte offset of any object allocated under
/// the new layout — a stale-offset getfield could silently return whatever
/// raw bytes sat at the old offset (e.g. a small int) where a reference was
/// expected, corrupting anything computed from it. Found while
/// investigating the guarded-inline-getfield SIGSEGV cluster
/// (docs/known-issues/elasticsearch-suite/ES-HANG-20260709-*), but that
/// specific SIGSEGV's confirmed root cause is a different, already-fixed
/// bug: the vm-side JIT field resolvers used to fabricate a `(0, false)`
/// compact slot for any field with no genuine registered `CompactLayout`
/// entry, and the compact-offset inline getfield arm trusted it — see
/// `be7102344`'s commit message ("perf(jit): re-enable guarded-inline-
/// getfield default-ON, root cause fixed") and the WildFly Host Controller
/// fix it cites. This invalidation gap is real and independent of that bug
/// — nothing else in the VM ever evicted JIT code after a layout-changing
/// synthetic-stub upgrade, regardless of the fabricated-slot bug's fix.
///
/// A full cache flush is used rather than a class-scoped eviction —
/// mirroring `redefine_class`'s own existing conservative pattern in
/// `vm_exec.rs` (`jit_cache.write().clear_all()`) — because any OTHER
/// class's compiled method may hold a getfield/putfield referencing the
/// changed class's fields, and there is no reverse index of "which
/// compiled methods read which class's fields" to evict precisely. A full
/// flush is rare (each synthetic class upgrades at most once) and safe:
/// `clear_all` retires evicted methods rather than freeing their code
/// immediately, so any still-active frame stays valid.
///
/// `owner_domain` is the firing class store's layout domain; the flush and the
/// recipe drop reach only the VM that owns it ([`live_hook_vms_for_domain`]).
pub(in crate::vm::vm_init) fn jit_invalidate_adapter(class_id: u32, owner_domain: u32) {
    // The socket path's `ByteBuffer` layout cache is keyed by `ClassId` and
    // holds resolved FIELD SLOTS, so it is invalidated by exactly the events
    // this hook fires on — a redefine, or a synthetic stub being replaced by
    // real bytecode with a different field count or order. Its failure mode
    // without this is the silent one: a transfer reading `position` from a
    // slot that now holds something else, which desynchronises a channel
    // rather than raising anything.
    cratonvm_native_io::socket_fast_io::invalidate_bb_slot_cache();

    // An in-place redefinition of this class on this thread flushes its own
    // VM itself, right after `redefine_class` returns (interpreter round i1
    // wave 17, `InPlaceRedefinitionScope`): it changes no layout, and the
    // flush below would duplicate that one (and, for an unknown owner token,
    // reach every other VM for a class id that names a different class there).
    let redefining_in_place = crate::vm::realms::jit_realm::in_place_redefinition_of(
        crate::classloading::ClassId::new(class_id),
    );

    // Only the VM whose class store fired the hook: `ClassId`s are per store,
    // so another VM's class with this id is a different class, and flushing
    // it expired that VM's verdicts, OSR denials and compiled bodies for
    // nothing (interpreter round i1 wave 18). A token no live VM owns (a
    // class manager built outside a VM, or a VM not yet registered) keeps
    // the old fan-out: missing the owning VM's eviction after a layout
    // change is a live use of compiled field offsets that no longer describe
    // the class, whereas evicting another VM's code is a recompile cost.
    for shared in live_hook_vms_for_domain(owner_domain) {
        // An in-place layout change (stub upgrade, subclass re-layout,
        // redefine) moves the field indices the allocation-init recipe baked
        // (`jit_post_alloc_init`); unpublish it so the next slow-path
        // allocation rebuilds it from the new layout. Takes only the cache's
        // own `retired` mutex, never the class manager (round 9 wave 8b,
        // review8b request 1).
        let _ = shared.jit.jit_alloc_class_cache.invalidate(class_id);
        crate::runtime::frozen_pages::invalidate_class(&shared, class_id);
        if redefining_in_place {
            continue;
        }
        // With the flush's verdict and code-state epochs (interpreter round i1
        // wave 14: a bare `clear_all` here retired no per-VM verdict).
        let evicted = shared.jit.flush_code_cache();
        if evicted > 0 {
            tracing::debug!(
                "JIT: fully invalidated {evicted} method(s) due to a class layout \
                 change (class_id={class_id})"
            );
        }
    }
}

/// The live VMs a JIT invalidation fired on behalf of the class store with
/// layout domain `owner_domain` must reach: the VM that owns the domain
/// (`JitRealm::class_layout_domain`), or every live VM when none does. Same
/// registry and lock discipline as `live_hook_vms`. The owned case is one
/// keyed probe and no heap allocation (interpreter round i1 wave 21).
pub(in crate::vm::vm_init) fn live_hook_vms_for_domain(owner_domain: u32) -> smallvec::SmallVec<[Arc<SharedVm>; 1]> {
    match hook_vm_for_domain(owner_domain) {
        Some(owner) => {
            let mut one = smallvec::SmallVec::new();
            one.push(owner);
            one
        }
        None => live_hook_vms().into_iter().collect(),
    }
}

/// The `ClassInfoHook` adapter handed to `cratonvm_gc::install_class_info_hook`.
///
/// Resolves a raw `ClassId` to `(class_name, num_total_fields)` so the
/// heap's out-of-bounds field-access diagnostic (`gen_heap::set_field` /
/// `get_field`) can print actionable class identity instead of a bare
/// numeric id. Reuses the same live-VM registry — all three hooks are plain
/// captureless `fn` pointers bridged to the VM-owned `SharedVm` through it.
///
/// Returns `None` before any VM handle is wired, after every registered VM has
/// been dropped, or for a class id that no live VM has in its class store.
///
/// This is a diagnostic-only lookup, so answering from the first VM that has
/// the id is acceptable: `ClassId`s are allocated per-VM (`ClassStore::next_id`
/// is `self.classes.len()`), so with two VMs live the same numeric id names two
/// different classes and the name printed may belong to the other VM. The
/// alternative — printing nothing — is strictly worse for a crash diagnostic.
/// Callers must not treat this as an authoritative class lookup.
///
/// `try_read_untracked`, like [`class_name_adapter`] and for the same reasons:
/// the heap reports from whatever thread made the bad access, which may hold
/// the class manager's writer (or a reader, with a writer queued behind it —
/// parking_lot read locks are not recursive) further up its stack. A contended
/// lock skips that VM rather than turning a diagnostic into a hang, and the
/// untracked form keeps the lock-order check from turning it into a panic
/// (interpreter round i1 wave 19; this was a blocking `read`).
pub(in crate::vm::vm_init) fn class_info_adapter(class_id: u32) -> Option<(String, usize)> {
    let cid = crate::classloading::ClassId::new(class_id);
    for shared in live_hook_vms() {
        let Some(cm) = shared.classes.class_manager.try_read_untracked() else {
            continue;
        };
        if let Some(class) = cm.class_store.get(cid) {
            return Some((class.name.to_string(), class.num_total_fields));
        }
    }
    None
}

/// The owner-scoped `set_class_namer_in_domain` adapter: `class_id` in the VM
/// whose class store (and heap) has layout domain `owner_domain`, the VM's
/// hook token. Interpreter round i1 wave 23, lane L5 (the i18-L5 owner-token
/// proposal's step 4): a heap that knows its domain asks this, so with two
/// VMs live its report names ITS VM's class, not whichever VM has the id
/// first ([`class_name_adapter`]). An unregistered domain (a heap bound before
/// its VM registered, or after it was dropped) falls back to that adapter.
/// Same `try_read_untracked` discipline: a contended lock answers `None`.
pub(in crate::vm::vm_init) fn class_name_in_domain_adapter(owner_domain: u32, class_id: u32) -> Option<String> {
    let Some(shared) = hook_vm_for_domain(owner_domain) else {
        return class_name_adapter(class_id);
    };
    let cm = shared.classes.class_manager.try_read_untracked()?;
    let name = cm
        .get_class(crate::classloading::ClassId::new(class_id))
        .map(|class| class.name.to_string());
    drop(cm);
    name
}

/// The owner-scoped twin of [`class_info_adapter`], for
/// `cratonvm_gc::install_class_info_in_domain_hook`; see
/// [`class_name_in_domain_adapter`].
pub(in crate::vm::vm_init) fn class_info_in_domain_adapter(owner_domain: u32, class_id: u32) -> Option<(String, usize)> {
    let Some(shared) = hook_vm_for_domain(owner_domain) else {
        return class_info_adapter(class_id);
    };
    let cm = shared.classes.class_manager.try_read_untracked()?;
    let info = cm
        .class_store
        .get(crate::classloading::ClassId::new(class_id))
        .map(|class| (class.name.to_string(), class.num_total_fields));
    drop(cm);
    info
}

impl SharedVm {
    /// Dump all loaded non-synthetic classes to a CDS archive file.
    /// Called on VM shutdown when `config.cds_mode == CdsMode::Dump`.
    #[cfg(feature = "experimental-aot")]
    pub fn dump_cds_archive(&self) -> Result<usize, String> {
        let archive_path = self
            .config
            .shared_archive_file
            .as_deref()
            .unwrap_or("classes.jsa");

        let cm = self.classes.class_manager.read();
        let mut generator = cratonvm_native_builtins::cds::CdsArchiveGenerator::new(archive_path);

        // Iterate all loaded classes and add non-synthetic ones with cached bytes
        // zt t18/df7 (x13 item 21a): an unbound or withdrawn define is not loaded.
        for class in cm.class_store.iter_named() {
            if class.origin.is_compatibility_stub() {
                continue;
            }
            if let Some(bytes) = cm.defined_class_bytes(class.id) {
                let entry = cratonvm_native_builtins::cds::CdsArchiveEntry {
                    class_name: class.name.to_string(),
                    bytes_offset: 0,
                    bytes_length: bytes.len() as u32,
                    access_flags: class.access_flags.bits(),
                    superclass: class.superclass.map(|sid| {
                        cm.class_store
                            .get(sid)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default()
                    }),
                    interfaces: class
                        .interfaces
                        .iter()
                        .filter_map(|iid| cm.class_store.get(*iid).map(|c| c.name.to_string()))
                        .collect(),
                    class_bytes: bytes.to_vec(),
                };
                generator.add_entry(entry);
            }
        }

        let count = generator.entry_count();
        generator.write_archive()?;
        Ok(count)
    }

    /// CDS is an explicitly compiled experiment. Keep the public shutdown hook
    /// available in production builds, but reject archive generation rather
    /// than linking the experimental implementation accidentally.
    #[cfg(not(feature = "experimental-aot"))]
    pub fn dump_cds_archive(&self) -> Result<usize, String> {
        Err("CDS support is not compiled; rebuild with --features experimental-aot".to_string())
    }

    /// Allocate a new synthetic ClassId for a lambda proxy.
    /// These IDs start at 0x8000_0000 and increment, avoiding collision
    /// with real ClassIds assigned by the ClassStore.
    pub fn alloc_lambda_proxy_id(&self) -> ClassId {
        ClassId::new(self.classes.next_lambda_id.fetch_add(1, Ordering::Relaxed))
    }

    /// The JDK's own serializability rule for a `LambdaMetafactory`-spun proxy,
    /// and the single place it is decided.
    ///
    /// `LambdaMetafactory.altMetafactory` makes a lambda serializable when its
    /// call site passed `FLAG_SERIALIZABLE`, and adds `Serializable` to the spun
    /// class's interface list only when no interface already extends it
    /// (`isSerializable && !foundSerializableSupertype`); an interface that
    /// extends `Serializable` WITHOUT the flag is `accidentallySerializable`
    /// (wave 86: [`cratonvm_native_api::LambdaSerializability::AccidentallySerializable`]).
    /// So real HotSpot gives an
    /// ordinary `Supplier<String> s = () -> "x"` no `writeReplace()`, no
    /// `Serializable` interface, and a ClassCastException on `(Serializable) s` --
    /// while `Comparator.comparing(..)` gets all three.
    ///
    /// Callers: the `instanceof`/`checkcast` fast path for lambda proxies, and the
    /// reflective `Class` surfaces (`getDeclaredMethods`, `getInterfaces`,
    /// `getGenericInterfaces`) via `NativeContext::lambda_proxy_serializability`.
    /// Keep it that way -- re-deriving this rule per call site is how the two
    /// halves drift apart.
    pub fn lambda_proxy_serializability(
        &self,
        proxy_class_id: ClassId,
    ) -> cratonvm_native_api::LambdaSerializability {
        use cratonvm_native_api::LambdaSerializability as S;
        let (flag, iface_name, iface_id) = {
            let proxies = self.classes.lambda_proxies.read();
            match proxies.get(&proxy_class_id) {
                Some(cs) => (
                    cs.serializable_flag,
                    cs.functional_interface.clone(),
                    cs.functional_interface_id,
                ),
                None => return S::NotSerializable,
            }
        };
        // Inheritance half. Prefer the bootstrap-captured, loader-correct id;
        // fall back to a load by name, the same order every other lambda-proxy
        // consumer uses. A load failure only costs us the inheritance arm.
        let iface_id = iface_id.or_else(|| self.load_class_concurrent(&iface_name).ok());
        let inherits = iface_id
            .map(|id| {
                self.classes
                    .class_manager
                    .read()
                    .is_assignable_to_name(id, "java/io/Serializable")
            })
            .unwrap_or(false);
        // Interpreter round i4 wave 87, lane L4: a `FLAG_MARKERS` interface
        // that extends `Serializable` counts as the functional interface does
        // (`InnerClassLambdaMetafactory`: `accidentallySerializable |= ...` per
        // alt interface; `altMetafactory`'s `foundSerializableSupertype` per
        // marker). Page
        // `interpreter-i86-L4-lambda-and-switch-residuals-FIXED-20261120`, residual 1.
        let inherits = inherits
            || crate::runtime::invokedynamic::lambda_proxy_marker_interfaces(
                self.vm_identity,
                proxy_class_id,
            )
            .is_some_and(|markers| {
                markers.iter().any(|m| {
                    let id = m
                        .class_id
                        .or_else(|| self.load_class_concurrent(&m.name).ok());
                    id.is_some_and(|id| {
                        self.classes
                            .class_manager
                            .read()
                            .is_assignable_to_name(id, "java/io/Serializable")
                    })
                })
            });
        // Interpreter round i4 wave 86, lane L4: JDK 25's
        // `InnerClassLambdaMetafactory` makes a lambda serializable ONLY for
        // `FLAG_SERIALIZABLE` (`isSerializable`); an inherited `Serializable`
        // without the flag is `accidentallySerializable`, whose spun class
        // declares the hostile `writeObject` / `readObject` and no
        // `writeReplace`. javac passes the flag for every lambda whose target
        // type is serializable, so its sites keep `ByInheritance`.
        if inherits && !flag {
            S::AccidentallySerializable
        } else if inherits {
            S::ByInheritance
        } else if flag {
            S::ByFlag
        } else {
            S::NotSerializable
        }
    }

    /// Get an `Arc<SharedVm>` from the stored weak self-reference, or `None`
    /// when this `SharedVm` has no self-reference installed.
    ///
    /// `self_arc` is installed at the end of `Vm::new()`, but `SharedVm::new()`
    /// is public and cannot install it (the `Arc` does not exist yet — the
    /// caller creates it). A bare `Arc::new(SharedVm::new(config))` — the shape
    /// most unit fixtures use — therefore has `self_arc == None`, which is a
    /// legitimate state, not an impossible one. Callers that can degrade
    /// gracefully (e.g. refusing to spawn a thread rather than aborting the
    /// process) must use this instead of [`Self::get_arc`].
    pub fn try_get_arc(&self) -> Option<Arc<SharedVm>> {
        // `upgrade()` succeeds whenever `self_arc` is set, because we are
        // called via `&self` on a `SharedVm` that lives inside an
        // `Arc<SharedVm>` — at least one strong reference is alive for the
        // duration of this borrow.
        self.self_arc.read().as_ref().and_then(|w| w.upgrade())
    }

    /// Get an `Arc<SharedVm>` from the stored weak self-reference.
    ///
    /// Panics if `self_arc` was never set. This used to be spelled
    /// `unreachable!("self_arc is set during Vm::new and never cleared")`, but
    /// the invariant that message asserts is false: it holds only for VMs built
    /// through `Vm::new()`, and `SharedVm::new()` is public. A reached
    /// `unreachable!` is always a bug, so the case is now named honestly and
    /// the message tells the caller how to fix the fixture.
    pub fn get_arc(&self) -> Arc<SharedVm> {
        self.try_get_arc().unwrap_or_else(|| {
            panic!(
                "SharedVm::get_arc() on a VM with no `self_arc` weak self-reference. \
                 `Vm::new()` installs it; a hand-built `Arc::new(SharedVm::new(..))` \
                 fixture must install it itself with \
                 `*shared.self_arc.write() = Some(Arc::downgrade(&shared));`. \
                 Call sites that can degrade should use `try_get_arc()` instead."
            )
        })
    }
}

impl SharedVm {
    /// Dump the collected missing-native-method audit log to tracing.
    ///
    /// Called on VM shutdown when `config.audit_missing_natives` is true.
    /// Prints a sorted, deduplicated list of ACC_NATIVE methods that were
    /// invoked during execution but had no Rust implementation.
    pub fn dump_missing_natives(&self) {
        let log = self.debug.missing_natives_log.lock();
        if log.is_empty() {
            return;
        }
        let mut sorted: Vec<_> = log.clone();
        sorted.sort_by(|a, b| {
            (&a.class_name, &a.method_name, &a.descriptor).cmp(&(
                &b.class_name,
                &b.method_name,
                &b.descriptor,
            ))
        });
        tracing::info!(
            "=== Missing Native Methods Audit ({} unique) ===",
            sorted.len()
        );
        for entry in &sorted {
            match &entry.sample_call_site {
                Some(site) => {
                    tracing::info!("  {} (first seen from {})", entry.full_signature(), site)
                }
                None => tracing::info!("  {}", entry.full_signature()),
            }
        }
        tracing::info!("=== End Missing Natives ===");
    }

    /// Query the collected missing-native-method audit log.
    ///
    /// Returns a sorted, deduplicated list of `"class.method descriptor"` strings
    /// representing every ACC_NATIVE method that was invoked but had no Rust
    /// implementation registered. Only populated when `config.audit_missing_natives`
    /// is `true`.
    pub fn get_missing_natives(&self) -> Vec<String> {
        let log = self.debug.missing_natives_log.lock();
        let mut sigs: Vec<String> = log.iter().map(MissingNativeEntry::full_signature).collect();
        sigs.sort();
        sigs.dedup();
        sigs
    }

    /// NEW-10: Write the collected missing-native-method audit log to
    /// `path` as JSON.
    ///
    /// The on-disk schema is stable and diff-friendly so a committed
    /// baseline can be compared across releases:
    ///
    /// ```json
    /// {
    ///   "missing_natives": [
    ///     {
    ///       "class": "java/lang/Foo",
    ///       "name": "bar",
    ///       "descriptor": "(I)V",
    ///       "sample_call_site": "com/example/Main.main([Ljava/lang/String;)V"
    ///     }
    ///   ]
    /// }
    /// ```
    ///
    /// Entries are sorted by `(class, name, descriptor)` so repeated
    /// runs on the same program produce byte-identical output. The
    /// file is created (or truncated) with `0600` permissions on Unix
    /// via the standard `File::create`; callers targeting a shared
    /// path should set their own umask beforehand.
    ///
    /// Returns an `io::Error` if the file cannot be written. Does not
    /// panic — safe to call unconditionally at VM shutdown.
    pub fn dump_missing_natives_json(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> std::io::Result<()> {
        let log = self.debug.missing_natives_log.lock();
        let mut sorted: Vec<MissingNativeEntry> = log.clone();
        sorted.sort_by(|a, b| {
            (&a.class_name, &a.method_name, &a.descriptor).cmp(&(
                &b.class_name,
                &b.method_name,
                &b.descriptor,
            ))
        });
        // Hand-serialize to avoid pulling in serde_json just for this
        // module. The schema is tiny and well-defined; a lightweight
        // escape routine handles the only dynamic data (class and
        // method names use ASCII + `/` + `$` + digits, so escape
        // work is minimal — but we handle the full set defensively).
        let mut out = String::with_capacity(256 + 128 * sorted.len());
        out.push_str("{\n  \"missing_natives\": [");
        for (i, entry) in sorted.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("\n    {\n");
            out.push_str(&format!(
                "      \"class\": {},\n",
                json_escape(&entry.class_name)
            ));
            out.push_str(&format!(
                "      \"name\": {},\n",
                json_escape(&entry.method_name)
            ));
            out.push_str(&format!(
                "      \"descriptor\": {},\n",
                json_escape(&entry.descriptor)
            ));
            match &entry.sample_call_site {
                Some(site) => out.push_str(&format!(
                    "      \"sample_call_site\": {}\n",
                    json_escape(site)
                )),
                None => out.push_str("      \"sample_call_site\": null\n"),
            }
            out.push_str("    }");
        }
        if !sorted.is_empty() {
            out.push('\n');
            out.push_str("  ");
        }
        out.push_str("]\n}\n");

        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        file.write_all(out.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    /// The substitution policy this VM booted under
    /// (`docs/book/src/user-guide/real-jdk-mode.md` §6).
    ///
    /// Read straight off the config, which is immutable after construction:
    /// the mode is decided once, at launch, and every consumer — the census
    /// dumps below, `vm_flags`, `java.vm.info` — reads it from here rather
    /// than from a process global (§2).
    pub fn compatibility_mode(&self) -> CompatibilityMode {
        self.config.compatibility_mode
    }

    /// Feature version of the **runtime image** this VM is running against, for
    /// the report's `jdk_feature` (`real-jdk-mode.md` §9).
    ///
    /// Read from `$JAVA_HOME/release`, and `None` when there is no configured
    /// `java_home` to read it from. Deliberately NOT
    /// `java.specification.version`: that property is a fixed `"25"` this VM
    /// reports to Java code regardless of which image is mounted, so using it
    /// here would report `25` for a run against a JDK 21 image — and the one
    /// thing every consumer does with `jdk_feature` is treat it as *image
    /// identity*. Census consumers must use the mounted image's version rather
    /// than the Java property when attributing missing native methods.
    pub fn jdk_feature_version(&self) -> Option<u32> {
        jdk_feature_from_release_file(self.config.java_home.as_deref()?)
    }

    /// Synthetic-stub census: dump every registered native with its
    /// [`NativeKind`](cratonvm_native_api::NativeKind) tag
    /// (intrinsic / bridge / synthetic-stub) to a diff-stable JSON file.
    ///
    /// Thin wrapper over [`SharedVm::dump_native_census_json`] with
    /// `verbose = false` (registration sites redacted). The name, parameter
    /// list and `(intrinsic, bridge, synthetic-stub)` return are **unchanged**
    /// on purpose: in-crate tests and embedders call this exact signature, and
    /// the schema-2 upgrade is a change to the file's *contents*, not to its
    /// Rust surface. `redacted` is the right default for a caller that has no
    /// way to express `--explain-jdk-only`.
    ///
    /// The counts it returns are **registrations**, counted over the same
    /// `registrations` vector `NativeMethodRegistry::dump_registrations`
    /// iterates — superseded rows included, one row per `register()` call — so
    /// they stay numerically identical to schema 1 and the registry census's
    /// exact 157 `synthetic-stub` baseline is unaffected by the schema bump.
    pub fn dump_native_registry_json(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> std::io::Result<(usize, usize, usize)> {
        self.dump_native_census_json(path, false)
    }

    /// Native census, **schema 2** (`real-jdk-mode.md` §4, §9).
    ///
    /// Schema 1 answered "what is registered". That was enough for a ratchet
    /// and useless for the untangling work: it could not say which
    /// registration *won* a contested slot, who installed it, or whether the
    /// slot was ever actually dispatched. Schema 2 adds those three columns:
    ///
    /// ```json
    /// {
    ///   "schema_version": 5,
    ///   "mode": "compatible",
    ///   "image_adjudication": true,
    ///   "counts": { "intrinsic": 2, "bridge": 1, "synthetic-stub": 1, "total": 4 },
    ///   "invocations": { "intrinsic": 267, "bridge": 2818, "synthetic-stub": 0 },
    ///   "slots_with_incomplete_invocations": 25,
    ///   "natives": [
    ///     { "class": "java/lang/System", "name": "arraycopy",
    ///       "descriptor": "([Ljava/lang/Object;I[Ljava/lang/Object;II)V",
    ///       "kind": "intrinsic",
    ///       "registered_by": "native-builtins/src/lib.rs:1234",
    ///       "overwrote": "synthetic-stub",
    ///       "invocations": 10,
    ///       "invocations_complete": false,
    ///       "kind_stated": true, "kind_chosen": true,
    ///       "owns_slot": true,
    ///       "real_declaring_method": { "loaded": true, "declared": true,
    ///                                  "acc_native": true, "has_code": false },
    ///       "image_declaring_method": { "image_has_class": true, "declared": true,
    ///                                   "acc_native": true, "has_code": false,
    ///                                   "inherited_from": null,
    ///                                   "inherited_acc_native": false,
    ///                                   "inherited_has_code": false,
    ///                                   "inherited_abstract": false } }
    ///   ]
    /// }
    /// ```
    ///
    /// ## Schema 3 — `image_declaring_method`
    ///
    /// Schema 2's `real_declaring_method` answers from the **loaded** class
    /// store, so it is a measurement of the run: `loaded: false` means "this
    /// workload never touched the class". That is the right answer to the
    /// question it asks and the wrong instrument for adjudicating the registry,
    /// because the registrations most in need of a verdict are the ones no
    /// single workload exercises. `docs/known-issues/jdk-only/`'s sixteen
    /// `JDK-ONLY-CLASSIFY: unknown — needs census` verdicts are all blocked on
    /// exactly that.
    ///
    /// `image_declaring_method` asks the same four questions of the **bytes on
    /// the class path**, parsed and thrown away — see
    /// [`ClassManager::adjudicate_natives_against_image`], which explains at
    /// length why it must not simply load the classes. It is populated only
    /// when `verbose`, because it costs one class-file parse per distinct
    /// registered class; `image_adjudication` at the top level says whether the
    /// pass ran, so a `null` column is never ambiguous.
    ///
    /// Reading the two together is the point: `real` says whether this run
    /// exercised the slot, `image` says whether the JDK declares it at all.
    ///
    /// ## Schema 5 — `invocations_complete` and `slots_with_incomplete_invocations`
    ///
    /// `invocations` was always documented as a **lower bound**
    /// (`NativeMethodRegistry::record_invocation`), and the registry has
    /// carried a per-slot "this is a floor" bit
    /// (`NativeMethodRegistry::mark_invocations_incomplete`) since
    /// 2026-08-17. Until schema 5 **this writer emitted neither** — measured
    /// against real dumps by `G37-1` §6 N2 and `G42-1` §6 N1 — so 25 slots
    /// declared themselves uncounted and every reader of the file was told
    /// `invocations: 0` with nothing to distinguish "never called" from "called
    /// through a path that does not count".
    ///
    /// Schema 5 emits the bit per row and the slot total in the header. It
    /// changes **no number**: not a count, not an invocation tally, not
    /// `owns_slot`. What it changes is what a number licenses, and only ever in
    /// the direction of less confidence.
    ///
    /// What the bit does and does not claim, spelled out because it is easy to
    /// over-read: `false` means a dispatch path is *wired* for this slot that
    /// will not count, not that such a dispatch has happened. `true` means no
    /// path has declared itself — which is **not** "exact", because a bypass
    /// nobody has audited is indistinguishable from no bypass. There is no
    /// configuration of this VM in which the whole column is exact
    /// (`G37-1` §2, `G42-1` §5); `--nojit` with `CRATONVM_DISABLE_INTRINSICS=1`
    /// is the least inexact one.
    ///
    /// Notes on the fields that are easy to misread:
    ///
    /// * `counts` still counts **registrations**, not invocations — it is the
    ///   schema-1 block, unchanged, so the existing ratchet keeps working.
    ///   Per-slot `invocations` is the new, different measurement; summing it
    ///   per kind is `difftest`'s job.
    /// * `overwrote` is the `NativeKind` of the entry this registration
    ///   displaced, or `null`. Registration is last-write-wins throughout
    ///   `SharedVm::new` (see the two LAST-WRITE-WINS BOUNDARY blocks), and
    ///   this column plus `registered_by` is the evidence the later
    ///   untangling wave needs — §8 explicitly defers the untangling itself.
    /// * `registered_by` is redacted unless `verbose` (i.e. unless
    ///   `--explain-jdk-only` was passed): absolute build paths leak the
    ///   builder's home directory into committed baselines and make the file
    ///   differ per machine. See [`redact_registration_site`].
    /// * `real_declaring_method` reports what the *real* class says about the
    ///   slot, which is how a reviewer tells a legitimate `ACC_NATIVE` bridge
    ///   from a native shadowing concrete bytecode (§1 item 4, §7 step 3).
    ///   `has_code` is derived from the access flags (JVMS §4.6: `Code` is
    ///   present iff the method is neither `native` nor `abstract`) rather
    ///   than from `ClassFileMethod::code()`, which returns `None` for a
    ///   not-yet-force-decoded lazy attribute — a decode-state artefact, not
    ///   a fact about the class.
    ///
    /// Rows are sorted by `(class, name, descriptor)` with a **stable** sort,
    /// so the duplicate rows a contested triple produces stay in registration
    /// order — which is the overwrite chronology the untangling wave needs, and
    /// is deterministic run to run. Hand-serialized (no serde_json), mirroring
    /// `dump_missing_natives_json`. Returns the per-kind registration counts.
    ///
    /// ## The only native-census writer
    ///
    /// `--dump-native-registry` is served by **this** function: `vm-cli`'s
    /// `write_jdk_only_dumps` calls it directly. There used to be a second,
    /// independently written schema-2 writer in `vm-cli/src/main.rs`, which is
    /// a genuine consumer hazard — one `schema_version` with two shapes means a
    /// reader that works against one silently mis-reads the other. The three
    /// places the two disagreed were resolved as follows.
    ///
    /// * **Top-level `"mode"` is kept.** It is additive (no consumer keys off
    ///   the object's shape: `difftest::census` finds the row array by scanning
    ///   for objects carrying `kind`, and CI / `scripts/jdk-only-census.sh`
    ///   grep the `counts` block), and it is what lets `registry-real.json` and
    ///   `registry-no-stubs.json` be told apart from their contents. Diffing
    ///   those two files is the entire point of the census script; a census
    ///   that cannot say which policy produced it is a footgun.
    /// * **Registration order is kept as the tie-break** for duplicate triples,
    ///   i.e. the stable sort above rather than the launcher's
    ///   `(class, name, descriptor, registered_by)`. Two reasons. It is the
    ///   overwrite chronology, which is the fact the untangling wave needs and
    ///   which `registered_by` order destroys; and `registered_by` can be an
    ///   absolute build path, so sorting on it makes *row order* depend on the
    ///   build machine even though the emitted value is redacted — the opposite
    ///   of diff-stable. `census()` documents registration order, and
    ///   `slice::sort_by` is stable, so this order is deterministic.
    /// * **`real_declaring_method` is filled in**, where the launcher emitted
    ///   `null` on the (correct) principle that a census must not perturb what
    ///   it measures. Filling it here carries none of that risk:
    ///   `get_loaded_class_id` is a lookup through `&self`, not an initiating
    ///   load, so nothing is loaded to answer the question and `"loaded":
    ///   false` is a real answer ("this run never loaded the class") rather
    ///   than a probe declined. Caveat worth knowing when reading the column:
    ///   the lookup is requester-less, so for a name no built-in loader has
    ///   defined it can answer from a lone user-defined loader's copy.
    pub fn dump_native_census_json(
        &self,
        path: impl AsRef<std::path::Path>,
        verbose: bool,
    ) -> std::io::Result<(usize, usize, usize)> {
        let cm = self.classes.class_manager.read();
        self.dump_native_census_json_with(path, verbose, Some(&cm))
    }

    /// [`Self::dump_native_census_json`] against a class manager the caller
    /// already holds — or, with `cm = None`, the same census minus the one
    /// column that needs the class store.
    ///
    /// The registry rows themselves come from `natives.native_methods`, which
    /// has its own lock, so a `None` here degrades *narrowly*: every row is
    /// still written with its kind, provenance and invocation count, and only
    /// `real_declaring_method` becomes `null`. The file is marked
    /// `"partial": true` all the same, because a reader cannot otherwise tell a
    /// `null` that means "this run never loaded the class" from one that means
    /// "nobody looked".
    pub(crate) fn dump_native_census_json_with(
        &self,
        path: impl AsRef<std::path::Path>,
        verbose: bool,
        cm: Option<&crate::classloading::ClassManager>,
    ) -> std::io::Result<(usize, usize, usize)> {
        use cratonvm_native_api::NativeKind;
        let mut rows = self.natives.native_methods.census();
        rows.sort_by(|a, b| {
            (&a.class, &a.name, &a.descriptor).cmp(&(&b.class, &b.name, &b.descriptor))
        });
        let mut n_intrinsic = 0usize;
        let mut n_bridge = 0usize;
        let mut n_stub = 0usize;
        for row in &rows {
            match row.kind {
                NativeKind::Intrinsic => n_intrinsic += 1,
                NativeKind::Bridge => n_bridge += 1,
                NativeKind::SyntheticStub => n_stub += 1,
            }
        }

        // The per-row image adjudication (`image_declaring_method`). Costs one
        // class-file parse per DISTINCT registered class — roughly a thousand —
        // so it rides `verbose` (`--explain-jdk-only`) rather than firing on
        // every difftest child. The key is emitted either way, `null` when the
        // pass did not run, so a reader never has to infer from the shape which
        // kind of census this is; `image_adjudication` says it outright.
        let image_verdicts: Option<Vec<cratonvm_classloading::ImageMethodVerdict>> =
            match (verbose, cm) {
                (true, Some(cm)) => {
                    let triples: Vec<(String, String, String)> = rows
                        .iter()
                        .map(|r| (r.class.clone(), r.name.clone(), r.descriptor.clone()))
                        .collect();
                    Some(cm.adjudicate_natives_against_image(&triples))
                }
                _ => None,
            };

        let mut out = String::with_capacity(256 + 256 * rows.len());
        // Schema 4 (2026-08-11): `image_declaring_method` gained the four
        // `inherited_*` keys. The bump is not cosmetic — a reader that scores a
        // schema-3 census with schema-4 logic silently counts every inherited
        // row as unadjudicated, which is the exact miscount the keys exist to
        // end, so `jdk-only-bridge-ratchet.py` refuses the older shape rather
        // than degrading.
        // Schema 5 (2026-08-17): rows carry `invocations_complete` and the
        // header carries `slots_with_incomplete_invocations`. See
        // [`NATIVE_CENSUS_SCHEMA_VERSION`] for why this is a bump and not an
        // additive-at-the-same-version change, and for the two consumers that
        // pin it by equality.
        out.push_str(&format!(
            "{{\n  \"schema_version\": {},\n",
            NATIVE_CENSUS_SCHEMA_VERSION
        ));
        out.push_str(&format!(
            "  \"image_adjudication\": {},\n",
            image_verdicts.is_some()
        ));
        out.push_str(&format!(
            "  \"mode\": {},\n",
            json_escape(self.compatibility_mode().as_str())
        ));
        if cm.is_none() {
            out.push_str(
                "  \"partial\": true,\n  \"partial_reason\": \"class-manager lock unavailable \
                 on the System.exit path; every real_declaring_method is null because the \
                 class store was not read, not because the class was absent\",\n",
            );
        }
        out.push_str("  \"counts\": {\n");
        out.push_str(&format!("    \"intrinsic\": {n_intrinsic},\n"));
        out.push_str(&format!("    \"bridge\": {n_bridge},\n"));
        out.push_str(&format!("    \"synthetic-stub\": {n_stub},\n"));
        out.push_str(&format!("    \"total\": {}\n", rows.len()));
        // Per-kind dispatch totals, the same three keys in the same order as
        // `vm-cli`'s writer. Separate from `counts` on purpose: `counts` is
        // registrations, this is invocations, and the two have been confused
        // before.
        out.push_str("  },\n  \"invocations\": {\n");
        out.push_str(&format!(
            "    \"intrinsic\": {},\n",
            self.natives
                .native_methods
                .invocations_of_kind(NativeKind::Intrinsic)
        ));
        out.push_str(&format!(
            "    \"bridge\": {},\n",
            self.natives
                .native_methods
                .invocations_of_kind(NativeKind::Bridge)
        ));
        out.push_str(&format!(
            "    \"synthetic-stub\": {}\n",
            self.natives
                .native_methods
                .invocations_of_kind(NativeKind::SyntheticStub)
        ));
        out.push_str("  },\n");
        // How many slots have a dispatch path that has declared itself
        // uncounted. Emitted between `invocations` and `natives` so it reads as
        // the qualifier on the block immediately above it. Cold: one relaxed
        // load per slot, once, at report time — the same shape and the same
        // justification as the three `invocations_of_kind` calls above.
        out.push_str(&native_census_incomplete_header_json(
            self.natives
                .native_methods
                .slots_with_incomplete_invocations(),
        ));
        // j27 CB17-6: the bodies the interpreter table serves beside the rows.
        out.push_str(&native_census_table_intrinsics_json(
            &rows,
            self.compatibility_mode().is_jdk_only(),
        ));
        out.push_str("  \"natives\": [");

        // One read lock for the whole loop: `real_declaring_method` asks the
        // class manager a question per row, and re-acquiring L10 tens of
        // thousands of times would turn a diagnostic dump into a contention
        // event. The caller acquires it (or, on the `System.exit` path, fails
        // to) so that all three artefacts describe the same instant.
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("\n    {\n");
            out.push_str(&format!("      \"class\": {},\n", json_escape(&row.class)));
            out.push_str(&format!("      \"name\": {},\n", json_escape(&row.name)));
            out.push_str(&format!(
                "      \"descriptor\": {},\n",
                json_escape(&row.descriptor)
            ));
            out.push_str(&format!(
                "      \"kind\": {},\n",
                json_escape(row.kind.as_str())
            ));
            match &row.registered_by {
                Some(site) => out.push_str(&format!(
                    "      \"registered_by\": {},\n",
                    json_escape(&if verbose {
                        site.clone()
                    } else {
                        redact_registration_site(site)
                    })
                )),
                None => out.push_str("      \"registered_by\": null,\n"),
            }
            match row.overwrote {
                Some(kind) => out.push_str(&format!(
                    "      \"overwrote\": {},\n",
                    json_escape(kind.as_str())
                )),
                None => out.push_str("      \"overwrote\": null,\n"),
            }
            // "How many dispatches resolved this slot by name or id" — and,
            // inseparably, whether that is a total or a floor. Before schema 5
            // only the first half was emitted, while the registry had already
            // been told about 25 bypassing slots; a reader could not tell a
            // counted zero from an uncounted one. See
            // [`native_census_invocations_json`].
            out.push_str(&native_census_invocations_json(
                row.invocations,
                row.invocations_complete,
            ));
            // "Did anyone adjudicate this kind, or did it inherit an ambient
            // `set_category`?" — the discriminator the 157-entry
            // reclassification needs. See `NativeCensusEntry::kind_stated`.
            out.push_str(&format!("      \"kind_stated\": {},\n", row.kind_stated));
            // "…or did nobody have an opinion at all?" — `kind_stated` is false
            // on every row a deliberate `with_category` scope covers, so it
            // cannot answer that. See `NativeCensusEntry::kind_chosen`.
            out.push_str(&format!("      \"kind_chosen\": {},\n", row.kind_chosen));
            // "Would a dispatch of this triple reach THIS row?" A superseded
            // registration answers `false` and can never be dispatched, so it
            // is not a registration any reclassification wave has to decide.
            // See `NativeCensusEntry::owns_slot` for the measured size of the
            // difference.
            out.push_str(&format!("      \"owns_slot\": {},\n", row.owns_slot));

            match cm {
                Some(cm) => {
                    let declaring = cm
                        .get_loaded_class_id(&row.class)
                        .and_then(|id| cm.get_class(id));
                    let method = declaring.and_then(|c| c.find_method(&row.name, &row.descriptor));
                    out.push_str("      \"real_declaring_method\": {");
                    out.push_str(&format!(
                        "\"loaded\": {}, \"declared\": {}, \"acc_native\": {}, \"has_code\": {}",
                        declaring.is_some(),
                        method.is_some(),
                        method.is_some_and(|m| m.is_native()),
                        method.is_some_and(|m| !m.is_native() && !m.is_abstract()),
                    ));
                    out.push_str("},\n");
                }
                // Not `{"loaded": false, …}`: that is a *measurement* saying the
                // run never touched the class, and it would be a lie here. See
                // this function's `partial` note.
                None => out.push_str("      \"real_declaring_method\": null,\n"),
            }
            match image_verdicts.as_ref().map(|v| &v[i]) {
                // Schema 4 adds the four `inherited_*` keys. They are emitted on
                // EVERY row — `null`/`false` included — so that "the named class
                // declares it", "a SUPERTYPE declares it" and "nothing in the
                // hierarchy does" are three distinguishable answers instead of
                // one absent key and a guess. Before them, `declared: false` was
                // read as "dead registration" and was wrong for three quarters
                // of the bucket; see `ImageMethodVerdict::inherited_from`.
                Some(v) => out.push_str(&format!(
                    "      \"image_declaring_method\": {{\"image_has_class\": {}, \
                     \"declared\": {}, \"acc_native\": {}, \"has_code\": {}, \
                     \"inherited_from\": {}, \"inherited_acc_native\": {}, \
                     \"inherited_has_code\": {}, \"inherited_abstract\": {}}}\n",
                    v.image_has_class,
                    v.declared,
                    v.acc_native,
                    v.has_code,
                    match &v.inherited_from {
                        Some(c) => json_escape(c),
                        None => "null".to_string(),
                    },
                    v.inherited_acc_native,
                    v.inherited_has_code,
                    v.inherited_abstract,
                )),
                None => out.push_str("      \"image_declaring_method\": null\n"),
            }
            out.push_str("    }");
        }

        if !rows.is_empty() {
            out.push_str("\n  ");
        }
        out.push_str("]\n}\n");

        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        file.write_all(out.as_bytes())?;
        file.sync_all()?;
        Ok((n_intrinsic, n_bridge, n_stub))
    }

    /// Class-origin census (`--dump-class-origins`, `real-jdk-mode.md` §5, §9).
    ///
    /// Schema 1:
    ///
    /// ```json
    /// {
    ///   "schema_version": 1,
    ///   "counts": { "boot-image": 312, "...": 0, "total": 343 },
    ///   "classes": [
    ///     { "name": "java/lang/String", "origin": "boot-image", "reason": null,
    ///       "requested_by": null, "real_bytes_found": true, "loader_id": 0 }
    ///   ]
    /// }
    /// ```
    ///
    /// `counts` is seeded to zero from the full
    /// [`ClassOrigin::as_str`](cratonvm_classloading::class_origin::ClassOrigin::as_str)
    /// vocabulary, so `"compatibility-stub": 0` is *stated* rather than implied
    /// by an absent key — the one assertion a strict-mode reader most wants and
    /// the one an omission would silently fake. Rows sort by
    /// `(name, loader_id, origin)`: a class name legitimately appears once per
    /// defining loader, and twice with different origins during an in-place
    /// stub-upgrade window, so all three are needed for a total order.
    /// Returns the row count.
    ///
    /// The **only** class-origin writer: `--dump-class-origins` calls this,
    /// and so does any embedder. `vm-cli` used to carry an independently
    /// written copy; the one difference worth keeping from it was the
    /// redaction, which is why this entry point now takes `verbose` instead of
    /// telling embedders to redact the rows themselves. `verbose` is
    /// `--explain-jdk-only`: **false redacts, and false is the default** for
    /// every caller that cannot express the flag.
    ///
    /// Redaction covers `name`, `reason` and `requested_by` (via
    /// [`redact_absolute_paths`]), because all three can carry an absolute path
    /// out of a `CompatibilityStub` reason or a requester attribution, and this
    /// file gets committed as a baseline and pasted into issues. `origin` is a
    /// closed tag vocabulary and is never redacted.
    ///
    /// Emitted in **both** modes: under `Compatible` the origins are recorded
    /// but nothing is refused (§5), which is what makes the census meaningful
    /// as a baseline *before* enforcement lands (§10).
    pub fn dump_class_origins_json(
        &self,
        path: impl AsRef<std::path::Path>,
        verbose: bool,
    ) -> std::io::Result<usize> {
        let cm = self.classes.class_manager.read();
        self.dump_class_origins_json_with(path, verbose, Some(&cm))
    }

    /// [`Self::dump_class_origins_json`] against a class manager the caller
    /// already holds — or, with `cm = None`, a **labelled empty** census.
    ///
    /// `None` exists for exactly one caller: the `System.exit` path, which runs
    /// on an arbitrary Java thread and may find the class-manager lock held (see
    /// [`Self::try_write_jdk_only_dumps_for_exit`]). Every row of this census
    /// comes from the class store, so without the lock there is nothing to
    /// report — and a zero-row census that does not *say* it is empty for want
    /// of a lock reads as "this run fabricated nothing", which is the exact
    /// false-green contract §11's acceptance criteria must be immune to. The
    /// `"partial": true` key is therefore not decoration; it is what makes the
    /// degraded file safe to publish.
    pub(crate) fn dump_class_origins_json_with(
        &self,
        path: impl AsRef<std::path::Path>,
        verbose: bool,
        cm: Option<&crate::classloading::ClassManager>,
    ) -> std::io::Result<usize> {
        let out = match cm {
            Some(cm) => {
                let mut rows = cm.dump_class_origins();
                let n = rows.len();
                (render_class_origins_json(&mut rows, verbose), n)
            }
            None => (render_partial_class_origins_json(), 0),
        };

        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        file.write_all(out.0.as_bytes())?;
        file.sync_all()?;
        Ok(out.1)
    }

    /// Snapshot of the three **process-global** Real-JDK violation sinks, in a
    /// stable order: `[jit-compile, jit-helpers, interpreter-dispatch]`.
    ///
    /// The other two sinks the report folds — `refused_registrations()` and
    /// `origin_violations()` — are VM-scoped: they hang off this `SharedVm`'s
    /// registry and class manager. The three here are `static`s, because the
    /// sites that record them (a JIT compile-time bytecode scan, a JIT runtime
    /// fast path reached from emitted code, and the interpreter's name-triple
    /// dispatch resolver) have no `&SharedVm` at hand cheaply enough to carry
    /// one. That difference is why they are gated below and why the ordering
    /// contract of the array matters — see [`JDK_ONLY_PROCESS_SINKS`].
    ///
    /// # Why `Compatible` returns three empty vectors without touching them
    ///
    /// Two independent reasons, and both are needed:
    ///
    /// 1. **Cost.** Each accessor takes a `parking_lot::Mutex` and clones a
    ///    `Vec<JdkOnlyViolation>`. A `Compatible` VM that asked for
    ///    `--real-jdk-report` would pay three uncontended locks for three
    ///    guaranteed-empty answers. The early return is free.
    /// 2. **Honesty.** The sinks are process-wide, so rows a sibling `JdkOnly`
    ///    VM recorded are visible from here. Folding them into a report whose
    ///    `mode` field says `"compatible"` would attribute another VM's policy
    ///    decisions to this one. A `Compatible` VM has no Real-JDK policy, so
    ///    it reports no Real-JDK violations, full stop. (Until 2026-09-12 the
    ///    JIT's policy was itself a process-global latch that only moved toward
    ///    strict, so a `Compatible` VM could even compile under strict policy
    ///    and record refusals of its own. The policy is now a per-compilation
    ///    argument taken from each VM's own config, and the latch is gone.)
    ///
    /// t1 wave 3 (lane Y): the two JIT sinks are now THIS VM's
    /// (`JitCache::jdk_only_sinks`); the compile-time one adds only the rows
    /// work with no VM in scope recorded (`compile_rows_for_report`). t1 wave 4
    /// (lane Y2): the interpreter's sink (index 2) is this VM's too
    /// (`NativeRealm::native_shadow_sink`), so the three lists no longer carry
    /// a sibling VM's rows; the name `process_violations` is historical. The
    /// only process-wide remainder is the compile sink's no-VM rows above
    /// (unit tests, the positional compile shim).
    ///
    /// Each sink is append-only and bounded (4096 entries by default since
    /// 2026-08-20, `CRATONVM_NATIVE_SHADOW_SINK_CAP`) with an internal dedup, so
    /// a positional watermark into these vectors is stable across calls — that
    /// is what `--trace-jdk-only` uses to drain incrementally. Do not re-derive
    /// the bound from a literal here: the report's `observation_sink` object
    /// publishes each sink's actual `cap`, and what it dropped.
    pub fn jdk_only_process_violations(
        &self,
    ) -> [Vec<cratonvm_types::error::JdkOnlyViolation>; JDK_ONLY_PROCESS_SINKS] {
        if !self.compatibility_mode().is_jdk_only() {
            // Written out rather than `std::array::from_fn` so that raising
            // `JDK_ONLY_PROCESS_SINKS` for a new sink is a compile error here
            // and in the array below, instead of silently returning a slot
            // nothing ever fills.
            return [Vec::new(), Vec::new(), Vec::new()];
        }
        [
            // Compile-time thin-direct-native binds refused by `try_compile`'s
            // bytecode scan. `NativeShadowsBytecode` with
            // `native_kind: "jit-thin-direct-helper"`.
            self.jit.jit_cache.jdk_only_sinks().compile_rows_for_report(),
            // Runtime by-name fast-path admissions refused at cache-fill time.
            // Carries whatever `resolve_native_dispatch_wave1` produced, which
            // for this site is a `SyntheticNativeInvocation` `Reject`.
            crate::jit::helpers::jdk_only_jit_helper_violations(self),
            // Interpreter dispatch: concrete bytecode preferred over a
            // registered non-intrinsic native. `NativeShadowsBytecode` with the
            // *registered* `NativeKind::as_str()`, so these never collide with
            // the JIT's `"jit-thin-direct-helper"` rows even for the same
            // triple — the two are different facts about the same method and
            // both belong in the report.
            crate::vm::jdk_only_native_shadow_observations(&self.natives.native_shadow_sink),
        ]
    }

    /// Exact refusal **event** counts from the same three (per-VM since t1
    /// waves 3-4) sources, for the report's `refusals` block and the `--trace-jdk-only`
    /// summary line.
    ///
    /// These are not a second spelling of `violations[]`. Every sink above is
    /// deduplicated by triple and CAPPED (see `observation_sink` in the report
    /// for each one's cap and drop count); these counters are uncapped
    /// `AtomicU64`s incremented once per refusal event. A run that
    /// refuses `HashMap.put` ten million times contributes **one** row to
    /// `violations[]` and ten million to `jit_direct_native_binds`. Reporting
    /// only the rows would understate the blast radius; reporting only the
    /// counters would lose the identities. Both, in separate blocks, is the
    /// only shape that double-counts neither.
    ///
    /// `jit_inline_cache_natives` is the one source with **no** structured
    /// violation behind it: `record_jdk_only_ic_native_refusal` increments and
    /// returns, because the inline-cache publication site has an entry address
    /// and not a name triple, so there is nothing to name. Per the task's rule,
    /// it is surfaced as a count rather than as a fabricated row.
    ///
    /// Zero in `Compatible` mode, for the same two reasons as
    /// [`SharedVm::jdk_only_process_violations`]. Reading them is cold (relaxed
    /// loads, plus a short sink lock for the direct-helper tallies), and every
    /// one is this VM's own, but the mode test is still
    /// checked first so a `Compatible` report cannot inherit a sibling VM's
    /// latched-strict counters.
    pub fn jdk_only_refusal_counts(&self) -> JdkOnlyRefusalCounts {
        if !self.compatibility_mode().is_jdk_only() {
            return JdkOnlyRefusalCounts::default();
        }
        JdkOnlyRefusalCounts {
            // t1 wave 3 (lane Y): this VM's sinks (`JitCache::jdk_only_sinks`).
            jit_direct_native_binds: self.jit.jit_cache.jdk_only_sinks().compile_events_for_report(),
            jit_inline_cache_natives: self
                .jit
                .jit_cache
                .jdk_only_sinks()
                .ic_native_refusals_for_report(),
            jit_fastpath_admissions: crate::jit::helpers::jdk_only_jit_fastpath_refusals(self),
            // t1 wave 6 (lane Y3): this VM's direct-helper gate tallies, also
            // on `JitCache::jdk_only_sinks` (they were summed process-wide).
            jit_direct_helper_fastpaths: crate::jit::helpers::jit_direct_helper_jdk_only_refusals(
                self,
            ),
            jit_direct_helper_admissions:
                crate::jit::helpers::jit_direct_helper_jdk_only_admissions(self),
            // t1 wave 4 (lane Y2): this VM's resolver sink.
            interpreter_bytecode_preferred: crate::vm::jdk_only_native_shadow_attempts(
                &self.natives.native_shadow_sink,
            ),
        }
    }

    /// Real-JDK violation/counter report (`--real-jdk-report`, §9). Schema 1,
    /// spelled exactly as the contract prints it:
    ///
    /// ```json
    /// {
    ///   "schema_version": 1,
    ///   "mode": "jdk-only",
    ///   "jdk_feature": 25,
    ///   "violations": [],
    ///   "counts": {
    ///     "boot_image_classes": 312, "application_classes": 18,
    ///     "generated_classes": 4,   "compatibility_classes": 0,
    ///     "bridge_invocations": 1082, "intrinsic_invocations": 4301,
    ///     "synthetic_stub_invocations": 0
    ///   },
    ///   "refusals": {
    ///     "jit_direct_native_binds": 0, "jit_inline_cache_natives": 0,
    ///     "jit_fastpath_admissions": 0,
    ///     "jit_direct_helper_fastpaths": 0, "jit_direct_helper_admissions": 0,
    ///     "interpreter_bytecode_preferred": 0
    ///   },
    ///   // EVERY key under `refusals` counts something PREVENTED, not
    ///   // something that happened. `jit_fastpath_admissions` is the one that
    ///   // reads backwards in isolation: it counts by-name native fast-path
    ///   // admissions REFUSED, so a LARGE value means strict mode blocked a
    ///   // lot, not that a lot leaked through. Measured on RJitStringLayout
    ///   // 2026-08-19: 38 refusals with 0 direct binds and 0 inline-cache
    ///   // natives — i.e. the JIT's native shortcuts are being denied, which
    ///   // is the intended posture. See known-issues/jdk-only/G87-1.
    ///   // A reader checking "is strict mode leaking JIT shortcuts?" would
    ///   // draw the opposite conclusion from a non-zero value here.
    ///   //
    ///   // 2026-09-22 — AND A REFUSAL IS NOT ALWAYS A BLOCK. Until this date
    ///   // `jdk_only_admit_jit_fast_native` handed §7 a step-3 input
    ///   // `vm_exec::invoke_or_native` — the tail every refusal routes to —
    ///   // does not compute for a `Bridge`, so a Bridge over concrete
    ///   // bytecode was counted here as refused and then RUN by the tail.
    ///   // Every pre-2026-09-22 reading of this column over-states what
    ///   // strict mode prevented by however many of its refusals were
    ///   // Bridges; the fix is in `jit_fast_native_bytecode_terms`, and
    ///   // `docs/feature-designs/native-dispatch-architecture.md`
    ///   // has the measurement. A refusal counted from here now means the
    ///   // tail refuses too.
    ///   //
    ///   // The block name carries the semantics; the field names do not
    ///   // repeat it. Renaming would break the schema, so this note is the
    ///   // fix. If the schema is ever versioned up, rename it then.
    ///   "observation_sink": {
    ///     "recorded": 81, "cap": 4096, "saturated": false,
    ///     "truncated": false, "dropped": 0,
    ///     "jit_fastpath": { "recorded": 3, "cap": 4096,
    ///                       "truncated": false, "dropped": 0 },
    ///     "jit_compile":  { "recorded": 0, "cap": 256,
    ///                       "truncated": false, "dropped": 0 }
    ///   }
    /// }
    /// ```
    ///
    /// # `intrinsic_invocations_by_triple` (W7-95 N8, additive, not in the
    /// example above)
    ///
    /// ```json
    ///   "intrinsic_invocations_by_triple": [
    ///     {"class": "java/lang/Math", "name": "floorDiv", "descriptor": "(II)I",
    ///      "invocations": 5000, "invocations_complete": false}
    ///   ]
    /// ```
    ///
    /// `NativeKind::Intrinsic` is exempt from `--real-jdk` shadow enforcement,
    /// so before this field the report could say `counts.intrinsic_invocations
    /// = 5000` and name none of the calls behind it — the reproducer in
    /// `W7-95-intrinsic-semantics-census.md` ran
    /// `--real-jdk-report` and `--dump-native-registry` side by side and
    /// cross-referenced them by hand because this file had nothing to search.
    /// One row per Intrinsic triple this run actually dispatched
    /// (`owns_slot && invocations > 0`), sorted by `(class, name, descriptor)`
    /// — the same per-row `invocations`/`invocations_complete` pair
    /// `dump_native_census_json` already writes, joined into this file instead
    /// of a second one. A triple with zero invocations is not listed: silence,
    /// not a claim that it is untested — `--dump-native-registry` is still the
    /// place to ask "what is registered but never named here".
    ///
    /// # `observation_sink` — is `violations[]` the population, or a floor?
    ///
    /// Additive, and here is why it is not decoration. The §7 shadow rows in
    /// `violations[]` come out of a bounded, deduplicated sink
    /// ([`crate::vm::jdk_only_native_shadow_cap`], 256 distinct rows by default,
    /// shared by both recorders), and until this object existed a TRUNCATED list
    /// was identical in shape to a complete one. Every reader who took the list
    /// as the population was reading a floor with nothing in the file to say
    /// so — `G60-1-what-jdk-only-still-overrides-RESOLVED-20260817.md`
    /// §4 had to instruct its readers to count the rows by hand and compare them
    /// against a constant compiled into the VM, which is not a check anyone
    /// performs twice.
    ///
    /// `cap` is emitted rather than assumed for the same reason: it is an
    /// operator override (`CRATONVM_NATIVE_SHADOW_SINK_CAP`), so a reader comparing
    /// `recorded` against a hard-coded 256 would be comparing against the wrong
    /// number on exactly the runs that raised it.
    ///
    /// `saturated` is **not** `recorded == cap`: a run whose last distinct
    /// observation exactly fills the sink drops nothing. See
    /// [`crate::vm::jdk_only_native_shadow_sink_saturated`].
    ///
    /// # 2026-08-20: `truncated`, `dropped`, and the other two sinks
    ///
    /// **A boolean nobody reads was the whole signal, and it capped every
    /// number in `docs/known-issues/jdk-only/`.** Three fixes, in the order
    /// they matter:
    ///
    /// 1. **`dropped`** — a monotonic counter that keeps counting past the cap,
    ///    so `recorded + dropped` is the population and a floor becomes a
    ///    total. This is the load-bearing half; a bigger cap is still a cap.
    /// 2. **`truncated`** — the same bit as `saturated`, emitted under the name
    ///    a reader actually looks for. `saturated` stays because every record
    ///    written before today quotes it.
    /// 3. **`jit_fastpath` / `jit_compile`** — the other two bounded
    ///    collections feeding `violations[]` had *no* saturation signal at all,
    ///    so `truncated: false` at the top level answered for one source of
    ///    three. `jit_compile`'s two fields rendered `null`, not `false`,
    ///    because that sink lived in `cratonvm_jit` with no counter and an
    ///    unmeasured thing must not render as a clean one.
    ///
    ///    **CLOSED 2026-08-22 (H1-1 §5.1).** `cratonvm_jit` now carries
    ///    `JDK_ONLY_VIOLATIONS_DROPPED` and its cap honours the same
    ///    `CRATONVM_NATIVE_SHADOW_SINK_CAP` as the other two, so all three
    ///    collections answer both questions and none of them renders `null`.
    ///    `run.sh`'s saturation verdict is no longer `UNKNOWN` by construction
    ///    — a strict run can say "totals" and mean it. Reports written by an
    ///    OLDER binary still carry the `null`, which is why
    ///    `regression-suite/harness-census.sh` keeps its third verdict.
    ///
    /// `saturated: true` used to condemn a COUNTER as well as a list — the
    /// former `refusals.interpreter_shadow_unenforced` stopped advancing once
    /// the sink filled. That counter is gone (t1 wave 6, lane Y3): its only
    /// recorder, the step-1 door's "a `Bridge` ran in front of bytecode"
    /// observation, lost its caller when dispatch became bytecode-first
    /// (`3d31008561`), so it read 0 on every run. `saturated` now qualifies
    /// only the IDENTITIES, which is what `dropped` counts.
    ///
    /// The **only** report writer: `--real-jdk-report` calls this. `verbose` is
    /// `--explain-jdk-only`; **false redacts and is the default**, applied
    /// post-hoc to each rendered body because `JdkOnlyViolation::to_json()`
    /// (unlike `render()`) has no `verbose` parameter and threading one through
    /// `types` is not this file's call.
    ///
    /// # `violations` unions all five sources
    ///
    /// | § | source | accessor | scope |
    /// |---|--------|----------|-------|
    /// | §4 | native registrations refused | `NativeMethodRegistry::refused_registrations` | this VM |
    /// | §5 | compatibility classes requested | `ClassManager::origin_violations` | this VM |
    /// | §7 | JIT compile-time direct binds refused | `JitCache::jdk_only_sinks().compile_rows_for_report` | VM (t1 w3) |
    /// | §7 | JIT fast-path admissions refused | `crate::jit::helpers::jdk_only_jit_helper_violations(vm)` | VM (t1 w3) |
    /// | §7 | interpreter preferred bytecode | `crate::vm::jdk_only_native_shadow_observations` | VM (t1 w4) |
    ///
    /// The last three arrive through
    /// [`SharedVm::jdk_only_process_violations`], which documents why they are
    /// process-scoped and why `Compatible` gets three empty vectors instead of
    /// three lock acquisitions. Every row is rendered by
    /// `JdkOnlyViolation::to_json()` so the wire shape is owned by the type
    /// rather than re-derived here, and the union is **sorted by
    /// `(kind, summary)`**.
    ///
    /// A report that folded only the first two would print `"violations": []`
    /// for a run whose JIT refused ten thousand native binds, and an empty
    /// array reads as *clean*, not as *not measured*. That is strictly worse
    /// than printing nothing, which is why the fold is all-or-nothing.
    ///
    /// # No source is counted twice
    ///
    /// The five lists are disjoint by construction, and the union is **not**
    /// deduplicated — deduplicating would be wrong here, because two sources
    /// reporting the same method are reporting two different events. The three
    /// process sinks each dedup internally by triple, and where two of them can
    /// name the same triple they disagree on `native_kind`
    /// (`"jit-thin-direct-helper"` for the JIT's own baked helper versus the
    /// registered `NativeKind::as_str()` for the interpreter's observation), so
    /// the rows differ in both `summary()` and `to_json()` and sort apart.
    ///
    /// The refusal **counters** are the other half of the same rule, and they
    /// deliberately do not live in this array: a counter increment that also
    /// produced a row would be double-counting if it were rendered as a second
    /// row, and a counter with no row behind it (`jit_inline_cache_natives`)
    /// would be a fabricated row. Both go to the `refusals` sibling object
    /// instead — see [`JdkOnlyRefusalCounts`] for why it is a sibling and not
    /// four more keys in the closed §9 `counts` set.
    ///
    /// Sorting rather than preserving recording order is the opposite of the
    /// choice [`SharedVm::dump_native_census_json`] makes, and for a reason
    /// that does not apply here: native registrations all happen on the boot
    /// thread and their relative order *is* the fact (the `overwrote` chain),
    /// whereas class-origin violations are recorded from arbitrary application
    /// threads, so recording order is genuinely nondeterministic and carries no
    /// meaning worth preserving. Sorting is what makes two runs of the same
    /// program produce byte-identical reports.
    ///
    /// The four class counters are a **partition** of the ten
    /// `ClassOrigin::as_str()` tags — see [`fold_origin_buckets`]. Their sum is
    /// the row count, so no class can go missing between this summary and
    /// `--dump-class-origins`.
    ///
    /// `counts` carries exactly the seven keys §9 spells and no more: no
    /// `total_classes`, no `vm_internal_classes`. A reader that needs either
    /// can add the partition up or read the class-origin census, and every
    /// extra key here is one more thing the two artefacts can disagree about.
    /// `refusals` is a **sibling** object for exactly that reason — widening
    /// `counts` would break both the closed set §9 documents and the partition
    /// the `debug_assert_eq!` below proves.
    ///
    /// `refusals` is emitted in both modes with all four keys, and is all-zero
    /// in `Compatible` by construction. A key set that varied with `mode` would
    /// force every consumer to branch on the mode before reading a number;
    /// zeros say "measured, none" where an absent key says "unknown".
    ///
    /// Returns `(violations, compatibility_classes)` — the violation count so
    /// the caller can decide whether to exit non-zero without re-reading the
    /// file, and the compatibility-class count because that is the number the
    /// launcher reports on its status line and §11 gates on.
    pub fn dump_jdk_only_report_json(
        &self,
        path: impl AsRef<std::path::Path>,
        verbose: bool,
    ) -> std::io::Result<(usize, usize)> {
        let cm = self.classes.class_manager.read();
        self.dump_jdk_only_report_json_with(path, verbose, Some(&cm))
    }

    /// [`Self::dump_jdk_only_report_json`] against a class manager the caller
    /// already holds — or, with `cm = None`, the report the `System.exit` path
    /// can still produce without it.
    ///
    /// Two of the five violation sources are reachable without the class
    /// manager (the native registry's refused registrations, and the three
    /// process-global JIT/dispatch sinks), so a `None` report is not empty —
    /// it is missing the `compatibility-class-requested` rows and the four
    /// class-bucket counts. Marked `"partial": true`, and the class buckets are
    /// **omitted** rather than written as zero, because a zero there would say
    /// "no compatibility classes were requested" — the single most misleading
    /// thing this file can say.
    pub(crate) fn dump_jdk_only_report_json_with(
        &self,
        path: impl AsRef<std::path::Path>,
        verbose: bool,
        cm: Option<&crate::classloading::ClassManager>,
    ) -> std::io::Result<(usize, usize)> {
        use cratonvm_native_api::NativeKind;

        // `(kind, summary, body)`: the first two are the sort key, the third is
        // what gets written.
        let registry = &self.natives.native_methods;
        // W7-95 N8: the same census `--dump-native-registry` reads, captured
        // once here so `intrinsic_invocations_by_triple` below can join
        // per-row invocation counts into THIS report instead of requiring a
        // second file and a hand cross-reference. See that field for why.
        let census_rows = registry.census();
        let mut violations: Vec<(String, String, String)> = registry
            .refused_registrations()
            .iter()
            .map(|v| (v.kind().to_string(), v.summary(), v.to_json()))
            .collect();
        // One L10 acquisition for both questions the class manager answers
        // here — the census and the violation list must describe the same
        // instant, and re-locking between them would let a still-running
        // thread load a class in the gap. The caller holds it.
        let (origins, origin_violations) = match cm {
            Some(cm) => {
                let origins = cm.dump_class_origins();
                let violations: Vec<(String, String, String)> = cm
                    .origin_violations()
                    .iter()
                    .map(|v| (v.kind().to_string(), v.summary(), v.to_json()))
                    .collect();
                (Some(origins), violations)
            }
            None => (None, Vec::new()),
        };
        violations.extend(origin_violations);
        // §7: the three process-global sinks — JIT compile-time refusals, JIT
        // runtime fast-path refusals, and the interpreter's bytecode-wins
        // observations. Empty (and untouched, so no lock and no allocation) in
        // `Compatible`; see `jdk_only_process_violations` for why the mode gate
        // lives there rather than here.
        //
        // Folded into the same `(kind, summary, body)` tuple as the two
        // VM-scoped sources so all five sort together under one comparator. The
        // sort is what makes the report reproducible, and it only works if
        // every source enters the same vector before it runs.
        //
        // Bound rather than consumed in place, so the `observation_sink` block
        // at the bottom can report each sink's ROW COUNT without re-taking its
        // mutex and re-cloning its `Vec` — and, more importantly, so the row
        // count it publishes is the same snapshot these rows came from. Two
        // separate reads could disagree on a VM that is still running.
        let process_sinks = self.jdk_only_process_violations();
        // §1.7's class-origin field, resolved HERE and nowhere on the dispatch
        // path. The three sinks above are filled by helpers that hold a name
        // triple and no class store — deliberately, because a store read lock
        // per observation is the cost their `#[cold]` split exists to avoid —
        // so every `native-shadows-bytecode` row arrives with `class_origin:
        // null` and gets it filled in once, cold, from the census that was
        // already taken three statements up.
        //
        // The SAME `dump_class_origins()` snapshot the report's `classes` block
        // is built from, not a second lookup: a row's origin and the census row
        // for that class must not be able to disagree, and re-asking would let
        // a still-running thread load a class in the gap — the reason the
        // comment above takes one L10 acquisition for both questions.
        //
        // A class the census does not name leaves the field `null`, which is
        // the honest answer: it means the triple was observed but its class is
        // not in the store under that name (an unloaded receiver, or a JIT row
        // naming a class that has since been unloaded). `<unknown>` in the
        // rendered form, absent from the registration-time variants entirely.
        let class_origins: std::collections::HashMap<&str, &str> = origins
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|e| (e.name.as_str(), e.origin.as_str()))
            .collect();
        for sink in &process_sinks {
            violations.extend(sink.iter().map(|v| {
                let resolved = v
                    .clone()
                    .with_class_origin(class_origins.get(v.class()).map(|o| (*o).to_string()));
                (
                    resolved.kind().to_string(),
                    resolved.summary(),
                    resolved.to_json(),
                )
            }));
        }
        // Deterministic order, primary key unchanged: `(kind, summary)`.
        // Class-origin rows are recorded from arbitrary application threads and
        // the JIT sinks are filled from compiler and mutator threads alike, so
        // recording order is nondeterministic for every source except native
        // registration — sorting is the only way two runs of the same program
        // produce identical bytes.
        //
        // `body` is a **tiebreaker**, not a third sort dimension: two rows can
        // tie on `(kind, summary)` and still differ, because `summary()` drops
        // the provenance fields `to_json()` keeps (`registered_by` on
        // `SyntheticNativeRegistered`, `call_site` on
        // `SyntheticNativeInvocation`). Before, such a tie fell back to
        // insertion order, which for the class-manager source is exactly the
        // nondeterministic order this sort exists to erase. Including the body
        // makes the comparator total, so the output no longer depends on which
        // thread got there first. Rows that tie on all three are genuinely
        // indistinguishable on the wire and their relative order cannot be
        // observed.
        violations.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
        let violations: Vec<String> = violations
            .into_iter()
            .map(|(_, _, body)| {
                if verbose {
                    body
                } else {
                    redact_absolute_paths(&body)
                }
            })
            .collect();

        let buckets = origins.as_ref().map(|origins| {
            let buckets = fold_origin_buckets(origins);
            debug_assert_eq!(
                buckets.total(),
                origins.len() as u64,
                "the origin fold must be a partition — no class may fall between buckets"
            );
            buckets
        });

        let mut out = String::with_capacity(512 + 128 * violations.len());
        out.push_str("{\n  \"schema_version\": 1,\n");
        out.push_str(&format!(
            "  \"mode\": {},\n",
            json_escape(self.compatibility_mode().as_str())
        ));
        if buckets.is_none() {
            out.push_str(
                "  \"partial\": true,\n  \"partial_reason\": \"class-manager lock unavailable \
                 on the System.exit path; compatibility-class-requested violations and the \
                 four class-bucket counts are absent, the other four violation sources are \
                 complete\",\n",
            );
        }
        match self.jdk_feature_version() {
            Some(f) => out.push_str(&format!("  \"jdk_feature\": {f},\n")),
            None => out.push_str("  \"jdk_feature\": null,\n"),
        }
        out.push_str("  \"violations\": [");
        for (i, v) in violations.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("\n    ");
            // Re-indent the violation body so nested objects stay readable.
            out.push_str(&v.replace('\n', "\n    "));
        }
        if !violations.is_empty() {
            out.push_str("\n  ");
        }
        out.push_str("],\n  \"counts\": {\n");
        // Omitted, not zeroed, when the class store could not be read: a
        // `"compatibility_classes": 0` is the report's headline green result
        // and must never be produced by a missing measurement.
        if let Some(buckets) = &buckets {
            out.push_str(&format!(
                "    \"boot_image_classes\": {},\n",
                buckets.boot_image
            ));
            out.push_str(&format!(
                "    \"application_classes\": {},\n",
                buckets.application
            ));
            out.push_str(&format!(
                "    \"generated_classes\": {},\n",
                buckets.generated
            ));
            out.push_str(&format!(
                "    \"compatibility_classes\": {},\n",
                buckets.compatibility
            ));
        }
        // `bridge_invocations` is registry bridges PLUS JNI dispatches.
        //
        // The JNI function-pointer table (`natives.jni_native_methods`) is a
        // second, parallel registry of `dlsym` results, and it issues no
        // `NativeMethodId` — so `record_invocation`, which every other dispatch
        // path calls, cannot count it. Reporting only the registry's own total
        // made this key silently under-count genuine JNI bridges by exactly the
        // number of dispatches through that table. A `RegisterNatives` target
        // is a bridge in §1's sense (a real function in a real library — the
        // case §11 sanctions), so adding it here is what makes the key mean
        // what it says rather than "bridges we happened to be able to count".
        //
        // The other two keys are unaffected and stay exact:
        // `synthetic_stub_invocations` cannot gain a JNI row (a
        // `NativeKind::SyntheticStub` cannot be created in that table at all),
        // and neither can `intrinsic_invocations`.
        out.push_str(&format!(
            "    \"bridge_invocations\": {},\n",
            registry
                .invocations_of_kind(NativeKind::Bridge)
                .saturating_add(
                    self.natives
                        .jni_bridge_invocations
                        .load(std::sync::atomic::Ordering::Relaxed)
                )
        ));
        out.push_str(&format!(
            "    \"intrinsic_invocations\": {},\n",
            registry.invocations_of_kind(NativeKind::Intrinsic)
        ));
        out.push_str(&format!(
            "    \"synthetic_stub_invocations\": {}\n",
            registry.invocations_of_kind(NativeKind::SyntheticStub)
        ));
        // §9's `counts` ends here — seven keys, closed set, class buckets
        // partitioned. Everything below is a sibling object.
        out.push_str("  },\n  \"refusals\": {\n");
        let refusals = self.jdk_only_refusal_counts();
        out.push_str(&format!(
            "    \"jit_direct_native_binds\": {},\n",
            refusals.jit_direct_native_binds
        ));
        out.push_str(&format!(
            "    \"jit_inline_cache_natives\": {},\n",
            refusals.jit_inline_cache_natives
        ));
        out.push_str(&format!(
            "    \"jit_fastpath_admissions\": {},\n",
            refusals.jit_fastpath_admissions
        ));
        // Additive fields, 2026-09-22. A reader that does not know them
        // still parses the object; one that does gets the door the page
        // `jdkonly-thin-jit-direct-helpers-refuse-what-the-tail-then-runs-20260922`
        // is about, which until then had a `pub fn` nobody called and so
        // reported nothing anywhere.
        out.push_str(&format!(
            "    \"jit_direct_helper_fastpaths\": {},\n",
            refusals.jit_direct_helper_fastpaths
        ));
        out.push_str(&format!(
            "    \"jit_direct_helper_admissions\": {},\n",
            refusals.jit_direct_helper_admissions
        ));
        // The last key of `refusals` (no trailing comma). t1 wave 6, lane Y3:
        // `interpreter_shadow_unenforced` used to follow it. It counted a
        // `Bridge` that ran in front of concrete bytecode at the step-1 door,
        // which bytecode-first dispatch (`3d31008561`) made impossible, so it
        // was structurally 0; readers that look it up tolerate its absence.
        out.push_str(&format!(
            "    \"interpreter_bytecode_preferred\": {}\n",
            refusals.interpreter_bytecode_preferred
        ));
        // A SIBLING object, not three more `counts` keys. `counts` is a closed
        // set of seven and these are not counts of anything that happened —
        // they are the observation sink's capacity state, and what they do is
        // qualify `violations[]` rather than join it.
        //
        // Written unconditionally, including in `Compatible` where the sink is
        // never touched and this reads `recorded: 0, saturated: false`. An
        // object present only when it had something to report would make its
        // ABSENCE ambiguous between "nothing was dropped" and "this binary does
        // not answer the question" — the same class of mistake that omitting the
        // class buckets, rather than zeroing them, exists to avoid above.
        out.push_str("  },\n  \"observation_sink\": {\n");
        out.push_str(&format!(
            "    \"recorded\": {},\n",
            crate::vm::jdk_only_native_shadow_sink_len(&self.natives.native_shadow_sink)
        ));
        out.push_str(&format!(
            "    \"cap\": {},\n",
            crate::vm::jdk_only_native_shadow_cap()
        ));
        out.push_str(&format!(
            "    \"saturated\": {},\n",
            crate::vm::jdk_only_native_shadow_sink_saturated(&self.natives.native_shadow_sink)
        ));
        // `truncated` is the same bit as `saturated`, under the name a reader
        // reaches for. Both are emitted because `saturated` is the name every
        // record written before 2026-08-20 quotes, and renaming a key to make a
        // point is how a machine consumer breaks silently. They can never
        // disagree — one expression feeds both.
        out.push_str(&format!(
            "    \"truncated\": {},\n",
            crate::vm::jdk_only_native_shadow_sink_saturated(&self.natives.native_shadow_sink)
        ));
        // The number that turns the list from a floor into a total.
        // `recorded + dropped` is the distinct population this run observed;
        // `recorded` alone is what it had room to NAME. See
        // `crate::vm::jdk_only_native_shadow_sink_dropped` for the one way this
        // errs (upward, via filter collisions) and why.
        out.push_str(&format!(
            "    \"dropped\": {},\n",
            crate::vm::jdk_only_native_shadow_sink_dropped(&self.natives.native_shadow_sink)
        ));
        // Per-source sub-objects for the OTHER two bounded collections that
        // feed `violations[]`. Until 2026-08-20 this object described only the
        // interpreter's sink, so a reader who checked `truncated: false` and
        // concluded "the list is complete" was right about one of three
        // sources and had no way to ask about the other two.
        out.push_str("    \"jit_fastpath\": {\n");
        out.push_str(&format!(
            "      \"recorded\": {},\n",
            process_sinks[1].len()
        ));
        out.push_str(&format!(
            "      \"cap\": {},\n",
            crate::jit::helpers::jdk_only_jit_helper_violation_cap()
        ));
        out.push_str(&format!(
            "      \"truncated\": {},\n",
            crate::jit::helpers::jdk_only_jit_helper_sink_saturated(self)
        ));
        out.push_str(&format!(
            "      \"dropped\": {}\n",
            crate::jit::helpers::jdk_only_jit_helper_sink_dropped(self)
        ));
        out.push_str("    },\n");
        // `cratonvm_jit`'s compile-time sink used to render `null` here,
        // because it had no drop counter and an unmeasured thing must not
        // render as a clean one. **It has one since 2026-08-22** (H1-1 §5.1),
        // so all three bounded collections now answer the same two questions
        // and `run.sh`'s saturation verdict is no longer UNKNOWN by
        // construction. Its cap honours the same knob as the other two.
        out.push_str("    \"jit_compile\": {\n");
        out.push_str(&format!(
            "      \"recorded\": {},\n",
            process_sinks[0].len()
        ));
        out.push_str(&format!(
            "      \"cap\": {},\n",
            cratonvm_jit::jdk_only_violation_cap()
        ));
        out.push_str(&format!(
            "      \"truncated\": {},\n",
            self.jit.jit_cache.jdk_only_sinks().compile_dropped_for_report() > 0
        ));
        out.push_str(&format!(
            "      \"dropped\": {}\n",
            self.jit.jit_cache.jdk_only_sinks().compile_dropped_for_report()
        ));
        out.push_str("    }\n");
        // THE CENSUS EXEMPTION, published beside the figure it qualifies.
        //
        // Every `native-shadows-bytecode` count in this report is a floor for a
        // reason `observation_sink` cannot express: `NativeKind::Intrinsic` is
        // §1.4's reviewed exception, so neither recorder fires for it and the
        // exempted rows were never dropped for want of room — they were never
        // OFFERED. `WORKER-3-NOTE-5` (2026-08-22) measured the static size of
        // that at 398 registry rows, +29% on the then-published 1387, and asked
        // for it to be published beside any quoted figure; nothing had. This is
        // the dispatch-time half of the same fact, from the same run that
        // produced the rows above.
        //
        // A SIBLING object rather than rows in `violations[]`, because
        // `difftest/src/census.rs` tallies that array by `kind` and moving a
        // published number by redefining its population is the one thing a
        // measurement record must not do. `events` is per dispatch and
        // `recorded` per distinct triple — the same polarity as
        // `refusals.interpreter_bytecode_preferred` against
        // `observation_sink.recorded`, and to be read the same way.
        //
        // Written unconditionally, including in `Compatible` where it reads
        // zero, for the reason `observation_sink` gives about itself: an absent
        // block makes "nothing was exempt" and "this binary cannot answer the
        // question" the same JSON.
        out.push_str("  },\n  \"census_exemptions\": {\n");
        out.push_str("    \"intrinsic_shadows\": {\n");
        out.push_str(&format!(
            "      \"events\": {},\n",
            crate::vm::jdk_only_intrinsic_shadow_exempt_events(&self.natives.native_shadow_sink)
        ));
        let exempt = crate::vm::jdk_only_intrinsic_shadow_exempt_observations(
            &self.natives.native_shadow_sink,
        );
        out.push_str(&format!("      \"recorded\": {},\n", exempt.len()));
        out.push_str(&format!(
            "      \"cap\": {},\n",
            crate::vm::jdk_only_native_shadow_cap()
        ));
        out.push_str(&format!(
            "      \"truncated\": {},\n",
            crate::vm::jdk_only_intrinsic_shadow_exempt_saturated(&self.natives.native_shadow_sink)
        ));
        out.push_str(&format!(
            "      \"dropped\": {},\n",
            crate::vm::jdk_only_intrinsic_shadow_exempt_dropped(&self.natives.native_shadow_sink)
        ));
        out.push_str("      \"triples\": [");
        for (i, (class, method, descriptor)) in exempt.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "\n        {{\"class\": {}, \"method\": {}, \"descriptor\": {}}}",
                json_escape(class),
                json_escape(method),
                json_escape(descriptor)
            ));
        }
        out.push_str("\n      ]\n");
        out.push_str("    }\n");
        // A SIBLING object again, and for the sharpest version of
        // `observation_sink`'s reason. `CRATONVM_ENFORCE_NATIVE_SHADOW` does
        // not add rows to this report -- it REMOVES them, because an enforced
        // shadow does not dispatch and so never produces a
        // `bridge-ran-over-bytecode` row. An armed report and an unarmed one
        // were therefore indistinguishable in every field, and the armed
        // one's emptier `violations[]` reads as the better result. It is not;
        // it is a different question.
        //
        // `doors` is the other half. Until 2026-08-21 the dial had exactly
        // one live call site (`resolve_step1_native`), so `scope` alone would
        // still have overstated what an armed run measured: 890 of 947 armed
        // `Bridge` dispatches never asked it, and the then
        // `refusals.interpreter_shadow_unenforced` read `0` for all 890,
        // because that one call site was also the only recorder of the
        // native-won half. (Since bytecode-first dispatch, `3d31008561`, that
        // call site no longer lets a `Bridge` win over bytecode at all; the
        // counter was removed in t1 wave 6, lane Y3.)
        //
        // **`declined_no_bytecode` is NOT a leak count, and an earlier draft of
        // this object called it one.** `reached` minus `yielded` is the dial
        // being ASKED and answering no, and after the three cheap guards the
        // only remaining reason to answer no is `dispatch_has_code == false` --
        // there is no concrete bytecode to yield TO, so the native runs exactly
        // as step 1 has always let it. Armed for `java/util/HashMap` it is 0,
        // which is why the wrong name survived its first checks; armed for
        // `all` it is 494 of 19 931. A widening scope cannot widen a leak.
        //
        // The question the wrong name implied -- does some door serve a
        // `Bridge` WITHOUT asking? -- this counter cannot answer, because a
        // door that never calls `note_dial_door` is invisible to it. It is
        // answered statically instead, by the three source-witness tests in
        // `native_override.rs` (`the_unrouted_invoke_or_native_doors_ask_the_
        // enforcement_dial`, `the_warm_invoke_cache_door_asks_the_enforcement_
        // dial`, and the scan that pins the helper's spelling so the other two
        // cannot silently degrade to searching for nothing).
        out.push_str("  },\n  \"enforcement_dial\": {\n");
        out.push_str(&format!(
            "    \"scope\": {},\n",
            json_escape(&crate::runtime::env_cache::enforce_shadow_scope().report_spelling())
        ));
        let doors = crate::vm::dial_door_counts();
        let reached: u64 = doors.iter().map(|(_, r, _)| *r).sum();
        let yielded: u64 = doors.iter().map(|(_, _, y)| *y).sum();
        out.push_str(&format!("    \"reached\": {reached},\n"));
        out.push_str(&format!("    \"yielded\": {yielded},\n"));
        out.push_str(&format!(
            "    \"declined_no_bytecode\": {},\n",
            reached.saturating_sub(yielded)
        ));
        out.push_str("    \"doors\": [");
        for (i, (door, r, y)) in doors.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "\n      {{\"door\": {}, \"reached\": {r}, \"yielded\": {y}}}",
                json_escape(door)
            ));
        }
        out.push_str("\n    ]\n");
        out.push_str("  },\n");
        // W7-95 N8 (`W7-95-intrinsic-semantics-census.md`
        // §"make the instrument able to see this category at all"): this
        // report could say "4301 intrinsic invocations"
        // (`counts.intrinsic_invocations` above) and could name none of them.
        // `NativeKind::Intrinsic` is deliberately exempt from `--real-jdk`'s
        // shadow enforcement (it is "a correct fast path for a hot method",
        // not a shadow to refuse), so nothing else in this report ever
        // mentions an Intrinsic triple — an operator auditing one had to run
        // a SEPARATE `--dump-native-registry` and cross-reference the two
        // files by hand, which is exactly what produced the reproducer at the
        // top of that record.
        //
        // The per-triple invocation count already exists on every census row
        // (`NativeCensusEntry::invocations`, the same field
        // `dump_native_census_json` writes); this array joins it into THIS
        // report rather than duplicating the whole census. A SIBLING object,
        // like `observation_sink` and `enforcement_dial` above: additive, so a
        // reader that does not know about it still parses the file.
        //
        // Filtered to `owns_slot` (a row that lost its slot to a
        // last-write-wins re-registration was never dispatched — counting it
        // here would double-book the triple against whichever row now owns
        // it) and to `invocations > 0` (an unexercised triple is not "named",
        // it is silence; the FULL registered set, exercised or not, is what
        // `--dump-native-registry` is for). `invocations_complete` rides
        // along per row for the same reason schema 5 added it to the census:
        // a floor and a total must not render identically.
        let mut intrinsic_rows: Vec<&cratonvm_native_api::NativeCensusEntry> = census_rows
            .iter()
            .filter(|r| r.kind == NativeKind::Intrinsic && r.owns_slot && r.invocations > 0)
            .collect();
        intrinsic_rows.sort_by(|a, b| {
            (&a.class, &a.name, &a.descriptor).cmp(&(&b.class, &b.name, &b.descriptor))
        });
        out.push_str("  \"intrinsic_invocations_by_triple\": [");
        for (i, r) in intrinsic_rows.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "\n    {{\"class\": {}, \"name\": {}, \"descriptor\": {}, \
                 \"invocations\": {}, \"invocations_complete\": {}}}",
                json_escape(&r.class),
                json_escape(&r.name),
                json_escape(&r.descriptor),
                r.invocations,
                r.invocations_complete
            ));
        }
        if !intrinsic_rows.is_empty() {
            out.push_str("\n  ");
        }
        out.push_str("]\n}\n");

        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        file.write_all(out.as_bytes())?;
        file.sync_all()?;
        Ok((
            violations.len(),
            buckets.map(|b| b.compatibility as usize).unwrap_or(0),
        ))
    }

    /// Write whichever Real-JDK census artefacts were requested, from a thread
    /// that is about to call [`std::process::exit`] and must not hang.
    ///
    /// # Why this exists
    ///
    /// `vm-cli`'s `write_jdk_only_dumps` is called from four exit paths, and
    /// `System.exit(N)` reaches none of them: `native_system_exit` /
    /// `native_runtime_exit` end in `std::process::exit`, which does not unwind
    /// — no `Drop`, no `catch_unwind`, no return to `run()`. So a strict run of
    /// any program that detects a problem and exits (a Spring Boot failure
    /// analyzer, a CLI argument-parse error path) produced **no census at all**,
    /// and that gap was correlated with failure: the runs most likely to need
    /// the report were exactly the runs that left none.
    ///
    /// # Why it is a try-lock and must stay one
    ///
    /// This runs on whichever Java thread called `System.exit`, at an arbitrary
    /// point in that thread's execution, holding whatever locks that thread
    /// already held. All three writers need the class-manager lock, which is an
    /// `OrderedPlRwLock` participating in workspace lock-order enforcement.
    /// A blocking acquire from here is both a deadlock risk and an order
    /// violation — **and a deadlock in the exit path hangs the process instead
    /// of losing a file, which is strictly worse than the bug being fixed.**
    ///
    /// So: [`try_read_untracked`], no retry loop, no timeout, no blocking
    /// fallback. If the lock is not free at this instant, each artefact is
    /// written in its labelled `"partial": true` form rather than not at all
    /// (see the three `_with` writers for what each one degrades to). A census
    /// that is usually complete and never hangs beats one that is sometimes
    /// absent.
    ///
    /// [`try_read_untracked`]: cratonvm_types::lock_order::OrderedPlRwLock::try_read_untracked
    ///
    /// # Cost in `Compatible` mode
    ///
    /// Zero. Every argument is `None` unless the operator passed the matching
    /// `--dump-*` / `--real-jdk-report` flag, and the three-`is_none` guard
    /// returns before touching a lock or the filesystem.
    ///
    /// Returns `(wrote_anything, was_partial)`.
    pub fn try_write_jdk_only_dumps_for_exit(
        &self,
        class_origins: Option<&str>,
        native_registry: Option<&str>,
        report: Option<&str>,
        verbose: bool,
    ) -> (bool, bool) {
        if class_origins.is_none() && native_registry.is_none() && report.is_none() {
            return (false, false);
        }
        let guard = self.classes.class_manager.try_read_untracked();
        let cm = guard.as_deref();
        let partial = cm.is_none();
        let mut wrote = false;
        if let Some(path) = class_origins {
            if self.dump_class_origins_json_with(path, verbose, cm).is_ok() {
                wrote = true;
            }
        }
        if let Some(path) = native_registry {
            if self.dump_native_census_json_with(path, verbose, cm).is_ok() {
                wrote = true;
            }
        }
        if let Some(path) = report {
            if self
                .dump_jdk_only_report_json_with(path, verbose, cm)
                .is_ok()
            {
                wrote = true;
            }
        }
        (wrote, partial)
    }

    /// NEW-10: append a missing-native entry to the audit log,
    /// deduplicating by `(class, method, descriptor)`. The first
    /// occurrence records `sample_call_site`; subsequent calls do not
    /// overwrite it so the baseline remains stable across runs.
    pub fn record_missing_native(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        sample_call_site: Option<String>,
    ) {
        let mut log = self.debug.missing_natives_log.lock();
        let already = log.iter().any(|e| {
            e.class_name == class_name && e.method_name == method_name && e.descriptor == descriptor
        });
        if !already {
            log.push(MissingNativeEntry {
                class_name: class_name.to_string(),
                method_name: method_name.to_string(),
                descriptor: descriptor.to_string(),
                sample_call_site,
            });
        }
    }

    /// T2.1.3: Group the missing-native audit log by JDK module.
    ///
    /// Classification uses the canonical `java.module-info` package ownership
    /// defined in the OpenJDK 25 module descriptors. Every key in the
    /// returned map is a JDK module name (`"java.base"`, `"java.net.http"`,
    /// `"jdk.unsupported"`, …) plus the special bucket `"other"` for
    /// entries whose class prefix does not match any known module.
    ///
    /// Entries are sorted within each group by `(class, name, descriptor)`
    /// so the result is stable across runs and suitable for committing as
    /// a census baseline.
    pub fn classify_missing_natives_by_module(
        &self,
    ) -> std::collections::BTreeMap<&'static str, Vec<MissingNativeEntry>> {
        let log = self.debug.missing_natives_log.lock();
        let mut grouped: std::collections::BTreeMap<&'static str, Vec<MissingNativeEntry>> =
            std::collections::BTreeMap::new();
        for entry in log.iter() {
            let module = classify_jdk_module(&entry.class_name);
            grouped.entry(module).or_default().push(entry.clone());
        }
        for v in grouped.values_mut() {
            v.sort_by(|a, b| {
                (&a.class_name, &a.method_name, &a.descriptor).cmp(&(
                    &b.class_name,
                    &b.method_name,
                    &b.descriptor,
                ))
            });
            v.dedup_by(|a, b| {
                a.class_name == b.class_name
                    && a.method_name == b.method_name
                    && a.descriptor == b.descriptor
            });
        }
        grouped
    }

    /// T2.1.3: Write the grouped census to `path` as a JSON object with
    /// one top-level key per JDK module. Schema:
    ///
    /// ```json
    /// {
    ///   "version": 1,
    ///   "modules": {
    ///     "java.base": [
    ///       { "class": "java/lang/Foo", "name": "bar",
    ///         "descriptor": "(I)V", "sample_call_site": null }
    ///     ],
    ///     "other": []
    ///   }
    /// }
    /// ```
    ///
    /// Module keys and entries within each module are sorted so the file
    /// is byte-stable and diff-friendly.
    pub fn dump_missing_natives_grouped_json(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> std::io::Result<()> {
        let grouped = self.classify_missing_natives_by_module();

        let mut out = String::with_capacity(512);
        out.push_str("{\n  \"version\": 1,\n  \"modules\": {");
        let mut first_module = true;
        for (module, entries) in &grouped {
            if !first_module {
                out.push(',');
            }
            first_module = false;
            out.push_str("\n    ");
            out.push_str(&json_escape(module));
            out.push_str(": [");
            for (i, entry) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str("\n      {\n");
                out.push_str(&format!(
                    "        \"class\": {},\n",
                    json_escape(&entry.class_name)
                ));
                out.push_str(&format!(
                    "        \"name\": {},\n",
                    json_escape(&entry.method_name)
                ));
                out.push_str(&format!(
                    "        \"descriptor\": {},\n",
                    json_escape(&entry.descriptor)
                ));
                match &entry.sample_call_site {
                    Some(site) => out.push_str(&format!(
                        "        \"sample_call_site\": {}\n",
                        json_escape(site)
                    )),
                    None => out.push_str("        \"sample_call_site\": null\n"),
                }
                out.push_str("      }");
            }
            if !entries.is_empty() {
                out.push_str("\n    ");
            }
            out.push(']');
        }
        if !grouped.is_empty() {
            out.push_str("\n  ");
        }
        out.push_str("}\n}\n");

        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        file.write_all(out.as_bytes())?;
        file.sync_all()?;
        Ok(())
    }
}

/// T2.1.3: Return the canonical JDK module name that owns a given class
/// (by binary name, e.g. `"java/net/http/HttpClient"`).
///
/// The mapping is a longest-prefix match against the OpenJDK 25 module
/// table. Unknown classes fall into the `"other"` bucket so they remain
/// visible in the census without triggering false module claims.
///
/// The returned slice is static — this function allocates nothing and is
/// safe to call from any context including `no_std` tests.
pub fn classify_jdk_module(class_name: &str) -> &'static str {
    // The list is ordered longest-prefix first. Any time a new JDK module
    // grows a package CratonVM needs to track, add it ABOVE its parent
    // module (e.g. `"java/net/http"` must appear before `"java/net"`).
    //
    // Prefixes use the JVMS binary form with `/` separators and always
    // end with `/` so `"java/netX"` does not match `"java/net"`.
    const PREFIX_TO_MODULE: &[(&str, &str)] = &[
        // Named packages that live in modules other than java.base.
        ("java/net/http/", "java.net.http"),
        ("jdk/internal/net/http/", "java.net.http"),
        ("java/sql/", "java.sql"),
        ("javax/sql/", "java.sql"),
        ("java/rmi/", "java.rmi"),
        ("javax/management/", "java.management"),
        ("java/lang/management/", "java.management"),
        ("javax/naming/", "java.naming"),
        ("javax/xml/", "java.xml"),
        ("org/w3c/dom/", "java.xml"),
        ("org/xml/sax/", "java.xml"),
        ("javax/crypto/", "java.base"), // javax.crypto is in java.base in JDK 25
        ("javax/security/", "java.base"),
        ("javax/net/ssl/", "java.base"),
        ("javax/net/", "java.base"),
        ("java/util/logging/", "java.logging"),
        ("java/util/prefs/", "java.prefs"),
        ("java/lang/instrument/", "java.instrument"),
        ("jdk/jfr/", "jdk.jfr"),
        ("jdk/management/jfr/", "jdk.management.jfr"),
        ("com/sun/management/", "jdk.management"),
        ("jdk/management/", "jdk.management"),
        ("jdk/jdi/", "jdk.jdi"),
        ("com/sun/jdi/", "jdk.jdi"),
        ("jdk/jshell/", "jdk.jshell"),
        ("jdk/compiler/", "jdk.compiler"),
        ("javax/tools/", "java.compiler"),
        ("jdk/javadoc/", "jdk.javadoc"),
        ("jdk/unsupported/", "jdk.unsupported"),
        ("sun/misc/", "jdk.unsupported"),
        // java.base (catch-all for the huge standard tree).
        ("java/lang/", "java.base"),
        ("java/util/", "java.base"),
        ("java/io/", "java.base"),
        ("java/nio/", "java.base"),
        ("java/net/", "java.base"),
        ("java/security/", "java.base"),
        ("java/math/", "java.base"),
        ("java/text/", "java.base"),
        ("java/time/", "java.base"),
        ("jdk/internal/", "java.base"),
        ("sun/nio/", "java.base"),
        ("sun/security/", "java.base"),
        ("sun/reflect/", "java.base"),
        ("sun/util/", "java.base"),
    ];

    for (prefix, module) in PREFIX_TO_MODULE {
        if class_name.starts_with(prefix) {
            return module;
        }
    }
    "other"
}

/// WP1.8 — ServiceLoader classpath bootstrap.
///
/// Registers `java.util.ServiceLoader` natives on `registry` and seeds any
/// JVM-side bookkeeping needed to make classpath `META-INF/services/*`
/// provider scans resolvable from `ServiceLoader.load(Class)`. This is the
/// WildFly-friendly entry point: classpath-scoped today, module-scoped in
/// Wave 2 (the roadmap re-extends WP1.8 with JBoss-Module resource-root
/// awareness, at which point this function gains a second scan source).
///
/// Today all the heavy lifting happens inside the native registration
/// (see [`cratonvm_native_builtins::service_loader::register_service_loader_natives`])
/// which bridges into `ClassLoader.getResources("META-INF/services/<fqcn>")`
/// and instantiates each listed provider via reflection. We keep this
/// entry point even though the body is minimal so that:
///
/// 1. Module-scoped (Wave 2) extensions land in a single, already-wired
///    location without having to touch the `SharedVm::new` hot path again.
/// 2. Callers outside `SharedVm::new` (e.g. VM embeddings that build
///    their own `NativeMethodRegistry` before constructing `SharedVm`)
///    have a single public name to call.
/// 3. Tests can exercise the bootstrap independently of the full VM
///    constructor.
///
/// Coordination note (WP1.8): this function is deliberately kept outside
/// `SharedVm::new`'s `set_init_level` region (which is owned by Wave 1
/// Owner B). It is called from both the synthetic-jdk and real-jdk
/// branches alongside the existing `register_service_loader_natives`
/// call — see the call sites immediately below that comment.
pub fn init_service_loader_bootstrap(registry: &mut NativeMethodRegistry) {
    cratonvm_native_builtins::service_loader::register_service_loader_natives(registry);
    tracing::info!(
        "WP1.8: ServiceLoader bootstrap wired — META-INF/services classpath scan enabled"
    );
}

/// The `schema_version` [`SharedVm::dump_native_census_json`] stamps on every
/// native census it writes.
///
/// **5** since 2026-08-17 (`G47-1`): rows gained `invocations_complete` and the
/// header gained `slots_with_incomplete_invocations`. The bump is not cosmetic
/// and the reason is the same one schema 4 was bumped for. A schema-4 reader
/// scoring a schema-5 file is harmless (it ignores two keys); a reader that
/// believes it is looking at schema 5 and is handed a schema-4 file concludes
/// **every row is a total**, because the absent key reads as "nothing declared
/// itself a bypass" — which is the exact direction
/// `NativeMethodRegistry::mark_invocations_incomplete` says this instrument
/// must never err in. One `schema_version` with two shapes is the hazard
/// `dump_native_census_json`'s "only native-census writer" note is about.
///
/// A named constant rather than a literal because the writer, this file's doc
/// example and the witness test below must not be able to drift apart — the
/// state `G37-1` §6 N2 and `G42-1` §6 N1 measured, where two records disagreed
/// about whether the shipping binary said 3 or 4 and `--help` said a third
/// thing.
///
/// **Consumers that pin this exactly** (equality, not `>=`), and therefore move
/// with it: `scripts/jdk-only-bridge-ratchet.py`'s `REQUIRED_CENSUS_SCHEMA` and
/// the `census_schema_version` recorded in
/// `scripts/baselines/jdk-only-bridge-ratchet.json`. Neither is in this crate;
/// see `docs/known-issues/jdk-only/G47-1-*.md` NOMINATION 1.
/// `scripts/jdk-only-kind-map.py` asks for `>= 2` and needs nothing.
pub(crate) const NATIVE_CENSUS_SCHEMA_VERSION: u32 = 5;

/// The `invocations` pair of a census row: the tally, and whether it is a
/// **total** or a **floor**.
///
/// The two are emitted together, in this order, by one function on purpose.
/// The whole defect `G33-1`/`G37-1`/`G42-1` chased is that `invocations` was
/// readable without its qualifier: 25 slots carried
/// `NativeCensusEntry::invocations_complete` `false` and **no reader could
/// see it**, so every consumer of the column read a floor as a count. Emitting
/// the number from a function that cannot emit it without the bit is the cheap
/// structural way to keep that from recurring; a future editor who wants one
/// has to delete the other deliberately.
///
/// `false` here means "a dispatch path has *declared* that it serves this slot
/// without counting" — a claim carried by code, not a proof. `true` means no
/// path has declared itself, which is weaker than "exact": see
/// `NativeMethodRegistry::record_invocation`'s bypass list.
pub(in crate::vm::vm_init) fn native_census_invocations_json(invocations: u64, complete: bool) -> String {
    format!(
        "      \"invocations\": {},\n      \"invocations_complete\": {},\n",
        invocations, complete
    )
}

/// The census header's one-line summary of how much of the `invocations`
/// column is a floor:
/// `NativeMethodRegistry::slots_with_incomplete_invocations`.
///
/// In the header rather than only per row so a reader is told the column is
/// partly a floor **before** quoting a number out of it, which is the order the
/// mistake actually happens in — `G33-1` §4 records a lane concluding a body
/// was dead from `invocations: 0`, and `G42-1` §4 shows zero is the *expected*
/// reading for a hot native whose loop began after its caller was compiled.
///
/// Counted over **slots**, while `natives` is one row per **registration**, so
/// this number is not the count of rows carrying `invocations_complete: false`
/// — a superseded row shares its successor's slot and shows the same bit. That
/// asymmetry is inherited from `counts` (registrations) versus `invocations`
/// (slots) and is deliberate in both.
pub(in crate::vm::vm_init) fn native_census_incomplete_header_json(slots: usize) -> String {
    format!("  \"slots_with_incomplete_invocations\": {},\n", slots)
}

/// j27 CB17-6: the census's `table_intrinsics` block -- one entry per row of
/// the interpreter's intrinsic table (`cratonvm_native_builtins::intrinsics`),
/// which serves bodies a cached interpreted call site reaches without the
/// registry. `registry_kind` is the kind of the registry row that owns the
/// same triple's slot, `null` when none does (the three `String` rows on a
/// real JDK: no other artefact showed them). `declined_by_strict_policy` is
/// `strict_policy_declines` for this run's mode. Additive: no consumer
/// pinned to schema 5 reads the key.
pub(in crate::vm::vm_init) fn native_census_table_intrinsics_json(
    rows: &[cratonvm_native_api::NativeCensusEntry],
    jdk_only: bool,
) -> String {
    use cratonvm_native_builtins::intrinsics as table;
    let entries: Vec<String> = table::TABLE
        .iter()
        .map(|&(class, name, desc, is_static)| {
            let declined = table::lookup(class, name, desc)
                .is_some_and(|kind| table::strict_policy_declines(kind, jdk_only));
            let owner = rows.iter().find(|r| {
                r.owns_slot && r.class == class && r.name == name && r.descriptor == desc
            });
            format!(
                "\n    {{\"class\": {}, \"name\": {}, \"descriptor\": {}, \"static\": {}, \
                 \"declined_by_strict_policy\": {}, \"registry_kind\": {}}}",
                json_escape(class),
                json_escape(name),
                json_escape(desc),
                is_static,
                declined,
                owner.map_or_else(|| "null".to_string(), |r| json_escape(r.kind.as_str())),
            )
        })
        .collect();
    let tail = if entries.is_empty() { "" } else { "\n  " };
    format!("  \"table_intrinsics\": [{}{}],\n", entries.join(","), tail)
}

/// Escape an arbitrary UTF-8 string as a JSON string literal, including
/// the surrounding double-quotes. Handles the six characters that must
/// be escaped per RFC 8259 (`\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`)
/// plus any C0 control code via `\u00XX`. Used by
/// [`SharedVm::dump_missing_natives_json`] so the file can be diffed
/// safely against a committed baseline.
pub(in crate::vm::vm_init) fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Every `ClassOrigin::as_str()` tag, in the enum's own declaration order.
///
/// The class-origin census seeds its `counts` block from this list so a tag
/// that never occurred reports `0` rather than being absent. `"compatibility-
/// stub": 0` is the single most load-bearing line in the whole artefact, and a
/// missing key would let "we did not observe any" and "we did not look" render
/// identically (real-jdk-mode.md §9, §11).
///
/// Kept in sync by hand with
/// [`ClassOrigin::as_str`](cratonvm_classloading::class_origin::ClassOrigin::as_str)
/// and with `vm-cli`'s `CLASS_ORIGIN_TAGS`; a tag added there and forgotten
/// here shows up as an unseeded (but still counted) key, never as a lost row.
pub(in crate::vm::vm_init) const CLASS_ORIGIN_TAGS: [&str; 10] = [
    "boot-image",
    "application-class-path",
    "user-defined",
    "vm-array",
    "hidden-class",
    "generated-lambda",
    "generated-proxy",
    "reflection-accessor",
    "vm-internal",
    "compatibility-stub",
];

/// Redact a native registration site for the schema-2 census
/// (`real-jdk-mode.md` §9: "absolute paths are redacted unless
/// `--explain-jdk-only` is passed").
///
/// Provenance is captured with `#[track_caller]`, so
/// [`core::panic::Location::file`] normally yields a **workspace-relative**
/// path — `native-builtins/src/lib.rs:1234` — which is exactly what a reviewer
/// needs and contains nothing private. Those are kept verbatim (modulo `\` →
/// `/`, so a census taken on Windows diffs against one taken on Linux).
///
/// An **absolute** path is a different animal: it appears for code compiled
/// out of a registry checkout or with `--remap-path-prefix` absent, and it
/// carries the builder's home directory into a file that gets committed as a
/// baseline and pasted into issues. Those collapse to `<redacted>/<file:line>`
/// — the basename plus line is still enough to find the registration, and the
/// output no longer varies per machine, which also keeps the baseline
/// diff-stable.
///
/// Windows drive letters are handled before any `:`-splitting: the path
/// separator is located first, so `C:/…/lib.rs:12` is not mistaken for a
/// `host:port`-shaped string.
pub(crate) fn redact_registration_site(site: &str) -> String {
    let normalised = site.replace('\\', "/");
    let bytes = normalised.as_bytes();
    let is_absolute = normalised.starts_with('/')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':');
    if !is_absolute {
        return normalised;
    }
    match normalised.rsplit_once('/') {
        Some((_, tail)) if !tail.is_empty() => format!("<redacted>/{tail}"),
        _ => "<redacted>".to_string(),
    }
}

/// The JDK feature version of the runtime image rooted at `java_home`, read
/// from its `release` file's `JAVA_VERSION=` line.
///
/// Returns `None` rather than guessing. `jdk_feature` is used to interpret a
/// violation ("does this JDK even declare that method?") and to key the
/// per-image blocker artifacts, so a fabricated number is worse than an absent
/// one — `null` reads as "not measured", a wrong `25` reads as a fact.
///
/// `"25"`, `"25.0.1"`, `"21.0.4+7-LTS"` → the feature; legacy `"1.8.0_402"`
/// → `8`.
pub fn jdk_feature_from_release_file(java_home: &str) -> Option<u32> {
    let text = std::fs::read_to_string(std::path::Path::new(java_home).join("release")).ok()?;
    let raw = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("JAVA_VERSION="))?;
    let value = raw.trim().trim_matches('"');
    let mut parts = value.split(['.', '-', '+', '_']);
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

/// The `java.vm.info` execution-mode list for `mode` — and the same list the
/// launcher's `-version` banner prints (`real-jdk-mode.md` §9).
///
/// ONE function, called from both places, because the property and the banner
/// used to spell the policy two different ways (`"mixed mode, jdk-only"` vs
/// `compatibility=jdk-only`) and nothing kept them in step. They still use two
/// *conventions*, deliberately, and both are load-bearing:
///
/// * **`java.vm.info` is HotSpot's convention, verbatim**: a comma-separated
///   list of execution-mode tokens (`"mixed mode"`, `"mixed mode, sharing"`),
///   read by Java code and printed by tools straight after the VM name. It
///   cannot carry a `key=value` token without breaking that shape, and under
///   `Compatible` it must stay byte-for-byte `"mixed mode"` because the
///   differential harness compares it literally (§10). Strict mode therefore
///   *appends a token* instead of restructuring the value.
/// * **The banner adds `compatibility=<mode>`** on top of this same list,
///   because a banner is grepped by scripts that must not have to parse a
///   comma list, and because `-version` prints it *before any VM exists*, so
///   it cannot read the property it has to agree with.
///
/// What must agree is the spelling and the position of the policy token: both
/// say `jdk-only`, both put it immediately after `mixed mode`.
pub fn vm_info_mode_list(mode: CompatibilityMode) -> &'static str {
    vm_info_execution_mode_list(mode, false)
}

/// [`vm_info_mode_list`] with HotSpot's execution-mode word chosen by
/// `interpreted` (interpreter round i7 wave 138, lane L1b; page
/// `i137-L1b-xint-is-dropped-and-java-vm-info-never-says-interpreted`):
/// `"interpreted mode"` for a `-Xint` launch whose JIT is off, as HotSpot
/// 25's `Arguments::_mode == _int` answers (`VM_Version::vm_info_string`),
/// `"mixed mode"` otherwise. CratonVM has no CDS archive, so never
/// `, sharing` (gci i33/ms). The `--real-jdk` token is appended in both
/// cases, unchanged (real-jdk-mode.md §10; whether it stays is the user's
/// decision, recorded on the page). `-Xcomp` has no CratonVM equivalent and
/// answers `"mixed mode"`, the mode it actually runs in.
pub fn vm_info_execution_mode_list(mode: CompatibilityMode, interpreted: bool) -> &'static str {
    match (interpreted, mode.is_jdk_only()) {
        (true, true) => "interpreted mode, jdk-only",
        (true, false) => "interpreted mode",
        (false, true) => "mixed mode, jdk-only",
        (false, false) => "mixed mode",
    }
}

/// The HotSpot execution-mode option that wins in `args` (the last of
/// `-Xint` / `-Xmixed` / `-Xcomp`, as `Arguments::parse_each_vm_init_arg`
/// sets `_mode` once per occurrence), or `None` when none is given. `args`
/// is HotSpot's `jvm_args()` list (`VmConfig::jvm_args`) or the launcher's
/// option section. Pure.
pub fn hotspot_execution_mode_option<'a, I>(args: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a String>,
{
    args.into_iter()
        .map(String::as_str)
        .filter(|a| matches!(*a, "-Xint" | "-Xmixed" | "-Xcomp"))
        .last()
}

/// Does `java.vm.info` say `interpreted mode` for this VM? Only when the
/// options ask for `-Xint` (last one wins) AND the JIT is really off
/// (`VmConfig::jit_enabled`): the launcher turns `-Xint` into the `--nojit`
/// configuration (`CRATONVM_LAUNCHER_XINT`, default ON, `=0` kill), so a
/// killed switch, or an embedder that lists `-Xint` without disabling the
/// JIT, keeps the honest `mixed mode`. `--nojit` alone (not a HotSpot
/// option) keeps `mixed mode` too, so `--nojit` battery arms still compare
/// equal to HotSpot's default run.
pub(crate) fn vm_info_interpreted(config: &VmConfig) -> bool {
    !config.jit_enabled && hotspot_execution_mode_option(&config.jvm_args) == Some("-Xint")
}

/// The VM properties `SharedVm::new` seeds that describe this VM itself, as
/// HotSpot's `Arguments` lists them, with their initial values: also what
/// the C JVMTI table answers for them in `Agent_OnLoad`, before the property
/// map exists (interpreter round i2 wave 61, lane L1;
/// `jvmti::native_env::onload_vm_properties`). One copy of each value.
/// `CRATONVM_VM_VERSION_FROM_IMAGE` (wave 138, lane L1b; parity, default ON,
/// `=0` off): `java.vm.version` is the image's `JAVA_RUNTIME_VERSION`
/// ([`vm_identity_properties`]). Read once per property seeding.
pub(in crate::vm::vm_init) fn vm_version_from_image_enabled() -> bool {
    true
}

pub(crate) fn vm_identity_properties(config: &VmConfig) -> [(&'static str, String); 8] {
    let mode = config.compatibility_mode;
    // Interpreter round i6 wave 118, lane L1 (i61-L1 stage 2): the fixed
    // values are `cratonvm_types::vm_properties::VM_IDENTITY`'s, the one copy
    // `native-builtins` reads too; `java.vm.info` is the launch's.
    let [spec_name, spec_vendor, spec_version, name, vendor, version, debug] =
        cratonvm_types::vm_properties::VM_IDENTITY;
    let fixed = |(key, value): (&'static str, &'static str)| (key, value.to_string());
    // zt t27/lg1 (`CRATONVM_VM_IDENTITY_HOTSPOT`, default ON, `=0` kill; host
    // row `pl8_census_J`: `java.property.java.vm.name` read `cratonvm` against
    // HotSpot's `OpenJDK 64-Bit Server VM`): HotSpot 25's identity of the
    // image it runs -- `java.vm.name` its 64-bit server VM's name
    // (`vm_identity_value`), `java.vm.vendor` the build's vendor (the image's
    // `release` `IMPLEMENTOR`, HotSpot's `VENDOR`) and `java.vm.version` the
    // build's version string (`JAVA_RUNTIME_VERSION`, HotSpot's
    // `VM_Version::vm_release()`, the same string as `java.runtime.version`).
    // Without an image (a synthetic JDK) the vendor and version keep their
    // constants. `=0`: `cratonvm`, `cratonvm`, `25.0.1+8`, as before.
    let (name, vendor, version) = if cratonvm_types::vm_properties::vm_identity_hotspot_enabled() {
        let java_home = initial_java_home(config);
        let release = |key: &str| jdk_image_release_value(&java_home, key);
        (
            (
                name.0,
                cratonvm_types::vm_properties::vm_identity_value(name.0).unwrap_or(name.1),
            ),
            (vendor.0, release("IMPLEMENTOR")),
            (
                version.0,
                // Interpreter round i7 wave 138, lane L1b reached the same
                // `java.vm.version` (page `i138-orch-java-vm-version-is-the-25-0-1-constant`);
                // its `CRATONVM_VM_VERSION_FROM_IMAGE=0` keeps the constant.
                vm_version_from_image_enabled()
                    .then(|| release("JAVA_RUNTIME_VERSION").or_else(|| release("JAVA_VERSION")))
                    .flatten(),
            ),
        )
    } else {
        (name, (vendor.0, None), (version.0, None))
    };
    [
        fixed(spec_name),
        fixed(spec_vendor),
        fixed(spec_version),
        fixed(name),
        (vendor.0, vendor.1.unwrap_or_else(|| VM_IDENTITY_VENDOR_FALLBACK.to_string())),
        (version.0, version.1.unwrap_or_else(|| VM_IDENTITY_VERSION_FALLBACK.to_string())),
        (
            "java.vm.info",
            vm_info_execution_mode_list(mode, vm_info_interpreted(config)).to_string(),
        ),
        fixed(debug),
    ]
}

/// zt t27/lg1: the `java.vm.vendor` / `java.vm.version` constants of
/// `VM_IDENTITY` (an image without a `release` value, and the `=0` arm of
/// `CRATONVM_VM_IDENTITY_HOTSPOT`).
pub(in crate::vm::vm_init) const VM_IDENTITY_VENDOR_FALLBACK: &str = cratonvm_types::vm_properties::VM_IDENTITY[4].1;
pub(in crate::vm::vm_init) const VM_IDENTITY_VERSION_FALLBACK: &str = cratonvm_types::vm_properties::VM_IDENTITY[5].1;

/// The VM's own property list, `cratonvm_types::vm_properties::VM_PROPERTIES`
/// with its values, in the list's order (interpreter round i6 wave 122, lane
/// L1; stage 3 of
/// `docs/feature-designs/real-jdk-mode.md`):
/// the identity keys ([`vm_identity_properties`]: the launch-independent
/// ones from `VM_IDENTITY` only, `java.vm.info` the launch's), the four paths
/// (`java_home` is [`initial_java_home`]'s answer, which the caller already
/// holds), and `java.vm.compressedOopsMode` when HotSpot would set it
/// ([`vm_compressed_oops_mode`], wave 136). `SharedVm::new` puts these into
/// its `sys_props` as one block, apart from the platform's keys; it has no
/// literal of its own for any of them.
pub(crate) fn vm_property_list(
    config: &VmConfig,
    java_home: &str,
) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> =
        Vec::with_capacity(cratonvm_types::vm_properties::VM_PROPERTIES.len());
    out.extend(vm_identity_properties(config));
    out.push(("java.home", java_home.to_string()));
    out.push(("java.class.path", initial_java_class_path(config)));
    out.push(("java.library.path", initial_java_library_path(java_home)));
    out.push((
        "sun.boot.library.path",
        initial_sun_boot_library_path(java_home),
    ));
    if let Some(mode) = vm_compressed_oops_mode(config) {
        out.push(("java.vm.compressedOopsMode", mode.to_string()));
    }
    out
}

/// `java.vm.compressedOopsMode`'s value, or `None` when HotSpot would set no
/// such property (interpreter round i7 wave 136, lane L1b;
/// `docs/feature-designs/real-jdk-mode.md`).
/// With `VmConfig::vm_compressed_oops_mode_hotspot`
/// (`CRATONVM_VM_COMPRESSED_OOPS_MODE_HOTSPOT`, parity, default ON, `=0` the
/// old constant `Zero based`): when this VM really runs with narrow oops
/// (admitted earlier in `SharedVm::new`), the mode of their base and shift --
/// what JFR's `jdk.GCHeapConfiguration` answers then; otherwise HotSpot's
/// answer for this command line's `MaxHeapSize`, collector and
/// `-XX:±UseCompressedOops` (`hotspot_compressed_oops_mode`): absent under
/// ZGC (this build's default collector), `-XX:-UseCompressedOops`, or a heap
/// past the compressed-oops limit.
pub(crate) fn vm_compressed_oops_mode(config: &VmConfig) -> Option<&'static str> {
    use cratonvm_types::vm_properties::{
        hotspot_compressed_oops_mode, hotspot_narrow_oop_mode_name,
        use_compressed_oops_from_jvm_args, CompressedOopsCollector,
    };
    if !config.vm_compressed_oops_mode_hotspot {
        return Some("Zero based");
    }
    if cratonvm_types::narrow_oop::narrow_oops_enabled() {
        return Some(hotspot_narrow_oop_mode_name(
            cratonvm_types::narrow_oop::narrow_base(),
            cratonvm_types::narrow_oop::narrow_shift(),
        ));
    }
    let collector = match config.gc_algorithm {
        crate::config::GcAlgorithm::Generational => CompressedOopsCollector::Serial,
        crate::config::GcAlgorithm::G1 => CompressedOopsCollector::G1,
        #[cfg(feature = "zgc")]
        crate::config::GcAlgorithm::Zgc => CompressedOopsCollector::Z,
    };
    hotspot_compressed_oops_mode(
        config.max_heap_size as u64,
        collector,
        use_compressed_oops_from_jvm_args(&config.jvm_args),
    )
}

/// `java.home`'s initial value (wave 61: shared with the C JVMTI table's
/// OnLoad answer): the same resolution order as boot classpath discovery
/// (`resolve_java_home_public`): explicit config, CRATONVM_JAVA_HOME,
/// JAVA_HOME, then PATH.
pub(crate) fn initial_java_home(config: &VmConfig) -> String {
    crate::config::resolve_java_home_public(config.java_home.as_deref())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string())
}

/// `java.class.path`'s initial value (wave 61: shared with the C JVMTI
/// table's OnLoad answer). The HotSpot contract:
///   * `-jar <FILE>` launch  ->  `java.class.path = <FILE>` (the bare
///     user-supplied jar path, NOT the manifest-expanded transitive
///     classpath). Liberty/Quarkus boot launchers do
///     `new JarFile(new File(System.getProperty("java.class.path")))` and
///     then `.getManifest().getMainAttributes()` — if we joined all expanded
///     entries here, that File wouldn't exist as a jar and `getManifest()`
///     would return null, producing the "Cannot invoke getMainAttributes on
///     null" NPE seen in 21 wlp tool jars (ws-schemagen, ws-wsimport,
///     ws-featureUtility, ...). The full transitive classpath is still wired
///     into the class loader via `config.classpath`, so resolution of
///     manifest `Class-Path:` siblings is unaffected.
///   * `-classpath <CP>` / `<class>` launch  ->  joined cp string.
///   * No cp configured  ->  fall back to $CLASSPATH env, then "".
pub(crate) fn initial_java_class_path(config: &VmConfig) -> String {
    if let Some(jar) = config.launcher_jar.as_deref() {
        jar.to_string()
    } else if !config.classpath.is_empty() {
        let sep = if cfg!(windows) { ";" } else { ":" };
        // The Java launcher expands `dir/*` before publishing this
        // property. Keep it aligned with the already-expanded loader
        // view so javax.tools/javac can discover wildcard JARs from its
        // default classpath (notably H2's dynamic CREATE ALIAS compiler).
        crate::classloading::ClassPath::expand_classpath_entries(&config.classpath).join(sep)
    } else {
        cratonvm_types::flags::runtime_var("CLASSPATH").unwrap_or_default()
    }
}

/// `java.library.path`'s initial value (wave 61: shared with the C JVMTI
/// table's OnLoad answer), as HotSpot's `os::init_system_properties_values`
/// builds it (interpreter round i2 wave 63, lane L1; MEASURED on HotSpot
/// 25.0.3 for Windows, where `<java.home>/lib` led the list here and an agent
/// saw a different value from `System.getProperty`):
///   * Windows: `<java.home>\bin;<windows>\Sun\Java\bin;<windows>\system32;<windows>;<PATH>;.`
///     (HotSpot's first entry is the launcher's directory, `<java.home>\bin`);
///   * Linux: `<LD_LIBRARY_PATH>:/usr/java/packages/lib:/usr/lib64:/lib64:/lib:/usr/lib`
///     (the leading part only when the variable is set);
///   * macOS keeps `<java.home>/lib` and `DYLD_LIBRARY_PATH`.
pub(crate) fn initial_java_library_path(java_home_val: &str) -> String {
    let var = |name: &str| {
        cratonvm_types::flags::runtime_var(name)
            .ok()
            .filter(|v| !v.is_empty())
    };
    if cfg!(windows) {
        let windows = var("SystemRoot")
            .or_else(|| var("windir"))
            .unwrap_or_else(|| "C:\\WINDOWS".to_string());
        let mut out =
            format!("{java_home_val}\\bin;{windows}\\Sun\\Java\\bin;{windows}\\system32;{windows}");
        if let Some(path) = var("PATH") {
            out.push(';');
            out.push_str(&path);
        }
        out.push_str(";.");
        out
    } else if cfg!(target_os = "macos") {
        match var("DYLD_LIBRARY_PATH") {
            Some(env) => format!("{java_home_val}/lib:{env}"),
            None => format!("{java_home_val}/lib"),
        }
    } else {
        const DEFAULT: &str = "/usr/java/packages/lib:/usr/lib64:/lib64:/lib:/usr/lib";
        match var("LD_LIBRARY_PATH") {
            Some(env) => format!("{env}:{DEFAULT}"),
            None => DEFAULT.to_string(),
        }
    }
}

/// The value a command-line `-D<key>=<value>` gives the VM's property (shared
/// with the C JVMTI table's OnLoad answer): the value itself, except that
/// HotSpot appends an explicit `-Dsun.boot.library.path` to the default
/// (`Arguments::add_property`; MEASURED on 25.0.3 for Windows:
/// `<java.home>\bin;<value>`), so the JDK's own natives still load.
pub(crate) fn command_line_property_value(java_home_val: &str, key: &str, value: &str) -> String {
    if key == "sun.boot.library.path" {
        let sep = if cfg!(windows) { ';' } else { ':' };
        format!(
            "{}{sep}{value}",
            initial_sun_boot_library_path(java_home_val)
        )
    } else {
        value.to_string()
    }
}

/// `sun.boot.library.path`'s initial value (wave 61: shared with the C
/// JVMTI table's OnLoad answer). SBR2 wave 9 (modload): HotSpot's Windows
/// value is `<java.home>\bin` (the JDK's DLLs live there; `lib` holds none).
/// The real `NativeLibraries.loadLibrary("jimage")` searches exactly this
/// property, so `lib` made every `jrt:` reader die with
/// `UnsatisfiedLinkError: no jimage in system library path` on Windows.
/// Linux/macOS keep `<java.home>/lib`, where the libraries are.
pub(crate) fn initial_sun_boot_library_path(java_home_val: &str) -> String {
    if cfg!(windows) {
        format!("{}\\bin", java_home_val)
    } else {
        format!("{}/lib", java_home_val)
    }
}

/// Replace absolute filesystem paths embedded in `text` with
/// `<redacted>/<basename>`.
///
/// Contract §9: "absolute paths are redacted unless `--explain-jdk-only` is
/// passed". Distinct from [`redact_registration_site`], which redacts a string
/// that *is* a path; this one hunts paths inside prose — a
/// `ClassOrigin::CompatibilityStub` reason, a `requested_by` attribution, a
/// rendered `JdkOnlyViolation` body. A census file is routinely attached to a
/// bug report; a build-agent home directory or a developer's user name is not
/// information the report needs, but the file *name* usually is.
///
/// Lives here rather than in `vm-cli` because the census writers moved into
/// this file and the launcher still needs the same function for its
/// `--trace-jdk-only` lines; two copies of a redaction rule is how a redaction
/// rule stops being applied. Deliberately regex-free (no dependency) and
/// deliberately conservative:
///
/// * only **rooted** runs are candidates — a leading `/`, or a `C:\` / `C:/`
///   drive prefix. Internal-form class names and method descriptors
///   (`java/lang/String`, `(Ljava/lang/Object;)V`) are not rooted and are left
///   alone, which matters because every violation body is full of them;
/// * a candidate needs at least **two** separators before it is rewritten, so
///   `/tmp` and a bare `/` survive while `/home/user/app.jar` does not;
/// * the run is applied to already-escaped JSON, where a Windows separator
///   appears as `\\`; consecutive separators count once.
pub fn redact_absolute_paths(text: &str) -> String {
    let b = text.as_bytes();
    // Characters that may precede a path: anything that is not part of a word.
    // This is what keeps `Ljava/lang/String;` (preceded by `L`) out.
    let boundary = |c: u8| {
        matches!(
            c,
            b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'(' | b'[' | b'{' | b'=' | b',' | b'<'
        )
    };
    // Characters that end a run.
    let terminator = |c: u8| {
        matches!(
            c,
            b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b')' | b']' | b'}' | b',' | b';' | b'>'
        )
    };

    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < b.len() {
        let at_boundary = i == 0 || boundary(b[i - 1]);
        let rooted_unix = b[i] == b'/';
        let rooted_windows = i + 2 < b.len()
            && b[i].is_ascii_alphabetic()
            && b[i + 1] == b':'
            && (b[i + 2] == b'\\' || b[i + 2] == b'/');
        if at_boundary && (rooted_unix || rooted_windows) {
            let mut end = i;
            while end < b.len() && !terminator(b[end]) {
                end += 1;
            }
            // Trailing sentence punctuation belongs to the prose, not the path.
            while end > i && matches!(b[end - 1], b'.' | b':') {
                end -= 1;
            }
            if let Some(redacted) = redact_one_path(&text[i..end]) {
                out.push_str(&redacted);
                i = end;
                continue;
            }
        }
        // Not a path: copy one whole UTF-8 character.
        let step = utf8_char_len(b[i]);
        let step = step.min(b.len() - i);
        out.push_str(&text[i..i + step]);
        i += step;
    }
    out
}

/// Byte length of the UTF-8 character starting with `first`.
pub(in crate::vm::vm_init) fn utf8_char_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// `<redacted>/<basename>` for a rooted run with >= 2 separators; `None` when
/// the run is too shallow to be worth hiding.
pub(in crate::vm::vm_init) fn redact_one_path(run: &str) -> Option<String> {
    let mut separators = 0usize;
    let mut prev_was_separator = false;
    let mut basename = "";
    let mut segment_start = 0usize;
    for (idx, ch) in run.char_indices() {
        if ch == '/' || ch == '\\' {
            if !prev_was_separator {
                separators += 1;
                if idx > segment_start {
                    basename = &run[segment_start..idx];
                }
            }
            prev_was_separator = true;
            segment_start = idx + ch.len_utf8();
        } else {
            prev_was_separator = false;
        }
    }
    if segment_start < run.len() {
        basename = &run[segment_start..];
    }
    if separators < 2 || basename.is_empty() {
        return None;
    }
    Some(format!("<redacted>/{basename}"))
}

/// A JSON string literal, path-redacted unless `verbose`.
pub(in crate::vm::vm_init) fn json_string(value: &str, verbose: bool) -> String {
    let escaped = json_escape(value);
    if verbose {
        escaped
    } else {
        redact_absolute_paths(&escaped)
    }
}

/// `null`, or a (possibly redacted) JSON string literal.
pub(in crate::vm::vm_init) fn json_opt_string(value: Option<&str>, verbose: bool) -> String {
    match value {
        Some(v) => json_string(v, verbose),
        None => "null".to_string(),
    }
}

/// How many process-global Real-JDK violation sinks
/// [`SharedVm::jdk_only_process_violations`] returns, and the meaning of each
/// index. The order is a contract: `--trace-jdk-only` keeps one watermark per
/// slot across the run, so inserting a sink in the middle would silently
/// re-report one source and skip another.
///
/// | index | sink | recorded by |
/// |-------|------|-------------|
/// | 0 | `cratonvm_jit::jdk_only_jit_violations` | JIT compile-time direct-bind scan |
/// | 1 | `crate::jit::helpers::jdk_only_jit_helper_violations` | JIT by-name fast-path admission |
/// | 2 | `crate::vm::jdk_only_native_shadow_observations` | interpreter dispatch resolver |
///
/// New sinks append; they never insert.
pub const JDK_ONLY_PROCESS_SINKS: usize = 3;

/// Exact refusal-event counts for the Real-JDK report's `refusals` block.
///
/// # Why this is a sibling of `counts` and not four more keys in it
///
/// Contract §9 spells `counts` as a **closed seven-key set**, and its four
/// class keys are a partition proved by a `debug_assert_eq!` against the
/// class-origin row count. Adding a refusal key there would break both
/// properties at once: the set would no longer be the one §9 documents, and a
/// reader summing the block to recover the class total would get a number that
/// is not the class total. The two blocks also answer different questions —
/// `counts` is a census of what the run *contains*, `refusals` is a tally of
/// what strict policy *stopped* — and they are not commensurable: a class
/// appears in `counts` once, whereas a refused method appears in `refusals`
/// once per call.
///
/// Every field is a count of events, not of distinct methods. The distinct
/// methods are the rows in `violations[]`, which are deduplicated and capped at
/// 256 per sink; these counters are exact and uncapped. A field being larger
/// than the matching row count is the normal case, not a discrepancy.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct JdkOnlyRefusalCounts {
    /// Compile-time thin-native direct binds the JIT refused. Each distinct
    /// triple also appears once in sink 0 of
    /// [`SharedVm::jdk_only_process_violations`].
    pub jit_direct_native_binds: u64,
    /// Inline-cache publications of an unowned (native/builtin) entry the JIT
    /// refused. **Counter-only**: the publication site holds an entry address,
    /// not a name triple, so there is no honest row to emit and none is
    /// fabricated. This number is the sole evidence of this refusal class.
    pub jit_inline_cache_natives: u64,
    /// JIT by-name native fast-path admissions refused at cache-fill time.
    /// Includes both outright `Reject`s (which contribute a row to sink 1) and
    /// §7-step-3 yields to bytecode (which do not — the resolver returns "no
    /// opinion", and there is no violation object to record). So this is a
    /// strict superset of sink 1's length, by design.
    pub jit_fastpath_admissions: u64,
    /// Thin JIT direct-call helper fast paths refused at CALL time, because
    /// the generic tail they decline to would refuse as well.
    ///
    /// The callee-side twin of `jit_direct_native_binds`, and the counter
    /// `docs/feature-designs/jit-ir-optimizations.md`
    /// is about. Counter-only, like `jit_inline_cache_natives`: a guarded
    /// helper holds a constant triple and the bind-time half already recorded
    /// a row for it, so emitting a second one per call would duplicate an
    /// identity `violations[]` already carries.
    ///
    /// **A pre-2026-09-22 binary's reading of this is not comparable.** Until
    /// that date the guard refused every call in strict mode without asking
    /// whether the tail would, and the tail — `vm_exec::invoke_or_native` —
    /// ran the same `Bridge` anyway. An old number counts detours; this one
    /// counts blocks.
    pub jit_direct_helper_fastpaths: u64,
    /// Thin JIT direct-call helper fast paths ADMITTED under strict policy.
    ///
    /// Not a refusal, and deliberately excluded from [`Self::total`]. It is
    /// here because its sibling above is otherwise unreadable: zero refusals
    /// means "strict mode blocked nothing" only if something reached the
    /// guard, and this is the number that says so.
    pub jit_direct_helper_admissions: u64,
    /// Times the interpreter's dispatch resolver preferred concrete bytecode
    /// over a registered non-intrinsic native. Distinct triples appear in
    /// sink 2; this count is exact and uncapped.
    pub interpreter_bytecode_preferred: u64,
    // t1 wave 6, lane Y3: `interpreter_shadow_unenforced` (a `Bridge` that
    // stood in front of concrete bytecode at `try_stackless_invoke` step 1 and
    // ran anyway) was removed. Its only recorder,
    // `record_native_shadow_ran_over_bytecode`, lost its caller when dispatch
    // became bytecode-first (`3d31008561`): step 1 now lets only a reviewed
    // `Intrinsic` win before method resolution, and `resolve_dispatch_in`
    // always prefers concrete bytecode over a `Bridge`. So it read 0 on every
    // run, which looked like a measurement.
}

impl JdkOnlyRefusalCounts {
    /// Total refusal events across every refusal source. Not a violation
    /// count — see the type docs. `jit_direct_helper_admissions` is
    /// excluded: it does not count something strict policy stopped.
    pub fn total(self) -> u64 {
        self.jit_direct_native_binds
            + self.jit_inline_cache_natives
            + self.jit_fastpath_admissions
            + self.jit_direct_helper_fastpaths
            + self.interpreter_bytecode_preferred
    }

    /// Whether strict policy refused nothing at all. Always true in
    /// `Compatible` mode.
    pub fn is_zero(self) -> bool {
        self.total() == 0
    }
}

/// The four coarse class buckets the Real-JDK report's `counts` block carries
/// (contract §9).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OriginBuckets {
    pub(crate) boot_image: u64,
    pub(crate) application: u64,
    pub(crate) generated: u64,
    pub(crate) compatibility: u64,
}

impl OriginBuckets {
    pub(crate) fn total(self) -> u64 {
        self.boot_image + self.application + self.generated + self.compatibility
    }
}

/// Fold the ten `ClassOrigin::as_str()` tags into the report's four buckets.
///
/// The fold is a **partition**: every row lands in exactly one bucket, so
/// [`OriginBuckets::total`] equals the row count and a class can never be lost
/// between `--dump-class-origins` and the report's summary. `user-defined`
/// counts as an application class (application bytes, whoever called
/// `defineClass`); the five VM-created origins plus `vm-internal` count as
/// generated — no class file, but a legitimate generation origin (§1 item 6).
///
/// `compatibility-stub` keeps its own bucket and is never folded in with the
/// legitimate generated origins: conflating those two is exactly the confusion
/// this feature exists to end.
pub(crate) fn fold_origin_buckets(rows: &[ClassOriginEntry]) -> OriginBuckets {
    let mut buckets = OriginBuckets::default();
    for row in rows {
        match row.origin.as_str() {
            "boot-image" => buckets.boot_image += 1,
            "application-class-path" | "user-defined" => buckets.application += 1,
            "vm-array"
            | "hidden-class"
            | "generated-lambda"
            | "generated-proxy"
            | "reflection-accessor"
            | "vm-internal" => buckets.generated += 1,
            "compatibility-stub" => buckets.compatibility += 1,
            // An unknown tag means the origin vocabulary grew. Count it as
            // generated rather than dropping it: an unclassified class must
            // still show up in the totals.
            _ => buckets.generated += 1,
        }
    }
    buckets
}

/// Serialize the class-origin census. Split out of
/// [`SharedVm::dump_class_origins_json`] so the sort order, the seeded `counts`
/// block and the redaction can be unit-tested without booting a VM.
///
/// `rows` is sorted in place by `(name, loader_id, origin)`.
pub(crate) fn render_class_origins_json(rows: &mut [ClassOriginEntry], verbose: bool) -> String {
    // A class name legitimately appears once per defining loader, and twice
    // with different origins during an in-place stub-upgrade window, so all
    // three fields are needed for a total order.
    rows.sort_by(|a, b| (&a.name, a.loader_id, &a.origin).cmp(&(&b.name, b.loader_id, &b.origin)));

    let mut counts: std::collections::BTreeMap<String, u64> = CLASS_ORIGIN_TAGS
        .iter()
        .map(|tag| ((*tag).to_string(), 0u64))
        .collect();
    for row in rows.iter() {
        *counts.entry(row.origin.clone()).or_insert(0) += 1;
    }

    let mut out = String::with_capacity(256 + 160 * rows.len());
    out.push_str("{\n  \"schema_version\": 1,\n  \"counts\": {\n");
    for (tag, n) in &counts {
        out.push_str(&format!("    {}: {n},\n", json_escape(tag)));
    }
    out.push_str(&format!("    \"total\": {}\n", rows.len()));
    out.push_str("  },\n  \"classes\": [");
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("\n    {\n");
        // `name` is redacted like the other two: a `CompatibilityStub` for a
        // class named after a jar entry can carry the jar's absolute path.
        out.push_str(&format!(
            "      \"name\": {},\n",
            json_string(&row.name, verbose)
        ));
        out.push_str(&format!(
            "      \"origin\": {},\n",
            json_escape(&row.origin)
        ));
        out.push_str(&format!(
            "      \"reason\": {},\n",
            json_opt_string(row.reason.as_deref(), verbose)
        ));
        out.push_str(&format!(
            "      \"requested_by\": {},\n",
            json_opt_string(row.requested_by.as_deref(), verbose)
        ));
        out.push_str(&format!(
            "      \"real_bytes_found\": {},\n",
            row.real_bytes_found
        ));
        out.push_str(&format!("      \"loader_id\": {},\n", row.loader_id));
        // Direct supertypes, in declaration order (superclass, then
        // interfaces). Never redacted: these are JDK/application type names,
        // not paths, and the interception join is useless without them.
        out.push_str("      \"supertypes\": [");
        for (i, s) in row.supertypes.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&json_escape(s));
        }
        out.push_str("]\n");
        out.push_str("    }");
    }
    if !rows.is_empty() {
        out.push_str("\n  ");
    }
    out.push_str("]\n}\n");
    out
}

/// The class-origin census a `System.exit` produced when it could not take the
/// class-manager lock.
///
/// Deliberately *not* the same shape as an empty successful census. A consumer
/// that sees `"total": 0` with no `"partial"` key is entitled to conclude the
/// run fabricated nothing; this file says the opposite — the run was not
/// measured. `difftest` and `scripts/jdk-only-census.sh` both categorise from
/// these files, and a silently-short census would categorise a strict failure
/// as a clean run.
///
/// The `counts` block is omitted rather than zero-filled for the same reason:
/// a zero in a count key is a measurement, and there was none.
pub(crate) fn render_partial_class_origins_json() -> String {
    concat!(
        "{\n",
        "  \"schema_version\": 1,\n",
        "  \"partial\": true,\n",
        "  \"partial_reason\": \"class-manager lock unavailable on the System.exit path; ",
        "no class-origin rows were read\",\n",
        "  \"classes\": []\n",
        "}\n"
    )
    .to_string()
}

impl SharedVm {
    /// Thread-safe class loading with per-class-name locks (Session 30).
    ///
    /// Instead of holding the global ClassManager write lock for the entire
    /// loading process, this method:
    /// 1. Checks under **read lock** if the class is already loaded (fast path).
    /// 2. Acquires a **per-class-name lock** to serialize concurrent loaders of
    ///    the *same* class (threads loading *different* classes proceed in parallel).
    /// 3. Re-checks under read lock (double-check after acquiring per-class lock).
    /// 4. Actually loads under write lock only for the parsing/registration step.
    /// 5. Notifies any waiters on the per-class lock.
    pub fn load_class_concurrent(
        &self,
        name: &str,
    ) -> Result<crate::classloading::ClassId, cratonvm_types::error::VmError> {
        self.load_class_concurrent_for(name, None)
    }

    /// [`Self::load_class_concurrent`], offering the class file to this VM's
    /// `java.lang.instrument` transformer chain first.
    ///
    /// This is the entry point every class load that has a Java thread in hand
    /// should use, because running a `ClassFileTransformer` means running Java,
    /// and that needs a thread. Callers with no thread (VM bootstrap, the JIT's
    /// own resolution, JNI helpers before attach) keep `load_class_concurrent`;
    /// they load classes an agent could not have been registered in time to see
    /// anyway.
    ///
    /// Strictly additive: with no agent installed
    /// (`instrument::transformers_armed` is false) this is
    /// `load_class_concurrent` plus one relaxed atomic load, and even with an
    /// agent installed a chain that declines every class changes nothing about
    /// how the load proceeds.
    pub fn load_class_transformed(
        &self,
        thread: &mut crate::threading::JvmThread,
        name: &str,
    ) -> Result<crate::classloading::ClassId, cratonvm_types::error::VmError> {
        if crate::runtime::instrument::load_hook_armed(self) {
            crate::runtime::instrument::pre_transform_for_load(self, thread, name, 0);
        }
        // gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
        // item 1c): a C agent's `ClassLoad` is posted from the class manager's
        // guard drop inside this thread-less load (`class_load_adapter`), which
        // has no handle of this thread. While a C env listens, the load runs
        // with this thread's JNI binding installed from `thread` itself (the
        // load cannot touch `thread`), and the callback's context keeps it.
        // One load when no C env listens.
        #[cfg(feature = "experimental-debug")]
        if self.debug.debugger_gates.native_class_load_armed() {
            return crate::runtime::interpreter::post_on_own_thread(thread, || {
                self.load_class_concurrent(name)
            });
        }
        self.load_class_concurrent(name)
    }

    /// The define of a FINALIZABLE class makes its ancestors' constructors
    /// that can throw before their initializing call stay interpreted (JIT
    /// round 21 wave 13, lane finalize,
    /// `CRATONVM_FINALIZER_ANCESTOR_CTOR_STAYS_INTERPRETED`;
    /// `exception_dispatch::finalizer_bars_compiling`). In order: the class
    /// store records each such ancestor (the bar answers `true` from then
    /// on), the `jit_gate_pass` memo forgets its constructors' allowed
    /// answers, then the cache withdraws the bodies compiled or spliced
    /// before (a compile racing the define is refused by the logged record).
    /// No lock is held across two of the steps; the caller holds none of
    /// them. Empty work, after one class read, for a class without a
    /// finalizer. Idempotent: a class that reaches both define listeners
    /// (`load_class_concurrent_for` and `define_class_full`) is settled by
    /// the first.
    ///
    /// Split out of the `load_class_concurrent_for` listener by review
    /// rev-w13 (JIT round 21 wave 14, item 1 of
    /// `docs/feature-designs/real-jdk-mode.md`): a class a
    /// Java loader defines (`ClassLoader.defineClass1/2/0`) reaches only
    /// `define_class_full`, which never ran it.
    pub(crate) fn note_finalizable_define(&self, class_id: crate::classloading::ClassId) {
        crate::runtime::frozen_pages::note_class_defined(self, class_id);
        // JIT round 21 wave 17 (lane finalize-reinit, items 1 and 2 of
        // `docs/feature-designs/real-jdk-mode.md`): the
        // walk goes past a finalizable superclass
        // (`finalizer_ancestor_walk_past_finalizable`), and every reached
        // ancestor is recorded, stale or not (`finalizer_ancestor_record_reachable`),
        // so the bar reads a redefined ancestor's current constructors; only
        // the stale ones are withdrawn.
        let reached = {
            let cm = self.classes.class_manager.read();
            crate::runtime::interpreter::finalizable_define_ancestors(
                &self.config,
                self.finalizer_ancestor_walk_past_finalizable,
                cm.class_store(),
                class_id,
            )
        };
        let record_reachable = self.finalizer_ancestor_record_reachable;
        if reached.is_empty() || (!record_reachable && !reached.iter().any(|a| a.stale)) {
            return;
        }
        // Only the stale ancestors this call recorded first go on: one the
        // other listener (or an earlier finalizable sibling) already recorded
        // was withdrawn then, and a second pass would only log another record.
        let finalizable_stale: Vec<(cratonvm_types::ClassId, String)> = {
            let mut cm = self.classes.class_manager.write();
            reached
                .into_iter()
                .filter(|a| record_reachable || a.stale)
                .filter(|a| cm.class_store.mark_finalizable_ancestor(a.id) && a.stale)
                .map(|a| (a.id, a.name))
                .collect()
        };
        if finalizable_stale.is_empty() {
            return;
        }
        self.jit.jit_gate_pass.write().retain(|(id, method, _), _| {
            &**method != "<init>" || !finalizable_stale.iter().any(|(a, _)| a == id)
        });
        let mut jit = self.jit.jit_cache.write();
        for (ancestor_id, ancestor_name) in &finalizable_stale {
            let _ = jit.invalidate_for_finalizable_subtype(*ancestor_id, ancestor_name);
        }
    }

    /// The VM half of a stub upgrade that moved `has_finalizer` (JIT round 21
    /// wave 23, lane finvm; item 8 of
    /// `docs/feature-designs/real-jdk-mode.md`,
    /// `CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES`, which the class
    /// manager reads: under `=0` it queues no row and this never runs).
    /// `flips` is `ClassManager::take_stub_upgrade_finalizer_flips`, the
    /// upgraded class and every recomputed subclass whose bit moved, parents
    /// first, each with its new bit. Called from the class-manager write
    /// guard's release (`ClassManagerWriteGuard::drop`), with no class-manager
    /// lock held.
    ///
    /// * Every moved class's published allocation recipe
    ///   (`jit_alloc_class_cache`, whose `has_finalizer` the interpreter's
    ///   `new`, the JIT's allocation helpers and `class_has_finalizer_of` read
    ///   lock-free) is dropped, so the next `new` rebuilds it from the current
    ///   bit. A stub-descended class never had one (`ClassAllocInfo::build`
    ///   refuses a stub chain); this is the drop that does not depend on the
    ///   JIT invalidate hook reaching this VM.
    /// * A class that BECAME finalizable runs the define-time finalizer
    ///   listener ([`Self::note_finalizable_define`]): its ancestors'
    ///   constructors that can throw before their initializing call are
    ///   recorded and their compiled bodies withdrawn, as if it had been
    ///   defined finalizable.
    /// * A class that STOPPED being finalizable is treated as the define of a
    ///   non-finalizable class is: no listener runs, and the ancestor records
    ///   an earlier define left stay (they are sticky, and fail closed: the
    ///   bar only keeps a constructor interpreted). Instances registered while
    ///   the old bit held keep their registration.
    pub(crate) fn note_stub_upgrade_finalizer_flips(
        &self,
        flips: &[(crate::classloading::ClassId, bool)],
    ) {
        for &(class_id, _) in flips {
            let _ = self.jit.jit_alloc_class_cache.invalidate(class_id.as_u32());
        }
        // Wave 23 follow-up (item 1 of `r21-review-w23-findings-FIXED-20261020.md`):
        // the class's OWN constructors were admitted while it was not
        // finalizable, and a memoized allowed `jit_gate_pass` row is never
        // asked again, so its rows go and its compiled or spliced constructor
        // bodies are withdrawn, as the ancestors' are below. The bit was
        // flipped under the class-manager write lock before this runs, so a
        // fill racing the drop is caught by the fill's own re-read of the bar
        // (`execute`'s gate in `interpreter.rs`).
        let became: Vec<(crate::classloading::ClassId, String)> = {
            let cm = self.classes.class_manager.read();
            flips
                .iter()
                .filter(|&&(_, now_finalizable)| now_finalizable)
                .filter_map(|&(id, _)| {
                    cm.class_store().get(id).map(|c| (id, c.name.to_string()))                })
                .collect()
        };
        if !became.is_empty() {
            self.jit.jit_gate_pass.write().retain(|(id, method, _), _| {
                &**method != "<init>" || !became.iter().any(|(b, _)| b == id)
            });
            let jit = self.jit.jit_cache.write();
            for (class_id, class_name) in &became {
                let _ = jit.invalidate_for_finalizable_subtype(*class_id, class_name);
            }
        }
        for (class_id, _) in &became {
            self.note_finalizable_define(*class_id);
        }
    }

    /// [`Self::load_class_concurrent`], naming the Java frame that asked.
    ///
    /// `requester` is `(owner_class, method_name, descriptor)` of the frame
    /// whose `new` / `checkcast` / `Class.forName` drove this resolution. It
    /// exists so that when the load fabricates a compatibility class, the
    /// recorded `CompatibilityClassRequested` violation — and the
    /// `--dump-class-origins` row — can say *who* depends on the fabrication.
    /// "Class `X` was fabricated" is a name; "class `X` was fabricated because
    /// `org/foo/Bar.baz(…)` resolved it" is a work item (contract §9).
    ///
    /// Passed as three borrowed `&str`s, not a formatted `String`: this is on
    /// the path of every `new` in the VM, and the overwhelming majority of
    /// loads fabricate nothing. Nothing is formatted or allocated unless the
    /// load actually recorded a violation.
    ///
    /// `None` is the right answer for callers with no Java frame — VM
    /// bootstrap, JNI, the JIT's own resolution — and leaves `requested_by`
    /// null, which the census documents as "not attributable to a frame"
    /// rather than "nobody asked".
    pub fn load_class_concurrent_for(
        &self,
        name: &str,
        requester: Option<(&str, &str, &str)>,
    ) -> Result<crate::classloading::ClassId, cratonvm_types::error::VmError> {
        // Fast path: read lock only — no contention for already-loaded classes.
        // Bind the result to a local so the `RwLockReadGuard` is dropped at
        // the semicolon, not extended to the end of an `if let` block.
        //
        // Runtime-package-identity bug fix: was the bare, requester-less
        // `get_loaded_class_id(name)`, which -- when no BUILT-IN loader
        // (bootstrap/extension/application) has defined `name` yet -- falls
        // back to returning an arbitrary lone user-defined loader's own
        // copy if exactly one such loader happens to have defined it (see
        // that fn's doc comment). This function's own slow path below
        // ultimately delegates via `ClassManager::load_class`, which can
        // only ever produce a Bootstrap/Extension/Application-loaded
        // class -- never an unrelated user-defined loader's redefinition
        // -- so a fast-path cache hit returning one answers a question
        // this function was never asked and hands back the WRONG class
        // (JVMS §5.3: defining loader is part of a class's identity).
        // `get_loaded_class_id_for_requester(name, Application)` probes
        // only the built-in delegation chain, matching what the slow path
        // can actually produce: a hit here is always right, a miss falls
        // through to the real load below instead of a stray loader's class.
        let fast_id = self
            .classes
            .class_manager
            .read()
            .resolve_fast_path_class_id(name);
        if let Some(id) = fast_id {
            // Check if it's a synthetic stub that needs upgrading
            let is_synthetic = self
                .classes
                .class_manager
                .read()
                .class_store
                .get(id)
                .map(|c| c.origin.is_compatibility_stub())
                .unwrap_or(false);
            if !is_synthetic {
                return Ok(id);
            }
            // Fall through to acquire write lock and upgrade
        }

        // Get or create per-class-name lock
        let class_lock = {
            let mut locks = self.classes.class_loading_locks.lock();
            locks
                .entry(name.to_string())
                .or_insert_with(|| {
                    Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()))
                })
                .clone()
        };

        let (lock, cvar) = &*class_lock;
        let mut loading = lock
            .lock()
            .expect("class-loading mutex poisoned: a thread panicked while loading a class");

        // Double-check: another thread may have loaded it while we waited for the lock.
        // Same loader-faithful lookup as the fast path above -- see that
        // comment for why the bare `get_loaded_class_id` is unsound here.
        {
            let cm = self.classes.class_manager.read();
            if let Some(id) = cm.resolve_fast_path_class_id(name) {
                let is_synthetic = cm
                    .class_store
                    .get(id)
                    .map(|c| c.origin.is_compatibility_stub())
                    .unwrap_or(false);
                if !is_synthetic {
                    return Ok(id);
                }
            }
        }

        // If another thread is currently loading this exact class, wait for it
        while *loading {
            loading = cvar
                .wait_timeout(loading, std::time::Duration::from_secs(30))
                .expect("class-loading condvar poisoned: a thread panicked while loading a class")
                .0;
            // Re-check after waking — class may now be loaded. Same
            // loader-faithful lookup as the fast path above (see that
            // comment) -- a lone unrelated user-defined loader's copy must
            // not be handed back here either.
            let cm = self.classes.class_manager.read();
            if let Some(id) = cm.resolve_fast_path_class_id(name) {
                let is_synthetic = cm
                    .class_store
                    .get(id)
                    .map(|c| c.origin.is_compatibility_stub())
                    .unwrap_or(false);
                if !is_synthetic {
                    return Ok(id);
                }
            }
        }

        // We are the loader for this class — mark as loading.
        //
        // B3: wrap the `*loading = true` … `*loading = false` + notify span in
        // an RAII guard so that an UNWIND through `load_class` (which has many
        // `.expect`/`pop_unchecked` paths) still resets the flag and wakes
        // waiters. Without it, a panic during load leaves `*loading == true`
        // forever: every later loader of this class double-checks (not loaded),
        // enters `while *loading`, and spins on the 30s wait timeout that no
        // notifier will ever satisfy — turning one load panic into a cascade of
        // stuck/aborting loaders. The guard's `Drop` runs on both the success
        // path and the unwind path; success-path cleanup (`remove(name)`) runs
        // after the guard is dropped below.
        *loading = true;
        drop(loading);

        /// Resets the per-class loading flag and notifies waiters on drop,
        /// including when the enclosing scope unwinds due to a panic.
        struct LoadingFlagGuard {
            lock: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        }
        impl Drop for LoadingFlagGuard {
            fn drop(&mut self) {
                let (lock, cvar) = &*self.lock;
                // On unwind the mutex is not poisoned (we dropped the guard
                // before the fallible work), but tolerate a poisoned lock
                // defensively so the notify still fires.
                let mut loading = match lock.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *loading = false;
                cvar.notify_all();
            }
        }
        // `Option` so the success path can release it between the write
        // lock's release and the ClassLoad drain (gci i47w/oz47, below).
        let mut loading_guard = Some(LoadingFlagGuard {
            lock: class_lock.clone(),
        });

        // T19.H7 diag — periodic checkpoints around class load.
        // Rate-limited via static AtomicU32 so we don't drown stderr.
        // Feature-gated (off by default) — sessions 93-94 reverted two silent
        // re-enables of this trace; the gate keeps it out of any non-debug
        // build. To re-enable: cargo build --features experimental-t19-diag.
        #[cfg(feature = "experimental-t19-diag")]
        static T19_H7_LOAD_TRACE: std::sync::atomic::AtomicU32 =
            std::sync::atomic::AtomicU32::new(0);
        #[cfg(feature = "experimental-t19-diag")]
        let _t19_h7_n = T19_H7_LOAD_TRACE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        #[cfg(feature = "experimental-t19-diag")]
        if _t19_h7_n < 250 {
            tracing::debug!(target: "cratonvm::t19_h7", "load[#{_t19_h7_n}] ENTER class={name}");
        }

        // Actually load the class (this takes the global write lock briefly)
        let mut cm_guard = self.classes.class_manager_write();
        // Contract §9 `requested_by`: read the violation count under the same
        // write lock that performs the load, so no other thread's fabrication
        // can be misattributed to this frame. `origin_violation_count` is a
        // `Vec::len`, and `attach_origin_requester` returns without allocating
        // when the count did not move — which is every load but the ones that
        // actually fabricate.
        let violations_before = requester.map(|_| cm_guard.origin_violation_count());
        // Whether the load below mints a class (see `define_withdraws_by_name`).
        let next_id_before = cm_guard.class_store.next_id();
        let result = cm_guard.load_class(name);
        if let (Some(before), Some((owner, method, descriptor))) = (violations_before, requester) {
            cm_guard.attach_origin_requester(before, owner, method, descriptor);
        }
        // gci i47w lane oz47 (`vi12-ev.list` T: every row a `[stw-stall]`
        // hang with the defer switch OFF, none with it ON): dropping
        // `cm_guard` drains this thread's queued JVMTI `ClassLoad` hooks, and
        // a C agent's callback may wait GC-safe (`RawMonitorWait`) and then
        // wait out a pause at its region's end. Until now the per-name
        // `loading` flag stayed raised through that drain, so a second loader
        // of the same class sat in the `cvar` wait above -- counted and
        // running, not GC-safe -- and the pause, the callback and that loader
        // waited for one another. Clear the flag and wake those loaders
        // between the write lock's release and the hook drain
        // (`run_before_hooks`): NOT before the release -- the flag's mutex
        // is taken by a waiting loader before its `class_manager.read()`
        // double-check, so taking it under the write lock deadlocked
        // (i48h-dbg stacks: this thread in the guard's drop under the write
        // lock, a second loader holding the per-name mutex in
        // `lock_shared_slow`). The woken loaders re-check under a read lock
        // and find the class (or, after a failed load, take the load over,
        // as before). HotSpot's placeholder
        // waiters wait safepoint-safe; this keeps ours out of the callback's
        // way instead. Both modes, every collector; with no agent the drain
        // is empty and only the wake moves ahead of the guard's drop.
        if let Some(flag) = loading_guard.take() {
            cm_guard.run_before_hooks(move || drop(flag));
        }
        drop(cm_guard);

        #[cfg(feature = "experimental-t19-diag")]
        if _t19_h7_n < 250 {
            match &result {
                Ok(_) => {
                    tracing::debug!(target: "cratonvm::t19_h7", "load[#{_t19_h7_n}] OK class={name}")
                }
                Err(e) => {
                    tracing::debug!(target: "cratonvm::t19_h7", "load[#{_t19_h7_n}] ERR class={name} err={e:?}")
                }
            }
        }

        // obsaudit D14 (2026-07-26): the hand-written real-env ClassLoad
        // notification that used to live here was removed. It fired on
        // every successful `Ok(class_id)` unconditionally — including a
        // cache hit against an already-loaded class, which is not a new
        // ClassLoad and should not re-fire one. ClassLoad now reaches the
        // real env exactly once per class, from
        // `JvmtiEventManager::fire_class_load`'s bridge (see
        // `install_real_agent_env_bridge` in `runtime/jvmti.rs`), which is
        // driven by `ClassManager::define_class_shared_with_options` and so
        // only runs when the class is actually newly defined.
        if let Ok(class_id) = &result {
            // T5.4.4 — class hierarchy change invalidation.
            //
            // When a new class is loaded, any JIT-compiled method
            // that inlined calls from this class (via CHA-based
            // devirtualization) must be evicted because a new subclass
            // may override the inlined method. The `inlined_methods`
            // list on each `CompiledMethod` records which classes
            // were inlined; `invalidate_for_class_change` walks the
            // cache and removes matching entries.
            //
            // Also invalidate for the superclass chain: if class `B`
            // extends `A`, and a method inlined from `A` was
            // devirtualized under the assumption that `A` had no
            // subclasses, loading `B` breaks that assumption.
            // PGO-02 R3 — two defects fixed here at once.
            //
            // 1. This used to be `c.superclass.map(|s| s.to_string())`.
            //    `superclass` is a `ClassId`, whose `Display` prints the raw
            //    u32, so the "superclass" handed to the name-keyed scans below
            //    was a DECIMAL NUMBER — `"42"`, never a class name. The scans
            //    match against `CompiledMethod::inlined_methods`, which holds
            //    internal class names, so the whole superclass half of
            //    class-load invalidation matched nothing and had been a silent
            //    no-op. Nothing failed when it broke: the guarded/devirtualised
            //    code stays CORRECT without the eviction (an exact class-id
            //    guard rechecks the receiver, and a MIC/PIC re-targets), so the
            //    only symptom was code that should have been retired staying
            //    resident. That is precisely the failure mode this project
            //    tracks — a capability that reads as landed but never runs.
            //
            // 2. Only the DIRECT superclass was consulted, which is the
            //    "known coarseness" `docs/feature-designs/profile-guided-
            //    inlining.md` §4 records: loading `C extends B extends A` did
            //    not reach a dependency on `A`. Walk the whole supertype
            //    closure — superclasses AND interfaces — so a speculation on
            //    any ancestor is retired when a new descendant appears. The
            //    closure is bounded by hierarchy depth and this runs once per
            //    class DEFINE, not per call.
            //
            // Interpreter round i4 wave 88, lane L7
            // (`CRATONVM_JIT_SUBTYPE_DEFINE_SCOPED`, default ON, `=0`
            // restores the name-only match): with each supertype goes the
            // set of its members a new subtype cannot rebind -- its static,
            // private and final methods (all of them for a final class; the
            // constructors and initialiser are excluded by name in the match)
            // -- so `JitCache::invalidate_for_new_subtype` withdraws only the
            // bodies that inlined or devirtualised an OVERRIDABLE member.
            // Matching by class name alone withdrew every body that inlined a
            // constructor chain whenever any subclass of a class in it was
            // loaded: every lazily loaded `AbstractPoolEntry` subclass
            // recompiled `Utf8EntryImpl.<init>`, and the transitive raw-caller
            // closure took its callers along (`L4W88FindConstructorSplitBench`:
            // 1284 installs over 713 methods).
            //
            // JIT round j27 wave 2 (lane REDEFINE): the closure walk and the
            // withdrawal live in `SharedVm::note_subtype_define`
            // (`vm/src/vm/subtype_define.rs`), which the Java-loader define
            // paths (`define_class_full` and its siblings, JNI `DefineClass`)
            // now run too; this listener alone missed every class a Java
            // loader defines.
            // JIT round 21 wave 13 (lane finalize,
            // `CRATONVM_FINALIZER_ANCESTOR_CTOR_STAYS_INTERPRETED`, default ON):
            // a FINALIZABLE class makes its ancestors' constructors that can
            // throw before their initializing call stay interpreted
            // (`exception_dispatch::finalizer_bars_compiling`), so the bodies
            // compiled or spliced before this define are withdrawn below.
            // Empty for every class without a finalizer. The order matters:
            // the class store records the ancestor first (the bar answers
            // `true` from then on), then the `jit_gate_pass` memo forgets its
            // constructors' allowed answers, then the cache withdraws (a
            // compile racing the define is refused by the logged record).
            //
            // Review rev-w13 (JIT round 21 wave 14, item 1 of
            // `r21-review-w14-findings-20261019.md`): the steps live in
            // `note_finalizable_define`, which `define_class_full`
            // (`ClassLoader.defineClass1/2/0`, `Lookup.defineClass`, the
            // hidden-class and `Unsafe` defines) now calls too. This listener
            // alone missed every class a Java loader defines.
            self.note_finalizable_define(*class_id);
            // Round 12 wave 5 (lane tier3): the loaded class's OWN name
            // matters only if the load replaced a class (a stub upgraded in
            // place). A freshly minted id is a new class beside any
            // same-named class of another loader, and a new class breaks no
            // assumption about itself; its supertypes, below, are what a new
            // subclass or implementor invalidates.
            if crate::vm::realms::jit_realm::define_withdraws_by_name(
                *class_id,
                next_id_before,
                false,
            ) {
                let _ = self.jit.jit_cache.write().invalidate_for_class_change(name);
            }
            let _ = self.note_subtype_define(*class_id);
            // T5.4.4 — that loop IS the whole class-hierarchy listener. A
            // compiled body's dependency record is its own `inlined_methods`,
            // filled from `InlinePlan::invalidation_triples`, which names both
            // an inlined callee and the receiver class a guarded speculation
            // relied on. A second listener used to run here over an
            // `InvalidationManager` whose `register_assumption` no compile path
            // ever called, so it evicted nothing while its comment claimed to
            // close the CHA loop; it has been deleted. A compile still running
            // when this define lands is refused at publication by the cache's
            // invalidation log (`JitCache::put`).

            // Phase 1 — Item 6: `@EnableGpuAsync(warmup = N)` class-load
            // warmup. If the class is annotated, eagerly pre-compile up
            // to `N` `@GpuKernel`-annotated methods so the first call
            // does not pay the analyzer + PTX lowering cost. Cheap when
            // offload is off — `maybe_warmup_gpu` short-circuits on the
            // `gpu_offload_enabled` flag.
            //
            // The class manager read-lock is held for the duration of
            // the warmup; the inner `lookup_or_compile` only touches
            // the `OffloadCache`'s own locks (`kernels`, `blacklist`)
            // so there is no re-entrancy risk against the manager.
            #[cfg(feature = "gpu-offload")]
            {
                let class_id = *class_id;
                let cm = self.classes.class_manager.read();
                if let Some(class) = cm.get_class(class_id) {
                    crate::runtime::offload::maybe_warmup_gpu(self, class, class_id);
                }
            }
        }

        // Mark done and notify all waiters for this class. Dropping the guard
        // re-acquires the lock, sets `*loading = false`, and `notify_all()`s
        // (the same work the old manual block did) — and it would have run the
        // same reset/notify automatically had `load_class` above unwound.
        // Already taken on the success path above; kept for any other.
        drop(loading_guard);

        // Clean up per-class lock entry to avoid unbounded growth
        // (only if nobody else is waiting — check refcount). Done after the
        // guard drop so waiters have already been notified.
        if Arc::strong_count(&class_lock) <= 2 {
            // 2 = our local + the HashMap entry — no other thread holds it
            self.classes.class_loading_locks.lock().remove(name);
        }

        result
    }

    /// Register a newly-allocated object as finalizable.
    ///
    /// Called by the allocator when the object's class has `has_finalizer == true`.
    /// The object address is registered with the reference processor as a
    /// Finalizer reference so GC can enqueue it for `finalize()` invocation.
    pub fn register_finalizable(&self, obj_addr: usize) {
        let mut rp = self.mem.ref_processor.lock();
        rp.discover_reference(
            cratonvm_gc::reference::ReferenceType::Finalizer,
            obj_addr, // reference_obj = the object itself
            obj_addr, // referent = same object
            None,     // no ReferenceQueue
        );
    }

    /// JIT round 21 wave 10, lane finalize (`CRATONVM_FINALIZER_REGISTER_AT_OBJECT_INIT`,
    /// default ON): withdraw the finalizer registration of an object whose
    /// constructor provably threw before `Object.<init>` returned, when no
    /// collection or sweep has run since `epoch` was read
    /// (`HeapRealm::address_epoch`), so `obj_addr` still names that object.
    /// The interpreter's unwinder calls it for a creator frame it unwinds
    /// (`exception_dispatch::unwind_to_handler`). Returns whether a row went.
    pub fn withdraw_unconstructed_finalizable_now(&self, obj_addr: usize, epoch: u64) -> bool {
        if self.mem.address_epoch() != epoch {
            return false;
        }
        self.mem.ref_processor.lock().withdraw_finalizable(obj_addr)
    }

    /// Record an unconstructed finalizable object whose creator is a native
    /// door (see [`Self::withdraw_unconstructed_finalizable_now`]; the
    /// unwinder reached its initial frame). Dropped when `epoch` is stale.
    pub fn note_unconstructed_finalizable(&self, obj_addr: usize, epoch: u64) {
        if self.mem.address_epoch() != epoch {
            return;
        }
        self.mem
            .ref_processor
            .lock()
            .note_unconstructed_finalizable(obj_addr, epoch);
    }

    /// The native door's half: `obj_addr` is the object it allocated and
    /// registered and whose constructor call returned an exception, and
    /// `epoch_before_call` the address epoch it read before that call. When
    /// no collection has run since, and the unwinder noted the object as
    /// unconstructed in this epoch, its registration is withdrawn. Returns
    /// whether a row went.
    pub fn withdraw_unconstructed_finalizable_noted(
        &self,
        obj_addr: usize,
        epoch_before_call: u64,
    ) -> bool {
        if self.mem.address_epoch() != epoch_before_call {
            return false;
        }
        self.mem
            .ref_processor
            .lock()
            .take_unconstructed_finalizable(obj_addr, epoch_before_call)
    }

    // `SharedVm::drain_finalizers` (M19) was deleted in gc-common w5-d
    // (2026-09-24, `common-w4d-legacy-m19-finalization-api-bypasses-every-fix`):
    // its one caller was `Vm::run_pending_finalizers`, which now runs the
    // production drain, and every reference pass (`process_references_after_gc`,
    // the G1 remark) already empties the processor's finalization queue itself
    // — with `enqueue_unfinalized`, not the address-keyed `enqueue` this used.

    // Drain the cleaner queue and return the addresses of cleaner actions
    // to execute.  The caller is responsible for actually running them.
    // Test-only: the sole caller is `m19_cleaner_actions_drained`. The
    // scanner could not see that until the region splitter stopped counting
    // braces inside string literals (2026-09-02).
    #[cfg(test)]
    pub fn drain_cleaners(&self) -> Vec<usize> {
        self.mem.cleaner_thread.drain_actions()
    }

    /// Run the reference processor's phases against `is_marked` (`true` for
    /// live objects) and funnel the results into the finalizer / cleaner
    /// queues — the bookkeeping half of a collection, for unit tests.
    ///
    /// Test-only since gc-common w5-d (2026-09-24,
    /// `common-w4d-legacy-m19-finalization-api-bypasses-every-fix`): it is NOT
    /// the VM's reference pass — no pre-collection referent null, no restore,
    /// no pointer map, no shape or identity screen — and a public method of
    /// that name next to the real one (`process_references_after_gc`, which
    /// every collection door runs) was a trap for an embedder. Finalizable
    /// objects are queued with the production `enqueue_unfinalized`.
    #[cfg(test)]
    pub fn process_references(
        &self,
        is_marked: &dyn Fn(usize) -> bool,
        free_heap_mb: usize,
        current_time_ms: u64,
    ) {
        let mut rp = self.mem.ref_processor.lock();
        let result = rp.process_references(is_marked, free_heap_mb, current_time_ms);

        // Enqueue objects needing finalization
        for obj_addr in &result.to_finalize {
            self.mem.finalizer_thread.enqueue_unfinalized(*obj_addr);
        }

        // Submit cleaner actions
        for action_addr in &result.cleaner_actions {
            self.mem.cleaner_thread.submit_action(*action_addr);
        }
    }
}

impl SharedVm {
    /// Record a deoptimization event to both the deopt log and the tiered
    /// compilation manager.
    pub fn record_deoptimization(
        &self,
        method_key: &str,
        event: crate::jit::deopt::DeoptEvent,
        tiered_key: &crate::jit::tiered::MethodKey,
    ) -> crate::jit::deopt::DeoptAction {
        let reason = event.reason;
        let bci = event.bci;
        let action = {
            let mut log = self.jit.deopt_log.lock();
            // The bci-aware policy: a method whose only failing speculation has
            // already been de-spec'd is recompiled, not blacklisted. See
            // `DeoptimizationLog::recommend_action_at_bci`.
            let action =
                log.recommend_action_at_bci(method_key, reason, bci, &self.jit.despec_registry);
            log.record_deopt(method_key, event);
            action
        };
        // Outside the log lock, and WITH the action: the manager charges a trap
        // only for an action that threw the body away, so a soft exit no
        // longer spends the method's C2 (and OSR) allowance.
        self.jit
            .tiered_manager
            .on_deoptimization(tiered_key, reason, bci, action);
        // j27 wave 5 (lane TRAPPROF, `CRATONVM_JIT_PROF_BRANCH_TRAP_BIT`,
        // opt-in): the bci's trap bit, set here, before the caller evicts the
        // body and re-queues the recompile, so that recompile cannot re-prune
        // the branch that trapped. A no-op with the switch off.
        self.jit.profile_store.note_trap(
            tiered_key.class_id.as_u32(),
            &tiered_key.method_name,
            &tiered_key.descriptor,
            reason,
            bci,
        );
        action
    }

    /// deopt-osr Step 9 — current live compilation epoch for `method_key`
    /// (`"<class>.<method>:<descriptor>"`), `0` if the method has never been
    /// invalidated. See [`Self::method_epochs`].
    pub fn compilation_epoch_for(&self, method_key: &str) -> u64 {
        self.jit
            .method_epochs
            .read()
            .get(method_key)
            .map(|c| c.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or_else(|| {
                self.jit
                    .method_epoch_overflow
                    .load(std::sync::atomic::Ordering::Relaxed)
            })
    }

    /// deopt-osr Step 9 — advance and return the live compilation epoch for
    /// `method_key`, marking every artifact compiled at an earlier epoch as
    /// superseded. Called on each invalidation (see
    /// `DeoptimizationController::deoptimize`). See [`Self::method_epochs`].
    pub fn bump_compilation_epoch(&self, method_key: &str) -> u64 {
        const METHOD_EPOCH_CAP: usize = 65_536;
        let mut map = self.jit.method_epochs.write();
        if !map.contains_key(method_key) && map.len() >= METHOD_EPOCH_CAP {
            return self
                .jit
                .method_epoch_overflow
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
        }
        let cell = map
            .entry(method_key.to_string())
            .or_insert_with(|| Box::new(std::sync::atomic::AtomicU64::new(0)));
        // fetch_add returns the PREVIOUS value; the new live epoch is +1.
        cell.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }

    /// deopt-osr Step 9 follow-up (a) — a STABLE process-lifetime pointer to
    /// `method_key`'s live compilation-epoch cell, creating the entry at epoch 0
    /// if absent. The cell is a boxed `AtomicU64`, so its address survives map
    /// rehashes and entries are never removed — the pointer is valid forever.
    /// Baked into the method's `DeoptEpochGuard` so `x64_deopt_entry` can read
    /// the live epoch lock-free before touching the deopt box. Only called under
    /// `deopt_real_enabled()` (at install, by `stamp_compilation_epoch`).
    pub fn live_epoch_cell_ptr(&self, method_key: &str) -> *const std::sync::atomic::AtomicU64 {
        const METHOD_EPOCH_CAP: usize = 65_536;
        let mut map = self.jit.method_epochs.write();
        if !map.contains_key(method_key) && map.len() >= METHOD_EPOCH_CAP {
            return &self.jit.method_epoch_overflow;
        }
        let cell = map
            .entry(method_key.to_string())
            .or_insert_with(|| Box::new(std::sync::atomic::AtomicU64::new(0)));
        cell.as_ref() as *const std::sync::atomic::AtomicU64
    }
}

