// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Native implementations for selected Java Collections Framework methods.
//!
//! The default VM uses these as bridge intrinsics alongside real-JDK bytecode.
//! The `synthetic-jdk` feature enables additional synthetic-stub-only
//! registrations for minimal harnesses that do not load the full JDK classes.

use cratonvm_native_api::{
    ClassDiscriminator, NativeContext, NativeMethodRegistry, WellKnownClass,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::jfp::{double_max as java_double_max, double_min as java_double_min};
use cratonvm_types::ArrayElementType;
use cratonvm_types::ClassId;
use cratonvm_types::{ObjectKind, ObjectRef, Value};
use rustc_hash::FxHashMap;
use std::collections::HashMap as StdHashMap;
use std::sync::Mutex;

// `pub` (gated by `#[doc(hidden)]` on the items themselves) so the
// GC-relocation integration harness in `tests/gc_relocation_harness.rs`
// can drive `obj_key` against a stub NativeContext directly.
#[doc(hidden)]
pub mod identity_hash;
mod overlay_rows;
use identity_hash::seed as ih_seed;
use overlay_rows::OverlayRows;

// ---------------------------------------------------------------------------
// Receiver classification memo (perf/collections-classification-cost)
//
// THE COST THIS REMOVES. `NativeContext::class_name_of_id` returns
// `Option<String>` and the VM implements it as a `class_manager` **RwLock read
// acquisition plus a heap `String` allocation**, per call
// (`vm/src/vm/vm_exec.rs`, `…classes.class_manager.read().get_class(id).map(|c|
// c.name.to_string())`). The receiver-classification predicates below
// (`is_tree_map_receiver`, `is_chm_receiver`, `is_lhm_receiver`,
// `is_unmod_wrapper`, `is_bare_java_lang_object`, `uses_native_hashtable_layout`,
// `unwrap_unmod`, and `map_state`'s own `receiver_class_name`) each called it
// once per superclass hop, and they compose: `HashMap.get` took 3,
// `HashMap.put` 3-4 plus 2 more through `map_state` plus a further full walk
// inside `native_map_put_evict`, `HashMap.remove` 4. A node-path `put`
// therefore paid on the order of **5-6 lock acquisitions and 5-6 `String`
// allocations of pure classification overhead before any lookup work** —
// recomputed on every call, for a receiver whose `ClassId` never changes. That
// is the shape of the measured 21.2x `HashMap` benchmark row against JDK 25 C2
// (versus 2.2-2.9x on arithmetic / sieve / matrix) and of its sub-linear
// scaling: a large fixed per-op constant, not a fragmentation signature.
//
// (`NativeContext::class_name_arc_of_id` has since removed the *allocation*
// half of that cost for every caller in the workspace — it hands back a clone
// of the `Arc<str>` the class already owns. It does not remove the lock, which
// is why these memos still exist and still pay for themselves.)
//
// THE FIX. Every one of those questions is a pure function of the receiver's
// `ClassId`: it depends only on the class's own name and its superclass chain,
// both immutable after linking (JVMTI redefinition may not change either — the
// structural check rejects a changed name or supertype). So classify each
// `ClassId` ONCE into a bitset and answer every later query from a
// thread-local direct-mapped cache: no lock, no allocation, no `str` compare.
//
// WHY A `ClassId` KEY NEEDS NO GENERATION STAMP. Class unloading landed on
// `dev` this wave, which would ordinarily make a bare `ClassId` key unsound —
// a recycled id could carry a dead class's classification onto a live one.
// It does not, because `ClassId`s are never reissued:
//
//   * `ClassStore` is a `Vec<Option<Class>>` (`classloading/src/class.rs:892`).
//   * `ClassStore::remove` (`:1096`) does `…get_mut(id)?.take()?` — it leaves a
//     `None` TOMBSTONE in the slot; it never shortens the vector.
//   * `ClassStore::next_id` (`:909`) is `ClassId::new(self.classes.len())` —
//     the SLOT VECTOR's length, deliberately not `live_count` (which `remove`
//     does decrement). Its doc comment states the intent: "ClassIds are
//     deliberately never reused".
//   * `add` (`:917`) only ever `push`es. Auditing every write to the slot
//     vector confirms it: the sole mutating accesses in the whole file are that
//     `push` and the `take` above — nothing ever stores `Some(_)` back into an
//     existing slot.
//   * The invariant is pinned by the test
//     `unloaded_slots_are_tombstoned_and_never_reused` (`:3034`), which unloads
//     id 0, then asserts `next_id() == 2` and that the next `add` returns 2.
//
// So a `ClassId` that once denoted `java/util/TreeMap` denotes it or nothing,
// forever. A memo entry for an unloaded class is stale but unreachable: the
// only key that could reach it can never be handed out again. No generation
// stamp, no unload hook, no invalidation.
//
// The cache is still scoped by `vm_identity`, for the unrelated reason that
// Rust tests stand up several `Vm`s in one process and `ClassId` spaces are
// per-VM (the same precedent as `PRIMITIVE_WRAPPER_CLASS_CACHE`).
// ---------------------------------------------------------------------------

/// Exact class is `java/lang/Object` (backs `is_bare_java_lang_object`).
const CF_EXACT_OBJECT: u32 = 1 << 0;
/// Exact class is `java/util/HashMap` (backs the integer-overlay entry test in
/// `native_map_put_evict`).
const CF_EXACT_HASHMAP: u32 = 1 << 1;
/// Exact class is one of the seven `cratonvm/internal/Unmodifiable*` views
/// (backs both `is_unmod_wrapper` and `unwrap_unmod`, whose name sets are
/// identical).
const CF_UNMOD_WRAPPER: u32 = 1 << 2;
/// `is_tree_map_receiver`: `TreeMap` met before `HashMap` / `Object`.
const CF_TREE_MAP: u32 = 1 << 3;
/// `is_chm_receiver`: `ConcurrentHashMap` met before `HashMap` / `TreeMap` /
/// `Object`.
const CF_CHM: u32 = 1 << 4;
/// `is_lhm_receiver`: `LinkedHashMap` met before `HashMap` / `TreeMap` /
/// `Object`.
const CF_LHM: u32 = 1 << 5;
/// `uses_native_hashtable_layout`: `Hashtable` ancestry with `Properties`
/// EXCLUDED — JDK 25 backs `Properties` with a side `ConcurrentHashMap`, not
/// the native Hashtable bucket layout.
const CF_HASHTABLE_LAYOUT: u32 = 1 << 6;
/// Hashtable ancestry with `Properties` INCLUDED. `native_map_put_evict`'s
/// walk wants this one: a `Properties` receiver still needs
/// `ensure_hashtable_load_factor`, which `CF_HASHTABLE_LAYOUT` would skip.
const CF_HASHTABLE_ANCESTRY: u32 = 1 << 7;
/// `is_native_bucket_map`'s own walk: `HashMap` or `Hashtable` met before
/// `Object`.
const CF_BUCKET_MAP_NAME: u32 = 1 << 8;
/// `class_name_of_id` returned `Some` for this exact class.
///
/// `native_map_put_evict` gates its whole family-dispatch block on
/// `if let Some(name) = ctx.class_name_of_id(cid)`, falling through to the
/// plain bucket path for a nameless receiver (a class-id-0 synthetic
/// allocation). Preserving that guard needs the *presence* of a name, not the
/// name, so it becomes a bit here.
///
/// Caching it is sound in both directions. `class_name_of_id` answers `None`
/// only when `ClassStore::get` misses, i.e. the id is unloaded or not yet
/// filled — and in either state no live object carries that `ClassId`, so the
/// stale bit is unreachable: `receiver_facts` is only ever asked about the
/// runtime class of an object that exists.
const CF_HAS_NAME: u32 = 1 << 9;
/// `IdentityHashMap` ancestry (exact class or a user subclass).
///
/// `IdentityHashMap.get`/`put` are registered straight onto the generic
/// bucket-map natives (`native_map_get_pub`/`native_map_put_pub` — see
/// `register_identity_hashmap_natives`), which by default compute the bucket
/// hash and collision equality via the key's *virtual* `hashCode()`/`equals()`
/// — content semantics. That happens to coincide with reference semantics for
/// a key class like `java.lang.Class` that doesn't override either method,
/// which is why this went unnoticed, but it is not what `IdentityHashMap`'s
/// contract requires, and every `hashCode()`/`equals()` dispatch it triggers
/// is a moving-GC window a pure identity computation would not need at all.
/// `map_hash_key`/`map_keys_equal` consult this flag to skip virtual dispatch
/// entirely for an identity-map receiver — see `map_hash_key_identity`
/// and `map_keys_equal_identity`.
const CF_IDENTITY_MAP: u32 = 1 << 10;
/// Exact class is `java/util/concurrent/ConcurrentHashMap$KeySetView` — the
/// object `newKeySet()` / `keySet(V)` hands back.
///
/// It is neither HashSet-shaped nor ArrayList-shaped: slot 0 is the backing
/// `ConcurrentHashMap`, whose own slot 0 is a segments array rather than a
/// bucket table. The interface-level natives (`java/util/Set.size`,
/// `java/util/Collection.iterator`, …) would read those slots as their own
/// layout and answer 0 / null, so they consult this bit and reroute — see
/// `ksv_route`. A memoized bit rather than a `class_name_of_id` call because
/// those natives are on the hot path for every ordinary HashSet and ArrayList
/// too (see the `collections-classification-cost` notes above).
const CF_KEY_SET_VIEW: u32 = 1 << 11;
/// Exact class is one of the five real JDK classes `Map.values()` returns:
/// `HashMap$Values`, `LinkedHashMap$LinkedValues`, `TreeMap$Values`,
/// `Hashtable$ValueCollection`, `ConcurrentHashMap$ValuesView`.
///
/// Every one of them holds its source map in a single reference field and
/// nothing else — so it matches neither the ArrayList layout the `native_al_*`
/// natives read nor the HashSet layout the `native_hs_*` natives read. Reaching
/// one of those natives with such a receiver answers EMPTY today (`al_state`'s
/// layout guard reports `data = None`, which every caller reads as size 0), and
/// its real bytecode is no better: `HashMap$Values.size()` is
/// `getfield this$0.size`, a field this VM never writes for an integer-keyed
/// map (see [`is_values_view_class`] for the measurement). `vc_route` consults
/// this bit and re-expresses the call over the live source map.
///
/// A memoized bit rather than a `class_name_of_id` call for the same reason
/// [`CF_KEY_SET_VIEW`] is one: the natives that ask are on the hot path for
/// every ordinary `ArrayList` too.
const CF_VALUES_VIEW: u32 = 1 << 12;

/// The receiver's class is [`TM_ENTRY_SET_CARRIER`] — the ONE
/// [`MAP_VIEW_CARRIERS`] member that is not a values view.
///
/// Its own bit rather than a sixth name in [`CF_VALUES_VIEW`], because
/// three predicates read that one and two of them mean *values*:
/// [`al_is_values_view`] has to answer `false` here (the real class extends
/// `AbstractSet`, which DOES override `equals`/`hashCode`, so the
/// content-based answer is the right one), while [`vc_route`] has to answer
/// `true` — it has carried an entry-shaped arm since `RTreeRangeGc`, keyed
/// on [`view_carrier_holds_entries_by_class`], and that arm was UNREACHABLE:
/// the route's gate was the values predicate, so the one carrier the arm was
/// written for never got past the door.
///
/// MEASURED on the wave-8 control, `--jdk-only`, dial armed on
/// `java/util/TreeMap`, `apps/probes/L1MapViewToArrayProbe`:
/// `treemap.entrySet.toArray.len` answered **0** where HotSpot 25.0.4+7
/// answers 3, while `treemap.values.toArray.len` — the same route, a
/// receiver one name over — was already right and printed its `[VIEWKIND]`
/// line saying so.
const CF_MAP_ENTRY_VIEW: u32 = 1 << 13;
/// Exact class is `java/lang/Boolean`. `unbox_wrapper` flattens a `Boolean`
/// to `Int(0|1)`, but `Boolean.hashCode()` is 1231/1237, so every hash that
/// starts from the unboxed value has to ask this first (round 13 wave 6,
/// `CRATONVM_COMPAT_BOOLEAN_KEY_HASH`, and the wave-4 element hash).
const CF_BOOLEAN_BOX: u32 = 1 << 14;
/// Exact class is `java/util/HashMap$Node`: a plain bucket node that real
/// `HashMap.treeifyBin` bytecode may convert (round 13 wave 11, lane compat6,
/// [`hm_tree_bins_enabled`]). The set is `u32` since this flag and the next
/// filled the old `u16`.
const CF_HM_NODE: u32 = 1 << 15;
/// Exact class is `java/util/HashMap$TreeNode`: the head of a JDK tree bin
/// ([`is_hm_tree_node`]).
const CF_HM_TREE_NODE: u32 = 1 << 16;
/// Exact class is `java/util/LinkedHashMap$Entry`: a plain `LinkedHashMap`
/// bucket node that `treeifyBin` may convert ([`lhm_tree_bins_enabled`]).
const CF_LHM_ENTRY: u32 = 1 << 17;
/// `java/lang/Enum` ancestry (an enum constant's class or its constant-body
/// subclass). [`enum_key_identity`] asks it of every non-wrapper,
/// non-`String` map key -- every user key's hash -- and its own walk was a
/// `class_name_of_id` (a `class_manager` read and a `String`) per superclass
/// hop. JIT round 16 (lane compatdrain).
const CF_ENUM: u32 = 1 << 18;
/// An IDENTITY key class: neither the class nor any superclass below
/// `java/lang/Object` declares `hashCode()I` or `equals(Ljava/lang/Object;)Z`,
/// so `key.hashCode()` is `Object.hashCode()` (the identity hash) and
/// `key.equals(o)` is `key == o` -- `java.lang.Class`, a bare `Object`, a
/// lambda, a plain user class. [`map_key_raw_hash`] / [`map_keys_equal`] answer
/// it without the re-entrant Java call (nio20 wave 6, L5: the `--compatible`
/// `ConcurrentHashMap` stand-in paid one per lookup for every `Class`-keyed JDK
/// cache, `LocaleServiceProviderPool.poolOfPools` among them). See
/// [`class_has_identity_hash_and_equals`] for the classes it refuses.
const CF_IDENTITY_KEY: u32 = 1 << 19;
/// Exact class is `java/util/Locale` (final): its `hashCode()` caches itself
/// in the `hashCodeValue` field, which [`map_key_raw_hash`] reads instead of
/// calling it once it is set (nio20 wave 6, L5).
const CF_LOCALE: u32 = 1 << 20;

/// Cached classification of one `ClassId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ClassFacts(u32);

impl ClassFacts {
    #[inline]
    fn has(self, flag: u32) -> bool {
        self.0 & flag != 0
    }
}

/// How far up a superclass chain the shared walk looks.
///
/// The predicates this replaces used `for _ in 0..32` (four of them) or an
/// unbounded `loop` with a self-cycle guard (`is_tree_map_receiver`). 64 is at
/// least as permissive as every one of them, so no receiver that used to be
/// recognised stops being recognised; a chain deeper than 64 does not occur
/// (`TreeMap` sits 2 hops below `Object`).
const FACTS_WALK_LIMIT: usize = 64;

/// How many well-known classes the shared walk records along one chain. Every
/// predicate's decision is made by the FIRST member of its interest set met,
/// and a real chain crosses at most a couple of them; the cap only bounds the
/// scratch array.
const FACTS_CHAIN_MAX: usize = 8;

/// Reproduce one of the original `match`-arm superclass walks: `true` iff a
/// member of `yes` is met before any member of `no`, scanning the recorded
/// chain in superclass order. `false` when neither is met (the exhausted-walk
/// arm every original shared).
#[inline]
fn chain_decides(chain: &[WellKnownClass], yes: &[WellKnownClass], no: &[WellKnownClass]) -> bool {
    for entry in chain {
        if yes.contains(entry) {
            return true;
        }
        if no.contains(entry) {
            return false;
        }
    }
    false
}

/// Classify `cid` — the cold path, run once per `ClassId` per thread.
///
/// One superclass walk answers every predicate, where the originals each ran
/// their own. `ClassDiscriminator::well_known_class` is itself allocation-free
/// once the handful of interesting classes have resolved, so even this path no
/// longer materialises a `String` per hop.
fn classify_class(ctx: &dyn NativeContext, cid: ClassId) -> ClassFacts {
    use WellKnownClass as W;

    let exact = ctx.well_known_class(cid);
    let mut chain = [W::Object; FACTS_CHAIN_MAX];
    let mut len = 0usize;
    let mut cur = cid;
    for _ in 0..FACTS_WALK_LIMIT {
        if let Some(found) = ctx.well_known_class(cur) {
            if len < FACTS_CHAIN_MAX {
                chain[len] = found;
                len += 1;
            }
            if matches!(found, W::Object) {
                // Every original walk treats `java/lang/Object` as terminal.
                break;
            }
        }
        match ctx.superclass_of(cur) {
            // `p != cur` is the self-cycle guard the originals carried; a
            // malformed chain must not spin.
            Some(parent) if parent != cur => cur = parent,
            _ => break,
        }
    }
    let chain = &chain[..len];

    let mut flags = 0u32;
    if matches!(exact, Some(W::Object)) {
        flags |= CF_EXACT_OBJECT;
    }
    if matches!(exact, Some(W::HashMap)) {
        flags |= CF_EXACT_HASHMAP;
    }
    if matches!(
        exact,
        Some(
            W::UnmodifiableMap
                | W::UnmodifiableList
                | W::UnmodifiableSet
                | W::UnmodifiableSortedSet
                | W::UnmodifiableNavigableSet
                | W::UnmodifiableEntrySet
                | W::UnmodifiableCollection
        )
    ) {
        flags |= CF_UNMOD_WRAPPER;
    }
    if chain_decides(chain, &[W::TreeMap], &[W::HashMap, W::Object]) {
        flags |= CF_TREE_MAP;
    }
    if chain_decides(
        chain,
        &[W::ConcurrentHashMap],
        &[W::HashMap, W::TreeMap, W::Object],
    ) {
        flags |= CF_CHM;
    }
    if chain_decides(
        chain,
        &[W::LinkedHashMap],
        &[W::HashMap, W::TreeMap, W::Object],
    ) {
        flags |= CF_LHM;
    }
    if chain_decides(
        chain,
        &[W::Hashtable],
        &[W::Properties, W::HashMap, W::Object],
    ) {
        flags |= CF_HASHTABLE_LAYOUT;
    }
    if chain_decides(chain, &[W::Hashtable], &[W::HashMap, W::Object]) {
        flags |= CF_HASHTABLE_ANCESTRY;
    }
    if chain_decides(chain, &[W::HashMap, W::Hashtable], &[W::Object]) {
        flags |= CF_BUCKET_MAP_NAME;
    }
    // The one remaining `class_name_of_id` on this path, and it runs once per
    // `ClassId` per thread rather than once per map operation. A well-known
    // class necessarily has a name, so the common case skips the call.
    if exact.is_some() || ctx.class_name_of_id(cid).is_some() {
        flags |= CF_HAS_NAME;
    }
    // `IdentityHashMap` is not a `WellKnownClass` variant (it participates in
    // no other family decision here), so a direct name walk is simpler than
    // adding a 16th variant for this one flag. Paid once per `ClassId`, then
    // cached forever by `receiver_facts` like every other bit above.
    {
        let mut cur = cid;
        for _ in 0..FACTS_WALK_LIMIT {
            if ctx.class_name_arc_of_id(cur).as_deref() == Some("java/util/IdentityHashMap") {
                flags |= CF_IDENTITY_MAP;
                break;
            }
            match ctx.superclass_of(cur) {
                Some(parent) if parent != cur => cur = parent,
                _ => break,
            }
        }
    }
    // `ConcurrentHashMap$KeySetView` decides no family question the
    // `WellKnownClass` chain walk above answers — it is an exact-class test on
    // one name — so it gets the same one-name treatment as `IdentityHashMap`
    // rather than a 17th variant. Paid once per `ClassId`, then cached forever.
    //
    // Under the `JdkConcurrentHashMap` row (wave 76, L1) every CHM view is the
    // JDK's own object, never a carrier this crate minted, so neither CHM view
    // class gets a carrier bit (here and in `CF_VALUES_VIEW` below). The memo
    // is per VM (`vm_identity`), and the row is fixed for a VM's life.
    let chm_views_are_jdk = chm_jdk_bodies(ctx);
    if ctx.class_name_arc_of_id(cid).as_deref() == Some(KSV_CLASS) && !chm_views_are_jdk {
        flags |= CF_KEY_SET_VIEW;
    }
    // The five `Map.values()` view classes — same one-name treatment, and the
    // same `class_name_arc_of_id` the two blocks above already pay for. Exact
    // class, not ancestry: `AbstractCollection` is the shared superclass of
    // four of them AND of every foreign collection that legitimately reaches
    // these natives, so an ancestry test would capture receivers whose backing
    // this VM does not own. See [`CF_VALUES_VIEW`].
    if matches!(
        ctx.class_name_arc_of_id(cid).as_deref(),
        Some(
            "java/util/HashMap$Values"
                | "java/util/LinkedHashMap$LinkedValues"
                | "java/util/TreeMap$Values"
                | "java/util/Hashtable$ValueCollection"
        )
    ) || (ctx.class_name_arc_of_id(cid).as_deref()
        == Some("java/util/concurrent/ConcurrentHashMap$ValuesView")
        && !chm_views_are_jdk)
    {
        flags |= CF_VALUES_VIEW;
    }
    // The sixth `MAP_VIEW_CARRIERS` member, on its own bit. See
    // [`CF_MAP_ENTRY_VIEW`] for why it is not a sixth name in the match above.
    if ctx.class_name_arc_of_id(cid).as_deref() == Some(TM_ENTRY_SET_CARRIER) {
        flags |= CF_MAP_ENTRY_VIEW;
    }
    if ctx.class_name_arc_of_id(cid).as_deref() == Some("java/lang/Boolean") {
        flags |= CF_BOOLEAN_BOX;
    }
    {
        let mut cur = cid;
        for _ in 0..FACTS_WALK_LIMIT {
            match ctx.class_name_arc_of_id(cur).as_deref() {
                Some("java/lang/Enum") => {
                    flags |= CF_ENUM;
                    break;
                }
                Some("java/lang/Object") | None => break,
                Some(_) => {}
            }
            match ctx.superclass_of(cur) {
                Some(parent) if parent != cur => cur = parent,
                _ => break,
            }
        }
    }
    match ctx.class_name_arc_of_id(cid).as_deref() {
        Some(HM_NODE_CLASS) => flags |= CF_HM_NODE,
        Some(HM_TREE_NODE_CLASS) => flags |= CF_HM_TREE_NODE,
        Some(LHM_ENTRY_CLASS) => flags |= CF_LHM_ENTRY,
        Some("java/util/Locale") => flags |= CF_LOCALE,
        _ => {}
    }
    if flags & CF_ENUM == 0 && class_has_identity_hash_and_equals(ctx, cid) {
        flags |= CF_IDENTITY_KEY;
    }
    ClassFacts(flags)
}

/// Does `cid` take `Object.hashCode()` and `Object.equals(Object)` unchanged
/// (the [`CF_IDENTITY_KEY`] question)? Cold: once per `ClassId` per thread.
///
/// `true` only when every class from `cid` up to (not including)
/// `java/lang/Object` declares neither method, AND the context proves it can
/// answer the question: `java/lang/Object` itself must report declaring both
/// (a context that answers `class_declares_method` with its trait default
/// `false` everywhere -- a mock, a synthetic library without `Object`'s
/// methods -- would otherwise call every class an identity class).
///
/// Refused outright, whatever the walk says, because something other than the
/// JDK's `Object` bodies answers `hashCode` / `equals` for them in this VM:
/// arrays; abstract / interface classes (only a VM-minted carrier has one as
/// its exact class, and its answers are registered natives'); the three
/// generic-reflection stubs `native_object_hash_code` hashes structurally;
/// `java/lang/Package` (a registered `hashCode` stand-in); any `cratonvm/`
/// carrier; and a fabricated synthetic stub class. An enum is never one
/// (`Enum` declares both), so the [`CF_ENUM`] arm keeps it.
fn class_has_identity_hash_and_equals(ctx: &dyn NativeContext, cid: ClassId) -> bool {
    const HASH: (&str, &str) = ("hashCode", "()I");
    const EQUALS: (&str, &str) = ("equals", "(Ljava/lang/Object;)Z");
    const ACC_INTERFACE_OR_ABSTRACT: u16 = 0x0200 | 0x0400;
    let Some(name) = ctx.class_name_arc_of_id(cid) else {
        return false;
    };
    if name.starts_with('[')
        || name.starts_with("cratonvm/")
        || matches!(
            &*name,
            "java/lang/reflect/GenericArrayType"
                | "java/lang/reflect/ParameterizedType"
                | "java/lang/reflect/WildcardType"
                | "java/lang/Package"
        )
        || ctx.class_access_flags(cid) & ACC_INTERFACE_OR_ABSTRACT != 0
        || ctx.is_class_synthetic_stub(&name)
    {
        return false;
    }
    let mut cur = cid;
    for _ in 0..FACTS_WALK_LIMIT {
        if ctx.class_name_arc_of_id(cur).as_deref() == Some("java/lang/Object") {
            return ctx.class_declares_method(cur, HASH.0, HASH.1)
                && ctx.class_declares_method(cur, EQUALS.0, EQUALS.1);
        }
        if ctx.class_declares_method(cur, HASH.0, HASH.1)
            || ctx.class_declares_method(cur, EQUALS.0, EQUALS.1)
        {
            return false;
        }
        match ctx.superclass_of(cur) {
            Some(parent) if parent != cur => cur = parent,
            _ => return false,
        }
    }
    false
}

/// `CRATONVM_COMPAT_IDENTITY_KEY_HASH` -- default ON, `=0` restores the
/// re-entrant `hashCode()` / `equals()` calls for [`CF_IDENTITY_KEY`] keys
/// and the `Locale.hashCode()` call for a `Locale` whose `hashCodeValue` is
/// already set. The answers are the same by construction (the JDK bodies those
/// calls run ARE an identity hash / `==` / a read of `hashCodeValue`); only
/// the native-to-Java crossing goes. nio20 wave 6, L5.
fn identity_key_hash_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    compat_switch_on(&ON, "CRATONVM_COMPAT_IDENTITY_KEY_HASH")
}

/// `Locale.hashCode()` without calling it: the JDK 25 body is
/// `int hc = hashCodeValue; if (hc == 0) { ..compute..; hashCodeValue = hc; }
/// return hc;`, so a nonzero `hashCodeValue` IS the answer. `None` (call the
/// method) while it is still 0 or when the layout has no such field (the
/// synthetic library's padded `Locale`). The slot index is memoized per
/// (VM, class id); `Locale` is one class, so one entry.
fn locale_cached_hash(ctx: &dyn NativeContext, key: ObjectRef, cid: ClassId) -> Option<i32> {
    thread_local! {
        /// `(vm_identity, class id, slot)`; slot `u32::MAX` = no such field.
        static LOCALE_HASH_SLOT: std::cell::Cell<(usize, u32, u32)> =
            const { std::cell::Cell::new((0, u32::MAX, u32::MAX)) };
    }
    let vm = ctx.vm_identity();
    let raw = cid.as_u32();
    let (memo_vm, memo_cid, memo_slot) = LOCALE_HASH_SLOT.with(|c| c.get());
    let slot = if memo_vm == vm && memo_cid == raw {
        memo_slot
    } else {
        let slot = ctx
            .resolve_field_index_by_class_id(cid, "hashCodeValue")
            .map_or(u32::MAX, |i| i as u32);
        LOCALE_HASH_SLOT.with(|c| c.set((vm, raw, slot)));
        slot
    };
    if slot == u32::MAX {
        return None;
    }
    match ctx.get_field(key, slot as usize) {
        Value::Int(h) if h != 0 => Some(h),
        _ => None,
    }
}

/// Ways in the direct-mapped classification cache. A power of two so the index
/// is a mask.
///
/// Direct-mapped rather than a growable map on purpose: a `Vec` indexed by
/// `ClassId` would be sound (ids are never reissued — see the module comment)
/// but its footprint scales with the application's class count, per thread. A
/// fixed 512 slots is ~4 KiB per thread and holds every map class a workload
/// actually dispatches on. A tag collision costs a re-classification, never a
/// wrong answer: the slot stores the `ClassId` it was filled for and is only
/// trusted on an exact match.
const RECEIVER_FACTS_WAYS: usize = 512;

/// Empty-slot marker. A real `ClassId` is an index into `ClassStore`'s slot
/// vector, so `u32::MAX` is unreachable.
const RECEIVER_FACTS_EMPTY: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct FactsSlot {
    class_id: u32,
    facts: ClassFacts,
}

impl FactsSlot {
    const EMPTY: Self = Self {
        class_id: RECEIVER_FACTS_EMPTY,
        facts: ClassFacts(0),
    };
}

struct ReceiverFactsCache {
    /// `None` until first use; see the module comment on `vm_identity` scoping.
    vm: Option<usize>,
    slots: [FactsSlot; RECEIVER_FACTS_WAYS],
}

impl ReceiverFactsCache {
    const fn new() -> Self {
        Self {
            vm: None,
            slots: [FactsSlot::EMPTY; RECEIVER_FACTS_WAYS],
        }
    }
}

thread_local! {
    static RECEIVER_FACTS: std::cell::RefCell<ReceiverFactsCache> =
        const { std::cell::RefCell::new(ReceiverFactsCache::new()) };
}

/// Direct-mapped slot for `class_id`. Multiplicative mix then take the high
/// bits, the same shape as `obj_key_shard_for`, so ids loaded consecutively
/// (as the `java/util` classes are) do not all land in one slot.
#[inline]
fn facts_slot_index(class_id: u32) -> usize {
    let mixed = (class_id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    ((mixed >> 55) as usize) & (RECEIVER_FACTS_WAYS - 1)
}

/// Every classification question about `cid`, answered from the thread-local
/// cache. Lock-free and allocation-free after the first call per `ClassId`.
#[inline]
fn class_facts(ctx: &dyn NativeContext, cid: ClassId) -> ClassFacts {
    let raw = cid.as_u32();
    if raw == RECEIVER_FACTS_EMPTY {
        // Not a reachable id; never let it match the empty marker.
        return classify_class(ctx, cid);
    }
    let vm = ctx.vm_identity();
    let index = facts_slot_index(raw);
    let hit = RECEIVER_FACTS.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.vm != Some(vm) {
            *cache = ReceiverFactsCache::new();
            cache.vm = Some(vm);
        }
        let slot = cache.slots[index];
        (slot.class_id == raw).then_some(slot.facts)
    });
    if let Some(facts) = hit {
        return facts;
    }
    // Cold. The borrow above is released first: `classify_class` re-enters the
    // VM, which must never find this `RefCell` already borrowed.
    let facts = classify_class(ctx, cid);
    RECEIVER_FACTS.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.vm == Some(vm) {
            cache.slots[index] = FactsSlot {
                class_id: raw,
                facts,
            };
        }
    });
    facts
}

/// `class_facts` for a heap object's runtime class.
#[inline]
fn receiver_facts(ctx: &dyn NativeContext, obj: ObjectRef) -> ClassFacts {
    class_facts(ctx, ctx.class_id_of_object(obj))
}

/// Ways in the direct-mapped receiver-class-NAME cache.
///
/// Separate from `RECEIVER_FACTS` because an `Rc<str>` is not `Copy`, and much
/// smaller because only two callers need the name itself rather than a
/// classification (`map_state`, which passes it to `resolve_field_index`, and
/// `native_map_put_evict`'s `ensure_hashtable_load_factor`).
const RECEIVER_NAMES_WAYS: usize = 64;

thread_local! {
    /// `(vm_identity, [(ClassId, name); WAYS])`. Handing out an
    /// `Rc<str>` clone is a non-atomic refcount bump — no lock, no allocation,
    /// where `class_name_of_id` was a `class_manager` read lock plus a fresh
    /// `String` every call. Sound for the same never-reissued-`ClassId` reason
    /// as `RECEIVER_FACTS`; a class name is immutable after linking.
    static RECEIVER_NAMES: std::cell::RefCell<(
        Option<usize>,
        [Option<(u32, std::rc::Rc<str>)>; RECEIVER_NAMES_WAYS],
    )> = std::cell::RefCell::new((None, std::array::from_fn(|_| None)));
}

/// The binary name of `cid`, memoized per thread.
///
/// `None` exactly when `class_name_of_id` returns `None` (unnamed / synthetic
/// class), and a `None` is deliberately not cached — the classification memo's
/// argument for caching successes does not extend to a lookup that can start
/// succeeding later.
fn class_name_rc(ctx: &dyn NativeContext, cid: ClassId) -> Option<std::rc::Rc<str>> {
    let raw = cid.as_u32();
    let vm = ctx.vm_identity();
    let index = (facts_slot_index(raw)) & (RECEIVER_NAMES_WAYS - 1);
    let hit = RECEIVER_NAMES.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.0 != Some(vm) {
            *cache = (Some(vm), std::array::from_fn(|_| None));
        }
        match &cache.1[index] {
            Some((cached_id, name)) if *cached_id == raw => Some(name.clone()),
            _ => None,
        }
    });
    if hit.is_some() {
        return hit;
    }
    // `class_name_arc_of_id`, not `class_name_of_id`: the miss path used to
    // allocate a `String` purely to copy it straight into the `Rc`. The `Arc`
    // deref feeds `Rc::from(&str)` the same bytes with one allocation instead
    // of two.
    let name: std::rc::Rc<str> = std::rc::Rc::from(&*ctx.class_name_arc_of_id(cid)?);
    RECEIVER_NAMES.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.0 == Some(vm) {
            cache.1[index] = Some((raw, name.clone()));
        }
    });
    Some(name)
}

// ---------------------------------------------------------------------------
// Widened side-table key (MEDIUM finding: 32-bit identity-hash aliasing).
//
// The four collection overlays (LinkedList `ll_overlay`, LinkedHashMap
// `lhm_overlay`, TreeMap `tm_array_table`/`tm_fast_table`, TreeSet
// `ts_array_table`) used to key their process-global side-tables by
// `identity_hash_code(this) as u32 as usize`. `NativeContext::identity_hash_code`
// only yields a 32-bit `i32`, so two distinct *live* collections whose
// identity hashes collide (birthday-bound ~50% near ~77k live collections)
// aliased the SAME side-table entry and silently corrupted each other's
// backing store.
//
// `identity_hash_code` has no more entropy to give, so we cannot simply widen
// the cast. Instead `widened_obj_key` keeps the GC-move stability that the
// identity-hash key was introduced for (a moving GC copies the hash word with
// the object header, so the hash is invariant across relocation) while adding
// a per-object disambiguator that splits genuine 32-bit collisions:
//
//   key = ((identity_hash as u64) << 32) | (generation as u64)
//
// A process-global registry (`obj_key_registry`) maps each 32-bit identity
// hash to the set of distinct objects observed under it. Each gets its own
// `generation`, so two colliding objects receive distinct full-width keys and
// can never alias. The registry resolves a slot by `(calling VM, the object's
// CURRENT raw pointer)` and by nothing else:
//
//   * a slot of this VM records this pointer -> reuse that slot's gen (steady
//     state: same object, same address). A GC-relocated object is resolved here
//     too: every moving collection's stop-the-world epilogue runs the post-GC
//     remap (`gc_update_collection_overlay_refs_for_vm`, reached from
//     `update_all_roots` through the "collection-overlays" root source), which
//     advances every relocated slot's `last_ptr` to the new address before any
//     mutator resumes.
//   * any other pointer -> a different object (or a caller holding a stale copy
//     of a moved object's address, a bug the stale-handle audits gate): mint a
//     fresh generation, so the newcomer gets its own entry instead of aliasing
//     an existing one. A second LIVE collection sharing the identity hash is
//     this case, and so is an object recycling a DEAD collection's hash before
//     the prune has run.
//
// (gc-common w26-a removed a "lone occupant" step that took a lone same-class
// slot at another address to be the caller MOVED and rebound it, and cleared
// a lone different-class slot's rows. Two live same-hash collections then
// shared one overlay, and a different-class one wiped a live collection's
// contents. See
// `docs/feature-designs/collections-interception.md`.)
//
// Generations are handed out monotonically (one past the max generation present
// in the bucket), never as `slots.len()`: once any slot is removed (GC prune),
// `slots.len()` could re-issue a generation a still-live slot already owns and
// silently alias two distinct objects onto one key. The GC-walk prune (fix item
// 2) removes dead slots to bound growth AND drops every overlay table keyed by
// the dead key (LinkedList, LinkedHashMap/Set, TreeMap array+fast, TreeSet,
// CSLM comparator, ...), so a generation it frees carries no stale state.
//
// The `gc_relocation_harness` integration test's mock models a move with
// `MockCtx::relocate_object`, which hands the pointer map to the post-GC remap
// exactly as a real collection's epilogue does.
struct ObjKeyEntry {
    /// The object's current raw pointer, used to resolve the slot by
    /// exact-pointer match. Advanced to the new address by the post-GC remap
    /// when the object relocates, and by nothing else.
    last_ptr: usize,
    /// Per-(hash, object) disambiguator packed into the key's low 32 bits.
    /// Handed out monotonically (see `next_generation`) so a pruned slot never
    /// causes a later object to be issued a generation a live slot still owns.
    generation: u32,
    /// GC-invariant identity marker: the registering object's class id (a
    /// moving GC copies the class word with the header). No longer consulted
    /// by `widened_obj_key` (gc-common w26-a); `owner_key_class_matches` reads
    /// it to tell a live collection from an unrelated object that recycled its
    /// block. Stored as the raw `ClassId` numeric value so the registry stays a
    /// plain `Send` struct.
    class_id: u32,
    /// Owning VM/heap, from `NativeContext::vm_identity`.
    ///
    /// This registry is process-global, but heap addresses, identity hashes and
    /// class ids are only unique *within* one VM — and Rust tests routinely
    /// stand up several independent `Vm`s in one process. Without this field
    /// two VMs' collections at one address (two heaps mapped over one
    /// reservation) would resolve to one slot, and the newcomer would read the
    /// other VM's overlay entries — including raw `ObjectRef`s pointing into a
    /// foreign heap — and the first `set_field` through one of them faults.
    ///
    /// `vm_identity`'s own contract states the rule ("native side caches that
    /// store heap `ObjectRef`s must scope entries to this value"); some twenty
    /// call sites across `native-builtins` already do. This registry did not.
    vm: usize,
}

// PERF (registry-shard): the identity-hash registry was a SINGLE global
// `Mutex<HashMap>` taken on EVERY `widened_obj_key` call (every TreeMap /
// TreeSet / LinkedHashMap / LinkedList side-table op, on every thread). Under
// multi-threaded collection churn that one lock serialized all those ops and
// became the dominant contention point. Shard it into `OBJ_KEY_SHARDS`
// independent stripes, each its own `Mutex<HashMap>`, selected by the identity
// hash. Because the shard is chosen deterministically from `hash` and every
// per-hash bucket lives entirely inside one shard, the per-hash `Vec` and the
// generation-disambiguation logic are bit-for-bit identical to the single-map
// version — only the lock granularity changes. Contention drops ~N-fold since
// threads touching different hashes now take different locks. The GC walks
// (relocate / prune) iterate every shard, which is equivalent to iterating the
// former single map. 64 is a power of two so the shard index is a cheap mask.
const OBJ_KEY_SHARDS: usize = 64;

type ObjKeyShard = Mutex<StdHashMap<u32, Vec<ObjKeyEntry>>>;

fn obj_key_shards() -> &'static [ObjKeyShard; OBJ_KEY_SHARDS] {
    static REG: std::sync::OnceLock<[ObjKeyShard; OBJ_KEY_SHARDS]> = std::sync::OnceLock::new();
    REG.get_or_init(|| std::array::from_fn(|_| Mutex::new(StdHashMap::new())))
}

/// Reverse index for the collection-overlay GC edge: a collection object's
/// current address maps to the stable side-table key(s) that hold state for it.
///
/// The GC must be able to answer "this collection has just been marked; which
/// Rust-side references does it own?" without linearly scanning every overlay
/// table for every marked heap object. `widened_obj_key` maintains this index
/// whenever an overlay is touched, and the post-move/prune paths keep it in
/// lockstep with `obj_key_registry`.
fn overlay_owner_keys() -> &'static Mutex<StdHashMap<usize, Vec<usize>>> {
    static INDEX: std::sync::OnceLock<Mutex<StdHashMap<usize, Vec<usize>>>> =
        std::sync::OnceLock::new();
    INDEX.get_or_init(|| Mutex::new(StdHashMap::new()))
}

/// Words in [`OVERLAY_OWNER_CLASS_BITS`] — 65,536 class ids at 8 KB, which is
/// more classes than any run this VM has loaded.
const OVERLAY_CLASS_BITMAP_WORDS: usize = 1024;

/// Class ids that have EVER owned a collection overlay, as a bitmap.
///
/// # The question this answers, and why a CLASS gate is the right shape
///
/// `gc_overlay_roots_for_collection` is called **once per marked object** — from
/// `ZgcRealHeap::visit_refs` and from `collect_garbage`'s serial loop — and asks
/// "does this object own overlay state?". It answered it by taking a
/// `std::sync::Mutex` and hashing the address into `overlay_owner_keys`. Measured
/// with `perf record` on 2026-08-17: that call plus its caller
/// `external_roots_for_owner` were **20–29% of samples on both the serial and the
/// one-worker parallel mark**, i.e. the largest single cost in the marker.
///
/// Two cheaper gates were considered and rejected on the evidence:
///
/// * **"is the index empty?"** — built, and measured **inert**. Every
///   native-backed collection operation registers an owner through
///   `widened_obj_key`, and the JDK bootstrap performs enough of them that the
///   index is non-empty from startup. "No native collections" is not a state a
///   real run is ever in.
/// * **a Bloom filter over owner ADDRESSES** — would saturate. Membership is per
///   *instance*, so a long-running app sets every bit and the filter degrades to
///   "always maybe"; and addresses are recycled, so removal cannot clear bits
///   without risking a false negative.
///
/// A **class** gate has neither problem. The set of classes that can own an
/// overlay is a handful (the native-backed `java.util` collections) and does not
/// grow with instance count, so it cannot saturate; and class ids are stable, so
/// address reuse is irrelevant. On an object graph of ordinary classes — which is
/// what the overwhelming majority of marked objects are, even in a Spring app —
/// every query is two relaxed loads and no lock.
///
/// # Why it cannot produce a false negative
///
/// Bits are only ever SET, never cleared, and the bit is set **before** the owner
/// is inserted into `overlay_owner_keys` (see `register_overlay_owner_key`). So a
/// reader that does not observe the bit ran before the insert it would have been
/// looking for, and a reader that does observe it takes the lock exactly as
/// before. Dropping an overlay edge here would be a use-after-free on the
/// contents of a live collection, so the direction of error is the whole design:
/// too permissive costs a lock, too strict corrupts the heap.
static OVERLAY_OWNER_CLASS_BITS: [std::sync::atomic::AtomicU64; OVERLAY_CLASS_BITMAP_WORDS] =
    [const { std::sync::atomic::AtomicU64::new(0) }; OVERLAY_CLASS_BITMAP_WORDS];

/// Disables the class gate wholesale: an owner was registered whose class is
/// unknown, or whose class id is outside the bitmap.
///
/// Fail-safe by construction — with no class to test, the only sound answer is
/// "maybe", which is what the mutex path already gives.
static OVERLAY_OWNER_CLASS_UNKNOWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Per-object overlay lookups the class gate answered with no lock.
static OVERLAY_CLASS_GATE_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Per-object overlay lookups that fell through the gate to the mutex.
static OVERLAY_CLASS_GATE_MISSES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Record that `owner_class` owns overlay state. **Call before publishing the
/// owner** — see [`OVERLAY_OWNER_CLASS_BITS`] for why the order is the proof.
#[inline]
fn note_overlay_owner_class(owner_class: Option<u32>) {
    let Some(cid) = owner_class else {
        // No class to gate on. See `OVERLAY_OWNER_CLASS_UNKNOWN`.
        OVERLAY_OWNER_CLASS_UNKNOWN.store(true, std::sync::atomic::Ordering::Release);
        return;
    };
    let idx = (cid as usize) / 64;
    if idx >= OVERLAY_CLASS_BITMAP_WORDS {
        OVERLAY_OWNER_CLASS_UNKNOWN.store(true, std::sync::atomic::Ordering::Release);
        return;
    }
    // PERF (gc-common w26-a): load before the read-modify-write. This runs on
    // EVERY `widened_obj_key` call -- every TreeMap / TreeSet / LinkedHashMap /
    // LinkedList overlay op on every thread -- and the handful of overlay
    // classes share a few bitmap words, so an unconditional `fetch_or` made
    // every such op take one contended cache line exclusive. Bits are never
    // cleared, so once the bit reads set it stays set; only the first owner of
    // a class pays the RMW. The ordering proof in `OVERLAY_OWNER_CLASS_BITS`
    // is unchanged: the bit is set (by this call or an earlier one) before the
    // insert that follows.
    let bit = 1u64 << ((cid as usize) % 64);
    if OVERLAY_OWNER_CLASS_BITS[idx].load(std::sync::atomic::Ordering::Acquire) & bit != 0 {
        return;
    }
    OVERLAY_OWNER_CLASS_BITS[idx].fetch_or(bit, std::sync::atomic::Ordering::Release);
}

/// Could an object of `class_id` own overlay state? `false` is definitive.
#[inline]
fn overlay_owner_class_possible(class_id: u32) -> bool {
    if OVERLAY_OWNER_CLASS_UNKNOWN.load(std::sync::atomic::Ordering::Acquire) {
        return true;
    }
    let idx = (class_id as usize) / 64;
    if idx >= OVERLAY_CLASS_BITMAP_WORDS {
        return true;
    }
    OVERLAY_OWNER_CLASS_BITS[idx].load(std::sync::atomic::Ordering::Acquire)
        & (1u64 << ((class_id as usize) % 64))
        != 0
}

/// `(gate_hits, gate_misses)` — per-object overlay lookups answered without the
/// mutex, and those that fell through to it.
///
/// The engagement pair. `gate_hits == 0` on a run with a live heap means the gate
/// is on and inert, and the *result* is identical either way (an empty `Vec`), so
/// nothing but this can tell the two apart. Reported at shutdown on
/// `[GC] zgc-overlay-gate:`.
pub(crate) fn overlay_class_gate_stats() -> (u64, u64) {
    (
        OVERLAY_CLASS_GATE_HITS.load(std::sync::atomic::Ordering::Relaxed),
        OVERLAY_CLASS_GATE_MISSES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Is the class gate disabled? See [`OVERLAY_OWNER_CLASS_UNKNOWN`].
pub fn overlay_class_gate_disabled() -> bool {
    OVERLAY_OWNER_CLASS_UNKNOWN.load(std::sync::atomic::Ordering::Acquire)
}

#[inline]
fn register_overlay_owner_key(owner_addr: usize, key: usize, owner_class: Option<u32>) -> usize {
    // THE BIT BEFORE THE INSERT. A reader that misses the bit therefore ran
    // before the insert, so there was nothing for it to find -- see
    // `OVERLAY_OWNER_CLASS_BITS`.
    note_overlay_owner_class(owner_class);
    let mut index = overlay_owner_keys()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let keys = index.entry(owner_addr).or_default();
    if !keys.contains(&key) {
        keys.push(key);
    }
    key
}

/// Remove one packed overlay key from its recorded collection owner.
///
/// Callers already know the owner address from the object-key registry.  Using
/// it directly keeps cleanup proportional to that collection's own keys;
/// scanning every live owner for every reclaimed key turns a large minor GC
/// into quadratic work under short-lived collection churn.
fn remove_overlay_owner_key(owner_addr: usize, key: usize) {
    let mut index = overlay_owner_keys()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let remove_owner = index
        .get_mut(&owner_addr)
        .map(|keys| {
            keys.retain(|registered| *registered != key);
            keys.is_empty()
        })
        .unwrap_or(false);
    if remove_owner {
        index.remove(&owner_addr);
    }
}

/// Whether the reverse owner index still records `key` as belonging to the
/// collection object at `owner_addr`.
///
/// This is the "is the previous tenant of this address really gone?" question.
/// The index is maintained in lockstep with the object-key registry across a
/// collection: `gc_update_collection_overlay_refs` RE-KEYS a relocated
/// collection's entry from its old address to its new one, and
/// the prune (`drop_overlay_entries_for_dead_keys`) drops it outright when the collection dies.
/// So for an address that a fresh object has just been allocated over:
///   * still recorded here -> the previous tenant died and has not been pruned
///     yet, and its overlay state is genuinely garbage;
///   * no longer recorded  -> the previous tenant MOVED and is still alive, and
///     its overlay state is its only storage.
/// See `native_map_init`, the one caller, for why that distinction matters.
fn overlay_owner_still_at(owner_addr: usize, key: usize) -> bool {
    overlay_owner_keys()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&owner_addr)
        .is_some_and(|keys| keys.contains(&key))
}


/// `CRATONVM_CF_DELEGATING_YIELD` — default ON. `=0` restores the
/// `CompletableFuture` / `CompletionStage` dependent-stage native registrations
/// over a real JDK, so the change can be A/B'd on one binary rather than
/// against a separately built branch. See the note in
/// `register_concurrent_completeness_natives`.
pub(crate) fn cf_delegating_yield_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(
        || match cratonvm_types::flags::runtime_var("CRATONVM_CF_DELEGATING_YIELD") {
            Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
            Err(_) => true,
        },
    )
}

/// The declared descriptor of the instance field at absolute heap slot
/// `slot` of `class_id`, or `None` when the class carries no field
/// metadata for it (a fabricated synthetic layout, an unresolvable
/// class, or a slot past the declared fields).
///
/// This exists for one reason: the layout probes in
/// [`collect_collection_elements`] guess a collection's shape from the
/// runtime `Value` of a slot, and a `boolean` and an `int` are the same
/// `Value::Int` here. A class laid out `(Object[], boolean)` is
/// therefore indistinguishable from `(Object[], int size)` — and reads
/// as a one-element collection whenever the boolean is `true`. The
/// declared descriptor is the only thing that separates them.
///
/// `declared_fields` allocates a `Vec<FieldMetadata>` of owned `String`s,
/// and the probe it guards runs on every `addAll` / copy constructor in
/// the process, so the answer is memoised per `(class, slot)`. Classes
/// are stable for the life of a `ClassId`, so the memo never needs
/// invalidating.
pub(crate) fn slot_declared_descriptor(
    ctx: &dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    slot: usize,
) -> Option<String> {
    use std::cell::RefCell;
    thread_local! {
        static MEMO: RefCell<rustc_hash::FxHashMap<(cratonvm_types::ClassId, usize), Option<String>>> =
            RefCell::new(rustc_hash::FxHashMap::default());
    }
    if let Some(hit) = MEMO.with(|m| m.borrow().get(&(class_id, slot)).cloned()) {
        return hit;
    }
    let found = ctx
        .declared_fields(class_id)
        .into_iter()
        .find(|f| !f.is_static && f.slot_index == slot)
        .map(|f| f.descriptor);
    MEMO.with(|m| {
        m.borrow_mut().insert((class_id, slot), found.clone());
    });
    found
}

#[cfg(test)]
mod overlay_owner_liveness_tests {
    use super::*;

    /// **The class gate answers an ordinary class with no lock, and never gates
    /// away a class that DOES own overlay state.**
    ///
    /// # Why the counter is the assertion and the result cannot be
    ///
    /// `gc_overlay_roots_for_collection` returns an empty `Vec` for a
    /// non-owning object either way — through the gate or through the mutex — so
    /// no assertion on the return value can distinguish them. That is exactly the
    /// shape of a change that is on and inert, which the *previous* attempt at
    /// this optimisation turned out to be. The engagement counters are the only
    /// instrument.
    ///
    /// # The half that matters is the second one
    ///
    /// Too permissive costs a lock. Too strict drops an overlay edge, which frees
    /// the contents of a live collection. So the test registers an owner under a
    /// class and then asserts that queries for that class do **not** take the
    /// fast path.
    #[test]
    fn the_class_gate_skips_the_mutex_for_a_class_that_owns_nothing() {
        // Relative to a baseline: this bitmap is process-global and cargo runs
        // tests in parallel threads, so an absolute count would be a flake.
        let never_owns: u32 = 51_001;
        assert!(
            !overlay_class_gate_disabled(),
            "no test in this binary may register an owner with an unknown class -- \
             that trips the process-global fail-safe and makes this assertion \
             depend on execution order. See TEST_OWNER_CLASS"
        );
        assert!(
            !overlay_owner_class_possible(never_owns),
            "a class that has never owned an overlay must be gated out"
        );
        let (h0, m0) = overlay_class_gate_stats();
        assert!(gc_overlay_roots_for_collection(0x7000_0000, Some(never_owns)).is_empty());
        let (h1, m1) = overlay_class_gate_stats();
        assert_eq!(h1, h0 + 1, "the gate must have answered it");
        assert_eq!(m1, m0, "and it must not have fallen through to the mutex");

        // Now the direction that would corrupt the heap if it were wrong.
        let owns: u32 = 51_002;
        let key = register_overlay_owner_key(0x7000_1000, 0xFEED, Some(owns));
        assert!(
            overlay_owner_class_possible(owns),
            "registering an owner must open the gate for its class -- the bit is \
             set BEFORE the map insert, so a reader can never miss an owner that \
             is already published"
        );
        let (h2, m2) = overlay_class_gate_stats();
        let _ = gc_overlay_roots_for_collection(0x7000_1000, Some(owns));
        let (h3, m3) = overlay_class_gate_stats();
        assert_eq!(
            h3, h2,
            "a class that DOES own overlay state must never be gated out -- that \
             would free the contents of a live collection"
        );
        assert_eq!(m3, m2 + 1, "it must fall through to the mutex");

        // And `None` cannot be gated: there is no class to test.
        let (h4, _) = overlay_class_gate_stats();
        let _ = gc_overlay_roots_for_collection(0x7000_2000, None);
        assert_eq!(
            overlay_class_gate_stats().0,
            h4,
            "a lookup with no class id must not take the fast path"
        );

        remove_overlay_owner_key(0x7000_1000, key);
    }

    /// A class id these tests register owners under.
    ///
    /// Deliberately a REAL id and not `None`: registering with no class trips
    /// `OVERLAY_OWNER_CLASS_UNKNOWN`, which is process-global and would disable
    /// the class gate for every other test in this binary -- making the gate's own
    /// engagement test flaky depending on execution order. Fail-safe is the right
    /// production behaviour and the wrong test fixture.
    const TEST_OWNER_CLASS: u32 = 4242;

    fn object(addr: usize) -> ObjectRef {
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// A collection that DIED leaves its owner-index entry at the address it
    /// died at, so a fresh object allocated over that address may evict its
    /// overlay state. This is the case `native_map_init`'s purge exists for.
    #[test]
    fn dead_tenant_is_still_recorded_at_its_address() {
        let addr = 0x5EAD_0100_usize;
        let key = 0x5EAD_0101_usize;
        register_overlay_owner_key(addr, key, Some(TEST_OWNER_CLASS));
        assert!(
            overlay_owner_still_at(addr, key),
            "a tenant that has not been pruned must still be recorded at its address"
        );
    }

    /// A collection that MOVED is re-keyed to its new address by the post-GC
    /// remap, so its old address no longer names it. Evicting on a raw-pointer
    /// match alone would empty this still-live collection; the guard must
    /// answer `false` for the old address and `true` for the new one.
    #[test]
    fn relocated_tenant_is_no_longer_recorded_at_its_old_address() {
        let old_addr = 0x5EAD_0200_usize;
        let new_addr = 0x5EAD_0300_usize;
        let key = 0x5EAD_0201_usize;
        register_overlay_owner_key(old_addr, key, Some(TEST_OWNER_CLASS));
        assert!(overlay_owner_still_at(old_addr, key));

        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(old_addr, new_addr);
        gc_update_collection_overlay_refs(&pointer_map);

        assert!(
            !overlay_owner_still_at(old_addr, key),
            "a relocated collection must not still be recorded at its pre-move address, \
             or HashMap.<init> on a fresh object at that address would evict its live overlay"
        );
        assert!(
            overlay_owner_still_at(new_addr, key),
            "the relocated collection must be recorded at its post-move address"
        );
        // Leave the global index as we found it so sibling tests in this
        // process see no residue.
        clear_overlay_entries_for_key(key, new_addr);
    }

    /// End-to-end on the overlay itself: an entry registered for a relocated
    /// owner survives the address recycle, and one registered for a dead owner
    /// is still evictable.
    #[test]
    fn relocated_owners_overlay_entry_survives_an_address_recycle() {
        let old_addr = 0x5EAD_0400_usize;
        let new_addr = 0x5EAD_0500_usize;
        let key = 0x5EAD_0401_usize;
        register_overlay_owner_key(old_addr, key, Some(TEST_OWNER_CLASS));
        hm_int_fast_shard_for(key)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_default()
            .entries
            .insert(1, (object(0x1000), Value::Int(7)));

        let mut pointer_map = cratonvm_types::PointerMap::default();
        pointer_map.insert(old_addr, new_addr);
        gc_update_collection_overlay_refs(&pointer_map);

        // What `native_map_init` now asks before evicting.
        assert!(!overlay_owner_still_at(old_addr, key));
        assert_eq!(
            hm_int_fast_shard_for(key)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
                .map(|state| state.entries.len()),
            Some(1),
            "the live, relocated map's only storage must still hold its entry"
        );
        clear_overlay_entries_for_key(key, new_addr);
    }

    /// A `pointer_map` that contains both `A -> B` and `B -> C` must land A's
    /// keys at B and B's at C — not sweep both to C.
    ///
    /// Old-gen sliding compaction routinely produces exactly that shape: it
    /// hands one surviving object the address another surviving object just
    /// vacated, so a single map legitimately holds a value that is also a key.
    /// (The moving young phase alone cannot — its keys are from-space and its
    /// values are to-space or old gen — which is why this only ever bites on a
    /// compacting cycle.)
    ///
    /// The re-key used to relocate entries in place, one move at a time. Apply
    /// `A -> B` first and A's keys sit at B; the later `B -> C` then carries
    /// them onward with B's own, leaving the collection that really is at B
    /// with no entry at its own address. `gc_overlay_roots_for_collection(B)`
    /// answers "owns nothing", the non-moving young marker and `old_gen_gc`'s
    /// mark BFS both skip its backing array, and it is freed while live.
    /// Whether it fired depended on `HashMap` iteration order — applying the
    /// moves in reverse happens to be harmless — so this uses a CHAIN of eight
    /// hops rather than a single pair. In-place relocation survives that only
    /// if every one of the eight lands in exactly reverse order, which the
    /// randomly-seeded `HashMap` iteration will not do. A two-address version
    /// of this test would pass half the time on the broken code.
    #[test]
    fn a_chained_pointer_map_does_not_sweep_one_owners_keys_onto_another() {
        // A private address range, so the process-global index cannot make
        // this pass or fail for a sibling test's reason.
        const HOPS: usize = 8;
        let addr = |i: usize| 0x5EAD_0A00_usize + i * 0x100;
        let key = |i: usize| 0x5EAD_0A01_usize + i * 0x100;

        // Owner i sits at addr(i) and moves to addr(i+1) — the address owner
        // i+1 is simultaneously vacating. Old-gen sliding compaction produces
        // exactly this: every live object slides down onto the space its
        // neighbour just left. (The moving young phase alone cannot — its keys
        // are from-space and its values are to-space or old gen — which is why
        // this only ever bites on a compacting cycle.)
        let mut pointer_map = cratonvm_types::PointerMap::default();
        for i in 0..HOPS {
            register_overlay_owner_key(addr(i), key(i), Some(TEST_OWNER_CLASS));
            pointer_map.insert(addr(i), addr(i + 1));
        }

        gc_update_collection_overlay_refs(&pointer_map);

        for i in 0..HOPS {
            assert!(
                overlay_owner_still_at(addr(i + 1), key(i)),
                "owner {i} moved to 0x{:x} but its key is not recorded there — \
                 a later hop in the same pointer_map carried it onward, so \
                 every per-owner root walk now reports this collection as \
                 owning nothing and frees its backing array while it is live",
                addr(i + 1)
            );
            assert!(
                !overlay_owner_still_at(addr(i), key(i)),
                "owner {i}'s pre-move address must no longer name it"
            );
        }
        // The tail address gained nothing but the last owner: keys must not
        // have piled up at the end of the chain.
        for i in 0..HOPS.saturating_sub(1) {
            assert!(
                !overlay_owner_still_at(addr(HOPS), key(i)),
                "owner {i}'s key was swept to the end of the chain"
            );
        }

        for i in 0..HOPS {
            remove_overlay_owner_key(addr(i + 1), key(i));
        }
    }
}

/// Select the registry shard for an identity hash. Mixes with the 64-bit
/// Fibonacci/golden-ratio constant first because identity hashes can be
/// low-entropy / sequentially assigned, which would otherwise cluster nearby
/// objects onto a handful of shards; the masked high bits of the product are
/// well distributed. Deterministic in `hash`, so a given object's bucket is
/// always in the same shard (preserves the per-hash generation contract).
#[inline]
fn obj_key_shard_for(hash: u32) -> &'static ObjKeyShard {
    let mixed = (hash as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    &obj_key_shards()[((mixed >> 58) as usize) & (OBJ_KEY_SHARDS - 1)]
}

/// Drop every overlay / side-table entry keyed by the packed `widened_obj_key`
/// `key`, and its owner-index row at `owner_addr`, leaving no state a later
/// object could read through that key.
///
/// Test-only since gc-common w26-a. Its one production caller was
/// `widened_obj_key`'s "lone occupant of another class" step, which cleared a
/// slot's rows when a different-class object presented the slot's identity
/// hash -- including a LIVE collider's rows. That step is gone (see
/// `widened_obj_key`). This used to be a second, hand-maintained list of the
/// tables keyed by `widened_obj_key`, and it had already drifted: it missed
/// `tm_force_array_set`, which the prune's list covers. It now forwards to the
/// prune's list, so there is one.
#[cfg(test)]
fn clear_overlay_entries_for_key(key: usize, owner_addr: usize) {
    drop_overlay_entries_for_dead_keys(vec![(key, owner_addr)]);
}

/// Next never-recycled generation for a hash bucket: one past the maximum
/// generation currently present. Unlike `slots.len()`, this does not hand out a
/// value that a still-live (or just-removed) slot already owns once any slot has
/// been replaced or pruned from the bucket — which would silently alias two
/// distinct objects onto the same packed key. The `u32` space (4 billion
/// generations *per identity hash*) cannot realistically be exhausted.
fn next_generation(slots: &[ObjKeyEntry]) -> u32 {
    slots
        .iter()
        .map(|s| s.generation)
        .max()
        .map_or(0, |m| m.wrapping_add(1))
}

/// GC-stable, collision-resistant side-table key. Replaces the former
/// `identity_hash::obj_key` truncation at every overlay key site. Returns a
/// full-width `usize` packing the 32-bit identity hash with a per-object
/// generation (see the module comment above for the disambiguation contract).
fn widened_obj_key(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    widened_obj_key_with(ctx, this, overlay_owner_once_per_slot())
}

/// `CRATONVM_OVERLAY_OWNER_ONCE_PER_SLOT` (default ON since gcg g6/xt, `=0` kills; gcg g2/k,
/// `common-w26a-proposal-register-overlay-owner-once-per-slot`): register a
/// collection's overlay owner row only when [`widened_obj_key`] MINTS its slot,
/// and skip the process-global owner-index mutex on a steady-state hit.
///
/// Sound while the owner index and the slot registry move in lockstep: a row
/// `(addr, key)` is created only with its slot, removed only with it
/// (`drop_overlay_entries_for_dead_keys`, from the prune and the VM teardown),
/// and moved only by `gc_update_collection_overlay_refs` with the same pointer
/// map as the slot's `last_ptr`. A debug build checks that on every skipped
/// registration (`overlay_owner_still_at`), so a future path that breaks the
/// lockstep fails the test suite rather than freeing a live collection's
/// backing. Read once per process.
fn overlay_owner_once_per_slot() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_OVERLAY_OWNER_ONCE_PER_SLOT")
    })
}

/// [`widened_obj_key`] with the once-per-slot owner registration decided by
/// the caller (`once_per_slot`), so a unit test can drive both arms.
fn widened_obj_key_with(ctx: &dyn NativeContext, this: ObjectRef, once_per_slot: bool) -> usize {
    let hash = ctx.identity_hash_code(this) as u32;
    let ptr = this.as_ptr() as usize;
    let class_id = ctx.class_id_of_object(this).as_u32();
    // Every comparison below is scoped to the calling VM: pointers, identity
    // hashes and class ids are only unique within one heap. See `ObjKeyEntry::vm`.
    let vm = ctx.vm_identity();

    let (key, minted) = {
        // PERF (registry-shard): lock only this hash's shard, not a
        // process-global mutex, so concurrent side-table ops on different
        // hashes don't serialize. Poison-recovered like every other table lock
        // in this file: the plain `unwrap()` this used turned one panic under
        // the shard lock into a panic on every later overlay op whose hash
        // maps to that shard.
        let mut reg = obj_key_shard_for(hash)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let slots = reg.entry(hash).or_default();

        // 1. `(calling VM, current address)` match: the same object at the
        //    same address (steady state). A GC-relocated object is resolved
        //    here too: every moving collection runs the stop-the-world epilogue
        //    (`run_collection_pause` -> `update_all_roots` ->
        //    `native_roots::remap_all_roots`, source "collection-overlays" ->
        //    `gc_update_collection_overlay_refs_for_vm`), which advances every
        //    relocated slot's `last_ptr` to the new address before any mutator
        //    resumes and so before the object is next observed here.
        let found = slots
            .iter()
            .find(|s| s.last_ptr == ptr && s.vm == vm)
            .map(|s| s.generation);
        if let Some(generation) = found {
            (pack_obj_key(hash, generation), false)
        } else {
            // 2. Any other address is a different object, or a caller holding a
            //    stale copy of a moved object's address (a bug the stale-handle
            //    and stale-receiver audits gate): mint a fresh, never-recycled
            //    generation (NOT `slots.len()`, which could re-issue a
            //    generation a live or just-pruned slot still owns).
            //
            //    gc-common w26-a: there used to be a "lone occupant" step here
            //    that, when this VM had exactly one slot for `hash` at another
            //    address, took the caller to be that slot's object MOVED: a
            //    same-class caller was REBOUND onto the slot (its key and
            //    overlay), a different-class one re-keyed the slot and cleared
            //    its overlay. A second LIVE collection sharing the identity hash
            //    (the per-heap counter wraps after ~2^32 identity-hash
            //    requests) therefore read and wrote the first one's contents,
            //    and a different-class one wiped them. The epilogue remap above
            //    makes the step unnecessary for moves, as w25-a established for
            //    the `native-builtins` lock-key registry
            //    (`common-w25a-native-collections-widened-obj-key-merges-live-same-class-colliders`).
            //    A dead predecessor's slot is dropped, with every row keyed by
            //    it, by `gc_prune_dead_collection_overlays_for_vm`; until then a
            //    newcomer at another address holds its own generation and never
            //    reaches those rows.
            //
            // DIAGNOSTIC (`CRATONVM_DBG_OBJKEY=1`): reaching here for an object
            // that is NOT new hands it a fresh, EMPTY overlay -- a populated
            // collection silently reads back as size 0, with no dangling
            // pointer and nothing for the heap/overlay verifiers to find.
            // Distinguishing that from a premature free is the first fork in
            // diagnosing any "collection lost its contents" report, so log the
            // inputs that decide it.
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_OBJKEY").is_some()
                && !slots.is_empty()
            {
                eprintln!(
                    "[objkey] FRESH generation for hash=0x{hash:x} ptr=0x{ptr:x} class_id={class_id} \
                     vm=0x{vm:x} -- {} existing slot(s) for this hash, none at this address \
                     (a live collider, an unpruned dead predecessor, or a stale caller address)",
                    slots.len(),
                );
            }
            let generation = next_generation(slots);
            note_obj_key_slot_vm(vm);
            slots.push(ObjKeyEntry {
                last_ptr: ptr,
                generation,
                class_id,
                vm,
            });
            (pack_obj_key(hash, generation), true)
        }
    };
    // gcg g2/k: a steady-state hit's owner row was written when its slot was
    // minted and has been kept in step since (`overlay_owner_once_per_slot`).
    if once_per_slot && !minted {
        debug_assert!(
            overlay_owner_still_at(ptr, key),
            "overlay owner index lost the row of a live slot (addr {ptr:#x}, key {key:#x}): \
             the once-per-slot registration's lockstep premise is broken"
        );
        return key;
    }
    // Outside the shard lock: the owner-index insert takes a second,
    // process-global mutex, and holding the shard across it only lengthened
    // the shard's hold time. The caller creates the overlay rows for `key`
    // after this returns, so the owner is published before any row exists for
    // a collector to look for; and a collection cannot start while this native
    // runs (it is not at a safepoint), so no remap or prune sits between the
    // two locks.
    register_overlay_owner_key(ptr, key, Some(class_id))
}

/// Pack a 32-bit identity hash and a 32-bit generation into the full-width
/// `usize` side-table key.
#[inline]
fn pack_obj_key(hash: u32, generation: u32) -> usize {
    (((hash as u64) << 32) | (generation as u64)) as usize
}

/// Does the object-key registry record `actual_class_id` for `key`'s slot?
///
/// The owner index (`overlay_owner_keys`) is keyed by ADDRESS and therefore
/// cannot tell a live collection from an unrelated object that recycled its
/// block. The registry slot behind the packed key does carry the registering
/// collection's `class_id`, which is GC-invariant, so comparing it against the
/// class actually at the address is a sound identity check (the one
/// `widened_obj_key` used for recycled identity hashes until gc-common w26-a
/// made it match by address alone).
///
/// Unknown key (no such slot) answers `true`: absence of evidence is not
/// evidence of a mismatch, and dropping a root on it would be the unsafe
/// direction. Over-retaining one cycle is the safe one.
/// OFF BY DEFAULT — measured harmful. Opt in with `CRATONVM_OWNER_CLASS_FILTER=1`.
///
/// The defect this guards against is real and unit-proven (see
/// `marker_owner_lookup_survives_address_recycling_by_a_non_collection`): the
/// marker's owner→overlay lookup has no identity check, so an ordinary object
/// that recycles a dead collection's block inherits its references. But
/// ENFORCING the check costs more than it saves on the workload that motivated
/// it.
///
/// One-binary A/B on `DefaultCatalogAndSchemaTest`, arms alternating, only this
/// variable differing (`probes/owner-filter-flag-ab-20260801.sh`):
///
/// | arm | rc  | stale receivers |
/// |-----|-----|-----------------|
/// | off | 139 | 0               |
/// | on  | 139 | 3641            |
/// | off | 127 | 0               |
/// | on  | 139 | 973             |
///
/// Dropping a key discards a LIVE collection's roots whenever the recorded and
/// actual class disagree for any reason OTHER than recycling, and on this
/// workload they disagree often enough to reclaim thousands of live objects.
/// Exempting the zeroed-header case (`class_id == 0`, an already-freed owner)
/// cut it from 5784/637 to 3641/973 but did not close it — so the remaining
/// mismatches are not just corrupted owners.
///
/// It stays a runtime switch, not a second binary, because that is what made
/// the effect attributable at all: a two-binary comparison had shown only
/// `found=132` vs `110` and looked like a clean win.
///
/// Before re-enabling this, find a discriminator that cannot fire on a live
/// collection — the recorded class is not one.
fn owner_class_filter_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_OWNER_CLASS_FILTER").is_some()
    })
}

/// `CRATONVM_DBG_OWNER_FILTER=1`: report every key this check DROPS.
///
/// A drop is only correct when the address was genuinely recycled. If the
/// registry's recorded class can disagree with the marker's header class for
/// any OTHER reason, this filter discards a LIVE collection's roots — the one
/// direction that turns a diagnostic improvement into a use-after-free. The
/// counter distinguishes "rare, and each one an aliased address" from
/// "systematic", which is the difference between a fix and a regression.
fn owner_filter_dbg() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_OWNER_FILTER").is_some())
}

static OWNER_FILTER_DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Total keys dropped by the owner identity check this process.
pub fn owner_class_filter_drop_count() -> u64 {
    OWNER_FILTER_DROPS.load(std::sync::atomic::Ordering::Relaxed)
}

fn owner_key_class_matches(key: usize, actual_class_id: u32, owner_addr: usize) -> bool {
    let hash = (key >> 32) as u32;
    let generation = key as u32;
    let recorded = {
        let shard = obj_key_shard_for(hash)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match shard.get(&hash) {
            None => None,
            Some(slots) => slots
                .iter()
                .find(|s| s.generation == generation)
                .map(|s| (s.class_id, s.last_ptr)),
        }
    };
    match recorded {
        // Unknown key: absence of evidence is not evidence of a mismatch, and
        // dropping a root here would be the unsafe direction. Over-retaining
        // one cycle is the safe one.
        None => true,
        Some((recorded_class, last_ptr)) => {
            if recorded_class == actual_class_id {
                return true;
            }
            // A ZEROED header is not evidence of recycling. It is evidence of
            // corruption somewhere ELSE, and dropping on it makes that
            // corruption strictly worse.
            //
            // `class_id == 0` is both the all-zero header of an already-freed
            // block and the legitimate `ClassId(0)` ad-hoc container shape, so
            // it cannot discriminate. Measured on `DefaultCatalogAndSchemaTest`
            // with `CRATONVM_DBG_OWNER_FILTER=1`: the drops were dominated by
            // `recorded_class=64 actual_class=0 same_addr=true` — the owner's
            // own block had already been freed underneath a live collection.
            // Dropping its overlay refs there removes the last thing keeping
            // that collection's CONTENTS marked, turning ONE premature free
            // into a cascade: 637 and 5784 stale receivers on the two runs that
            // crashed, against 117 with no filter at all.
            //
            // Retaining on an unreadable owner costs one cycle of
            // over-retention — the same safe direction as the unknown-key arm.
            if actual_class_id == 0 || recorded_class == 0 {
                return true;
            }
            let n = OWNER_FILTER_DROPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if owner_filter_dbg() && n < 32 {
                eprintln!(
                    "[owner-filter] DROP key=0x{key:x} owner=0x{owner_addr:x} \
                     recorded_class={recorded_class} actual_class={actual_class_id} \
                     slot_last_ptr=0x{last_ptr:x} same_addr={}",
                    last_ptr == owner_addr,
                );
            }
            false
        }
    }
}

/// Authoritative lazy store for fresh, exact-class HashMaps with primitive
/// Integer keys. Heap HashMap$Node objects are materialized on demand when an
/// operation outside the fast put/get/size surface needs the ordinary table.
#[derive(Clone, Copy)]
struct SparseIntEntry {
    key: ObjectRef,
    value: Value,
    seq: u64,
}

#[derive(Default)]
struct DenseIntEntries {
    // The common dense lookup array stays key/value-only. For monotonically
    // inserted non-negative keys, insertion sequence equals the key itself,
    // so storing an extra u64 in every entry only widens the 10M-key hot set.
    dense: Vec<Option<(ObjectRef, Value)>>,
    sparse: FxHashMap<i32, SparseIntEntry>,
    // Only out-of-order, migrated, or reinserted dense keys need metadata.
    // Sequential generated-ID maps leave this empty.
    dense_seq_overrides: FxHashMap<i32, u64>,
    len: usize,
    // Real JDK `HashMap` never shrinks its table on `remove`, and a node's
    // position within its bucket chain is fixed at first-insertion time
    // (a value-only update via a later `put` of the same key does not move
    // it). `peak_len` mirrors the never-shrinks capacity rule and the sparse
    // entry/exception sequences plus `next_seq` mirror per-key insertion order,
    // so
    // `keys_in_java_hashmap_order` below can reproduce real bucket-iteration
    // order instead of this dense store's raw ascending-key layout. Deriving
    // the common dense sequence from the key avoids both a widened 10M-entry
    // array and a second full hash-table insertion.
    peak_len: usize,
    next_seq: u64,
    // `CRATONVM_COMPAT_OVERLAY_REMAP_ORDER`: the table size, kept as state
    // from the first compute-family insert on (`0` until then, when it is
    // derived from `peak_len`). A compute-family insert grows it on ENTRY and
    // not after, so it can no longer be read off the entry count.
    table_cap: usize,
    // Head inserts made so far; see [`DenseIntEntries::insert_remap`].
    next_head_seq: u64,
}

/// Marks an insertion sequence as a bucket-HEAD insert (a compute-family
/// insert, `tab[i] = newNode(hash, key, v, first)`); the low bits count head
/// inserts. See [`DenseIntEntries::chain_rank`].
const HEAD_SEQ_BIT: u64 = 1 << 63;

impl DenseIntEntries {
    const MAX_DENSE_KEY: usize = 16 * 1024 * 1024;

    fn len(&self) -> usize {
        self.len
    }

    fn note_fresh_insert(&mut self) -> u64 {
        self.len += 1;
        self.peak_len = self.peak_len.max(self.len);
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    /// `put`: [`Self::insert_raw`], then `putVal`'s `if (++size > threshold)
    /// resize()` once the table size is state (`table_cap != 0`).
    fn insert(&mut self, key: i32, value: (ObjectRef, Value)) -> Option<(ObjectRef, Value)> {
        let old = self.insert_raw(key, value);
        if old.is_none() && self.table_cap != 0 && self.len > (self.table_cap * 3) / 4 {
            self.table_cap = (self.table_cap * 2).min(1 << 30);
        }
        old
    }

    /// The table size [`Self::keys_in_java_hashmap_order`] iterates over:
    /// the tracked one, else the `put`-only derivation from `peak_len` over
    /// `start_cap` (`0`: the historical 16).
    fn java_table_cap(&self, start_cap: usize) -> usize {
        if self.table_cap != 0 {
            return self.table_cap;
        }
        let mut cap: usize = if start_cap == 0 {
            16
        } else {
            start_cap.checked_next_power_of_two().unwrap_or(1 << 30)
        };
        while self.peak_len > (cap * 3) / 4 && cap < (1 << 30) {
            cap *= 2;
        }
        cap
    }

    /// `computeIfAbsent` / `compute` / `merge` inserting `key`: the table
    /// grows on ENTRY when `size > threshold` (never after the insert), and a
    /// NEW key is linked at its bucket's HEAD, ahead of every entry the bucket
    /// holds now. A key already present is an ordinary value update, which
    /// moves nothing.
    fn insert_remap(
        &mut self,
        key: i32,
        value: (ObjectRef, Value),
        start_cap: usize,
    ) -> Option<(ObjectRef, Value)> {
        if self.contains_key(&key) {
            return self.insert_raw(key, value);
        }
        let mut cap = self.java_table_cap(start_cap);
        if self.len > (cap * 3) / 4 {
            cap = (cap * 2).min(1 << 30);
        }
        self.table_cap = cap;
        let old = self.insert_raw(key, value);
        let head = HEAD_SEQ_BIT | self.next_head_seq;
        self.next_head_seq += 1;
        self.set_seq(key, head);
        old
    }

    /// Overwrite the insertion sequence of a key this store holds.
    fn set_seq(&mut self, key: i32, seq: u64) {
        if key >= 0 && self.dense.get(key as usize).is_some_and(Option::is_some) {
            self.dense_seq_overrides.insert(key, seq);
        } else if let Some(entry) = self.sparse.get_mut(&key) {
            entry.seq = seq;
        }
    }

    /// Where a key sits in its bucket's chain, smallest first: every head
    /// insert ahead of every tail insert, head inserts newest first (each was
    /// linked ahead of all the bucket then held), tail inserts oldest first.
    /// A table resize splits each chain keeping relative order, so the rank
    /// orders a bucket at any later table size too.
    fn chain_rank(seq: u64) -> (u8, u64) {
        if seq & HEAD_SEQ_BIT != 0 {
            (0, u64::MAX - (seq & !HEAD_SEQ_BIT))
        } else {
            (1, seq)
        }
    }

    fn insert_raw(&mut self, key: i32, value: (ObjectRef, Value)) -> Option<(ObjectRef, Value)> {
        if key >= 0 {
            let index = key as usize;
            // Generated-ID maps append 0, 1, 2, ... . Handle that dominant
            // case before the general dense/sparse migration machinery:
            // `resize`, an occupied-slot probe, and sparse lookup are all
            // redundant when this is exactly the next dense slot and no
            // sparse entry can already own it.
            if index <= Self::MAX_DENSE_KEY && index == self.dense.len() && self.sparse.is_empty() {
                let seq = self.note_fresh_insert();
                self.dense.push(Some(value));
                if seq != key as u64 {
                    self.dense_seq_overrides.insert(key, seq);
                }
                return None;
            }
            if index <= Self::MAX_DENSE_KEY && index <= self.dense.len().saturating_add(1024) {
                if index >= self.dense.len() {
                    self.dense.resize(index + 1, None);
                }
                if let Some(entry) = self.dense[index].as_mut() {
                    let old = *entry;
                    *entry = value;
                    return Some(old);
                }
                // A key can begin sparse when it is far ahead of the dense
                // frontier, then migrate here after intervening inserts grow
                // the vector. Preserve its first-insertion sequence.
                if !self.sparse.is_empty() {
                    if let Some(entry) = self.sparse.remove(&key) {
                        let old = (entry.key, entry.value);
                        self.dense[index] = Some(value);
                        self.record_dense_seq(key, entry.seq);
                        return Some(old);
                    }
                }
                let seq = self.note_fresh_insert();
                self.dense[index] = Some(value);
                self.record_dense_seq(key, seq);
                return None;
            }
        }
        if let Some(entry) = self.sparse.get_mut(&key) {
            let old = (entry.key, entry.value);
            entry.key = value.0;
            entry.value = value.1;
            return Some(old);
        }
        let seq = self.note_fresh_insert();
        self.sparse.insert(
            key,
            SparseIntEntry {
                key: value.0,
                value: value.1,
                seq,
            },
        );
        None
    }

    fn record_dense_seq(&mut self, key: i32, seq: u64) {
        debug_assert!(key >= 0);
        if seq == key as u64 {
            if !self.dense_seq_overrides.is_empty() {
                self.dense_seq_overrides.remove(&key);
            }
        } else {
            self.dense_seq_overrides.insert(key, seq);
        }
    }

    fn get(&self, key: &i32) -> Option<(ObjectRef, Value)> {
        if *key >= 0 {
            if let Some(value) = self.dense.get(*key as usize).and_then(Option::as_ref) {
                return Some(*value);
            }
        }
        self.sparse.get(key).map(|entry| (entry.key, entry.value))
    }

    fn insertion_seq(&self, key: &i32) -> Option<u64> {
        if *key >= 0 {
            let index = *key as usize;
            if self.dense.get(index).is_some_and(Option::is_some) {
                return Some(
                    self.dense_seq_overrides
                        .get(key)
                        .copied()
                        .unwrap_or(*key as u64),
                );
            }
        }
        self.sparse.get(key).map(|entry| entry.seq)
    }

    fn remove(&mut self, key: &i32) -> Option<(ObjectRef, Value)> {
        let old = if *key >= 0 {
            self.dense
                .get_mut(*key as usize)
                .and_then(Option::take)
                .or_else(|| {
                    self.sparse
                        .remove(key)
                        .map(|entry| (entry.key, entry.value))
                })
        } else {
            self.sparse
                .remove(key)
                .map(|entry| (entry.key, entry.value))
        };
        if old.is_some() {
            self.len -= 1;
            self.dense_seq_overrides.remove(key);
        }
        old
    }

    fn contains_key(&self, key: &i32) -> bool {
        self.get(key).is_some()
    }

    fn keys(&self) -> impl Iterator<Item = i32> + '_ {
        self.dense
            .iter()
            .enumerate()
            .filter_map(|(key, value)| value.as_ref().map(|_| key as i32))
            .chain(self.sparse.keys().copied())
    }

    fn values(&self) -> impl Iterator<Item = (ObjectRef, Value)> + '_ {
        self.dense
            .iter()
            .filter_map(|entry| *entry)
            .chain(self.sparse.values().map(|entry| (entry.key, entry.value)))
    }

    fn values_mut(&mut self) -> impl Iterator<Item = (&mut ObjectRef, &mut Value)> {
        self.dense
            .iter_mut()
            .filter_map(Option::as_mut)
            .map(|entry| (&mut entry.0, &mut entry.1))
            .chain(
                self.sparse
                    .values_mut()
                    .map(|entry| (&mut entry.key, &mut entry.value)),
            )
    }

    /// Real-JDK `HashMap` bucket-iteration order for the entries currently
    /// held: grouped by `(capacity - 1) & hash(key)` in ascending bucket
    /// order (capacity derived from `peak_len` via the same doubling-at-.75-
    /// load-factor rule real `HashMap` uses, since real `HashMap` capacity
    /// never shrinks on `remove`), with original insertion order breaking
    /// ties within a bucket -- exactly how a real bucket array of chains
    /// iterates. `keys()`/`values()` above are bucket-oblivious (dense-index
    /// ascending order), which is fine for `get`/`put`/`size` but gives the
    /// WRONG order for anything Java-visible: `HashSet<Integer>`/
    /// `HashMap<Integer, V>` `keySet()`/`values()`/`entrySet()` iteration,
    /// and hence `CollectionUtils.firstElement`/`lastElement`, observe
    /// dense ascending order instead of real hash-bucket order without this.
    ///
    /// `start_cap` is the table the first insert allocated (see
    /// `HmIntFastState::start_cap`); `0` means the historical 16.
    fn keys_in_java_hashmap_order(&self, start_cap: usize) -> Vec<i32> {
        let cap = self.java_table_cap(start_cap);
        let mask = (cap as i32) - 1;
        let mut ordered: Vec<i32> = self.keys().collect();
        ordered.sort_by_key(|&k| {
            // JDK `HashMap.hash(Object)`: `(h = key.hashCode()) ^ (h >>> 16)`;
            // for `Integer`, `hashCode()` is the int value itself.
            let h = k ^ (((k as u32) >> 16) as i32);
            let bucket = mask & h;
            (
                bucket,
                Self::chain_rank(self.insertion_seq(&k).unwrap_or(0)),
            )
        });
        ordered
    }
}

#[cfg(test)]
mod chm_table_size_tests {
    use super::chm_jdk_table_size_for;

    /// Every one of these is a reading of `ConcurrentHashMap.table.length` on
    /// HotSpot JDK 25, not a value derived from the constructor's source. The
    /// source reads like `tableSizeFor(c + (c >>> 1) + 1)`, which would put
    /// `CHM(11)` at 32; the real map holds 16.
    #[test]
    fn matches_the_measured_jdk_table_sizes() {
        // (initial capacity or size, table length) — sized constructor sweep.
        let measured = [
            (1, 2),
            (2, 4),
            (3, 8),
            (4, 8),
            (5, 8),
            (6, 16),
            (11, 16),
            (12, 32),
            (23, 32),
            (24, 64),
            (47, 64),
            (48, 128),
            (95, 128),
            (512, 1024),
        ];
        for (n, want) in measured {
            assert_eq!(
                chm_jdk_table_size_for(n),
                want,
                "table size for {n} entries"
            );
        }
    }

    /// A default map's growth points, read the same way: it doubles as the
    /// size reaches 12, 24, 48, 96, 192.
    #[test]
    fn matches_the_measured_default_map_growth() {
        for (size, want) in [
            (1, 16),
            (11, 16),
            (12, 32),
            (23, 32),
            (24, 64),
            (47, 64),
            (48, 128),
            (95, 128),
            (96, 256),
            (191, 256),
            (192, 512),
        ] {
            assert_eq!(
                chm_jdk_table_size_for(size).max(16),
                want,
                "default map holding {size} entries"
            );
        }
    }

    /// The threshold is STRICT: a table of size P holds up to `0.75P - 1`
    /// entries, and the entry that reaches `0.75P` is what doubles it. An
    /// off-by-one here shifts every key's bucket for a whole size band.
    #[test]
    fn the_load_threshold_is_strict() {
        for bits in 4..20 {
            let cap = 1usize << bits;
            let threshold = cap - (cap >> 2);
            assert_eq!(chm_jdk_table_size_for(threshold - 1), cap);
            assert_eq!(chm_jdk_table_size_for(threshold), cap << 1);
        }
    }
}

#[cfg(test)]
mod dense_int_entries_tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    fn object(address: usize) -> ObjectRef {
        // The entry store treats refs as opaque values; these aligned sentinels
        // are never dereferenced by this unit test.
        unsafe { ObjectRef::from_raw(address as *mut u8) }
    }

    /// r13w6 hashtree: the overlay's iteration order is computed over the
    /// table the map's first insert allocates. Three keys that share a bucket
    /// of a 4-bucket table (`new HashMap<>(4)`) iterate in insertion order
    /// there, and in bucket order over the historical 16.
    #[test]
    fn r13_hashtree_overlay_order_starts_from_the_maps_own_capacity() {
        let mut entries = DenseIntEntries::default();
        for k in [-11, -7, -3] {
            assert_eq!(entries.insert(k, (object(0x1000), Value::Int(k))), None);
        }
        // spread(-11) & 15 = 10, spread(-7) & 15 = 6, spread(-3) & 15 = 2.
        assert_eq!(entries.keys_in_java_hashmap_order(0), vec![-3, -7, -11]);
        assert_eq!(entries.keys_in_java_hashmap_order(16), vec![-3, -7, -11]);
        // ... and all three are bucket 2 of 4.
        assert_eq!(entries.keys_in_java_hashmap_order(4), vec![-11, -7, -3]);
    }

    /// r13w10 compat5 (`CRATONVM_COMPAT_OVERLAY_REMAP_ORDER`): a
    /// compute-family insert links the new key at its bucket's HEAD. HotSpot:
    /// `m.put(1, ..); m.computeIfAbsent(17, ..)` iterates `{17, 1}`, and a
    /// later `put(33)` (same bucket) goes to the tail, a later
    /// `computeIfAbsent(49)` to the head again.
    #[test]
    fn r13_compat5_overlay_remap_insert_heads_its_bucket() {
        let mut entries = DenseIntEntries::default();
        assert_eq!(entries.insert(1, (object(0x1000), Value::Int(1))), None);
        assert_eq!(
            entries.insert_remap(17, (object(0x1008), Value::Int(2)), 16),
            None
        );
        assert_eq!(entries.keys_in_java_hashmap_order(16), vec![17, 1]);
        assert_eq!(entries.insert(33, (object(0x1010), Value::Int(3))), None);
        assert_eq!(
            entries.insert_remap(49, (object(0x1018), Value::Int(4)), 16),
            None
        );
        assert_eq!(entries.keys_in_java_hashmap_order(16), vec![49, 17, 1, 33]);
        // An update through the remap door moves nothing.
        assert_eq!(
            entries.insert_remap(1, (object(0x1020), Value::Int(5)), 16),
            Some((object(0x1000), Value::Int(1)))
        );
        assert_eq!(entries.keys_in_java_hashmap_order(16), vec![49, 17, 1, 33]);
    }

    /// r13w10 compat5: the table grows on a compute-family insert's ENTRY
    /// only. HotSpot: twelve `put(i)` then `merge(17, ..)` keeps 16 buckets
    /// (`size > threshold` is `12 > 12` on entry), so 17 heads bucket 1; the
    /// next `put(18)` takes the size to 14 and doubles the table, whose split
    /// keeps each chain's order.
    #[test]
    fn r13_compat5_overlay_remap_insert_grows_on_entry_only() {
        let mut entries = DenseIntEntries::default();
        for k in 0..12 {
            assert_eq!(entries.insert(k, (object(0x2000), Value::Int(k))), None);
        }
        assert_eq!(
            entries.insert_remap(17, (object(0x3000), Value::Int(17)), 16),
            None
        );
        let mut want: Vec<i32> = vec![0, 17];
        want.extend(1..12);
        assert_eq!(entries.keys_in_java_hashmap_order(16), want);
        assert_eq!(entries.insert(18, (object(0x3008), Value::Int(18))), None);
        let mut want: Vec<i32> = (0..12).collect();
        want.extend([17, 18]);
        assert_eq!(entries.keys_in_java_hashmap_order(16), want);
        // Size 14 is under 32's threshold of 24, so the table stays; 33 heads
        // bucket 1.
        assert_eq!(
            entries.insert_remap(33, (object(0x3010), Value::Int(33)), 16),
            None
        );
        let mut want: Vec<i32> = vec![0, 33, 1];
        want.extend(2..12);
        want.extend([17, 18]);
        assert_eq!(entries.keys_in_java_hashmap_order(16), want);
    }

    #[test]
    fn dense_and_sparse_integer_keys_preserve_map_semantics() {
        let mut entries = DenseIntEntries::default();
        assert_eq!(entries.insert(0, (object(0x1000), Value::Int(10))), None);
        assert_eq!(entries.insert(1024, (object(0x2000), Value::Int(20))), None);
        assert_eq!(entries.insert(-7, (object(0x3000), Value::Int(30))), None);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries.get(&1024).map(|(_, value)| value),
            Some(Value::Int(20))
        );
        assert_eq!(
            entries.insert(1024, (object(0x4000), Value::Int(21))),
            Some((object(0x2000), Value::Int(20)))
        );
        assert_eq!(entries.len(), 3);
        assert!(entries.contains_key(&-7));
        assert_eq!(entries.remove(&-7), Some((object(0x3000), Value::Int(30))));
        assert_eq!(entries.len(), 2);
        let mut keys: Vec<_> = entries.keys().collect();
        keys.sort_unstable();
        assert_eq!(keys, vec![0, 1024]);
    }

    #[test]
    fn insertion_sequence_survives_updates_migration_and_reinsert() {
        let mut entries = DenseIntEntries::default();
        // 2048 starts sparse because it is beyond the initial dense frontier.
        assert_eq!(entries.insert(2048, (object(0x1000), Value::Int(1))), None);
        assert_eq!(entries.insert(0, (object(0x2000), Value::Int(2))), None);
        for key in 1..=1024 {
            assert_eq!(
                entries.insert(key, (object(0x3000 + key as usize * 8), Value::Int(key))),
                None
            );
        }
        let original_seq = entries.insertion_seq(&2048).expect("sparse entry");
        // The now-nearby key migrates into the dense vector and a value update
        // must retain its original position within a bucket.
        assert_eq!(
            entries.insert(2048, (object(0x9000), Value::Int(3))),
            Some((object(0x1000), Value::Int(1)))
        );
        assert_eq!(
            entries.insertion_seq(&2048).expect("migrated entry"),
            original_seq
        );
        assert_eq!(
            entries.insert(2048, (object(0xA000), Value::Int(4))),
            Some((object(0x9000), Value::Int(3)))
        );
        assert_eq!(
            entries.insertion_seq(&2048).expect("updated entry"),
            original_seq
        );

        assert_eq!(entries.remove(&2048), Some((object(0xA000), Value::Int(4))));
        assert_eq!(entries.insert(2048, (object(0xB000), Value::Int(5))), None);
        assert!(
            entries.insertion_seq(&2048).expect("reinserted entry") > original_seq,
            "remove followed by reinsert must create a new bucket-chain position"
        );
    }
}

#[derive(Default)]
struct HmIntFastState {
    entries: DenseIntEntries,
    /// Runtime class of every key currently stored in `entries`; `None` only
    /// before the overlay's first insert.
    ///
    /// CORRECTNESS (cross-wrapper key aliasing). `unbox_wrapper` collapses
    /// `Integer`, `Short`, `Byte`, `Character` AND `Boolean` onto the same
    /// `Value::Int`, so keying this overlay on that `i32` alone made
    /// `Integer.valueOf(65)` and `Character.valueOf('A')` — or `Boolean.TRUE`
    /// and `Integer.valueOf(1)` — the SAME overlay entry. Real `HashMap` keeps
    /// them apart: every JDK wrapper's `equals` starts with an `instanceof` of
    /// its own type, so wrappers of different classes are never equal. The
    /// symptom was a silent wrong answer, not a crash:
    ///
    /// ```java
    /// Map<Object, String> m = new HashMap<>();
    /// m.put(Integer.valueOf(65), "int");
    /// m.put(Character.valueOf('A'), "char");
    /// m.size();                        // JDK 2, overlay 1
    /// m.get(Integer.valueOf(65));      // JDK "int", overlay "char"
    /// ```
    ///
    /// Recording the wrapper class of the first key an overlay ever saw and
    /// refusing any later key of a different class restores the contract: the
    /// mismatching key falls through to `materialize_hm_int_fast` plus the
    /// ordinary node path, which compares through `map_keys_equal` (itself
    /// class-guarded — see there). The check costs one `class_id_of_object`,
    /// a heap-header read with no `class_manager` lock and no `String`
    /// allocation, so the Integer-keyed path this overlay exists for is
    /// unaffected.
    key_class: Option<ClassId>,
    /// The table size the map's first insert would allocate, captured when
    /// this row is created (`map_state`'s capacity: the live table's length,
    /// else the `threshold` a sized constructor parked, else 16). The JDK
    /// iterates `new HashMap<>(4)` over 4 buckets and `new HashMap<>(64)`
    /// over 64, and `materialize_hm_int_fast` re-puts in the order computed
    /// from this, so it must be the same capacity the node path will use.
    /// `0` (the switch off) keeps the historical 16.
    /// `CRATONVM_COMPAT_MAP_LOAD_FACTOR`, round 13 wave 6.
    start_cap: usize,
}

/// True when `key_class` may be stored in / looked up against `state`.
/// A `None` (fresh) overlay accepts any wrapper class and adopts it.
#[inline]
fn hm_int_fast_key_class_ok(state: &HmIntFastState, key_class: ClassId) -> bool {
    match state.key_class {
        Some(existing) => existing == key_class,
        None => true,
    }
}

// PERF (hm-int-fast-shard): the exact-`HashMap` integer overlay used to be a
// SINGLE process-global `Mutex<FxHashMap<..>>`, taken on EVERY overlay
// `put`/`get`/`remove`/`containsKey`/`size`/`clear`/`keySet`/`values`/
// `entrySet` — for all maps, on all threads. Since the overlay is the
// authoritative store for a fresh `HashMap<Integer, ?>`, that one lock
// serialized the single hottest collection path in the VM across every thread
// in the process.
//
// Sharding follows the identity-hash registry precedent immediately above
// (`OBJ_KEY_SHARDS`, `PERF (registry-shard)`), and the monitor table sharded 64
// ways elsewhere this wave. It is semantics-preserving for the same reason:
// the shard is chosen deterministically from the overlay key alone, and an
// entry lives entirely inside one shard, so every keyed operation sees exactly
// the map it saw before — only the lock granularity changes.
//
// ENUMERATION. Three paths walk the whole table rather than one key, and each
// must iterate all shards to stay equivalent to the former single map:
//   * `for_each_overlay_ref` — GC root scan and post-move remap. Missing a
//     shard here would drop a root (use-after-free), so it iterates every
//     shard, poison-recovering each guard exactly as before.
//   * `gc_prune_dead_collection_overlays` — batched removal of dead keys. It
//     now groups the dead keys by shard so the per-GC lock count stays
//     proportional to the shard count, not to the number of dead keys.
//   * `overlay_roots_for_owner` — per-owner root collection; it looks each key
//     up individually, so it simply routes through `hm_int_fast_shard_for`.
const HM_INT_FAST_SHARDS: usize = 64;

type HmIntFastShard =
    Mutex<OverlayRows<HmIntFastState, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>>;

fn hm_int_fast_shards() -> &'static [HmIntFastShard; HM_INT_FAST_SHARDS] {
    static TABLE: std::sync::OnceLock<[HmIntFastShard; HM_INT_FAST_SHARDS]> =
        std::sync::OnceLock::new();
    TABLE.get_or_init(|| std::array::from_fn(|_| Mutex::new(OverlayRows::default())))
}

/// The one shard that may hold overlay key `key`.
///
/// An overlay key is `widened_obj_key`'s packed `(identity_hash << 32) |
/// generation`, so the low bits are a small dense generation and the entropy
/// lives high. Mix then take the top bits, the same shape as
/// `obj_key_shard_for`, so keys do not pile into one shard.
#[inline]
fn hm_int_fast_shard_index(key: usize) -> usize {
    let mixed = (key as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    ((mixed >> 58) as usize) & (HM_INT_FAST_SHARDS - 1)
}

#[inline]
fn hm_int_fast_shard_for(key: usize) -> &'static HmIntFastShard {
    &hm_int_fast_shards()[hm_int_fast_shard_index(key)]
}

thread_local! {
    /// Single-entry memo for [`hm_int_fast_obj_key`]:
    /// `(vm identity, raw pointer, identity hash, overlay key)`.
    ///
    /// gc-common w20-a: scoped to `vm_identity` like every other per-thread
    /// memo in this file. Without it, a thread that ran two VMs in turn (a
    /// test binary, an embedder) and met a map at the same address with the
    /// same identity hash in the second -- a fresh heap mapped where the old
    /// one was, and a per-VM hash counter that restarts at the same seed --
    /// was handed the FIRST VM's overlay key, and `map_init_inner` could purge
    /// that VM's row as a "dead previous tenant".
    static HM_INT_FAST_LAST_KEY: std::cell::Cell<Option<(usize, usize, i32, usize)>> =
        const { std::cell::Cell::new(None) };
}

#[inline]
fn hm_int_fast_obj_key(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    let ptr = this.as_ptr() as usize;
    // GC-SOUNDNESS: the memo used to be validated by raw pointer ALONE. A raw
    // address is not a stable object identity under a moving/compacting
    // collector: once a map dies and its address is handed to a fresh
    // allocation, the pointer test still passed and this returned the DEAD
    // map's overlay key. The new map would then read and write its entries
    // under the wrong key — invisible to any other thread (whose own memo is
    // cold and resolves the correct key through `widened_obj_key`), and
    // invisible to `gc_prune_dead_collection_overlays`, which walks the key
    // registry and would drop those entries as belonging to a dead object.
    //
    // Identity hashes are minted from a counter at allocation
    // (`VmHeap::next_identity_hash`), never derived from the address, so the
    // recycling object always presents a different hash — validating it closes
    // the hole. `native_map_init` still purges a same-address predecessor's
    // overlay entry (see there); that stays a pointer-only match on purpose,
    // since its job is precisely to evict the previous tenant.
    //
    // Cost: `identity_hash_code` is a header word read. The memo still earns
    // its keep — the path it skips (`widened_obj_key`) takes a sharded global
    // mutex and scans the hash's slot vector.
    let hash = ctx.identity_hash_code(this);
    let vm = ctx.vm_identity();
    let memo = HM_INT_FAST_LAST_KEY.with(|cache| cache.get());
    if let Some((_, _, _, key)) = memo.filter(|(cached_vm, cached_ptr, cached_hash, _)| {
        *cached_vm == vm && *cached_ptr == ptr && *cached_hash == hash
    }) {
        return key;
    }
    let key = widened_obj_key(ctx, this);
    HM_INT_FAST_LAST_KEY.with(|cache| cache.set(Some((vm, ptr, hash, key))));
    key
}

/// Drop whatever the integer overlay is holding for `this`, answering
/// whether it held anything.
///
/// The overlay is keyed by an identity the object keeps across relocation,
/// NOT by its contents, so nothing that resets a map through its fields --
/// `publish_map_table`, `set_map_size(.., 0)`, a fresh bucket array -- empties
/// it. `native_map_size` prefers [`hm_int_fast_len`] over the size field, so a
/// map reset field-by-field while an overlay survives reports the OVERLAY's
/// count and can only ever grow. That is what a keySet view's backing did on
/// every rebuild: `resync_view_set_inner` re-put the source's current keys
/// into a backing whose overlay still held every key it had ever seen, so
/// `keySet().size()` tracked the high-water mark and never came back down
/// after a `remove` or a `clear`.
fn hm_int_fast_purge(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let object_key = hm_int_fast_obj_key(ctx, this);
    hm_int_fast_shard_for(object_key)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&object_key)
        .is_some()
}

fn hm_int_fast_len(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    let key = hm_int_fast_obj_key(ctx, this);
    hm_int_fast_shard_for(key)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .map(|state| state.entries.len())
}

/// The bucket count of the JDK table an integer-overlay map models (the one
/// [`materialize_hm_int_fast`] would build), or `None` when `this` has no
/// overlay row. JIT round 16 (lane compatdrain): the serialized form's
/// `buckets` and `threshold` of a map the overlay serves.
fn hm_int_fast_java_cap(ctx: &dyn NativeContext, this: ObjectRef) -> Option<usize> {
    let key = hm_int_fast_obj_key(ctx, this);
    hm_int_fast_shard_for(key)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .map(|state| state.entries.java_table_cap(state.start_cap))
}

fn try_hm_int_fast_put(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key_value: Value,
    value: Value,
) -> Option<MethodCallResult> {
    try_hm_int_fast_put_mode(ctx, this, key_value, value, false)
}

/// [`try_hm_int_fast_put`], with `remap` choosing the compute-family insert
/// ([`DenseIntEntries::insert_remap`]: a new key at its bucket's head, the
/// table grown on entry only) over `put`'s.
fn try_hm_int_fast_put_mode(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key_value: Value,
    value: Value,
    remap: bool,
) -> Option<MethodCallResult> {
    let key_ref = match key_value {
        Value::Object(Some(key)) => key,
        _ => return None,
    };
    // The overlay is a Rust-side REPRESENTATION of a `HashMap`: the entries are
    // in `hm_int_fast_shards()` and the real `table`/`size` stay empty until
    // `materialize_hm_int_fast` moves them. Natives can read either, but under
    // `--jdk-only` the real `HashMap$HashIterator` bytecode drives the iterator
    // carriers directly and sees only the real table, so every `HashSet` /
    // `HashMap` keyed by `Integer`, `Character`, `Short`, `Byte` or `Boolean`
    // iterated as EMPTY -- `size()` and `contains` answered from the overlay
    // and `iterator().hasNext()` from the empty table. MEASURED 2026-09-18
    // (`HashSet<Integer>` of {17, 3, 2, 1}: `size()` 4, iteration 0), and it
    // is what failed `CollectionUtilsTests.firstElementWithSet`. A map that is
    // never put through the overlay is real from its first entry. The same
    // holds in `--compatible` once the `jdk-hash-collections` row hands the
    // family to its bytecode (JIT round 16, hcb).
    if ctx.aligned(cratonvm_types::HotSpotAlignment::JdkHashCollections) {
        return None;
    }
    let int_key = match unbox_wrapper(ctx, key_ref) {
        Some(Value::Int(value)) => value,
        _ => return None,
    };
    // See `HmIntFastState::key_class`: a key whose wrapper class differs from
    // the one the overlay already holds is a DIFFERENT `HashMap` key in the
    // JDK, so it must not reuse the overlay's `i32` slot. Returning `None`
    // sends the caller down the materialize + node path, which distinguishes
    // the two through `map_keys_equal`.
    let key_class = ctx.class_id_of_object(key_ref);
    // `CRATONVM_COMPAT_BOOLEAN_KEY_HASH`: the overlay iterates in the order
    // `keys_in_java_hashmap_order` derives from the UNBOXED value, which is
    // 0/1 for a `Boolean` where the JDK buckets 1231/1237. Refusing the key
    // here (before any overlay row exists) keeps a `Boolean`-keyed map on the
    // node path, whose hash is `map_key_raw_hash`'s. A map with at most two
    // distinct keys loses nothing by it. `try_hm_int_fast_get` needs no twin:
    // with no `Boolean` ever admitted, its `key_class` screen declines.
    if boolean_key_bucket_hash(ctx, key_class, int_key).is_some() {
        return None;
    }
    let object_key = hm_int_fast_obj_key(ctx, this);
    {
        let mut table = hm_int_fast_shard_for(object_key)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(state) = table.get_mut(&object_key) {
            if !hm_int_fast_key_class_ok(state, key_class) {
                return None;
            }
            state.key_class = Some(key_class);
            let start_cap = state.start_cap;
            let old = (if remap {
                state
                    .entries
                    .insert_remap(int_key, (key_ref, value), start_cap)
            } else {
                state.entries.insert(int_key, (key_ref, value))
            })
            .map(|(_, old_value)| old_value);
            let structural = old.is_none();
            drop(table);
            // A key the overlay did not already hold is a structural
            // modification of the `HashMap` this overlay stands in for, and
            // until 2026-08-27 it moved NO generation at all: the entries live
            // in the Rust side table, so none of the node-path
            // `bump_map_mod_count` calls ran. See [`hm_int_fast_bump`].
            if structural {
                hm_int_fast_bump(ctx, this);
            }
            return Some(Ok(Some(old.unwrap_or(Value::Object(None)))));
        }
    }
    // JIT round 20 wave 17 (lane hashOff), correctness, unconditional: never
    // START an overlay for a receiver with a real `table` field (a JDK
    // image's `HashMap`, in every mode). With the `jdk-hash-collections` row
    // off the natives share that object with JDK bytecode they cannot gate
    // -- `HashMap.clone()` (`Object.clone` copies no side table, then
    // `putMapEntries` / `putVal`), a subclass's own bodies, the
    // `HashMap$HashIterator` family, `treeifyBin`, any JDK body the JIT
    // dispatches or splices -- and each read `table` / `size` empty while
    // the entries sat in the side table. Materialising before every such
    // route would need a hook in front of code this crate never sees, so the
    // overlay is refused at its only creation point instead: an Integer-keyed
    // map then takes the node path into `table`, like every other key type.
    // The synthetic class library (no `table` field) keeps the overlay. The
    // state-exists arm above cannot fire for such a receiver any more.
    // `docs/feature-designs/jdk-only-mode.md`.
    if receiver_table_slot(ctx, this).is_some() {
        return None;
    }
    let (_, size, cap) = map_state(ctx, this);
    if size != 0 {
        return None;
    }
    // `CRATONVM_COMPAT_MAP_LOAD_FACTOR`: the overlay's iteration order models
    // a 0.75 table grown from `start_cap`; a map with its own factor keeps
    // the node path, which grows at that factor (`map_grow_threshold`).
    let start_cap = if map_load_factor_enabled() {
        if map_custom_load_factor(ctx, this).is_some() {
            return None;
        }
        std::cmp::max(cap, 1) as usize
    } else {
        0
    };

    let old = {
        let mut table = hm_int_fast_shard_for(object_key)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = table.entry(object_key).or_default();
        if !hm_int_fast_key_class_ok(state, key_class) {
            return None;
        }
        if state.key_class.is_none() {
            state.start_cap = start_cap;
        }
        state.key_class = Some(key_class);
        let start_cap = state.start_cap;
        let old = (if remap {
            state
                .entries
                .insert_remap(int_key, (key_ref, value), start_cap)
        } else {
            state.entries.insert(int_key, (key_ref, value))
        })
        .map(|(_, old_value)| old_value);
        old
    };
    if old.is_none() {
        hm_int_fast_bump(ctx, this);
    }
    Some(Ok(Some(old.unwrap_or(Value::Object(None)))))
}

/// Move `map`'s structural-modification generation for an overlay insert.
///
/// The integer overlay is a REPRESENTATION of a `java.util.HashMap`, so every
/// observer of that map must see the same events it would see from the node
/// table, and the generation is one of those events. Two readers depend on it:
/// the `HashMap$HashIterator` comodification check, and the keySet-view
/// rebuild elision, which SKIPS a resync while it has not moved.
///
/// Without this an int-keyed `HashMap` reported "never modified" for its whole
/// life. `probes/MapGenProbe` measured `keySet().size() == 0` on a six-entry
/// map whose iterator yielded all six -- and that pair is exactly the shape
/// ecj's `StackMapFrameCodeStream.getFramePositions` is built out of:
///
/// ```java
/// int[] positions = new int[set.size()];
/// int n = 0;
/// for (Object pos : set) { positions[n++] = ...; }   // index == length
/// ```
///
/// which threw `ArrayIndexOutOfBoundsException: Index n out of bounds for
/// length n` on 49 of Tomcat's 81 non-passing classes on 2026-08-27 -- every
/// JSP compile runs through ecj, and its frame-position map is int-keyed.
///
/// No Java heap allocation and no Java dispatch, so the contract on
/// [`jit_overlay_hashmap_put`] -- which reaches this through the same
/// `try_hm_int_fast_put` -- still holds.
fn hm_int_fast_bump(ctx: &mut dyn NativeContext, map: ObjectRef) {
    bump_map_mod_count(&*ctx, map);
}

fn try_hm_int_fast_get(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    key_value: Value,
) -> Option<MethodCallResult> {
    let key_ref = match key_value {
        Value::Object(Some(key)) => key,
        _ => return None,
    };
    let int_key = match unbox_wrapper(ctx, key_ref) {
        Some(Value::Int(value)) => value,
        _ => return None,
    };
    let object_key = hm_int_fast_obj_key(ctx, this);
    let key_class = ctx.class_id_of_object(key_ref);
    let table = hm_int_fast_shard_for(object_key)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let state = table.get(&object_key)?;
    // Wrapper-class mismatch (see `HmIntFastState::key_class`): this is not a
    // key this overlay can answer for. Fall through to the node path rather
    // than reporting the numerically-equal entry of a different wrapper type.
    if !hm_int_fast_key_class_ok(state, key_class) {
        return None;
    }
    Some(Ok(Some(
        state
            .entries
            .get(&int_key)
            .map(|(_, value)| value)
            .unwrap_or(Value::Object(None)),
    )))
}

/// The integer overlay's own answer for `key`, telling "absent" apart from
/// "present with a null value" (which [`try_hm_int_fast_get`] cannot):
/// `Some(None)` absent, `Some(Some(v))` present, and `None` when the overlay
/// does not hold this map or cannot answer for this key. No Java, no
/// allocation.
fn hm_int_fast_lookup(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    key: Value,
) -> Option<Option<Value>> {
    let Value::Object(Some(key_ref)) = key else {
        return None;
    };
    let Some(Value::Int(int_key)) = unbox_wrapper(ctx, key_ref) else {
        return None;
    };
    let object_key = hm_int_fast_obj_key(ctx, this);
    let key_class = ctx.class_id_of_object(key_ref);
    let table = hm_int_fast_shard_for(object_key)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let state = table.get(&object_key)?;
    if !hm_int_fast_key_class_ok(state, key_class) {
        return None;
    }
    Some(state.entries.get(&int_key).map(|(_, value)| value))
}

fn materialize_hm_int_fast(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let object_key = hm_int_fast_obj_key(ctx, this);
    let (int_keys, java_cap): (Vec<i32>, usize) = {
        let table = hm_int_fast_shard_for(object_key)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(state) = table.get(&object_key) else {
            return Ok(this);
        };
        (
            state.entries.keys_in_java_hashmap_order(state.start_cap),
            state.entries.java_table_cap(state.start_cap),
        )
    };

    let map_pin = ctx.pin_native_root(this);
    let current = ctx.read_native_pin(map_pin, this);
    // Moving the overlay into the node table changes the REPRESENTATION, not
    // the contents, so it must not be observable as a modification: the
    // per-entry `native_map_put_evict_pinned` below bumps the generation once
    // per entry, and an iterator already walking this map would then throw a
    // `ConcurrentModificationException` HotSpot never throws. Capture the
    // generation here and put it back at the end.
    let gen_before = read_map_mod_count(&*ctx, current);
    set_map_size(ctx, current, 0);
    // `CRATONVM_COMPAT_OVERLAY_MATERIALIZE_CAP`: the node table is the one the
    // overlay models (`java_table_cap`: never shrunk by a `remove`, and grown
    // on ENTRY by a compute-family insert, which can leave it one entry past
    // its threshold), and the re-puts below do not grow it. Re-putting into
    // the start table grew it again from the entry count: a table grown and
    // then emptied came back smaller, and 13 entries at 16 buckets came back
    // at 32 -- every later iteration in the wrong order until the next insert.
    let presize = overlay_materialize_cap_enabled();
    if presize {
        let (buckets, _, cap) = map_state(ctx, current);
        let have = if buckets.is_some() {
            cap.max(0) as usize
        } else {
            0
        };
        if have < java_cap {
            // Both calls can collect; `current` is re-read after them.
            let component = bucket_table_component(ctx, current);
            let fresh = ctx.new_ref_array(component, java_cap);
            let fresh_pin = ctx.pin_native_root(fresh);
            let current = ctx.read_native_pin(map_pin, this);
            let fresh = ctx.read_native_pin(fresh_pin, fresh);
            publish_map_table(ctx, current, fresh, java_cap as i32);
            ctx.unpin_native_roots(fresh_pin);
            // `resize()`'s other postcondition, as `map_resize_inner` writes it.
            let lf = map_custom_load_factor(&*ctx, current).unwrap_or(0.75);
            try_set_jdk_map_field(
                ctx,
                current,
                "threshold",
                Value::Int(jdk_hashmap_threshold(java_cap as i32, lf)),
            );
        }
    }
    for int_key in int_keys {
        let (key_ref, value) = {
            let table = hm_int_fast_shard_for(object_key)
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            table
                .get(&object_key)
                .and_then(|state| state.entries.get(&int_key))
                .expect("fast HashMap entry disappeared during materialization")
        };
        let iter_pin_base = ctx.pin_native_root(key_ref);
        let value_pin = pin_value(ctx, value);
        let mut current = ctx.read_native_pin(map_pin, this);
        let key_ref = ctx.read_native_pin(iter_pin_base, key_ref);
        let value = read_pinned_elem(ctx, value_pin, value);
        let put = native_map_put_evict_pinned_inner(
            ctx,
            &mut current,
            Value::Object(Some(key_ref)),
            value,
            true,
            map_pin,
            iter_pin_base,
            value_pin,
            GrowAlloc::SingleAttempt,
            None,
            false,
            PutMode {
                no_grow: presize,
                assume_absent: false,
            },
        );
        // `CRATONVM_COMPAT_OVERLAY_MATERIALIZE_ROLLBACK` (round 15 wave 4,
        // lane compat4): a failed re-put leaves ONE representation. The
        // overlay is still whole and still answers every read first, and the
        // node table was empty with `size` 0 on entry (`try_hm_int_fast_put`
        // admits only an empty map), so the nodes linked so far are dropped,
        // `size` goes back to 0 and the generation to its entry value -- the
        // node table no longer disagrees with the overlay about `size()` or
        // what an iterator over it sees. The next materialisation starts over.
        if put.is_err() && overlay_materialize_rollback_enabled() {
            let current = ctx.read_native_pin(map_pin, this);
            if let (Some(buckets), _, _) = map_state(&*ctx, current) {
                for slot in 0..ctx.array_length(buckets) {
                    ctx.set_array_element(buckets, slot, Value::Object(None));
                }
            }
            set_map_size(ctx, current, 0);
            restore_map_mod_count(ctx, current, gen_before);
        }
        // `map_pin` is this function's base: an `Err` releases the whole
        // group (the entry pins included) before it propagates.
        unpin_on_err(ctx, map_pin, put)?;
        ctx.unpin_native_roots(iter_pin_base);
    }
    hm_int_fast_shard_for(object_key)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&object_key);
    let current = ctx.read_native_pin(map_pin, this);
    restore_map_mod_count(ctx, current, gen_before);
    ctx.unpin_native_roots(map_pin);
    Ok(current)
}

/// JIT direct-call probe for the exact-HashMap integer-overlay GET fast path
/// (perf/halfgap-20260717). `None` = not servable by the overlay (non-Integer
/// key, or the map has materialized real nodes) — the caller must fall back
/// to the full dispatch path. Never allocates on the Java heap and never
/// dispatches Java code, so the VM-side thin helper may call it without the
/// full `safe_native_call` wrapper (same contract as the wrapper-free
/// `Integer.intValue` direct helper).
pub fn jit_overlay_hashmap_get(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    key: Value,
) -> Option<MethodCallResult> {
    try_hm_int_fast_get(ctx, this, key)
}

/// PUT sibling of [`jit_overlay_hashmap_get`]. The overlay insert only
/// touches the Rust-side table (whose entries the GC scans as roots via
/// `gc_overlay_roots_for_collection`), so the no-Java-heap-allocation /
/// no-Java-dispatch contract holds here too; a `None` (first put on a map
/// with existing real nodes, non-Integer key, serialization put, ...) must
/// fall back to full dispatch, which handles materialization.
pub fn jit_overlay_hashmap_put(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: Value,
    value: Value,
) -> Option<MethodCallResult> {
    try_hm_int_fast_put(ctx, this, key, value)
}

// ---------------------------------------------------------------------------
// Cached debug-flag probes.
//
// These flags are read from the OS environment on every native call in the
// original code (`cratonvm_types::flags::runtime_var`/`var_os`), which is a syscall-backed lookup
// on the hot path. The values never change for the lifetime of the process,
// so we resolve each exactly once into a `OnceLock<bool>` and reference the
// cached boolean thereafter. Behaviour is identical — the flag is still
// "enabled iff the env var is set" — but the per-call OS probe is gone.
// ---------------------------------------------------------------------------

/// `true` iff `CRATONVM_HM_TRACE` is set (HashMap equals/contract tracing).
fn dbg_hm_trace() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_HM_TRACE").is_some())
}

/// `true` iff `CRATONVM_HS_ITR_DBG` is set (HashSet iterator tracing).
fn dbg_hs_itr() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_HS_ITR_DBG").is_some())
}

/// `true` iff `CRATONVM_DBG_SBLOAD` is set (synthetic-build-load tracing).
fn dbg_sbload() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SBLOAD").is_some())
}

/// `true` iff `CRATONVM_DBG_KCBOOL` is set — traces enum-keyed map lookups,
/// the prime suspect for Keycloak's `Profile.isFeatureEnabled` NPE
/// (`features.get(feature)` returning null → `Boolean.booleanValue()` on
/// null). When set, an enum-keyed `Map.get` that misses logs the key's
/// `(class_id, ordinal)` plus, for every node in the map, that node's enum
/// identity — so the orchestrator can confirm whether a duplicate enum
/// constant object exists (same `(class_id, ordinal)`, different pointer).
fn dbg_kcbool() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_KCBOOL").is_some())
}

/// `true` iff `CRATONVM_DBG_HMPUT` is set (HashMap put node-walk tracing).
/// Cached to avoid a syscall-backed env probe per node on the hot put path.
fn dbg_hmput() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_HMPUT").is_some())
}

/// Diagnostic: on an enum-keyed `Map.get` miss, dump the lookup key's enum
/// identity and every node's enum identity in the map. `nodes` is an iterator
/// of `(node_key_ref)` for each entry currently in the map.
fn dbg_kcbool_report_miss(
    ctx: &dyn NativeContext,
    kind: &str,
    key: ObjectRef,
    node_keys: &[ObjectRef],
) {
    let key_id = enum_key_identity(ctx, key);
    eprintln!(
        "[cratonvm-dbg KCBOOL] {kind} MISS: key ptr={:p} enum_id={:?} entries={}",
        key.as_ptr(),
        key_id,
        node_keys.len(),
    );
    if let Some((kcls, kord)) = key_id {
        for nk in node_keys {
            if let Some((ncls, nord)) = enum_key_identity(ctx, *nk) {
                let same_logical = ncls == kcls && nord == kord;
                if same_logical {
                    eprintln!(
                        "[cratonvm-dbg KCBOOL]   DUPLICATE ENUM CONSTANT: node ptr={:p} \
                         enum_id=({ncls},{nord}) == key but pointer differs — \
                         this is the VM enum-canonicalization bug",
                        nk.as_ptr(),
                    );
                }
            }
        }
    }
}

/// Create an iterator over a snapshot array of the given size.
///
/// Returns a real `java.util.Arrays$ArrayItr` — the JDK's own `Object[]`
/// iterator — so `getClass()`, `hasNext()`, `next()` and the `remove()` that
/// throws are all the real thing. It falls back to the historical
/// `HashMap$KeyItr` shape (field 0 = array, 1 = cursor, 2 = total) only when
/// the image has no `Arrays$ArrayItr` to build, i.e. a `synthetic-jdk` build.
///
/// This used to mint `HashMap$KeyItr` unconditionally, and two things were
/// wrong with that. The visible one: no JDK declares that class, so
/// `Arrays.asList(a).iterator().getClass()` reported `java.util.HashMap$KeyItr`
/// where HotSpot says `java.util.Arrays$ArrayItr` — measured by
/// `probes/StrictIterPrimitivesProbe`. The structural one: it minted through
/// the INFALLIBLE `alloc_synthetic` while three sibling sites used the fallible
/// spelling, so under `--jdk-only` the refusal was order-dependent rather than
/// a policy — `Arrays.asList(a).iterator()` created the class and every LATER
/// `try_alloc_synthetic` for that name then found it and succeeded, which is
/// why `linkedhashset.iterator()` passed in a probe that had called
/// `Arrays.asList` first and threw `NoClassDefFoundError` in one that had not.
///
/// Building the real class costs one allocation and two field writes, against
/// one allocation and three writes for the fabricated shape, so the honest
/// version is not the expensive one — see `probes/SnapshotIteratorCostProbe`.
pub fn make_iterator_from_array(
    ctx: &mut dyn NativeContext,
    snapshot_array: ObjectRef,
    size: usize,
) -> MethodCallResult {
    // GC-SAFETY: `alloc_synthetic` can run the iterator class's `<clinit>` and
    // allocate, which relocates the snapshot array the caller handed us. Root
    // it across the allocation and read both halves of the new graph back
    // through their pins. Covered by
    // `gc_native_pins::generic_snapshot_iterator_roots_array_and_shell_across_allocation`.
    let array_pin = ctx.pin_native_root(snapshot_array);
    // The real iterator reads `a.length`, so a caller that over-allocated its
    // snapshot has to be trimmed first — `real_snapshot_iterator` does that and
    // then builds the same shape.
    if ctx.array_length(snapshot_array) != size {
        let cur = ctx.read_native_pin(array_pin, snapshot_array);
        // `None`: this entry point is handed a bare array and knows of no
        // collection behind it, so `remove()` must keep raising
        // `UnsupportedOperationException` — which is what the real
        // `Arrays.asList(a).iterator()` does too.
        match real_snapshot_iterator(ctx, cur, size, None) {
            Ok(v) => {
                ctx.unpin_native_roots(array_pin);
                return Ok(v);
            }
            // Only reachable in an image with no `Arrays$ArrayItr`. Re-read the
            // array through its pin BEFORE dropping it: `real_snapshot_iterator`
            // allocated, so the bare local above is stale by now, and handing a
            // from-space reference to the fallback would store a dead array in
            // the iterator. Nothing allocates between this unpin and the re-pin
            // the callee takes as its first action.
            Err(_) => {
                let cur = ctx.read_native_pin(array_pin, snapshot_array);
                ctx.unpin_native_roots(array_pin);
                return make_fabricated_iterator_from_array(ctx, cur, size);
            }
        }
    }
    let snapshot_array_cur = ctx.read_native_pin(array_pin, snapshot_array);
    if let Some(itr) = alloc_real_array_iterator(ctx, snapshot_array_cur) {
        ctx.unpin_native_roots(array_pin);
        return Ok(Some(Value::Object(Some(itr))));
    }
    let snapshot_array = ctx.read_native_pin(array_pin, snapshot_array);
    ctx.unpin_native_roots(array_pin);
    make_fabricated_iterator_from_array(ctx, snapshot_array, size)
}

/// The pre-2026-08-06 snapshot-iterator shape, kept for images with no real
/// `java.util.Arrays$ArrayItr` to build (`synthetic-jdk`). Under `--jdk-only`
/// the fabrication is refused, and correctly: no JDK declares this class.
fn make_fabricated_iterator_from_array(
    ctx: &mut dyn NativeContext,
    snapshot_array: ObjectRef,
    size: usize,
) -> MethodCallResult {
    let array_pin = ctx.pin_native_root(snapshot_array);
    let itr = match try_alloc_synthetic(ctx, "java/util/HashMap$KeyItr", 3) {
        Ok(itr) => itr,
        Err(refused) => {
            ctx.unpin_native_roots(array_pin);
            return Err(refused);
        }
    };
    let itr_pin = ctx.pin_native_root(itr);
    let itr = ctx.read_native_pin(itr_pin, itr);
    let snapshot_array = ctx.read_native_pin(array_pin, snapshot_array);
    ctx.set_field(itr, 0, Value::Object(Some(snapshot_array)));
    let itr = ctx.read_native_pin(itr_pin, itr);
    ctx.set_field(itr, 1, Value::Int(0));
    let itr = ctx.read_native_pin(itr_pin, itr);
    ctx.set_field(itr, 2, Value::Int(size as i32));
    let itr = ctx.read_native_pin(itr_pin, itr);
    ctx.unpin_native_roots(array_pin);
    Ok(Some(Value::Object(Some(itr))))
}

/// The `--jdk-only` stand-in for a snapshot iterator, built out of real JDK
/// classes only.
///
/// Every snapshot iterator this crate hands back wears the class name
/// `java.util.HashMap$KeyItr` or `java.util.TreeSet$Itr`. No JDK declares
/// either — the real ones are `HashMap$KeyIterator` and `TreeMap$KeyIterator`
/// — so under `--jdk-only` the fabrication is refused and the iteration dies
/// with `NoClassDefFoundError` before it starts. That was six of the seven
/// strict-mode divergences left in `probes/JdkOnlyCollectionViewProbe`:
/// `Collections.unmodifiable{Set,Map,SortedSet,NavigableSet}`, `Map.of` and
/// `Map.copyOf` all reach a set/entry-set iterator.
///
/// The snapshot itself needs nothing fabricated: it is an `Object[]`, and the
/// JDK already ships the fixed-size list for exactly that shape. Wrapping it in
/// a real `Arrays$ArrayList` and asking THAT for its iterator yields a real
/// `java.util.Arrays$ArrayItr` running real bytecode — same elements, same
/// order, and a `getClass()` that finally tells the truth.
///
/// `invoke_virtual_bytecode_only`, not `invoke_virtual`: this crate registers a
/// native over `Arrays$ArrayList.iterator` that routes straight back into
/// [`make_iterator_from_array`], i.e. back to the fabricated class this
/// function exists to avoid.
///
/// `remove()` used to be the one behaviour that genuinely differed: it writes
/// through to the backing collection on the fabricated iterator
/// (`native_map_key_itr_remove` / `native_ts_itr_remove`), while a real
/// `Arrays$ArrayItr` declares no `remove()` at all and reaches the
/// `java.util.Iterator.remove()` DEFAULT method, whose whole body is
/// `throw new UnsupportedOperationException("remove")`.
///
/// That difference was not survivable. `com.sun.jmx.mbeanserver.MXBeanSupport`
/// reduces a `HashSet` of candidate MXBean interfaces with `it.remove()` inside
/// `while (candidates.size() > 1)`, so `--jdk-only` died on
/// `ManagementFactory.getPlatformMBeanServer()` with
/// `NotCompliantMBeanException: sun.management.GarbageCollectorImpl: remove`
/// (`regression-suite/src/RJdkJmx.java:133`). `backing` closes it: the caller
/// names the live collection, it is recorded in
/// [`snapshot_itr_backing_table`], and `native_itr_remove_noop`'s
/// `Arrays$ArrayItr` arm deletes from it. A caller with no backing to offer
/// passes `None` and keeps the JDK-correct `UnsupportedOperationException`.
fn real_snapshot_iterator(
    ctx: &mut dyn NativeContext,
    elems: ObjectRef,
    count: usize,
    backing: Option<(ObjectRef, SnapshotItrRoute)>,
) -> MethodCallResult {
    // GC-SAFETY: `backing` is a bare Rust local, and everything below it
    // allocates (the exact-length copy, the iterator itself). Root it for the
    // whole function and read it back through the pin before it is stored, or
    // the table would hold a from-space reference to the collection whose
    // `remove()` must land.
    let backing_pin = backing.map(|(b, _)| ctx.pin_native_root(b));
    // `Arrays$ArrayList` derives `size()` from `a.length`, so the array it is
    // handed has to be exactly the logical length. Callers over-allocate:
    // `native_ts_iterator` snapshots into `size.max(1)` slots, which would
    // otherwise report an empty TreeSet as a one-element list of `null`.
    let arr = if ctx.array_length(elems) == count {
        elems
    } else {
        let (elems, exact) = rooted_across1(ctx, elems, |ctx| alloc_ref_array(ctx, count));
        // Neither accessor allocates, so no GC point separates the read from
        // the store and both refs stay live for the whole copy.
        for i in 0..count {
            let v = ctx.get_array_element(elems, i);
            ctx.set_array_element(exact, i, v);
        }
        exact
    };
    match alloc_real_array_iterator(ctx, arr) {
        Some(itr) => {
            // Nothing allocates between here and the store, and `itr` is the
            // freshly returned (already post-allocation) reference.
            if let (Some(pin), Some((b, route))) = (backing_pin, backing) {
                let b = ctx.read_native_pin(pin, b);
                record_snapshot_itr_backing(ctx, itr, b, route);
                ctx.unpin_native_roots(pin);
            }
            Ok(Some(Value::Object(Some(itr))))
        }
        // Nothing is left to fall back TO — returning the fabricated shape
        // here would defeat the whole point — so surface a refusal naming the
        // class that is actually missing, not the synthetic one the caller
        // asked for.
        None => {
            if let Some(pin) = backing_pin {
                ctx.unpin_native_roots(pin);
            }
            Err(cratonvm_native_api::refusal_to_java_failure(
                ctx,
                cratonvm_native_api::ClassIdentityError::Refused {
                    name: "java/util/Arrays$ArrayItr".to_string(),
                    reason: "--jdk-only: a snapshot iterator needs the real array \
                             iterator to stand in for the refused \
                             `java.util.HashMap$KeyItr`, and this image has none"
                        .to_string(),
                },
            ))
        }
    }
}

/// Allocate a real `java.util.Arrays$ArrayItr` over `arr`, or `None` when this
/// image has no such class.
///
/// `Arrays$ArrayItr` is the JDK's own iterator over a bare `Object[]`, and it
/// declares exactly the two fields a snapshot iterator needs:
///
/// ```text
///   cursor : int
///   a      : Object[]   (final)
/// ```
///
/// So this is construction, not a fabricated layout: both fields are declared
/// by the real class, both are written by NAME, and both receive a value of the
/// declared type — the same thing `ArrayItr(E[] a)` does, minus running a
/// constructor whose entire body is `this.a = a`. `hasNext` and `next` are then
/// real JDK bytecode reading real fields. `remove` is the one exception, and
/// only when the creator recorded a backing collection for it: the real class
/// declares none, so the call would otherwise reach the `Iterator.remove()`
/// default and throw — see [`snapshot_itr_backing_table`].
///
/// `arr` must ALREADY be exactly the logical length: `hasNext()` is
/// `cursor < a.length`, so an over-allocated snapshot would iterate trailing
/// nulls. Both callers trim first.
///
/// Returns `None` rather than fabricating anything, so a build without the real
/// `java.util` (the `synthetic-jdk` feature) keeps its old shape instead of
/// losing the ability to make an iterator at all.
fn alloc_real_array_iterator(ctx: &mut dyn NativeContext, arr: ObjectRef) -> Option<ObjectRef> {
    alloc_real_snapshot_iterator_of(ctx, "java/util/Arrays$ArrayItr", "a", arr)
}

/// Build a real snapshot iterator of `class_name`, whose backing array lives in
/// the field `array_field` and whose position lives in `cursor`.
///
/// The JDK has more than one iterator of exactly this shape, and which one is
/// correct depends on the collection that produced it:
///
/// ```text
///   java.util.Arrays$ArrayItr                            a         cursor
///   java.util.concurrent.CopyOnWriteArrayList$COWIterator snapshot cursor
/// ```
///
/// Both are array-plus-position, both snapshot at creation, and both inherit or
/// declare a `remove()` that throws — which is why one helper serves both and
/// why the choice is the CALLER's: returning an `Arrays$ArrayItr` from
/// `CopyOnWriteArrayList.iterator()` iterates correctly but reports the wrong
/// `getClass()`, and `probes/SnapshotIteratorShapeProbe` measures exactly that.
pub fn alloc_real_snapshot_iterator_of(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    array_field: &str,
    arr: ObjectRef,
) -> Option<ObjectRef> {
    // GC-SAFETY: `alloc_object` collects, and `arr` is a bare Rust local the
    // collector cannot see. Root it across the allocation and read both halves
    // back through their pins — the contract `make_iterator_from_array`
    // documents, and what `gc_native_pins` covers.
    //
    // gc-common w20-a: the pin is taken BEFORE `ensure_class_initialized`,
    // which runs the iterator class's `<clinit>` (and loads it) on first use —
    // a GC point. Pinned after it, a first-use collection left `arr` naming the
    // array's pre-move address, and the iterator was built over it.
    let arr_pin = ctx.pin_native_root(arr);
    let Some((cid, a_slot, cursor_slot, n_fields)) =
        real_snapshot_iterator_layout(ctx, class_name, array_field)
    else {
        ctx.unpin_native_roots(arr_pin);
        return None;
    };
    let itr = ctx.alloc_object(cid, n_fields);
    let itr_pin = ctx.pin_native_root(itr);
    let itr = ctx.read_native_pin(itr_pin, itr);
    let arr = ctx.read_native_pin(arr_pin, arr);
    ctx.set_field(itr, a_slot, Value::Object(Some(arr)));
    let itr = ctx.read_native_pin(itr_pin, itr);
    ctx.set_field(itr, cursor_slot, Value::Int(0));
    let itr = ctx.read_native_pin(itr_pin, itr);
    ctx.unpin_native_roots(arr_pin);
    Some(itr)
}

/// The class id, array slot, cursor slot and field count of the real snapshot
/// iterator `class_name`, initialising the class; `None` when this image has
/// no such class. Split out of [`alloc_real_snapshot_iterator_of`] so its
/// caller can hold the array's pin across the `<clinit>` this may run.
fn real_snapshot_iterator_layout(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    array_field: &str,
) -> Option<(ClassId, usize, usize, usize)> {
    let cid = ctx.ensure_class_initialized(class_name).ok()?;
    // `ensure_class_initialized` reports success with another class's id for
    // some loader fallbacks. The real class or nothing.
    if ctx.class_name_of_id(cid).unwrap_or_default() != class_name {
        return None;
    }
    let a_slot = ctx.resolve_field_index(class_name, array_field)?;
    let cursor_slot = ctx.resolve_field_index(class_name, "cursor")?;
    let n_fields = ctx
        .class_num_total_fields(cid)
        .max(a_slot.max(cursor_slot) + 1);
    Some((cid, a_slot, cursor_slot, n_fields))
}

// ---------------------------------------------------------------------------
// Write-through backing for the `--jdk-only` snapshot iterator
// ---------------------------------------------------------------------------
//
// `real_snapshot_iterator` hands back a REAL `java.util.Arrays$ArrayItr`, whose
// JDK 25 declaration is exactly
//
//     private static class ArrayItr<E> implements Iterator<E> {
//         private int cursor;
//         private final E[] a;
//         ...  hasNext()  next()
//     }
//
// — two fields, no `remove()`, and no third slot a backing pointer could live
// in. The fabricated `HashMap$KeyItr` it replaces carries that pointer in
// `MAP_KEY_ITR_FIELD_BACKING` and its position in `MAP_KEY_ITR_FIELD_LAST_RET`;
// this table is where the same two facts live for the real shape.
//
// WHY IT MUST WRITE THROUGH. `MXBeanSupport.findMXBeanInterface` reduces its
// candidate set with
//
//     while (candidates.size() > 1) { ... it.remove(); continue reduce; }
//
// so an iterator whose `remove()` mutates only the snapshot leaves `size()`
// unchanged and turns that loop into a livelock (or, once the outer `for` runs
// out, `IllegalArgumentException: implements more than one MXBean interface`).
// Wrapping the snapshot in a mutable `ArrayList` and returning ITS iterator
// looks like a one-line fix and is exactly this trap.

/// Which `remove(Object)` implementation owns the collection behind a snapshot
/// iterator. Recorded by the CREATOR, which knows; deriving it at removal time
/// from the backing's class would have to re-guess what `native_hs_iterator` /
/// `native_ts_iterator` already knew.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SnapshotItrRoute {
    /// `HashSet`, a `Map` key-set view, or a `ConcurrentHashMap$KeySetView` —
    /// the family `native_map_key_itr_remove` already discriminates.
    SetLike,
    /// A native `TreeSet` (including the descending snapshot), removed through
    /// the sorted array + size bookkeeping in [`ts_remove_element`].
    TreeSet,
    /// A native `LinkedList`, removed through [`native_ll_remove_object`].
    ///
    /// This is the one route whose `remove()` is not the same operation the
    /// fabricated iterator performed: `LinkedList$Itr` is node-live and unlinks
    /// the node `next()` returned, while this deletes the FIRST occurrence of
    /// that element. The two differ only for a list holding duplicates, and
    /// only on the `--jdk-only` path — where the alternative is not a
    /// better-behaved `remove()` but a `NoClassDefFoundError` before `next()`.
    LinkedList,
    /// A native `ArrayDeque`, removed through
    /// [`native_ad_remove_first_occurrence`] — which is the same call
    /// `native_ad_itr_remove` already makes for the fabricated `ArrayDeque$Itr`,
    /// so this route changes the class name and nothing else.
    ArrayDeque,
    /// A `MAP_VIEW_CARRIERS` values view — `Hashtable$ValueCollection`,
    /// `Properties`' the same, `HashMap$Values` — removed through
    /// [`native_al_remove_obj`], which is the view's own registered
    /// `remove(Object)` and already propagates into the SOURCE MAP for a live
    /// values view. So, like the `ArrayDeque` route above, this changes the
    /// class name and nothing else.
    ///
    /// It exists because [`alloc_arraylist_iterator`]'s `AL_VIEW_ITR_CLASS`
    /// mint had NO refusal arm: `try_alloc_synthetic(..)?` propagated, and
    /// under `--jdk-only` the caller died with
    /// `NoClassDefFoundError: cratonvm/internal/ArrayListViewItr` before
    /// `next()`. MEASURED on three probes at once —
    /// `MapViewBehaviourProbe` produced 0 of its 194 rows, `ItrClassProbe` 31
    /// of 66, and `RJdkEnumerations` failed the `--jdk-only` arm — all of them
    /// through `Collections$SynchronizedCollection.iterator()` over a
    /// `Properties`/`Hashtable` values view, which is what
    /// `Hashtable.values()` returns.
    ViewCollection,
}

/// The live collection a snapshot iterator's `remove()` must delete from.
#[derive(Clone, Copy)]
struct SnapshotItrBacking {
    backing: ObjectRef,
    route: SnapshotItrRoute,
    /// The `cursor` value at the last successful `remove()`, or `-1` before the
    /// first one.
    ///
    /// The real `ArrayItr.next()` is real bytecode (`return a[cursor++]`), so
    /// nothing of ours runs there to maintain a `lastRet`. It does not have to:
    /// `cursor` alone says which element was last returned (`cursor - 1`), and
    /// this field supplies the other half of the JDK's `IllegalStateException`
    /// contract — `remove()` twice with no intervening `next()` finds `cursor`
    /// unchanged since the previous removal.
    last_removed_cursor: i32,
}

/// Iterator `ObjectRef` (as a GC-stable [`widened_obj_key`]) -> backing
/// collection.
///
/// GC CONTRACT — this table holds a heap reference the collector cannot see any
/// other way, so it is wired into all three overlay hooks, exactly like
/// [`cslm_comparator_table`]:
///
///   * [`for_each_overlay_ref`] — the single funnel behind
///     `gc_scan_collection_overlay_roots` (root scan) and
///     `gc_update_collection_overlay_refs` (post-move remap), so the backing is
///     both kept live and repointed by a moving young collection;
///   * [`gc_overlay_roots_for_collection_with_keys`] — the per-owner rule the
///     non-moving marker and the old-gen mark BFS use, so the backing is
///     retained only while the ITERATOR is itself reachable;
///   * `drop_overlay_entries_for_dead_keys` — reached from
///     `gc_prune_dead_collection_overlays` and VM teardown, so a dead
///     iterator's entry is swept; a later object that recycles its identity
///     hash gets its own generation from `widened_obj_key` and cannot inherit
///     it even before the sweep.
///
/// The KEY is GC-safe for the same reason every other overlay key is: an
/// identity hash travels with the object header across a relocation, and
/// `overlay_owner_keys` is rebuilt onto post-move addresses by the remap.
fn snapshot_itr_backing_table() -> &'static Mutex<OverlayRows<SnapshotItrBacking>> {
    static T: std::sync::OnceLock<Mutex<OverlayRows<SnapshotItrBacking>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(OverlayRows::new()))
}

/// Record the collection `itr`'s `remove()` must write through to.
///
/// `itr` and `backing` must both be POST-allocation references: the caller
/// allocates the iterator and has to read `backing` back through its pin first.
fn record_snapshot_itr_backing(
    ctx: &dyn NativeContext,
    itr: ObjectRef,
    backing: ObjectRef,
    route: SnapshotItrRoute,
) {
    let key = widened_obj_key(ctx, itr);
    snapshot_itr_backing_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            key,
            SnapshotItrBacking {
                backing,
                route,
                last_removed_cursor: -1,
            },
        );
}

/// The recorded backing for `itr`, or `None` for a genuine snapshot iterator
/// (no backing collection — `remove()` correctly raises
/// `UnsupportedOperationException`).
fn snapshot_itr_backing(ctx: &dyn NativeContext, itr: ObjectRef) -> Option<SnapshotItrBacking> {
    let key = widened_obj_key(ctx, itr);
    snapshot_itr_backing_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .copied()
}

/// Record that `itr` removed the element it returned at `cursor - 1`, so a
/// second `remove()` with no intervening `next()` raises `IllegalStateException`
/// the way the JDK's `lastRet = -1` does.
fn note_snapshot_itr_removed(ctx: &dyn NativeContext, itr: ObjectRef, cursor: i32) {
    let key = widened_obj_key(ctx, itr);
    let mut table = snapshot_itr_backing_table()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(state) = table.get_mut(&key) {
        state.last_removed_cursor = cursor;
    }
}

fn native_unsorted_set_comparator(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

/// Register all collection native methods.
// JDK-ONLY-CLASSIFY: stub — crate-wide verdict, applied by one line. The
// `set_category(Bridge)` below is not a per-registration judgement: it is a
// dynamic ambient assignment that every callee in this file inherits, and it
// covers 1,195 of the crate's 1,219 registrations. Cross-checked against JDK 25
// with `javap -p -s`, **not one** of those 1,195 targets an ACC_NATIVE method
// (622 target methods with concrete bytecode, 214 are abstract interface
// methods, the remainder are absent from the image or use a class name this
// audit could not resolve statically). `java.util` is pure Java: there is no
// VM/OS boundary in this crate to bridge to. The correct disposition for
// almost everything here is jdk-only-native-review.md's rule 4 — "concrete
// bytecode + incomplete replacement → delete from the strict path" — with a
// small number of genuine `Intrinsic` promotions.
//
// One such promotion exists as of 2026-08-06 and is the crate's only
// `register_with_kind`: `("java/util/Iterator", "remove", "()V")`. The counts
// above are unchanged (1,219 registrations); the ambient `Bridge` now covers
// 1,194 of them. Its §1.4 justification is written out at the registration
// site in `register_iterator_protocol_natives`.
//
// DO NOT flip this line to `SyntheticStub` as a bulk edit. That is the exact
// shape of the 2026-07-14 regression, at ~8x the blast radius, and the 214
// abstract-interface registrations additionally decide dispatch for every USER
// subclass, not just for `java.util` classes. Retag per subsystem, one PR each,
// with schema-v2 `invocations` and `overwrote` evidence.
//
// ---------------------------------------------------------------------------
// AND DO NOT FLIP THE *CLUSTER* EITHER — the missing half of `G88-1` §5.
// (H4-1, 2026-08-20. Source census, no build; every number below is a grep.)
//
// `G88-1` §5 measured that retagging the whole map/set ownership cluster at
// once — containers, view carriers, iterators, map entries, bulk ops — took
// `RCollections`, `RJdkMapViews` and `RChmKeySetView` from broken to green,
// and concluded the unit of work is the cluster rather than the registrar.
// That is right as far as it goes. **It is not the whole boundary of the
// cluster, because the cluster is not confined to Java dispatch.**
//
// Three producer populations write this crate's map/set state WITHOUT going
// through `NativeMethodRegistry` at all, so refusing a registration under
// `--jdk-only` does not stop them. `NativeKind` cannot reach any of them:
//
//   1. 168 DIRECT RUST CALLS into these natives from 18 files in two other
//      crates — `native_map_put_pub` (42), `native_map_init` (34),
//      `native_map_get_pub` (33), `native_map_contains_key_pub` (11),
//      `native_map_remove_pub` (10), and eleven more. Largest holders:
//      `native-builtins/src/phases_early.rs` (53),
//      `phases_late/beans_jndi.rs` (42), `locale_resources.rs` (12),
//      `phases_late/net_channels.rs` (12), `phases_late/xml_json.rs` (11),
//      `reflect_annotations.rs` (10).
//      Recipe: `grep -rn 'cratonvm_native_collections::native_map' --include=*.rs`.
//   2. 45 `try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", n)` sites
//      (plus 19 `HashSet`, 89 `ArrayList`, 4 `Hashtable`, 3 `Properties`)
//      outside this crate. Each allocates under the REAL class id and then
//      fills it through (1), so the object IS a real `java.util.HashMap`
//      whose real `table` is null.
//   3. 6 JIT direct helpers in `vm/src/jit/helpers.rs` that call
//      `native_chm_get`, `native_hashmap_get_exact`, `native_hashmap_put_exact`
//      and the two `jit_overlay_hashmap_*` entry points straight from compiled
//      code, past dispatch and past the kind check entirely.
//
// The consequence is the one failure mode a green arm cannot see: after the
// retag those objects are handed to REAL bytecode, which reads the real
// (empty) `table` and answers "absent" — a silently empty map, not an error.
// `G88-1` §5's three green vectors do not construct a map through any of the
// 168 sites, which is why the experiment read as a success. Compare
// `HANDOFF-20260819.md` §3: a gate whose stated population is wider than its
// measured one always reads as success.
//
// So the cluster boundary is: **every writer of a container's state, in Java
// dispatch AND in Rust.** Retiring the side state (`G88-1` N3) means migrating
// those producers to real construction — `new_object_initialized("java/util/
// HashMap", "()V", &[])` plus `invoke_virtual("put", …)` — BEFORE any tag
// moves. That work is almost entirely outside this crate: 6 of the 168 call
// sites are in files a `native-collections` change may touch.
//
// See `docs/feature-designs/jdk-only-mode.md`
// for the full map, the per-family split, and the verification plan.
// ---------------------------------------------------------------------------
pub fn register_collections_natives(registry: &mut NativeMethodRegistry) {
    register_gc_root_provider();
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_arraylist_natives(registry);
    register_hashmap_natives(registry);
    register_hashset_natives(registry);
    register_hibernate_persistent_map_natives(registry);
    // Guard against an invokeinterface/assignability edge where AssertJ's
    // SortedSet fast path can incorrectly dispatch `SortedSet.comparator()`
    // against a plain HashSet/LinkedHashSet receiver in real-JDK mode.
    // Returning null matches the Comparator contract for natural/no explicit
    // ordering and avoids a spurious NoSuchMethodError on unsorted sets.
    for set_class in ["java/util/HashSet", "java/util/LinkedHashSet"] {
        registry.register(
            set_class,
            "comparator",
            "()Ljava/util/Comparator;",
            native_unsorted_set_comparator,
        );
    }
    register_iterator_natives(registry);
    register_arrays_natives(registry);
    register_optional_natives(registry);
    register_collections_utility_natives(registry);
    register_map_entry_natives(registry);
    // RETAGGED per subsystem — second of the twelve AMBIENT-inheriting
    // registrars (see the note at `register_comparator_natives`; the other 48
    // set their own category and a call-site wrapper is inert on them, G85-1).
    //
    // MEASURED 2026-08-19 (`--dump-native-registry`, default mode):
    //   registrations 36 | invocations 61 | overwrote 0
    //   real target: 36 DECLARED WITH CODE, 0 not-declared-here, 0 ACC_NATIVE
    //
    // Every row shadows a concrete real implementation — rule 4 with no
    // residue. It is LIVE (61 dispatches), so unlike Comparator this does not
    // rest on "nothing depends on it"; it rests on `List.of`/`Map.of`/`Set.of`
    // having complete real bytecode and on heavy arm coverage.
    //
    // Note for whoever reads a red `RImmutableFactoryTypes`: that vector was
    // ALREADY failing in `SUITE=all` before this change, and is one of the five
    // long-standing baseline failures. The tag move and the arm result were
    // both checked against that baseline rather than assumed.
    registry.with_category(
        cratonvm_native_api::NativeKind::SyntheticStub,
        register_factory_natives,
    );
    register_stream_natives(registry);
    register_collectors_natives(registry);
    register_int_stream_natives(registry);
    register_long_stream_natives(registry);
    register_double_stream_natives(registry);
    register_interface_natives(registry);
    register_copy_constructor_natives(registry);
    // RETAGGED per subsystem (P0 "wholesale Bridge over-tagging").
    //
    // This registrar is one of the TWELVE in this crate that INHERIT the
    // ambient tag. The other 48 set their own `set_category(Bridge)` at their
    // head, which silently overrides a call-site wrapper — six retags written
    // that way on 2026-08-19 were no-ops and were reverted (G85-1). The
    // call-site technique only works here, and the tag move is verified from a
    // registry dump rather than inferred from green arms.
    //
    // MEASURED 2026-08-19 (`--dump-native-registry`, default mode):
    //   registrations 14 | invocations 0 | overwrote 0
    //   real target: 12 declared with code, 2 not-declared-here, 0 ACC_NATIVE
    //
    // Rule 4: `java.util.Comparator`'s implementations are pure Java with no
    // VM boundary to bridge to, and nothing in the corpus depends on these
    // shims answering.
    registry.with_category(
        cratonvm_native_api::NativeKind::SyntheticStub,
        register_comparator_natives,
    );
    register_string_joiner_natives(registry);
    register_random_natives(registry);
    register_optional_int_natives(registry);
    register_optional_long_natives(registry);
    register_optional_double_natives(registry);
    register_linked_list_natives(registry);
    register_linked_hashmap_natives(registry);
    register_array_deque_natives(registry);
    register_priority_queue_natives(registry);
    register_vector_natives(registry);
    register_stack_natives(registry);
    register_bulk_ops_natives(registry);
    register_queue_deque_interface_natives(registry);
    register_tree_map_natives(registry);
    register_tree_set_natives(registry);
    register_concurrent_hashmap_natives(registry);
    register_properties_natives(registry);
    register_collections_extras_natives(registry);
    // RETAGGED per subsystem — the largest single win available on the ambient
    // path, and the evidence is unusually direct.
    //
    // MEASURED 2026-08-19 (`--dump-native-registry`, default mode):
    //   registrations 300 | overwrote 2
    //   all 300 target ELEVEN `cratonvm/internal/Unmodifiable*` classes —
    //   VM-INTERNAL STAND-INS that no JDK declares. There is nothing to bridge
    //   to: a native registered against a class this VM invented is a
    //   compatibility shim by definition, which is exactly what this file's
    //   header says of the crate as a whole.
    //
    // And under `--jdk-only` they are already inert. Measured directly:
    //
    //   Collections.unmodifiableList(...).getClass().getName()
    //     HotSpot   java.util.Collections$UnmodifiableRandomAccessList
    //     CratonVM  java.util.Collections$UnmodifiableRandomAccessList
    //
    // — plus the same for `unmodifiableMap`, the delegating reads, and the
    // `UnsupportedOperationException` on mutation. Strict mode already answers
    // this surface from real bytecode and never reaches the stand-ins, so
    // dropping them removes dead weight rather than changing behaviour.
    //
    // This registrar is one of the twelve that INHERIT the ambient tag, so the
    // call-site wrapper takes effect; the other 48 override it (G85-1). The tag
    // move is asserted from a dump, not inferred from green arms.
    registry.with_category(
        cratonvm_native_api::NativeKind::SyntheticStub,
        register_unmodifiable_natives,
    );
    register_set_from_map_natives(registry);
    // BlockingQueue family (LinkedBlockingQueue, ArrayBlockingQueue,
    // ConcurrentLinkedQueue, ConcurrentLinkedDeque) is overridden with a
    // synthetic 4-field layout (head/tail/size/capacity) that conflicts with
    // the real JDK's field layout (head/last/count/putLock/takeLock/notEmpty/
    // notFull/capacity for LBQ).  In real-JDK mode the synthetic <init> never
    // assigns putLock/takeLock, so later real bytecode (e.g. drainTo line 706
    // in JDK 25) NPEs on `takeLock.lock()`.  Gate behind synthetic-jdk only.
    #[cfg(feature = "synthetic-jdk")]
    register_blocking_queue_natives(registry);
    #[cfg(not(feature = "synthetic-jdk"))]
    register_linked_blocking_deque_stub_natives(registry);
    register_iterator_protocol_natives(registry);
    // ScheduledThreadPoolExecutor.schedule is implemented in
    // native-builtins `register_p63_scheduled_executor` (scheduled_pump +
    // delay-aware firing). The previous inline `native_stpe_schedule` ran
    // every runnable immediately, which breaks Surefire ForkedBooter.exit1
    // (it schedules kill() as a delayed backup; immediate kill NPEs on a
    // null CommandReader when setupBooter failed first).
    // ConcurrentSkipListMap: the native "sorted array" impl is BROKEN in
    // real-JDK mode — it stores state in synthetic object slots 0/1/2 that
    // don't exist in the real CSLM layout, AND it ignores a custom Comparator
    // (binary search is hardcoded to natural ordering), AND it never intercepts
    // the `(Comparator)` constructor. A `new ConcurrentSkipListMap(cmp)` with
    // non-Comparable keys (e.g. `Class` keyed by a name-comparator, as Gradle's
    // DefaultSerializerRegistry does) silently drops every put → size 0. Run the
    // real java.util.concurrent.ConcurrentSkipListMap bytecode instead (it works
    // under CratonVM's real java.u.c support — Unsafe/VarHandle CAS + the
    // comparator). This unblocks the Gradle test worker: its
    // WorkerLoggingSerializer registers LogEvent/StyledText/LogLevelChange
    // serializers into such a map; with the native dropping them, canSerialize
    // returned false → LogEvent fell back to (failing) Java serialization →
    // worker→daemon stream desync. (Disabled, not deleted; impl kept for
    // reference.)
    //
    // W7-15, 2026-08-07 — DO NOT DELETE THE REGISTRAR, and mind the correction
    // below. The `let _ =` is a dead-code silencer, so nothing here registers;
    // the sibling copy is
    // `native-builtins/src/util_concurrent_ext.rs::register_t31_concurrent_extras`,
    // and it is NOT "already inert" — its CSLM block sits OUTSIDE that
    // function's `#[cfg(feature = "synthetic-jdk")]` LinkedTransferQueue block,
    // so it registers whenever it is reached; what gates it is its caller
    // (`register_synthetic_overrides` <- `register_builtins`), i.e. it is live
    // in synthetic-jdk and absent from the default real-JDK build. It covers
    // twelve triples — `<init>()V`, `put`, `get`, `remove`, `size`, `isEmpty`,
    // `containsKey`, `firstKey`, `lastKey`, `subMap`, `headMap`, `tailMap` —
    // and the symmetric difference is NOT one-sided: TWO triples registered
    // here exist nowhere else in the tree, `<init>(Ljava/util/Comparator;)V`
    // and `keySet()Ljava/util/Set;`. Deleting this registrar would therefore
    // drop the only record of them, and the `__test_cslm_*` hooks below drive
    // these natives from `tests/gc_side_table_root_audit.rs`.
    let _ = register_concurrent_skip_list_map_natives;
    // StampedLock: DISABLED 2026-07-28 — this crate's copy silently destroyed
    // mutual exclusion, and it WON, because `register_collections_natives`
    // runs after `register_builtins` (last-registration-wins) and so
    // overwrote `native-builtins`' real `parking_lot`-backed implementation.
    //
    // Two independent defects, both measured against HotSpot 25:
    //
    //  1. `native_sl_write_lock` spins at most `SL_SPIN_LIMIT` (1000) yields
    //     and then GIVES UP, returning stamp 0 — while the caller believes it
    //     holds the lock. `writeLock()` must block until granted; returning 0
    //     is `tryWriteLock`'s failure contract, not `writeLock`'s.
    //  2. It keeps its state in `set_field(this, SL_FIELD_STATE, Value::Int)`
    //     BY INDEX. In real-JDK mode index 0 of a real `StampedLock` is
    //     `state`, a *long*, so the write is coerced and the read-back
    //     `match { Value::Int(v) => v, _ => 0 }` always falls to 0 — the lock
    //     reads as permanently unlocked, the spin loop never waits, and every
    //     thread enters.
    //
    // Measured with 4 threads incrementing under `writeLock()` with a yield
    // inside the critical section (max threads observed inside; HotSpot = 1):
    //     default real-JDK mode : 335 threads inside, 345 lost updates
    //     --synthetic-jdk       :   2 threads inside,   1 lost update
    //
    // `native-builtins`' implementation is layout-independent (state lives in
    // a side table keyed by a GC-stable identity key, and it mirrors `state`
    // BY NAME), genuinely parks on a `Condvar` instead of spinning, and
    // provides a strictly larger surface (`tryUnlockRead`/`tryUnlockWrite`,
    // `tryConvertTo*Lock`, `getReadLockCount`, plus `unlock(J)V` added
    // alongside this change).
    //
    // REGISTRAR DELETED 2026-08-07 (W7-15). What stood here was
    // `let _ = register_stamped_lock_natives;` — a dead-code silencer, not a
    // call: the registrar it named ran in NO mode and NO feature build, so its
    // eleven `r.register` lines read as coverage while registering nothing.
    // All eleven triples (`<init>()V`, `readLock`/`writeLock`/`tryReadLock`/
    // `tryWriteLock`/`tryOptimisticRead`, `unlockRead`/`unlockWrite`,
    // `validate`, `isReadLocked`/`isWriteLocked`) are a strict subset of the
    // twenty-five registered by
    // `native-builtins/src/util_concurrent_ext.rs::register_stamped_lock_natives`,
    // which runs on EVERY boot (twice inside
    // `register_essential_natives_with_shims`, once more directly from
    // `vm_init`). The natives themselves are still in this file, below the
    // `StampedLock` banner, unreachable and marked as such — do not re-register
    // them here.
    // Phaser: DISABLED 2026-08-07 — same shape as the StampedLock call above,
    // and for the same two reasons. It used to be
    // `#[cfg(feature = "synthetic-jdk")] register_phaser_natives(registry);`
    // (gap-phaser-real-bytecode-state.md), which was not enough:
    //
    //  1. SPLIT-BRAIN under synthetic-jdk. `native-builtins`
    //     `phases_early::register_phaser_natives` registers TWELVE triples via
    //     `register_synthetic_overrides` -> `register_phase51_natives`; this
    //     copy registers NINE of the same twelve and, running later
    //     (`register_builtins` then `register_collections_natives` in
    //     `vm_init`), WON them. The two implementations do not share state:
    //     this one keeps parties/arrived/phase in object slots 0/1/2, the
    //     `native-builtins` one keeps an `int[3]` holder in slot 1. So the
    //     three triples this copy does NOT register (`getUnarrivedParties`,
    //     `isTerminated`, `forceTermination`) read a holder that `arrive()`
    //     never updates — and worse, `ph_holder` finds a non-Object in slot 1,
    //     takes its migration path, and OVERWRITES slot 1 with the new array,
    //     destroying the `arrived` count this copy's `arrive()` had been
    //     keeping there. One `getUnarrivedParties()` call corrupts the phaser.
    //     A partial surface is the failure mode here, exactly as with
    //     StampedLock; the twelve-triple holder-based implementation is a
    //     strict superset of these nine and serves the whole surface alone.
    //  2. The `cfg` gate was a BUILD-time gate standing in for a MODE
    //     question. `use_synthetic_jdk` is runtime config: a synthetic-jdk
    //     *feature* build running real-JDK *mode* takes the `vm_init` arm that
    //     skips `register_builtins` but still calls
    //     `register_collections_natives` — so the cfg was satisfied, this copy
    //     registered, and the synthetic 3-int layout shadowed real `Phaser`
    //     bytecode over the real state(0,J)/parent(1,L)/root(2,L)/evenQ/oddQ
    //     layout anyway. That is the very regression the cfg was added to fix.
    //
    // Dropping the call site fixes both: under synthetic-jdk the
    // `native-builtins` holder implementation serves all twelve triples
    // (the fabricated `Phaser` gets 3 `Ljava/lang/Object;` slots, so the
    // holder reference in slot 1 is type-correct there), and in real-JDK mode
    // Phaser has no native shadow at all and the real bytecode runs. The
    // default build (no `synthetic-jdk`) is unchanged: neither copy ran there.
    // REGISTRAR DELETED 2026-08-07 (W7-15). What stood here was
    // `let _ = register_phaser_natives;` — a dead-code silencer, not a call, so
    // this copy's nine `r.register` lines read as coverage while registering
    // nothing in any mode or feature build. No synthetic method is lost: the
    // nine triples (`<init>()V`, `<init>(I)V`, `register`, `arrive`,
    // `arriveAndAwaitAdvance`, `arriveAndDeregister`, `getPhase`,
    // `getRegisteredParties`, `getArrivedParties`) are a strict SUBSET of the
    // twelve registered by
    // `native-builtins/src/phases_early.rs::register_phaser_natives`, which
    // adds the three this copy never had (`getUnarrivedParties`,
    // `isTerminated`, `forceTermination`) — the symmetric difference is
    // one-sided, so there is nothing here the live registrar does not serve.
    // That registrar is live in synthetic-jdk mode
    // (`register_phase51_natives` <- `register_synthetic_overrides` <-
    // `register_builtins`); in real-JDK mode `Phaser` deliberately has no
    // native shadow at all. The natives themselves are still in this file,
    // below the `Phaser` banner, unreachable and marked as such — do not
    // re-register them here.
    register_priority_blocking_queue_natives(registry);
    // ScheduledThreadPoolExecutor natives use a synthetic 3-field layout
    // (poolSize=0, shutdown=1, taskList=2). On a REAL STPE that maps onto
    // ctl(0)/workQueue(1)/mainLock(2): `native_stpe_init` writes Int into the
    // reference fields ctl/workQueue (coerced to null) and an `Object[16]`
    // into `mainLock`, so the inherited real `ThreadPoolExecutor.shutdownNow()`
    // (which f157de8a switched to real worker-reaping bytecode) NPEs on
    // `mainLock.lock()` in every `TomcatBaseTest.tearDown`. Gate behind
    // synthetic-jdk only so the self-contained real STPE bytecode runs in
    // real-JDK mode (same fix pattern as register_blocking_queue_natives /
    // register_phaser_natives above). See
    // 11-stpe-mainlock-npe-teardown-regression.md.
    #[cfg(feature = "synthetic-jdk")]
    register_executors_scheduled_natives(registry);
    register_concurrent_completeness_natives(registry);
    registry.set_category(__prev_cat);
}

// ===========================================================================
// Helper: allocate a synthetic object with a well-known class name
// ===========================================================================
//
// An entire second `make_iterator_from_array` used to sit on the line above,
// collapsed onto ONE line behind a `///` by some batch edit, so the whole
// definition was silently a doc comment. It described the pre-2026-08-06
// behaviour (mint `java/util/Iterator` and fill two slots) that the live
// implementation above deliberately no longer has, and it would have become a
// duplicate definition the moment anyone "fixed" the missing newline.
// Deleted 2026-08-06.

// The infallible `alloc_synthetic` twin is DELETED (JDK-only wave 2, step 3,
// 2026-08-10). It reached `ensure_synthetic_class` — the entry point step 3
// removes — and its last caller, `native_ksv_iterator`, moved to the fallible
// spelling with the trade stated at that site. There is no infallible spelling
// in this crate any more, which is what makes the grep gate meaningful.

/// Allocate a synthetic object, trying to load the real class first, with the
/// refusal `--jdk-only` requires.
///
/// The deleted infallible twin reached `ensure_synthetic_class`, which under
/// [`CompatibilityMode::JdkOnly`] recorded a `CompatibilityClassRequested`
/// violation and then fabricated anyway, because its signature had no error
/// channel. This one goes through `try_ensure_synthetic_class`, so the policy's
/// refusal actually reaches the caller as a `NoClassDefFoundError` naming the
/// class — which for a shape like `java/util/HashMap$KeyItr` is the honest
/// answer: no class file with that name exists in any JDK, so a strict run that
/// gets one is running a synthetic collection iterator in place of the real
/// bytecode.
///
/// Under the default `Compatible` mode this is byte-for-byte what the deleted
/// twin did.
///
/// Every native returning `MethodCallResult` should prefer this spelling;
/// `ClassIdentityError` converts with `?`.
///
/// It keeps the twin's two real-class preferences, and the reason for each:
/// when `<clinit>` fails (a class mid-initialization on a parent frame) a
/// name-only lookup comes before any fabrication, because degrading to bare
/// `Object` produced objects whose `class_id_of` reported `java/lang/Object`
/// and surfaced downstream as a swallowed `NoSuchMethodError Object.iterator()`;
/// and a success that resolves to a DIFFERENT name is re-asked by name, because
/// some class-loading fallbacks report success with the legacy
/// `java/lang/Object` id and a named synthetic helper must keep its requested
/// identity or its field writes land on Object's zero-slot layout.
///
/// [`CompatibilityMode::JdkOnly`]: cratonvm_types::compat::CompatibilityMode
#[track_caller]
fn try_alloc_synthetic(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    num_fields: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    // Same real-class preference, and the same two fallbacks, as the
    // infallible spelling above. Only the final "no class of this name exists
    // anywhere" arm differs: it asks the policy instead of overriding it.
    //
    // `refusal_to_java_failure`, not the `?` conversion: the latter yields
    // `MethodCallFailed::InternalError`, which is uncatchable and aborts the
    // run. A policy refusal has to arrive as the `NoClassDefFoundError` the
    // contract names, so the program can see and handle it.
    let cid = match ctx.ensure_class_initialized(class_name) {
        Ok(class_id) => {
            let resolved_name = ctx.class_name_of_id(class_id).unwrap_or_default();
            if resolved_name == class_name || class_name == "java/lang/Object" {
                class_id
            } else {
                match ctx.class_id_by_name(class_name) {
                    Some(id) => id,
                    None => refused_class(ctx, class_name, num_fields)?,
                }
            }
        }
        Err(_) => match ctx.class_id_by_name(class_name) {
            Some(id) => id,
            None => refused_class(ctx, class_name, num_fields)?,
        },
    };
    // JVMS 6.5 census (nio21 close): this funnel mints the stream and iterator
    // carriers; `observe_uninstantiable_receiver` is `#[track_caller]` and so is
    // this function, so the row names the native that asked.
    let _ = cratonvm_native_api::instantiable::observe_uninstantiable_receiver(
        class_name,
        ctx.class_access_flags(cid),
    );
    Ok(ctx.alloc_object(cid, num_fields))
}

/// `try_ensure_synthetic_class`, with the refusal converted to a catchable
/// Java throwable. Shared by [`try_alloc_synthetic`]'s two arms.
#[track_caller]
fn refused_class(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    num_fields: usize,
) -> Result<ClassId, MethodCallFailed> {
    match ctx.try_ensure_synthetic_class(class_name, num_fields) {
        Ok(id) => Ok(id),
        Err(err) => Err(cratonvm_native_api::refusal_to_java_failure(ctx, err)),
    }
}

/// [`try_alloc_synthetic`] at the width the LOADED class declares, with
/// `synthetic_width` used only when the class cannot be resolved at all.
///
/// For the collections in this file whose whole state lives in an
/// address-keyed side table — `ts_get_slot`/`ts_set_slot` and their TreeMap
/// twins never touch the object's own fields, and say so — the private slot
/// count is not a layout. Passing it to the allocator only widens the object
/// past what the class declares: real `java.util.TreeSet` declares ONE field
/// (`javap -p` against JDK 25.0.3.9: `private transient NavigableMap m`), so
/// asking for `TS_NUM_FIELDS` put two slots past the declared width that
/// nothing in the workspace ever writes or reads. See
/// W7-66-live-over-allocations.md for the census and
/// W7-59-layout-detector-coverage.md for the instrument that found it.
///
/// In synthetic-JDK mode this is a no-op **by construction**, not by argument:
/// `class_manager` fabricates `java/util/TreeSet` with three instance fields,
/// so the declared width already IS the synthetic width, and
/// `NativeContext::alloc_object` clamps any request UP to the declared count
/// regardless. The `synthetic_width` fallback covers only the "no class of
/// this name anywhere" arm that [`try_alloc_synthetic`] itself handles.
///
/// This is deliberately NOT `try_alloc_with_appended_slots`' shape: that idiom
/// is for natives carrying private slots ON the object, which these do not.
#[track_caller]
fn try_alloc_declared_width(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    synthetic_width: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let declared = match ctx.ensure_class_initialized(class_name) {
        Ok(cid) => ctx.class_num_total_fields(cid),
        Err(_) => 0,
    };
    let width = if declared > 0 {
        declared
    } else {
        synthetic_width
    };
    try_alloc_synthetic(ctx, class_name, width)
}

/// Allocate a *real* JDK object of `class_name` with its natural field count and
/// no constructor run — the caller sets the fields it needs by name. Used to
/// produce real instances (e.g. `Collections$SingletonList`) whose methods then
/// execute real JDK bytecode. Returns `None` if the class is unavailable so the
/// caller can fall back to a synthetic stand-in.
fn alloc_real_jdk(ctx: &mut dyn NativeContext, class_name: &str) -> Option<ObjectRef> {
    let cid = ctx.ensure_class_initialized(class_name).ok()?;
    let n = ctx.class_num_total_fields(cid);
    Some(ctx.alloc_object(cid, n))
}

/// Allocate a reference array (Object[]) of the given length.
fn alloc_ref_array(ctx: &mut dyn NativeContext, length: usize) -> ObjectRef {
    ctx.new_ref_array(ClassId::new(0), length)
}

fn alloc_ref_array_or_oom(
    ctx: &mut dyn NativeContext,
    length: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    ctx.try_new_ref_array(ClassId::new(0), length)
        .ok_or_else(|| {
            RuntimeError::OutOfMemoryError {
                message: "Java heap space".to_string(),
            }
            .into()
        })
}

/// Whether a caller-sized growth allocation (`ArrayList.grow`, a `HashMap`
/// resize) may collect before it gives up.
///
/// gc-common w9-d (`common-w8b-native-factories-still-single-attempt`). A
/// reclaim is a real, moving collection, so it is legal only when EVERY
/// `ObjectRef` still in use afterwards -- in this native AND in every Rust
/// frame above it -- is pinned. The growth helpers cannot see their callers,
/// so the caller says which case it is:
///
/// * [`GrowAlloc::Reclaim`] -- reached only as a REGISTERED native, i.e. from
///   Java dispatch or `invoke_virtual`. Both are already GC points for
///   whatever called them (the target may be arbitrary bytecode), so no Rust
///   frame above holds a bare reference across this call by contract. The
///   `pub` `*_entry_pub` twins (`native_map_put_entry_pub`,
///   `native_al_add_entry_pub`, `native_al_add_at_entry_pub`,
///   `native_al_to_array_entry_pub`) carry it to another crate's registration
///   or to a Rust caller that pins and re-reads everything it holds.
/// * [`GrowAlloc::SingleAttempt`] -- the historical path, for the `pub`
///   natives other crates call as plain Rust functions while holding bare
///   references (after gc-common w11-c, only `native_es_add` in
///   `native-builtins`) and for internal callers that keep bare locals across
///   the call (`native_stack_push` returns its pre-call `elem`). Byte-for-byte
///   what these sites did before.
///
/// Explicit rather than ambient on purpose: a thread-local "may collect" mode
/// would be inherited by whatever the entry calls, which is the silent-default
/// failure `AGENTS.md` records for `NativeKind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GrowAlloc {
    SingleAttempt,
    Reclaim,
}

/// HotSpot's `SOFT_MAX_ARRAY_LENGTH` (`Integer.MAX_VALUE - 8`). A request
/// above it can never be served, so the reclaim ladder does not collect for
/// it: HotSpot throws "Requested array size exceeds VM limit" without a GC.
const RECLAIM_MAX_ARRAY_LENGTH: usize = (i32::MAX as usize) - 8;

/// A reference array of `length` with one reclaim-and-retry: try, and on a
/// refusal pin `live`, run `reclaim_before_alloc_retry`, write each `live`
/// entry's post-GC address back in place, unpin, and try once more. `None`
/// when both attempts are refused (or the reclaim declined); the CALLER picks
/// the failure it reports, so each site keeps its historical exception.
///
/// The twin of `native-builtins`' `new_ref_array_reclaiming` (that one is
/// `pub(crate)` in a crate this one cannot depend on). The precondition is the
/// same: **every `ObjectRef` the caller still uses afterwards is in `live` or
/// already pinned and re-read from its pin**, and the caller was reached as a
/// registered native (see [`GrowAlloc`]).
fn try_ref_array_reclaiming(
    ctx: &mut dyn NativeContext,
    component: ClassId,
    length: usize,
    live: &mut [ObjectRef],
) -> Option<ObjectRef> {
    if let Some(arr) = ctx.try_new_ref_array(component, length) {
        return Some(arr);
    }
    if length > RECLAIM_MAX_ARRAY_LENGTH {
        return None;
    }
    let mut roots: Vec<&mut ObjectRef> = live.iter_mut().collect();
    let retry = rooted_across(ctx, &mut roots, |ctx| ctx.reclaim_before_alloc_retry());
    if retry {
        ctx.try_new_ref_array(component, length)
    } else {
        None
    }
}

/// A caller-sized reference array under `grow`'s policy, for a site whose
/// every live reference is ALREADY pinned (by the caller's own pins or an
/// enclosing [`rooted_across`]) and re-read after this call.
///
/// * [`GrowAlloc::SingleAttempt`]: `ctx.new_ref_array`, byte-for-byte the
///   historical call.
/// * [`GrowAlloc::Reclaim`]: [`try_ref_array_reclaiming`] with an empty
///   `live`, then, when both attempts are refused, the same historical
///   allocator, so exhaustion still surfaces exactly as it did.
///
/// gc-common w10-g (`common-w8b-native-factories-still-single-attempt`,
/// residue item 2): the `toArray` family, `trimToSize`, `Hashtable(int)` and
/// the `LinkedHashMap` table. `ClassId::new(0)` is [`alloc_ref_array`]'s
/// `Object[]` sentinel.
fn new_ref_array_grow(
    ctx: &mut dyn NativeContext,
    component: ClassId,
    length: usize,
    grow: GrowAlloc,
) -> ObjectRef {
    if grow == GrowAlloc::Reclaim {
        if let Some(arr) = try_ref_array_reclaiming(ctx, component, length, &mut []) {
            return arr;
        }
    }
    ctx.new_ref_array(component, length)
}

/// GC-SAFETY helper: pin every object-typed `Value` in `vals` so a subsequent
/// allocating call cannot relocate/collect them while they sit in a bare Rust
/// `Vec`. Returns the base handle for [`NativeContext::unpin_native_roots`]
/// (`usize::MAX` if nothing was pinned) and a per-index handle vector
/// (`usize::MAX` for non-object slots) for [`read_value_slice`]. Pair every call
/// with `unpin_native_roots(base)` once the refs are safely stored.
fn pin_value_slice(ctx: &mut dyn NativeContext, vals: &[Value]) -> (usize, Vec<usize>) {
    let mut base = usize::MAX;
    let handles = vals
        .iter()
        .map(|v| match v {
            Value::Object(Some(o)) => {
                let h = ctx.pin_native_root(*o);
                if base == usize::MAX {
                    base = h;
                }
                h
            }
            _ => usize::MAX,
        })
        .collect();
    (base, handles)
}

#[inline]
fn pin_value(ctx: &mut dyn NativeContext, v: Value) -> usize {
    match v {
        Value::Object(Some(o)) => ctx.pin_native_root(o),
        _ => usize::MAX,
    }
}

/// `r`, with the pin group from `base` onward released first when `r` is an
/// `Err` -- for a `?` inside a pinned window, which would otherwise return
/// with the group still up (holding its objects live until the caller's pin
/// floor). `base == usize::MAX` (nothing was pinned) releases nothing. Bind
/// the GC-capable call first: `let m = f(ctx, ..); x = unpin_on_err(ctx,
/// base, m)?;`. Round 15 wave 3 (lane compat3).
#[inline]
fn unpin_on_err<T>(
    ctx: &mut dyn NativeContext,
    base: usize,
    r: Result<T, MethodCallFailed>,
) -> Result<T, MethodCallFailed> {
    if r.is_err() && base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    r
}

/// Pin an `Option<ObjectRef>` cursor, returning `usize::MAX` for `None`.
/// Companion to [`opt_read`]; both exist for `map_resize_inner`'s chain-split
/// walk, which carries four such cursors across a GC-capable reference store.
#[inline]
fn opt_pin(ctx: &mut dyn NativeContext, o: Option<ObjectRef>) -> usize {
    match o {
        Some(r) => ctx.pin_native_root(r),
        None => usize::MAX,
    }
}

/// Read back an `Option<ObjectRef>` cursor pinned with [`opt_pin`]. `None`
/// cursors and unpinned handles are returned verbatim.
#[inline]
fn opt_read(ctx: &dyn NativeContext, handle: usize, o: Option<ObjectRef>) -> Option<ObjectRef> {
    match o {
        Some(r) if handle != usize::MAX => Some(ctx.read_native_pin(handle, r)),
        _ => o,
    }
}

/// Read back the (post-GC, forwarded) refs for a slice pinned with
/// [`pin_value_slice`]. Non-object slots are returned verbatim.
fn read_value_slice(ctx: &dyn NativeContext, handles: &[usize], orig: &[Value]) -> Vec<Value> {
    handles
        .iter()
        .zip(orig)
        .map(|(&h, v)| match v {
            Value::Object(Some(o)) if h != usize::MAX => {
                Value::Object(Some(ctx.read_native_pin(h, *o)))
            }
            _ => *v,
        })
        .collect()
}

/// Read back ONE (post-GC, forwarded) ref pinned with [`pin_value_slice`].
/// Non-object slots / unpinned handles are returned verbatim. Use in a
/// per-element dispatch loop to refresh `elements[i]` just before passing it to
/// an allocating `invoke_virtual` (the moving young collector relocates the
/// object and remaps its native pin, but the bare `Vec` slot is not rewritten).
#[inline]
fn read_pinned_elem(ctx: &dyn NativeContext, handle: usize, orig: Value) -> Value {
    match orig {
        Value::Object(Some(o)) if handle != usize::MAX => {
            Value::Object(Some(ctx.read_native_pin(handle, o)))
        }
        _ => orig,
    }
}

/// GC-SAFETY idiom: run `body` — which may allocate, dispatch Java, or block
/// in a GC-safe wait, and can therefore complete a moving young collection —
/// while `roots` stay rooted, then rewrite each one in place to its post-move
/// address.
///
/// This is the "Family 1" stale-`ObjectRef` bug in one place. A native that
/// captures a heap reference in a bare Rust local, calls something GC-capable,
/// and then reuses the local is writing through a pre-move address: the object
/// itself survives (the caller's `safe_native_call` pin keeps it alive and the
/// collector remaps *that* pin), but this function's private copy is never
/// rewritten. Worse, a reference reachable ONLY from such a local — a bucket
/// chain the walk has already unlinked, a table not yet published — is not a
/// root at all, and the collection reclaims it outright, leaving a zeroed
/// receiver behind (`num_slots=0 class_id=ClassId(0)`, the
/// `gen_heap::set_field: out-of-bounds field write dropped` guard).
///
/// Fixing that per site is what produced the long tail of one-off
/// `pin_native_root`/`read_native_pin` pairs elsewhere in this file, and each
/// pass missed some. Prefer this helper for new code:
///
/// ```ignore
/// let mut this = this;
/// let new_table = rooted_across(ctx, &mut [&mut this], |ctx| {
///     alloc_ref_array(ctx, new_cap)
/// });
/// // `this` is now the CURRENT address; `new_table` is post-GC by construction.
/// ```
///
/// The pin frame is taken above whatever the caller already holds and torn
/// down before returning, so this composes with an enclosing pin base.
fn rooted_across<T>(
    ctx: &mut dyn NativeContext,
    roots: &mut [&mut ObjectRef],
    body: impl FnOnce(&mut dyn NativeContext) -> T,
) -> T {
    if roots.is_empty() {
        return body(ctx);
    }
    // Pin in order; `handles[0]` is the frame base, and
    // `unpin_native_roots(base)` releases the base and everything above it.
    let mut handles: Vec<usize> = Vec::with_capacity(roots.len());
    for r in roots.iter() {
        handles.push(ctx.pin_native_root(**r));
    }
    let base = handles[0];
    let out = body(ctx);
    for (r, &h) in roots.iter_mut().zip(handles.iter()) {
        **r = ctx.read_native_pin(h, **r);
    }
    ctx.unpin_native_roots(base);
    out
}

/// [`rooted_across`] for the overwhelmingly common single-root case: keep
/// `this` rooted across `body` and return `(this_now, body_result)`.
#[inline]
fn rooted_across1<T>(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    body: impl FnOnce(&mut dyn NativeContext) -> T,
) -> (ObjectRef, T) {
    let mut this = this;
    let out = rooted_across(ctx, &mut [&mut this], body);
    (this, out)
}

/// Allocate a hash-map bucket table of `cap` entries, **capping the eager
/// allocation to what the heap can hold**, and return `(table, actual_cap)`.
///
/// HotSpot allocates the `HashMap`/`LinkedHashMap` table lazily on the first
/// insert, so `new HashMap(Integer.MAX_VALUE)` never allocates and never
/// throws. CratonVM's synthetic map allocates the bucket array eagerly in the
/// constructor, so an over-large `cap` (`tableSizeFor` caps at `1<<30` ⇒ ~8 GiB
/// of `Object[]`) would hit the panicking allocator and abort the VM. When the
/// requested table doesn't fit we start from the default and let the map grow
/// on demand — matching HotSpot's "constructor succeeds, no throw". The caller
/// must use the returned `actual_cap` for `__capacity`/`threshold` so they stay
/// consistent with the real table length.
///
/// `component` is the class the table is an array OF — see
/// [`bucket_table_component`], which is the only thing that should compute it.
/// `ClassId::new(0)` is the untyped sentinel and yields today's `Object[]`.
/// **Both arms below take it**: the reduced-capacity fallback used to spell its
/// allocation `alloc_ref_array`, which hard-codes the sentinel, so a map that
/// lost the capacity race would have silently kept an `Object[]` while every
/// other map got a typed one — a divergence visible only under memory pressure,
/// which is the worst place to find one.
fn alloc_bucket_table(
    ctx: &mut dyn NativeContext,
    component: ClassId,
    cap: usize,
) -> (ObjectRef, usize) {
    match ctx.try_new_ref_array(component, cap) {
        Some(table) => (table, cap),
        None => {
            let small = MAP_DEFAULT_CAPACITY;
            (ctx.new_ref_array(component, small), small)
        }
    }
}

// ---------------------------------------------------------------------------
// Per-segment resize lock (Bug 1+2 CRIT correctness fix, round-10)
// ---------------------------------------------------------------------------
//
// Lock-free CHM readers walk old bucket chains and follow NEXT pointers via
// `get_field_volatile`. When `map_resize` mutates a chain's NEXT pointers in
// place to splice the hi/lo partitions, a concurrent reader can skip past
// the keys living in the hi partition and return a spurious miss.
//
// Round-10 fix (Bug 1+2 CRIT, round-9 native-misc CRIT-1/CRIT-2):
// The earlier implementation looked up a per-segment RwLock through a global
// `Mutex<HashMap<ptr, Arc<RwLock>>>` — every CHM read AND write paid the
// global Mutex, completely defeating the per-segment design. The map was
// also keyed by raw segment heap pointer, which is GC-relocatable, so a
// moving GC could cause cross-segment aliasing (two distinct segments share
// a lock if their heap addresses overlap after collection).
//
// Replacement: a fixed-size striped array of 256 `parking_lot::RwLock`s
// (workspace-shared `parking_lot` dep). The stripe index is derived from
// the segment's identity hash code, which is stable across GC moves —
// previously the global HashMap was keyed by raw heap pointer, which a
// moving GC could relocate and alias across distinct segments. No global
// lock; lookup is a single masked array index, with a Fibonacci
// multiplier to spread sequential identity hashes uniformly.
//
// Trade-off: occasional false sharing if two segments hash to the same
// stripe (1/256 collision rate). That serializes resizes between unrelated
// segments — acceptable since resize is rare. Reads stay fully concurrent
// because the read lock is non-exclusive.
//
// TODO(round-6+): replace the RwLock with a clone-resize implementation that
// allocates new Node objects via `alloc_node` and links the new sub-chains
// without mutating the old chain. The old chain is then unreachable from the
// new buckets array and becomes GC-collectible. That restores fully
// lock-free reads and eliminates the need for striped locks entirely.

const NUM_SEG_LOCKS: usize = 256;

/// Striped per-segment resize lock array.
///
/// Round-10 CRIT fix (native-misc CRIT-1 + concurrency CRIT-2):
/// the original implementation kept a `Mutex<FxHashMap<id, Arc<RwLock>>>`
/// where every CHM read AND write took the global Mutex just to look up
/// its per-segment RwLock. That defeated the per-segment design entirely
/// — worse than a single global RwLock, because reads serialized through
/// a Mutex (exclusive) instead of an RwLock::read (shared).
///
/// Round-10 replacement: a fixed-size 256-stripe array indexed by the
/// segment's identity hash. No global lock, lookup is one masked array
/// index. False-sharing risk is 1/256 between unrelated segments — only
/// matters during resize, which is rare.
///
/// `parking_lot::RwLock` is preferred over `std::sync::RwLock` because:
///   - no poison-Result handling (lock acquisition is infallible),
///   - smaller (single-word) and faster fast path,
///   - guarantees writer fairness on contention, avoiding reader-starve
///     bursts during resize storms.
///
/// Lock ordering / recursion: this RwLock is only acquired in
/// `map_resize` (write side) and `chm_seg_get` (read side). All other
/// CHM mutator paths (put / put_if_absent / compute_* / merge / replace)
/// serialize against each other via the per-segment Java monitor
/// (`ChmMonitorGuard::acquire(ctx, seg)`), NOT this RwLock. Therefore
/// the only nesting that occurs is:
///   mutator-with-monitor → map_resize → write_guard
/// which never recurses because `map_resize` does not invoke user code.
/// The blocking read side is reached only by `chm_seg_get`. It enters the VM's
/// native blocking protocol while waiting, so a reader parked behind a resize
/// is excluded from the stop-the-world census and cannot deadlock a collection
/// requested while the writer owns the stripe. The JIT fast paths use
/// non-blocking `try_read` retries and remain lock-free while the stripe is
/// busy.
static SEG_LOCKS: std::sync::OnceLock<[parking_lot::RwLock<()>; NUM_SEG_LOCKS]> =
    std::sync::OnceLock::new();

fn seg_locks() -> &'static [parking_lot::RwLock<()>; NUM_SEG_LOCKS] {
    SEG_LOCKS.get_or_init(|| std::array::from_fn(|_| parking_lot::RwLock::new(())))
}

fn chm_seg_lock_for(seg_id: i32) -> &'static parking_lot::RwLock<()> {
    // Multiply by the 64-bit Fibonacci constant (golden-ratio derived)
    // to spread sequential / low-entropy `identity_hash_code` values
    // across all 256 stripes; identity hashes are often assigned
    // sequentially by the GC so a raw mod would cluster nearby segments
    // on a handful of stripes. The top byte of the product is well
    // mixed and is masked with `NUM_SEG_LOCKS - 1` (256 is a power of
    // two, so the mask is exact).
    let h = (seg_id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    &seg_locks()[((h >> 56) as usize) & (NUM_SEG_LOCKS - 1)]
}

static SEG_RESIZE_EPOCHS: std::sync::OnceLock<[std::sync::atomic::AtomicU64; NUM_SEG_LOCKS]> =
    std::sync::OnceLock::new();

fn chm_seg_resize_epoch(seg_id: i32) -> &'static std::sync::atomic::AtomicU64 {
    let h = (seg_id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    &SEG_RESIZE_EPOCHS.get_or_init(|| std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)))
        [((h >> 56) as usize) & (NUM_SEG_LOCKS - 1)]
}

// Validity generation for the per-thread CHM String-node memo. Every CHM
// mutator holds a `ChmMonitorGuard`, which bumps both striped counters below:
// `SEG_MUTATION_ACTIVE` for as long as it may be altering nodes, and
// `SEG_MUTATION_EPOCHS` once as it leaves. Deliberately separate from the
// resize epoch, because a resize nests inside a mutator.
//
// A reader may memoize a node only if it saw `active == 0` both before and
// after its chain walk AND the epoch did not move across it; validating a memo
// hit later re-checks the same pair against the recorded epoch. Any mutator
// whose window overlaps the walk is caught by one of those three observations,
// because the epoch is bumped after the last node write and the in-flight
// count is dropped after that.
//
// This is a counter plus an in-flight count rather than the usual single
// odd/even parity word because a parity word needs the writers sharing one
// stripe serialized against each other, and the only place to serialize them
// is around `monitor_enter` — i.e. a host mutex held across a GC-safepoint
// park. A thread blocked on such a mutex is not at a safepoint, so it would
// stall every collection queued behind it.
static SEG_MUTATION_EPOCHS: std::sync::OnceLock<[std::sync::atomic::AtomicU64; NUM_SEG_LOCKS]> =
    std::sync::OnceLock::new();
static SEG_MUTATION_ACTIVE: std::sync::OnceLock<[std::sync::atomic::AtomicU64; NUM_SEG_LOCKS]> =
    std::sync::OnceLock::new();

fn chm_seg_mutation_epoch(seg_id: i32) -> &'static std::sync::atomic::AtomicU64 {
    let h = (seg_id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    &SEG_MUTATION_EPOCHS
        .get_or_init(|| std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)))
        [((h >> 56) as usize) & (NUM_SEG_LOCKS - 1)]
}

fn chm_seg_mutation_active(seg_id: i32) -> &'static std::sync::atomic::AtomicU64 {
    let h = (seg_id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    &SEG_MUTATION_ACTIVE
        .get_or_init(|| std::array::from_fn(|_| std::sync::atomic::AtomicU64::new(0)))
        [((h >> 56) as usize) & (NUM_SEG_LOCKS - 1)]
}

/// `(epoch, in-flight mutators)` for the stripe owning `seg_id`. The in-flight
/// count is read first so a snapshot can never pair an already-advanced epoch
/// with a stale-low count.
fn chm_seg_mutation_snapshot(seg_id: i32) -> (u64, u64) {
    let active = chm_seg_mutation_active(seg_id).load(std::sync::atomic::Ordering::Acquire);
    let epoch = chm_seg_mutation_epoch(seg_id).load(std::sync::atomic::Ordering::Acquire);
    (epoch, active)
}

struct ChmSegmentResizeGuard {
    epoch: &'static std::sync::atomic::AtomicU64,
    lock: Option<parking_lot::RwLockWriteGuard<'static, ()>>,
}

impl ChmSegmentResizeGuard {
    /// Acquire the write side without leaving a contending native thread in
    /// the GC's counted-mutator set. A resize deliberately holds this guard
    /// across allocations and reference stores, either of which can request a
    /// collection. A reader or competing writer waiting on this host lock must
    /// therefore use the native blocking protocol.
    ///
    /// `seg` is rooted across the wait because a completed moving collection
    /// may relocate it. The returned reference is the post-wait value and
    /// callers must discard their pre-wait copy.
    fn acquire_gc_safe(ctx: &mut dyn NativeContext, seg: ObjectRef) -> (Self, ObjectRef) {
        let seg_id = ctx.identity_hash_code(seg);
        let pin = ctx.pin_native_root(seg);
        chm_stripe_wait_begin(ctx);
        // `parking_lot::RwLock::write` is infallible (no PoisonError), so no
        // `unwrap_or_else` wrapper is needed.
        let lock = chm_seg_lock_for(seg_id).write();
        ctx.end_blocking_region();
        let seg = ctx.read_native_pin(pin, seg);
        ctx.unpin_native_roots(pin);
        // Take the stripe's write lock BEFORE marking the epoch odd, and
        // release it after marking it even again. Bumping first would let two
        // resizers sharing a stripe interleave their increments and leave the
        // epoch EVEN while one of them is still relinking — which is exactly
        // what an optimistic reader reads as "no resize in progress".
        let epoch = chm_seg_resize_epoch(seg_id);
        epoch.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        (
            Self {
                epoch,
                lock: Some(lock),
            },
            seg,
        )
    }
}

/// GC-safe acquisition for the blocking read side of a CHM resize stripe.
///
/// Callers must pin and refresh every raw `ObjectRef` they use after the wait.
/// Keeping that rule at the call site makes the references that cross a
/// possible moving collection explicit, while this helper ensures every
/// blocking `SEG_LOCKS` acquisition uses the same audited protocol.
fn chm_seg_read_gc_safe(
    ctx: &mut dyn NativeContext,
    seg_id: i32,
) -> parking_lot::RwLockReadGuard<'static, ()> {
    chm_stripe_wait_begin(ctx);
    // `parking_lot::RwLock::read` is infallible — no PoisonError to recover
    // from. The guard remains held after the thread is restored to its native
    // running state; only the wait itself is GC-blocked.
    let guard = chm_seg_lock_for(seg_id).read();
    ctx.end_blocking_region();
    guard
}

/// Open the GC-safe region around a wait for a CHM resize stripe's host
/// `RwLock` (interpreter round i2 wave 67, lane L1,
/// `CRATONVM_CLASSIFIED_STAND_IN_WAITS`, default on;
/// `docs/feature-designs/jdk-only-mode.md`).
/// Paired with exactly one `end_blocking_region`.
///
/// The stripe lock has no JDK counterpart: `ConcurrentHashMap.get` is
/// lock-free, and a JDK `transfer` never waits for readers to drain. So the
/// wait is not a Java wait and HotSpot's thread would read RUNNABLE (it would
/// be running the lock-free read or the bin copy); the region is
/// [`NativeContext::begin_vm_wait_region`], as the other waits HotSpot has no
/// Java state for (wave 66's `CRATONVM_VM_INTERNAL_WAITS_RUNNABLE`). Before,
/// the region read WAITING with no wait kind, the census's second and third
/// most frequent `--compatible` sites (15 regions on `L1W66UnrecordedWaitKinds`).
///
/// `=0`: the plain WAITING region it was (`BlockKind::Waiting`, which the VM's
/// unrecorded-wait census counts).
#[track_caller]
fn chm_stripe_wait_begin(ctx: &mut dyn NativeContext) {
    if chm_stripe_wait_classified() {
        ctx.begin_vm_wait_region();
    } else {
        ctx.begin_blocking_region_as(chm_stripe_wait_fallback_kind());
    }
}

/// `CRATONVM_CLASSIFIED_STAND_IN_WAITS` for [`chm_stripe_wait_begin`]: read
/// per wait (the region it guards costs a root deposit, which dwarfs a flag
/// lookup), so a per-thread test override is honoured.
fn chm_stripe_wait_classified() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_CLASSIFIED_STAND_IN_WAITS")
}

/// The kill switch's region kind for [`chm_stripe_wait_begin`]: the kind-less
/// WAITING region of before wave 67.
fn chm_stripe_wait_fallback_kind() -> cratonvm_native_api::BlockKind {
    cratonvm_native_api::BlockKind::Waiting
}

#[cfg(test)]
mod i67_chm_stripe_wait_tests {
    use super::{chm_stripe_wait_classified, chm_stripe_wait_fallback_kind};

    /// Interpreter round i2 wave 67, lane L1 (`CRATONVM_CLASSIFIED_STAND_IN_WAITS`):
    /// the stripe wait is a VM-internal wait by default; `=0` opens the
    /// kind-less WAITING region it was.
    #[test]
    fn the_stripe_wait_is_a_vm_wait_unless_switched_off() {
        assert!(chm_stripe_wait_classified());
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_CLASSIFIED_STAND_IN_WAITS", Some("0"))],
            || assert!(!chm_stripe_wait_classified()),
        );
        assert_eq!(
            chm_stripe_wait_fallback_kind(),
            cratonvm_native_api::BlockKind::Waiting
        );
    }
}

impl Drop for ChmSegmentResizeGuard {
    fn drop(&mut self) {
        self.epoch
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        self.lock.take();
    }
}

/// RAII guard for a native monitor (`ctx.monitor_enter` / `monitor_exit`).
///
/// CRIT fix (round-5): the previous CHM write path performed
///   `ctx.monitor_enter(seg); ... do work ...; ctx.monitor_exit(seg);`
/// directly. If the "do work" body returned `Err(..)` early via `?`, or
/// panicked (e.g. resize hit a guard-tripped chain corruption case), the
/// matching `monitor_exit` never ran and the segment stayed locked
/// forever — every subsequent thread blocking on that segment would
/// deadlock.
///
/// Using `ChmMonitorGuard` instead ensures `monitor_exit` runs on every
/// exit path, including unwinding. We store the context as a raw pointer
/// because Drop cannot hold the original `&mut dyn NativeContext` borrow
/// without conflicting with the body's use of the same reference. The
/// pointer is always live: the guard is bound to a local that cannot
/// outlive the borrow used to acquire it (the borrow is reborrowed
/// fresh for each call inside the function body).
///
/// If `monitor_exit` itself panics during unwind the process aborts —
/// preferable to silently leaking the monitor.
///
/// MED fix: a `PhantomData<&'a mut dyn NativeContext>` field carries a
/// lifetime parameter `'a` at the type level, so any struct that tries to
/// store the guard must also name `'a` (which a function-local borrow
/// cannot project to a caller scope). The transmute remains for runtime
/// storage — without it the guard would conflict with the body's reuse of
/// `ctx` — but the lifetime parameter prevents the previous
/// silently-`'static` escape hatch that would have let a refactor stash
/// the guard into a long-lived struct and miscompile catastrophically.
struct ChmMonitorGuard<'a> {
    ctx: *mut (dyn NativeContext + 'static),
    seg: ObjectRef,
    /// Native pin tracking the segment across moving collections while the
    /// guard is held. GC-SAFETY fix (2026-07-21): the guarded body routinely
    /// allocates (key hashCode/equals, map growth), so a moving GC can
    /// relocate the segment BETWEEN acquire and drop; exiting with the
    /// acquire-time address then misses the (remapped) monitor entirely and
    /// LEAKS it as owned-forever -- live-captured as the residual WildFly
    /// parallel-extension-add wedge (hunt2-a1: tid=29 owned a segment
    /// monitor with no CHM frame on its stack, parked on the capability
    /// write lock; every later toucher of that segment deadlocked). Drop
    /// re-reads the pin and exits the CURRENT address.
    pin: usize,
    /// Bumped once on the way out, after the last node write. See
    /// [`chm_seg_mutation_snapshot`].
    mutation_epoch: &'static std::sync::atomic::AtomicU64,
    /// Held up (`+1`) for the whole guarded section, so a reader can tell
    /// "a mutator is in flight on this stripe" from "a mutator has finished".
    mutation_active: &'static std::sync::atomic::AtomicU64,
    /// Phantom borrow tying the guard's lifetime parameter `'a` to a
    /// surrounding scope at the type level. Using
    /// `fn() -> &'a mut dyn NativeContext` rather than `&'a mut …` directly
    /// means no actual mutable borrow is held at runtime (so the body
    /// inside the guarded section can keep using `ctx` via reborrows, as
    /// before), but `'a` is still part of the guard's type and propagates
    /// into any container that tries to store the guard.
    _borrow: std::marker::PhantomData<fn() -> &'a mut dyn NativeContext>,
}

impl<'a> ChmMonitorGuard<'a> {
    fn acquire(ctx: &mut dyn NativeContext, seg: ObjectRef) -> Self {
        let pin = ctx.pin_native_root(seg);
        let seg_id = ctx.identity_hash_code(seg);
        let mutation_epoch = chm_seg_mutation_epoch(seg_id);
        let mutation_active = chm_seg_mutation_active(seg_id);
        mutation_active.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        ctx.monitor_enter(seg);
        // SAFETY: the guard MUST be dropped before the `&mut dyn NativeContext`
        // borrow ends. We transmute away the lifetime so the guard doesn't
        // hold the &mut borrow for its scope — call sites still use ctx
        // mutably between acquire and drop, which is sound as long as no
        // code outlives the original &mut borrow. The `PhantomData<fn() ->
        // &'a mut …>` field pins `'a` to a surrounding scope at the type
        // level without retaining the borrow at runtime, so attempts to
        // escape the guard into a longer-lived container fail to type-check.
        let ctx_ptr: *mut dyn NativeContext = ctx;
        let ctx_ptr_static: *mut (dyn NativeContext + 'static) =
            unsafe { core::mem::transmute(ctx_ptr) };
        ChmMonitorGuard {
            ctx: ctx_ptr_static,
            seg,
            pin,
            mutation_epoch,
            mutation_active,
            _borrow: std::marker::PhantomData,
        }
    }

    /// GC-safe variant of [`ChmMonitorGuard::acquire`]: a contended wait runs
    /// under the blocking-region protocol (`NativeContext::monitor_enter_gc_safe`
    /// → `monitor_enter_blocking`: TLAB retire, root-snapshot deposit,
    /// `GcBarrier::enter_blocked`, `check_post_block_gc` on wake), so a
    /// concurrent stop-the-world COMPLETES instead of deadlocking against a
    /// safepoint-parked segment owner. The plain `acquire` blocks as a counted
    /// mutator, which wedged the whole VM whenever the owner was already
    /// parked at the STW barrier — the WildFly `parallel-extension-add` boot
    /// hang (5/8 no-JIT boots: ~40 threads hammering the same management-
    /// registry CHM, gdb-confirmed three contenders inside `native_chm_put`
    /// → `acquire` → `monitor_enter` while the initiator spun at
    /// `rounds=64 pending=3 taken=0`).
    ///
    /// Because the wait can now span a completed moving GC, the segment may
    /// relocate: the guard unlocks the returned (current) address, and the
    /// caller MUST use the returned `ObjectRef` — and re-read every other raw
    /// `ObjectRef`/`Value` local it captured before this call through a pin
    /// (`pin_value`/`read_pinned_elem`) — for everything after this call.
    fn acquire_gc_safe(ctx: &mut dyn NativeContext, seg: ObjectRef) -> (Self, ObjectRef) {
        let pin = ctx.pin_native_root(seg);
        let seg_id = ctx.identity_hash_code(seg);
        let mutation_epoch = chm_seg_mutation_epoch(seg_id);
        let mutation_active = chm_seg_mutation_active(seg_id);
        mutation_active.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let fixed = ctx.monitor_enter_gc_safe(seg);
        let fixed = ctx.read_native_pin(pin, fixed);
        // SAFETY: identical lifetime-erasure contract to `acquire` above.
        let ctx_ptr: *mut dyn NativeContext = ctx;
        let ctx_ptr_static: *mut (dyn NativeContext + 'static) =
            unsafe { core::mem::transmute(ctx_ptr) };
        (
            ChmMonitorGuard {
                ctx: ctx_ptr_static,
                seg: fixed,
                pin,
                mutation_epoch,
                mutation_active,
                _borrow: std::marker::PhantomData,
            },
            fixed,
        )
    }
}

impl<'a> Drop for ChmMonitorGuard<'a> {
    fn drop(&mut self) {
        // SAFETY: the guard is always a local whose lifetime is bounded
        // by the `&mut dyn NativeContext` borrow used in `acquire`. No
        // other code can drop or invalidate the context while the guard
        // is live.
        //
        // Bug 4 (HIGH): if `monitor_exit` itself panics while we are
        // already unwinding from a panic in the protected block, Rust
        // promotes the second panic to an abort. Catch any panic from
        // `monitor_exit` here so we degrade to a leaked monitor + log
        // instead of taking down the whole VM. The closure captures a
        // raw pointer (`self.ctx`) plus a Copy ObjectRef, neither of
        // which carries UnwindSafe bounds, so we wrap in
        // `AssertUnwindSafe` — `monitor_exit` does not maintain
        // invariants that would be broken by an unwinding caller.
        let ctx_ptr = self.ctx;
        // GC-SAFETY (2026-07-21): exit the segment at its CURRENT address --
        // see the `pin` field doc. The pin slot was remapped by any moving
        // collection that ran while the guard was held.
        let seg = unsafe { (*ctx_ptr).read_native_pin(self.pin, self.seg) };
        let pin = self.pin;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            (*ctx_ptr).monitor_exit(seg);
            (*ctx_ptr).unpin_native_roots(pin);
        }));
        // Order matters: the epoch must advance after the last node write
        // (and after `monitor_exit` publishes it), and the in-flight count
        // must drop only after that. A reader that sees `active == 0` on both
        // sides of its walk with an unchanged epoch has therefore observed no
        // mutation of this stripe across the walk.
        self.mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        self.mutation_active
            .fetch_sub(1, std::sync::atomic::Ordering::Release);
        if result.is_err() {
            eprintln!(
                "[CHM] monitor_exit panicked during ChmMonitorGuard::drop — ignoring to avoid double-panic abort"
            );
        }
    }
}

/// Java `Double.toString(double)` formatting (fix item 4).
///
/// Local replica of native-builtins' `lang_string::format_double` (which is
/// `pub(crate)` there and so unreachable from this crate). Rust's default
/// `{}` float formatting already produces the shortest decimal that round-trips
/// (Grisu/Ryū), matching Java's shortest-round-trip contract; the remaining
/// Java-specific rules are: `Infinity` / `-Infinity` / `NaN` literals, and a
/// mandatory decimal point (Java prints `1.0`, never `1`). Without this the
/// wrapper `toString` path below rendered `Double`/`Float` via Rust's raw
/// formatting, dropping the trailing `.0` and emitting `inf`/`-inf`/`NaN` Rust
/// spellings instead of the Java literals.
fn java_double_to_string(v: f64) -> String {
    if v == f64::INFINITY {
        "Infinity".to_string()
    } else if v == f64::NEG_INFINITY {
        "-Infinity".to_string()
    } else if v.is_nan() {
        "NaN".to_string()
    } else {
        let s = format!("{v}");
        if !s.contains('.') && !s.contains('e') && !s.contains('E') {
            format!("{s}.0")
        } else {
            s
        }
    }
}

/// Java `Float.toString(float)` formatting (fix item 4). See
/// [`java_double_to_string`] for the rules; the only difference is the
/// narrower `f32` round-trip width.
fn java_float_to_string(v: f32) -> String {
    if v == f32::INFINITY {
        "Infinity".to_string()
    } else if v == f32::NEG_INFINITY {
        "-Infinity".to_string()
    } else if v.is_nan() {
        "NaN".to_string()
    } else {
        let s = format!("{v}");
        if !s.contains('.') && !s.contains('e') && !s.contains('E') {
            format!("{s}.0")
        } else {
            s
        }
    }
}

/// A rendered display value in UTF-16 code units.
///
/// Every `toString` in this file used to build a Rust `String`, and a Rust
/// `String` cannot hold an unpaired UTF-16 surrogate -- so all seventeen of
/// them substituted U+FFFD for text a Java program is entitled to keep.
/// `G63-1` section 3 measured three of them (`list_toString`, `map_toString`,
/// `arrays_toString`) and named the other fourteen as the reason not to fix
/// only those three: they are one defect in seventeen places, and converting a
/// subset would have left siblings that are wrong in exactly the same way while
/// looking like the question had been settled.
type Units = Vec<u16>;

/// An ASCII/BMP literal (the punctuation these renderers emit) as units.
fn units_of(s: &str) -> Units {
    s.encode_utf16().collect()
}

fn push_lit(buf: &mut Units, s: &str) {
    buf.extend(s.encode_utf16());
}

/// `parts` joined by `sep`.
fn join_units(parts: &[Units], sep: &[u16]) -> Units {
    let total =
        parts.iter().map(|p| p.len()).sum::<usize>() + sep.len() * parts.len().saturating_sub(1);
    let mut out = Units::with_capacity(total);
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(sep);
        }
        out.extend_from_slice(part);
    }
    out
}

/// `open + parts.join(sep) + close` -- `[a, b, c]`, `{k=v, k=v}`.
fn wrap_join(open: &str, parts: &[Units], sep: &str, close: &str) -> Units {
    let mut out = units_of(open);
    out.extend_from_slice(&join_units(parts, &units_of(sep)));
    push_lit(&mut out, close);
    out
}

/// `k=v` for one map entry.
fn entry_units(k: &[u16], v: &[u16]) -> Units {
    let mut out = Units::with_capacity(k.len() + v.len() + 1);
    out.extend_from_slice(k);
    out.push(u16::from(b'='));
    out.extend_from_slice(v);
    out
}

/// Read a Java String field as units, for the delimiter/prefix/suffix a
/// `Collectors.joining` carries. They are Java Strings like any other and can
/// hold the same unpaired surrogate the elements can.
fn read_units_field(ctx: &dyn NativeContext, obj: ObjectRef, field: usize) -> Units {
    match ctx.get_field(obj, field) {
        Value::Object(Some(r)) => ctx.read_string_units(r).unwrap_or_default(),
        _ => Units::new(),
    }
}

/// Try to read an object's string representation for display purposes.
///
/// For objects that are not plain strings or primitives, this calls
/// `toString()` via virtual dispatch so that overridden implementations
/// (e.g. ArrayList, HashMap, user classes) produce correct output.
///
/// Fallible for the reason on [`collection_elements_generic`]. The
/// `ClassName@hash` fallback below is the right answer when there is no
/// `toString()` to run, and it was the wrong one when there was and it *threw*
/// or was refused: `[a, cratonvm.internal.Foo@1a2b, c]` is a plausible string
/// with no exception behind it, so a strict refusal inside an element's own
/// `toString()` came out of `list.toString()` looking like ordinary output.
/// HotSpot propagates — `AbstractCollection.toString` is a bare
/// `sb.append(e)` loop with no catch.
fn obj_to_display_units(
    ctx: &mut dyn NativeContext,
    val: &Value,
) -> Result<Units, MethodCallFailed> {
    Ok(match val {
        Value::Object(None) => units_of("null"),
        Value::Object(Some(obj)) => {
            // Fast path: if it's a Java String, read it directly -- in UNITS.
            // This line and the `toString()` result below are the only two
            // places a caller's own text enters this function, and they were
            // both `read_string`, so both dropped an unpaired surrogate. Every
            // other arm here renders a number or a class name and cannot.
            if let Some(s) = ctx.read_string_units(*obj) {
                return Ok(s);
            }

            // Fast path for wrapper types: if the object is a `java.lang.*`
            // boxed primitive (exactly 1 field holding its value), format it
            // directly. This MUST be gated on the class actually being a
            // wrapper — a user class with a single primitive field (e.g. a
            // `Comparable` value object with one `int`) has the same shape and
            // would otherwise be rendered by its raw field value instead of
            // dispatching its overridden `toString()` (HotSpot prints
            // `element.toString()`; we were printing the field). See the
            // TreeSet/TreeMap toString gap.
            let nf = ctx.object_num_fields(*obj);
            if nf == 1 {
                let class_id = ctx.class_id_of_object(*obj);
                let name = ctx.class_name_of_id(class_id).unwrap_or_default();
                let is_wrapper = matches!(
                    name.as_str(),
                    "java/lang/Integer"
                        | "java/lang/Long"
                        | "java/lang/Short"
                        | "java/lang/Byte"
                        | "java/lang/Boolean"
                        | "java/lang/Character"
                        | "java/lang/Float"
                        | "java/lang/Double"
                );
                if is_wrapper {
                    match ctx.get_field(*obj, 0) {
                        Value::Int(v) => {
                            // A boxed `Character` CAN be a lone surrogate --
                            // `Character.valueOf('\ud800')` is legal and prints
                            // as one unit. `char::from_u32` rejects exactly
                            // those, and the old `unwrap_or('?')` turned them
                            // into a question mark. Emitting the raw unit is
                            // both simpler and correct.
                            return Ok(if name.contains("Boolean") {
                                units_of(if v != 0 { "true" } else { "false" })
                            } else if name.contains("Character") {
                                vec![v as u16]
                            } else if name.contains("Byte") {
                                units_of(&(v as i8).to_string())
                            } else if name.contains("Short") {
                                units_of(&(v as i16).to_string())
                            } else {
                                units_of(&v.to_string())
                            });
                        }
                        Value::Long(v) => return Ok(units_of(&v.to_string())),
                        // Fix (item 4): Java shortest-round-trip formatting,
                        // not Rust's default `{}` (which omits the trailing
                        // `.0` and uses `inf`/`NaN` spellings).
                        Value::Float(v) => return Ok(units_of(&java_float_to_string(v))),
                        Value::Double(v) => return Ok(units_of(&java_double_to_string(v))),
                        _ => {}
                    }
                }
            }

            // Call toString() via virtual dispatch
            match ctx.invoke_virtual(*obj, "toString", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(str_ref)))) => ctx
                    .read_string_units(str_ref)
                    .unwrap_or_else(|| units_of("null")),
                Err(e) => return Err(e),
                _ => {
                    // Final fallback: ClassName@hash. Arrays render the JVMS
                    // array-class name (the header's class_id is the COMPONENT
                    // class — mirrors lang_class::array_descriptor_for in
                    // native-builtins).
                    let class_name = if ctx.heap_kind_of(*obj) == cratonvm_types::ObjectKind::Array
                    {
                        display_array_class_name(ctx, *obj)
                    } else {
                        let class_id = ctx.class_id_of_object(*obj);
                        ctx.class_name_of_id(class_id)
                            .unwrap_or_else(|| "?".to_string())
                    };
                    let hash = ctx.identity_hash_code(*obj);
                    units_of(&format!("{}@{:x}", class_name.replace('/', "."), hash))
                }
            }
        }
        Value::Int(v) => units_of(&v.to_string()),
        Value::Long(v) => units_of(&v.to_string()),
        // Fix (item 4): Java shortest-round-trip formatting (see the boxed
        // wrapper arms above) for bare primitive floats/doubles too.
        Value::Float(v) => units_of(&java_float_to_string(*v)),
        Value::Double(v) => units_of(&java_double_to_string(*v)),
        _ => units_of("?"),
    })
}

/// [`obj_to_display_units`] over every element of `vals`, in order.
///
/// gc-common w20-a: each element's `toString()` runs Java and can complete a
/// moving collection, so a caller that looped `obj_to_display_units` over a
/// `Vec<Value>` it had collected earlier rendered every later element through
/// its pre-move address. Here every element is pinned first and read back
/// through its pin just before it is rendered; the pins are released on every
/// path.
fn display_units_pinned(
    ctx: &mut dyn NativeContext,
    vals: &[Value],
) -> Result<Vec<Units>, MethodCallFailed> {
    let (base, handles) = pin_value_slice(ctx, vals);
    let mut out = Vec::with_capacity(vals.len());
    let mut failed = None;
    for (v, &h) in vals.iter().zip(handles.iter()) {
        let v = read_pinned_elem(ctx, h, *v);
        match obj_to_display_units(ctx, &v) {
            Ok(u) => out.push(u),
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    if base != usize::MAX {
        ctx.unpin_native_roots(base);
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// [`display_units_pinned`] for `(key, value)` rows: the rendered key and
/// value of each row, in order.
fn display_entry_units_pinned(
    ctx: &mut dyn NativeContext,
    rows: &[(Value, Value)],
) -> Result<Vec<(Units, Units)>, MethodCallFailed> {
    let flat: Vec<Value> = rows.iter().flat_map(|(k, v)| [*k, *v]).collect();
    let mut units = display_units_pinned(ctx, &flat)?.into_iter();
    let mut out = Vec::with_capacity(rows.len());
    while let (Some(k), Some(v)) = (units.next(), units.next()) {
        out.push((k, v));
    }
    Ok(out)
}

/// Append `arr[0..len]` to `text` as `e0, e1, ...` ([`obj_to_display_units`]
/// per element), reading the array back through a pin before every element:
/// each `toString()` runs Java and can move it (gc-common w20-a — the
/// `ArrayList` / `subList` `toString` loops indexed the array through its
/// first address).
fn push_array_display_units(
    ctx: &mut dyn NativeContext,
    text: &mut Units,
    arr: ObjectRef,
    len: usize,
) -> Result<(), MethodCallFailed> {
    let pin = ctx.pin_native_root(arr);
    let mut failed = None;
    for i in 0..len {
        if i > 0 {
            push_lit(text, ", ");
        }
        let arr = ctx.read_native_pin(pin, arr);
        let val = ctx.get_array_element(arr, i);
        match obj_to_display_units(ctx, &val) {
            Ok(d) => text.extend_from_slice(&d),
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    ctx.unpin_native_roots(pin);
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// JVMS array-class name for a heap array (`[Ljava/lang/Class;`, `[I`,
/// `[[I`, …). Local mirror of native-builtins' lang_class::array_descriptor_for
/// (separate crate): primitive arrays carry ClassId(0) and are identified by
/// the header's element type; reference arrays carry the COMPONENT class id.
fn display_array_class_name(ctx: &dyn NativeContext, obj: ObjectRef) -> String {
    use cratonvm_types::ArrayElementType;
    match ctx.heap_element_type_of(obj) {
        ArrayElementType::Boolean => "[Z".to_string(),
        ArrayElementType::Char => "[C".to_string(),
        ArrayElementType::Float => "[F".to_string(),
        ArrayElementType::Double => "[D".to_string(),
        ArrayElementType::Byte => "[B".to_string(),
        ArrayElementType::Short => "[S".to_string(),
        ArrayElementType::Int => "[I".to_string(),
        ArrayElementType::Long => "[J".to_string(),
        ArrayElementType::Reference => {
            let comp = ctx
                .class_name_of_id(ctx.class_id_of_object(obj))
                .unwrap_or_default();
            if comp.is_empty() {
                "[Ljava/lang/Object;".to_string()
            } else if comp.starts_with('[') {
                format!("[{}", comp)
            } else {
                format!("[L{};", comp)
            }
        }
    }
}

/// Unbox a wrapper object (Integer, Long, Boolean, etc.) to its primitive Value.
/// Returns None if the object is not a recognized JDK wrapper type.
/// True when a `read_string` decode is a FAITHFUL rendering of the receiver's
/// UTF-16 code units.
///
/// A Rust `str` cannot hold an unpaired surrogate, so the VM's reader
/// substitutes `U+FFFD` for one. Text containing no `U+FFFD` therefore provably
/// survived the decode; text containing one may be a genuine `U+FFFD` OR a
/// mangled surrogate, and no `&str`-level comparison can tell those apart.
///
/// This crate cannot read code units — `native-collections` has no dependency
/// on `native-builtins`, whose `lang_string::read_string_chars` is the
/// units-preserving reader, and adding a second decoder here would be a second
/// encoding of one concept. So every place that must decide something about
/// Java text from a `&str` is gated on this predicate and REFUSES rather than
/// guesses, handing the decision to the units-exact path instead
/// (`java_strings_equal` / `java_string_hash_code` / a real `compareTo`
/// dispatch). The two failure modes it closes were both MEASURED against
/// HotSpot 25 on 2026-08-17: a `TreeSet` that silently merged two distinct
/// keys, and a `contains` that answered "found" for a string the collection
/// never held.
fn decode_is_faithful(text: &str) -> bool {
    !text.contains('\u{FFFD}')
}

/// Compare two DECODED Java strings the way `String.compareTo` compares them —
/// by UTF-16 code unit — or refuse.
///
/// `None` means "this comparison was not made": one of the decodes may have
/// destroyed an unpaired surrogate (see [`decode_is_faithful`]), and two keys
/// mangled to the same `U+FFFD` text would compare EQUAL, which is how a
/// natural-order `TreeSet`/`TreeMap` silently drops one of two distinct
/// elements. The caller must fall back to dispatching the receiver's own
/// `compareTo`.
///
/// The unit comparison is not the same as `str::cmp`, which orders by code
/// POINT. They disagree above the BMP: a supplementary character starts with a
/// surrogate unit in `0xD800..=0xDBFF`, below `U+E000..=U+FFFF`, while its code
/// point is above them. MEASURED on both VMs 2026-08-17: a `TreeSet` of
/// `{U+10000, U+E000, U+FFFF}` enumerates `[U+10000, U+E000, U+FFFF]` on
/// HotSpot; `str::cmp` put `U+10000` last.
fn compare_decoded_strings(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    if !decode_is_faithful(a) || !decode_is_faithful(b) {
        return None;
    }
    Some(a.encode_utf16().cmp(b.encode_utf16()))
}

/// Build a fast-mode [`TreeKey`] from a DECODED Java string, or refuse.
///
/// Same rule as [`compare_decoded_strings`], for the representation rather than
/// the comparison: a key whose decode may have lost an unpaired surrogate
/// cannot be stored faithfully, and two such keys would collapse into ONE
/// `BTreeMap` entry (MEASURED 2026-08-17: `TreeMap.size()` 1 where HotSpot says
/// 2, with the survivor's value answered for both keys). Refusing sends the
/// caller to the array path, which keeps the real `ObjectRef` and runs the
/// receiver's own units-exact `String.compareTo`.
fn tree_key_from_decoded_string(text: &str) -> Option<TreeKey> {
    if !decode_is_faithful(text) {
        return None;
    }
    Some(TreeKey::Str(text.encode_utf16().collect()))
}

fn unbox_wrapper(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<Value> {
    if let Some(result) = ctx.fast_unbox_primitive_wrapper(obj) {
        return result;
    }
    let class_name = ctx.class_name_of_id(ctx.class_id_of_object(obj))?;
    if !matches!(
        class_name.as_str(),
        "java/lang/Integer"
            | "java/lang/Long"
            | "java/lang/Boolean"
            | "java/lang/Character"
            | "java/lang/Byte"
            | "java/lang/Short"
            | "java/lang/Float"
            | "java/lang/Double"
    ) {
        return None;
    }

    if ctx.object_num_fields(obj) != 1 {
        return None;
    }
    match ctx.get_field(obj, 0) {
        inner @ (Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_)) => {
            Some(inner)
        }
        _ => None,
    }
}

/// Normalize a Value for comparison: unbox wrapper objects to their primitive.
fn normalize_for_compare(ctx: &dyn NativeContext, v: &Value) -> Value {
    match v {
        Value::Object(Some(obj)) => {
            if let Some(prim) = unbox_wrapper(ctx, *obj) {
                prim
            } else {
                *v
            }
        }
        _ => *v,
    }
}

/// Check if two Values refer to the same object (by identity) or are equal
/// strings/wrapper values. Used by ArrayList.contains/indexOf and HashMap key lookup.
fn values_equal(ctx: &dyn NativeContext, a: &Value, b: &Value) -> bool {
    // Two BOXED wrappers of different runtime classes are never `equals` in
    // Java (each wrapper's `equals` opens with an `instanceof` of its own
    // type), but `normalize_for_compare` below erases the class and leaves
    // only the primitive — so `list.contains(Long.valueOf(1L))` reported true
    // for a list holding `Integer.valueOf(1)`, and `map.containsValue('A')`
    // reported true for a map holding `Integer.valueOf(65)`. Mirrors the guard
    // in `map_keys_equal`; see there for the full rationale.
    //
    // Scoped to the object/object case on purpose: when one side is a RAW
    // `Value::Int`/`Value::Long` there is no Java wrapper class to compare and
    // the cross-type numeric arms below stay in force, since that comparison
    // is about how the VM represented the value, not about two Java objects.
    // The ClassId inequality test short-circuits the two `unbox_wrapper` calls
    // in the overwhelmingly common same-class case.
    if let (Value::Object(Some(oa)), Value::Object(Some(ob))) = (a, b) {
        if ctx.class_id_of_object(*oa) != ctx.class_id_of_object(*ob)
            && unbox_wrapper(ctx, *oa).is_some()
            && unbox_wrapper(ctx, *ob).is_some()
        {
            return false;
        }
    }
    // Normalize: unbox wrapper objects to primitives for comparison
    let na = normalize_for_compare(ctx, a);
    let nb = normalize_for_compare(ctx, b);
    match (&na, &nb) {
        (Value::Object(Some(oa)), Value::Object(Some(ob))) => {
            // Identity check first
            if std::ptr::eq(oa.as_ptr(), ob.as_ptr()) {
                return true;
            }
            // String value equality, asked of the VM so it compares code units.
            //
            // This was `read_string(a) == read_string(b)`, and a Rust `str`
            // cannot hold an unpaired UTF-16 surrogate: both operands decoded
            // to `U+FFFD` and compared EQUAL. MEASURED 2026-08-17 against
            // HotSpot 25 — `list.contains`, `list.indexOf`, `Deque.contains`
            // and `Map.containsValue` all answered "found" for a string that
            // differs from the stored one in exactly the way `String.equals`
            // calls different. A false positive, not a miss, so nothing
            // downstream had any way to notice.
            if let Some(equal) = ctx.java_strings_equal(*oa, *ob) {
                return equal;
            }
            // Enum constants: compare by (declaring class, ordinal) so an
            // enum-keyed List/Set still finds a member when the VM produced a
            // duplicate object for the same constant. See `enum_key_identity`.
            if let (Some(ia), Some(ib)) = (enum_key_identity(ctx, *oa), enum_key_identity(ctx, *ob))
            {
                return ia == ib;
            }
            false
        }
        (Value::Object(None), Value::Object(None)) => true,
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Long(a), Value::Long(b)) => a == b,
        // `Float.equals`/`Double.equals` compare bit patterns, not `==`:
        // NaN equals itself and +0.0 != -0.0. Every caller of `values_equal`
        // implements a Java `equals`-semantics operation (`List.contains`,
        // `List.indexOf`, `List.remove(Object)`, `Map.containsValue`), so the
        // bit semantics is the correct one for all of them. With `==`,
        // `list.contains(Double.NaN)` was false for a list that held NaN.
        (Value::Float(a), Value::Float(b)) => {
            (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
        }
        (Value::Double(a), Value::Double(b)) => {
            (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
        }
        // Cross-type numeric: Int vs Long
        (Value::Int(a), Value::Long(b)) => (*a as i64) == *b,
        (Value::Long(a), Value::Int(b)) => *a == (*b as i64),
        _ => false,
    }
}

/// Compare two grouping/keying values using the SAME semantics a real
/// `HashMap` key would: identity, then the object's own Java `equals()`.
///
/// `values_equal` only knows about identity, `String`, and enum constants and
/// returns `false` for any other two distinct object instances — so a
/// `Collectors.groupingBy` / `toMap` whose key is e.g. a `LinkedHashMap`,
/// record, or any value class would treat two equal-but-distinct keys as
/// different groups. The Rust-side group list then ends up with one entry per
/// element, and `make_map_of` (which keys on real Java `equals`) collapses them
/// to a single key keeping only the LAST value — silently dropping the rest
/// (the kafka `RangeAssignor.assignWithRackMatching` `groupingBy(consumers)`
/// bug: all topic states share one consumer-set key, so only the last topic was
/// grouped → rack-aware assignment skipped for the others). Invoking the real
/// `equals` closes the gap for arbitrary key types.
///
/// Fallible for the reason [`map_keys_equal`] already states, and this is the
/// same defect one function over: `if let Ok(..)` folded a thrown or refused
/// `equals` into `false`, so a failing key comparison did not fail — it opened
/// a new group. `groupingBy` then answered a map with one bucket per element
/// and no exception, which is a wrong answer wearing the shape of a correct
/// one. A non-exceptional contract violation (the callee returned a non-`Int`)
/// still falls through as `false`, exactly as in `map_keys_equal`.
fn group_key_equal(
    ctx: &mut dyn NativeContext,
    a: &Value,
    b: &Value,
) -> Result<bool, MethodCallFailed> {
    if let (Value::Object(Some(oa)), Value::Object(Some(ob))) = (a, b) {
        if std::ptr::eq(oa.as_ptr(), ob.as_ptr()) {
            return Ok(true);
        }
        return match ctx.invoke_virtual(
            *oa,
            "equals",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(*ob))],
        ) {
            Ok(Some(Value::Int(v))) => Ok(v != 0),
            Err(e) => Err(e),
            _ => Ok(false),
        };
    }
    // Primitives / null: the cheap structural comparison is exact.
    Ok(values_equal(&*ctx, a, b))
}

/// `List.contains`/`indexOf` element comparison with full JDK semantics.
///
/// `values_equal` only recognises identity, `String`, enum constants, and
/// (1-field) wrapper/primitive values — for any *other* two distinct object
/// instances it returns `false`. That silently breaks `List.contains` /
/// `indexOf` for value classes whose equality is defined by a real `equals`
/// override but whose object layout `values_equal` can't introspect: `UUID`,
/// `BigInteger`, `BigDecimal`, `File`, `URI`, records, and arbitrary user
/// classes. (ANTLR's `ATNDeserializer` does `SUPPORTED_UUIDS.contains(uuid)`
/// over a `List<UUID>`; the structural-only check returned -1 even for a
/// value-equal UUID, breaking Groovy's `GroovyLexer` — SB-13 blocker #4.)
///
/// Mirrors [`group_key_equal`] (which already does this for map keys): take the
/// cheap structural check first, then fall back to the search target's real
/// Java `equals`. JDK `List.indexOf` semantics call `o.equals(es[i])` — i.e.
/// `target.equals(elem)` — so the real-equals receiver is the search `target`.
///
/// Fallible for the same reason as [`group_key_equal`] and [`map_keys_equal`]:
/// `if let Ok(..)` mapped a thrown or refused `equals` onto "these two are not
/// equal", and every caller below is a `contains` / `indexOf` / `remove` /
/// `retainAll` / `frequency`, so the swallow does not merely lose the answer —
/// it produces the *opposite* one and lets the caller act on it (`remove`
/// reports "not present" and mutates nothing; `retainAll` drops the element).
/// The real JDK propagates out of `List.contains`, since the whole method body
/// is `indexOf(o) >= 0` over `o.equals(es[i])`.
///
/// A non-exceptional contract violation — the callee returned something that
/// is not an `Int` — still answers `false`, as in the two helpers above.
fn list_element_matches(
    ctx: &mut dyn NativeContext,
    elem: &Value,
    target: &Value,
) -> Result<bool, MethodCallFailed> {
    // Cheap structural path: identity, unboxed primitives, String, enum, null.
    if values_equal(&*ctx, elem, target) {
        return Ok(true);
    }
    // Distinct objects whose value-equality the structural check can't see:
    // defer to the target's real `equals(elem)`.
    if let (Value::Object(Some(t)), Value::Object(Some(e))) = (target, elem) {
        return match ctx.invoke_virtual(
            *t,
            "equals",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(*e))],
        ) {
            Ok(Some(Value::Int(v))) => Ok(v != 0),
            Err(err) => Err(err),
            _ => Ok(false),
        };
    }
    Ok(false)
}

/// GC-safe linear scan of `buf[0..len]` for an element matching `target`
/// (JDK `indexOf`/`contains` semantics via [`list_element_matches`], whose
/// `equals()` dispatch can trigger a moving GC on every iteration).
///
/// Family-1 stale-at-store fix (cce0079 follow-up): the naive loop most
/// callers used — capture `buf`/`target` once, dispatch `equals()` per
/// element — leaves both stale after the first GC-capable dispatch, so
/// subsequent reads walk a recycled array (wrong result, or a stale element
/// handed onward to a mutation such as remove/shift). This helper pins both,
/// refreshes them after every dispatch, and returns the REFRESHED
/// `(index, buf, target)` so the caller can act on current addresses.
/// The caller must have pinned (and afterwards refresh) its own receiver.
fn pinned_array_search(
    ctx: &mut dyn NativeContext,
    buf: ObjectRef,
    len: usize,
    target: Value,
) -> Result<(Option<usize>, ObjectRef, Value), MethodCallFailed> {
    let buf_pin = ctx.pin_native_root(buf);
    let th = pin_value(ctx, target);
    let mut buf = buf;
    let mut target = target;
    let mut found = None;
    // A refused/throwing element `equals` is recorded and the loop left by the
    // ordinary `break`, so `unpin_native_roots(buf_pin)` still runs — an early
    // `?` here would skip the truncate this helper exists to guarantee.
    let mut failed: Option<MethodCallFailed> = None;
    for i in 0..len {
        let elem = ctx.get_array_element(buf, i);
        let matched = match list_element_matches(ctx, &elem, &target) {
            Ok(m) => m,
            Err(e) => {
                failed = Some(e);
                break;
            }
        };
        buf = ctx.read_native_pin(buf_pin, buf);
        target = read_pinned_elem(ctx, th, target);
        if matched {
            found = Some(i);
            break;
        }
    }
    ctx.unpin_native_roots(buf_pin);
    match failed {
        Some(e) => Err(e),
        None => Ok((found, buf, target)),
    }
}


// ReentryEntry shared by collections
pub(crate) type ReentryEntry = (usize, ObjectRef, &'static str);

mod array_list;
pub use array_list::*;

// ===========================================================================
mod hash_map;
pub use hash_map::*;

mod hash_set;
pub use hash_set::*;

mod iterators;
pub use iterators::*;

mod arrays;
pub use arrays::*;

mod collections_util;
pub use collections_util::*;

mod streams;
pub use streams::*;
mod misc_util;
pub use misc_util::*;

// ===========================================================================
mod linked_list;
pub use linked_list::*;

// ===========================================================================
mod linked_hash_map;
pub use linked_hash_map::*;

mod deque;
pub use deque::*;

mod tree_map;
pub use tree_map::*;

// ===========================================================================
mod concurrent_hash_map;
pub use concurrent_hash_map::*;

// ===========================================================================
mod concurrent_queue;
pub use concurrent_queue::*;

#[cfg(test)]
mod tests;
