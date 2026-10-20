// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Class loading, linking, resolution and per-class caches. Owns the L10 lock.
//!
//! Extracted verbatim from the former monolithic `SharedVm` struct.
//! Field types, lock types and lock levels are unchanged; only the
//! owning struct differs. Access paths are `shared.classes.<field>`.

use crate::classloading::resolution::{LambdaCallSite, LinkResolver, ResolutionCache};
use crate::classloading::{ClassId, ClassManager};
use crate::runtime::lock_order::{
    LockLevel, OrderedPlMutex, OrderedPlRwLock, OrderedPlRwLockWriteGuard,
};
use crate::types::{ObjectRef, Value};
use crate::vm::vm_init::ANON_CLASS_CACHE_LEN;
use parking_lot::RwLock;

/// First `ClassId` the lambda-proxy allocator ([`ClassRealm::next_lambda_id`])
/// hands out. Synthetic proxy ids are minted from a counter seeded here so they
/// can never collide with a real `ClassStore` id, which starts at 0 and is
/// bumped once per loaded class -- 2^31 real classes is not a reachable state.
///
/// The range is therefore a **sound necessary precondition**: a class id below
/// this base is not a lambda proxy, and no lookup is needed to say so. That
/// matters because `lambda_proxies` is behind an `RwLock` and was probed on
/// every non-`invokespecial` virtual invoke in the interpreter -- see
/// [`ClassRealm::is_lambda_proxy_class`].
///
/// Declared here so the seed and the predicate cannot drift apart; `vm_init`
/// seeds the counter from this constant.
pub const LAMBDA_PROXY_ID_BASE: u32 = 0x8000_0000;

/// The VM-internal class every annotation proxy instance shares.
///
/// No JDK declares this name -- it is minted by `ensure_vm_internal_class` and
/// lives in the VM's own reserved namespace -- so exactly one class can ever
/// carry it, under the bootstrap loader. That uniqueness is what lets
/// [`ClassRealm::is_annotation_proxy_class`] answer from a single `ClassId`
/// instead of a name comparison under the class-manager lock.
pub const ANNOTATION_PROXY_CLASS: &str = "java/lang/annotation/AnnotationProxy";

/// Kill switch for the `StoreEpochs::annotation_proxy_defined` fast path in
/// [`ClassRealm::is_annotation_proxy_class`]
/// (`CRATONVM_LOADER_NO_ANN_PROXY_LATCH=1`, or
/// `CRATONVM_LOADER=-ann-proxy-latch`). Set, the gate falls through to the
/// epoch-keyed resolver on every call, exactly as it did before the latch —
/// which is what makes the two arms comparable inside one binary.
///
/// Read once per VM into [`ClassRealm::ann_proxy_latch`] (interpreter round i2
/// wave 61, lane L7): the predicate runs on every virtual-door call, where this
/// was a process `OnceLock` read whenever the proxy class was not yet minted.
pub(crate) fn ann_proxy_latch_from_env() -> bool {
    cratonvm_types::flags::runtime_var_os("CRATONVM_LOADER_NO_ANN_PROXY_LATCH").is_none()
}

/// Proxy ids the [`LambdaProxyHitBits`] bitmap covers, from
/// [`LAMBDA_PROXY_ID_BASE`]: 2^20 bits (128 KiB, allocated on the first hit). A
/// proxy past it keeps the locked lookup.
pub(crate) const LAMBDA_PROXY_HIT_BITS_CAPACITY: usize = 1 << 20;

/// JIT round 20 wave 12, lane qLam (W15L5-7 of
/// `docs/internal/proposals/jit/jit-r21-nio-proposals-w15-L5.md`, w17-L1 item
/// 6): [`ClassRealm::is_lambda_proxy_class`] answered every HIT with an
/// `RwLock` read of `lambda_proxies` (1.0-1.2% of the w16 `rlock`, `optional`
/// and `hdr` profiles: it is asked on every compiled virtual call that reaches
/// Rust with a lambda receiver). This is the lock-free positive answer: one bit
/// per proxy id, per VM (a field of the realm, never a process global).
///
/// * **Set** ([`Self::note`]) only by a lookup that found the id in the table,
///   WHILE it still holds the table's read guard.
/// * **Cleared** ([`Self::forget`]) for every row the two unload paths remove
///   (`memory::gc::unload_dead_class_metadata`,
///   `invokedynamic::forget_unloaded_lambda_proxy_rows`), inside their
///   `retain`, i.e. under the table's WRITE guard.
///
/// The two guards exclude each other, so a set bit always names an id the
/// table holds: a note either precedes the removal (and is cleared by it) or
/// follows it (and then never happens, because its lookup misses). Insertion
/// never needs a bit -- a missing bit only sends the caller to the table, which
/// stays the authority. Off (`CRATONVM_LAMBDA_PROXY_LOCKFREE_HIT=0`, read once
/// per VM) nothing is allocated, set or read: the lookup is today's exactly.
pub(crate) struct LambdaProxyHitBits {
    enabled: bool,
    words: OnceLock<Box<[AtomicU64]>>,
}

impl LambdaProxyHitBits {
    /// A bitmap with nothing set; `enabled == false` makes every method inert.
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            words: OnceLock::new(),
        }
    }

    /// [`Self::new`] with the switch's per-VM reading.
    pub(crate) fn from_env() -> Self {
        Self::new(cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_LAMBDA_PROXY_LOCKFREE_HIT",
        ))
    }

    /// `(word index, bit mask)` of `class_id`, or `None` outside the covered
    /// band (below the base, or past the capacity).
    #[inline]
    fn slot(class_id: ClassId) -> Option<(usize, u64)> {
        let off = usize::try_from(class_id.as_u32().checked_sub(LAMBDA_PROXY_ID_BASE)?).ok()?;
        if off >= LAMBDA_PROXY_HIT_BITS_CAPACITY {
            return None;
        }
        Some((off / 64, 1u64 << (off % 64)))
    }

    /// Is `class_id`'s bit set? `false` means "ask the table", never "no".
    #[inline]
    pub(crate) fn contains(&self, class_id: ClassId) -> bool {
        if !self.enabled {
            return false;
        }
        let (Some(words), Some((w, bit))) = (self.words.get(), Self::slot(class_id)) else {
            return false;
        };
        words[w].load(Ordering::Acquire) & bit != 0
    }

    /// Record that the table holds `class_id`. The caller holds the table's
    /// read (or write) guard and has just found the id in it.
    #[cold]
    pub(crate) fn note(&self, class_id: ClassId) {
        if !self.enabled {
            return;
        }
        let Some((w, bit)) = Self::slot(class_id) else {
            return;
        };
        let words = self.words.get_or_init(|| {
            cratonvm_types::zeroed_table::zeroed_boxed_slice(
                LAMBDA_PROXY_HIT_BITS_CAPACITY / 64,
                || AtomicU64::new(0),
            )
        });
        let before = words[w].fetch_or(bit, Ordering::Release);
        // The positive control: one line per proxy recorded (every later hit
        // on it is answered from the bit, without the lock).
        if before & bit == 0 && crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] lambda-proxy-hit-bit set class_id={}",
                class_id.as_u32()
            );
        }
    }

    /// Clear `class_id`'s bit: its row is being removed. The caller holds the
    /// table's write guard.
    pub(crate) fn forget(&self, class_id: ClassId) {
        let (Some(words), Some((w, bit))) = (self.words.get(), Self::slot(class_id)) else {
            return;
        };
        words[w].fetch_and(!bit, Ordering::Release);
    }
}

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};

/// Number of ordinary class ids [`GetClassMirrorWords`] has a word for. A
/// class id at or above it has no word (compiled code takes the helper),
/// except, since nio20 wave 5 (lane L2), a lambda proxy's in
/// `LAMBDA_PROXY_ID_BASE .. + GETCLASS_WORDS_LAMBDA_CAPACITY`, which has one in
/// the table's second band.
pub const GETCLASS_MIRROR_WORDS_CAPACITY: usize = 1 << 16;

// nio20 wave 5, lane L2 (W4-L2-2): the table's shape is shared with the code
// both JIT tiers emit (`cratonvm_jit::ir_lower::getclass_table_lookup_bytes`),
// which bakes the lambda band's displacement and bounds. Pinned here so the two
// crates cannot drift apart.
const _: () = assert!(
    GETCLASS_MIRROR_WORDS_CAPACITY == cratonvm_jit::ir_lower::GETCLASS_WORDS_CLASS_CAPACITY
);
const _: () =
    assert!(LAMBDA_PROXY_ID_BASE == cratonvm_jit::ir_lower::GETCLASS_WORDS_LAMBDA_BASE_ID);

/// JIT round 20 (nio20 wave 1, lane L3, NIO19-5 / NIO17-1): the per-VM,
/// GC-maintained `class id -> java.lang.Class mirror` table compiled code reads
/// to answer `Object.getClass()` with two loads instead of a CALL to
/// `jit_object_get_class_direct`.
///
/// # Layout (what compiled code bakes)
///
/// One allocation of `cratonvm_jit::ir_lower::GETCLASS_WORDS_TOTAL` machine
/// words at a stable address ([`Self::base_addr`]): word 0 holds the capacity,
/// word `1 + class_id` the mirror's address or `0`, and (nio20 wave 5 L2) word
/// `1 + CAPACITY + (id - LAMBDA_PROXY_ID_BASE)` a lambda proxy's, and (nio21
/// wave 1 L5) the eight words from `GETCLASS_WORDS_PRIM_ARRAY_BASE` the
/// one-dimensional primitive array classes' (keyed by element type, published
/// by [`Self::publish_prim_array`]). An optimizing-tier
/// `getClass()` site (`ir_lower::try_emit_ir_getclass_prefix`) emits
///
/// ```text
///   TEST RAX,RAX ; JZ call                 ; null: the call raises the NPE
///   TEST BYTE [RAX+KIND_TAGS],MASK ; JNZ call  ; arrays: header holds the COMPONENT id
///   MOV  EAX,[RAX]                         ; class id
///   MOV  R10,imm64 base
///   CMP  RAX,[R10] ; JAE call              ; no word for this id
///   MOV  RAX,[R10+RAX*8+8] ; TEST ; JZ call ; 0 = not published: take the helper
/// ```
///
/// # Who writes it
///
/// * **Publish** -- only the two doors that already answer `getClass()` from
///   `class_mirrors` without the native (the thin helper
///   `jit_object_get_class_direct` and the SBR2 leaf arm), and only for the
///   case they answer: a plain object whose display verdict is memoised as
///   "the stamped class" (`getclass_display_is_cached_stamp`) and whose class
///   has a mirror. A class whose `getClass()` the native REMAPS (the synthetic
///   collection stamps) therefore never gets a word, which is the NIO17-1
///   rule "publish a word only for a class whose verdict is `Stamp`". The
///   word is written under the `class_mirrors` READ lock from the value read
///   under it.
/// * **Re-key** -- `memory::gc::update_all_roots` step 6, under the
///   `class_mirrors` WRITE lock with every mutator parked: a moved mirror's
///   word follows it (compare-and-swap old -> new, so a word that was never
///   published stays `0`).
/// * **Forget** -- class unload (`memory::gc`'s `class_mirrors` removal) and
///   `reconcile_class_mirrors`' prune of dead mirrors, under the same WRITE
///   lock. A re-used class id therefore never inherits a dead mirror.
///
/// Those are every non-test writer of `class_mirrors` that can change or
/// remove an existing entry (`get_or_create_class_mirror` only inserts an
/// absent one), so a non-zero word always equals `class_mirrors[class_id]`.
///
/// # Why no read barrier
///
/// The words are plain addresses outside the heap, like the `ldc` slot table's
/// (`jit::helpers::ldc_global_slots`), and are rewritten only while mutators
/// are parked. The emitter refuses while ZGC's read barrier is armed
/// (`x64::zgc_read_barrier_blocks_inline_fields`), the gate every inline
/// reference load shares.
///
/// # Lifetime
///
/// Allocated on first [`Self::base_addr`] (a compile that will emit the
/// prefix), so a VM that never JITs a `getClass()` pays nothing, and freed
/// with the VM. Per VM, never a process global (`AGENTS.md`).
pub struct GetClassMirrorWords {
    words: OnceLock<Box<[AtomicUsize]>>,
    /// nio20 wave 4, lane L2: `CRATONVM_DBG_JITC` engagement counts of the
    /// thin helper (`jit_object_get_class_direct`), per VM: calls it answered
    /// from the mirror map, and calls it declined to the generic dispatcher.
    /// A compiled prefix HIT never reaches the helper, so these two are the
    /// prefix MISSES (plus the calls of sites that have no prefix). Written
    /// only under the debug switch ([`Self::note_helper_call`]).
    helper_served: AtomicU64,
    helper_declined: AtomicU64,
    /// nio21 wave 1, lane L5 (W4-L2-1): the array class id whose mirror each
    /// primitive-array word holds (`u32::MAX` = none yet), so the collector's
    /// re-key and forget -- which name a CLASS ID -- reach the alias word too
    /// ([`Self::publish_prim_array`]). Written only under the `class_mirrors`
    /// read lock (publish), read under either lock.
    prim_array_ids: [AtomicU32; cratonvm_jit::ir_lower::GETCLASS_WORDS_PRIM_ARRAY_CAPACITY],
    /// nio21 wave 2, lane L5 (L5W1-1): for each ARRAY class id below
    /// `GETCLASS_WORDS_REF_ARRAY_CAPACITY`, `1 + the component id` whose
    /// reference-array word holds that array class's mirror (`0` = none), so
    /// the collector's re-key / forget of the array class id reach the word
    /// ([`Self::publish_ref_array`]). Allocated on the first reference-array
    /// publish. Written under the `class_mirrors` read lock (publish) or
    /// write lock (forget).
    ref_array_owner: OnceLock<Box<[AtomicU32]>>,
    /// JIT round 20 wave 19, lane zAssign: `CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE`
    /// as the mirror-hint filler reads it, per VM (`0` not read yet, `1` off,
    /// `2` on): the filler runs on every resolved `Class.isAssignableFrom` /
    /// `isInstance` native call, where a per-call flag read is not affordable.
    mirror_hints_mode: std::sync::atomic::AtomicU8,
}

impl Default for GetClassMirrorWords {
    fn default() -> Self {
        Self::new()
    }
}

impl GetClassMirrorWords {
    /// An unallocated table: every lookup answers `0`, every write is a no-op.
    pub const fn new() -> Self {
        Self {
            words: OnceLock::new(),
            helper_served: AtomicU64::new(0),
            helper_declined: AtomicU64::new(0),
            prim_array_ids: [const { AtomicU32::new(u32::MAX) };
                cratonvm_jit::ir_lower::GETCLASS_WORDS_PRIM_ARRAY_CAPACITY],
            ref_array_owner: OnceLock::new(),
            mirror_hints_mode: std::sync::atomic::AtomicU8::new(0),
        }
    }

    /// JIT round 20 wave 19, lane zAssign: does this VM fill mirror hints
    /// (`CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE`, default ON, `=0` off)?
    /// Read once per VM.
    pub fn mirror_hints_enabled(&self) -> bool {
        match self.mirror_hints_mode.load(Ordering::Relaxed) {
            1 => false,
            2 => true,
            _ => {
                let on = cratonvm_jit::class_is_assignable_inline_enabled();
                self.mirror_hints_mode
                    .store(if on { 2 } else { 1 }, Ordering::Relaxed);
                on
            }
        }
    }

    /// JIT round 20 wave 19, lane zAssign: the mirror-hint word in the slot of
    /// the mirror at `mirror_addr`, or `0` (no table yet, or an empty slot).
    /// Only a hint: see `cratonvm_jit::ir_lower::GETCLASS_WORDS_MIRROR_HINT_BASE`.
    pub fn mirror_hint(&self, mirror_addr: usize) -> usize {
        let slot = cratonvm_jit::ir_lower::mirror_hint_slot(mirror_addr);
        self.words
            .get()
            .and_then(|w| w.get(cratonvm_jit::ir_lower::GETCLASS_WORDS_MIRROR_HINT_BASE + slot))
            .map_or(0, |word| word.load(Ordering::Acquire))
    }

    /// JIT round 20 wave 19, lane zAssign: write `hint`
    /// (`cratonvm_jit::ir_lower::mirror_hint_word`) into the slot of the
    /// mirror at `mirror_addr`, replacing whatever hint was there. A no-op
    /// before the table exists. The caller has seen `class_mirrors[c] ==
    /// mirror` and the class word of `c` published as that mirror (`c` the
    /// hint's class id), under the `class_mirrors` read lock it still holds.
    pub fn publish_mirror_hint(&self, mirror_addr: usize, hint: usize) {
        let slot = cratonvm_jit::ir_lower::mirror_hint_slot(mirror_addr);
        if let Some(word) = self
            .words
            .get()
            .and_then(|w| w.get(cratonvm_jit::ir_lower::GETCLASS_WORDS_MIRROR_HINT_BASE + slot))
        {
            word.store(hint, Ordering::Release);
        }
    }

    /// JIT round 20 wave 19, lane zAssign: clear every mirror hint naming
    /// `class_id`, so a re-used id never inherits a hint's flags (compiled
    /// code already refuses a hint whose class word is not the mirror; the
    /// flags are the one part of a hint the class word does not prove).
    fn forget_mirror_hints(&self, class_id: ClassId) {
        let raw = class_id.as_u32();
        if usize::try_from(raw).map_or(true, |i| i >= GETCLASS_MIRROR_WORDS_CAPACITY) {
            return;
        }
        let Some(words) = self.words.get() else {
            return;
        };
        let base = cratonvm_jit::ir_lower::GETCLASS_WORDS_MIRROR_HINT_BASE;
        let end = base + cratonvm_jit::ir_lower::GETCLASS_WORDS_MIRROR_HINT_CAPACITY;
        // A hint, or a remembered refusal (`MIRROR_HINT_DECLINED`), naming it.
        let flags = usize::try_from(
            cratonvm_jit::ir_lower::MIRROR_HINT_PRESENT
                | cratonvm_jit::ir_lower::MIRROR_HINT_DECLINED,
        )
        .unwrap_or(0);
        for word in words.get(base..end).unwrap_or(&[]) {
            let v = word.load(Ordering::Acquire);
            // Cast: the low 32 bits of a hint word are its class id.
            if v & flags != 0 && (v as u32) == raw {
                word.store(0, Ordering::Release);
            }
        }
    }

    /// Count one thin-helper call (`served`: answered from the mirror map;
    /// else declined to the dispatcher) and return `(served, declined)` when
    /// this call's count is a power of two, so a `CRATONVM_DBG_JITC` run logs
    /// a bounded number of lines however hot the site is. Diagnostics only:
    /// the caller asks the debug switch first.
    pub fn note_helper_call(&self, served: bool) -> Option<(u64, u64)> {
        let (own, other) = if served {
            (&self.helper_served, &self.helper_declined)
        } else {
            (&self.helper_declined, &self.helper_served)
        };
        let n = own.fetch_add(1, Ordering::Relaxed) + 1;
        if !n.is_power_of_two() {
            return None;
        }
        let m = other.load(Ordering::Relaxed);
        Some(if served { (n, m) } else { (m, n) })
    }

    fn alloc() -> Box<[AtomicUsize]> {
        // `vec![0; n]` is `alloc_zeroed`: pages no class id reaches are never
        // touched.
        // nio20 wave 5 L2: the lambda-proxy band follows the class band.
        let zeroed: Box<[usize]> =
            vec![0usize; cratonvm_jit::ir_lower::GETCLASS_WORDS_TOTAL].into_boxed_slice();
        // SAFETY: `AtomicUsize` has the same size, alignment and bit validity
        // as `usize` (documented on `AtomicUsize`), and the slice-pointer cast
        // keeps the length, so the box is reinterpreted element for element
        // and freed with the same layout it was allocated with.
        let words: Box<[AtomicUsize]> =
            unsafe { Box::from_raw(Box::into_raw(zeroed) as *mut [AtomicUsize]) };
        words[0].store(GETCLASS_MIRROR_WORDS_CAPACITY, Ordering::Release);
        words
    }

    /// The table's address for compiled code to bake, allocating it on first
    /// use. Stable for the VM's life.
    pub fn base_addr(&self) -> usize {
        // Cast: the allocation's address, for compiled code to bake.
        self.words.get_or_init(Self::alloc).as_ptr() as usize
    }

    /// `true` once [`Self::base_addr`] has allocated the table.
    pub fn is_allocated(&self) -> bool {
        self.words.get().is_some()
    }

    /// `class_id`'s word: `1 + class_id` in the class band, or (nio20 wave 5,
    /// lane L2, W4-L2-2) `1 + CAPACITY + (class_id - LAMBDA_PROXY_ID_BASE)` in
    /// the lambda-proxy band -- proxy ids come from one counter seeded at the
    /// base and are never reused, and the thin helper already serves a proxy
    /// whose display verdict is its stamp, so publishing it is the same rule.
    /// Publish, re-key and forget all go through here, so the band needs no
    /// writer of its own. Any other id has no word.
    fn word(&self, class_id: ClassId) -> Option<&AtomicUsize> {
        let raw = class_id.as_u32();
        let index = usize::try_from(raw).ok()?;
        if index < GETCLASS_MIRROR_WORDS_CAPACITY {
            return self.words.get()?.get(index + 1);
        }
        let lambda = usize::try_from(raw.checked_sub(LAMBDA_PROXY_ID_BASE)?).ok()?;
        if lambda >= cratonvm_jit::ir_lower::GETCLASS_WORDS_LAMBDA_CAPACITY {
            return None;
        }
        self.words
            .get()?
            .get(1 + GETCLASS_MIRROR_WORDS_CAPACITY + lambda)
    }

    /// Publish `mirror` as `class_id`'s word. A no-op before the table exists
    /// and for an id past the capacity. The caller holds the `class_mirrors`
    /// read lock and read `mirror` under it (see the type's doc).
    pub fn publish(&self, class_id: ClassId, mirror: ObjectRef) {
        if let Some(word) = self.word(class_id) {
            // Cast: the mirror's address, the word compiled code returns.
            word.store(mirror.as_ptr() as usize, Ordering::Release);
        }
    }

    /// `class_id`'s published word, or `0`.
    pub fn get(&self, class_id: ClassId) -> usize {
        self.word(class_id)
            .map_or(0, |word| word.load(Ordering::Acquire))
    }

    /// Withdraw `class_id`'s word (its mirror is gone, or its class unloaded),
    /// and (nio21 wave 1 L5) the primitive-array word it is aliased under.
    pub fn forget(&self, class_id: ClassId) {
        if let Some(word) = self.word(class_id) {
            word.store(0, Ordering::Release);
        }
        // JIT round 20 wave 19, lane zAssign: and the mirror hints naming it.
        self.forget_mirror_hints(class_id);
        if let Some(word) = self.prim_array_alias(class_id) {
            word.store(0, Ordering::Release);
        }
        // nio21 wave 2 L5: the reference-array word this ARRAY class id is
        // published under, and -- conservatively -- the word keyed by this id
        // as a COMPONENT (a forgotten component never leaves a word a re-used
        // id could inherit).
        if let Some(word) = self.ref_array_alias(class_id) {
            word.store(0, Ordering::Release);
        }
        if let Some(owner) = self.ref_array_owner_slot(class_id) {
            owner.store(0, Ordering::Release);
        }
        if let Some(word) = self.ref_array_word(class_id) {
            word.store(0, Ordering::Release);
        }
    }

    /// The collector moved `class_id`'s mirror from `old` to `new`: a word
    /// that names `old` follows it; any other word (never published, `0`) is
    /// left alone. The primitive-array alias (nio21 wave 1 L5) follows the
    /// same way.
    pub fn rekey(&self, class_id: ClassId, old: usize, new: usize) {
        if let Some(word) = self.word(class_id) {
            let _ = word.compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire);
        }
        if let Some(word) = self.prim_array_alias(class_id) {
            let _ = word.compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire);
        }
        // nio21 wave 2 L5: the reference-array alias follows the same way.
        if let Some(word) = self.ref_array_alias(class_id) {
            let _ = word.compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire);
        }
    }

    /// nio21 wave 1, lane L5: the primitive-array word `class_id` is
    /// published under, if any. Eight relaxed compares; only the collector's
    /// re-key / forget (mutators parked or the WRITE lock held) ask it.
    fn prim_array_alias(&self, class_id: ClassId) -> Option<&AtomicUsize> {
        let raw = class_id.as_u32();
        if raw == u32::MAX {
            return None;
        }
        let slot = self
            .prim_array_ids
            .iter()
            .position(|id| id.load(Ordering::Acquire) == raw)?;
        self.words
            .get()?
            .get(cratonvm_jit::ir_lower::GETCLASS_WORDS_PRIM_ARRAY_BASE + slot)
    }

    /// nio21 wave 1, lane L5 (W4-L2-1): publish `mirror`, the mirror of the
    /// one-dimensional primitive array class `array_class_id`, as the word of
    /// primitive-array band `slot` (`ir_lower::getclass_prim_array_slot` of
    /// its element type), which both tiers' `getClass()` prefix reads keyed by
    /// the receiver's tag byte. The id is recorded first so the collector's
    /// re-key / forget of `array_class_id` reaches the word. A no-op before the
    /// table exists, for a slot past the band, and under
    /// `CRATONVM_JIT_GETCLASS_PRIM_ARRAY_WORDS=0`. The caller holds the
    /// `class_mirrors` read lock and read `mirror` under it.
    pub fn publish_prim_array(&self, slot: usize, array_class_id: ClassId, mirror: ObjectRef) {
        if slot >= cratonvm_jit::ir_lower::GETCLASS_WORDS_PRIM_ARRAY_CAPACITY
            || array_class_id.as_u32() == u32::MAX
            || !cratonvm_jit::ir_lower::getclass_prim_array_words_enabled()
        {
            return;
        }
        let Some(words) = self.words.get() else {
            return;
        };
        let Some(word) = words.get(cratonvm_jit::ir_lower::GETCLASS_WORDS_PRIM_ARRAY_BASE + slot)
        else {
            return;
        };
        self.prim_array_ids[slot].store(array_class_id.as_u32(), Ordering::Release);
        // Cast: the mirror's address, the word compiled code returns.
        word.store(mirror.as_ptr() as usize, Ordering::Release);
    }

    /// nio21 wave 2, lane L5: the reference-array band word keyed by
    /// `component_id`, if the table exists and the id is inside the band.
    fn ref_array_word(&self, component_id: ClassId) -> Option<&AtomicUsize> {
        let index = usize::try_from(component_id.as_u32()).ok()?;
        if index >= cratonvm_jit::ir_lower::GETCLASS_WORDS_REF_ARRAY_CAPACITY {
            return None;
        }
        self.words
            .get()?
            .get(cratonvm_jit::ir_lower::GETCLASS_WORDS_REF_ARRAY_BASE + index)
    }

    /// nio21 wave 2, lane L5: `array_class_id`'s entry in the owner map.
    fn ref_array_owner_slot(&self, array_class_id: ClassId) -> Option<&AtomicU32> {
        let index = usize::try_from(array_class_id.as_u32()).ok()?;
        self.ref_array_owner.get()?.get(index)
    }

    /// nio21 wave 2, lane L5: the reference-array word `array_class_id` is
    /// published under, if any (one owner-map read; only the collector's
    /// re-key / forget ask it).
    fn ref_array_alias(&self, array_class_id: ClassId) -> Option<&AtomicUsize> {
        let owner = self
            .ref_array_owner_slot(array_class_id)?
            .load(Ordering::Acquire);
        let component = owner.checked_sub(1)?;
        self.ref_array_word(ClassId::new(component))
    }

    /// nio21 wave 2, lane L5 (L5W1-1): publish `mirror`, the mirror of the
    /// array class `array_class_id` over the component `component_id`, as
    /// that component's reference-array word, which both tiers' `getClass()`
    /// prefix reads keyed by a reference array's header class id. The owner
    /// map records `array_class_id` first so the collector's re-key / forget
    /// of the array class reach the word. A no-op before the table exists,
    /// for either id past the band, and under
    /// `CRATONVM_JIT_GETCLASS_REF_ARRAY_WORDS=0`. The caller holds the
    /// `class_mirrors` read lock, read `mirror` under it, and has shown that
    /// `array_class_id` is `native_object_get_class`'s answer for every array
    /// over `component_id`
    /// (`jit::helpers::ref_array_class_mirror_publishing_word`).
    pub fn publish_ref_array(
        &self,
        component_id: ClassId,
        array_class_id: ClassId,
        mirror: ObjectRef,
    ) {
        if !cratonvm_jit::ir_lower::getclass_ref_array_words_enabled() {
            return;
        }
        let Some(word) = self.ref_array_word(component_id) else {
            return;
        };
        let cap = cratonvm_jit::ir_lower::GETCLASS_WORDS_REF_ARRAY_CAPACITY;
        let Ok(array_index) = usize::try_from(array_class_id.as_u32()) else {
            return;
        };
        if array_index >= cap {
            return;
        }
        let owners = self.ref_array_owner.get_or_init(|| {
            cratonvm_types::zeroed_table::zeroed_boxed_slice(cap, || AtomicU32::new(0))
        });
        // `component_id < cap`, so `+ 1` cannot overflow.
        owners[array_index].store(component_id.as_u32() + 1, Ordering::Release);
        // Cast: the mirror's address, the word compiled code returns.
        word.store(mirror.as_ptr() as usize, Ordering::Release);
    }

    /// The reference-array word of `component_id`, or `0` (tests and
    /// diagnostics).
    pub fn get_ref_array(&self, component_id: ClassId) -> usize {
        self.ref_array_word(component_id)
            .map_or(0, |word| word.load(Ordering::Acquire))
    }

    /// Primitive-array band `slot`'s published word, or `0` (tests and
    /// diagnostics).
    pub fn get_prim_array(&self, slot: usize) -> usize {
        self.words
            .get()
            .and_then(|w| w.get(cratonvm_jit::ir_lower::GETCLASS_WORDS_PRIM_ARRAY_BASE + slot))
            .filter(|_| slot < cratonvm_jit::ir_lower::GETCLASS_WORDS_PRIM_ARRAY_CAPACITY)
            .map_or(0, |word| word.load(Ordering::Acquire))
    }
}

/// One class's static-field storage, at a **stable, never-freed address**.
///
/// Static reads were the most expensive field access in compiled code (~35 ns
/// against HotSpot's ~1 — see
/// `jit-getstatic-costs-a-helper-call-FIXED-20260803.md`). What
/// was left after trimming the helper is the `RwLock` + hash probe that reaching
/// a `Vec<Value>` inside an `FxHashMap` requires — and, once this block had a
/// stable address, the prerequisite for deleting the helper CALL outright:
/// compiled `getstatic` now bakes [`StaticsIndex::base_cell_addr`] and loads
/// from the block directly.
///
/// A `Vec` cannot be read without that lock, because a `resize` would move the
/// buffer out from under a concurrent reader. This block therefore **leaks**
/// its allocation: created once, never freed, and growth allocates a *new*
/// block while leaving the old one mapped. A raw pointer handed out here stays
/// valid for the life of the VM, which is what lets [`StaticsIndex`] serve
/// reads with no lock at all.
///
/// Deliberately `Deref<Target = [Value]>` so call sites that index, iterate or
/// take `.len()` keep working unchanged.
pub struct StaticsBlock {
    ptr: *mut Value,
    len: usize,
}

// SAFETY: a plain owned allocation of `Value` (itself `Send + Sync`). Creation,
// growth and publication happen under the `statics` write lock. Single-cell
// stores into an already-published block are also made lock-free, through
// `StaticsIndex::cell_ptr` (`set_static_shared`'s steady state, since
// 2026-09-23), the same unsynchronized cell access the lock-free reader and
// compiled `getstatic` already make.
unsafe impl Send for StaticsBlock {}
unsafe impl Sync for StaticsBlock {}

impl StaticsBlock {
    /// Allocate `len` zero-initialized slots and leak them.
    pub fn new(len: usize) -> Self {
        Self::from_values(vec![Value::Int(0); len])
    }

    pub fn from_values(values: Vec<Value>) -> Self {
        let leaked: &'static mut [Value] = Box::leak(values.into_boxed_slice());
        Self {
            ptr: leaked.as_mut_ptr(),
            len: leaked.len(),
        }
    }

    /// Base address of slot 0. Stable for the life of the VM.
    #[inline]
    pub fn base_ptr(&self) -> *mut Value {
        self.ptr
    }

    #[inline]
    pub fn slot_len(&self) -> usize {
        self.len
    }

    /// Grow to at least `new_len`, preserving contents.
    ///
    /// The OLD block is intentionally left allocated: a lock-free reader may
    /// still hold a pointer into it. Growth only fires when a write targets an
    /// index past the class's declared field count — already the unexpected
    /// path — so the leak is bounded by that rarity.
    pub fn grow_to(&mut self, new_len: usize) {
        if new_len <= self.len {
            return;
        }
        let mut values = vec![Value::Int(0); new_len];
        values[..self.len].copy_from_slice(self.as_slice());
        let grown = Self::from_values(values);
        self.ptr = grown.ptr;
        self.len = grown.len;
    }

    #[inline]
    pub fn as_slice(&self) -> &[Value] {
        // SAFETY: `ptr`/`len` describe a leaked, never-freed allocation.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [Value] {
        // SAFETY: as above; `&mut self` proves exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl std::ops::Deref for StaticsBlock {
    type Target = [Value];
    #[inline]
    fn deref(&self) -> &[Value] {
        self.as_slice()
    }
}

impl std::ops::DerefMut for StaticsBlock {
    #[inline]
    fn deref_mut(&mut self) -> &mut [Value] {
        self.as_mut_slice()
    }
}

pub struct StaticsIndexSlot {
    base: std::sync::atomic::AtomicPtr<Value>,
    len: AtomicUsize,
}

// SAFETY: two atomics; all-zero is `base` null and `len` 0, the empty slot
// `StaticsIndex::slots`' eager arm builds (zs s6/rs).
unsafe impl cratonvm_types::zeroed_table::ZeroInit for StaticsIndexSlot {}

/// Lock-free `ClassId -> statics base pointer` index.
///
/// Mirrors `ClassRealm::statics`, so a reader that needs one slot can skip the
/// `RwLock` and the hash probe entirely. Entries are published when a class's
/// [`StaticsBlock`] is created or grown; because blocks are never freed, a
/// pointer read from here stays valid.
///
/// Ids at or beyond [`StaticsIndex::CAPACITY`] are simply not indexed and fall
/// back to the map, making the capacity a performance bound, not a correctness
/// one.
pub struct StaticsIndex {
    slots: OnceLock<Box<[StaticsIndexSlot]>>,
}

impl Default for StaticsIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl StaticsIndex {
    /// Covers class ids `[0, 1 << 17)`; ~1.5 MiB, allocated on first publish.
    pub const CAPACITY: usize = 1 << 17;

    pub const fn new() -> Self {
        Self {
            slots: OnceLock::new(),
        }
    }

    fn slots(&self) -> &[StaticsIndexSlot] {
        // zs s6/rs: allocated zeroed (`CRATONVM_LAZY_ZERO_TABLES`): 2 MiB
        // for 128 K ids, of which a boot writes a few pages.
        self.slots.get_or_init(|| {
            cratonvm_types::zeroed_table::zeroed_boxed_slice(Self::CAPACITY, || StaticsIndexSlot {
                base: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
                len: AtomicUsize::new(0),
            })
        })
    }

    /// Publish (or re-publish, after a grow) a class's statics base.
    ///
    /// **Callers hold the `statics` WRITE lock**, so publications of one class
    /// never interleave with each other (`set_static_shared`,
    /// `prepare_class_shared`).
    ///
    /// `base` and `len` are two independent atomics, so a reader can see one
    /// publication's base with another's length. The only pairing that is
    /// unsafe is one that OVERSTATES the block, so the store order depends on
    /// the direction:
    ///
    /// * growing (or first publication, from a null base): `base` first, then
    ///   `len`. A reader pairing the new base with the old length under-reads.
    ///   A reader pairing the OLD base with the NEW length is excluded by its
    ///   re-check of `base` after reading `len` ([`Self::published`]): having
    ///   `Acquire`d the new length, it must see the new base.
    /// * shrinking (a `prepare_class_shared` replacing a block that
    ///   `set_static_shared` created early, sized by ALL the class's fields):
    ///   `len` first, then `base`. A reader that `Acquire`s the new base must
    ///   see the new (smaller) length; one that sees the old base with either
    ///   length stays inside the old block.
    ///
    /// Until 2026-09-23 this stored `len` then `base` unconditionally, and a
    /// reader racing a GROW could pair the old base with the new length and
    /// read past the end of the old block.
    pub fn publish(&self, class_id: ClassId, block: &StaticsBlock) {
        let idx = class_id.as_u32() as usize;
        if idx >= Self::CAPACITY {
            return;
        }
        let slot = &self.slots()[idx];
        let new_len = block.slot_len();
        if new_len >= slot.len.load(Ordering::Relaxed) {
            slot.base.store(block.base_ptr(), Ordering::Release);
            slot.len.store(new_len, Ordering::Release);
        } else {
            slot.len.store(new_len, Ordering::Release);
            slot.base.store(block.base_ptr(), Ordering::Release);
        }
    }

    /// Withdraw a class's publication when the class is unloaded
    /// (`memory::gc`'s class-metadata unload, which drops the class's block
    /// from the `statics` map under the same WRITE lock).
    ///
    /// Before this existed the entry stayed published after the map forgot
    /// the block, so the lock-free read kept answering from a block the GC no
    /// longer scans (dead references, where the map path answers the default),
    /// and `set_static_shared`'s lock-free store wrote references into it
    /// unrooted.
    ///
    /// Only the LENGTH goes to zero; `base` keeps naming the old block. Every
    /// lock-free consumer bounds-checks against the length, so it now declines
    /// and takes the locked path. Leaving `base` non-null is what keeps
    /// already-compiled code safe: it bakes the address of this cell
    /// ([`Self::base_cell_addr`]) and loads through it with no null check, and
    /// the old block is leaked, so it stays readable. A later
    /// [`Self::publish`] is a grow from length 0 (base, then length).
    pub fn unpublish(&self, class_id: ClassId) {
        let idx = class_id.as_u32() as usize;
        if idx >= Self::CAPACITY {
            return;
        }
        // Nothing was ever published in this VM: nothing to withdraw, and no
        // reason to force the 1.5 MiB allocation.
        let Some(slots) = self.slots.get() else {
            return;
        };
        slots[idx].len.store(0, Ordering::Release);
    }

    /// The published `(base, len)` pair for `class_id`, never overstating the
    /// block `base` names (see [`Self::publish`] for the protocol). `None` when
    /// the id is not indexable, nothing is published, or a republication raced
    /// this read — every caller has a locked fallback.
    #[inline]
    fn published(&self, class_id: ClassId) -> Option<(*mut Value, usize)> {
        let idx = class_id.as_u32() as usize;
        if idx >= Self::CAPACITY {
            return None;
        }
        // Never force the 1.5 MiB allocation for a VM that has not published.
        let slots = self.slots.get()?;
        let slot = &slots[idx];
        let base = slot.base.load(Ordering::Acquire);
        if base.is_null() {
            return None;
        }
        let len = slot.len.load(Ordering::Acquire);
        if slot.base.load(Ordering::Acquire) != base {
            return None;
        }
        Some((base, len))
    }

    /// Read one static slot without taking any lock.
    ///
    /// `None` means "not indexed / out of range" — the caller falls back to the
    /// locked map path.
    #[inline]
    pub fn get(&self, class_id: ClassId, field_index: usize) -> Option<Value> {
        let (base, len) = self.published(class_id)?;
        if field_index >= len {
            return None;
        }
        // Read the cell as two independent 8-byte words instead of copying
        // the 16-byte `Value` in one go.
        //
        // `*base.add(i)` on a 16-byte type is lowered as a wide (SSE) copy, and
        // a 16-byte access is NOT single-copy atomic on x86-64. Against a
        // concurrent `putstatic` this reader observed **halves of two different
        // writes**: `probes/StaticRaceProbe.java` reproduces it in seconds, a
        // `static long` flipped between `0x0123456789ABCDEF` and
        // `0x7EDCBA9876543210` reading back as `0x7EDCBA9889ABCDEF`. For a
        // plain `long`/`double` static JLS §17.7 permits that; for a
        // **`volatile`** one it does not, and the same probe tore a
        // `static volatile long` too — on the interpreter path (`--nojit`) as
        // well, since `get_static_shared` routes through here.
        //
        // Two aligned 8-byte volatile loads fix it: the discriminant word and a
        // 4-byte payload share the low word, an 8-byte payload IS the high
        // word, so no payload ever spans a load. `read_volatile` is what stops
        // the optimizer from merging them back into one wide move.
        //
        // The STORE side needs no matching change, and that is a measurement,
        // not an assumption: with the read narrowed, 15 s x 4 reader threads x
        // {inline JIT, helper JIT, interpreter} observed no tear at all, which
        // is only possible if the payload qword is already stored atomically.
        // (Which is what x86-64 gives: whatever a 16-byte struct store lowers
        // to, it never splits an 8-byte-aligned qword.)
        //
        // Transmuting the pair back into a `Value` needs no knowledge of the
        // enum's discriminant encoding, only its size — and every bit pattern
        // that can be assembled here is a valid `Value`, including a tag from
        // one write paired with a payload from another: the `Object` variant is
        // `Option<ObjectRef>`, whose niche makes a zero pointer word `None`
        // rather than an invalid `NonNull`.
        const _: () = assert!(std::mem::size_of::<Value>() == 16);
        // SAFETY: `base` points into a leaked, never-freed `StaticsBlock`, and
        // `field_index` was bounds-checked against the length published for
        // that same block. `base` is 8-aligned (it is a `Value` pointer), so
        // both words are aligned. The read stays unsynchronized against a
        // concurrent `putstatic` — as the JIT's inline `getfield` is for
        // instance-field cells — it is only no longer TORN.
        let cell = unsafe { base.add(field_index) } as *const u64;
        let lo = unsafe { cell.read_volatile() };
        let hi = unsafe { cell.add(1).read_volatile() };
        // SAFETY: `Value` is 16 bytes (asserted above) and these are its own
        // bytes, read from a live cell.
        Some(unsafe { std::mem::transmute::<[u64; 2], Value>([lo, hi]) })
    }

    /// Address of one published static cell, for a lock-free STORE
    /// (`vm_object::set_static_shared`'s steady state).
    ///
    /// Same validation as [`Self::get`]: `None` when the id is not indexable,
    /// nothing is published, or `field_index` is past the published length —
    /// the caller then takes the locked path, which creates or grows the block.
    /// The pointer stays valid for the life of the VM (blocks are leaked), but
    /// it names the block published NOW; see `set_static_shared` for what a
    /// concurrent republication means for a store through it.
    #[inline]
    pub fn cell_ptr(&self, class_id: ClassId, field_index: usize) -> Option<*mut Value> {
        let (base, len) = self.published(class_id)?;
        if field_index >= len {
            return None;
        }
        // SAFETY: bounds-checked against a length `published` guarantees does
        // not overstate `base`'s block, which is leaked.
        Some(unsafe { base.add(field_index) })
    }

    /// Address of the `AtomicPtr` **cell** that names a class's statics base —
    /// what compiled `getstatic` code bakes as an immediate.
    ///
    /// Deliberately NOT the block address. A `StaticsBlock` is stable for the
    /// life of the VM in the normal case, but two paths can still publish a
    /// different one for the same class: `StaticsBlock::grow_to` (a write past
    /// the published length) and a re-`prepare_class_shared`. Baking the block
    /// address would leave compiled code reading the abandoned copy — writes
    /// would land in the new block and never be observed. Baking the address of
    /// the pointer cell costs one extra dependent load and makes every
    /// republication visible to already-compiled code with no patching, no
    /// invalidation protocol and no new invariant to maintain: the slot array
    /// is allocated once (`OnceLock`) and never freed, so this address is valid
    /// forever.
    ///
    /// `None` = not indexable, nothing published yet, or `field_index` past the
    /// published length — the caller keeps the `jit_getstatic` helper path.
    pub fn base_cell_addr(&self, class_id: ClassId, field_index: usize) -> Option<usize> {
        let (_, len) = self.published(class_id)?;
        if field_index >= len {
            return None;
        }
        // `published` succeeded, so the id is indexable and the slots exist.
        let slot = &self.slots.get()?[class_id.as_u32() as usize];
        Some(&slot.base as *const std::sync::atomic::AtomicPtr<Value> as usize)
    }
}

/// This VM's lock-free "class is initialized" memo: the class manager's
/// [`cratonvm_classloading::ClassInitBits`] (one bit per dense `ClassId` for the
/// JIT's static-field helpers, and the init-reset generation the per-thread
/// memo checks). In the classloading crate since interpreter round i2 wave 61
/// (lane L5), so a stub upgrade can clear it under the write lock.
pub use cratonvm_classloading::ClassInitBits;

/// What this VM knows of one class's `NestHost` claim beyond its class file
/// ([`ClassRealm::nest_host_claims`]; JIT round 20 wave 16, lane nest).
/// HotSpot resolves a claim lazily, at the first private access check
/// between two classes (`InstanceKlass::nest_host`), by loading the host
/// through the member's defining loader; the member-access check of a
/// resolution miss cannot load, so it records the claim it could not judge
/// and a door that holds a thread loads the host
/// (`field_access::settle_nest_host_claims`).
#[derive(Clone, Debug)]
pub enum NestHostClaim {
    /// A private access check needed the claim and the host (`host`,
    /// internal name) is not known in the member's namespace. `member` is
    /// the member's external name, for the failure text.
    ///
    /// `seq` is the [`ClassRealm::nest_host_unsettled_reads`] value of the
    /// latest check that needed the claim: a door settles only the claims
    /// its own check noted (JIT round 20 wave 17, lane rev15), never another
    /// loader's unrelated claim.
    Pending {
        host: std::sync::Arc<str>,
        member: std::sync::Arc<str>,
        seq: u64,
    },
    /// A door of the thread `by` (`ThreadId.0`) is loading the host: a
    /// resolution that thread's `loadClass` runs keeps the old answer
    /// (uncached) instead of recursing; another thread loads it too.
    Loading {
        host: std::sync::Arc<str>,
        member: std::sync::Arc<str>,
        by: u64,
        seq: u64,
    },
    /// The host was loaded through the member's loader once. `Some(text)`:
    /// the load failed, and `text` is HotSpot's `Nest host resolution of
    /// <member> with host <host> failed: <throwable>` (the member is then its
    /// own nest host); `None`: it loaded, and the class store judges the
    /// claim (package, `NestMembers`).
    Settled(Option<std::sync::Arc<str>>),
}

/// Class loading, linking, resolution and per-class caches. Owns the L10 lock.
pub struct ClassRealm {
    /// Class loader and cache, protected by an RwLock.
    ///
    /// L10 in the global lock hierarchy — the coarsest lock, acquired first.
    /// See [`crate::runtime::lock_order`]. [`OrderedPlRwLock`] is a drop-in for
    /// `parking_lot::RwLock` that asserts the descending acquisition order on
    /// every `.read()` / `.write()` / `.read_recursive()`, so holding any
    /// lower-level lock (a monitor, `ref_processor`, …) across a class-manager
    /// acquisition is caught instead of deadlocking.
    pub class_manager: OrderedPlRwLock<ClassManager>,

    /// The class store's secondary-supers closures, readable WITHOUT
    /// [`Self::class_manager`]: a subtype question whose receiver class has a
    /// published closure is answered by one atomic load and a binary search
    /// (`typecheck::class_is_subtype`). The same `Arc` the store fills under
    /// the read lock and empties under the write lock; see
    /// `cratonvm_classloading::PublishedSupers` for why that needs no lock on
    /// the read side. Taken from the `ClassManager` below at construction —
    /// its `class_store` is never replaced.
    pub published_supers: Arc<cratonvm_classloading::PublishedSupers>,

    /// zs s9/ob6: the class store's next `ClassId`, readable WITHOUT
    /// [`Self::class_manager`] (`ClassStore::next_id_cell`): the ZGC
    /// `Metaspace` census at a collection edge
    /// (`vm_init.rs::class_store_cut`). Taken at construction, like
    /// [`Self::published_supers`].
    pub class_store_next_id: Arc<std::sync::atomic::AtomicU32>,

    /// This VM's verifier type maps, readable WITHOUT [`Self::class_manager`]
    /// (the interpreter's per-method fast-path gate,
    /// `interpreter::frame_fast_path_admitted`). The same `Arc` the class
    /// store's verifier publishes into; per VM because a `ClassId` is a
    /// per-store index (see `cratonvm_classloading::TypeMapStore`). Taken from
    /// the `ClassManager` at construction, like [`Self::published_supers`].
    pub type_maps: Arc<cratonvm_classloading::TypeMapStore>,

    /// Which of this VM's class ids name a `java/util/` class, readable
    /// WITHOUT [`Self::class_manager`] (the virtual-invoke door's tier-up gate,
    /// `dispatch_virtual`). The same bitmap the class manager sets at each
    /// definition; taken from it at construction, like
    /// [`Self::published_supers`]. Per VM since interpreter round i1 wave 22:
    /// the process-wide bitmap it replaces answered `true` for another VM's
    /// class at any id where one VM had a `java/util/` class.
    pub java_util_classes: cratonvm_classloading::JavaUtilClassBits,

    /// Which of this VM's class ids are hidden, readable WITHOUT
    /// [`Self::class_manager`] (JIT round 21 wave 14, lane hidden-static):
    /// the compiled dispatch paths' prefilter for a hidden caller's reference
    /// to itself (`jit::helpers::jit_hidden_self_owner`). The same bitmap the
    /// class manager sets at each hidden define; taken from it at
    /// construction, like [`Self::java_util_classes`].
    pub hidden_classes: cratonvm_classloading::HiddenClassBits,

    /// Whether this VM ever appended a class to its bootstrap search, readable
    /// WITHOUT [`Self::class_manager`] (round 14 wave 3, RV3-4): `false`
    /// answers `is_bootstrap_appended_class` with no lock. The same `Arc` the
    /// class manager sets in `extend_bootstrap_classpath`; taken from it at
    /// construction, like [`Self::java_util_classes`].
    pub bootstrap_append_seen: Arc<std::sync::atomic::AtomicBool>,

    /// This VM's "class is initialized" memo for the JIT static-field
    /// helpers; see [`ClassInitBits`].
    pub class_init_bits: ClassInitBits,

    /// This VM's redefinitions on the process redefinition clock
    /// (`cratonvm_classloading::RedefinitionLog`; nio21 wave 3 L1), readable
    /// WITHOUT [`Self::class_manager`]: the same `Arc` the class manager
    /// writes, taken from it at construction like [`Self::class_init_bits`].
    /// Each thread's invoke cache is bound to it at registration, so it
    /// retires for this VM's redefinitions only.
    pub redefinition_log: Arc<cratonvm_classloading::RedefinitionLog>,

    /// The class file each retransformed class of this VM was last installed
    /// from by `redefine_class_with`, with the class's redefine generation
    /// right after that install. Filled only under
    /// `CRATONVM_REDEFINE_IDENTICAL_FAST_PATH` (default ON since the i6 wave-126 flip, `=0` off): a retransform
    /// whose file is byte-identical, at an unchanged generation, installs
    /// nothing (`vm_exec::redefine_class_with`). Interpreter round i6 wave
    /// 117, lane L3 (the wave-28 note of
    /// `i14-L3-proposal-class-scoped-redefinition-invalidation`).
    pub installed_retransform_files: parking_lot::Mutex<FxHashMap<ClassId, (Arc<[u8]>, u32)>>,

    /// Lock-free cache of the shared `cratonvm/synthetic/AnonymousObject$N`
    /// ClassId, indexed by field count `N` (slot 0 is unused — `num_fields > 0`
    /// always on this path). `0` means "not yet resolved" — a valid sentinel
    /// because these synthetic classes are minted with non-zero ClassIds
    /// (`ClassId(0)` is `java/lang/Object`).
    ///
    /// `vm_exec::alloc_object` rewrites every `ClassId(0)`-with-fields
    /// allocation (e.g. a `HashMap` node) to one of these synthetic classes.
    /// The first allocation for a given `N` resolves it through
    /// `ClassManager::ensure_synthetic_class` (a write-lock + name `format!` +
    /// hash probe) and stores the id here; every later allocation reads the id
    /// with a single relaxed atomic load and skips the class-manager locks
    /// entirely. The synthetic stub declares exactly `N` fields, so the
    /// field-count clamp is a provable no-op and is skipped on the fast path.
    pub anon_class_cache: [std::sync::atomic::AtomicU32; ANON_CLASS_CACHE_LEN],

    /// Static fields: class_id -> field_index -> Value.
    /// T10.9.B: FxHashMap — keys are internal ClassId, hot path accessed
    /// on every getstatic/putstatic bytecode.
    pub statics: RwLock<FxHashMap<ClassId, StaticsBlock>>,

    /// Lock-free mirror of [`Self::statics`] for single-slot reads. Kept in
    /// step by `set_static_shared` / the class-init path, which publish every
    /// block they create or grow.
    pub statics_index: StaticsIndex,

    /// Initialized holders of static-final VarHandles, including a handle
    /// whose first checked access has yet to resolve its field slot. Pure ids,
    /// never heap roots; a native resolution refreshes these on its mutator.
    pub static_varhandle_holders: RwLock<FxHashSet<ClassId>>,

    /// The `static final VarHandle` fields (keyed `(declaring class id, static
    /// field index)`, the pair a `getstatic` resolves to) whose handle names ONE
    /// instance field of a `final` class and reads as an `int` or a `long`:
    /// everything the optimizing tier needs to fold `VarHandle.get*` on it into
    /// a guarded field load, as pure data.
    ///
    /// Filled on a MUTATOR thread by
    /// `jit::helpers::note_class_initialized_varhandles` when the declaring
    /// class's clinit completes, and refreshed when a native access resolves
    /// or adopts its handle after initialization. Read by the background compiler,
    /// which therefore never dereferences a heap object that a concurrent
    /// collection could be relocating. Entries are never removed: a `static
    /// final` is assigned once and a handle never changes its target.
    pub static_varhandle_fields: RwLock<FxHashMap<(u32, u32), cratonvm_jit::StaticVarHandleField>>,

    /// JIT round 20 wave 11 (lane pIR; L3W6-2 / W6L4-5): the LENGTH of each
    /// trusted `static final` array field (keyed `(declaring class id, static
    /// field index)`, the pair a `getstatic` resolves to), for the optimizing
    /// tier's `arraylength` fold (`ir_optimize::fold_trusted_static_array_lengths`).
    ///
    /// Filed on a MUTATOR thread by
    /// `jit::helpers::note_class_initialized_final_array_lengths` when the
    /// declaring class's `<clinit>` completes, under the trust rule of
    /// `CompileRequest::cp_static_field_final_resolver` (an application or
    /// user-loader class of class-file version 53+, so no method but
    /// `<clinit>` may write the field), and read by the background compiler as
    /// pure data: no heap object is ever dereferenced off the mutator. Entries
    /// are never removed: the field holds that one array forever, and an
    /// array's length never changes.
    pub static_final_array_lengths: RwLock<FxHashMap<(u32, u32), i32>>,

    /// The CLASS of the object each trusted `static final` reference field
    /// holds (keyed as [`Self::static_final_array_lengths`]), for the
    /// optimizing tier's exact receiver proof
    /// (`CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`). Filed on a mutator by
    /// `jit::helpers::note_class_initialized_final_ref_classes` when the
    /// declaring class's `<clinit>` completes and read by the compiler as pure
    /// data. Never removed: the field holds that one object forever, and an
    /// object never changes class.
    pub static_final_ref_classes: RwLock<FxHashMap<(u32, u32), u32>>,

    /// `java/lang/System`'s `ClassId` in THIS VM, or `u32::MAX` while unknown.
    ///
    /// Recorded by `prepare_class_shared`, which already has the class name in
    /// hand. Compiled `getstatic` uses it as a lock-free "does this read need
    /// the `System.out`/`err`/`in` bootstrap intercept?" test before deciding
    /// to emit a direct load: the intercept lives in `jit_getstatic`, so a
    /// static of this one class must never bypass the helper. Answering by
    /// name would need a `class_manager` acquisition on the compiler thread,
    /// which is the one thing the resolver must not do.
    ///
    /// Per-`ClassRealm`, so unlike the deleted process-global `system_class_id`
    /// atomic it cannot leak one VM's id into another's decisions.
    ///
    /// Also the interpreter's screen (`opcodes::static_may_be_system`), which
    /// records the id itself if a `getstatic` runs before `System` is
    /// prepared; it replaced the process-global `classloading` latch in i1
    /// wave 5.
    pub system_class_id: AtomicU32,

    /// `java/lang/Boolean`'s `ClassId` in THIS VM, or `u32::MAX` while unknown.
    ///
    /// Recorded by `prepare_class_shared` beside [`Self::system_class_id`], for
    /// the same reason: `op_getstatic` repairs a synthetic `Boolean` whose
    /// `TRUE`/`FALSE` statics were never initialized, and it used to decide
    /// "is this `Boolean`?" by taking the `class_manager` read lock and
    /// comparing the class NAME on every `getstatic` that read back `null`.
    pub boolean_class_id: AtomicU32,

    /// Cache of resolved symbolic references (fields and methods).
    pub resolution_cache: RwLock<ResolutionCache>,

    /// Set once this VM has recorded a JVMS §5.4.3 resolution failure in
    /// [`Self::resolution_cache`]; never reset. A pre-filter only: the table is
    /// the authority, and this bit keeps its read lock off the resolution miss
    /// paths of a VM that has never failed a resolution (almost every
    /// program). A process static (`ANY_RESOLUTION_FAILURE_RECORDED` in
    /// `interpreter/constants.rs`) until interpreter round i1 wave 25, lane
    /// L5: one VM's first failure made every VM probe the table.
    pub resolution_failure_recorded: std::sync::atomic::AtomicBool,

    /// How many field / method resolution misses in THIS VM met a JVMS §5.4.4
    /// refusal — of the member, or of the class the member reference names —
    /// that CratonVM did not enforce before interpreter round i1 wave 26
    /// (lane L5). `--jdk-only` throws `IllegalAccessError` for each;
    /// `--compatible` admits and only counts them (the census the next stage
    /// of `interpreter-L5-member-access-...` is decided on).
    /// `CRATONVM_DBG=access` traces each one as `[ACCESS-DBG] MEMBER`, with
    /// this running count. Per VM, like every compatibility decision.
    pub member_access_refusals: std::sync::atomic::AtomicU64,

    /// How many method resolution misses in THIS VM found a JVMS §5.4.4
    /// refusal against an owner that only the loader-blind fallback knew
    /// (`resolve_method_metadata`: not the caller, a direct supertype, the
    /// loader's own or recorded classes, nor its initiating memo). Never
    /// enforced — the owner may be another loader's same-named copy, whose
    /// runtime package is not the reference's — and never cached, so a later
    /// resolution that knows the owner decides. Interpreter round i1 wave 27,
    /// lane L5; printed with [`Self::member_access_refusals`] at exit under
    /// `CRATONVM_DBG=access`.
    pub member_access_unverified: std::sync::atomic::AtomicU64,

    /// How many field and method resolutions in THIS VM met a JVMS §5.3.4
    /// loader-constraint violation: the resolving class and the member's
    /// declaring class, of different loaders, each already see a different
    /// class for a type the member's descriptor names
    /// (`field_access::loader_constraint_census`): refused under `--jdk-only`,
    /// counted under `--compatible`
    /// (`i29-L5-loader-constraints-are-not-imposed-at-member-resolution`,
    /// interpreter round i1 wave 31); printed with
    /// [`Self::member_access_refusals`] at exit under `CRATONVM_DBG=access`.
    pub loader_constraint_violations: std::sync::atomic::AtomicU64,

    /// The `NestHost` claims of this VM's classes that a private access
    /// check could not judge without loading, and what loading their host
    /// answered ([`NestHostClaim`]; JIT round 20 wave 16, lane nest). Keyed
    /// by the member (class ids are never reused). Taken under the
    /// class-manager guard by the access check, never the other way round,
    /// and never held across a load.
    pub nest_host_claims: parking_lot::Mutex<FxHashMap<ClassId, NestHostClaim>>,

    /// How many times a member-access check of THIS VM answered from an
    /// unsettled [`NestHostClaim::Pending`] claim (admitted as before, or
    /// refused without the host's text). A compile door that resolves
    /// without a thread compares it around its resolution and binds nothing
    /// when it moved (JIT round 20 wave 16, lane nest).
    pub nest_host_unsettled_reads: std::sync::atomic::AtomicU64,

    /// How many receiver-driven `invokevirtual` / `invokespecial` /
    /// `invokeinterface` slow paths in THIS VM named an owner the caller's
    /// loader did not know (`invoke::resolve_owner_unknown_to_caller`,
    /// interpreter round i1 wave 27, lane L5). `--jdk-only` resolves the owner
    /// through the caller's loader there; `--compatible` dispatches on the
    /// receiver as before and only counts.
    pub receiver_owner_unresolved: std::sync::atomic::AtomicU64,

    /// How many VM-initiated class resolutions in THIS VM failed with the
    /// initiating loader's own throwable, which the global fallback used to
    /// replace (`--jdk-only`, a loader whose `loadClass` is its own bytecode;
    /// `runtime::resolve::loader_throw`, interpreter round i1 wave 38, lane
    /// L5). Printed at exit under `CRATONVM_DBG=access`: a real workload must
    /// read zero, or each traced row is a class HotSpot would fail too.
    pub loader_throws_propagated: std::sync::atomic::AtomicU64,

    /// How many class resolutions in THIS VM whose initiating loader refused
    /// (or threw) took another thread's answer published meanwhile for the same
    /// loader and name instead (JVMS §5.4.3, the loser adopts the winner), or
    /// rethrew the failure another thread recorded for the entry after this
    /// one's own resolution succeeded (`--jdk-only`; interpreter round i1 wave
    /// 38, lane L5). Printed with [`Self::loader_throws_propagated`].
    pub resolution_race_adoptions: std::sync::atomic::AtomicU64,

    /// How many VM-initiated loads in THIS VM re-entered a non-parallel-capable
    /// user loader for a name the same thread was already loading through it,
    /// and failed with `ClassCircularityError` as on HotSpot (`--jdk-only`;
    /// `constants.rs` `drive_defining_loader_load_named`, wave 38, lane L5).
    /// Printed with [`Self::loader_throws_propagated`].
    pub loader_reentry_circularities: std::sync::atomic::AtomicU64,

    /// How many VM-initiated loads in THIS VM re-entered a PARALLEL-CAPABLE
    /// user loader from the same referencing class for a name the same thread
    /// was already loading through it, and asked the loader again as HotSpot
    /// does instead of falling back to the global store (`--jdk-only`;
    /// `constants.rs` `drive_defining_loader_load_named`, interpreter round i1
    /// wave 39, lane L5). Printed with [`Self::loader_throws_propagated`].
    pub loader_reentry_reasks: std::sync::atomic::AtomicU64,

    /// How many flat-store misses of the base `ClassLoader.loadClass` native
    /// in THIS VM did not drive the calling frame's loader
    /// ([`Self::load_class_asks_receiver_only`]; interpreter round i2 wave 53,
    /// lane L5): each is a `loadClass` of the caller's loader (often the
    /// receiver itself, a second `URLClassPath` walk) that the old route made
    /// and HotSpot does not. Printed with [`Self::loader_throws_propagated`]
    /// (`caller-drives-skipped=`); informational.
    pub load_class_caller_drives_skipped: std::sync::atomic::AtomicU64,

    /// `CRATONVM_DBG=access` only (interpreter round i6 wave 116, lane L5;
    /// step 3 of `i53-L5-proposal-ctx-load-class-takes-an-explicit-initiating-loader`):
    /// every drive of the calling frame's user loader made for a native's
    /// by-name lookup (`NativeContextImpl::class_via_caller_loader`), keyed by
    /// `"<when> <innermost frame class>.<method>"` (`when`: `before-stub` for a
    /// name that would fabricate a stub, `after-miss` for a flat-store miss),
    /// counting `[drives, answered]`. The rows say which native families rely on
    /// the implicit caller loader, i.e. what an explicit initiating loader must
    /// port first. Printed by `report_loader_census_at_exit` as
    /// `CALLER-DRIVE` rows. Bounded at `CALLER_DRIVE_CENSUS_CAP` rows.
    pub caller_drive_census: OrderedPlMutex<std::collections::HashMap<String, [u64; 2]>>,

    /// `CRATONVM_DBG=access` only (interpreter round i6 wave 122, lane L5;
    /// `i39-L5-proposal-one-loader-request-door-with-an-explicit-kind`): every
    /// VM-initiated user-loader request through the one prologue
    /// (`initiating_records::loader_request_prologue`), keyed by
    /// `"<kind> <door>"`, counting `[requests, answered by the dictionary]`.
    /// Printed as `LOADER-REQUEST` rows by `report_loader_census_at_exit`.
    pub loader_request_census: OrderedPlMutex<std::collections::HashMap<String, [u64; 2]>>,

    /// Calls of the `URLClassPath$Loader` walk intrinsic in THIS VM
    /// (`CRATONVM_NESTED_JAR_WALK_INDEX`; interpreter round i2 wave 54, lane
    /// L5) that the nested-jar index answered `null` without running the JDK
    /// body (`nested-jar-walk-skipped=`), and those that ran it
    /// (`nested-jar-walk-asked=`). Printed with
    /// [`Self::loader_throws_propagated`]; informational: the positive control
    /// of the switch.
    pub nested_jar_walk_skipped: std::sync::atomic::AtomicU64,
    /// See [`Self::nested_jar_walk_skipped`].
    pub nested_jar_walk_asked: std::sync::atomic::AtomicU64,
    /// Calls of the nested-jar bytes intrinsic on `Resource.getBytes` in THIS
    /// VM (`CRATONVM_NESTED_JAR_BYTES_INDEX`; interpreter round i2 wave 63,
    /// lane L5) over the generic `URLClassPath$Loader`'s `Resource` that the
    /// VM's copy of the nested jar answered (`nested-jar-bytes-served=`), and
    /// those that ran the JDK body (`nested-jar-bytes-body=`). Printed with
    /// [`Self::loader_throws_propagated`]; informational: the positive control
    /// of the switch.
    pub nested_jar_bytes_served: std::sync::atomic::AtomicU64,
    /// See [`Self::nested_jar_bytes_served`].
    pub nested_jar_bytes_body: std::sync::atomic::AtomicU64,
    /// Calls of the jar-loader bytes intrinsic on `Resource.getBytes` in THIS
    /// VM (`CRATONVM_JAR_LOADER_BYTES_INDEX`; interpreter round i2 wave 64,
    /// lane L5) over a `URLClassPath$JarLoader` `Resource` that the VM's
    /// mapping of the jar answered (`jar-loader-bytes-served=`), and those
    /// that ran the JDK body (`jar-loader-bytes-body=`). Printed with
    /// [`Self::loader_throws_propagated`]; informational: the positive control
    /// of the switch.
    pub jar_loader_bytes_served: std::sync::atomic::AtomicU64,
    /// See [`Self::jar_loader_bytes_served`].
    pub jar_loader_bytes_body: std::sync::atomic::AtomicU64,
    /// The nested-jar rows' events in THIS VM
    /// (`cratonvm_native_api::NestedJarRowEvent`, one counter each, in the
    /// enum's order: built, rebuilt, refused, refusals retried, refusals
    /// kept, class-path lends, kept-handle reads; interpreter round i2 wave
    /// 69, lane L5; then jar-loader class-path lends, wave 70; then
    /// jar-loader class-path direct serves and their fallbacks, interpreter
    /// round i3 wave 71; then nested bytes-row stats skipped for a pooled
    /// read, wave 71). Printed with [`Self::loader_throws_propagated`];
    /// informational.
    pub nested_jar_row_events: [std::sync::atomic::AtomicU64; 11],

    /// How many VM-initiated class resolutions in THIS VM failed with
    /// `NoClassDefFoundError` because the initiating loader's `loadClass`
    /// returned `null` or a class of another name, which the global fallback
    /// used to replace (`--jdk-only`, a loader whose `loadClass` is its own
    /// bytecode; `runtime::resolve::loader_throw::loader_null_as_resolution_error`,
    /// interpreter round i1 wave 39, lane L5). Printed with
    /// [`Self::loader_throws_propagated`]; a real workload must read zero.
    pub loader_nulls_propagated: std::sync::atomic::AtomicU64,

    /// Set once the `CRATONVM_DBG=access` exit line of the two counters above
    /// has been printed for this VM
    /// (`runtime::resolve::loader_throw::report_loader_census_at_exit`).
    pub loader_census_reported: std::sync::atomic::AtomicBool,

    /// Set once the `CRATONVM_DBG=access` exit total has been printed for this
    /// VM (the launcher's `System.exit` hook and its `main`-returned arm both
    /// ask).
    pub member_access_census_reported: std::sync::atomic::AtomicBool,

    /// Round 8 audit fix (CRIT #2): the reflective `(class, name,
    /// descriptor)` cache. Previously built (`LinkResolver::new()`) but
    /// never wired into any caller, so the entire dedupe win was dead
    /// code. Now reachable from native reflective callers
    /// (`Class.getDeclaredMethod`, `Class.getMethod`, JNI
    /// `GetMethodID`/`GetFieldID`) via `vm.link_resolver()`. The
    /// cache is invalidated on `redefine_class` through the same
    /// hook that drops `ResolutionCache` entries (see
    /// `link_resolver_invalidate_adapter`).
    pub link_resolver: LinkResolver,

    /// T10 — vtable manager for virtual/interface dispatch.
    ///
    /// Shared as an `Arc` so the class-loader install hook (a plain
    /// `fn` pointer with no captured state) can reach the same manager
    /// through `crate::runtime::vtable::register_vm_vtable_manager`, keyed
    /// by this VM's `vm_identity` (the scoped hook's `ClassHandle` names it).
    pub vtable_manager: std::sync::Arc<parking_lot::RwLock<crate::runtime::vtable::VtableManager>>,

    /// T10 — read-optimized shared resolution cache (uses RwLock internally).
    pub shared_resolution: crate::runtime::lockfree_resolve::SharedResolutionState,

    /// Per-class synthetic lock objects for static synchronized methods.
    /// When a static synchronized method is called, we need an object to use
    /// as the monitor (since there's no `this`). We allocate a dummy object
    /// per class and store it here.
    /// T10.9.B: FxHashMap — ClassId-keyed internal cache.
    pub class_locks: RwLock<FxHashMap<ClassId, ObjectRef>>,

    /// Class mirror cache: maps ClassId to java/lang/Class ObjectRef.
    /// Used by `Object.getClass()`.
    /// T10.9.B: FxHashMap — ClassId-keyed.
    pub class_mirrors: RwLock<FxHashMap<ClassId, ObjectRef>>,

    /// Reverse of `class_mirrors`: maps a Class mirror back to its ClassId.
    /// Populated whenever `get_or_create_class_mirror` allocates a new mirror.
    /// This lets `mirror_class_id` recover the ClassId without storing it in
    /// the mirror's Java-visible fields (which would clash with the real-JDK
    /// `java/lang/Class` layout).
    /// T10.9.B: FxHashMap — ObjectRef pointer keys.
    pub class_mirrors_reverse: RwLock<FxHashMap<ObjectRef, ClassId>>,

    /// JIT round 18 wave 10, lane buffers (`CRATONVM_CLASS_ID_FROM_MIRROR_TABLE`,
    /// opt-in, read once per VM here): a lock-free, generation-validated cache
    /// of [`Self::class_mirrors_reverse`] for `NativeContext::class_id_from_mirror`
    /// (`vm_exec.rs`). Empty and never consulted when the switch is off. See
    /// `cratonvm_native_builtins::mirror_id_memo::MirrorIdTable`.
    pub class_mirror_id_table: cratonvm_native_builtins::mirror_id_memo::MirrorIdTable,

    /// JIT round 20 (nio20 wave 1, lane L3, NIO19-5): the GC-maintained
    /// `class id -> mirror` words compiled `getClass()` reads inline. See
    /// [`GetClassMirrorWords`] for its writers; every one of them runs under
    /// the [`Self::class_mirrors`] lock.
    pub getclass_mirror_words: GetClassMirrorWords,

    /// Loader-faithful resolution cache (gated by
    /// `CRATONVM_LOADER_AWARE_RESOLUTION`). Maps an *initiating* loader and an
    /// internal class name to the `ClassId` that loader resolves it to —
    /// CratonVM's analogue of the JVMS §5.4.3 *initiating loader* table. Only
    /// populated for references reached from bytecode defined by a user-defined
    /// loader: once `resolve_class_loader_aware` has driven that loader's
    /// `loadClass` (or proved the name is bootstrap/global) for a given name,
    /// the answer is memoised here so subsequent references skip the re-entrant
    /// `loadClass` invocation. Grow-only — CratonVM does not unload classes, and
    /// in-place `redefine_class` keeps the `ClassId` stable. Empty (and never
    /// read) when the gate is off.
    ///
    /// Nested `loader -> (name -> id)` so a hot-path read can probe by borrowed
    /// `&str` (`Arc<str>: Borrow<str>`) without allocating an `Arc` per lookup.
    pub initiating_resolution_cache:
        RwLock<FxHashMap<cratonvm_types::ClassLoaderId, FxHashMap<Arc<str>, ClassId>>>,

    /// JVMS §5.3 initiating-loader RECORDS, `--jdk-only`: a user-defined
    /// loader that the VM asked for a name (the class-resolution door, or
    /// `Class.forName` with the loader) and that answered ANOTHER loader's
    /// class. VM state, not a memo: never capped or evicted while the loader
    /// lives, first answer kept; rows naming an unloaded class, and a whole
    /// emptied namespace, go with the unload sweep (`memory::gc`). Read by the
    /// loader's `findLoadedClass` and by the define path, which refuses the
    /// loader's own definition of a recorded name (§5.3.5), as HotSpot's
    /// dictionary does. `runtime::resolve::initiating_records`; interpreter
    /// round i1 wave 38, lane L5. Empty under `--compatible`.
    pub initiating_records:
        RwLock<FxHashMap<cratonvm_types::ClassLoaderId, FxHashMap<Arc<str>, ClassId>>>,

    /// `findLoadedClass` answers served from [`Self::initiating_records`]
    /// (`--jdk-only`; printed in the `CRATONVM_DBG=access` loader census).
    /// Wave 38, lane L5.
    pub initiating_record_find_hits: std::sync::atomic::AtomicU64,
    /// Definitions [`Self::initiating_records`] refused (`--jdk-only`; printed
    /// in the `CRATONVM_DBG=access` loader census). Wave 38, lane L5.
    pub initiating_record_define_refusals: std::sync::atomic::AtomicU64,

    /// Per non-bootstrap loader: the JDK classes (bootstrap-defined, or
    /// platform-defined for a loader other than the platform loader) its
    /// classes' `CONSTANT_Class` resolutions (while
    /// [`Self::resolved_entry_table`] is off: on, those go to
    /// [`Self::resolved_class_entries`]), `Class.forName` with the loader,
    /// and -- since wave 47 -- the reflective lookups (the launcher's, an
    /// agent's, the reflection natives') answered, which HotSpot files in the
    /// loader's dictionary and `Instrumentation.getInitiatedClasses` lists.
    /// An array resolution records its element class. Written only while
    /// [`Self::jdk_names_initiated_armed`] is set; rows naming an unloaded
    /// class go with the unload sweep (`memory::gc`).
    /// `runtime::resolve::initiating_records::record_jdk_name_initiated`;
    /// interpreter round i1 waves 46-47, lane L5.
    pub jdk_names_initiated: RwLock<FxHashMap<cratonvm_types::ClassLoaderId, FxHashSet<ClassId>>>,
    /// Set at VM creation when [`Self::resolved_entry_table`] is on, else
    /// once (either mode since wave 47) when the start-up `-javaagent` agents
    /// are about to run (`agent_loader::invoke_premains`): only an `Instrumentation` can read
    /// [`Self::jdk_names_initiated`], and an agent made at start-up sees every
    /// resolution of the application from its first. A VM with no start-up
    /// agent writes nothing (one relaxed load per class resolution) and
    /// lists by the constant-pool scan (`vm_exec.rs`
    /// `list_initiated_class_ids`).
    pub jdk_names_initiated_armed: std::sync::atomic::AtomicBool,

    /// The resolved class-entry table (JVMS §5.4.3's record of a class
    /// entry, first stage): per referencing class, the classes its class
    /// entries resolved to -- through the `CONSTANT_Class` door or as a
    /// method reference's owner -- an array with its element. A row naming
    /// another loader's class makes the referencing class's loader an
    /// initiating loader of it (HotSpot's dictionary row), which
    /// `Instrumentation.getInitiatedClasses` lists. `None` for a class of the
    /// bootstrap loader (nothing to record, asked once). Written on a door's
    /// miss only, and only while [`Self::resolved_entry_table`] is on; a row
    /// naming an unloaded class, and an unloaded referencing class's rows,
    /// go with the unload sweep (`memory::gc`).
    /// `runtime::resolve::initiating_records::note_resolved_class_entry`;
    /// interpreter round i1 wave 47, lane L5
    /// (`i46-L5-proposal-one-resolved-class-entry-table-every-door-writes`).
    pub resolved_class_entries: RwLock<FxHashMap<ClassId, Option<FxHashSet<ClassId>>>>,
    /// The switch of [`Self::resolved_class_entries`]:
    /// `CRATONVM_RESOLVED_ENTRY_TABLE`, read once at VM creation, on by
    /// default (`=0` turns it off). On, the
    /// table is written, [`Self::jdk_names_initiated`] is armed from VM
    /// creation in both modes (so an agent attached late and `--compatible`
    /// read the same record), and `getInitiatedClasses` reads the records
    /// only -- no constant-pool scan.
    pub resolved_entry_table: bool,
    /// `CRATONVM_INDY_OTHER_SHAPES_TO_JDK`, read once at VM creation, on by
    /// default (`=0` turns it off): an
    /// `invokedynamic` of one of the four natively linked JDK bootstraps
    /// (`LambdaMetafactory`, `StringConcatFactory`, `SwitchBootstraps`,
    /// `ObjectMethods`) whose shape is not one javac emits, or that the
    /// native checks refuse or cannot decide, is linked through the JDK's own
    /// bootstrap (`invokedynamic::bootstrap_generic`), which raises the JDK's
    /// own error or builds the JDK's own call site. Interpreter round i1
    /// wave 48, lane L4
    /// (`i38-L4-proposal-native-bootstraps-link-only-javac-shapes-natively`).
    pub indy_other_shapes_to_jdk: bool,
    /// `CRATONVM_LMF_REFLECTIVE_TO_JDK`, read once at VM creation, on by
    /// default since wave 51 (`=0` turns it off): a DIRECT (reflective) call
    /// of `LambdaMetafactory.metafactory` / `altMetafactory` runs the JDK's own factory (its checks, its hidden
    /// proxy class) instead of the registered reflective `Bridge`
    /// (`native-builtins/src/lang_invoke.rs`), in every mode. Interpreter
    /// round i1 wave 49, lane L4
    /// (`i48-L4-proposal-retire-the-reflective-lambdametafactory-bridge`).
    pub lmf_reflective_to_jdk: bool,
    /// `CRATONVM_VH_EXACT_SITE_MEMO`, read once at VM creation: an exact
    /// `VarHandle` judged at a thin helper's compiled site recovers the site's
    /// descriptor from a per-thread memo keyed on the innermost compiled
    /// activation (`JvmThread::vh_exact_site_memo`) instead of walking the
    /// compiled frames on every access. On by default, `=0` turns it off.
    /// Interpreter round i1 wave 50, lane L4
    /// (`i49-L4-proposal-memoize-the-exact-varhandle-site-recovery`).
    pub vh_exact_site_memo: bool,
    /// Do the natively linked bootstraps (`invokedynamic.rs`: the lambda
    /// implementation's resolution and the conversion checks of
    /// `lambda_site_refusal`, the concat `Class` constant load, the switch
    /// and record checks and getter resolution, the static argument casts of
    /// `link_generic_call_site`, `callsite_target_type_mismatch`) make the
    /// JDK's checks? `--jdk-only` always; `--compatible` when
    /// `CRATONVM_INDY_CHECKS_EVERY_MODE` is on (read once at VM creation, on
    /// by default, `=0` turns it off). Interpreter round i1 wave 49, lane L4
    /// (`interpreter-L4-compatible-mode-skips-the-jdk-only-indy-checks-FIXED-20261013`):
    /// both modes are held to HotSpot.
    pub indy_link_checks: bool,
    /// Do the invoke doors make the JDK-only selection checks (JVMS §5.4.6 /
    /// §6.5: `invokespecial` selection and its errors, an `invokeinterface`
    /// receiver that does not implement the interface -- an array, a plain
    /// `Object`, a pinned private method's receiver -- and a methodref whose
    /// tag disagrees with its owner's kind), in the interpreter and at the
    /// compile and compiled doors? `--jdk-only` always; `--compatible` when
    /// `CRATONVM_SELECTION_CHECKS_EVERY_MODE` is on (read once at VM
    /// creation, on by default, `=0` turns it off). Interpreter round i1 wave
    /// 50, lane L4: both modes are held to HotSpot.
    pub selection_checks: bool,
    /// `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS` (JIT round 20 wave 8c, lane
    /// huntDisp; opt-in, read once per VM): `Unsafe` instance-field offsets
    /// are byte offsets `16 + slot * 8 + k` instead of slot indices, so an
    /// offset inside a field addresses that field's bytes as on HotSpot
    /// (`native-builtins` `unsafe_encode_field_offset` /
    /// `unsafe_field_slot`). Every producer and every reader of an offset
    /// asks this one answer (natives through
    /// `NativeSystemAccess::unsafe_byte_field_offsets`, the JIT through
    /// this field).
    pub unsafe_byte_field_offsets: bool,
    /// Do the `MethodHandles` combinator natives (`filterArguments`,
    /// `filterReturnValue`, `foldArguments`, `collectArguments`,
    /// `catchException`, `permuteArguments`, `guardWithTest`,
    /// `dropArguments`) make the JDK's argument checks? `--jdk-only` always;
    /// `--compatible` when `CRATONVM_MH_COMBINATOR_CHECKS_EVERY_MODE` is on
    /// (read once at VM creation, on by default, `=0` turns it off).
    /// Interpreter round i1 wave 50, lane L4.
    pub mh_combinator_checks: bool,
    /// Does an `ldc` of a `CONSTANT_MethodHandle` make the JVMS §5.4.4 access
    /// check of its member from the class holding the constant
    /// (`interpreter::constants`, `method_handle_constant_access_refusal`)?
    /// `--jdk-only` always; `--compatible` when
    /// `CRATONVM_LDC_HANDLE_ACCESS_EVERY_MODE` is on (read once at VM
    /// creation, on by default, `=0` turns it off). Interpreter round i1
    /// wave 50, lane L4.
    pub ldc_handle_access_checks: bool,

    /// `CRATONVM_METHOD_TYPE_VM_TYPES`, read once at VM creation: a
    /// `CONSTANT_MethodType` resolution
    /// (`interpreter::constants::resolve_method_type_constant`) resolves its
    /// types in the VM as HotSpot's `SystemDictionary::find_method_handle_type`
    /// does -- the leading "always visible" types (public classes of
    /// `java.lang` / `java.lang.invoke`, primitives) with the bootstrap
    /// loader, asking the class's loader for none of them, the rest through
    /// that loader -- and builds the `MethodType` with one
    /// `MethodHandleNatives.findMethodHandleType` upcall. Off: the JDK's
    /// `MethodType.fromMethodDescriptorString(desc, loader)`, which asks the
    /// loader for every reference type (interpreter round i1 wave 48, lane
    /// L5; `interpreter-L5-proposal-method-types-resolve-their-types-in-the-vm-FIXED-20261012`).
    pub method_type_vm_types: bool,

    /// Are the JVMS §5.3 initiating-loader records
    /// ([`Self::initiating_records`], `runtime::resolve::initiating_records`)
    /// kept? `--jdk-only`, or `--compatible` with
    /// `CRATONVM_COMPAT_INITIATING_RECORDS` (read once at VM creation).
    /// Interpreter round i1 wave 48, lane L5
    /// (`interpreter-L5-compatible-resolution-door-gates-that-still-differ-from-hotspot-FIXED-20261015`,
    /// item 5).
    pub initiating_records_on: bool,

    /// Are JVMS §5.3.4 loader constraints recorded and enforced
    /// (`runtime::resolve::loader_constraints`)? `--jdk-only`, or
    /// `--compatible` with `CRATONVM_COMPAT_LOADER_CONSTRAINTS` (read once at
    /// VM creation). Off, `--compatible` only counts a violation both sides
    /// already see. Interpreter round i1 wave 49, lane L5 (the i47-L5
    /// `--compatible` gates page, item 6).
    pub loader_constraints_on: bool,

    /// The HotSpot-alignment rows in force for this VM, EFFECTIVE: every row
    /// under `--jdk-only`, the VM's set (`cratonvm_types::hotspot_alignment`,
    /// read once in `vm_init`) under `--compatible`. Asked through
    /// [`Self::aligned`], one bit test, by the VM-side doors that a row
    /// covers (interpreter round i1 wave 50, lane L5).
    pub hotspot_aligned: cratonvm_types::HotSpotAlignments,

    /// How often each alignment row answered other than its row-off route
    /// would have (`cratonvm_types::AlignmentCensus`), shared with the class
    /// manager; bumped through [`Self::note_alignment_divergence`] and by
    /// the natives through `NativeContext::note_alignment_divergence`, and
    /// printed as the exit `ALIGNMENT census` line under
    /// `CRATONVM_DBG=access`. Interpreter round i1 wave 51, lane L5.
    pub alignment_census: std::sync::Arc<cratonvm_types::AlignmentCensus>,

    /// Is a user loader's throwable from a VM-driven `loadClass` the
    /// resolution's error (JVMS §5.3; `constants.rs`
    /// `drive_defining_loader_load_named`, the checked door)? `--jdk-only`,
    /// or `--compatible` with `CRATONVM_COMPAT_LOADER_THROWS` (read once at
    /// VM creation). Off, `--compatible` drops the throw and the global
    /// route answers. Interpreter round i1 wave 49, lane L5 (the i47-L5
    /// `--compatible` gates page, item 1).
    pub loader_throws_on: bool,

    /// Is a user loader that may answer a JDK-global name (`javax/`, `jdk/`,
    /// `sun/`, `com/sun/`) itself asked for it before the global route
    /// (`constants.rs` `drive_loader_for_global_name`)? `--jdk-only`, or
    /// `--compatible` with `CRATONVM_COMPAT_GLOBAL_NAME_ASK` (read once at VM
    /// creation). Interpreter round i1 wave 49, lane L5 (the i47-L5
    /// `--compatible` gates page, item 3).
    pub global_name_ask_on: bool,

    /// Under `--jdk-only`, does a user loader's initiating RECORD
    /// ([`Self::initiating_records`]) replace the capped memo
    /// ([`Self::initiating_resolution_cache`]) as its answer for a name
    /// outside the JDK-global namespaces (`constants.rs`
    /// `loader_initiated_answer`)? `CRATONVM_RECORD_REPLACES_MEMO`, read once
    /// at VM creation; always `false` under `--compatible`. Interpreter round
    /// i1 wave 49, lane L5
    /// (`i38-L5-proposal-the-initiating-record-replaces-the-memo-under-jdk-only`).
    pub record_replaces_memo: bool,

    /// Under `--compatible` with the `RecordBehindMemo` HotSpot-alignment
    /// row (and the `InitiatingRecords` row), does a user loader's initiating
    /// RECORD answer a name the capped memo has no row for (evicted, or never
    /// written: `Class.forName`), so the loader is not asked again
    /// (`initiating_records.rs` `loader_initiated_answer`)? Read once at VM creation;
    /// always `false` under `--jdk-only`, which has
    /// [`Self::record_replaces_memo`]. Interpreter round i1 wave 52, lane L5
    /// (the i38-L5 proposal's `--compatible` stage).
    pub record_behind_memo: bool,

    /// Does the base `ClassLoader.loadClass` native's flat-store step
    /// (`classloader_real::load_class_visible_to`, through
    /// `NativeClassAccess::load_class_for_loader_receiver`) skip the drive of
    /// the calling frame's defining loader that a global miss takes in
    /// `NativeContextImpl::load_class` (`class_via_caller_loader`)? HotSpot's
    /// `loadClass` asks only the receiver and its parents; the drive asked the
    /// CALLER's loader -- for a loader asked from its own classes (Spring Boot's
    /// `LaunchedClassLoader` from Spring's code), the receiver again, so a
    /// name it does not have walked its whole `URLClassPath` twice. Both modes;
    /// read once at VM creation (`CRATONVM_LOAD_CLASS_ASKS_RECEIVER_ONLY`,
    /// default on). Interpreter round i2 wave 53, lane L5 (the
    /// `unflattened-app-class-path` start-up cost).
    pub load_class_asks_receiver_only: bool,

    /// Does linking an application class-path class run the JDK's
    /// `BuiltinClassLoader.defineOrCheckPackage(pn, man, url)` for it
    /// (`vm_util::define_class_path_package_at_link`), the step HotSpot's
    /// `BuiltinClassLoader.defineClass` takes before it defines the class: the
    /// package is defined into the loader's map with its first class, and a
    /// sealing violation fails the class with the JDK's `SecurityException`?
    /// Read once at VM creation (`CRATONVM_CLASS_PATH_PACKAGE_AT_LINK`); it
    /// engages only where `CRATONVM_JDK_BUILT_PACKAGES` does. Interpreter round
    /// i2 wave 62, lane L5 (the i58-L5 proposal's next stage).
    pub class_path_package_at_link: bool,

    /// Do the doors that hand out an application class-path class WITHOUT
    /// linking it -- the `CONSTANT_Class` resolver (`ldc C.class`,
    /// `checkcast`, `instanceof`, `anewarray`; `constants::resolve_class_loader_aware`)
    /// and the `--compatible` `ClassLoader.loadClass` natives -- run the
    /// package step of [`Self::class_path_package_at_link`] first
    /// (`vm_util::check_class_path_package_unlinked_at_door`), so a class of a
    /// sealed package from another code source is refused there with the
    /// JDK's `SecurityException`, as HotSpot refuses it at its define? Read
    /// once at VM creation (`CRATONVM_UNLINKED_DOORS_CHECK_PACKAGE`, default
    /// on, `=0` off). Interpreter round i4 wave 94, lane L5
    /// (`i92-L5-unlinked-class-path-class-doors-skip-the-package-step`).
    pub unlinked_doors_check_package: bool,

    /// Does `vm_util::link_claimed_class` move a class from `Verifying` to
    /// `Preparing` under ONE class-manager write guard instead of two (the
    /// `Verified` write and the `Preparing` write)? The intermediate `Verified`
    /// state was visible to other threads only between the two sections, and
    /// nothing waits on it; every reader that asks "linked?" answers the same
    /// for both states (`class_linked_or_link_trivial`). It saves one write
    /// acquisition and one guard release (whose drop re-sweeps every held
    /// deferred-`new` retry) per class link. Read once at VM creation
    /// (`CRATONVM_LINK_STATE_ONE_WRITE`, default off). Interpreter round i4
    /// wave 91, lane L5 (the i87 define-cost page's `link-prepare` row).
    pub link_state_one_write: bool,

    /// The `(package, code source)` pairs [`Self::class_path_package_at_link`]
    /// has had the JDK accept (or found the step not to apply to): the JDK's
    /// answer depends on nothing else, so a later class of the pair costs no
    /// Java call. A refused pair is not
    /// recorded (the next class of it is refused again, as HotSpot refuses
    /// every define of it).
    pub class_path_packages_checked: parking_lot::Mutex<rustc_hash::FxHashSet<(String, String)>>,

    /// The `(package, code source)` pairs the JDK's `defineOrCheckPackage`
    /// REFUSED (a sealing violation) at a link or an unlinked door: HotSpot
    /// never defined a class of such a pair, so the `findLoadedClass` natives
    /// do not hand one out (`vm_util::class_path_package_refused`). Empty in
    /// every run without a sealed-package violation. Interpreter round i4
    /// wave 95, lane L5.
    pub class_path_packages_refused: parking_lot::Mutex<rustc_hash::FxHashSet<(String, String)>>,

    /// Do the by-name `ClassLoader` null-resource-name intercepts run
    /// (`invoke_or_native`'s null-name arm, the two `native_override`
    /// interceptors, the vtable door's `java/lang/ClassLoader` arm and its
    /// invoke-cache eviction in `dispatch_virtual`, and the JIT's three
    /// doors)? They send `getResource*(null)` (and in places `loadClass` /
    /// `resources`) on an inherited method to `ClassLoader`'s native. `false`
    /// over a real JDK with `CRATONVM_RETIRE_CLASSLOADER_NULL_NAME_INTERCEPTS`:
    /// the JDK bodies open with `Objects.requireNonNull(name)` and the natives
    /// raise the same message-less NPE, so ordinary dispatch runs whichever
    /// body the receiver selects. Always `true` over the synthetic class
    /// library. Read once at VM creation; interpreter round i2 wave 55, lane
    /// L5 (`docs/internal/fixed-bugs/interpreter-L5-proposal-retire-the-classloader-null-name-intercepts-FIXED-20261022.md`).
    pub classloader_null_name_intercepts: bool,

    /// Loader-namespaced lookups answered from a memo row with no initiating
    /// record behind it (the rows [`Self::record_replaces_memo`] stops
    /// serving), counted under `CRATONVM_DBG=access` only and printed in the
    /// exit `LOADER census` line as `initiating-memo-only=`.
    pub initiating_memo_only_answers: std::sync::atomic::AtomicU64,

    /// The JDK-global half of the census beside
    /// [`Self::initiating_memo_only_answers`], which counts recordable names
    /// only and so never saw a `javax/`, `jdk/`, `sun/` or `com/sun/` row
    /// (interpreter round i6 wave 125, lane L5;
    /// `i38-L5-proposal-the-initiating-record-replaces-the-memo-under-jdk-only`).
    /// Counted under `CRATONVM_DBG=access` only, printed in the exit `LOADER
    /// census` line:
    /// * `[0]` `initiating-memo-only-global=`: a loader-namespaced lookup of
    ///   such a name answered from a memo row with no initiating record behind
    ///   it;
    /// * `[1]` / `[2]` `global-name-refusal-memo=<base-miss>/<degrade>`:
    ///   `drive_loader_for_global_name`'s refusals by the checked door's
    ///   reason, a `BaseMiss` (HotSpot records the built-in class for the
    ///   loader) or a degrade case (HotSpot records nothing).
    pub global_name_record_census: [std::sync::atomic::AtomicU64; 3],

    /// Per user-defined loader: whether its `loadClass` is known to answer a
    /// JDK-global name (`javax/`, `jdk/`, `sun/`, `com/sun/`) exactly as the
    /// global route does — neither `loadClass` overload overridden below the
    /// JDK's base, and every parent built-in or itself such a loader
    /// (`classloader_real::loader_answers_jdk_names_as_the_jdk`). `false`
    /// means the loader may answer its own class and is asked under
    /// `--jdk-only` (`constants.rs` `drive_loader_for_global_name`). Written
    /// once per loader, on the first such resolution; dropped with the
    /// loader's namespace by the unload sweep. Interpreter round i1 wave 28,
    /// lane L5.
    pub loader_global_name_transparency: RwLock<FxHashMap<cratonvm_types::ClassLoaderId, bool>>,

    /// Lambda proxy registry: maps synthetic proxy ClassId → LambdaCallSite metadata.
    /// Used by the interpreter to dispatch method calls on lambda proxy objects.
    /// T10.9.B: FxHashMap — ClassId-keyed.
    ///
    /// Held behind an `Arc` because `try_lambda_dispatch` must take an owned
    /// copy out from under the `RwLock` before it can run the lambda body (the
    /// body can itself register proxies, so the read guard cannot be held
    /// across it). With a bare `LambdaCallSite` that copy was a DEEP CLONE on
    /// every lambda call — six `Arc<str>` refcount pairs plus a fresh
    /// `Vec<char>` — and the clone/drop/`mi_free`/`memmove` around it measured
    /// ~27% of a lambda-only `perf` profile. As an `Arc` the same line is one
    /// refcount bump.
    pub lambda_proxies: RwLock<FxHashMap<ClassId, Arc<LambdaCallSite>>>,

    /// JIT round 20 wave 12, lane qLam (W15L5-7): the lock-free positive answer
    /// of [`Self::is_lambda_proxy_class`]. See [`LambdaProxyHitBits`].
    pub(crate) lambda_proxy_hit_bits: LambdaProxyHitBits,

    /// `CRATONVM_RAW_WRITE_SETTLES_FINALIZER_FLIPS` (JIT round 21 wave 25,
    /// lane rawwrite; item 3 of `r21-review-w24-findings-20261019.md`;
    /// default ON, `=0` restores the old wait), read once per VM: the release
    /// of a [`ClassRealm::class_manager_write_settling`] guard settles the
    /// stub-upgrade finalizer flips its section queued. Off, that guard is a
    /// raw write lock and the rows wait for the next guarded release.
    pub(crate) raw_write_settles_finalizer_flips: bool,

    /// JIT round 20, lane rev12: the leaked call-site record
    /// `lambda_native_instance_target` keys the JIT native site cache with,
    /// interned per `(receiver class, method, descriptor)` for this VM. The
    /// lambda site caches are per thread, so leaking a fresh record on every
    /// build leaked one per thread per site (and again on every stale-gate
    /// rebuild), without bound on a thread-churning workload; interned, the
    /// leak is bounded by the distinct native targets this VM ever bound.
    pub(crate) lambda_native_infos: parking_lot::Mutex<
        FxHashMap<(ClassId, Box<str>, Box<str>), &'static cratonvm_jit::JitInvokeInfo>,
    >,

    /// Cache of the synthesized `java.lang.reflect.Method` (with its
    /// `parameterTypes`/`exceptionTypes` arrays and name/signature strings
    /// already populated) that `proxy_invoke_handler_shared` builds for every
    /// `InvocationHandler.invoke(proxy, method, args)` dispatch. Keyed by
    /// (proxy's ClassId, method name, descriptor) — real JDK dynamic-proxy
    /// classes build this Method object ONCE per interface method in their
    /// static initializer, not per call; before this cache, CratonVM rebuilt
    /// it (a fresh object + two arrays + two strings) on every single
    /// reflective dispatch, which on allocation-heavy reflection-driven
    /// workloads (e.g. ByteBuddy's `JavaDispatcher.INVOKER`) generated enough
    /// short-lived garbage to fragment the non-compacting old-gen arena and
    /// OOM even though most of the heap was nominally free.
    /// Grow-only, bounded by the number of distinct (proxy class, method)
    /// pairs an application actually exercises — not user-input-sized.
    /// Rooted unconditionally in `memory::roots` §6b alongside `class_mirrors`
    /// since these Method objects, like class mirrors, are meant to outlive
    /// any single call and be shared across every future dispatch to the
    /// same proxy method — **and remapped** in
    /// `memory::gc::update_all_roots` §6a via `remap_proxy_method_cache`.
    ///
    /// Both halves are load-bearing, and for a long time only the scan half
    /// shipped. Rooting alone makes a MOVING young collection *evacuate* the
    /// Method (it is reachable) and leave this map holding the from-space
    /// address, so the next dispatch on the same key hands running Java an
    /// object whose body has been reset: `Method.getName()` returns `null`,
    /// and any `InvocationHandler` that switches on the method name (javac
    /// lowers a String switch to `String.hashCode()`) throws
    /// `NullPointerException: Cannot invoke "String.hashCode()"`. That was
    /// 18 + 11 Spring Boot failures under `-XX:+UseGenerationalGC` with ZGC
    /// and G1 green on the same binary. Witness:
    /// `probes/ProxyMethodNameProbe.java`.
    pub proxy_method_cache: RwLock<FxHashMap<(ClassId, String, String), ObjectRef>>,

    /// Resolved implementation-owner `ClassId` for each lambda proxy id, on the
    /// **globally-resolved** path only.
    ///
    /// `try_lambda_dispatch` used to re-resolve its target *by name* on every
    /// single SAM call — `invoke_shared`'s `load_class_concurrent` for the
    /// `InvokeStatic` arm, and a `class_manager.write()` + `load_class` for the
    /// `InvokeSpecial` / `NewInvokeSpecial` arms. `CRATONVM_DBG=lambda-prof`
    /// priced it: of ~3,000-3,900 ns per dispatch of an **empty** lambda,
    /// **2,300-3,080 ns was the target invoke** — and with a no-op body, that is
    /// all resolution — against ~130 ns for the proxy-table lookup and ~230-300
    /// ns for argument coercion. It is what made a lambda's interface call cost
    /// ~4,200 ns against ~18 ns for the identical call on a named class, and why
    /// the JIT was worth only 17% on Spring context startup: the cost sits in
    /// Rust dispatch machinery that compiled Java bodies never enter.
    ///
    /// Only the *global* answer is memoised. The loader-faithful override
    /// (`lambda_impl_dispatch_override_driven`) is still consulted first on
    /// every call and is not cached here, so a user-loader-local copy keeps
    /// winning exactly as before; this table is only ever reached once that
    /// override has declined, which means the by-name answer is the stable
    /// global copy. In-place `redefine_class` keeps the `ClassId`, so a
    /// redefinition needs no eviction. Class unloading does: a row is dropped
    /// when its implementation owner, or the host class that spun its proxy,
    /// unloads (`invokedynamic::forget_unloaded_lambda_proxy_rows`, from
    /// `memory::gc::unload_dead_class_metadata`). Class ids and proxy ids are
    /// never reused, so a row is never read for a different class.
    pub lambda_impl_owner_memo: RwLock<FxHashMap<ClassId, ClassId>>,

    /// Defining class (the class whose `invokedynamic` created this lambda /
    /// method-ref) for each lambda proxy id — what HotSpot names the proxy after
    /// and reports as its nest host. Kept as a side table so the reflection name
    /// natives report `<host>$$Lambda/0x<id>` and a correct `getNestHost()` even
    /// for a cross-class method reference, whose implementation method lives in a
    /// different class than the one that defines the reference. bug-06 fam5 #1.
    pub lambda_proxy_hosts: RwLock<FxHashMap<ClassId, ClassId>>,

    /// Counter for generating unique synthetic lambda proxy ClassIds.
    /// Seeded from [`LAMBDA_PROXY_ID_BASE`] to avoid collisions with real
    /// ClassIds from the ClassStore, and to keep the seed and the range test
    /// in [`ClassRealm::is_lambda_proxy_class`] spelled once.
    pub next_lambda_id: AtomicU32,

    /// Resolved `ClassId` of [`ANNOTATION_PROXY_CLASS`], or `u32::MAX` while it
    /// has not been observed. Written once, by
    /// [`ClassRealm::resolve_annotation_proxy_cid`].
    ///
    /// Per-realm rather than process-global on purpose: two VMs in one process
    /// mint the class independently and need not agree on its id, and this
    /// value gates a **correctness** decision (annotation-proxy dispatch must
    /// reach `execute_invoke`'s interception layer), not a fast-path hint.
    ///
    /// Safe to hold without an invalidation key: the class is a `java/`
    /// class, which only the boot loader defines, and only classes of
    /// user-defined loaders unload (`memory::gc::unload_dead_class_metadata`);
    /// in-place `redefine_class` keeps the `ClassId`.
    pub annotation_proxy_cid: AtomicU32,

    /// This VM's class-store definition epoch (`StoreEpochs`) at which
    /// [`ANNOTATION_PROXY_CLASS`] was last confirmed **not** to be defined, or
    /// `u64::MAX` if it has never been looked up.
    ///
    /// The negative half needs a key where the positive half does not: "no
    /// class carries this name" is only true until the next class definition,
    /// and the epoch moves on every one of them (`loaded_classes_insert` is its
    /// sole writer). Holding the answer for one epoch turns an unbounded
    /// per-invoke lookup into one lookup per class defined. The store's own
    /// epoch, not the process-wide one: only a definition in this VM can
    /// define the class here (interpreter round i1 wave 21, lane L5).
    pub annotation_proxy_absent_epoch: std::sync::atomic::AtomicU64,

    /// This VM's class store's layout domain (`ClassStore::layout_domain`,
    /// the same token as `JitRealm::class_layout_domain`), readable without
    /// the class-manager lock: it names the store's `StoreEpochs` slot for
    /// [`Self::annotation_proxy_absent_epoch`]. Fixed at construction.
    pub(crate) class_layout_domain: u32,

    /// The `StoreEpochs::annotation_proxy_defined` latch of
    /// [`Self::is_annotation_proxy_class`] is on: `CRATONVM_LOADER_NO_ANN_PROXY_LATCH`
    /// clear ([`ann_proxy_latch_from_env`]). Fixed at construction.
    pub(crate) ann_proxy_latch: bool,

    /// Primitive type Class mirrors: "int" → ObjectRef, "boolean" → ObjectRef, etc.
    /// T10.9.B: FxHashMap — keys are fixed primitive type names, not user input.
    pub primitive_mirrors: RwLock<FxHashMap<String, ObjectRef>>,

    /// Canonical `java.lang.Module` mirrors keyed by module name (e.g.
    /// "java.base"), with `""` reserved for the unnamed module. `Class.getModule()`
    /// MUST hand back the SAME `Module` instance for every class in a module,
    /// because the JDK compares modules by identity (`Module` does not override
    /// `equals`). Real bytecode relies on this — e.g.
    /// `Throwable.validateSuppressedExceptionsList` deserializes a Throwable's
    /// suppressed-exceptions list and throws `StreamCorruptedException("List
    /// implementation not in base module.")` unless
    /// `Object.class.getModule() == deserializedList.getClass().getModule()`.
    /// Allocating a fresh Module per `getModule()` call (the old behaviour) made
    /// that comparison always false and broke ObjectInputStream round-trips of any
    /// object holding a `java.util` List (HIB-CV-29).
    pub module_mirrors: RwLock<FxHashMap<String, ObjectRef>>,

    /// `--jdk-only`: the `java.lang.Module` of a non-boot `ModuleLayer` that
    /// holds a package for a user-defined loader namespace, keyed by
    /// `(loader namespace id, package in internal form)`; the value is a JNI
    /// global-root handle of the Module (permanent, as a loader keeps its
    /// modules). Written by `Module.defineModule0`
    /// (`NativeContext::record_layer_module_packages`), read by
    /// `Class.getModule()` for a class of that loader and package, which
    /// otherwise answers the loader's unnamed module (interpreter round i1
    /// wave 43, lane L5). The flag spares every other `getModule()` the lock.
    pub layer_module_packages: RwLock<FxHashMap<(u32, Box<str>), usize>>,
    pub layer_module_packages_any: std::sync::atomic::AtomicBool,
    /// zs s5/nd5: on ZGC a USER loader's Module is not a permanent root
    /// (`CRATONVM_ZGC_LAYER_MODULE_LOADER_OWNED`): its `layer_module_packages`
    /// handle is a JNI WEAK global, and this list holds `(loader weak handle,
    /// module weak handle)` per such Module, which the `layer-modules` root
    /// row pins to its loader (`metadata_pin`): the Module lives exactly as
    /// long as the loader, as HotSpot's `ModuleEntry` lives in the loader's
    /// `ClassLoaderData`. The strong root (Module -> `loader` field) kept
    /// every such loader, its classes and their mirrors alive for the life of
    /// the VM: one per `Proxy.getProxyClass(new ClassLoader(){}, ..)` (its
    /// `jdk.proxyN` dynamic module), ~6.5 KB a call.
    pub layer_module_pins: parking_lot::Mutex<Vec<(usize, usize)>>,
    /// zs s5/nd5: the `layer_module_pins` length at which the next record
    /// sweeps out the rows whose Module is gone (amortised).
    pub layer_module_prune_at: AtomicUsize,

    /// Cached field count for java/lang/String (0 = not yet resolved).
    /// Once resolved, this is the actual `num_total_fields` from the loaded
    /// String class, avoiding lock contention in `create_java_string()`.
    pub cached_string_num_fields: AtomicUsize,

    /// True when the loaded java/lang/String uses JDK 9+ compact strings
    /// (byte[] value + byte coder) instead of pre-JDK-9 char[] layout.
    /// Set once during bootstrap; read by create_java_string / read_java_string.
    pub compact_strings: std::sync::atomic::AtomicBool,

    /// Cached field count for java/lang/Class mirror objects (0 = not yet resolved).
    pub cached_class_mirror_num_fields: AtomicUsize,

    /// `CRATONVM_CLASS_MIRROR_LAYOUT_MEMO` (`VmConfig::class_mirror_layout_memo`,
    /// interpreter round i3 wave 80, lane L5): the mirror layout of this VM's
    /// REAL `java/lang/Class` (its id, the field count, the by-name resolved
    /// slots, the slot-0 overlay verdict, the two mirror debug flags), set once
    /// by the first mirror minted after that class is loaded from real bytes
    /// (never from a compatibility stub, whose layout an upgrade replaces).
    /// Read by `vm_object::class_mirror_impl` and the primitive-mirror factory
    /// in place of the class-manager write lock and the by-name slot walk.
    pub(crate) class_mirror_layout: std::sync::OnceLock<crate::vm::vm_object::ClassMirrorLayout>,

    /// Per-class initialization condition variables (JVM spec §5.5).
    ///
    /// When a thread starts initializing a class, it inserts an entry here.
    /// Other threads that try to initialize the same class will wait on the
    /// condvar until the initializing thread completes (success or error).
    /// Entries are removed once initialization finishes.
    /// T10.9.B: FxHashMap — ClassId-keyed.
    /// Round-9 HIGH-4: inner `std::sync::Mutex<bool>` + `std::sync::Condvar`
    /// migrated to `parking_lot` equivalents — removes poison handling and
    /// yields a smaller, faster condvar with the same wait/notify API.
    pub class_init_waiters: parking_lot::Mutex<
        FxHashMap<ClassId, Arc<(parking_lot::Mutex<bool>, parking_lot::Condvar)>>,
    >,

    /// Why each erroneous class's initialization failed, as the message of
    /// the `ExceptionInInitializerError` HotSpot 21+ records
    /// (`InstanceKlass::add_initialization_error`, JDK-8048190):
    /// `Exception <class>[: <message>] [in thread "<name>"]`. Written by
    /// `exceptions::record_class_init_error` when a class is parked in
    /// `InitializationError`; read by `raise_could_not_initialize_class_for`,
    /// which attaches a fresh error with this message, and the original
    /// exception's stack trace, as the cause of every later "Could not
    /// initialize class" `NoClassDefFoundError`. Text and built trace entries
    /// only, so it holds no heap reference; `ClassId`s are never reused, so a
    /// row of an unloaded class is inert.
    pub class_init_errors:
        parking_lot::Mutex<FxHashMap<ClassId, crate::runtime::exceptions::RecordedClassInitError>>,

    /// Per-class-name loading locks (Session 30: Thread-Safe Class Loading).
    ///
    /// When a thread starts loading a class, it acquires the per-name lock.
    /// Other threads that try to load the *same* class will block on this lock
    /// instead of blocking the global ClassManager write lock, which allows
    /// concurrent loading of *different* classes to proceed without contention.
    /// The bool inside the Mutex indicates whether loading is in-progress.
    /// T10.9.B: FxHashMap — keys are internal class names.
    pub class_loading_locks:
        parking_lot::Mutex<FxHashMap<String, Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>>>,

    /// T10.9.E — Per-(class, slot-index) cache of the field's declared
    /// descriptor byte (the first byte of the JVM type descriptor:
    /// `b'J'` for `long`, `b'D'` for `double`, `b'L'` or `b'['` for
    /// reference types, etc.).
    ///
    /// Populated lazily on the first native-side field access that
    /// resolves the descriptor via the class manager, then consulted by
    /// `NativeContextImpl::get_field`/`set_field` on every subsequent
    /// access to the same slot so the hot path stays O(1) after the
    /// first resolution. Missing entries (never-seen class or index)
    /// fall back to the legacy descriptor-unaware `get_field`/`set_field`
    /// paths so the cache is strictly a correctness normalizer +
    /// performance acceleration, never a correctness hazard.
    ///
    /// Sizing: unbounded by design — total entries are bounded by
    /// `(loaded_classes * avg_fields)`, typically ~O(10k) for large
    /// applications like WildFly/Keycloak. The hottest access pattern
    /// (Unsafe-style concurrent data structures re-reading a handful
    /// of offsets) produces at most a few dozen entries per class.
    /// No eviction: entries are stable for the lifetime of the VM
    /// because class bytecode is immutable after loading.
    pub field_descriptor_cache:
        parking_lot::RwLock<crate::runtime::fx_collections::FxHashMap<(ClassId, usize), u8>>,

    /// WP0.2 — per-class cache of `ObjectStreamClass` descriptor
    /// mirrors. Populated by the first call to
    /// `ObjectStreamClass.lookup(cls)` for any given class; every
    /// subsequent lookup returns the same `ObjectRef`, matching the
    /// real-JDK singleton-per-class semantics (`ObjectStreamClass.
    /// Caches.localDescs`).
    ///
    /// See `crate::runtime::serialization::oscache` for the cache
    /// implementation and rationale.
    pub osc_cache: crate::runtime::serialization::OscCache,

    /// JIT round 16 (lane mh, MH3-2) — this VM's descriptor-keyed table of
    /// `MethodType`s for `MethodHandle.bindTo`'s bound types
    /// (`NativeContext::bound_method_type_get` / `_put`). Rooted and remapped
    /// by the `bound-method-types` row of `memory::native_roots`; holds
    /// java.*-and-primitive types (`lang_invoke::bound_method_type_shareable`)
    /// and, under `CRATONVM_MH_BOUND_TYPE_TABLE_APP_CLASSES`, class-keyed rows
    /// naming built-in loaders' classes (round 16 wave 15, lane mhapp), which
    /// the class-unload transaction drops (`MethodTypeTable::forget_classes`).
    pub bound_method_types: cratonvm_native_api::method_type_table::MethodTypeTable,
}

impl ClassRealm {
    /// Does this VM behave as HotSpot for `a` (see
    /// [`Self::hotspot_aligned`])? One bit test.
    #[inline]
    pub fn aligned(&self, a: cratonvm_types::HotSpotAlignment) -> bool {
        self.hotspot_aligned.contains(a)
    }

    /// Row `a` answered other than its row-off route would have (see
    /// [`Self::alignment_census`]). Cold: callers reach it only on the arm
    /// where the two answers differ.
    #[cold]
    #[inline(never)]
    pub fn note_alignment_divergence(&self, a: cratonvm_types::HotSpotAlignment) {
        self.alignment_census.note(a);
    }

    /// Is `class_id` one of the synthetic proxy classes spun for a lambda or
    /// method reference?
    ///
    /// Lambda proxies have no bytecode implementation of their
    /// functional-interface method, so every dispatch path that would install
    /// or use a direct target has to recognise them and cede to the slow SAM
    /// route. That check ran as `lambda_proxies.read().contains_key(&cid)` on
    /// **every** non-`invokespecial` virtual invoke in the interpreter -- an
    /// `RwLock` acquisition plus a hash probe to answer "no" for every ordinary
    /// class in the program.
    ///
    /// The range test in front of it is exact rather than heuristic: proxy ids
    /// come only from `alloc_lambda_proxy_id`, whose counter is seeded at
    /// [`LAMBDA_PROXY_ID_BASE`], so an id below the base cannot be in the map.
    /// A hit still pays the full lookup, which is what keeps the answer
    /// identical -- the map, not the range, remains the authority on membership
    /// (under `CRATONVM_LAMBDA_PROXY_UNCAPPED=0`, `MAX_LAMBDA_PROXIES` means an
    /// allocated id can be refused a table slot).
    ///
    /// JIT round 20 wave 12, lane qLam (W15L5-7): a hit already recorded in
    /// [`Self::lambda_proxy_hit_bits`] is answered without the lock; a first
    /// hit records itself while the read guard is still held (see
    /// [`LambdaProxyHitBits`] for why that keeps the bit exact).
    #[inline]
    pub fn is_lambda_proxy_class(&self, class_id: ClassId) -> bool {
        if class_id.as_u32() < LAMBDA_PROXY_ID_BASE {
            return false;
        }
        if self.lambda_proxy_hit_bits.contains(class_id) {
            return true;
        }
        let proxies = self.lambda_proxies.read();
        let hit = proxies.contains_key(&class_id);
        if hit {
            self.lambda_proxy_hit_bits.note(class_id);
        }
        drop(proxies);
        hit
    }

    /// Clear the lock-free hit bit of a lambda proxy whose `lambda_proxies`
    /// row is being removed (W15L5-7). Called from inside the unload paths'
    /// `retain`, i.e. under the table's write guard.
    pub(crate) fn forget_lambda_proxy_hit(&self, class_id: ClassId) {
        self.lambda_proxy_hit_bits.forget(class_id);
    }

    /// The lambda call site for `class_id`, or `None` for an ordinary class.
    ///
    /// The value-returning sibling of [`Self::is_lambda_proxy_class`], and it
    /// exists for the same reason: `NativeContextImpl::invoke_virtual` took
    /// `lambda_proxies.read()` and probed the map on EVERY native->Java
    /// callback, to answer "no" for every ordinary receiver. The id-range test
    /// in front is exact — see [`Self::is_lambda_proxy_class`] — so an ordinary
    /// receiver now costs one `u32` compare instead of an `RwLock` acquisition
    /// and a hash probe. On `HibfixComposeProbe2` that path is
    /// `CompletableFuture.complete`'s `postComplete()` callback, 2.5 times per
    /// composition chain.
    #[inline]
    pub fn lambda_call_site_for(&self, class_id: ClassId) -> Option<Arc<LambdaCallSite>> {
        if class_id.as_u32() < LAMBDA_PROXY_ID_BASE && crate::runtime::env_cache::hot_lookup_cache()
        {
            return None;
        }
        self.lambda_proxies.read().get(&class_id).cloned()
    }

    /// Is `class_id` the VM-internal [`ANNOTATION_PROXY_CLASS`]?
    ///
    /// Annotation proxies have no real bytecode for the `Annotation` contract
    /// (nor for `Object.equals`/`hashCode`/`toString`), so a cached virtual
    /// target must never serve them -- the dispatch has to fall through to
    /// `execute_invoke`'s interception layer. The gate is consumed immediately
    /// and decides correctness, so unlike an optional tier-up predicate it
    /// cannot be deferred; it can only be made cheaper.
    ///
    /// Steady state is one relaxed load and one `u32` compare. The name
    /// comparison it replaces took the class-manager read lock, resolved the
    /// class, and compared an `Arc<str>` against a literal, once per virtual
    /// invoke.
    ///
    /// # The `u32::MAX` state is the common one, and it is not the fast one
    ///
    /// "Steady state" above meant *once the class exists*. `AnnotationProxy` is
    /// a VM-internal synthetic class that most programs never mint, so the hint
    /// stays `u32::MAX` for the whole process and every virtual invoke reaches
    /// [`Self::resolve_annotation_proxy_cid`] — which is `#[cold]`, hence an
    /// out-of-line call, and which does two atomic loads before it can answer.
    /// That is what the advertised "one relaxed load and one `u32` compare"
    /// actually costs in the overwhelmingly common case.
    ///
    /// **Stated precisely, because the first version of this note overstated
    /// it.** The cold resolver *stamps* the epoch on its negative, so it is
    /// **not** true that every virtual invoke took the class-manager read lock
    /// while classes were loading: only the first invoke after each class
    /// definition did. The lock traffic is therefore O(classes defined), not
    /// O(invokes) — a real cost during a Spring or Tomcat start, but a much
    /// smaller one than "per invoke", and the difference matters to anyone
    /// budgeting against this line.
    ///
    /// `StoreEpochs::annotation_proxy_defined` removes both halves: it is this
    /// store's one-way latch, raised by `loaded_classes_insert` the first time
    /// the name is defined in it, so it is immune to the epoch. False ⇒ this
    /// VM has no such class ⇒ no `class_id` can be it, in one relaxed load with
    /// no call. It gates only the *lookup*, never the answer — the per-realm
    /// `annotation_proxy_cid` still decides identity, because two VMs in one
    /// process mint the class independently and must not share an id. (A
    /// process latch until interpreter round i1 wave 25, lane L5, so one VM
    /// minting the class sent every other VM to the cold resolver.)
    ///
    /// Not separately measured: the effect is a few nanoseconds on a path whose
    /// run-to-run spread on the development host is tens. It is kept because it
    /// strictly removes work and adds one `bool`, not because a probe resolved
    /// it. `CRATONVM_LOADER_NO_ANN_PROXY_LATCH=1` is there for whoever gets a
    /// quiet host.
    #[inline]
    pub fn is_annotation_proxy_class(&self, class_id: ClassId) -> bool {
        let hint = self.annotation_proxy_cid.load(Ordering::Relaxed);
        if hint != u32::MAX {
            return class_id.as_u32() == hint;
        }
        if self.ann_proxy_latch
            && !cratonvm_classloading::store_epochs(self.class_layout_domain)
                .annotation_proxy_defined()
        {
            return false;
        }
        self.resolve_annotation_proxy_cid() == Some(class_id)
    }

    /// The cold half of [`Self::is_annotation_proxy_class`]: look the class up
    /// by name, at most once per class-definition epoch until it exists.
    ///
    /// Answering by NAME rather than per-receiver is what makes the negative
    /// cacheable at all. "Is receiver X the proxy" depends on X and cannot be
    /// held across receivers; "is the proxy class defined yet" does not depend
    /// on X, and is fixed for as long as the class-definition epoch is.
    #[cold]
    fn resolve_annotation_proxy_cid(&self) -> Option<ClassId> {
        let epoch =
            cratonvm_classloading::store_epochs(self.class_layout_domain).class_definition_epoch();
        if self.annotation_proxy_absent_epoch.load(Ordering::Relaxed) == epoch {
            return None;
        }
        let found = self
            .class_manager
            .read()
            .get_loaded_class_id(ANNOTATION_PROXY_CLASS);
        match found {
            Some(cid) => {
                self.annotation_proxy_cid
                    .store(cid.as_u32(), Ordering::Relaxed);
                Some(cid)
            }
            None => {
                self.annotation_proxy_absent_epoch
                    .store(epoch, Ordering::Relaxed);
                None
            }
        }
    }

    /// Acquire the L10 `class_manager` write lock through a guard that
    /// drains any JVMTI ClassLoad events queued during the critical section
    /// once the underlying lock is released.
    ///
    /// obsaudit D1 (2026-07-26): this is now the *only* sanctioned way to
    /// take this write lock — `cratonvm_classloading::fire_class_load_hook`
    /// no longer invokes an installed JVMTI hook
    /// synchronously; it queues the event on a thread-local and relies on
    /// this guard's `Drop` to fire it after the lock is gone. Acquiring the
    /// write lock any other way (e.g. reaching into `.class_manager_write()`
    /// directly) would silently strand queued events until the next time
    /// this method happens to run on the same OS thread — every former
    /// direct-`.write()` call site in the workspace was mechanically
    /// switched to this method for exactly that reason. See the DEFERRED
    /// FIRING notes in `classloading/src/class_manager.rs` near
    /// `install_class_load_hook`.
    pub fn class_manager_write(&self) -> ClassManagerWriteGuard<'_> {
        ClassManagerWriteGuard {
            guard: Some(self.class_manager.write()),
            before_hooks: None,
        }
    }

    /// The class-manager write lock for a section that may load, fabricate
    /// or define a class but must NOT run the full guarded release
    /// ([`Self::class_manager_write`]'s JVMTI `ClassLoad` drain and its two
    /// resweeps) where it stands: on release it settles only the
    /// stub-upgrade finalizer flips the section queued
    /// (`ClassManager::take_stub_upgrade_finalizer_flips`,
    /// `SharedVm::note_stub_upgrade_finalizer_flips`), with no class-manager
    /// lock held, exactly as the guarded release does.
    ///
    /// JIT round 21 wave 25, lane rawwrite (item 3 of
    /// `docs/known-issues/jit/r21-review-w24-findings-20261019.md`,
    /// `CRATONVM_RAW_WRITE_SETTLES_FINALIZER_FLIPS`, default ON): the
    /// production `class_manager.write()` sites that reach `load_class`,
    /// `try_ensure_synthetic_class`, `ensure_generated_class` or
    /// `load_array_class_for_loader` (each can upgrade a stub in place) take
    /// this instead of the raw lock, so a flip no longer waits for some later
    /// guarded release on any thread. Under `=0` this is the raw lock and the
    /// release does nothing.
    pub fn class_manager_write_settling(&self) -> ClassManagerFlipSettlingGuard<'_> {
        ClassManagerFlipSettlingGuard {
            guard: Some(self.class_manager.write()),
            settles: self.raw_write_settles_finalizer_flips,
        }
    }

    /// Class-hierarchy analysis: the single loaded, CONCRETE class at or below
    /// `static_type` that provides `name`/`descriptor`, or `None` when there
    /// are zero of them or more than one.
    ///
    /// `NOTES-deopt2.md` M5 / `NOTES-deopt.md` N8 finding 14. This is the
    /// analysis half of CHA; the invalidation half
    /// (`CompiledMethod::inlined_methods`, `JitCache::invalidate_for_class_change`
    /// and the supertype-closure listener in `vm_init.rs`) has existed for a
    /// long time with nothing feeding it a hierarchy assumption.
    ///
    /// # The answer is the RECEIVER class, not the declaring class
    ///
    /// The consumer bakes this id into an exact `CMP DWORD [recv+0], id`, so
    /// the only useful answer is one a receiver's class word can equal. A
    /// method declared on an abstract base and inherited unchanged by one
    /// concrete subclass answers with the SUBCLASS: that is the class objects
    /// at the site actually have. Answering with the declarer would produce a
    /// guard that never hits.
    ///
    /// # Computed over LOADED classes only, and therefore only a hint
    ///
    /// Every word of this is a statement about the hierarchy as it stands right
    /// now. The next class define can add a second implementor, and this VM has
    /// no way to stop a thread that is already inside a compiled body. That is
    /// why the consumer binds behind a receiver guard rather than directly: a
    /// receiver of a class this analysis never saw MISSES the compare and takes
    /// unchanged dispatch, so a stale or incomplete answer costs a useless
    /// compare and can never run the wrong method. See
    /// `cratonvm_jit::InlineDependency::UniqueConcreteMethod` for the full
    /// argument, including what would have to change before an unguarded bind
    /// could be sound.
    ///
    /// # Interfaces are in reach
    ///
    /// The walk is `ClassStore::subtypes_of`, which unions the store's two
    /// adjacency indices: the direct-SUBCLASS one built from
    /// `Class::superclass`, and the direct-IMPLEMENTOR one built from
    /// `Class::interfaces`. Both are maintained by the store's own mutators
    /// (`add` / `set_superclass` / `set_interfaces` / `remove`) and by nothing
    /// else, which is why they are exactly as complete as the define paths
    /// themselves rather than as complete as a hand-maintained side table
    /// happens to be.
    ///
    /// An `invokeinterface` site is therefore answered on the same terms as a
    /// virtual one, and that is the larger half of the opportunity: streams,
    /// collections, Netty handler pipelines and Spring proxies are interface
    /// calls. The interface root itself is never the answer — it fails the
    /// instantiable test in the loop below, exactly as an abstract class does.
    ///
    /// # Bounded, and a truncated walk says so
    ///
    /// A site declared on a hierarchy root — `java/lang/Object.toString`, or a
    /// JDK interface such as `java/util/List` — has every loaded subtype below
    /// it and the answer is always "many". [`CHA_MAX_SUBTYPES`] is handed to
    /// `subtypes_of` as its cap, applied to the COMBINED walk, so the analysis
    /// cannot become O(loaded classes) per call site per compile.
    ///
    /// `subtypes_of` reports hitting that cap as `None` rather than as a
    /// truncated list, and that distinction is the whole point: a truncated
    /// list read as complete is how "exactly one implementor" gets reported for
    /// an interface that has hundreds. The `?` below turns it into a refusal —
    /// "cannot answer" — instead.
    pub fn unique_implementor(
        &self,
        static_type: ClassId,
        name: &str,
        descriptor: &str,
    ) -> Option<ClassId> {
        use cratonvm_reader::class_access_flags::MethodAccessFlags;
        let cm = self.class_manager.read();
        let store = cm.class_store();
        let root = store.get(static_type)?;
        // An array class has no declared methods of its own and no subtype
        // index entry; an interface root, by contrast, is now a perfectly good
        // question to ask.
        if root.name.starts_with('[') {
            return None;
        }
        let subtypes = store.subtypes_of(static_type, CHA_MAX_SUBTYPES)?;
        let mut found: Option<ClassId> = None;
        for cid in std::iter::once(static_type).chain(subtypes) {
            let Some(class) = store.get(cid) else {
                continue;
            };
            // Only a class that can be INSTANTIATED can be a receiver's exact
            // class, so an abstract class or an interface is not a provider
            // however many bodies it declares. An array class has no declared
            // methods of its own.
            if class.is_interface() || class.is_abstract() || class.name.starts_with('[') {
                continue;
            }
            let Some((method, _declaring)) =
                crate::classloading::find_method_recursive(cid, name, descriptor, store)
            else {
                continue;
            };
            // An abstract declaration is not a provider — the design note says
            // so explicitly, and a concrete class that selects one is a
            // malformed class file, not an implementor.
            //
            // Nothing else is filtered. A `native`, `static` or `private`
            // selection IS counted as a provider, so a hierarchy with one
            // ordinary body and one such oddity reports "many" and binds
            // nothing. Filtering them would let this answer `One` for a
            // hierarchy that has two, which is the only direction in which
            // being wrong here costs anything at all.
            if method.access_flags.contains(MethodAccessFlags::ABSTRACT) {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some(cid);
        }
        found
    }
}

/// Ceiling on the subtype walk [`ClassRealm::unique_implementor`] performs.
///
/// A call site declared on a hierarchy root sees every loaded subtype below it,
/// and in a Spring- or Tomcat-scale run that is tens of thousands — per site,
/// per compile. The answer for such a root is "many" with overwhelming
/// probability, so the cap costs essentially no devirtualization while bounding
/// the analysis at a constant.
///
/// Passed to `ClassStore::subtypes_of`, which walks the SUBCLASS and
/// IMPLEMENTOR indices together and applies this ceiling to their union. That
/// matters now that interface roots are answered: `java/util/List` has one
/// direct-subclass edge and hundreds of implementor edges, so a cap applied to
/// either index alone would bound nothing.
///
/// 64 is comfortably above the breadth of an ordinary application hierarchy (a
/// service interface's implementations, a visitor's cases) and far below a JDK
/// root's.
pub const CHA_MAX_SUBTYPES: usize = 64;

/// RAII write guard for [`ClassRealm::class_manager`] that fires queued
/// JVMTI class-lifecycle events after releasing the lock. See
/// [`ClassRealm::class_manager_write`].
pub struct ClassManagerWriteGuard<'a> {
    // `Option` so `Drop` can explicitly drop the inner guard (releasing the
    // lock) before draining hooks, rather than relying on field-drop order
    // (which Rust does guarantee top-to-bottom for a single-field struct,
    // but the `Option` makes the ordering an explicit, checkable step
    // instead of an implicit language rule future edits could disturb).
    guard: Option<OrderedPlRwLockWriteGuard<'a, ClassManager>>,
    /// gci i47w lane oz47: work run on release, AFTER the write lock is gone
    /// and BEFORE the queued class-lifecycle hooks fire
    /// ([`Self::run_before_hooks`]). `None` on every guard that set nothing.
    before_hooks: Option<Box<dyn FnOnce() + 'a>>,
}

impl<'a> ClassManagerWriteGuard<'a> {
    /// gci i47w lane oz47 (`vi12-ev.list` T): run `work` when this guard is
    /// dropped, between the release of the class-manager write lock and the
    /// drain of the JVMTI `ClassLoad` hooks. `SharedVm::load_class_concurrent_for`
    /// clears its per-name loading flag here: before the release it would
    /// take the per-name mutex while holding the write lock, the reverse of
    /// the order a second loader takes them in (the per-name mutex, then
    /// `class_manager.read()` for its double-check) -- a lock-order deadlock
    /// (the i48h-dbg native stacks); after the drain, a second loader waits
    /// on the flag, not GC-safe, while a C agent's callback in the drain waits
    /// out a pause. A later call replaces the earlier work.
    pub fn run_before_hooks(&mut self, work: impl FnOnce() + 'a) {
        self.before_hooks = Some(Box::new(work));
    }
}

impl std::ops::Deref for ClassManagerWriteGuard<'_> {
    type Target = ClassManager;
    fn deref(&self) -> &ClassManager {
        self.guard.as_ref().expect("guard taken before drop")
    }
}

impl std::ops::DerefMut for ClassManagerWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut ClassManager {
        self.guard.as_mut().expect("guard taken before drop")
    }
}

impl Drop for ClassManagerWriteGuard<'_> {
    fn drop(&mut self) {
        // Which VM this class manager belongs to, for the deferred-`new`
        // resweep below: read while the guard is still held (one field load).
        let owner_domain = self
            .guard
            .as_ref()
            .map(|guard| guard.class_store().layout_domain());
        // JIT round 21 wave 23, lane finvm (item 8 of
        // `r21-review-w18-findings-20261019.md`,
        // `CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES`): the classes whose
        // `has_finalizer` a stub upgrade moved. Taken while the guard is held
        // (one length test on every release that upgraded nothing), and only
        // when the VM that owns this store is registered: class ids are per
        // store, so no other VM may settle them, and a store whose VM is not
        // registered yet keeps them for the first release after it is.
        // Settled after the release below.
        let finalizer_flips = self
            .guard
            .as_mut()
            .and_then(|guard| take_owned_stub_upgrade_finalizer_flips(guard));
        // Release the class-manager write lock first...
        self.guard = None;
        // gci i47w lane oz47: the caller's between-release-and-hooks work
        // (`run_before_hooks`), with no class-manager lock held.
        if let Some(work) = self.before_hooks.take() {
            work();
        }
        // The VM half of a stub upgrade's finalizer change, with no
        // class-manager lock held (the listener takes its own read and write):
        // each moved class's published allocation recipe is dropped, and each
        // class that became finalizable runs the define-time finalizer
        // listener (`SharedVm::note_stub_upgrade_finalizer_flips`).
        if let Some((owner, flips)) = finalizer_flips {
            owner.note_stub_upgrade_finalizer_flips(&flips);
        }
        // ...then fire whatever ClassLoad/ClassPrepare events this critical
        // section queued. A hook invoked from here may freely take a fresh
        // `class_manager_write()`/`.read()` itself: the lock this guard held
        // is already gone by this point.
        // `CRATONVM_DBG_DEFINE_CENSUS` (interpreter round i4 wave 95, lane L1):
        // the `class-load-hooks` row times the drain at every release.
        let hooks_clock = cratonvm_classloading::define_census::phase_start();
        cratonvm_classloading::drain_pending_class_hooks();
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::ClassLoadHooks,
            hooks_clock,
        );
        // `CRATONVM_DBG_DEFINE_CENSUS` (interpreter round i4 wave 91, lane L5):
        // the `write-guard-sweep` row times the two sweeps below at every
        // release (one flag read, no clock, with the census off).
        let sweep_clock = cratonvm_classloading::define_census::phase_start();
        // A class definition is the only event that can turn a deferred `new`
        // site resolvable, and a method holding a retry for one already has a
        // body — so nothing recompiles it and no other sweep ever looks at it
        // again. Costs one relaxed load when nothing is held. Only this VM's
        // memo is swept (interpreter round i1 wave 18).
        crate::runtime::interpreter::jit_bridge::resweep_deferred_new_after_class_definition(
            owner_domain,
        );
        // A class definition is also what a parked profile-replay record
        // (`CRATONVM_JIT_PROFILE_LOAD`) waits for: the load in `SharedVm::new`
        // runs before any application class exists. Costs one relaxed load
        // when no replay is waiting. Only this VM is swept (interpreter round
        // i1 wave 21).
        crate::runtime::interpreter::jit_bridge::seed_profile_replay_after_class_definition(
            owner_domain,
        );
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::WriteGuardSweep,
            sweep_clock,
        );
    }
}

/// The stub-upgrade finalizer flips `cm` queued, with the VM that owns its
/// store, taken while the write lock is held: `None`, after one length test,
/// when nothing is queued, and `None` with the rows LEFT queued when no VM
/// owns the store yet (class ids are per store, so no other VM may settle
/// them; the first release after `Vm::new` registers does). The caller
/// settles them after the release (`SharedVm::note_stub_upgrade_finalizer_flips`).
/// Shared by [`ClassManagerWriteGuard`] (JIT round 21 wave 23, lane finvm)
/// and [`ClassManagerFlipSettlingGuard`] (wave 25, lane rawwrite).
fn take_owned_stub_upgrade_finalizer_flips(
    cm: &mut ClassManager,
) -> Option<(std::sync::Arc<crate::vm::SharedVm>, Vec<(ClassId, bool)>)> {
    if !cm.has_stub_upgrade_finalizer_flips() {
        return None;
    }
    let owner = crate::vm::vm_init::hook_vm_for_domain(cm.class_store().layout_domain())?;
    Some((owner, cm.take_stub_upgrade_finalizer_flips()))
}

/// RAII write guard for [`ClassRealm::class_manager`] whose release settles
/// the section's stub-upgrade finalizer flips and does nothing else. See
/// [`ClassRealm::class_manager_write_settling`].
///
/// Why not the full [`ClassManagerWriteGuard`] at these sites: its release
/// also drains this thread's queued JVMTI class hooks (an agent callback
/// that may run Java code and wait out a pause) and runs the deferred-`new`
/// and profile-replay resweeps. Those sites were written against the raw
/// lock, several sit inside exception construction, array resolution or
/// boxing, and moving the hook drain there would change when a `ClassLoad`
/// is posted. The flip settle is the one piece of that release the review
/// asked for, and it takes only what the guarded release already takes
/// after its own release (the allocation-recipe cache, a class-manager read,
/// `jit_gate_pass`, the JIT cache, then the raw class-manager read and write
/// of `note_finalizable_define`), so its lock order is the guarded one.
pub struct ClassManagerFlipSettlingGuard<'a> {
    guard: Option<OrderedPlRwLockWriteGuard<'a, ClassManager>>,
    /// [`ClassRealm::raw_write_settles_finalizer_flips`] at acquisition.
    settles: bool,
}

impl std::ops::Deref for ClassManagerFlipSettlingGuard<'_> {
    type Target = ClassManager;
    fn deref(&self) -> &ClassManager {
        self.guard.as_ref().expect("guard taken before drop")
    }
}

impl std::ops::DerefMut for ClassManagerFlipSettlingGuard<'_> {
    fn deref_mut(&mut self) -> &mut ClassManager {
        self.guard.as_mut().expect("guard taken before drop")
    }
}

impl Drop for ClassManagerFlipSettlingGuard<'_> {
    fn drop(&mut self) {
        // Taken while the lock is held, as the guarded release takes them.
        let finalizer_flips = if self.settles {
            self.guard
                .as_mut()
                .and_then(|guard| take_owned_stub_upgrade_finalizer_flips(guard))
        } else {
            None
        };
        // Release the class-manager write lock, then settle with none held.
        self.guard = None;
        if let Some((owner, flips)) = finalizer_flips {
            owner.note_stub_upgrade_finalizer_flips(&flips);
        }
    }
}

#[cfg(test)]
mod r21w25_raw_write_settling_tests {
    //! JIT round 21 wave 25, lane rawwrite (item 3 of
    //! `r21-review-w24-findings-20261019.md`): no production chained
    //! `classes.class_manager.write().<loader>(..)` is left in the files whose
    //! sites moved to [`super::ClassRealm::class_manager_write_settling`].

    /// `src` without its top-level test-only items (`#[cfg(test)]` or
    /// `#[cfg(all(test, ..))]`: a `mod x;`, or an item running to the next
    /// column-0 `}`), with every whitespace removed.
    fn production_squeezed(src: &str) -> String {
        let src = src.replace("\r\n", "\n");
        let mut out = String::new();
        let mut lines = src.lines().peekable();
        while let Some(line) = lines.next() {
            let test_only =
                line.starts_with("#[cfg(") && line.contains("(test") && !line.contains("not(test");
            if test_only {
                // Skip further attributes, then the item.
                while lines.peek().is_some_and(|l| l.starts_with("#[")) {
                    lines.next();
                }
                let Some(item) = lines.next() else { break };
                if item.trim_end().ends_with(';') {
                    continue;
                }
                for inner in lines.by_ref() {
                    if inner.starts_with('}') {
                        break;
                    }
                }
                continue;
            }
            out.extend(line.chars().filter(|c| !c.is_whitespace()));
        }
        out
    }

    #[test]
    fn no_production_raw_class_manager_write_reaches_a_loader() {
        let files: [(&str, &str); 9] = [
            ("native/jni.rs", include_str!("../../native/jni.rs")),
            ("runtime/exceptions.rs", include_str!("../../runtime/exceptions.rs")),
            ("runtime/interpreter.rs", include_str!("../../runtime/interpreter.rs")),
            (
                "runtime/interpreter/constants.rs",
                include_str!("../../runtime/interpreter/constants.rs"),
            ),
            ("runtime/interpreter/invoke.rs", include_str!("../../runtime/interpreter/invoke.rs")),
            ("runtime/interpreter/lambda.rs", include_str!("../../runtime/interpreter/lambda.rs")),
            (
                "runtime/interpreter/typecheck.rs",
                include_str!("../../runtime/interpreter/typecheck.rs"),
            ),
            ("vm/vm_exec.rs", include_str!("../vm_exec.rs")),
            ("debug/mod.rs", include_str!("../../debug/mod.rs")),
        ];
        for (name, src) in files {
            let prod = production_squeezed(src);
            assert!(prod.contains(".class_manager_write_settling()"), "{name}: a settling site");
            for loader in [
                "load_class(",
                "try_ensure_synthetic_class(",
                "ensure_generated_class(",
                "load_array_class_for_loader(",
            ] {
                let raw = format!("classes.class_manager.write().{loader}");
                assert!(!prod.contains(&raw), "{name}: a raw `{raw}` in production code");
            }
        }
    }
}

#[cfg(test)]
mod receiver_shape_gate_tests {
    use super::*;
    use crate::vm::SharedVm;
    use crate::VmConfig;
    use std::sync::Arc;

    /// The range test in [`ClassRealm::is_lambda_proxy_class`] must never
    /// answer "no" for an id that IS in the table, and must answer "no"
    /// without touching the table for every ordinary class id. Both halves are
    /// asserted: a range test that were merely conservative would be silently
    /// wrong here, because the caller uses `true` to cede to the slow SAM route.
    #[test]
    fn is_lambda_proxy_class_agrees_with_the_table() {
        use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};

        let shared = Arc::new(SharedVm::new(VmConfig::default()));

        // Every id the allocator hands out is inside the reserved range.
        let proxy = shared.alloc_lambda_proxy_id();
        assert!(
            proxy.as_u32() >= LAMBDA_PROXY_ID_BASE,
            "lambda proxy ids must come from the range the predicate tests"
        );

        // In range but not registered: the table stays the authority.
        assert!(!shared.classes.is_lambda_proxy_class(proxy));

        shared.classes.lambda_proxies.write().insert(
            proxy,
            Arc::new(LambdaCallSite {
                functional_interface_id: None,
                functional_interface: "java/lang/Runnable".into(),
                sam_method_name: "run".into(),
                sam_descriptor: "()V".into(),
                impl_handle: MethodHandle {
                    kind: MethodHandleKind::InvokeStatic,
                    class_name: "test/A".into(),
                    member_name: "lambda$0".into(),
                    descriptor: "()V".into(),
                },
                instantiated_descriptor: "()V".into(),
                capture_types: vec![],
                proxy_class_id: proxy,
                serializable_flag: false,
            }),
        );
        assert!(shared.classes.is_lambda_proxy_class(proxy));
        // A registered proxy is initialized (it has no `<clinit>`), so
        // `Method.invoke` on a method of the lambda's own class never asks
        // `prepare_class_shared` for an id the class manager does not hold.
        assert!(crate::vm::vm_util::is_class_initialized_via_manager(
            &shared, proxy
        ));
        let unregistered = shared.alloc_lambda_proxy_id();
        assert!(!crate::vm::vm_util::is_class_initialized_via_manager(
            &shared,
            unregistered
        ));

        // Ordinary class ids are below the base and answer "no".
        for raw in [0u32, 1, 42, 65_535, LAMBDA_PROXY_ID_BASE - 1] {
            assert!(
                !shared.classes.is_lambda_proxy_class(ClassId::new(raw)),
                "class id {raw} is not a lambda proxy"
            );
        }

        // JIT round 20 wave 12 (lane qLam, W15L5-7): the hit above was
        // recorded for the lock-free path (when the switch is on), and a row
        // removed the way the unload paths remove one answers "no" again.
        if shared.classes.lambda_proxy_hit_bits.enabled {
            assert!(shared.classes.lambda_proxy_hit_bits.contains(proxy));
        }
        assert!(shared.classes.is_lambda_proxy_class(proxy));
        shared.classes.lambda_proxies.write().retain(|id, _| {
            let keep = *id != proxy;
            if !keep {
                shared.classes.forget_lambda_proxy_hit(*id);
            }
            keep
        });
        assert!(!shared.classes.lambda_proxy_hit_bits.contains(proxy));
        assert!(!shared.classes.is_lambda_proxy_class(proxy));
    }

    /// W15L5-7: the bitmap's own contract -- set by a note, cleared by a
    /// forget, nothing outside the covered band, inert when switched off.
    #[test]
    fn lambda_proxy_hit_bits_note_and_forget() {
        let bits = LambdaProxyHitBits::new(true);
        let id = ClassId::new(LAMBDA_PROXY_ID_BASE + 70);
        let neighbour = ClassId::new(LAMBDA_PROXY_ID_BASE + 71);
        assert!(!bits.contains(id), "nothing is set before a note");
        bits.note(id);
        assert!(bits.contains(id));
        assert!(!bits.contains(neighbour), "one bit per id");
        bits.forget(id);
        assert!(!bits.contains(id));
        // Cast: the capacity is far below u32::MAX - LAMBDA_PROXY_ID_BASE.
        let past = ClassId::new(LAMBDA_PROXY_ID_BASE + LAMBDA_PROXY_HIT_BITS_CAPACITY as u32);
        bits.note(past);
        assert!(
            !bits.contains(past),
            "an id past the band keeps the locked lookup"
        );
        bits.note(ClassId::new(5));
        assert!(
            !bits.contains(ClassId::new(5)),
            "an id below the base is never a hit"
        );
        let off = LambdaProxyHitBits::new(false);
        off.note(id);
        assert!(!off.contains(id), "switched off, nothing is recorded");
        assert!(
            off.words.get().is_none(),
            "switched off, nothing is allocated"
        );
    }

    /// [`ClassRealm::is_annotation_proxy_class`] replaced a name comparison
    /// under the class-manager lock with a `ClassId` identity test. The failure
    /// mode that matters is the NEGATIVE one: the memo must not latch "absent"
    /// from before the class was minted, because a stale `false` sends an
    /// annotation proxy down a cached-target path that has no bytecode for it.
    #[test]
    fn is_annotation_proxy_class_learns_the_class_after_it_is_minted() {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));

        // Before the class exists every id answers "no" -- and asking
        // repeatedly is exactly what would latch a wrong answer if the negative
        // memo were not keyed on the class-definition epoch.
        for _ in 0..4 {
            for raw in [0u32, 1, 7, 1234] {
                assert!(!shared.classes.is_annotation_proxy_class(ClassId::new(raw)));
            }
        }

        // Mint it the way `ensure_vm_internal_class` does.
        let proxy_cid = shared.classes.class_manager_write().ensure_generated_class(
            ANNOTATION_PROXY_CLASS,
            4,
            cratonvm_classloading::ClassOrigin::VmInternal,
        );

        assert!(
            shared.classes.is_annotation_proxy_class(proxy_cid),
            "the proxy class must be recognised once defined, even though every \
             earlier query answered `no`"
        );
        // Repeat reads take the memoized identity path and stay stable.
        assert!(shared.classes.is_annotation_proxy_class(proxy_cid));
        // ...and nothing else is the proxy.
        for raw in [0u32, 1, 7, 1234] {
            let other = ClassId::new(raw);
            if other != proxy_cid {
                assert!(!shared.classes.is_annotation_proxy_class(other));
            }
        }
    }
}

#[cfg(test)]
mod statics_index_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    /// A lock-free static read must never observe halves of two different
    /// writes.
    ///
    /// [`StaticsIndex::get`] used to copy the whole 16-byte `Value` cell in one
    /// go, and a 16-byte access is not single-copy atomic on x86-64: a reader
    /// racing a `putstatic` of a `long` saw the high half of one value with the
    /// low half of another. `probes/StaticRaceProbe.java` reproduces it from
    /// Java in seconds — on the interpreter too, since `get_static_shared`
    /// routes through here — and for a `volatile` long that is a JLS §17.7
    /// violation, not merely the permitted non-atomic plain-long read.
    ///
    /// This is the same race in miniature: one writer flipping a slot between
    /// two patterns, four readers asserting they only ever see those two.
    ///
    /// **It does not, on its own, reproduce the historical tear.** Whether
    /// rustc emits one wide copy or two 8-byte loads depends on the inlining
    /// context, and in this test's context it already chose the narrow form:
    /// verified by reverting the fix, at which point the Java probe still tore
    /// within seconds and this test still passed. So read it as a cheap CI pin
    /// on the invariant, not as the reproduction — that is
    /// `probes/StaticRaceProbe.java`, and a future rewrite of `get` has to be
    /// re-checked against the probe, not against this.
    #[test]
    fn get_never_returns_a_torn_long() {
        const A: i64 = 0x0123_4567_89AB_CDEF;
        const B: i64 = 0x7EDC_BA98_7654_3210;

        let block = StaticsBlock::new(4);
        let index = Arc::new(StaticsIndex::new());
        index.publish(ClassId::new(3), &block);
        // The block is leaked by construction, so a raw base pointer outlives
        // both threads; `usize` to carry it across the spawn boundary.
        let base = block.base_ptr() as usize;

        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = Arc::clone(&stop);
        let writer = std::thread::spawn(move || {
            let slot = base as *mut Value;
            let mut flip = false;
            while !writer_stop.load(AtomicOrdering::Relaxed) {
                flip = !flip;
                // Exactly what `set_static_shared` does to the cell, minus the
                // lock the lock-free reader does not take either.
                // SAFETY: `slot` is the leaked block's first cell.
                unsafe { *slot = Value::Long(if flip { B } else { A }) };
            }
        });

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let index = Arc::clone(&index);
                std::thread::spawn(move || {
                    for _ in 0..200_000 {
                        match index.get(ClassId::new(3), 0) {
                            Some(Value::Long(v)) => assert!(
                                v == A || v == B,
                                "torn long static read: {v:#x} is neither {A:#x} nor {B:#x}"
                            ),
                            // The writer only ever stores `Value::Long`; the
                            // initial `Value::Int(0)` is legitimate until its
                            // first store lands.
                            Some(Value::Int(0)) => {}
                            other => panic!("unexpected static cell: {other:?}"),
                        }
                    }
                })
            })
            .collect();

        for r in readers {
            r.join().expect("reader thread");
        }
        stop.store(true, AtomicOrdering::Relaxed);
        writer.join().expect("writer thread");
    }

    /// The address handed to compiled code is the POINTER CELL, so a
    /// republication (a grown or re-prepared block) is followed with no
    /// patching — that is the whole reason the backend pays an extra load.
    #[test]
    fn base_cell_addr_is_stable_across_republication() {
        let index = StaticsIndex::new();
        let first = StaticsBlock::new(2);
        index.publish(ClassId::new(9), &first);
        let cell = index
            .base_cell_addr(ClassId::new(9), 1)
            .expect("published block must resolve");

        let second = StaticsBlock::new(8);
        assert_ne!(
            first.base_ptr(),
            second.base_ptr(),
            "the two blocks must be distinct allocations for this test to mean anything"
        );
        index.publish(ClassId::new(9), &second);

        assert_eq!(
            index.base_cell_addr(ClassId::new(9), 1),
            Some(cell),
            "the pointer cell address must not move when the block does"
        );
        // And what compiled code loads through it is the NEW block.
        // SAFETY: `cell` is the address of this index's `AtomicPtr` slot.
        let observed = unsafe {
            (*(cell as *const std::sync::atomic::AtomicPtr<Value>)).load(Ordering::Acquire)
        };
        assert_eq!(observed, second.base_ptr());
    }

    /// Republication in both directions: a grow and a shrink (the
    /// `prepare_class_shared`-after-early-store shape) each leave the index
    /// answering exactly the newest block's bounds, and `cell_ptr` names a
    /// cell of that block.
    #[test]
    fn republication_bounds_follow_the_newest_block_both_ways() {
        let index = StaticsIndex::new();
        let id = ClassId::new(21);
        let small = StaticsBlock::new(2);
        index.publish(id, &small);
        assert!(index.cell_ptr(id, 1).is_some());
        assert!(index.cell_ptr(id, 2).is_none());

        let big = StaticsBlock::new(8);
        index.publish(id, &big);
        assert_eq!(index.cell_ptr(id, 5), Some(big.base_ptr().wrapping_add(5)));
        assert!(index.get(id, 7).is_some());
        assert!(index.get(id, 8).is_none());

        let shrunk = StaticsBlock::new(3);
        index.publish(id, &shrunk);
        assert_eq!(
            index.cell_ptr(id, 2),
            Some(shrunk.base_ptr().wrapping_add(2))
        );
        assert!(
            index.cell_ptr(id, 5).is_none(),
            "shrink must not keep the old length"
        );
        assert!(index.get(id, 3).is_none());
    }

    /// Out of range in either direction means "keep the helper", never a baked
    /// address that would read past the block.
    #[test]
    fn base_cell_addr_refuses_unpublished_and_out_of_bounds() {
        let index = StaticsIndex::new();
        assert_eq!(index.base_cell_addr(ClassId::new(4), 0), None);
        let block = StaticsBlock::new(2);
        index.publish(ClassId::new(4), &block);
        assert!(index.base_cell_addr(ClassId::new(4), 1).is_some());
        assert_eq!(index.base_cell_addr(ClassId::new(4), 2), None);
        assert_eq!(
            index.base_cell_addr(ClassId::new(StaticsIndex::CAPACITY as u32), 0),
            None
        );
    }

    /// An unloaded class's entry stops answering every lock-free consumer, and
    /// the class can be published again afterwards.
    #[test]
    fn unpublish_withdraws_every_lock_free_answer() {
        let index = StaticsIndex::new();
        let id = ClassId::new(33);
        // Unpublishing in a VM that never published is a no-op.
        index.unpublish(id);
        let block = StaticsBlock::new(4);
        index.publish(id, &block);
        let cell = index.base_cell_addr(id, 0);
        assert!(cell.is_some());
        index.unpublish(id);
        assert!(index.get(id, 0).is_none());
        assert!(index.cell_ptr(id, 0).is_none());
        assert!(index.base_cell_addr(id, 0).is_none());
        // A baked cell still names a readable, non-null base.
        let baked = cell.map(|addr| {
            // SAFETY: the slot array is allocated once and never freed.
            let base_cell = unsafe { &*(addr as *const std::sync::atomic::AtomicPtr<Value>) };
            base_cell.load(AtomicOrdering::Acquire)
        });
        assert_eq!(baked, Some(block.base_ptr()));
        let again = StaticsBlock::new(2);
        index.publish(id, &again);
        assert_eq!(
            index.cell_ptr(id, 1),
            Some(again.base_ptr().wrapping_add(1))
        );
        // Out-of-capacity ids are ignored.
        index.unpublish(ClassId::new(StaticsIndex::CAPACITY as u32));
    }
}

/// Interpreter round i1 wave 21, lane L5: the annotation-proxy absence answer
/// is held against the VM's own class-store epoch, so another VM's class
/// definitions keep it and this VM's retire it.
#[cfg(test)]
mod w21_l5_annotation_proxy_absent_epoch_tests {
    use std::sync::atomic::Ordering;

    #[test]
    fn only_this_vms_definitions_retire_the_absent_answer() {
        let mine = crate::vm::SharedVm::new(crate::VmConfig::default());
        let other = crate::vm::SharedVm::new(crate::VmConfig::default());
        let slot = |vm: &crate::vm::SharedVm| {
            cratonvm_classloading::store_epoch_slot(vm.classes.class_layout_domain)
        };
        if slot(&mine) == slot(&other) {
            // The two stores share an epoch slot: nothing to assert.
            return;
        }
        assert_eq!(
            mine.classes.class_layout_domain, mine.jit.class_layout_domain,
            "one token per VM"
        );
        let define = |vm: &crate::vm::SharedVm, name: &str| {
            vm.classes
                .class_manager
                .write()
                .try_ensure_synthetic_class(name, 0)
                .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        };
        let epoch = || {
            cratonvm_classloading::store_epochs(mine.classes.class_layout_domain)
                .class_definition_epoch()
        };
        let held = || {
            mine.classes
                .annotation_proxy_absent_epoch
                .load(Ordering::Relaxed)
                == epoch()
        };
        let mut checked = false;
        for round in 0..50 {
            let before = epoch();
            if mine.classes.resolve_annotation_proxy_cid().is_some() {
                // The proxy class exists in this VM: no absence to hold.
                return;
            }
            define(
                &other,
                format!("cratonvm/test/W21L5AnnOther{round}").as_str(),
            );
            if epoch() != before {
                // A concurrent test's store shares this VM's slot.
                continue;
            }
            assert!(
                held(),
                "another VM's class definition must not retire this VM's answer"
            );
            checked = true;
            break;
        }
        assert!(checked, "no quiet window for this VM's epoch slot");
        define(&mine, "cratonvm/test/W21L5AnnMine");
        assert!(!held(), "this VM's own definition retires it");
    }
}

/// Interpreter round i1 wave 25, lane L5: the annotation-proxy latch is the
/// defining store's (`StoreEpochs::annotation_proxy_defined`), not a process
/// static, so a VM that mints the proxy class does not send every other VM's
/// virtual-invoke gate to the cold resolver.
#[cfg(test)]
mod w25_l5_annotation_proxy_latch_tests {
    #[test]
    fn another_vms_annotation_proxy_does_not_raise_this_vms_latch() {
        let mine = crate::vm::SharedVm::new(crate::VmConfig::default());
        let other = crate::vm::SharedVm::new(crate::VmConfig::default());
        let slot = |vm: &crate::vm::SharedVm| {
            cratonvm_classloading::store_epoch_slot(vm.classes.class_layout_domain)
        };
        let latch = |vm: &crate::vm::SharedVm| {
            cratonvm_classloading::store_epochs(vm.classes.class_layout_domain)
                .annotation_proxy_defined()
        };
        if slot(&mine) == slot(&other) || latch(&mine) {
            // The two stores share a slot, or a store sharing this VM's slot
            // (a concurrent test's VM) minted the class already: nothing to
            // assert.
            return;
        }
        other
            .classes
            .class_manager
            .write()
            .try_ensure_synthetic_class(super::ANNOTATION_PROXY_CLASS, 4)
            .expect("a VM-internal carrier is admitted in every mode");
        assert!(latch(&other), "the defining store's latch is raised");
        assert!(
            !latch(&mine),
            "another VM's annotation proxy must not raise this VM's latch"
        );
    }
}

#[cfg(test)]
mod getclass_mirror_words_tests {
    use super::{GetClassMirrorWords, GETCLASS_MIRROR_WORDS_CAPACITY};
    use crate::classloading::ClassId;
    use crate::types::ObjectRef;

    fn fake(addr: usize) -> ObjectRef {
        // SAFETY: used only as an address; never dereferenced.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// nio20 L3: before a compile allocates the table nothing is stored, so
    /// a VM that never JITs a `getClass()` pays no table; once allocated,
    /// word 0 is the capacity compiled code bounds-checks against.
    #[test]
    fn an_unallocated_table_stores_nothing_and_an_allocated_one_states_its_capacity() {
        let words = GetClassMirrorWords::new();
        let cid = ClassId::new(7);
        words.publish(cid, fake(0xD100));
        assert!(!words.is_allocated());
        assert_eq!(words.get(cid), 0);

        let base = words.base_addr();
        assert_ne!(base, 0);
        assert_eq!(words.base_addr(), base, "the address is stable");
        // SAFETY: `base` is the live allocation `words` owns; word 0 is in it.
        let capacity = unsafe { *(base as *const usize) };
        assert_eq!(capacity, GETCLASS_MIRROR_WORDS_CAPACITY);

        words.publish(cid, fake(0xD100));
        assert_eq!(words.get(cid), 0xD100);
        // SAFETY: as above; word `1 + 7` is inside the allocation.
        let raw = unsafe { *((base as *const usize).add(1 + 7)) };
        assert_eq!(raw, 0xD100, "compiled code reads word 1 + class_id");
    }

    /// JIT round 20 wave 19, lane zAssign: a mirror hint lands in the band
    /// slot compiled code reads (`mirror_hint_slot` of the address), and
    /// forgetting its class clears it -- and a remembered refusal -- but no
    /// other class's hint.
    #[test]
    fn a_mirror_hint_sits_in_its_slot_and_dies_with_its_class() {
        use cratonvm_jit::ir_lower as jl;
        let words = GetClassMirrorWords::new();
        words.publish_mirror_hint(0xE100, 1);
        assert_eq!(words.mirror_hint(0xE100), 0, "no table, no hint");
        let base = words.base_addr();
        let (a, b, c) = (0xE100usize, 0xE2F8usize, 0xE4F0usize);
        let (ca, cb, cc) = (ClassId::new(9), ClassId::new(10), ClassId::new(11));
        let hint = |cid: ClassId, ok: bool| {
            usize::try_from(jl::mirror_hint_word(cid.as_u32(), ok)).expect("64-bit")
        };
        words.publish_mirror_hint(a, hint(ca, true));
        words.publish_mirror_hint(b, hint(cb, false));
        let refused =
            usize::try_from(jl::MIRROR_HINT_DECLINED | u64::from(cc.as_u32())).expect("64-bit");
        words.publish_mirror_hint(c, refused);
        assert_eq!(words.mirror_hint(a), hint(ca, true));
        // SAFETY: `base` is the live allocation `words` owns; the hint band is
        // inside its `GETCLASS_WORDS_TOTAL` words.
        let raw = unsafe {
            *((base as *const usize)
                .add(jl::GETCLASS_WORDS_MIRROR_HINT_BASE + jl::mirror_hint_slot(a)))
        };
        assert_eq!(raw, hint(ca, true), "compiled code reads HINT_BASE + slot");
        words.forget(ca);
        words.forget(cc);
        assert_eq!(words.mirror_hint(a), 0);
        assert_eq!(words.mirror_hint(c), 0);
        assert_eq!(
            words.mirror_hint(b),
            hint(cb, false),
            "another class's hint stays"
        );
    }

    /// An id past the class band and outside the lambda band has no word:
    /// publishing it is a no-op and compiled code's bounds checks send it to
    /// the helper. nio20 wave 5 L2: a lambda proxy id inside the band HAS a
    /// word, at `1 + CAPACITY + (id - LAMBDA_PROXY_ID_BASE)`, which is where
    /// `getclass_table_lookup_bytes` reads it.
    #[test]
    fn an_id_past_the_capacity_has_no_word_but_a_lambda_id_has_its_band_word() {
        let words = GetClassMirrorWords::new();
        let base = words.base_addr();
        // Cast: the capacity fits a class id.
        let past = ClassId::new(GETCLASS_MIRROR_WORDS_CAPACITY as u32);
        words.publish(past, fake(0xD200));
        assert_eq!(words.get(past), 0);
        let between = ClassId::new(super::LAMBDA_PROXY_ID_BASE - 1);
        words.publish(between, fake(0xD200));
        assert_eq!(words.get(between), 0);
        let lcap = cratonvm_jit::ir_lower::GETCLASS_WORDS_LAMBDA_CAPACITY as u32; // Cast: 1 << 16
        let beyond = ClassId::new(super::LAMBDA_PROXY_ID_BASE + lcap);
        words.publish(beyond, fake(0xD200));
        assert_eq!(words.get(beyond), 0);

        let lambda = ClassId::new(super::LAMBDA_PROXY_ID_BASE + 3);
        words.publish(lambda, fake(0xD208));
        assert_eq!(words.get(lambda), 0xD208);
        // SAFETY: `base` is the live allocation `words` owns, and the index is
        // inside its `GETCLASS_WORDS_TOTAL` words.
        let raw = unsafe { *((base as *const usize).add(1 + GETCLASS_MIRROR_WORDS_CAPACITY + 3)) };
        assert_eq!(raw, 0xD208, "compiled code reads the lambda band");
        words.rekey(lambda, 0xD208, 0xD210);
        assert_eq!(words.get(lambda), 0xD210);
        words.forget(lambda);
        assert_eq!(words.get(lambda), 0);
    }

    /// A re-key moves only a word that names the old address; a word never
    /// published stays `0` (the helper keeps answering it), and `forget`
    /// clears.
    #[test]
    fn rekey_follows_only_the_published_address_and_forget_clears() {
        let words = GetClassMirrorWords::new();
        let _ = words.base_addr();
        let (published, unpublished) = (ClassId::new(11), ClassId::new(12));
        words.publish(published, fake(0xD300));
        words.rekey(published, 0xD300, 0xD308);
        assert_eq!(words.get(published), 0xD308);
        words.rekey(unpublished, 0xD310, 0xD318);
        assert_eq!(words.get(unpublished), 0, "never published: stays 0");
        // A stale `old` (not the word's value) leaves it alone.
        words.rekey(published, 0xD300, 0xD3F0);
        assert_eq!(words.get(published), 0xD308);
        words.forget(published);
        assert_eq!(words.get(published), 0);
    }

    /// nio20 w4 L2: the debug census reports at powers of two only, with both
    /// counts in `(served, declined)` order whichever side ticked.
    #[test]
    fn helper_call_census_reports_at_powers_of_two() {
        let words = GetClassMirrorWords::new();
        assert_eq!(words.note_helper_call(true), Some((1, 0)));
        assert_eq!(words.note_helper_call(true), Some((2, 0)));
        assert_eq!(words.note_helper_call(true), None);
        assert_eq!(words.note_helper_call(false), Some((3, 1)));
        assert_eq!(words.note_helper_call(true), Some((4, 1)));
        assert_eq!(words.note_helper_call(false), Some((4, 2)));
        assert_eq!(words.note_helper_call(false), None);
    }

    /// nio21 wave 1 L5: a primitive-array word sits where compiled code reads
    /// it (`GETCLASS_WORDS_PRIM_ARRAY_BASE + slot`), and the collector's
    /// re-key / forget of the ARRAY CLASS ID reach it; the class band word of
    /// that id is never written by the alias.
    #[test]
    fn prim_array_words_follow_their_array_class_id() {
        if !cratonvm_jit::ir_lower::getclass_prim_array_words_enabled() {
            return;
        }
        let words = GetClassMirrorWords::new();
        let arr = ClassId::new(21);
        // Before the table exists: nothing.
        words.publish_prim_array(6, arr, fake(0xD400));
        assert_eq!(words.get_prim_array(6), 0);
        let base = words.base_addr();
        words.publish_prim_array(6, arr, fake(0xD400));
        assert_eq!(words.get_prim_array(6), 0xD400);
        // SAFETY: `base` is the live allocation `words` owns, and the index is
        // inside its `GETCLASS_WORDS_TOTAL` words.
        let raw = unsafe {
            *((base as *const usize)
                .add(cratonvm_jit::ir_lower::GETCLASS_WORDS_PRIM_ARRAY_BASE + 6))
        };
        assert_eq!(raw, 0xD400, "compiled code reads the primitive-array band");
        assert_eq!(words.get(arr), 0, "the class band word is not the alias");
        words.rekey(arr, 0xD400, 0xD408);
        assert_eq!(words.get_prim_array(6), 0xD408);
        words.rekey(ClassId::new(22), 0xD408, 0xD4F0);
        assert_eq!(
            words.get_prim_array(6),
            0xD408,
            "another id leaves it alone"
        );
        words.forget(arr);
        assert_eq!(words.get_prim_array(6), 0);
        // A slot past the band is a no-op.
        words.publish_prim_array(8, arr, fake(0xD500));
        assert_eq!(words.get_prim_array(8), 0);
    }

    /// nio21 wave 2 L5: a reference-array word sits where compiled code reads
    /// it (`GETCLASS_WORDS_REF_ARRAY_BASE + component_id`); the collector's
    /// re-key / forget of the ARRAY CLASS ID reach it through the owner map,
    /// forgetting the COMPONENT clears it too, and ids past the band are
    /// no-ops.
    #[test]
    fn ref_array_words_follow_their_array_class_id() {
        if !cratonvm_jit::ir_lower::getclass_ref_array_words_enabled() {
            return;
        }
        let words = GetClassMirrorWords::new();
        let (component, arr) = (ClassId::new(31), ClassId::new(32));
        words.publish_ref_array(component, arr, fake(0xD600));
        assert_eq!(words.get_ref_array(component), 0, "no table yet");
        let base = words.base_addr();
        words.publish_ref_array(component, arr, fake(0xD600));
        assert_eq!(words.get_ref_array(component), 0xD600);
        // SAFETY: `base` is the live allocation `words` owns, and the index is
        // inside its `GETCLASS_WORDS_TOTAL` words.
        let raw = unsafe {
            *((base as *const usize)
                .add(cratonvm_jit::ir_lower::GETCLASS_WORDS_REF_ARRAY_BASE + 31))
        };
        assert_eq!(raw, 0xD600, "compiled code reads the reference-array band");
        assert_eq!(
            words.get(component),
            0,
            "the class band word is not the alias"
        );
        assert_eq!(words.get(arr), 0, "nor the array class's own word");
        words.rekey(arr, 0xD600, 0xD608);
        assert_eq!(words.get_ref_array(component), 0xD608);
        words.rekey(component, 0xD608, 0xD6F0);
        assert_eq!(
            words.get_ref_array(component),
            0xD608,
            "the component id is not the re-key"
        );
        words.forget(arr);
        assert_eq!(words.get_ref_array(component), 0);
        // Re-published, then the component forgotten: cleared as well.
        words.publish_ref_array(component, arr, fake(0xD610));
        assert_eq!(words.get_ref_array(component), 0xD610);
        words.forget(component);
        assert_eq!(words.get_ref_array(component), 0);
        // Ids past the band are no-ops.
        let cap = cratonvm_jit::ir_lower::GETCLASS_WORDS_REF_ARRAY_CAPACITY as u32; // Cast: 1 << 16
        words.publish_ref_array(ClassId::new(cap), arr, fake(0xD700));
        assert_eq!(words.get_ref_array(ClassId::new(cap)), 0);
        words.publish_ref_array(component, ClassId::new(cap), fake(0xD700));
        assert_eq!(words.get_ref_array(component), 0);
    }
}

#[cfg(test)]
mod gci_i47w_oz47_write_guard_tests {
    //! gci i47w lane oz47: `ClassManagerWriteGuard::run_before_hooks` runs its
    //! work with the class-manager write lock already released, once, at the
    //! guard's drop (`SharedVm::load_class_concurrent_for` clears its per-name
    //! loading flag there).
    use crate::vm::SharedVm;
    use crate::VmConfig;

    #[test]
    fn gci_i47w_oz47_before_hooks_work_runs_after_the_write_lock_is_released() {
        let shared = SharedVm::new(VmConfig::default());
        let ran = std::cell::Cell::new(0u32);
        {
            let mut guard = shared.classes.class_manager_write();
            guard.run_before_hooks(|| {
                assert!(
                    shared.classes.class_manager.try_read().is_some(),
                    "the write lock is released before the work runs"
                );
                ran.set(ran.get() + 1);
            });
            assert_eq!(ran.get(), 0, "nothing runs while the guard is held");
        }
        assert_eq!(ran.get(), 1, "runs exactly once, at the drop");
        // A guard that set nothing releases as before.
        drop(shared.classes.class_manager_write());
        assert!(shared.classes.class_manager.try_write().is_some());
    }
}
