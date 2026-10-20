// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-class allocation-init cache for the JIT slow-path allocators.
//!
//! Every slow-path object allocation (`jit_new_object` and the guarded
//! TLAB-refill arm) used to pay a `class_manager.read()` + full superclass
//! hierarchy walk in `jit_init_primitive_fields`, plus a SECOND
//! `class_manager.read()` + class_store lookup for the `has_finalizer`
//! registration check. The compile-time resolver (`resolve_jit_new_site`)
//! already proves this metadata is stable by the time a `new` executes — it
//! bakes `(num_fields, has_primitive_init, has_finalizer)` into the emitted
//! code for the inline-TLAB fast path. This cache extends the same stability
//! assumption to the helper path: the first slow-path allocation of a class
//! computes a compact init recipe (primitive-field slots + finalizer flag)
//! under the read lock, publishes it lock-free, and every later allocation
//! of that class skips the class manager entirely.
//!
//! Layout: a two-level table indexed by `ClassId` (dense, allocator-issued —
//! same assumption as `cratonvm_types::CLASS_LAYOUTS`). The top level is a
//! fixed array of `AtomicPtr<Chunk>`; chunks hold `AtomicPtr<ClassAllocInfo>`
//! entries published with a `null -> ptr` CAS. Entries are immutable once
//! published and freed only on VM drop, so `get` can hand out `&` borrows
//! tied to the cache's lifetime — see [`JitAllocClassCache::invalidate`], which
//! unpublishes without freeing precisely to keep that sentence true. The cache
//! lives on `SharedVm` (NOT a process
//! global): it is populated lazily from `ClassManager` state, and a process
//! global would serve stale entries to a second VM in the same test process.
//!
//! Opt out with `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE=1` (presence check, like
//! `CRATONVM_NO_PRECISE_JIT_MAPS`): the helpers then fall back to the legacy
//! per-allocation class-manager lookups.
//!
//! # The interpreter's `new` reads it too (round i1 wave 3)
//!
//! `gc_and_alloc::gc_alloc_object` — the interpreter's `new` after the site
//! cache — used to take the class-manager read lock and walk the whole
//! superclass chain on EVERY allocation to write the JVM defaults and read
//! `has_finalizer`: an atomic RMW on a lock word every allocating thread
//! shares, and a wait behind any thread defining a class. It now reads the
//! same recipe (building and publishing it on a miss, through the one builder
//! [`ClassAllocInfo::build`]), so a warm interpreted `new` takes no lock. For
//! that the recipe also lists the REFERENCE slots (ClassAllocInfo::ref_inits):
//! both interpreter and JIT allocation helpers write the typed null defaults.
//! The JIT compile gate includes reference-only classes so this recipe is not
//! skipped. Since round i1 wave 5 both readers skip PrimKind::Int entries (a
//! zeroed body already decodes as Int(0)); the helper writes other primitive
//! kinds and reference defaults. The one weakness the recipe has and
//! # Per-VM state audit: BENIGN — already correct, do not re-litigate
//!
//! The 2026-08-01 `vm/src/jit/` cache-keying sweep
//! (`vm-jit-cache-keying.md`) checked this module and found
//! nothing to fix. The table is `ClassId`-keyed, which is per-VM state, but it
//! is NOT a process global: it is a by-value field of `JitRealm` inside
//! `Arc<SharedVm>` (`vm/src/vm/realms/jit_realm.rs` `jit_alloc_class_cache`,
//! constructed at `vm/src/vm/vm_init.rs`), so each VM has its own and the
//! `ClassId` index is unambiguous within it. The paragraph above says as much;
//! this note records that the claim was verified rather than assumed. It also
//! holds no `ObjectRef` and no heap address — only field indices, a `PrimKind`
//! and a `bool` — so it is not a GC-root concern either.

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

/// Primitive-field kind for default-value initialization (JLS §4.12.5).
/// `Int` covers all int-family descriptors (`I B C S Z`) — the interpreter's
/// `jit_init_primitive_fields` stores `Value::Int(0)` for all five.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimKind {
    Int,
    Long,
    Float,
    Double,
}

impl PrimKind {
    /// The kind of a field whose descriptor starts with `desc_first`, or
    /// `None` for a reference (and for any malformed byte, which
    /// `gc_and_alloc::jvm_default_for_descriptor` defaults to `null`).
    #[inline]
    pub fn of_descriptor(desc_first: u8) -> Option<Self> {
        match desc_first {
            b'I' | b'B' | b'C' | b'S' | b'Z' => Some(PrimKind::Int),
            b'J' => Some(PrimKind::Long),
            b'F' => Some(PrimKind::Float),
            b'D' => Some(PrimKind::Double),
            _ => None,
        }
    }

    /// The JVM default value of this kind (JVMS §2.3).
    #[inline]
    pub fn default_value(self) -> cratonvm_types::Value {
        use cratonvm_types::Value;
        match self {
            PrimKind::Int => Value::Int(0),
            PrimKind::Long => Value::Long(0),
            PrimKind::Float => Value::Float(0.0),
            PrimKind::Double => Value::Double(0.0),
        }
    }
}

/// Immutable per-class allocation-init recipe.
#[derive(Debug)]
pub struct ClassAllocInfo {
    /// Whether the class (or a superclass) overrides `finalize()` — gates
    /// `register_finalizable` (JLS §12.6).
    pub has_finalizer: bool,
    /// SBR3 lane `ref`: the `java.lang.ref` kind an instance registers at
    /// allocation (`crate::memory::ref_discovery`), as a code: `0` none, `1`
    /// soft, `2` weak, `3` phantom, `4` `jdk.internal.ref.Cleaner`. Immutable
    /// per class, so `note_allocation` reads it here instead of walking the
    /// superclass chain under the class-manager lock on every allocation.
    pub reference_kind: u8,
    /// `(absolute field index, kind)` of every non-static primitive field in
    /// the hierarchy, in the same walk order as `jit_init_primitive_fields`.
    /// Empty when the class needs no primitive default-init.
    pub prim_inits: Box<[(u32, PrimKind)]>,
    /// Absolute field index of every non-static reference, array, or
    /// malformed descriptor field in the hierarchy. Its JVM default must be
    /// written as Object(None) because a zeroed legacy slot decodes as Int(0).
    /// Both interpreter and JIT allocation helpers write these slots, and the
    /// JIT compile gate includes reference-only classes so this recipe is used.
    pub ref_inits: Box<[u32]>,
}

impl ClassAllocInfo {
    /// Build the recipe for `class_id` from `store`: the superclass-chain walk
    /// of `gc_and_alloc::write_default_instance_fields` and
    /// `jit_init_primitive_fields`, split by default kind. `None` — publish
    /// nothing, take the caller's locked walk — when `class_id` or any class
    /// in its superclass chain is missing from the store, so only a COMPLETE
    /// recipe is ever cached.
    ///
    /// The one builder for both readers, so the interpreter's and the JIT's
    /// view of a class's defaults cannot drift apart.
    ///
    /// # A compatibility stub in the chain is never cached (gc-common w9-f)
    ///
    /// Also `None` when `class_id` or any superclass is a
    /// `ClassOrigin::CompatibilityStub`. `upgrade_synthetic_class` rewrites a
    /// stub's `fields` IN PLACE when its real bytes arrive, and a recipe
    /// published before that keeps the stub's descriptors. The invalidation
    /// hook reaches the upgraded class itself and every descendant whose
    /// `first_field_index` / `num_total_fields` moved, but NOT a descendant
    /// whose layout numbers stayed put: the synthetic field-count floor
    /// (`apply_synthetic_floor`) keeps the parent's total, so its subclasses
    /// are never recomputed, while the parent's slots change kind
    /// (`J` -> `L`, say). Such a subclass then kept writing the stub's
    /// defaults -- a `Long(0)` into a reference slot, or no store at all where
    /// the real field needs the `Object(None)` tag -- on every `new`, forever.
    /// The classic locked walk reads the live fields each time, so it is always
    /// right; a stub chain takes it, as the interpreter's (since retired)
    /// per-thread plan cache already refused to plan one. No
    /// class in `--jdk-only` has a stub in its chain, so the refusal costs
    /// nothing there; in `--compatible` it costs one class-manager read per
    /// allocation of a stub-descended class, the pre-cache behaviour.
    pub fn build(
        store: &crate::classloading::ClassStore,
        class_id: crate::classloading::ClassId,
    ) -> Option<Self> {
        let root = store.get(class_id)?;
        let has_finalizer = root.has_finalizer;
        // Pass 1, allocation-free: refuse before building anything. A refused
        // class is never published, so EVERY `new` of it asks again (the
        // interpreter's `gc_alloc_object` and the JIT helper both do); this
        // pass keeps that repeated refusal from allocating and freeing the
        // partial lists on each of those allocations (round i1 wave 16). It
        // also counts the instance fields, so pass 2 allocates each list once.
        let mut prim_count = 0usize;
        let mut ref_count = 0usize;
        let mut cid = Some(class_id);
        let mut depth = 0usize;
        while let Some(current_id) = cid {
            let class = store.get(current_id)?;
            // See the doc above: a stub's layout can change under a published
            // recipe without anything unpublishing it.
            if class.origin.is_compatibility_stub() {
                return None;
            }
            // A malformed (cyclic) superclass chain must not hang an
            // allocation; the locked walk it replaces is the fallback.
            depth += 1;
            if depth > 4096 {
                return None;
            }
            for f in class.fields.iter().filter(|f| !f.is_static()) {
                let desc_first = f.descriptor.as_bytes().first().copied().unwrap_or(b'L');
                if PrimKind::of_descriptor(desc_first).is_some() {
                    prim_count += 1;
                } else {
                    ref_count += 1;
                }
            }
            cid = class.superclass;
        }
        // Pass 2: the same chain (the store is borrowed immutably throughout,
        // so it cannot have changed), filled in walk order.
        let mut prim_inits: Vec<(u32, PrimKind)> = Vec::with_capacity(prim_count);
        let mut ref_inits: Vec<u32> = Vec::with_capacity(ref_count);
        let mut cid = Some(class_id);
        while let Some(current_id) = cid {
            let class = store.get(current_id)?;
            let mut inst_idx = class.first_field_index;
            for f in &class.fields {
                if f.is_static() {
                    continue;
                }
                // Truncation-checked: field indices are bounded by
                // `num_total_fields`, far below u32::MAX.
                let idx = u32::try_from(inst_idx).ok()?;
                let desc_first = f.descriptor.as_bytes().first().copied().unwrap_or(b'L');
                match PrimKind::of_descriptor(desc_first) {
                    Some(kind) => prim_inits.push((idx, kind)),
                    None => ref_inits.push(idx),
                }
                inst_idx += 1;
            }
            cid = class.superclass;
        }
        Some(ClassAllocInfo {
            has_finalizer,
            reference_kind: crate::memory::ref_discovery::reference_kind_code_in(store, class_id),
            prim_inits: prim_inits.into_boxed_slice(),
            ref_inits: ref_inits.into_boxed_slice(),
        })
    }
}

/// Entries per chunk / chunks in the top level. `TOP_LEN * CHUNK_LEN` covers
/// the same id range as `cratonvm_types::MAX_DENSE_CLASS_LAYOUTS` (1<<20);
/// ids beyond that fall back to the uncached path instead of allocating
/// sparsely.
const CHUNK_LEN: usize = 1024;
const TOP_LEN: usize = 1024;

struct Chunk {
    entries: [AtomicPtr<ClassAllocInfo>; CHUNK_LEN],
}

impl Chunk {
    fn new_boxed() -> Box<Chunk> {
        Box::new(Chunk {
            entries: [const { AtomicPtr::new(std::ptr::null_mut()) }; CHUNK_LEN],
        })
    }
}

/// gcg g1/p — how often a COMPILED `new` reached one of the two allocation
/// helpers, per VM.
///
/// # Why this exists
///
/// `gcf-f2p-compiled-new-of-a-compact-class-with-long-fields-calls-the-allocation-helper`
/// read its engagement off the planner's `TLAB object shapes: compact=` count,
/// and vf2's positive control showed `compact=46991` on BOTH arms of an
/// eight-million-`new` row. That counter is bumped only by the Rust planner
/// (`plan_tlab_object_shape_at`): the interpreter's `new` and `jit_new_object`.
/// A compiled `new` that takes the inline bump never calls the planner -- not
/// even when the bump is followed by `jit_post_tlab_init` (the old rule's
/// shape on the optimizing tier, whose `Op::New` always bumps inline) -- so
/// the counter could not tell the old rule from the new one.
///
/// These two counters can. A compiled `new` ends in exactly one of three
/// places: the full helper (`jit_new_object`, also the inline bump's TLAB-miss
/// slow path), the inline bump followed by `jit_post_tlab_init`, or the inline
/// bump with no call at all. The first two are counted here, at the helpers'
/// entries; the third is the remainder of the row's known allocation count.
/// The old rule (`CRATONVM_JIT_COMPACT_NEW_ZERO_PRIM=0`) puts every `new` of a
/// compact class with a `long`/`float`/`double` field in one of the counted
/// two (the positive control); the new rule takes it out of both.
///
/// Per VM (a field of [`JitAllocClassCache`], which `JitRealm` owns), never a
/// process `static`. Counted only under `CRATONVM_DBG_JIT_METHOD_STATS`, the
/// switch every other allocation-shape census in the exit report uses, so a
/// default run pays one cached-flag load per helper entry.
#[derive(Debug, Default)]
pub struct JitNewPathCounts {
    /// Entries to `jit_new_object` (the full helper, and the inline bump's
    /// TLAB-miss slow path).
    new_object: AtomicU64,
    /// Entries to `jit_post_tlab_init` (an inline bump that still owed
    /// primitive defaults or a finalizer registration).
    post_tlab_init: AtomicU64,
    /// zr z3/ja: entries to `jit_anewarray_object` -- the positive control of
    /// the ZGC inline reference-array bump (`CRATONVM_JIT_INLINE_TLAB_ANEWARRAY`).
    anewarray: AtomicU64,
    /// zr z3/ja: the subset of [`Self::anewarray`] whose array fits the inline
    /// cap (`tlab_max_alloc()`, 32 KiB) and has a non-negative length: with the
    /// bump on, only its TLAB-miss slow path (one per refill) and sites that
    /// declined it; with `=0`, every small `Object[]`.
    anewarray_small: AtomicU64,
    /// JIT round 21 wave 1 (lane alloc, `R21AllocLean`): entries to
    /// `jit_newarray` (a compiled PRIMITIVE array: the inline bump's slow
    /// path, a site that declined the bump, or an array above the cap).
    newarray: AtomicU64,
    /// r21 alloc: the subset of [`Self::newarray`] with a non-negative length
    /// whose footprint fits the inline cap (`tlab_max_alloc()`): with the bump
    /// engaged, one per TLAB refill; a count near the row's allocation count
    /// means the bump is not engaging (or the TLAB misses on every array).
    newarray_small: AtomicU64,
    /// r21 alloc: the subset of [`Self::newarray`] the helper served from
    /// this thread's TLAB (`tlab_alloc_array_guarded_refill`); the rest of a
    /// small entry spilled to the locked per-object path (a refused refill).
    newarray_tlab: AtomicU64,
    /// r21 alloc: entries to `jit_zgc_note_tlab_object` (ZGC: an inline bump
    /// whose inline start-bit store refused -- table unpublished or blocked,
    /// epoch moved, granule not owned -- or a site emitted without the inline
    /// store). Near the row's allocation count = a Rust call per allocation.
    zgc_note: AtomicU64,
    /// JIT r21 wave 2 (lane alloc): set by the first
    /// [`take_compiled_new_paths_line_for`], so the two exit arms (normal
    /// return and `System.exit`) print the line once per VM.
    line_printed: std::sync::atomic::AtomicBool,
}

impl JitNewPathCounts {
    /// Count one `jit_new_object` entry when `armed`.
    #[inline]
    pub fn note_new_object_if(&self, armed: bool) {
        if armed {
            self.new_object.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Count one `jit_post_tlab_init` entry when `armed`.
    #[inline]
    pub fn note_post_tlab_init_if(&self, armed: bool) {
        if armed {
            self.post_tlab_init.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Count one `jit_new_object` entry under the production switch.
    #[inline]
    pub fn note_new_object(&self) {
        self.note_new_object_if(compiled_new_path_counting_enabled());
    }

    /// Count one `jit_post_tlab_init` entry under the production switch.
    #[inline]
    pub fn note_post_tlab_init(&self) {
        self.note_post_tlab_init_if(compiled_new_path_counting_enabled());
    }

    /// zr z3/ja: count one `jit_anewarray_object` entry when `armed`; `small`
    /// when the array fits the inline bump's cap.
    #[inline]
    pub fn note_anewarray_if(&self, armed: bool, small: bool) {
        if armed {
            self.anewarray.fetch_add(1, Ordering::Relaxed);
            if small {
                self.anewarray_small.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// zr z3/ja: [`Self::note_anewarray_if`] under the production switch.
    #[inline]
    pub fn note_anewarray(&self, small: bool) {
        self.note_anewarray_if(compiled_new_path_counting_enabled(), small);
    }

    /// zr z3/ja: `(jit_anewarray_object entries, of which small)`.
    pub fn anewarray_snapshot(&self) -> (u64, u64) {
        (
            self.anewarray.load(Ordering::Relaxed),
            self.anewarray_small.load(Ordering::Relaxed),
        )
    }

    /// r21 alloc: count one `jit_newarray` entry when `armed`; `small` when
    /// the array fits the inline bump's cap with a non-negative length.
    #[inline]
    pub fn note_newarray_if(&self, armed: bool, small: bool) {
        if armed {
            self.newarray.fetch_add(1, Ordering::Relaxed);
            if small {
                self.newarray_small.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// r21 alloc: [`Self::note_newarray_if`] under the production switch.
    #[inline]
    pub fn note_newarray(&self, small: bool) {
        self.note_newarray_if(compiled_new_path_counting_enabled(), small);
    }

    /// r21 alloc: count one `jit_newarray` entry served from the TLAB.
    #[inline]
    pub fn note_newarray_tlab_if(&self, armed: bool) {
        if armed {
            self.newarray_tlab.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// r21 alloc: [`Self::note_newarray_tlab_if`] under the production switch.
    #[inline]
    pub fn note_newarray_tlab(&self) {
        self.note_newarray_tlab_if(compiled_new_path_counting_enabled());
    }

    /// r21 alloc: count one `jit_zgc_note_tlab_object` entry when `armed`.
    #[inline]
    pub fn note_zgc_note_if(&self, armed: bool) {
        if armed {
            self.zgc_note.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// r21 alloc: [`Self::note_zgc_note_if`] under the production switch.
    #[inline]
    pub fn note_zgc_note(&self) {
        self.note_zgc_note_if(compiled_new_path_counting_enabled());
    }

    /// r21 alloc: `(jit_newarray entries, of which small, of which served
    /// from the TLAB, jit_zgc_note_tlab_object entries)`.
    pub fn newarray_snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.newarray.load(Ordering::Relaxed),
            self.newarray_small.load(Ordering::Relaxed),
            self.newarray_tlab.load(Ordering::Relaxed),
            self.zgc_note.load(Ordering::Relaxed),
        )
    }

    /// `(jit_new_object entries, jit_post_tlab_init entries)`.
    pub fn snapshot(&self) -> (u64, u64) {
        (
            self.new_object.load(Ordering::Relaxed),
            self.post_tlab_init.load(Ordering::Relaxed),
        )
    }
}

/// Whether [`JitNewPathCounts`] counts on this run:
/// `CRATONVM_DBG_JIT_METHOD_STATS` (`flags().jit.method_stats`, a cached
/// struct read).
#[inline]
pub fn compiled_new_path_counting_enabled() -> bool {
    cratonvm_types::flags().jit.method_stats
}

/// The exit-report line for [`JitNewPathCounts`], or `None` when the counters
/// were not armed (a `0` from an unarmed counter must never be printed as a
/// reading). Keys are `jit_new_object_calls=` and `jit_post_tlab_init_calls=`.
pub fn compiled_new_paths_line(counts: &JitNewPathCounts) -> Option<String> {
    compiled_new_path_counting_enabled().then(|| {
        format!(
            "{}{}{}",
            format_compiled_new_paths_line(counts.snapshot()),
            format_compiled_anewarray_keys(counts.anewarray_snapshot()),
            format_compiled_newarray_keys(counts.newarray_snapshot())
        )
    })
}

/// JIT round 21 wave 1 (lane alloc): the primitive-array and ZGC-announce keys
/// appended to the compiled-new line: `jit_newarray_calls=`,
/// `jit_newarray_small_calls=`, `jit_newarray_tlab_served=` and
/// `jit_zgc_note_calls=`. A small primitive array counted in none took the
/// inline bump with no call (and, on ZGC, the inline start-bit store).
fn format_compiled_newarray_keys(
    (newarray, small, tlab, zgc_note): (u64, u64, u64, u64),
) -> String {
    format!(
        " jit_newarray_calls={newarray} jit_newarray_small_calls={small} \
         jit_newarray_tlab_served={tlab} jit_zgc_note_calls={zgc_note}"
    )
}

/// zr z3/ja: the `anewarray` keys appended to the compiled-new line:
/// `jit_anewarray_calls=` and `jit_anewarray_small_calls=`. A small
/// `Object[]` counted in neither took the ZGC inline bump.
fn format_compiled_anewarray_keys((anewarray, small): (u64, u64)) -> String {
    format!(" jit_anewarray_calls={anewarray} jit_anewarray_small_calls={small}")
}

/// [`compiled_new_paths_line`] for one VM: the shape `vm-cli`'s two shutdown
/// arms call, next to `oome_ladder_doors_line(&vm)`.
pub fn compiled_new_paths_line_for(vm: &crate::vm::SharedVm) -> Option<String> {
    compiled_new_paths_line(&vm.jit.jit_alloc_class_cache.new_paths)
}

/// JIT r21 wave 2 (lane alloc): [`compiled_new_paths_line_for`], claimed once
/// per VM. The exit arms print this under `CRATONVM_DBG_JIT_METHOD_STATS`
/// alone; it used to be printed only inside the GC-stats block.
pub fn take_compiled_new_paths_line_for(vm: &crate::vm::SharedVm) -> Option<String> {
    // r21 alloc wave 2: with this VM's TLAB refill total, so a one-row run of
    // `R21AllocLean <row>` reads helper entries per refill.
    take_compiled_new_paths_line(&vm.jit.jit_alloc_class_cache.new_paths).map(|line| {
        format!(
            "{line} tlab_refills={}",
            vm.mem.tlab_refill_count.load(Ordering::Relaxed)
        )
    })
}

/// [`take_compiled_new_paths_line_for`] on the counters themselves.
pub fn take_compiled_new_paths_line(counts: &JitNewPathCounts) -> Option<String> {
    let line = compiled_new_paths_line(counts)?;
    (!counts.line_printed.swap(true, Ordering::Relaxed)).then_some(line)
}

/// The line's text, split out so a test can pin the keys without arming the
/// process switch.
fn format_compiled_new_paths_line((new_object, post_tlab_init): (u64, u64)) -> String {
    format!(
        "[cratonvm] compiled-new helper entries: jit_new_object_calls={new_object} \
         jit_post_tlab_init_calls={post_tlab_init} (a compiled new counted in neither \
         took the inline bump with no call)"
    )
}

/// Lock-free `ClassId -> ClassAllocInfo` side table (see module docs).
pub struct JitAllocClassCache {
    /// gcg g1/p — compiled-`new` helper entries; see [`JitNewPathCounts`].
    pub new_paths: JitNewPathCounts,
    chunks: Box<[AtomicPtr<Chunk>; TOP_LEN]>,
    /// Entries unpublished by [`Self::invalidate`], held until `Drop`.
    ///
    /// Keeps "published entries are freed only on VM drop" — the invariant
    /// [`Self::get`]'s borrow depends on — literally true. Touched only by
    /// [`Self::invalidate`] (class unload and layout-change invalidation), so
    /// the mutex is never taken on an allocation.
    retired: std::sync::Mutex<Vec<*mut ClassAllocInfo>>,
    /// gcg g6/jm: `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE` as THIS VM read it at
    /// construction ([`Self::enabled`]). It replaced the process `OnceLock`
    /// behind [`alloc_class_cache_enabled`], which answered every VM in the
    /// process with the first one's reading.
    enabled: bool,
}

// SAFETY: `retired` holds `*mut ClassAllocInfo` only as deferred-free
// bookkeeping — the pointers are never dereferenced while in the vector, and
// `ClassAllocInfo` is `Send + Sync` by construction (a `bool` and a boxed slice
// of `Copy` scalars). The rest of the cache is already shared across threads
// through atomics.
unsafe impl Send for JitAllocClassCache {}
unsafe impl Sync for JitAllocClassCache {}

impl Default for JitAllocClassCache {
    fn default() -> Self {
        Self::new()
    }
}

impl JitAllocClassCache {
    pub fn new() -> Self {
        JitAllocClassCache {
            new_paths: JitNewPathCounts::default(),
            chunks: Box::new([const { AtomicPtr::new(std::ptr::null_mut()) }; TOP_LEN]),
            retired: std::sync::Mutex::new(Vec::new()),
            enabled: !cratonvm_types::flags::runtime_flag_on("CRATONVM_NO_JIT_ALLOC_CLASS_CACHE"),
        }
    }

    /// Whether this VM uses the cache (`CRATONVM_NO_JIT_ALLOC_CLASS_CACHE=1`
    /// opts out), read once at construction. Every production reader asks
    /// this, per VM; [`alloc_class_cache_enabled`] is the flag-reading form for
    /// code with no VM in hand.
    #[inline]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Cached recipe for `class_id`, or `None` if not yet published (or the
    /// id is outside the dense range).
    #[inline]
    pub fn get(&self, class_id: u32) -> Option<&ClassAllocInfo> {
        let idx = class_id as usize;
        let chunk = self.chunks.get(idx / CHUNK_LEN)?.load(Ordering::Acquire);
        if chunk.is_null() {
            return None;
        }
        // SAFETY: a non-null chunk pointer was published by `insert` via a
        // Release CAS and is only freed in `Drop` (which takes `&mut self`,
        // so no `&self` borrow can be live).
        let entry = unsafe { &(*chunk).entries[idx % CHUNK_LEN] }.load(Ordering::Acquire);
        if entry.is_null() {
            None
        } else {
            // SAFETY: same publication/immutability argument as the chunk.
            Some(unsafe { &*entry })
        }
    }

    /// Publish the recipe for `class_id` (first writer wins; racing inserts
    /// return the winner's entry). Returns `None` — without publishing —
    /// when `class_id` is outside the dense range.
    pub fn insert(&self, class_id: u32, info: ClassAllocInfo) -> Option<&ClassAllocInfo> {
        let idx = class_id as usize;
        let chunk_slot = self.chunks.get(idx / CHUNK_LEN)?;
        let mut chunk = chunk_slot.load(Ordering::Acquire);
        if chunk.is_null() {
            let fresh = Box::into_raw(Chunk::new_boxed());
            match chunk_slot.compare_exchange(
                std::ptr::null_mut(),
                fresh,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => chunk = fresh,
                Err(winner) => {
                    // SAFETY: `fresh` was just created above, never published.
                    drop(unsafe { Box::from_raw(fresh) });
                    chunk = winner;
                }
            }
        }
        // SAFETY: chunk is non-null and published (see `get`).
        let entry_slot = unsafe { &(*chunk).entries[idx % CHUNK_LEN] };
        let fresh = Box::into_raw(Box::new(info));
        match entry_slot.compare_exchange(
            std::ptr::null_mut(),
            fresh,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            // SAFETY: we just published `fresh`; it stays live until Drop.
            Ok(_) => Some(unsafe { &*fresh }),
            Err(winner) => {
                // SAFETY: `fresh` lost the race and was never published.
                drop(unsafe { Box::from_raw(fresh) });
                // SAFETY: `winner` is a published entry (see `get`).
                Some(unsafe { &*winner })
            }
        }
    }

    /// Unpublish the recipe for a class whose field layout may have changed.
    ///
    /// # Who calls this, and why the answer matters
    ///
    /// There are two callers:
    ///
    /// * `jit_invalidate_adapter` (`vm/src/vm/vm_init.rs`), the `ClassInfoHook`
    ///   the GC fires on an **in-place layout change**: a class redefinition, a
    ///   subclass re-layout, or a synthetic stub being replaced by real bytecode
    ///   with a different field count or order. It fans out over every live
    ///   VM, because the hook carries no VM identity.
    /// * the class-UNLOAD path in `vm/src/memory/gc.rs`, once per unloaded
    ///   class. `ClassId`s are NOT recycled today (`ClassStore::add` appends,
    ///   `ClassStore::remove` leaves a tombstone), so this call only releases
    ///   the dead class's recipe; it becomes load-bearing if ids are ever
    ///   reused, when the next class issued the id would otherwise inherit the
    ///   dead class's primitive-field recipe and `has_finalizer` bit.
    ///
    /// (This paragraph used to say the adapter was the ONLY caller; the unload
    /// call was not listed. It also said ids were recycled, which they are not.)
    ///
    /// This comment used to say "called only from the stop-the-world
    /// loader-unload transaction, after reachability proved that no instance or
    /// activation of the class remains". Both halves were wrong about the
    /// caller that exists, and the second half was the load-bearing one: on a
    /// redefinition, instances and activations of the class emphatically DO
    /// remain — that is the whole point of a redefinition — so no argument that
    /// rests on their absence may be made here. Nothing below rests on one; the
    /// deferred free is what makes this sound, and it is sound for either
    /// caller.
    ///
    /// The contract for the caller is therefore just this: after this returns,
    /// the next slow-path allocation of `class_id` rebuilds the recipe from the
    /// current layout. An allocation already past its `get()` keeps the recipe
    /// it read, which for a layout change means primitive default-init at the
    /// OLD field indices of a freshly zeroed object — bounded by `set_field`'s
    /// own range check, and a no-op for any index the new layout still has,
    /// since the object's slots are already zero.
    ///
    /// # Why this does not free the entry
    ///
    /// [`Self::get`] returns `&ClassAllocInfo` borrowed from `&self`, and its
    /// SAFETY comment justifies that with "entries ... are only freed in `Drop`
    /// (which takes `&mut self`)". This method used to `drop(Box::from_raw(..))`
    /// immediately, which made that sentence false and the borrow a
    /// use-after-free — the reader holds a plain `&`, so nothing at the type
    /// level connects it to the swap here.
    ///
    /// No stop-the-world argument rescues it, and the actual caller does not
    /// even offer one: `jit_invalidate_adapter` runs on whatever thread the
    /// `ClassInfoHook` fires on, with other threads running. Even under a real
    /// STW the guarantee would be about *instances and activations of the
    /// class*, not about which line of `jit_post_alloc_init` some other thread
    /// is parked on; and the collector reaches its stop-the-world state partly
    /// by FORCIBLY freezing in-JIT peers (`stw_take_over_and_wait`), which stops
    /// a thread at an arbitrary instruction — including between `get()` and the
    /// `info.prim_inits` walk it feeds. Such a thread resumes and reads a freed
    /// `Box<[(u32, PrimKind)]>`: garbage field indices into `set_field` (dropped
    /// by its bounds check, but only after the allocator has reused the memory)
    /// and a garbage `has_finalizer` that can register an arbitrary object as
    /// finalizable.
    ///
    /// So the entry is unpublished (no later `get` can find it, which is all
    /// invalidation is FOR) and moved to `retired`, which `Drop` reclaims. The
    /// leak is one recipe per invalidation — a `bool` plus a boxed slice of
    /// `(u32, PrimKind)` — bounded by layout changes actually performed, and
    /// paid only on a path that already evicts the whole JIT method cache.
    pub fn invalidate(&self, class_id: u32) -> bool {
        let idx = class_id as usize;
        let Some(chunk_slot) = self.chunks.get(idx / CHUNK_LEN) else {
            return false;
        };
        let chunk = chunk_slot.load(Ordering::Acquire);
        if chunk.is_null() {
            return false;
        }
        // SAFETY: published chunks live until cache Drop.
        let entry = unsafe { &(*chunk).entries[idx % CHUNK_LEN] }
            .swap(std::ptr::null_mut(), Ordering::AcqRel);
        if entry.is_null() {
            false
        } else {
            // Ownership moves to `retired`, NOT to a `drop` here: a concurrent
            // (or forcibly-frozen) reader may still hold a `&` to it. See the
            // doc comment above.
            self.retired
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(entry);
            true
        }
    }
}

impl Drop for JitAllocClassCache {
    fn drop(&mut self) {
        for chunk_slot in self.chunks.iter() {
            let chunk = chunk_slot.swap(std::ptr::null_mut(), Ordering::AcqRel);
            if chunk.is_null() {
                continue;
            }
            // SAFETY: exclusive access in Drop; pointers came from Box::into_raw.
            let chunk = unsafe { Box::from_raw(chunk) };
            for entry_slot in chunk.entries.iter() {
                let entry = entry_slot.swap(std::ptr::null_mut(), Ordering::AcqRel);
                if !entry.is_null() {
                    // SAFETY: as above.
                    drop(unsafe { Box::from_raw(entry) });
                }
            }
        }
        // The deferred-free list from `invalidate`. `&mut self` here means no
        // `get` borrow can still be live, which is exactly the condition that
        // was missing at the `invalidate` call site.
        for entry in self
            .retired
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            // SAFETY: unpublished by `invalidate` (so unreachable via `get`)
            // and owned by this list ever since; pointer came from
            // `Box::into_raw` in `insert`.
            drop(unsafe { Box::from_raw(entry) });
        }
    }
}

/// Whether the cache is enabled.
///
/// Enabled by default. Opt out with `CRATONVM_NO_JIT_ALLOC_CLASS_CACHE=1`.
/// Fresh isolated bintrees measurements show that removing the repeated class
/// manager lookup materially complements the inline allocation path; cache
/// entries are immutable and scoped to a `SharedVm`, so this does not weaken
/// allocation or class-lifetime correctness.
///
/// gcg g6/jm: no longer a process `OnceLock` (the first VM's reading answered
/// every VM, and a test override after the first read was ignored). This form
/// reads the flag on every call; production code asks the VM's own cache,
/// [`JitAllocClassCache::enabled`], which read it once.
#[inline]
pub fn alloc_class_cache_enabled() -> bool {
    !cratonvm_types::flags::runtime_flag_on("CRATONVM_NO_JIT_ALLOC_CLASS_CACHE")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// gcg g6/jm: each cache reads the opt-out when it is built, so two VMs
    /// built under different settings answer differently (the process
    /// `OnceLock` answered both with the first reading).
    #[test]
    fn gcg_g6jm_each_cache_reads_its_own_opt_out() {
        let on = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_NO_JIT_ALLOC_CLASS_CACHE", None)],
            JitAllocClassCache::new,
        );
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_NO_JIT_ALLOC_CLASS_CACHE", Some("1"))],
            JitAllocClassCache::new,
        );
        assert!(on.enabled());
        assert!(!off.enabled());
    }

    #[test]
    fn get_empty_returns_none() {
        let cache = JitAllocClassCache::new();
        assert!(cache.get(0).is_none());
        assert!(cache.get(12345).is_none());
        assert!(cache.get(u32::MAX).is_none());
    }

    #[test]
    fn insert_then_get_roundtrip() {
        let cache = JitAllocClassCache::new();
        let info = ClassAllocInfo {
            has_finalizer: true,
            reference_kind: 0,
            prim_inits: vec![(3, PrimKind::Int), (7, PrimKind::Long)].into_boxed_slice(),
            ref_inits: Box::new([]),
        };
        let published = cache.insert(42, info).unwrap();
        assert!(published.has_finalizer);
        assert_eq!(
            published.prim_inits.as_ref(),
            &[(3, PrimKind::Int), (7, PrimKind::Long)]
        );
        let got = cache.get(42).unwrap();
        assert!(got.has_finalizer);
        assert_eq!(got.prim_inits.len(), 2);
        // Neighbors untouched.
        assert!(cache.get(41).is_none());
        assert!(cache.get(43).is_none());
    }

    #[test]
    fn first_insert_wins() {
        let cache = JitAllocClassCache::new();
        let a = ClassAllocInfo {
            has_finalizer: false,
            reference_kind: 0,
            prim_inits: Box::new([]),
            ref_inits: Box::new([]),
        };
        let b = ClassAllocInfo {
            has_finalizer: true,
            reference_kind: 0,
            prim_inits: Box::new([(1, PrimKind::Float)]),
            ref_inits: Box::new([]),
        };
        cache.insert(7, a);
        let winner = cache.insert(7, b).unwrap();
        assert!(!winner.has_finalizer, "first published entry must win");
        assert!(cache.get(7).unwrap().prim_inits.is_empty());
    }

    #[test]
    fn out_of_range_id_is_uncached() {
        let cache = JitAllocClassCache::new();
        let info = ClassAllocInfo {
            has_finalizer: false,
            reference_kind: 0,
            prim_inits: Box::new([]),
            ref_inits: Box::new([]),
        };
        assert!(cache.insert(u32::MAX, info).is_none());
        assert!(cache.get(u32::MAX).is_none());
    }

    #[test]
    fn cross_chunk_ids() {
        let cache = JitAllocClassCache::new();
        for id in [0u32, 1023, 1024, 1025, 999_999] {
            let info = ClassAllocInfo {
                has_finalizer: id % 2 == 0,
                reference_kind: 0,
                prim_inits: Box::new([(id, PrimKind::Double)]),
                ref_inits: Box::new([]),
            };
            cache.insert(id, info);
        }
        for id in [0u32, 1023, 1024, 1025, 999_999] {
            let got = cache.get(id).unwrap();
            assert_eq!(got.has_finalizer, id % 2 == 0);
            assert_eq!(got.prim_inits.as_ref(), &[(id, PrimKind::Double)]);
        }
    }

    /// `invalidate` must unpublish WITHOUT freeing: a reader that already has
    /// the `&` — a thread frozen between `get()` and its `prim_inits` walk — is
    /// unreachable from here and must not have its recipe reclaimed underneath.
    ///
    /// This asserts on the entry's CONTENT after the invalidate, so it fails
    /// (loudly under Miri/ASan, and by mismatch once the allocator reuses the
    /// block) if the entry is freed rather than retired. It also fixes the
    /// direction of the other half: `get` must report the class as gone.
    #[test]
    fn invalidate_unpublishes_but_does_not_free_a_live_borrow() {
        let cache = JitAllocClassCache::new();
        cache.insert(
            9,
            ClassAllocInfo {
                has_finalizer: true,
                reference_kind: 0,
                prim_inits: vec![(11, PrimKind::Long), (12, PrimKind::Double)].into_boxed_slice(),
                ref_inits: Box::new([]),
            },
        );
        // Stand in for the frozen reader: take the borrow BEFORE invalidating.
        let borrowed = cache.get(9).expect("published");
        assert!(
            cache.invalidate(9),
            "entry was published, so it unpublishes"
        );
        assert!(cache.get(9).is_none(), "invalidate must unpublish");
        assert!(borrowed.has_finalizer);
        assert_eq!(
            borrowed.prim_inits.as_ref(),
            &[(11, PrimKind::Long), (12, PrimKind::Double)],
            "a borrow taken before invalidate must still read its own recipe",
        );
    }

    #[test]
    fn invalidate_is_idempotent_and_reinsertable() {
        let cache = JitAllocClassCache::new();
        cache.insert(
            5,
            ClassAllocInfo {
                has_finalizer: false,
                reference_kind: 0,
                prim_inits: Box::new([(1, PrimKind::Int)]),
                ref_inits: Box::new([]),
            },
        );
        assert!(cache.invalidate(5));
        assert!(
            !cache.invalidate(5),
            "second invalidate has nothing to take"
        );
        // The id is free again: a reloaded class with the same id must be able
        // to publish a fresh recipe rather than inherit the retired one.
        let fresh = cache
            .insert(
                5,
                ClassAllocInfo {
                    has_finalizer: true,
                    reference_kind: 0,
                    prim_inits: Box::new([(2, PrimKind::Float)]),
                    ref_inits: Box::new([]),
                },
            )
            .expect("slot is empty again");
        assert!(fresh.has_finalizer);
        assert_eq!(
            cache.get(5).unwrap().prim_inits.as_ref(),
            &[(2, PrimKind::Float)]
        );
    }

    #[test]
    fn concurrent_insert_get_smoke() {
        use std::sync::Arc;
        let cache = Arc::new(JitAllocClassCache::new());
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let cache = Arc::clone(&cache);
                std::thread::spawn(move || {
                    for i in 0..2000u32 {
                        let id = i % 512;
                        if let Some(info) = cache.get(id) {
                            // Whoever won, the entry must be self-consistent.
                            assert_eq!(info.prim_inits.len(), 1);
                            assert_eq!(info.prim_inits[0].0, id);
                        } else {
                            let _ = cache.insert(
                                id,
                                ClassAllocInfo {
                                    has_finalizer: t % 2 == 0,
                                    reference_kind: 0,
                                    prim_inits: Box::new([(id, PrimKind::Int)]),
                                    ref_inits: Box::new([]),
                                },
                            );
                        }
                    }
                })
            })
            .collect();
        for th in threads {
            th.join().unwrap();
        }
        for id in 0..512u32 {
            assert_eq!(cache.get(id).unwrap().prim_inits[0].0, id);
        }
    }

    /// gcg g1/p: the compiled-`new` path counters count only when armed, keep
    /// the two helpers apart, and belong to ONE cache (a second VM's cache
    /// sees none of the first's entries).
    #[test]
    fn gcg_g1p_new_path_counts_are_armed_per_helper_and_per_cache() {
        let a = JitAllocClassCache::new();
        let b = JitAllocClassCache::new();
        assert_eq!(a.new_paths.snapshot(), (0, 0));
        // Unarmed: nothing moves.
        a.new_paths.note_new_object_if(false);
        a.new_paths.note_post_tlab_init_if(false);
        assert_eq!(a.new_paths.snapshot(), (0, 0));
        // Armed: each helper its own counter.
        for _ in 0..3 {
            a.new_paths.note_new_object_if(true);
        }
        for _ in 0..5 {
            a.new_paths.note_post_tlab_init_if(true);
        }
        assert_eq!(a.new_paths.snapshot(), (3, 5));
        assert_eq!(b.new_paths.snapshot(), (0, 0), "per VM, not per process");
    }

    /// gcg g1/p: the exit line carries the two keys the probe rows grep.
    #[test]
    fn gcg_g1p_the_new_paths_line_names_both_keys() {
        let line = format_compiled_new_paths_line((7, 11));
        assert!(line.contains("jit_new_object_calls=7 "), "{line}");
        assert!(line.contains("jit_post_tlab_init_calls=11 "), "{line}");
        assert!(
            line.starts_with("[cratonvm] compiled-new helper entries:"),
            "{line}"
        );
    }

    /// zr z3/ja: the `anewarray` pair is per VM, counts only when armed, and
    /// its keys are the ones the probe rows grep.
    #[test]
    fn zr3ja_anewarray_counts_are_per_vm_and_named() {
        let a = JitAllocClassCache::new();
        let b = JitAllocClassCache::new();
        a.new_paths.note_anewarray_if(false, true);
        assert_eq!(a.new_paths.anewarray_snapshot(), (0, 0));
        a.new_paths.note_anewarray_if(true, true);
        a.new_paths.note_anewarray_if(true, false);
        assert_eq!(a.new_paths.anewarray_snapshot(), (2, 1));
        assert_eq!(b.new_paths.anewarray_snapshot(), (0, 0));
        let keys = format_compiled_anewarray_keys((5, 3));
        assert_eq!(keys, " jit_anewarray_calls=5 jit_anewarray_small_calls=3");
    }

    /// JIT round 21 wave 1 (lane alloc): the `newarray` / ZGC-announce
    /// counters are per VM, count only when armed, keep their four keys apart,
    /// and are named as `R21AllocLean`'s run notes grep them.
    #[test]
    fn r21_alloc_newarray_counts_are_per_vm_and_named() {
        let a = JitAllocClassCache::new();
        let b = JitAllocClassCache::new();
        a.new_paths.note_newarray_if(false, true);
        a.new_paths.note_newarray_tlab_if(false);
        a.new_paths.note_zgc_note_if(false);
        assert_eq!(a.new_paths.newarray_snapshot(), (0, 0, 0, 0));
        a.new_paths.note_newarray_if(true, true);
        a.new_paths.note_newarray_if(true, true);
        a.new_paths.note_newarray_if(true, false);
        a.new_paths.note_newarray_tlab_if(true);
        for _ in 0..4 {
            a.new_paths.note_zgc_note_if(true);
        }
        assert_eq!(a.new_paths.newarray_snapshot(), (3, 2, 1, 4));
        assert_eq!(b.new_paths.newarray_snapshot(), (0, 0, 0, 0), "per VM");
        // The earlier pairs are untouched by the new counters.
        assert_eq!(a.new_paths.snapshot(), (0, 0));
        assert_eq!(a.new_paths.anewarray_snapshot(), (0, 0));
        let keys = format_compiled_newarray_keys((9, 7, 5, 3));
        assert_eq!(
            keys,
            " jit_newarray_calls=9 jit_newarray_small_calls=7 \
             jit_newarray_tlab_served=5 jit_zgc_note_calls=3"
        );
    }

    /// gcg g1/p: counted from many threads, nothing is lost (the counters are
    /// read once at exit, so `Relaxed` adds are enough).
    #[test]
    fn gcg_g1p_new_path_counts_are_exact_under_threads() {
        let cache = std::sync::Arc::new(JitAllocClassCache::new());
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let cache = std::sync::Arc::clone(&cache);
                std::thread::spawn(move || {
                    for _ in 0..10_000 {
                        cache.new_paths.note_new_object_if(true);
                        cache.new_paths.note_post_tlab_init_if(true);
                    }
                })
            })
            .collect();
        for th in threads {
            th.join().unwrap();
        }
        assert_eq!(cache.new_paths.snapshot(), (40_000, 40_000));
    }
}
