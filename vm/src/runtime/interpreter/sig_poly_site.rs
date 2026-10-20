// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The interpreter's per-site ROUTE cache for signature-polymorphic
//! `invokevirtual` sites: `VarHandle` access modes and
//! `MethodHandle.invokeExact` / `invoke` (interpreter round i1 wave 48, lane
//! L7; proposal
//! `docs/internal/fixed-bugs/interpreter-L7-proposal-cache-signature-polymorphic-invokes-in-the-interpreter-FIXED-20261012.md`).
//!
//! # Why
//!
//! Such a site names a descriptor no method declares, so the invoke cache
//! never holds an entry for it and EVERY execution took the full slow path:
//! the fast door's probe, the cached dispatcher's probe, `vtable_fast`, then
//! `execute_invoke_kind` — method-ref resolution, the owner and selection
//! checks, `try_stackless_invoke`'s arms, and for a `VarHandle` the whole of
//! `invoke_on_class_shared_inner` down to its signature-polymorphic block —
//! only to reach the same access-mode native with the same arguments. Under
//! `--nojit` that was ~12 us per `VarHandle.get` against ~0.16 us for a field
//! read.
//!
//! # What is cached, and what is not
//!
//! The ROUTE only: which registered native the slow path ran (by registry
//! slot), which of the two slow-path doors ran it (their call and return
//! conventions differ), and which call-site channels it armed
//! (`cratonvm_native_api::poly_call_site`). It is recorded where the slow path
//! actually dispatched — never re-derived — and guarded by the receiver
//! handle's CLASS, which is everything the slow path's choice depended on
//! besides the site's own name and descriptor (see [`note_route`]). Every
//! per-call rule stays in the native: the invokeExact type check, the
//! VarHandle coordinate NPE, value adaptation, a `CallSite`'s current target,
//! a handle's form. So a `MutableCallSite.setTarget`, a handle of the same
//! class with another type or target, and an exact `VarHandle` whose access
//! mode type differs from the site all take the same route to the same native
//! and are judged there, as on the slow path.
//!
//! # Invalidation
//!
//! * Class redefinition: any redefinition drops every entry of the thread (a
//!   redefined caller's pool may name another member under the same index; a
//!   redefined class may lose or gain a native shadow). A fill that straddles
//!   one is not stored.
//! * The native registry: an entry holds the registry generation it was
//!   filled under and declines when a registration has added a triple since
//!   (the same rule as `NativeCallSite`); the callback is redeemed from the
//!   slot id on every hit, so an in-place re-registration is followed.
//! * A receiver of another class, a null receiver, an array receiver, a frame
//!   running an obsolete method: declined to the slow path, which owns them.
//!
//! # Cost when unused
//!
//! The probe sits on the general dispatcher's MISS path of `invokevirtual`
//! only (after the fast door and the cached dispatcher declined), behind the
//! hoisted per-VM switch; a thread that never filled an entry pays one
//! `is_empty` test there. The fill side arms one per-thread cell around the
//! slow path.
//!
//! Switch: `CRATONVM_SIG_POLY_SITE_CACHE` (`VmConfig::sig_poly_site_cache`).
//! The hit lends its site instead of cloning the `Arc` under
//! `CRATONVM_SIG_POLY_SITE_LEND` (`VmConfig::sig_poly_site_lend`, wave 57).

use super::*;
use rustc_hash::FxHashMap;

/// At most this many receiver classes are remembered per site (a site that
/// sees more is served for the first ones and takes the slow path for the
/// rest, as before this cache existed).
const ROUTES_PER_SITE: usize = 4;

/// Which slow-path door ran the native. Their conventions differ, and the
/// hit replays the one that ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SigPolyDoor {
    /// `try_stackless_invoke`'s erased `MethodHandle.invoke*` arm: the call
    /// runs through `safe_native_call_synchronized` with the monitor fact the
    /// slow path computed, a `Some` result is unboxed against the site
    /// (`None` from the unboxing pushes `null`), and the call-site descriptor
    /// is armed when `arms_poly_call_site` says so (never cleared).
    StacklessMethodHandle {
        /// The slow path's `synchronized_native_target` answer.
        sync: Option<(bool, ClassId)>,
    },
    /// `vm_exec::invoke_on_class_shared_inner`'s signature-polymorphic
    /// block: the call-site descriptor is armed or CLEARED, the VarHandle
    /// value-site cell armed when the block armed it, the call runs through
    /// `safe_native_call`, and its result is unboxed against the site before
    /// `execute_invoke_kind` pushes it.
    PolyBlock {
        /// The whole parameter-site word
        /// (`poly_call_site::vh_param_site_classes`, coordinates included) the
        /// block armed, if it armed the cell. It was the low 16 value bits
        /// only until JIT round 16 wave 7 (lane varhandle), and a site whose
        /// word had anything above them (every three-parameter site: an
        /// instance `compareAndSet` / `compareAndExchange`) was kept off the
        /// cache for it.
        vh_param_word: Option<u64>,
        /// The site returns the variable into a reference type narrower than
        /// `Object` (`vm_exec::vh_site_returns_narrow_reference`): the hit runs
        /// the block's reference-return checkcast
        /// (`vm_exec::unbox_poly_return_checked_vh`) too.
        vh_narrow_reference_return: bool,
    },
}

/// One route: the receiver class it was recorded for and what ran.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SigPolyRoute {
    pub(crate) receiver_class_id: ClassId,
    pub(crate) native_id: cratonvm_native_api::NativeMethodId,
    pub(crate) door: SigPolyDoor,
    /// The door armed the call-site descriptor (`arms_poly_call_site`).
    pub(crate) arm_call_site: bool,
}

/// The per-thread cell the slow path's dispatch points write into while a
/// signature-polymorphic-capable slow path runs (see [`note_route`]).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum SigPolyRecord {
    /// No interpreter slow path is listening.
    #[default]
    Idle,
    /// `execute_invoke_kind` runs for a site at this frame depth.
    Armed { depth: usize },
    /// A door at that depth dispatched a signature-polymorphic native.
    Recorded {
        depth: usize,
        route: SigPolyRoute,
        /// `site_key_hash(method_name, descriptor)` of the dispatch, which
        /// the fill compares with the site it resolves.
        key_hash: u64,
    },
}

/// A cached site: its constant facts and up to [`ROUTES_PER_SITE`] routes.
pub(crate) struct SigPolySite {
    method_name: Arc<str>,
    descriptor: Arc<str>,
    facts: cratonvm_jit_api::DescriptorFacts,
    num_params: usize,
    /// The site's owner is a `VarHandle` family class: the hit runs the
    /// invoke-exact `VarHandle` gate `execute_invoke_kind` runs.
    is_var_handle_site: bool,
    /// A `VarHandle.compareAndSet` site shaped `(receiver, expected, new)Z`
    /// with both values of one kind: that kind
    /// (`jit::helpers::varhandle_cas_site_kind`), for the instance-field CAS
    /// arm of the hit (MH16-6, `CRATONVM_VH_INTERP_CAS_FAST`, default ON, `=0` kills it).
    vh_cas_site_kind: Option<u8>,
    /// A `VarHandle.compareAndExchange` (plain, `Acquire`, `Release`) site
    /// shaped `(receiver, expected, new)` over `int` or `long` returning the
    /// same kind: that kind (`jit::helpers::varhandle_cae_site_kind`), for the
    /// instance-field exchange arm of the hit (interpreter round i2 wave 54,
    /// lane L7, `CRATONVM_VH_INTERP_CAE_FAST`).
    vh_cae_site_kind: Option<u8>,
    /// The native registry generation the routes were resolved under.
    registry_generation: u32,
    routes: smallvec::SmallVec<[SigPolyRoute; ROUTES_PER_SITE]>,
}

/// Bits in [`SigPolySiteCache::filter`].
const SITE_FILTER_BITS: usize = 1024;

/// One entry of [`SigPolySiteCache`].
struct SigPolySlot {
    /// `None` only while the site is LENT to a hit that is running its route
    /// (`VmConfig::sig_poly_site_lend`, see [`SigPolySiteCache::lend`]); a
    /// probe of a lent entry declines, as a miss does.
    site: Option<Arc<SigPolySite>>,
    /// A probe found this site lent: its route re-enters it (a
    /// `MethodHandle` whose target calls back through the same
    /// `invokeExact` site). Such a site is never lent again but served by an
    /// `Arc` clone, so its nested hits are answered from the cache instead
    /// of every nested level taking the slow path.
    reentrant: bool,
}

/// The per-thread table. Keyed by `(caller class, cp index, invoke pc)`.
#[derive(Default)]
pub(crate) struct SigPolySiteCache {
    sites: FxHashMap<(ClassId, u16, u32), SigPolySlot>,
    /// `class_redefinition_count()` every entry was filled under.
    redefinitions_seen: u64,
    /// A one-hash Bloom filter over the `(cp index, invoke pc)` of every
    /// filled site (interpreter round i2 wave 54, lane L7,
    /// `VmConfig::sig_poly_site_first`): the `invokevirtual` arm tests one bit
    /// before its fast door and probes the table first only when it is set.
    /// No false negatives (a bit is set at every fill and cleared only with
    /// the whole table); a false positive costs one table probe that
    /// declines. All zero for a thread that never filled a site.
    filter: [u64; SITE_FILTER_BITS / 64],
    /// [`basic_entry_method`]'s memo (interpreter round i6 wave 120, lane L4,
    /// `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH`): direct-mapped by the vmentry
    /// `MemberName`'s address, [`BASIC_ENTRY_MEMO_SLOTS`] lines once used, empty
    /// on a thread that never pushed an entry. On the thread's own table, not a
    /// `thread_local!` (`vm/tests/per_vm_state_statics_ratchet.rs`): a
    /// `JvmThread` belongs to one VM.
    ///
    /// One THIN pointer, not a `Vec` (wave 121): `SigPolySiteCache` is inline
    /// in `#[repr(C)]` `JvmThread`, and the 24-byte `Vec` pushed every frozen
    /// row after `sig_poly_sites` (`running_native`, `tlab`, `shadow_stack`,
    /// `jit_monitor_block`, `vh_exact_site_memo`) down 16 in
    /// `runtime::frame::layout_ratchet_tests`. The cache ended 8 bytes short
    /// of the next 16-byte boundary, so these 8 bytes fill that padding and
    /// no frozen offset moves.
    ///
    /// Wave 130 (lane L4): the same box also holds [`exact_link`]'s lines
    /// ([`BasicMemo::exact_links`]), so the pointer stays one thin word.
    basic_entries: Option<Box<BasicMemo>>,
}

/// The boxed memo behind [`SigPolySiteCache::basic_entries`]: the entry
/// templates ([`entry_memo`]) and, since interpreter round i6 wave 130 (lane
/// L4), the interpreted `invokeExact` linker answers ([`exact_link`]).
struct BasicMemo {
    entries: [BasicEntryLine; BASIC_ENTRY_MEMO_SLOTS],
    exact_links: [ExactLinkLine; EXACT_LINK_MEMO_SLOTS],
}

impl BasicMemo {
    fn new() -> Box<Self> {
        Box::new(BasicMemo {
            entries: std::array::from_fn(|_| BasicEntryLine {
                key: (0, 0, 0, 0, 0),
                cached: None,
            }),
            exact_links: [ExactLinkLine::EMPTY; EXACT_LINK_MEMO_SLOTS],
        })
    }
}

/// Lines of [`BasicMemo::exact_links`].
const EXACT_LINK_MEMO_SLOTS: usize = 16;

/// What [`exact_link`] remembers for one (address epoch, site, caller class,
/// handle class).
#[derive(Clone, Copy)]
enum ExactLinkMemo {
    /// Nothing filed.
    Empty,
    /// `lang_invoke::InterpExactLink::DoorForGood`: decline without asking.
    Door,
    /// `lang_invoke::InterpExactLink::Pair`: the shareable site's rooted
    /// `linker:` row `(MemberName, appendix)` and `null_first_ok`. The
    /// addresses are valid in the key's address epoch only.
    Pair(ObjectRef, ObjectRef, bool),
}

/// One line of [`BasicMemo::exact_links`]: key `(address epoch, site key
/// hash, caller class id, handle class id)`.
#[derive(Clone, Copy)]
struct ExactLinkLine {
    key: (u64, u64, u32, u32),
    answer: ExactLinkMemo,
}

impl ExactLinkLine {
    const EMPTY: ExactLinkLine = ExactLinkLine {
        key: (0, 0, 0, 0),
        answer: ExactLinkMemo::Empty,
    };
}

/// The filter bit of a site: `(cp index, invoke pc)`, mixed so neighbouring
/// pcs of one method and equal pcs of different methods spread (the caller
/// class is not mixed in: the arm would need the frame's class id for it).
#[inline(always)]
fn site_filter_bit(cp_index: u16, pc: usize) -> usize {
    let key = (pc as u32) ^ (u32::from(cp_index) << 16) ^ u32::from(cp_index); // Cast: a pc fits 32 bits (code length < 64 KiB)
    (key.wrapping_mul(0x9E37_79B1) >> (32 - SITE_FILTER_BITS.trailing_zeros())) as usize
    // Cast: < SITE_FILTER_BITS
}

impl SigPolySiteCache {
    /// Is the table current? A redefinition since the last probe drops every
    /// site (and answers `false`).
    #[inline]
    fn current(&mut self) -> bool {
        let now = crate::classloading::class_redefinition_count();
        if now != self.redefinitions_seen {
            self.clear_sites();
            self.redefinitions_seen = now;
            return false;
        }
        true
    }

    /// The site, as a new reference (an `Arc` clone: two locked
    /// read-modify-writes per hit, the clone and its drop).
    #[inline]
    fn get(&mut self, key: (ClassId, u16, u32)) -> Option<Arc<SigPolySite>> {
        if !self.current() {
            return None;
        }
        self.sites.get(&key)?.site.clone()
    }

    /// The site, TAKEN out of its entry for the duration of one hit
    /// (`VmConfig::sig_poly_site_lend`; proposal
    /// `interpreter-L7-proposal-the-signature-polymorphic-site-hit-lends-its-site-instead-of-cloning-the-arc-FIXED-20261021`,
    /// interpreter round i2 wave 57, lane L7), with `Some(the redefinition
    /// count it was lent under)`. No atomic: the reference moves out of the
    /// table and [`give_back`](Self::give_back) moves it in again; a nested
    /// `clear_sites` cannot drop the site the outer hit reads.
    ///
    /// While it is out, a nested probe of the SAME key (Java the route's
    /// native ran re-entering its own site) finds the entry empty, declines
    /// to the slow path once, and marks the entry `reentrant`; a reentrant
    /// entry is answered with an `Arc` clone (and `None` for the count), as
    /// without the switch, so a recursion through one site is not sent down
    /// the slow path at every level.
    #[inline]
    fn lend(&mut self, key: (ClassId, u16, u32)) -> Option<(Arc<SigPolySite>, Option<u64>)> {
        if !self.current() {
            return None;
        }
        let slot = self.sites.get_mut(&key)?;
        if slot.reentrant {
            return slot.site.clone().map(|site| (site, None));
        }
        match slot.site.take() {
            Some(site) => Some((site, Some(self.redefinitions_seen))),
            None => {
                slot.reentrant = true;
                None
            }
        }
    }

    /// Return a lent site to its entry (`lent_under` is `lend`'s count; a
    /// clone, `None`, is just dropped). Skipped (the site is dropped) when
    /// the table was cleared since the lend (every clear moves
    /// `redefinitions_seen`: the redefinition count only grows), or when a
    /// nested fill wrote the entry again meanwhile (the newer site stays).
    #[inline]
    fn give_back(
        &mut self,
        key: (ClassId, u16, u32),
        site: Arc<SigPolySite>,
        lent_under: Option<u64>,
    ) {
        if lent_under != Some(self.redefinitions_seen) {
            return;
        }
        if let Some(slot) = self.sites.get_mut(&key) {
            if slot.site.is_none() {
                slot.site = Some(site);
            }
        }
    }

    /// Drop every site, and the filter with them.
    fn clear_sites(&mut self) {
        self.sites.clear();
        self.filter = [0; SITE_FILTER_BITS / 64];
    }

    /// May the site at `(cp_index, pc)` (of any caller) be cached? `false`
    /// is exact; `true` may be a false positive. One load and a bit test.
    #[inline(always)]
    pub(crate) fn may_hold(&self, cp_index: u16, pc: usize) -> bool {
        let bit = site_filter_bit(cp_index, pc);
        self.filter[bit >> 6] & (1u64 << (bit & 63)) != 0
    }

    /// How many sites are cached (tests and diagnostics).
    pub(crate) fn len(&self) -> usize {
        self.sites.len()
    }
}

/// The hash a record and a fill compare: the dispatch's method name and
/// call-site descriptor.
#[inline]
fn site_key_hash(method_name: &str, descriptor: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rustc_hash::FxHasher::default();
    method_name.hash(&mut h);
    descriptor.hash(&mut h);
    h.finish()
}

/// Called by the two slow-path doors immediately before they run a
/// signature-polymorphic native. A no-op unless an interpreter slow path for
/// a site at the CURRENT frame depth is listening, so a dispatch made by Java
/// the slow path ran (a class initializer, a loader upcall: they push frames)
/// cannot be taken for the site's own. The recorded route is a function of
/// the receiver's class, the method name and the descriptor (and the
/// registry / redefinition state the fill stamps), which is what the hit
/// re-checks.
#[inline]
#[allow(clippy::too_many_arguments)]
pub(crate) fn note_route(
    shared: &SharedVm,
    thread: &mut JvmThread,
    receiver: Option<ObjectRef>,
    owner: &str,
    method_name: &str,
    registered_descriptor: &str,
    call_site_descriptor: &str,
    door: SigPolyDoor,
    arm_call_site: bool,
) {
    let SigPolyRecord::Armed { depth } = thread.sig_poly_record else {
        return;
    };
    note_route_armed(
        shared,
        thread,
        depth,
        receiver,
        owner,
        method_name,
        registered_descriptor,
        call_site_descriptor,
        door,
        arm_call_site,
    );
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn note_route_armed(
    shared: &SharedVm,
    thread: &mut JvmThread,
    depth: usize,
    receiver: Option<ObjectRef>,
    owner: &str,
    method_name: &str,
    registered_descriptor: &str,
    call_site_descriptor: &str,
    door: SigPolyDoor,
    arm_call_site: bool,
) {
    if thread.frames.len() != depth {
        return;
    }
    let Some(receiver) = receiver else {
        return;
    };
    if shared.mem.heap.kind_of(receiver) == cratonvm_types::ObjectKind::Array {
        return;
    }
    let receiver_class_id = shared.mem.heap.class_id_of(receiver);
    if receiver_class_id == ClassId::new(0) {
        return;
    }
    let Some(native_id) =
        shared
            .natives
            .native_methods
            .resolve_id(owner, method_name, registered_descriptor)
    else {
        return;
    };
    thread.sig_poly_record = SigPolyRecord::Recorded {
        depth,
        route: SigPolyRoute {
            receiver_class_id,
            native_id,
            door,
            arm_call_site,
        },
        key_hash: site_key_hash(method_name, call_site_descriptor),
    };
}

/// `execute_invoke_kind` for an `invokevirtual` with the route recorder
/// armed: runs the slow path unchanged and, when one of its doors recorded a
/// signature-polymorphic dispatch for THIS site, fills the site's entry.
#[inline(never)]
pub(super) fn execute_invoke_recording(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    pc: usize,
) -> Result<CachedCallResult, MethodCallFailed> {
    let depth = thread.frames.len();
    let redefinitions_as_of = crate::classloading::class_redefinition_count();
    let registry_generation = shared.natives.native_methods.generation();
    let outer = std::mem::replace(&mut thread.sig_poly_record, SigPolyRecord::Armed { depth });
    let result = execute_invoke_kind(shared, thread, frame_idx, cp_index, false, false, pc);
    let record = std::mem::replace(&mut thread.sig_poly_record, outer);
    if let (
        Ok(CachedCallResult::Handled),
        SigPolyRecord::Recorded {
            depth: at,
            route,
            key_hash,
        },
    ) = (&result, record)
    {
        if at == depth && thread.frames.len() == depth {
            fill(
                shared,
                thread,
                frame_idx,
                cp_index,
                pc,
                route,
                key_hash,
                redefinitions_as_of,
                registry_generation,
            );
        }
    }
    result
}

#[cold]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn fill(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    pc: usize,
    route: SigPolyRoute,
    key_hash: u64,
    redefinitions_as_of: u64,
    registry_generation: u32,
) {
    // Nothing the slow path ran may have moved the state the route stands
    // on: a redefinition (the caller's pool, a native shadow) or a new
    // registry triple (a different `find` answer).
    if crate::classloading::class_redefinition_count() != redefinitions_as_of
        || shared.natives.native_methods.generation() != registry_generation
    {
        return;
    }
    let frame = &thread.frames[frame_idx];
    if frame.runs_obsolete_method() {
        return;
    }
    let caller = frame.class_id;
    let Ok((owner, method_name, descriptor, num_params)) =
        resolve_method_ref(shared, caller, cp_index)
    else {
        return;
    };
    if site_key_hash(&method_name, &descriptor) != key_hash
        || !crate::vm::vm_exec::is_signature_polymorphic_method_name(&method_name)
    {
        return;
    }
    let is_var_handle_site =
        crate::vm::vm_exec::is_var_handle_signature_polymorphic_receiver(&owner);
    if !is_var_handle_site
        && !crate::vm::vm_exec::is_method_handle_signature_polymorphic_receiver(&owner)
    {
        return;
    }
    let facts = cratonvm_jit_api::DescriptorFacts::of(&descriptor);
    if facts.param_tags_overflow
        || facts.param_tag_len as usize != num_params
        || num_params + 1 > MAX_CACHED_NATIVE_ARGS
    {
        return;
    }
    let cache = &mut thread.sig_poly_sites;
    if cache.redefinitions_seen != redefinitions_as_of {
        cache.clear_sites();
        cache.redefinitions_seen = redefinitions_as_of;
    }
    let key = (caller, cp_index, pc as u32);
    let mut routes: smallvec::SmallVec<[SigPolyRoute; ROUTES_PER_SITE]> = smallvec::SmallVec::new();
    // A LENT entry (`site: None`: this fill runs inside the hit that lent
    // it) contributes no routes, and its site stays `reentrant` (the outer
    // hit's give-back then keeps this fill).
    let reentrant = cache
        .sites
        .get(&key)
        .is_some_and(|slot| slot.reentrant || slot.site.is_none());
    if let Some(SigPolySlot {
        site: Some(old), ..
    }) = cache.sites.get(&key)
    {
        if old.registry_generation == registry_generation
            && *old.method_name == *method_name
            && *old.descriptor == *descriptor
        {
            routes.extend(
                old.routes
                    .iter()
                    .copied()
                    .filter(|r| r.receiver_class_id != route.receiver_class_id),
            );
        }
    }
    if routes.len() >= ROUTES_PER_SITE {
        // Full: the classes already served keep their routes.
        return;
    }
    routes.push(route);
    // MH16-6 (JIT round 16 wave 7, lane varhandle): the site's CAS operand
    // kind, parsed once here rather than per hit; read only when
    // `VmConfig::vh_interp_cas_fast` is on.
    //
    // Interpreter round i2 wave 53 (lane L7): the four weak CAS access modes
    // too. Each is `(receiver, expected, new)Z` over the same variable with
    // the same refusals; a weak CAS is permitted to fail spuriously, never
    // required to, and its ordering is at most the strong one's, so the one
    // barriered strong CAS is a valid answer for each (the registered
    // natives answer them with the same strong CAS). `=0` of
    // `CRATONVM_VH_INTERP_CAS_FAST` withdraws all five.
    let vh_cas_site_kind = if is_var_handle_site
        && facts.ret_tag == b'Z'
        && is_boolean_cas_access_mode(&method_name)
    {
        crate::jit::helpers::varhandle_cas_site_kind(&descriptor)
    } else {
        None
    };
    // Interpreter round i2 wave 54 (lane L7): the exchange forms over an `int`
    // or `long` variable. A SeqCst exchange is a valid answer for the
    // `Acquire` and `Release` forms (its ordering is stronger). Read only
    // when `VmConfig::vh_interp_cae_fast` is on.
    let vh_cae_site_kind = if is_var_handle_site && is_exchange_access_mode(&method_name) {
        crate::jit::helpers::varhandle_cae_site_kind(&descriptor, facts.ret_tag)
    } else {
        None
    };
    // Set whether or not the early probe is on: one store per fill.
    let bit = site_filter_bit(cp_index, pc);
    cache.filter[bit >> 6] |= 1u64 << (bit & 63);
    cache.sites.insert(
        key,
        SigPolySlot {
            site: Some(Arc::new(SigPolySite {
                method_name,
                descriptor,
                facts,
                num_params,
                is_var_handle_site,
                vh_cas_site_kind,
                vh_cae_site_kind,
                registry_generation,
                routes,
            })),
            reentrant,
        },
    );
}

/// The `VarHandle` access modes whose answer is the witness of one
/// compare-and-exchange: `compareAndExchange` and its `Acquire` / `Release`
/// forms (the instance-field exchange arm, wave 54, lane L7).
#[inline]
fn is_exchange_access_mode(method_name: &str) -> bool {
    matches!(
        method_name,
        "compareAndExchange" | "compareAndExchangeAcquire" | "compareAndExchangeRelease"
    )
}

/// The `VarHandle` access modes whose answer is "did the CAS store": the
/// strong `compareAndSet` and its four weak forms. The site cache's
/// instance-field CAS arm serves each with one strong barriered CAS.
#[inline]
fn is_boolean_cas_access_mode(method_name: &str) -> bool {
    matches!(
        method_name,
        "compareAndSet"
            | "weakCompareAndSet"
            | "weakCompareAndSetPlain"
            | "weakCompareAndSetAcquire"
            | "weakCompareAndSetRelease"
    )
}

/// The hit: `None` declines to the general path (which is then exactly what
/// ran before this cache existed). Called from the `invokevirtual` arm after
/// the fast door and the cached dispatcher missed.
#[inline]
pub(super) fn try_hit(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    pc: usize,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if thread.sig_poly_sites.sites.is_empty() {
        return None;
    }
    try_hit_nonempty(shared, thread, frame_idx, cp_index, pc)
}

/// The probe AHEAD of the fast door and the cached dispatcher (interpreter
/// round i2 wave 54, lane L7; proposal
/// `i53-L7-proposal-signature-polymorphic-sites-probe-their-route-first`;
/// `VmConfig::sig_poly_site_first`, `CRATONVM_SIG_POLY_SITE_FIRST`). The
/// `invokevirtual` arm calls it only when [`SigPolySiteCache::may_hold`]
/// says the site may be cached. A signature-polymorphic site never has an
/// invoke-cache entry (its descriptor names no method), so the door and the
/// cached dispatcher could only miss before the route cache answered; a
/// decline here leaves the arm's old order to run unchanged (and the arm then
/// skips its own `try_hit`, which would probe the same key again).
#[inline(never)]
pub(super) fn try_hit_first(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    pc: usize,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    let answer = try_hit_nonempty(shared, thread, frame_idx, cp_index, pc);
    super::site_cache::site_stats::bump(if answer.is_some() {
        super::site_cache::site_stats::SIG_POLY_FIRST_HIT
    } else {
        super::site_cache::site_stats::SIG_POLY_FIRST_DECLINE
    });
    answer
}

#[inline(never)]
fn try_hit_nonempty(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    cp_index: u16,
    pc: usize,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    let caller = thread.frames[frame_idx].class_id;
    let key = (caller, cp_index, pc as u32);
    if shared.config.sig_poly_site_lend {
        // Wave 57 (lane L7): the site is moved out of its entry for the hit
        // and moved back after it, every exit of the hit included (the
        // declines and `run_route`'s `?` returns all come back here).
        let (site, lent_under) = thread.sig_poly_sites.lend(key)?;
        let answer = try_hit_site(shared, thread, frame_idx, &site);
        thread.sig_poly_sites.give_back(key, site, lent_under);
        if answer.is_some() && lent_under.is_some() {
            super::site_cache::site_stats::bump_if(
                shared.config.site_census,
                super::site_cache::site_stats::SIG_POLY_LENT_HIT,
            );
        }
        return answer;
    }
    let site = thread.sig_poly_sites.get(key)?;
    try_hit_site(shared, thread, frame_idx, &site)
}

/// The hit's body over a site the caller holds (a lent or a cloned
/// reference): `None` declines to the general path.
#[inline(always)]
fn try_hit_site(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    site: &SigPolySite,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    let frame = &thread.frames[frame_idx];
    if frame.runs_obsolete_method() {
        return None;
    }
    let registry = &shared.natives.native_methods;
    if registry.generation() != site.registry_generation {
        return None;
    }
    let Value::Object(Some(receiver)) = frame.stack.peek_at(site.num_params) else {
        // Null (the slow path owns the NPE text) or not a reference.
        return None;
    };
    // One validation for the three header reads (interpreter round i2 wave
    // 53, lane L7): `load_and_forward`, `kind_of` and `class_id_of` each
    // walked the conservative validator on every hit. A receiver it refuses
    // is the slow path's (which reports it), as a `ClassId(0)` that matched
    // no route was before.
    let heap = &shared.mem.heap;
    let (receiver, valid) = heap.load_and_forward_checked(receiver);
    if !valid || heap.kind_of_validated(receiver) == cratonvm_types::ObjectKind::Array {
        return None;
    }
    let receiver_class_id = heap.class_id_of_validated(receiver);
    let route = *site
        .routes
        .iter()
        .find(|r| r.receiver_class_id == receiver_class_id)?;
    let callback = registry.callback_of(route.native_id)?;
    // Interpreter round i6 wave 120, lane L4 (`CRATONVM_MH_INTERP_BASIC_FRAME_PUSH`,
    // default off, `--nojit` only): an `invokeBasic` on a real JDK handle
    // pushes the handle's `form.vmentry` frame instead of the native door's
    // re-entry ([`try_basic_frame_push`]).
    if shared.config.mh_interp_basic_frame_push && !shared.config.jit_enabled {
        if let Some(answer) = try_basic_frame_push(shared, thread, frame_idx, site, receiver) {
            return Some(answer);
        }
    }
    dbg_invoke_stats_record(shared.config.invoke_stats, 4);
    Some(run_route(shared, thread, frame_idx, site, route, callback))
}

/// Interpreter round i6 wave 120, lane L4 (`CRATONVM_MH_INTERP_BASIC_FRAME_PUSH`,
/// `VmConfig::mh_interp_basic_frame_push`, default off; read only without a
/// JIT; page `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`): an
/// interpreted `invokevirtual MethodHandle.invokeBasic` whose receiver is a
/// real JDK handle is HotSpot's `vmIntrinsics::_invokeBasic`: load
/// `mh.form.vmentry` and jump to its method. This pushes that method's frame
/// (a static `LambdaForm` entry taking the handle and the site's values)
/// into this dispatch loop, where the registered native's door re-entered
/// `interpreter::execute` (a Rust recursion, ~5 us per crossing on the i110
/// census) for every `invokeBasic` of an interpreted `LambdaForm` chain: the
/// spread invoker, the `asType` adapter, a bound handle's reinvoker.
///
/// Taken only when the door would take the same entry first
/// (`lang_invoke::mh_compiled_basic_vmentry`: not a carrier, not a downcall,
/// the switches that serve `vmentry` on) and the entry is provably what the
/// door would run: a static `MemberName` (`REF_invokeStatic`) decoded by
/// `linkTo*`'s own memoised decode (`mh_compiled_member`), its owner
/// initialized (`bind_by_id`), not synchronized, with a `Code` attribute, its
/// parameters the handle then the site's values in the same basic types, its
/// return the site's. Hidden spun `LambdaForm$MH` classes are found by their
/// id, never by name. Anything else declines to the route's native call, with
/// nothing popped. Frames are the ones the door's re-entry pushed (the same
/// methods), so walks, traces and exceptions are unchanged; what goes away is
/// the native frame between them and its re-entry cost.
///
/// `--nojit` only: under a JIT the LambdaForm bodies compile through the
/// door's `execute` entry (whose counter a pushed frame would not bump), and
/// compiled callers link them themselves (`CRATONVM_JIT_MH_COMPILED_LINK`,
/// L2's `_BY_ID` for spun forms).
fn try_basic_frame_push(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    site: &SigPolySite,
    receiver: ObjectRef,
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    if site.is_var_handle_site {
        return None;
    }
    // Wave 120 second follow-up (lane L4; i114-L7's reflective rows): an
    // interpreted `invokeExact` whose `(linker, appendix)` pair the door has
    // filed (`lang_invoke::mh_compiled_linker_pair`, the compiled sites' own
    // proof that the door runs that linker first) pushes the linker's frame
    // (`Invokers$Holder.invokeExact_MT(handle, values.., appendix)`).
    let (cached, appendix) = match &*site.method_name {
        "invokeBasic" => (basic_entry_method(shared, thread, receiver, &site.descriptor)?, None),
        "invokeExact" => {
            let caller = thread.frames[frame_idx].class_id;
            let first_is_null = site.num_params > 0
                && matches!(
                    thread.frames[frame_idx].stack.peek_at(site.num_params - 1),
                    Value::Object(None)
                );
            // The handle as the operand stack holds it NOW (the template's
            // decode between the two asks may have moved it).
            let linker = |thread: &mut JvmThread| {
                let Value::Object(Some(handle)) =
                    thread.frames[frame_idx].stack.peek_at(site.num_params)
                else {
                    return None;
                };
                let handle = shared.mem.heap.load_and_forward(handle);
                exact_link(shared, thread, handle, site, caller)
            };
            let (member, appendix, null_first_ok) = linker(thread)?;
            if first_is_null && !null_first_ok {
                return None;
            }
            // Wave 130 (lane L4): the appendix of the first ask is still
            // current when no collection ran in the template's decode (an
            // `entry_memo` hit runs none); the second ask is kept for the
            // decode that did.
            let epoch = shared.mem.address_epoch();
            let cached = entry_method_memoized(
                shared,
                thread,
                member,
                "invokeExact",
                &site.descriptor,
                (1, 0, 1),
            )?;
            let appendix = if shared.mem.address_epoch() == epoch {
                appendix
            } else {
                // The appendix read again after the template's decode: a
                // rooted row of the VM's table, current now.
                linker(thread)?.1
            };
            (cached, Some(appendix))
        }
        _ => return None,
    };
    if thread
        .frames
        .at_frame_limit(0, shared.config.max_stack_depth)
    {
        // The door's own path raises it (with its trace): decline.
        return None;
    }
    // The receiver, the site's values and the appendix must fit the buffer.
    if site.num_params + 1 + usize::from(appendix.is_some()) > MAX_CACHED_NATIVE_ARGS {
        return None;
    }
    let mut buf = [Value::Uninitialized; MAX_CACHED_NATIVE_ARGS];
    let n = match pop_coerced_invoke_args_virtual_facts(
        shared,
        frame_idx,
        thread,
        &site.facts,
        site.num_params,
        &mut buf,
    ) {
        Ok(n) => n,
        Err(e) => return Some(Err(e)),
    };
    // The linker's trailing appendix (`invokeExact`); the buffer holds the
    // receiver and the site's values (`n <= num_params + 1`).
    // (Room for it was checked before the pop.)
    let n = match appendix {
        Some(appendix) => {
            // Wave 121 (lane L9): forwarded as the popped values were
            // (`refresh_stale_object_args`), so the appendix the frame gets
            // is current under the same rule as the handle beside it.
            buf[n] = Value::Object(Some(shared.mem.heap.load_and_forward(appendix)));
            n + 1
        }
        None => n,
    };
    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        cached.max_locals as usize, // Widening: u16 -> usize
        (cached.max_stack as usize).max(16) + 8, // Widening: u16 -> usize
    );
    install_cached_frame(
        shared,
        thread,
        cached,
        &buf[..n],
        None,
        Some("sig_poly_basic"),
        false,
    );
    Some(Ok(CachedCallResult::FramePushed))
}

/// Interpreter round i6 wave 120 follow-up, lane L4 (the same
/// `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH`, `--nojit` only; page
/// `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`): an interpreted
/// `invokestatic MethodHandle.linkToStatic(args.., MemberName)` -- what a
/// `DirectMethodHandle$Holder` / `LambdaForm$DMH` form's bytecode ends in -- is
/// HotSpot's `_linkToStatic`: jump to the `MemberName`'s method. This pushes that
/// method's frame with `args` minus the trailing `MemberName`, where
/// `native_method_handle_link_to` re-entered `interpreter::execute`.
///
/// Taken only for what `link_to`'s static arm would run with nothing in
/// between: a `REF_invokeStatic` member decoded by its own memo
/// (`mh_compiled_member`; not a lambda proxy's), an initialized owner (that arm
/// initializes first), a static, unsynchronized method with `Code` and no
/// registered native in front of it, the site's values in the member's basic
/// types. `None` declines (the native runs as before, the arguments untouched).
/// `args` are the popped call arguments, the `MemberName` last; `descriptor` is
/// the `invokestatic`'s own.
pub(super) fn link_to_static_frame_push(
    shared: &SharedVm,
    thread: &mut JvmThread,
    descriptor: &str,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    let (Value::Object(Some(member)), Some(call_args)) =
        (args.last().copied()?, args.get(..args.len().checked_sub(1)?))
    else {
        return None;
    };
    let cached =
        entry_method_memoized(shared, thread, member, "linkToStatic", descriptor, (0, 1, 0))?;
    if thread
        .frames
        .at_frame_limit(0, shared.config.max_stack_depth)
    {
        return None;
    }
    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        cached.max_locals as usize, // Widening: u16 -> usize
        (cached.max_stack as usize).max(16) + 8, // Widening: u16 -> usize
    );
    install_cached_frame(
        shared,
        thread,
        cached,
        call_args,
        None,
        Some("link_to_static_push"),
        false,
    );
    Some(Ok(CachedCallResult::FramePushed))
}

/// One line of `SigPolySiteCache::basic_entries` ([`basic_entry_method`]'s
/// memo): the entry `MemberName`'s address in one collection epoch (an equal
/// address there is the same, final-fielded `MemberName`), the redefinition
/// count (a redefined owner rebuilds its template), the call's own name and
/// site descriptor (hashed), and the answer, a decline included. The table is
/// the `JvmThread`'s, so it is one VM's.
struct BasicEntryLine {
    key: (u64, u64, usize, u64, u32),
    cached: Option<Arc<crate::classloading::resolution::CachedBytecodeMethod>>,
}

const BASIC_ENTRY_MEMO_SLOTS: usize = 32;

/// A descriptor token's JVM basic type: `L` for a reference or array, `I` for
/// the subword ints, else the token's own letter.
fn basic_type_of(token: &str) -> u8 {
    match token.as_bytes().first() {
        Some(b'L' | b'[') => b'L',
        Some(b'Z' | b'B' | b'S' | b'C' | b'I') => b'I',
        Some(&c) => c,
        None => 0,
    }
}

/// The frame template of the `invokeBasic` entry of `receiver` for a site of
/// descriptor `site_desc` (see [`try_basic_frame_push`]), or `None`.
fn basic_entry_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    receiver: ObjectRef,
    site_desc: &str,
) -> Option<Arc<crate::classloading::resolution::CachedBytecodeMethod>> {
    let entry = {
        let ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        cratonvm_native_builtins::lang_invoke::mh_compiled_basic_vmentry(&ctx, receiver)?
    };
    entry_method_memoized(shared, thread, entry, "invokeBasic", site_desc, (1, 0, 0))
}

/// The frame template of the static entry `MemberName` `entry` called as
/// `call_name` from a site of descriptor `site_desc` that passes `lead` extra
/// leading references (the handle, for `invokeBasic`) and `trail` trailing
/// ones (the `MemberName`, for `linkToStatic`) besides the entry's own
/// parameters: from the thread's memo (`SigPolySiteCache::basic_entries`),
/// else [`entry_method_uncached`], filed either way (a decline too).
fn entry_method_memoized(
    shared: &SharedVm,
    thread: &mut JvmThread,
    entry: ObjectRef,
    call_name: &str,
    site_desc: &str,
    shape: (usize, usize, usize),
) -> Option<Arc<crate::classloading::resolution::CachedBytecodeMethod>> {
    entry_memo(shared, thread, entry, call_name, site_desc, 0, |shared, ctx| {
        entry_method_uncached(shared, ctx, entry, site_desc, shape)
    })
}

/// The thread's entry memo (`SigPolySiteCache::basic_entries`) around a
/// decode: the answer for (`entry`, `call_name`, `site_desc`, `receiver_class`
/// (`0` for a static entry)) in this collection epoch and redefinition count,
/// else `decode`'s, filed either way (a decline too).
fn entry_memo(
    shared: &SharedVm,
    thread: &mut JvmThread,
    entry: ObjectRef,
    call_name: &str,
    site_desc: &str,
    receiver_class: u32,
    decode: impl FnOnce(
        &SharedVm,
        &mut crate::vm::NativeContextImpl<'_>,
    ) -> Option<Arc<crate::classloading::resolution::CachedBytecodeMethod>>,
) -> Option<Arc<crate::classloading::resolution::CachedBytecodeMethod>> {
    let key = (
        // `NativeContext::gc_address_epoch`'s own answer.
        shared.mem.address_epoch(),
        crate::classloading::class_redefinition_count(),
        entry.as_ptr() as usize,
        // The answer depends on the call and its site's descriptor too (the
        // basic-type agreement).
        site_key_hash(call_name, site_desc),
        // And, for a virtual entry, on the receiver's class (the selection).
        receiver_class,
    );
    let slot = ((key.2 >> 3) ^ receiver_class as usize) & (BASIC_ENTRY_MEMO_SLOTS - 1); // Widening: u32 -> usize
    // A remembered answer, a decline included (`Some(None)`).
    if let Some(memo) = thread.sig_poly_sites.basic_entries.as_deref() {
        let line = &memo.entries[slot];
        if line.key == key {
            return line.cached.clone();
        }
    }
    let answer = {
        let mut ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        decode(shared, &mut ctx)
    };
    let memo = thread
        .sig_poly_sites
        .basic_entries
        .get_or_insert_with(BasicMemo::new);
    memo.entries[slot] = BasicEntryLine {
        key,
        cached: answer.clone(),
    };
    answer
}

/// Interpreter round i6 wave 130, lane L4 (the same
/// `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH`, `--nojit` only; page
/// `i110-L4-jdk-invoke-with-arguments-body-costs-35-us-a-call`, "Wave 130"):
/// the linker pair an interpreted `invokeExact` on `handle` pushes the frame
/// of, or `None` (the door). `lang_invoke::mh_interp_exact_link` decides, and
/// its stable answers are remembered per (address epoch, site, caller class,
/// handle class) in [`BasicMemo::exact_links`]:
///
/// * a door decision for good (a before-linker lane serves the handle's class,
///   or the site can hold no row): declined at once afterwards, where every
///   call re-parsed the site descriptor (+220 ns a call on the w124
///   `getter-warm-exact` row);
/// * a shareable site's rooted `linker:` row: answered from the line after
///   [`lang_invoke::mh_interp_exact_handle_admits`] holds for this very
///   handle (the per-object half of the admission), where every call formatted
///   the row key and probed the VM's table.
///
/// A transient door (no row filed yet) and a user-class site's pair are asked
/// again on every call. `handle` is current (forwarded).
fn exact_link(
    shared: &SharedVm,
    thread: &mut JvmThread,
    handle: ObjectRef,
    site: &SigPolySite,
    caller: ClassId,
) -> Option<(ObjectRef, ObjectRef, bool)> {
    use cratonvm_native_builtins::lang_invoke::{
        mh_interp_exact_handle_admits, mh_interp_exact_link, InterpExactLink,
    };
    let handle_class = shared.mem.heap.class_id_of(handle).as_u32();
    let key = (
        shared.mem.address_epoch(),
        site_key_hash("invokeExact", &site.descriptor),
        caller.as_u32(),
        handle_class,
    );
    let slot = (key.1 as usize ^ handle_class as usize ^ (caller.as_u32() as usize).rotate_left(5)) // Cast: a hash's low bits; Widening: u32 -> usize
        & (EXACT_LINK_MEMO_SLOTS - 1);
    if let Some(memo) = thread.sig_poly_sites.basic_entries.as_deref() {
        let line = memo.exact_links[slot];
        if line.key == key {
            match line.answer {
                ExactLinkMemo::Door => return None,
                ExactLinkMemo::Pair(member, appendix, null_first_ok) => {
                    let ctx = crate::vm::NativeContextImpl {
                        shared,
                        thread: &mut *thread,
                    };
                    return mh_interp_exact_handle_admits(&ctx, handle)
                        .then_some((member, appendix, null_first_ok));
                }
                ExactLinkMemo::Empty => {}
            }
        }
    }
    let answer = {
        let ctx = crate::vm::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        mh_interp_exact_link(&ctx, handle, &site.descriptor, caller.as_u32())
    };
    let (filed, out) = match answer {
        InterpExactLink::Pair(member, appendix, null_first_ok) => (
            ExactLinkMemo::Pair(member, appendix, null_first_ok),
            Some((member, appendix, null_first_ok)),
        ),
        InterpExactLink::PairOnce(member, appendix, null_first_ok) => {
            return Some((member, appendix, null_first_ok));
        }
        InterpExactLink::Door => return None,
        InterpExactLink::DoorForGood => (ExactLinkMemo::Door, None),
    };
    let memo = thread
        .sig_poly_sites
        .basic_entries
        .get_or_insert_with(BasicMemo::new);
    memo.exact_links[slot] = ExactLinkLine {
        key,
        answer: filed,
    };
    out
}

/// Interpreter round i6 wave 121, lane L4 (the same
/// `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH`, `--nojit` only; i114-L7's
/// `reflect-virtual` row): an interpreted `invokestatic MethodHandle.linkToVirtual`
/// / `linkToInterface(receiver, args.., MemberName)` pushes the frame of the
/// method the native's `invoke_virtual_declared` would run for this receiver,
/// where `native_method_handle_link_to` re-entered `interpreter::execute`.
///
/// The selection is the interpreter's own: `selection::select_for_receiver_dispatch`
/// with the member's declaring class as the owner when it is on the receiver's
/// superclass chain (`linkToVirtual`; `vm_exec::declared_dispatch_selection`'s
/// rule, its `CRATONVM_REFLECTIVE_DISPATCH_SELECTS` kill switch honoured), the
/// lenient receiver walk otherwise (`linkToInterface`, as the native's
/// `invoke_virtual` falls back). Declined to the native, nothing popped:
/// a null receiver (its message-less `NullPointerException` stays the
/// native's own, raised exactly as before), an array, a lambda proxy, a
/// receiver whose selection is a §5.4.6 error (`receiver_selection_error`:
/// the native raises it), a selected method that is static, abstract, native,
/// synchronized, an interface's (default) method, without `Code`, or shadowed
/// by any registered native or interpreter intrinsic from the receiver's class
/// up to its declaring class (`populate_virtual_invoke_cache`'s
/// `native_override_below_declaring` rule), the `Object` identity members
/// (`hashCode` / `equals` / `toString`, which records answer by intrinsic),
/// and a site whose values do not match the member's basic types.
pub(super) fn link_to_virtual_frame_push(
    shared: &SharedVm,
    thread: &mut JvmThread,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<Result<CachedCallResult, MethodCallFailed>> {
    let (Value::Object(Some(member)), Some(call_args)) =
        (args.last().copied()?, args.get(..args.len().checked_sub(1)?))
    else {
        return None;
    };
    let Value::Object(Some(receiver)) = call_args.first().copied()? else {
        return None;
    };
    let heap = &shared.mem.heap;
    let receiver = heap.load_and_forward(receiver);
    if heap.kind_of(receiver) == cratonvm_types::ObjectKind::Array {
        return None;
    }
    let receiver_class = heap.class_id_of(receiver);
    if receiver_class == ClassId::new(0) || receiver_class.as_u32() >= 0x8000_0000 {
        return None;
    }
    let virtual_kind = match method_name {
        "linkToVirtual" => 5,
        "linkToInterface" => 9,
        _ => return None,
    };
    let cached = entry_memo(
        shared,
        thread,
        member,
        method_name,
        descriptor,
        receiver_class.as_u32(),
        |shared, ctx| {
            virtual_entry_uncached(shared, ctx, member, receiver_class, virtual_kind, descriptor)
        },
    )?;
    if thread
        .frames
        .at_frame_limit(0, shared.config.max_stack_depth)
    {
        return None;
    }
    thread.refill_pools_from_shared(
        &shared.mem.operand_stack_pool,
        &shared.mem.tag_pool,
        cached.max_locals as usize, // Widening: u16 -> usize
        (cached.max_stack as usize).max(16) + 8, // Widening: u16 -> usize
    );
    install_cached_frame(
        shared,
        thread,
        cached,
        call_args,
        None,
        Some("link_to_virtual_push"),
        false,
    );
    Some(Ok(CachedCallResult::FramePushed))
}

/// [`link_to_virtual_frame_push`]'s decode for one receiver class.
fn virtual_entry_uncached(
    shared: &SharedVm,
    ctx: &mut crate::vm::NativeContextImpl<'_>,
    member_obj: ObjectRef,
    receiver_class: ClassId,
    virtual_kind: i32,
    site_desc: &str,
) -> Option<Arc<crate::classloading::resolution::CachedBytecodeMethod>> {
    let member = cratonvm_native_builtins::mh_compiled_member(ctx, member_obj)?;
    if member.ref_kind() != virtual_kind {
        return None;
    }
    // The site: the receiver, the member's values, the `MemberName`.
    let (site_params, site_ret) = split_method_descriptor_ref(site_desc);
    let (member_params, member_ret) = split_method_descriptor_ref(member.desc());
    if site_params.len() != member_params.len() + 2
        || basic_type_of(site_params[0]) != b'L'
        || basic_type_of(site_params[site_params.len() - 1]) != b'L'
        || basic_type_of(member_ret) != basic_type_of(site_ret)
        || member_params
            .iter()
            .zip(&site_params[1..site_params.len() - 1])
            .any(|(m, s)| basic_type_of(m) != basic_type_of(s))
    {
        return None;
    }
    let bind = member.bind_by_id(&*ctx, 0)?;
    let (name, desc) = (bind.name.as_str(), bind.desc.as_str());
    if matches!(name, "hashCode" | "equals" | "toString") {
        return None;
    }
    let declared = ClassId::new(bind.class_id);
    let cm = shared.classes.class_manager.read();
    let store = &cm.class_store;
    // The declared class on the receiver's own chain (by id), as
    // `declared_dispatch_selection` finds it by name; the kill switch as there.
    let owner_on_chain = virtual_kind == 5
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_REFLECTIVE_DISPATCH_SELECTS")
        && {
            let mut cursor = Some(receiver_class);
            let mut steps = 0usize;
            let mut found = false;
            while let Some(id) = cursor {
                steps += 1;
                if id == declared {
                    found = true;
                    break;
                }
                if steps > 512 {
                    break;
                }
                cursor = store.get(id).and_then(|c| c.superclass);
            }
            found
        };
    if owner_on_chain
        && crate::runtime::resolve::selection::receiver_selection_error(
            store,
            receiver_class,
            declared,
            name,
            desc,
        )
        .is_some()
    {
        return None;
    }
    let (method, declaring) = crate::runtime::resolve::selection::select_for_receiver_dispatch(
        store,
        receiver_class,
        owner_on_chain.then_some(declared),
        name,
        desc,
    )?;
    if method.is_static()
        || method.is_abstract()
        || method.is_native()
        || method.is_synchronized()
    {
        return None;
    }
    let declaring_class = store.get(declaring)?;
    if declaring_class.is_interface() {
        return None;
    }
    // No registered native or interpreter intrinsic anywhere from the
    // receiver's class up to the declaring one.
    if cratonvm_native_builtins::intrinsics::lookup(&declaring_class.name, name, desc).is_some() {
        return None;
    }
    let mut cursor = Some(receiver_class);
    while let Some(id) = cursor {
        let class = store.get(id)?;
        if shared
            .natives
            .native_methods
            .find(&class.name, name, desc)
            .is_some()
        {
            return None;
        }
        if id == declaring {
            break;
        }
        cursor = class.superclass;
    }
    let code_attr = method.code()?;
    Some(Arc::new(
        crate::classloading::resolution::CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: declaring,
                class_name: Arc::clone(&declaring_class.name),
                method_name: Arc::clone(&method.name),
                method_descriptor: Arc::clone(&method.descriptor),
                source_file: declaring_class.source_file.as_deref().map(Arc::from),
                code: crate::runtime::frame::padded_bytecode_for_method(
                    declaring,
                    &method.name,
                    &method.descriptor,
                    &code_attr.code,
                    &code_attr.exception_table,
                ),
                exception_table: Arc::from(code_attr.exception_table.as_slice()),
                max_stack: code_attr.max_stack,
                max_locals: code_attr.max_locals,
                num_params: member_params.len() as u16, // Cast: at most 255 parameters (JVMS §4.3.3)
                is_synchronized: false,
                is_static: false,
            },
        )
        .with_pool_generation(crate::classloading::class_redefinition_count()),
    ))
}

/// [`entry_method_memoized`]'s decode of the static entry `MemberName`
/// `entry`. The site passes `lead` references the entry takes as its first
/// parameters beyond the site's own (`invokeBasic` / `invokeExact`: the handle
/// is the receiver, not in the site descriptor, and is the entry's first
/// parameter), `trail` trailing site parameters the entry does not take
/// (`linkToStatic`: the `MemberName`), and the entry takes `member_trail`
/// trailing references the site does not pass (the linker's `MethodType`
/// appendix); everything else must agree in basic type, the return included.
fn entry_method_uncached(
    shared: &SharedVm,
    ctx: &mut crate::vm::NativeContextImpl<'_>,
    entry: ObjectRef,
    site_desc: &str,
    (lead, trail, member_trail): (usize, usize, usize),
) -> Option<Arc<crate::classloading::resolution::CachedBytecodeMethod>> {
    let member = cratonvm_native_builtins::mh_compiled_member(ctx, entry)?;
    if member.ref_kind() != 6 {
        return None;
    }
    let (site_params, site_ret) = split_method_descriptor_ref(site_desc);
    let (member_params, member_ret) = split_method_descriptor_ref(member.desc());
    // The site's own values (its trailing `trail` excluded) against the
    // member's core (its leading `lead` and trailing `member_trail` excluded).
    let site_core = site_params.len().checked_sub(trail)?;
    let core = member_params.len().checked_sub(lead + member_trail)?;
    if core != site_core
        || member_params[..lead].iter().any(|p| basic_type_of(p) != b'L')
        || member_params[lead + core..]
            .iter()
            .any(|p| basic_type_of(p) != b'L')
        || site_params[site_core..]
            .iter()
            .any(|p| basic_type_of(p) != b'L')
        || basic_type_of(member_ret) != basic_type_of(site_ret)
        || member_params[lead..lead + core]
            .iter()
            .zip(&site_params[..site_core])
            .any(|(m, s)| basic_type_of(m) != basic_type_of(s))
    {
        return None;
    }
    let bind = member.bind_by_id(&*ctx, 3)?;
    let owner = ClassId::new(bind.class_id);
    let cached = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(owner)?;
        let method = class
            .methods
            .iter()
            .find(|m| &*m.name == bind.name.as_str() && &*m.descriptor == bind.desc.as_str())?;
        if !method.is_static() || method.is_synchronized() {
            return None;
        }
        // A registered native in front of the entry (an `Intrinsic` on a
        // `$Holder` method) is what the door's `invoke_static` would run:
        // leave such an entry to the door.
        if shared
            .natives
            .native_methods
            .find(&class.name, &method.name, &method.descriptor)
            .is_some()
        {
            return None;
        }
        let code_attr = method.code()?;
        Arc::new(
            crate::classloading::resolution::CachedBytecodeMethod::from_parts(
                cratonvm_jit_api::CachedMethodParts {
                    declaring_class_id: owner,
                    class_name: Arc::clone(&class.name),
                    method_name: Arc::clone(&method.name),
                    method_descriptor: Arc::clone(&method.descriptor),
                    source_file: class.source_file.as_deref().map(Arc::from),
                    code: crate::runtime::frame::padded_bytecode_for_method(
                        owner,
                        &method.name,
                        &method.descriptor,
                        &code_attr.code,
                        &code_attr.exception_table,
                    ),
                    exception_table: Arc::from(code_attr.exception_table.as_slice()),
                    max_stack: code_attr.max_stack,
                    max_locals: code_attr.max_locals,
                    num_params: member_params.len() as u16, // Cast: at most 255 parameters (JVMS §4.3.3)
                    is_synchronized: false,
                    is_static: true,
                },
            )
            .with_pool_generation(crate::classloading::class_redefinition_count()),
        )
    };
    Some(cached)
}

/// Replays what the recorded door does from its native call on, over
/// arguments popped exactly as `execute_invoke_kind` pops them.
fn run_route(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    site: &SigPolySite,
    route: SigPolyRoute,
    callback: cratonvm_native_api::NativeCallback,
) -> Result<CachedCallResult, MethodCallFailed> {
    let mut buf = [Value::Uninitialized; MAX_CACHED_NATIVE_ARGS];
    let n = pop_coerced_invoke_args_virtual_facts(
        shared,
        frame_idx,
        thread,
        &site.facts,
        site.num_params,
        &mut buf,
    )?;
    let args = &buf[..n];
    // `execute_invoke_kind`'s invoke-exact `VarHandle` gate (wave 37, lane
    // L4), which runs before either door.
    if site.is_var_handle_site {
        if let Some(exact_slot) = var_handle_exact_gate(shared) {
            if let Some(Value::Object(Some(vh))) = args.first().copied() {
                if var_handle_receiver_is_exact(shared, vh, exact_slot) {
                    if let Some(refusal) = var_handle_exact_refusal(
                        shared,
                        thread,
                        vh,
                        &site.method_name,
                        &site.descriptor,
                    ) {
                        return Err(refusal);
                    }
                }
            }
        }
    }
    let ret = site.facts.ret_tag;
    match route.door {
        SigPolyDoor::StacklessMethodHandle { sync } => {
            if route.arm_call_site {
                arm_site_descriptor(shared, site);
            }
            let result = safe_native_call_synchronized(shared, thread, callback, args, sync)?;
            if let Some(value) = result {
                let value = crate::vm::unbox_poly_return_checked(
                    shared,
                    thread,
                    Some(value),
                    &site.descriptor,
                    &site.method_name,
                )?
                .unwrap_or(Value::Object(None));
                if ret != b'V' {
                    push_invoke_return_value(
                        &mut thread.frames[frame_idx].stack,
                        crate::vm::coerce_value_for_return(value, ret),
                    )?;
                    crate::vm::native_return_pushed_to_stack(shared, thread);
                }
            }
        }
        SigPolyDoor::PolyBlock {
            vh_param_word,
            vh_narrow_reference_return,
        } => {
            // MH16-6 (JIT round 16 wave 7, lane varhandle;
            // `CRATONVM_VH_INTERP_CAS_FAST`, default ON, `=0` kills it): an instance-field
            // `compareAndSet` the JIT funnel's shared body can prove servable
            // (a resolved, non-final instance field whose kind is the site's,
            // a receiver of the handle's class, an admitted new value) is one
            // barriered CAS. It runs before anything is armed, so a decline
            // leaves the native's door exactly as without it. The invoke-exact
            // gate above has already run.
            let cas_kind = site
                .vh_cas_site_kind
                .filter(|_| shared.config.vh_interp_cas_fast);
            if let (Some(kind), [Value::Object(Some(vh)), Value::Object(Some(recv)), exp, new]) =
                (cas_kind, args)
            {
                if let Some(swapped) = crate::jit::helpers::varhandle_instance_field_cas_values(
                    shared, *vh, *recv, kind, *exp, *new,
                ) {
                    super::site_cache::site_stats::bump_if(
                        shared.config.site_census,
                        super::site_cache::site_stats::VH_CAS_FAST,
                    );
                    push_invoke_return_value(
                        &mut thread.frames[frame_idx].stack,
                        crate::vm::coerce_value_for_return(Value::Int(i32::from(swapped)), ret),
                    )?;
                    return Ok(CachedCallResult::Handled);
                }
            }
            // Interpreter round i2 wave 54 (lane L7,
            // `CRATONVM_VH_INTERP_CAE_FAST`; proposal
            // `i53-L7-proposal-an-instance-field-arm-for-compare-and-exchange`):
            // the exchange forms over an `int` / `long` instance field, with the
            // CAS arm's checks (one shared body), answered by one hardware
            // compare-exchange whose witness is pushed. Before anything is
            // armed, as the CAS arm; a decline writes nothing.
            let cae_kind = site
                .vh_cae_site_kind
                .filter(|_| shared.config.vh_interp_cae_fast);
            if let (Some(kind), [Value::Object(Some(vh)), Value::Object(Some(recv)), exp, new]) =
                (cae_kind, args)
            {
                if let Some(witness) = crate::jit::helpers::varhandle_instance_field_cae_values(
                    shared, *vh, *recv, kind, *exp, *new,
                ) {
                    super::site_cache::site_stats::bump_if(
                        shared.config.site_census,
                        super::site_cache::site_stats::VH_CAE_FAST,
                    );
                    push_invoke_return_value(
                        &mut thread.frames[frame_idx].stack,
                        crate::vm::coerce_value_for_return(witness, ret),
                    )?;
                    return Ok(CachedCallResult::Handled);
                }
            }
            if route.arm_call_site {
                arm_site_descriptor(shared, site);
            } else {
                cratonvm_native_api::poly_call_site::clear();
            }
            // The whole word, as the block arms it (round 16 wave 7, lane
            // varhandle: it was the low 16 bits through `arm_vh_value_site`).
            if let Some(word) = vh_param_word {
                cratonvm_native_api::poly_call_site::arm_vh_param_site(word);
            }
            // The block's reference-return variable fact, read before the
            // native as it reads it (`vm_exec::vh_reference_return_variable`).
            let vh_variable_is_reference = if vh_narrow_reference_return {
                crate::vm::vm_exec::vh_reference_return_variable(
                    shared,
                    thread,
                    &site.method_name,
                    &site.descriptor,
                    match args.first() {
                        Some(Value::Object(Some(o))) => Some(*o),
                        _ => None,
                    },
                )
            } else {
                None
            };
            let mut result = crate::vm::safe_native_call(shared, thread, callback, args);
            // As the block: a native that did not take the word must not
            // leave it; and, as the block, an `asType` refusal names this
            // site's descriptor (interpreter round i2 wave 53, lane L4).
            if vh_param_word.is_some() {
                cratonvm_native_api::poly_call_site::clear_vh_value_site();
                result = crate::vm::vm_exec::vh_astype_refusal_site_text(
                    shared,
                    thread,
                    result,
                    &site.method_name,
                    &site.descriptor,
                );
            }
            let result = result?;
            let result = if vh_narrow_reference_return {
                crate::vm::vm_exec::unbox_poly_return_checked_vh(
                    shared,
                    thread,
                    result,
                    &site.descriptor,
                    &site.method_name,
                    vh_variable_is_reference,
                )?
            } else {
                crate::vm::unbox_poly_return_checked(
                    shared,
                    thread,
                    result,
                    &site.descriptor,
                    &site.method_name,
                )?
            };
            if let Some(value) = result {
                if ret != b'V' {
                    push_invoke_return_value(
                        &mut thread.frames[frame_idx].stack,
                        crate::vm::coerce_value_for_return(value, ret),
                    )?;
                    crate::vm::native_return_pushed_to_stack(shared, thread);
                }
            }
        }
    }
    Ok(CachedCallResult::Handled)
}

/// Publish the site's call-site descriptor for the native about to run
/// (`cratonvm_native_api::poly_call_site`). With
/// `VmConfig::poly_call_site_shared` (`CRATONVM_POLY_CALL_SITE_SHARED`) the
/// native gets the site's own `Arc<str>`, a reference-count bump; without it,
/// a fresh copy (one allocation per call, as the slow path's doors arm).
/// Proposal `interpreter-L7-proposal-the-call-site-descriptor-channel-allocates-per-call-FIXED-20261013`
/// (interpreter round i1 wave 49, lane L7).
#[inline]
fn arm_site_descriptor(shared: &SharedVm, site: &SigPolySite) {
    if shared.config.poly_call_site_shared {
        super::site_cache::site_stats::bump_if(
            shared.config.site_census,
            super::site_cache::site_stats::POLY_ARM_SHARED,
        );
        cratonvm_native_api::poly_call_site::arm_shared(&site.descriptor);
    } else {
        cratonvm_native_api::poly_call_site::arm(&site.descriptor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_site_key_hash_separates_name_and_descriptor() {
        assert_eq!(
            site_key_hash("get", "(LH;)I"),
            site_key_hash("get", "(LH;)I")
        );
        assert_ne!(
            site_key_hash("get", "(LH;)I"),
            site_key_hash("get", "(LH;)J")
        );
        assert_ne!(
            site_key_hash("get", "(LH;)I"),
            site_key_hash("set", "(LH;)I")
        );
    }

    /// JIT round 16 wave 7 (lane varhandle): the shapes behind
    /// `r16-varhandle-compare-and-set-seven-times-slower-since-round-15`. A
    /// three-parameter site's word has its receiver coordinate above the low
    /// 16 value bits, which the round-15 merge guard read as "the replay
    /// cannot reproduce this door"; the two-parameter controls never had.
    /// The route now carries the whole word.
    #[test]
    fn a_compare_and_set_site_word_has_bits_above_the_value_classes() {
        use cratonvm_native_api::poly_call_site::vh_param_site_classes;
        for three in [
            "(LH;II)Z",
            "(LH;II)I",
            "(LH;Ljava/lang/Object;Ljava/lang/Object;)Z",
        ] {
            assert_ne!(vh_param_site_classes(three) >> 16, 0, "{three}");
        }
        for control in [
            "(LH;)I",
            "(LH;I)I",
            "(LH;Ljava/lang/Object;)Ljava/lang/Object;",
        ] {
            assert_eq!(vh_param_site_classes(control) >> 16, 0, "{control}");
        }
        let door = SigPolyDoor::PolyBlock {
            vh_param_word: Some(vh_param_site_classes("(LH;II)Z")),
            vh_narrow_reference_return: false,
        };
        let whole = vh_param_site_classes("(LH;II)Z");
        assert!(
            matches!(door, SigPolyDoor::PolyBlock { vh_param_word: Some(w), .. } if w == whole),
            "kept whole"
        );
    }

    /// Interpreter round i2 wave 53 (lane L7): the CAS arm serves the strong
    /// `compareAndSet` and its four weak forms, never the exchange forms
    /// (whose answer is the witness, not a boolean) nor `getAndSet`.
    #[test]
    fn the_cas_arm_serves_the_strong_and_weak_boolean_cas_modes_only() {
        for mode in [
            "compareAndSet",
            "weakCompareAndSet",
            "weakCompareAndSetPlain",
            "weakCompareAndSetAcquire",
            "weakCompareAndSetRelease",
        ] {
            assert!(is_boolean_cas_access_mode(mode), "{mode}");
        }
        for mode in [
            "compareAndExchange",
            "compareAndExchangeAcquire",
            "getAndSet",
            "get",
            "weakCompareAndSetVolatile",
        ] {
            assert!(!is_boolean_cas_access_mode(mode), "{mode}");
        }
    }

    /// Interpreter round i2 wave 54 (lane L7, `CRATONVM_VH_INTERP_CAE_FAST`):
    /// the exchange arm serves the three exchange modes over an `int` or
    /// `long` variable whose site returns that kind, nothing else.
    #[test]
    fn i54_l7_the_exchange_arm_serves_int_and_long_exchange_sites_only() {
        use crate::jit::helpers::varhandle_cae_site_kind;
        for mode in [
            "compareAndExchange",
            "compareAndExchangeAcquire",
            "compareAndExchangeRelease",
        ] {
            assert!(is_exchange_access_mode(mode), "{mode}");
        }
        for mode in [
            "compareAndSet",
            "getAndSet",
            "getAndAdd",
            "weakCompareAndSet",
        ] {
            assert!(!is_exchange_access_mode(mode), "{mode}");
        }
        assert_eq!(varhandle_cae_site_kind("(LH;II)I", b'I'), Some(b'I'));
        assert_eq!(varhandle_cae_site_kind("(LH;JJ)J", b'J'), Some(b'J'));
        for (desc, ret) in [
            ("(LH;II)Z", b'Z'),
            ("(LH;IJ)J", b'J'),
            (
                "(LH;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                b'L',
            ),
            ("(LH;BB)B", b'B'),
            ("(LH;DD)D", b'D'),
            ("(II)I", b'I'),
        ] {
            assert_eq!(varhandle_cae_site_kind(desc, ret), None, "{desc}");
        }
    }

    /// Interpreter round i2 wave 54 (lane L7, `CRATONVM_SIG_POLY_SITE_FIRST`):
    /// the site filter has no false negative, says nothing for an empty
    /// table, and is cleared with the table.
    #[test]
    fn i54_l7_the_site_filter_holds_every_filled_site_and_clears_with_the_table() {
        let mut cache = SigPolySiteCache::default();
        let sites = [(3u16, 0usize), (3, 7), (40, 7), (65_535, 65_530), (1, 12)];
        for &(cp, pc) in &sites {
            assert!(site_filter_bit(cp, pc) < SITE_FILTER_BITS);
            assert!(!cache.may_hold(cp, pc), "an empty table holds nothing");
        }
        for &(cp, pc) in &sites {
            let bit = site_filter_bit(cp, pc);
            cache.filter[bit >> 6] |= 1u64 << (bit & 63);
        }
        for &(cp, pc) in &sites {
            assert!(cache.may_hold(cp, pc), "({cp}, {pc})");
        }
        // Neighbouring pcs of one method spread over the filter.
        // addr-keyed: not an address (filter bit indices, local to this test).
        let distinct: std::collections::HashSet<usize> =
            (0..64usize).map(|pc| site_filter_bit(9, pc * 3)).collect();
        assert!(distinct.len() > 48, "{}", distinct.len());
        cache.clear_sites();
        for &(cp, pc) in &sites {
            assert!(!cache.may_hold(cp, pc));
        }
    }

    fn test_site(name: &str) -> Arc<SigPolySite> {
        Arc::new(SigPolySite {
            method_name: Arc::from(name),
            descriptor: Arc::from("(LH;)I"),
            facts: cratonvm_jit_api::DescriptorFacts::of("(LH;)I"),
            num_params: 1,
            is_var_handle_site: true,
            vh_cas_site_kind: None,
            vh_cae_site_kind: None,
            registry_generation: 0,
            routes: smallvec::SmallVec::new(),
        })
    }

    fn slot(name: &str) -> SigPolySlot {
        SigPolySlot {
            site: Some(test_site(name)),
            reentrant: false,
        }
    }

    /// Interpreter round i2 wave 57 (lane L7, `CRATONVM_SIG_POLY_SITE_LEND`):
    /// a lent site leaves its entry empty, comes back on `give_back` with no
    /// extra reference, and is NOT put back over a clear (the table moved to
    /// another redefinition count) or over a nested fill of the same key. A
    /// nested probe of a lent entry declines once and marks the entry
    /// reentrant, which is served by a clone from then on.
    #[test]
    fn i57_l7_a_lent_site_comes_back_unless_the_table_moved_on() {
        let key = (ClassId::new(7), 3u16, 11u32);
        let mut cache = SigPolySiteCache::default();
        cache.sites.insert(key, slot("get"));
        // Other tests may redefine a class meanwhile (the count is a process
        // counter): the table is made current right before each lend, and
        // the entry is read directly, not through `get`.
        let current = |cache: &mut SigPolySiteCache| {
            cache.redefinitions_seen = crate::classloading::class_redefinition_count();
        };
        let lend = |cache: &mut SigPolySiteCache| {
            current(cache);
            let lent = cache.lend(key).expect("lent");
            assert_eq!(lent.1, Some(cache.redefinitions_seen));
            lent
        };
        let name_in = |cache: &SigPolySiteCache| {
            cache
                .sites
                .get(&key)?
                .site
                .as_ref()
                .map(|s| s.method_name.to_string())
        };

        // Lend and give back: one reference, the entry restored.
        let (site, under) = lend(&mut cache);
        assert_eq!(
            Arc::strong_count(&site),
            1,
            "lending moves, it does not clone"
        );
        assert_eq!(name_in(&cache), None, "a lent entry is empty");
        cache.give_back(key, site, under);
        assert_eq!(name_in(&cache).as_deref(), Some("get"));
        assert!(!cache.sites[&key].reentrant);

        // A nested fill of the same key while lent: the newer site stays.
        let (site, under) = lend(&mut cache);
        cache.sites.insert(key, slot("set"));
        cache.give_back(key, site, under);
        assert_eq!(name_in(&cache).as_deref(), Some("set"));

        // A clear while lent (a redefinition): nothing is put back.
        let (site, under) = lend(&mut cache);
        cache.clear_sites();
        cache.redefinitions_seen = under.expect("lent") + 1;
        cache.sites.insert(
            key,
            SigPolySlot {
                site: None,
                reentrant: false,
            },
        );
        cache.give_back(key, site, under);
        assert_eq!(name_in(&cache), None, "not put back over a clear");

        // A nested probe of a lent entry: declines once, marks it reentrant;
        // the outer give-back restores it, and later probes clone.
        cache.sites.insert(key, slot("get"));
        let (site, under) = lend(&mut cache);
        current(&mut cache);
        assert!(
            cache.lend(key).is_none(),
            "a nested probe of a lent entry declines"
        );
        assert!(cache.sites[&key].reentrant, "and marks it reentrant");
        cache.give_back(key, site, under);
        assert_eq!(name_in(&cache).as_deref(), Some("get"));
        current(&mut cache);
        let (outer, outer_under) = cache.lend(key).expect("cloned");
        assert_eq!(outer_under, None, "a reentrant entry is cloned, not lent");
        assert_eq!(Arc::strong_count(&outer), 2);
        current(&mut cache);
        let (inner, inner_under) = cache.lend(key).expect("a nested probe is served");
        assert_eq!(inner_under, None);
        cache.give_back(key, inner, inner_under);
        cache.give_back(key, outer, outer_under);
        assert_eq!(name_in(&cache).as_deref(), Some("get"));
        assert_eq!(
            Arc::strong_count(cache.sites[&key].site.as_ref().expect("site")),
            1
        );
    }

    #[test]
    fn an_idle_record_is_the_default() {
        assert!(matches!(SigPolyRecord::default(), SigPolyRecord::Idle));
        assert_eq!(SigPolySiteCache::default().len(), 0);
    }
}
