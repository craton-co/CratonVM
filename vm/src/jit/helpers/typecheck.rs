thread_local! {
    /// Resolved JIT type-check targets on this mutator, stamped with the
    /// `class_definition_epoch` they were resolved under: `(epoch, entries)`.
    /// Compiled loops repeatedly execute the same checkcast/instanceof sites,
    /// whose class-name bytes live in the immutable JIT string table.
    ///
    /// # Why the epoch stamp, r10 `help`
    ///
    /// This used to say "Class IDs are stable for a VM, so `(vm, ptr, len)` is
    /// a complete cache key", and cached the answer forever. What is cached is
    /// `ClassManager::find_unique_class_by_name`, and `class_definition_epoch`
    /// exists precisely for "a cache whose validity condition is *this name
    /// still resolves to this `ClassId`*" — its own doc says so, and names
    /// `find_unique_class_by_name` as one of the three readers it covers. Two
    /// mutations break a cached answer:
    ///
    ///  * a SECOND loader defining the same name, after which the name is no
    ///    longer unique and the real answer is `None` — the hazard the inline
    ///    type-check fast paths already refuse on
    ///    (`cratonvm_types::duplicate_class_name_seen_addr`, consulted by
    ///    `jit/src/ir_lower.rs` and `jit/src/x64/op_object.rs`), and which this
    ///    helper-path memo alone was ignoring;
    ///  * an unload, which also reuses `ClassId`s —
    ///    [`JIT_TYPECHECK_NEGATIVE_CACHE`]'s doc states that in as many words
    ///    ("and every unload (so also on any `ClassId` reuse)") while this
    ///    cache in the same file assumed the opposite.
    ///
    /// One `Acquire` load re-validates the whole table, which is what the
    /// negative memo already pays; a stale entry here is a wrong TARGET class
    /// for a `checkcast`, i.e. a wrong `ClassCastException` or a missing one.
    ///
    /// This holds SEVERAL entries, not one. It used to be a single `Cell`, and
    /// because the key is the class-name *pointer*, two distinct type-check
    /// sites — even two `checkcast`s to the very same class, which get separate
    /// string-table entries — evicted each other on every iteration. Every
    /// execution then missed and re-ran `find_unique_class_by_name`, which
    /// allocates two host `String`s (`to_string()` plus a `'/'`→`'.'`
    /// `replace`) before it even looks anything up. Measured: in a loop whose
    /// body holds one type-check site the site costs ~64ns, and a second site
    /// in the same body costs ~1750ns *each* — a 27x cliff that any
    /// `instanceof` ladder or twice-casting method falls off. A short linear
    /// scan cannot thrash that way.
    static JIT_TYPECHECK_TARGET_CACHE:
        std::cell::RefCell<(u64, Vec<(usize, usize, usize, u32)>)> =
        const { std::cell::RefCell::new((0, Vec::new())) };

    /// Memoized *positive* answers from `ClassManager::is_subclass_of` for the
    /// JIT type-check path, keyed by `(vm, child_class_id, parent_class_id)`.
    ///
    /// `is_subclass_of` is not cheap: it takes the process-wide
    /// `class_manager` read lock, allocates an `FxHashSet` visited set, and
    /// then DFS-walks the superclass chain *and* every transitively implemented
    /// interface. `jit_typecheck_resolve` calls it on every `checkcast` /
    /// `instanceof` whose receiver class is not *identical* to the target —
    /// i.e. on every genuinely polymorphic type check, which is the common
    /// case. A compiled loop doing `checkcast` on the result of a map lookup
    /// (`(Charset) cache.get(name)`, the shape
    /// `org.apache.tomcat.util.buf.CharsetCache.getCharset` has) therefore paid
    /// a lock acquisition, a heap allocation and a hierarchy walk per
    /// iteration. With several mutator threads in that loop the contended read
    /// lock dominated everything else.
    ///
    /// Only `true` answers are memoized, and that is deliberate. A class's
    /// superclass and interface lists are fixed when it is defined, so once a
    /// subtype relation holds for a given PAIR OF CLASSES it holds for the
    /// life of the `SharedVm` — a cached `true` can never go stale as long as
    /// the key still names that same pair. A `false`, in contrast, can be
    /// observed while the hierarchy is still being populated (a supertype not
    /// yet in the class store makes that DFS branch stop early), so caching it
    /// could pin a wrong answer; those keep paying the full walk exactly as
    /// before.
    ///
    /// # Why this is stamped with `class_definition_epoch`, r10 help2
    ///
    /// The key is `(vm, child.as_u32(), parent.as_u32())` — bare `ClassId`
    /// numbers, not the classes themselves. "Fixed when it is defined" is a
    /// property of the CLASS, not of the number: an unload frees its
    /// `ClassId` for reuse (`class_definition_epoch`'s own doc: the epoch
    /// "moves whenever a class is defined or unloaded"; see
    /// `JIT_TYPECHECK_NEGATIVE_CACHE`'s doc for the same fact stated for its
    /// sibling memo: "every unload (so also on any `ClassId` reuse)"). Once
    /// `child` (or `parent`) unloads and a later, unrelated class is handed
    /// the same numeric id, the old entry's `true` would answer for a pair of
    /// classes that were never checked — the same shape of bug
    /// `JIT_TYPECHECK_TARGET_CACHE` had before it gained this exact stamp.
    /// This table used to have no stamp and relied only on the (incomplete,
    /// and in practice not even consulted along this call path — see
    /// `jit_is_subclass_of_cached`) claim that `any_class_redefined()` bypass
    /// covered it; redefinition cannot change a hierarchy, but it was never
    /// the hazard here, so that guard did not address ClassId reuse at all.
    static JIT_SUBTYPE_POSITIVE_CACHE:
        std::cell::RefCell<(u64, Vec<(usize, u32, u32)>)> =
        const { std::cell::RefCell::new((0, Vec::new())) };
}

/// Entry cap for [`JIT_SUBTYPE_POSITIVE_CACHE`]. Scanned linearly (the same
/// shape `jit_hashmap_string_node_cache` uses) rather than direct-mapped: a
/// direct-mapped table silently collides two hot pairs into one slot, and a
/// loop alternating between them then misses *every* time — measured at a 40%
/// miss rate over just five distinct receiver classes. A linear scan over this
/// many `(usize, u32, u32)` entries cannot collide and is a handful of
/// compares.
const JIT_SUBTYPE_CACHE_CAP: usize = 32;

/// Entry cap for [`JIT_TYPECHECK_TARGET_CACHE`]. One entry per distinct
/// type-check *site* reached on this thread; 64 covers a long `instanceof`
/// ladder plus the sites around it.
const JIT_TYPECHECK_TARGET_CACHE_CAP: usize = 64;

/// Entry cap for [`JIT_TYPECHECK_ANSWER_CACHE`].
const JIT_TYPECHECK_ANSWER_CACHE_CAP: usize = 16;

thread_local! {
    /// Whole-answer memo for a *non-array* receiver at one type-check site:
    /// `(vm, class_name_ptr, class_name_len, obj_class_id)` -> the check
    /// passed. Move-to-front, so a monomorphic site — the overwhelmingly
    /// common shape in a compiled loop — is found on the first compare.
    ///
    /// This sits in front of everything [`jit_typecheck_resolve`] does, and
    /// in front of `str::from_utf8` in the callers. A `checkcast
    /// java/lang/Integer` in a hot loop was paying, per iteration: a UTF-8
    /// validation of the 17-byte class name, an `ObjectKind` header read, a
    /// linear scan of the 64-entry [`JIT_TYPECHECK_TARGET_CACHE`], and then
    /// the identity compare that actually answers it. Together with
    /// `jit_typecheck_resolve` and `str::from_utf8` that was 4.5% of the
    /// `CratonBench hashmap` phase.
    ///
    /// SOUNDNESS mirrors [`JIT_SUBTYPE_POSITIVE_CACHE`] exactly, and rests on
    /// the same two facts:
    ///
    ///  * Only `true` is memoized. A class's superclass/interface lists are
    ///    fixed at definition, so once assignability holds for a
    ///    `(receiver class, target name)` pair it holds for the life of the
    ///    `SharedVm` -- as long as `obj_class_id` still names that same
    ///    receiver class. A `false` can be observed while the hierarchy is
    ///    still being populated — or before the target class is loaded at all
    ///    — so negatives keep paying the full path.
    ///  * Redefinition cannot change a hierarchy, so it does not retire an
    ///    entry. The cache used to be bypassed for the process once
    ///    `any_class_redefined()` was set; that latch is now only the
    ///    `TYPECHECK_MEMOS_SURVIVE_REDEFINITION = false` kill switch
    ///    (interpreter round i1 wave 18, lane L4).
    ///
    /// # Why this is stamped with `class_definition_epoch`, r10 help2
    ///
    /// `obj_class_id` is a bare `ClassId` number, not the class itself, and an
    /// unload frees its number for reuse by an unrelated later class —
    /// exactly the hazard [`JIT_TYPECHECK_TARGET_CACHE`] and
    /// [`JIT_TYPECHECK_NEGATIVE_CACHE`] both stamp for ("every unload (so also
    /// on any `ClassId` reuse)", per the negative cache's own doc). This table
    /// used to have no stamp at all: `any_class_redefined()` guards
    /// REDEFINITION, which cannot change a hierarchy, but says nothing about
    /// UNLOAD, so a `true` proven for `(target name, id=5)` before an unload
    /// stayed cached and would wrongly answer `true` for whatever unrelated
    /// class a later definition handed id 5 to. Stamped and cleared as one
    /// unit exactly like the sibling negative memo, rather than per-entry:
    /// cheaper, and every entry in the table is invalidated by the same event
    /// anyway.
    ///
    /// Two further conditions are checked by the callers before consulting it:
    /// the receiver must not be an array (a reference array's header stores
    /// its *component* class id, so `obj_class_id` does not identify the
    /// receiver's type — see the array branch of `jit_typecheck_resolve`),
    /// and `lenient` must match, which it does because only `checkcast`
    /// (lenient) and `instanceof` (strict) use this and they key on distinct
    /// call-site name pointers only by accident. To keep that from ever
    /// mattering, `lenient` is part of the key.
    static JIT_TYPECHECK_ANSWER_CACHE:
        std::cell::RefCell<(u64, Vec<(usize, usize, usize, u32, bool)>)> =
        const { std::cell::RefCell::new((0, Vec::new())) };
}

/// Entry cap for [`JIT_TYPECHECK_NEGATIVE_CACHE`]. An `instanceof` ladder over
/// `k` receiver types at `n` rungs needs `k * (n - 1)`-ish entries; 32 covers
/// the four-by-four ladder of `CratonBenchC2 dispatch` with room to spare, and
/// a linear scan over this many small tuples is still a handful of compares.
const JIT_TYPECHECK_NEGATIVE_CACHE_CAP: usize = 32;

thread_local! {
    /// Whole-answer memo for a *failing* type check, keyed exactly like
    /// [`JIT_TYPECHECK_ANSWER_CACHE`] and stamped with the
    /// `class_definition_epoch` it was proven under: `(epoch, entries)`.
    ///
    /// # Why a `false` may be memoized here when the positive memo refuses to
    ///
    /// The positive memo's objection is that a `false` "can be observed while
    /// the hierarchy is still being populated — or before the target class is
    /// loaded at all". Both halves are closed by construction:
    ///
    ///  * **Only a recorded-site answer is admitted.** The site's target was
    ///    resolved at compile time through the compiling class's loader, and
    ///    the interned name pointer names that one `ClassId` for the life of
    ///    the process (`cratonvm_jit::intern_typecheck_target` keys its
    ///    intern table by id). `jit_typecheck_resolve` sets `stable_negative`
    ///    only on that branch's final `return false`, after re-validating the
    ///    id against this VM. The by-name fallback (no recorded id, target
    ///    possibly unloaded) is never memoized.
    ///  * **The whole memo dies with the class set.** Every remaining input —
    ///    `is_subclass_of`'s walk, `is_assignable_to_name`'s by-name walk,
    ///    `synthetic_implements` (a table keyed by the receiver's NAME) — is a
    ///    function of the receiver class, the target class and which classes
    ///    are defined. `class_definition_epoch` moves on every definition and
    ///    every unload (so also on any `ClassId` reuse), and an entry is
    ///    consulted only under the epoch it was proven in; the answer is
    ///    recorded only if the epoch did not move while it was computed.
    ///    Redefinition (which cannot change supertypes, but is sticky and
    ///    cheap to honour) bypasses the memo exactly as it does the positive
    ///    one.
    ///
    /// Two receiver shapes are excluded because their answer is NOT a
    /// function of the class id: an annotation proxy (every proxy shares one
    /// `ClassId`; the annotation type is on the instance) and a lambda proxy
    /// (its check resolves the functional interface by name). Arrays are
    /// excluded by the caller, as for the positive memo. Since round 13 wave 7
    /// (lane predicates) the screen is the interpreter's own
    /// `receiver_class_determines_cast_verdict`, which also excludes a
    /// `Proxy$Instance` / generated proxy (interface slot) and an
    /// `Unmodifiable*` stamp (backing collection).
    ///
    /// Measured motivation: `CratonBenchC2 dispatch`'s `PayloadHandler.handle`
    /// is a four-rung `instanceof` ladder whose payload rotates through four
    /// types, so three tests in four are false and each paid the full walk —
    /// `is_subclass_of_inner`, a per-query `FxHashSet` in
    /// `is_assignable_to_name`, `synthetic_implements`,
    /// `annotation_proxy_satisfies_target`, all under the class-manager lock:
    /// ~25% of the phase (round 9 wave 2 `perfdiag`).
    static JIT_TYPECHECK_NEGATIVE_CACHE:
        std::cell::RefCell<(u64, Vec<(usize, usize, usize, u32, bool)>)> =
        const { std::cell::RefCell::new((0, Vec::new())) };
}

/// The class-definition epoch the typecheck memos ([`JIT_TYPECHECK_TARGET_CACHE`],
/// [`JIT_SUBTYPE_POSITIVE_CACHE`], [`JIT_TYPECHECK_ANSWER_CACHE`],
/// [`JIT_TYPECHECK_NEGATIVE_CACHE`]) are stamped with in `vm`: its class
/// store's own counter (`StoreEpochs`), tagged with the store's slot
/// (`site_cache::cast_memo_epoch_in`), where it used to be the process-wide
/// `class_definition_epoch` (interpreter round i1 wave 21, lane L5). Every
/// definition, unload and edge change in `vm`'s store moves it, which is all
/// the memos' soundness argument needs, since every entry is keyed by
/// `vm_identity`; a class defined in another VM no longer empties them. The
/// tag keeps a stamp from one slot from equalling a stamp from another (or a
/// process-wide snapshot), so a thread that alternates VMs empties a table
/// rather than serving it under the other VM's counter.
#[inline(always)]
fn typecheck_memo_epoch(vm: &SharedVm) -> u64 {
    crate::runtime::interpreter::site_cache::cast_memo_epoch_in(vm)
}

/// Probe [`JIT_TYPECHECK_NEGATIVE_CACHE`] under `epoch`. A memo stamped with any
/// other epoch is emptied first, so a stale `false` can never be served.
/// Move-to-front on hit.
fn jit_typecheck_negative_cached(key: (usize, usize, usize, u32, bool), epoch: u64) -> bool {
    JIT_TYPECHECK_NEGATIVE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let (stamp, entries) = &mut *cache;
        if *stamp != epoch {
            entries.clear();
            *stamp = epoch;
            return false;
        }
        match entries.iter().position(|entry| *entry == key) {
            Some(0) => true,
            Some(idx) => {
                entries.swap(0, idx);
                true
            }
            None => false,
        }
    })
}

/// Record a proven, class-stable `false` under `epoch` (the epoch that was
/// current both before and after it was computed), evicting the least recently
/// used entry.
fn jit_typecheck_negative_cache_put(key: (usize, usize, usize, u32, bool), epoch: u64) {
    JIT_TYPECHECK_NEGATIVE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let (stamp, entries) = &mut *cache;
        if *stamp != epoch {
            entries.clear();
            *stamp = epoch;
        }
        if entries.iter().any(|entry| *entry == key) {
            return;
        }
        if entries.len() >= JIT_TYPECHECK_NEGATIVE_CACHE_CAP {
            entries.pop();
        }
        entries.insert(0, key);
    });
}

/// Memo probe for a proven-assignable, non-array receiver at one type-check
/// site, under `epoch`. Move-to-front on hit. A memo stamped with any other
/// epoch is emptied first (r10 help2 — see the stamp rationale on
/// [`JIT_TYPECHECK_ANSWER_CACHE`]'s doc), so a stale `true` recorded for a
/// since-unloaded, since-reused `ClassId` can never be served.
fn jit_typecheck_answer_cached(key: (usize, usize, usize, u32, bool), epoch: u64) -> bool {
    JIT_TYPECHECK_ANSWER_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let (stamp, entries) = &mut *cache;
        if *stamp != epoch {
            entries.clear();
            *stamp = epoch;
            return false;
        }
        match entries.iter().position(|entry| *entry == key) {
            Some(0) => true,
            Some(idx) => {
                entries.swap(0, idx);
                true
            }
            None => false,
        }
    })
}

/// Record a proven-assignable answer under `epoch` (the epoch that was
/// current both before and after it was computed — see the call site in
/// [`jit_typecheck_memoized`]), evicting the least recently used entry.
fn jit_typecheck_answer_cache_put(key: (usize, usize, usize, u32, bool), epoch: u64) {
    JIT_TYPECHECK_ANSWER_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let (stamp, entries) = &mut *cache;
        if *stamp != epoch {
            entries.clear();
            *stamp = epoch;
        }
        if entries.iter().any(|entry| *entry == key) {
            return;
        }
        if entries.len() >= JIT_TYPECHECK_ANSWER_CACHE_CAP {
            entries.pop();
        }
        entries.insert(0, key);
    });
}

/// What a compiled type check must raise instead of answering: met by
/// [`jit_typecheck_holder_site_target`] at an unbound (holder-keyed) site.
#[derive(Debug)]
enum TypecheckRaise {
    /// A class-access refusal (JVMS §5.4.4; i9-L2, every mode since i10-L2):
    /// the `IllegalAccessError` message, built into a throwable by the body
    /// and recorded against the site's `(holder, cp_index)` entry (JVMS
    /// §5.4.3; i11-L2, wave 12), as the interpreter records its own.
    AccessDenied {
        message: String,
        holder: ClassId,
        cp_index: u16,
        /// `ResolutionCache::fill_snapshot` read before the site's key was
        /// translated: the record is made under it (i22-L5, wave 24).
        fill_as_of: u64,
    },
    /// The target's resolution failed (i11-L2) and its Java error —
    /// `NoClassDefFoundError`, as the interpreter's `checkcast` /
    /// `instanceof` throws at the same site — is already pending on this
    /// thread (`set_jit_pending_exception`).
    Pending,
}

thread_local! {
    /// The [`TypecheckRaise`] met by [`jit_typecheck_holder_site_target`],
    /// carried out of [`jit_typecheck_resolve`] to [`jit_typecheck_memoized`],
    /// which clears it before the resolve and takes it right after: it never
    /// outlives one type check.
    static JIT_TYPECHECK_RAISE: std::cell::RefCell<Option<TypecheckRaise>> =
        const { std::cell::RefCell::new(None) };
}

/// Has this type check already met something it must raise? The resolve path
/// then returns at once rather than answering by name.
fn jit_typecheck_raise_stashed() -> bool {
    JIT_TYPECHECK_RAISE.with(|d| d.borrow().is_some())
}

/// Kill switch (interpreter round i1 wave 18, lane L4). `true`: the typecheck
/// answer memos keep serving after a class redefinition. They hold subtype
/// verdicts keyed on a receiver `ClassId` and an interned target NAME, and a
/// redefinition changes neither: `ClassManager::redefine_class` rewrites
/// method bodies, the constant pool, bootstrap methods and annotations in
/// place and never the superclass or interface edges (the structural check
/// refuses a change to them, and even `skip_structural_check` swaps none),
/// nor the name-to-id mapping. What can change the answers — a class
/// defined or unloaded, which also covers a recycled id — is the
/// `class_definition_epoch` both memos are already stamped with. `false`:
/// the historical latch, which bypassed both memos for the rest of the
/// process after the first redefinition of any class.
const TYPECHECK_MEMOS_SURVIVE_REDEFINITION: bool = true;

/// Whether the typecheck answer memos are latched off by a redefinition:
/// never, unless the kill switch restores the latch.
#[inline(always)]
fn typecheck_memos_latched_off() -> bool {
    !TYPECHECK_MEMOS_SURVIVE_REDEFINITION && crate::classloading::any_class_redefined()
}

/// Shared front end for `jit_checkcast` / `jit_instanceof`: answer from
/// [`JIT_TYPECHECK_ANSWER_CACHE`] when possible, otherwise run the full
/// [`jit_typecheck_resolve`] and memoize a positive, non-array answer.
///
/// `raise` receives what the full path met at an unbound site and must be
/// raised instead of answered ([`JIT_TYPECHECK_RAISE`]): a class-access
/// refusal or a resolution failure. Nothing is memoised then, so the next
/// execution meets it again. It is left `None` on every other return.
///
/// # Safety
/// Same contract as [`jit_typecheck_resolve`]: `obj_ref` must be non-null and
/// live, and `vm_ptr` must come from JIT code compiled against this live VM.
unsafe fn jit_typecheck_memoized(
    vm: &SharedVm,
    obj_class_id: ClassId,
    obj_ref: &mut ObjectRef,
    class_name_ptr: *const u8,
    class_name_len: usize,
    lenient: bool,
    raise: &mut Option<TypecheckRaise>,
) -> Option<bool> {
    let vm_key = vm.vm_identity;
    let receiver_is_array = vm.mem.heap.kind_of(*obj_ref) == cratonvm_types::ObjectKind::Array;
    // JIT round 21, lane tcarray (`CRATONVM_JIT_TYPECHECK_ARRAY_MEMO`): a
    // REFERENCE-array receiver's verdict is a function of its header class id
    // (the component's), the site's target, the mode and the class set
    // (`site_cache::ArrayReceiverKey`, the interpreter's array memo key), so
    // its proven `true` is memoized under the same epoch, in a key of its own:
    // the length word carries `JIT_TYPECHECK_ARRAY_MEMO_BIT`, so it never meets
    // a plain receiver of the component class. A primitive array (header id
    // `0`, `java/lang/Object`'s) is never memoized.
    let reference_array = receiver_is_array
        && vm.jit.supers_displays.array_cast_memo()
        && vm.mem.heap.element_type_of(*obj_ref) == cratonvm_types::ArrayElementType::Reference;
    let key = (
        vm_key,
        class_name_ptr as usize,
        if reference_array {
            class_name_len | JIT_TYPECHECK_ARRAY_MEMO_BIT
        } else {
            class_name_len
        },
        obj_class_id.as_u32(),
        lenient,
    );
    // Any other array receiver is excluded from the memo entirely: its header
    // holds the component class id, which `jit_typecheck_resolve`'s array
    // branch (the authority for that shape) never consults by itself.
    let memoizable = !typecheck_memos_latched_off() && (!receiver_is_array || reference_array);
    // Read BEFORE either cache consult: r10 help2 — the positive memo now
    // needs the same epoch gate the negative one already had (a `true` cached
    // for `obj_class_id` is only sound while that id still names the class it
    // was proven against; see `JIT_TYPECHECK_ANSWER_CACHE`'s doc), and a
    // negative is recorded only if no class was defined or unloaded while it
    // was being proven. This VM's store's epoch ([`typecheck_memo_epoch`]).
    let epoch = typecheck_memo_epoch(vm);
    if memoizable && jit_typecheck_answer_cached(key, epoch) {
        return Some(true);
    }
    if memoizable && jit_typecheck_negative_cached(key, epoch) {
        return Some(false);
    }
    // SAFETY: `class_name_ptr` is non-null with `class_name_len > 0` (checked
    // by both callers); it points into the JIT string table, which outlives
    // this call.
    let class_name =
        std::str::from_utf8(std::slice::from_raw_parts(class_name_ptr, class_name_len)).ok()?;
    // JIT round 21, lane tcarray: the component of a reference array gets its
    // supers display, so the optimizing tier's array arm answers its next
    // subtype hit inline (`jit_instanceof` does this for a plain receiver).
    if reference_array {
        ensure_array_component_display(vm, obj_class_id);
    }
    let mut stable_negative = false;
    let mut instance_admitted = false;
    JIT_TYPECHECK_RAISE.with(|d| *d.borrow_mut() = None);
    let assignable = jit_typecheck_resolve(
        vm,
        obj_class_id,
        obj_ref,
        class_name,
        lenient,
        &mut stable_negative,
        &mut instance_admitted,
    );
    *raise = JIT_TYPECHECK_RAISE.with(|d| d.borrow_mut().take());
    if raise.is_some() {
        return Some(assignable);
    }
    // Re-test the gates: `jit_typecheck_resolve` can load classes (and so can
    // flip `any_class_redefined`, under the kill switch), and it can move
    // `*obj_ref`.
    let still_memoizable = memoizable
        && !typecheck_memos_latched_off()
        && (reference_array || vm.mem.heap.kind_of(*obj_ref) != cratonvm_types::ObjectKind::Array);
    // Both puts additionally require the epoch to have held still across the
    // resolve (which can itself define/load classes): an answer proven while
    // the class set was still moving is not memoized, matching
    // `jit_typecheck_target_cache_put`'s "current both before and after" rule.
    // A miss here just repeats the (unmemoized) full resolve next time.
    let epoch_held = typecheck_memo_epoch(vm) == epoch;
    // A `true` that came from the INSTANCE (an annotation proxy's slot-0
    // type, a `Proxy$Instance`'s slot-1 `Class[]`, an `Unmodifiable*` stamp's
    // backing collection) is not a fact about `obj_class_id`: every such
    // object of that class shares the id, so memoizing it answered the next
    // one -- `List.of(..)` then `unmodifiableList(linkedList)` at one
    // `instanceof RandomAccess` site -- with the first one's verdict (round
    // 13 wave 7, lane predicates).
    if assignable && still_memoizable && epoch_held && !instance_admitted {
        jit_typecheck_answer_cache_put(key, epoch);
    } else if !assignable && stable_negative && still_memoizable && epoch_held {
        jit_typecheck_negative_cache_put(key, epoch);
    }
    Some(assignable)
}

/// JIT round 21, lane tcarray (`CRATONVM_JIT_TYPECHECK_ARRAY_MEMO`): set in the
/// LENGTH word of a [`JIT_TYPECHECK_ANSWER_CACHE`] key for a reference-array
/// receiver. A site name is a short interned string, never 2^62 bytes long,
/// so the bit separates the array key from the plain-receiver key of the same
/// `(site, class id)`.
const JIT_TYPECHECK_ARRAY_MEMO_BIT: usize = 1 << 62;

/// JIT round 21, lane tcarray: publish the supers display of a reference
/// array's COMPONENT `component` when it is an ordinary class without one yet
/// (an array of a class whose `<clinit>` never ran has none), so the
/// optimizing tier's array arm (`Lowerer::emit_typecheck_array_arm`) can
/// answer that component's subtype hits. A lambda proxy / synthetic id, a
/// nested array component and a class manager that cannot be read without
/// waiting are left alone. Helper slow path only.
fn ensure_array_component_display(vm: &SharedVm, component: ClassId) {
    let raw = component.as_u32();
    if raw == 0
        || raw >= 0x8000_0000
        || !vm.jit.supers_displays.enabled()
        || vm.jit.supers_displays.is_published(raw)
    {
        return;
    }
    let ordinary_class = vm
        .classes
        .class_manager
        .try_read_untracked()
        .and_then(|cm| cm.get_class(component).map(|c| !c.name.starts_with('[')))
        .unwrap_or(false);
    if ordinary_class {
        vh_const_publish_display(vm, component, false);
    }
}

/// Probe [`JIT_TYPECHECK_TARGET_CACHE`] under `epoch`. A table stamped with any
/// other epoch is emptied first, so a name that no longer resolves to the
/// cached `ClassId` — or no longer resolves uniquely at all — can never be
/// served from here.
fn jit_typecheck_target_cached(cache_key: (usize, usize, usize), epoch: u64) -> Option<ClassId> {
    JIT_TYPECHECK_TARGET_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let (stamp, entries) = &mut *cache;
        if *stamp != epoch {
            entries.clear();
            *stamp = epoch;
            return None;
        }
        entries
            .iter()
            .find(|(cached_vm, cached_ptr, cached_len, _)| {
                *cached_vm == cache_key.0
                    && *cached_ptr == cache_key.1
                    && *cached_len == cache_key.2
            })
            .map(|(_, _, _, raw)| ClassId::new(*raw))
    })
}

/// Record `(site key) -> resolved target class id` under `epoch`, evicting the
/// oldest entry once the cache is full.
///
/// `epoch` must be the `class_definition_epoch` that was current BOTH before
/// and after the resolution — the same discipline
/// [`jit_typecheck_negative_cache_put`] follows — so a mapping mutation that
/// raced the resolve cannot be recorded as if it had not happened.
fn jit_typecheck_target_cache_put(cache_key: (usize, usize, usize), target: ClassId, epoch: u64) {
    JIT_TYPECHECK_TARGET_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let (stamp, entries) = &mut *cache;
        if *stamp != epoch {
            entries.clear();
            *stamp = epoch;
        }
        let entry = (cache_key.0, cache_key.1, cache_key.2, target.as_u32());
        if let Some(slot) = entries.iter_mut().find(|(vm, ptr, len, _)| {
            *vm == cache_key.0 && *ptr == cache_key.1 && *len == cache_key.2
        }) {
            *slot = entry;
            return;
        }
        if entries.len() >= JIT_TYPECHECK_TARGET_CACHE_CAP {
            entries.remove(0);
        }
        entries.push(entry);
    });
}

/// Whether `receiver_raw` is a `java.lang.ClassLoader` — the one receiver the
/// null-resource-name early-NPE arms in [`jit_invoke_dispatch`] and the MIC
/// dispatch helper exist for.
///
/// Round 11 (lane `rt`): those arms matched on the method NAME and descriptor
/// alone (`getResource(String)URL`, `getResources`, `getResourceAsStream`),
/// before any receiver check, so a compiled `x.getResource(null)` threw
/// `NullPointerException` for EVERY receiver class with that signature —
/// `ServletContext.getResource(null)` (whose contract is
/// `MalformedURLException`) or a user type that accepts null — while the
/// interpreter ran the receiver's own method. Only a class loader's resource
/// methods carry the NPE contract the arms enforce. Anything this cannot prove
/// is a class loader (including a null or unvalidatable receiver) answers
/// `false` and takes ordinary dispatch, which is what the interpreter does.
///
/// Reached only with a null name argument, i.e. never on a throughput path.
#[cold]
#[inline(never)]
fn null_resource_name_receiver_is_class_loader(
    vm: &SharedVm,
    receiver_raw: i64,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // Retired over a real JDK (`ClassRealm::classloader_null_name_intercepts`;
    // interpreter round i2 wave 55, lane L5): the three doors decline and the
    // compiled call dispatches as any other.
    if !vm.classes.classloader_null_name_intercepts {
        crate::vm::note_classloader_null_name_retired(vm, None, "jit", method_name, descriptor);
        return false;
    }
    let raw = receiver_raw as u64;
    if raw == 0 || (raw & 0x7) != 0 {
        return false;
    }
    let Some(receiver) = vm.mem.heap.is_object_address(raw as usize) else {
        return false;
    };
    // An array is never a ClassLoader, whatever its component is.
    let Some(receiver_cid) = jit_dispatch_class_validated(vm, receiver).class_id() else {
        return false;
    };
    let loader_cid = vm
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("java/lang/ClassLoader");
    loader_cid.is_some_and(|loader| jit_is_subclass_of_cached(vm, receiver_cid, loader))
        // ...that INHERITS `ClassLoader`'s method: an override (a user loader
        // answering a null name itself, `URLClassLoader.getResourceAsStream`)
        // runs its own body, as on HotSpot and in the interpreter (interpreter
        // round i2 wave 54, lane L5).
        && crate::vm::class_inherits_class_loader_method(vm, receiver_cid, method_name, descriptor)
}

/// `ClassManager::is_subclass_of` with the positive-answer memo described on
/// [`JIT_SUBTYPE_POSITIVE_CACHE`]. Behaviour-identical to calling
/// `is_subclass_of` directly: a hit can only ever replace a call that would
/// have returned `true` with `true`.
fn jit_is_subclass_of_cached(vm: &SharedVm, child: ClassId, parent: ClassId) -> bool {
    let vm_key = vm.vm_identity;
    let key = (vm_key, child.as_u32(), parent.as_u32());
    // JVMTI class redefinition cannot change superclass or interface identity,
    // so a positive subtype result remains valid across method-body changes.
    // An UNLOAD is a different hazard entirely -- it can hand `child`'s or
    // `parent`'s numeric id to an unrelated class -- and is what
    // `class_definition_epoch` catches; see the stamp rationale on
    // `JIT_SUBTYPE_POSITIVE_CACHE`'s own doc. A stale-epoch table is cleared
    // rather than consulted, exactly like `JIT_TYPECHECK_NEGATIVE_CACHE`.
    // This VM's store's epoch ([`typecheck_memo_epoch`]).
    let epoch = typecheck_memo_epoch(vm);
    let hit = JIT_SUBTYPE_POSITIVE_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let (stamp, entries) = &mut *cache;
        if *stamp != epoch {
            entries.clear();
            *stamp = epoch;
            return false;
        }
        entries.iter().any(|e| *e == key)
    });
    if hit {
        return true;
    }
    // Lock-free, positive AND negative, once `child`'s supers closure is
    // published (the locked call below publishes it). Same relation as
    // `is_subclass_of`; see `typecheck::class_is_subtype`. Asked after the
    // memo so a stale-epoch table is still cleared and re-stamped above.
    if let Some(verdict) = crate::runtime::interpreter::published_subtype_verdict(vm, child, parent)
    {
        return verdict;
    }
    let is_subclass = {
        vm.classes
            .class_manager
            .read()
            .is_subclass_of(child, parent)
    };
    if is_subclass {
        // Re-read rather than reuse `epoch`: `is_subclass_of` takes only the
        // class-manager READ lock and neither defines nor unloads a class, so
        // this is expected to be a no-op re-load in practice, but skipping it
        // would stamp the entry with a possibly-already-stale epoch if some
        // OTHER mutator thread unloaded a class concurrently while this one
        // held the read lock. Mirrors `jit_typecheck_target_cache_put`'s
        // "current both before and after" requirement; an entry proven under
        // an epoch that has already moved is simply not cached; the next
        // lookup recomputes it.
        let put_epoch = typecheck_memo_epoch(vm);
        if put_epoch == epoch {
            JIT_SUBTYPE_POSITIVE_CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();
                let (stamp, entries) = &mut *cache;
                if *stamp != put_epoch {
                    entries.clear();
                    *stamp = put_epoch;
                }
                if entries.len() >= JIT_SUBTYPE_CACHE_CAP {
                    entries.remove(0);
                }
                entries.push(key);
            });
        }
    }
    is_subclass
}

/// Common type-check resolution shared by `jit_checkcast` and `jit_instanceof`.
///
/// Returns `true` if `obj_ref` (which must be non-null and live) is an instance
/// of the class named `class_name`. Mirrors the interpreter's `Instanceof` /
/// `Checkcast` semantics exactly:
///
/// 1. Resolve the target class via `load_class_concurrent` so a not-yet-loaded
///    target class is loaded on demand. This is the fix for the long-standing
///    JIT instanceof miscompile that returned `false` whenever the target class
///    happened to be loaded only after the JIT call site warmed up.
/// 2. Fall back to `lambda_proxy_satisfies` for objects whose class id is a
///    synthetic lambda proxy (>= 0x8000_0000 — never present in `class_store`).
/// 3. Fall back to `synthetic_implements` for the hand-built collection helper
///    classes whose interface relationships live in `synthetic_implements`
///    rather than in the loaded class hierarchy.
///
/// # Safety
/// Caller must ensure `vm_ptr` is a valid `SharedVm` pointer and `obj_ref` is
/// derived from a live heap object (or that the caller has already short-circuited
/// the null case). The function holds only short-lived `class_manager.read()` /
/// `class_manager.write()` locks and never reborrows the heap.
// SAFETY: Caller must ensure vm_ptr (via `vm`) is a valid SharedVm reference and obj_ref is
// derived from a live heap object. Only short-lived class_manager read/write locks are held;
// the heap is never reborrowed. The null case must be handled by the caller before entry.
/// `CRATONVM_DBG=typecheck-filter=<substring>` — name the branch that decided a
/// compiled `checkcast`/`instanceof` whose TARGET class name contains the
/// substring.
///
/// A compiled type check leaves no other trace. By the time anything observable
/// happens the answer has already collapsed into a taken/not-taken branch, so a
/// disagreement with the interpreter surfaces only as a wrong result far
/// downstream. `CRATONVM_JIT_DENY=<Class>.<method>` says WHICH compiled body is
/// wrong; this says WHY, by printing the ids actually compared.
///
/// Prints the RECEIVER's class id and name beside the target so a two-ids-one-name
/// split is visible directly, which is the failure this was written to catch.
/// A compiled type check reached its helper with NO target class name.
///
/// `bytecode_walk`'s `0xc0`/`0xc1` emission falls back to
/// `(pc, ptr::null(), 0)` when the pc is absent from `typecheck_info_idx`, and
/// both helpers answer that shape defensively — `instanceof` with `0`,
/// `checkcast` fails closed. Defensible as a guard against a malformed
/// artifact; catastrophic if it is reachable from a NORMAL compile, because
/// then a live `instanceof` has been compiled to constant `false` and nothing
/// downstream can tell. This fires only when `CRATONVM_DBG_TYPECHECK_FILTER`
/// is set, and its whole purpose is to answer "is this shape actually
/// reachable?" — which no other instrument can, since the answer never reaches
/// `jit_typecheck_resolve`.
fn jit_typecheck_null_name_trace(kind: &str) {
    // t1 wave 3 (lane Y): the typed field, not a direct variable read. Read
    // per call (j27 wave 7, lane T1IC: was a process `OnceLock`): a typed
    // field of the latched snapshot is one load, and the shape is rare.
    if cratonvm_types::flags::flags()
        .loader
        .dbg_typecheck_filter
        .is_none()
    {
        return;
    }
    eprintln!("[DBG_TYPECHECK] NULL-TARGET-NAME: {kind} compiled with no target class -> answering false/closed WITHOUT resolving");
}

/// The `CRATONVM_DBG_TYPECHECK_FILTER` substring, read once (from the typed
/// field `LoaderFlags::dbg_typecheck_filter` since t1 wave 3, lane Y). Still
/// latched: it is read on the typecheck resolve path (see the next item), and
/// a debug filter needs no per-VM answer.
fn jit_typecheck_trace_filter() -> Option<&'static str> {
    use std::sync::OnceLock;
    static FILTER: OnceLock<Option<String>> = OnceLock::new();
    FILTER
        .get_or_init(|| {
            cratonvm_types::flags::flags()
                .loader
                .dbg_typecheck_filter
                .clone()
        })
        .as_deref()
}

/// j27 wave 3 (lane CLONE; page
/// `javac-zipfs-typecheck-sites-and-instance-clone-leave-compiled-code-20261027.md`
/// item 1): the per-call type-check CENSUS, on the trace's own switch
/// (`CRATONVM_DBG_TYPECHECK_FILTER` / `CRATONVM_DBG=typecheck-filter=<sub>`):
/// why a compiled `instanceof` / `checkcast` reached its helper, in terms of
/// the inline arms that declined it. Read BEFORE the helper publishes a
/// missing display, so `no-display` names the state the compiled code saw.
/// `None` with the switch off (one `OnceLock` read).
fn jit_typecheck_census_reason(
    vm: &SharedVm,
    obj_ref: ObjectRef,
    obj_class_id: ClassId,
) -> Option<&'static str> {
    jit_typecheck_trace_filter()?;
    Some(jit_typecheck_inline_miss_reason(vm, obj_ref, obj_class_id))
}

/// [`jit_typecheck_census_reason`]'s answer: `array-receiver` (no display or
/// row exists for an array), `lambda-proxy` (a synthetic id `>= 0x8000_0000`,
/// round rx27), `displays-off` (the VM keeps none), `no-display`
/// (the receiver's class had none published: the display and row-first arms
/// could not answer), `display-verdict-0` (published, but a miss is not the
/// hierarchy's alone, so every miss goes to the helper), or `display-hierarchy-only`
/// (the arms had the facts: the site's own arm set declined it -- an interface
/// target without an interface arm, a target whose own admission screen
/// refuses, or a site compiled before the display existed).
#[cold]
#[inline(never)]
fn jit_typecheck_inline_miss_reason(
    vm: &SharedVm,
    obj_ref: ObjectRef,
    obj_class_id: ClassId,
) -> &'static str {
    if vm.mem.heap.kind_of(obj_ref) == cratonvm_types::ObjectKind::Array {
        return "array-receiver";
    }
    // Round rx27 (lane compile): a lambda-proxy id (or the autobox sentinel)
    // is in no display; only the optimizing tier's proxy ways answer it
    // inline (`CRATONVM_JIT_IR_INSTANCEOF_PROXY_WAYS`, and for a cast
    // `CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS`), so a census line with this
    // reason is a proxy those ways missed, or a single-pass site.
    if obj_class_id.as_u32() >= crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE {
        return "lambda-proxy";
    }
    let displays = &vm.jit.supers_displays;
    if !displays.enabled() {
        return "displays-off";
    }
    if !displays.is_published(obj_class_id.as_u32()) {
        return "no-display";
    }
    let cm = vm.classes.class_manager.read();
    if supertype_closure_is_hierarchy_only(&cm, obj_class_id) {
        "display-hierarchy-only"
    } else {
        "display-verdict-0"
    }
}

/// The helper's answer as the census prints it.
fn jit_typecheck_census_answer(answer: Option<bool>, raised: bool) -> &'static str {
    match (raised, answer) {
        (true, _) => "raise",
        (false, Some(true)) => "true",
        (false, Some(false)) => "false",
        (false, None) => "unanswered",
    }
}

/// One census line, `[DBG_TYPECHECK] census op=.. site=.. target=..
/// receiver=<id>:<name> reason=.. answer=..`, for a target whose name holds the
/// filter. `site` is the site's interned name address (the key the checkcast
/// pass memo already uses), so `sort | uniq -c` ranks sites, targets and
/// reasons. Rank with e.g. `CRATONVM_DBG_TYPECHECK_FILTER=/` (every named
/// class) and `grep -o 'target=[^ ]* .* reason=[^ ]*' | sort | uniq -c | sort -rn`.
///
/// SAFETY: `name_ptr` / `name_len` are the site's class-name slice, non-null
/// and positive (checked by both helpers before this is reached).
#[cold]
#[inline(never)]
unsafe fn jit_typecheck_census_line(
    vm: &SharedVm,
    op: &str,
    (name_ptr, name_len): (*const u8, usize),
    obj_class_id: ClassId,
    reason: &str,
    answer: &str,
) {
    let Some(filter) = jit_typecheck_trace_filter() else {
        return;
    };
    // SAFETY: as the function contract.
    let bytes = std::slice::from_raw_parts(name_ptr, name_len);
    let name = std::str::from_utf8(bytes).unwrap_or("<not-utf8>");
    if !name.contains(filter) {
        return;
    }
    let receiver = vm
        .classes
        .class_manager
        .read()
        .get_class(obj_class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    eprintln!(
        "[DBG_TYPECHECK] census op={op} site={:#x} target={name} receiver={}:{receiver} \
         reason={reason} answer={answer}",
        name_ptr as usize,
        obj_class_id.as_u32(),
    );
}

#[cfg(test)]
mod j27_typecheck_census_tests {
    use super::*;

    /// An array receiver and a plain receiver whose class never published a
    /// display name the arm that could not answer them.
    #[test]
    fn j27_the_census_names_the_receiver_state_the_arms_saw() {
        let vm = SharedVm::new(crate::config::VmConfig::default());
        let ints = vm.mem.heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 2);
        assert_eq!(jit_typecheck_inline_miss_reason(&vm, ints, ClassId::new(0)), "array-receiver");
        let plain_class = ClassId::new(0x7123);
        let plain = vm.mem.heap.alloc_object(plain_class, 0);
        let reason = jit_typecheck_inline_miss_reason(&vm, plain, plain_class);
        let expected = if vm.jit.supers_displays.enabled() { "no-display" } else { "displays-off" };
        assert_eq!(reason, expected);
    }

    /// Round rx27: a receiver in the lambda-proxy id range is named as such,
    /// whatever the displays say (none is ever published for it).
    #[test]
    fn rx27_the_census_names_a_lambda_proxy_receiver() {
        let vm = SharedVm::new(crate::config::VmConfig::default());
        let proxy_class = ClassId::new(crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE + 7);
        let proxy = vm.mem.heap.alloc_object(proxy_class, 0);
        assert_eq!(jit_typecheck_inline_miss_reason(&vm, proxy, proxy_class), "lambda-proxy");
        let plain_class = ClassId::new(crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE - 1);
        let plain = vm.mem.heap.alloc_object(plain_class, 0);
        assert_ne!(jit_typecheck_inline_miss_reason(&vm, plain, plain_class), "lambda-proxy");
    }

    #[test]
    fn j27_the_census_answer_puts_a_raise_first() {
        assert_eq!(jit_typecheck_census_answer(Some(true), false), "true");
        assert_eq!(jit_typecheck_census_answer(Some(false), false), "false");
        assert_eq!(jit_typecheck_census_answer(None, false), "unanswered");
        assert_eq!(jit_typecheck_census_answer(Some(false), true), "raise");
    }
}

/// The trace gate, inlined into every caller: one `OnceLock` read and a
/// branch. `jit_typecheck_trace` used to be an out-of-line call made up to four
/// times per `jit_typecheck_resolve` just to find the filter unset — 2.1% of
/// `CratonBenchC2 dispatch` on its own (round 9 wave 2 `perfdiag`,
/// `perf-negative-typechecks-take-the-full-hierarchy-walk-every-time-20260918.md`).
#[inline(always)]
fn jit_typecheck_trace(
    vm: &SharedVm,
    obj_class_id: ClassId,
    class_name: &str,
    lenient: bool,
    stage: &str,
    target: Option<ClassId>,
) {
    if let Some(filter) = jit_typecheck_trace_filter() {
        jit_typecheck_trace_armed(vm, obj_class_id, class_name, lenient, stage, target, filter);
    }
}

#[cold]
#[inline(never)]
fn jit_typecheck_trace_armed(
    vm: &SharedVm,
    obj_class_id: ClassId,
    class_name: &str,
    lenient: bool,
    stage: &str,
    target: Option<ClassId>,
    filter: &str,
) {
    if !class_name.contains(filter) {
        return;
    }
    let cm = vm.classes.class_manager.read();
    let obj_name = cm
        .get_class(obj_class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    let obj_loader = cm
        .get_class(obj_class_id)
        .map(|c| format!("{:?}", c.loader_id))
        .unwrap_or_else(|| "<unknown>".to_string());
    let target_desc = match target {
        Some(t) => format!(
            "{} (id={} loader={:?})",
            cm.get_class(t)
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| "<unknown>".to_string()),
            t.as_u32(),
            cm.get_class(t).map(|c| c.loader_id)
        ),
        None => "<none>".to_string(),
    };
    eprintln!(
        "[DBG_TYPECHECK] {stage}: target_name={class_name} lenient={lenient} recv={obj_name} (id={} loader={obj_loader}) resolved_target={target_desc}",
        obj_class_id.as_u32()
    );
}

/// The target of a holder-keyed type-check site (one the compiler could not
/// resolve; see `cratonvm_jit::intern_typecheck_unresolved_in`), resolved
/// through the holder's own defining loader, then recorded on the site.
///
/// `None` for any other site. A holder-keyed site whose class cannot be
/// resolved raises the resolution's Java error (i11-L2: the interpreter's
/// `checkcast` / `instanceof` throw it at the same site, JVMS §6.5), stashed
/// as [`TypecheckRaise::Pending`]; only when no Java throwable can be built
/// (no JIT thread, an internal error) does it answer `None` and take the
/// legacy by-name path below, as it did before sites carried their holder.
///
/// Only the single-pass doors intern holder-keyed sites
/// (`cratonvm_jit::intern_typecheck_unresolved_in`); the optimizing tier
/// admits a type check only with a compile-time-resolved target, so an
/// `Op::InstanceOf` never reaches a raise and needs no sentinel drain.
///
/// # Safety
/// As [`jit_typecheck_resolve`]. The loader-driving arm runs Java and so can
/// move `*obj_ref`; it is pinned across the call and refreshed. After a
/// raise `*obj_ref` may be stale (building the throwable can collect), and
/// every caller returns without reading it.
unsafe fn jit_typecheck_holder_site_target(
    vm: &SharedVm,
    class_name: &str,
    obj_ref: &mut ObjectRef,
) -> Option<ClassId> {
    let (holder_word, site_index) =
        cratonvm_jit::typecheck_holder_word_for_site(class_name.as_ptr())?;
    // The entry this code's index names now: translated when the code
    // predates its holder's redefinition (interpreter round i1 wave 21, lane
    // L1), so the JVMS §5.4.3 record below is keyed as an interpreter frame
    // of that code keys it. `fill_as_of` is read before the translation (the
    // key's pool read): a failure recorded below is published only while no
    // redefinition of the holder may have moved the key since (i22-L5,
    // interpreter round i1 wave 24).
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    // The index, and whether the holder's loader already knows the class,
    // under one guard: the key is judged against the pool as that guard holds
    // it (`typecheck_site_index_in`; interpreter round i1 wave 44, lane L2).
    // Through the loader-faithful rule, as `jit_resolve_cp_class`: a
    // JDK-global name the holder's loader may define itself is asked of the
    // loader below rather than answered with java.base's class (interpreter
    // round i1 wave 38, lane L2; `L2W38JdkNameColdTypeCheck`).
    let (holder, cp_index, known) = {
        let cm = vm.classes.class_manager.read();
        let (holder, cp_index) = typecheck_site_index_in(&cm, holder_word, site_index, class_name);
        let known = crate::runtime::interpreter::class_resolved_without_loading(
            vm, &cm, holder, class_name,
        )
        .and_then(|id| {
            crate::runtime::interpreter::jit_bridge::loader_faithful_global_name_answer(
                vm, &cm, holder, class_name, id,
            )
        });
        (holder, cp_index, known)
    };
    // The entry's recorded resolution first, as `jit_resolve_cp_class` (round
    // 15 wave 5, lane classsite2): the interpreter's cast of this entry
    // answers it.
    let known = jit_recorded_class_entry(vm, holder, cp_index).or(known);
    // JVMS §5.4.3 (i11-L2, wave 12): an entry whose resolution already failed
    // -- in either tier -- rethrows the recorded error without asking a loader
    // again, even if the class has since become loadable. Same record, key and
    // position as the interpreter's `resolve_cast_target` and this file's
    // `jit_resolve_cp_class`; one relaxed load when nothing ever failed.
    if crate::runtime::interpreter::constants::any_resolution_failure_recorded(vm) {
        if let Some((thread, _guard)) = jit_thread_mut() {
            // Building the throwable can collect: pinned, as below.
            let pin = thread.native_pin_roots.len();
            thread.native_pin_roots.push(*obj_ref);
            let recorded = crate::runtime::interpreter::recorded_resolution_failure(
                vm, thread, holder, cp_index,
            );
            *obj_ref = thread.native_pin_roots[pin];
            thread.native_pin_roots.truncate(pin);
            if let Some(failure) = recorded {
                if let crate::error::MethodCallFailed::ExceptionThrown(exc) = failure {
                    set_jit_pending_exception(thread, exc);
                    JIT_TYPECHECK_RAISE.with(|d| *d.borrow_mut() = Some(TypecheckRaise::Pending));
                }
                return None;
            }
        }
    }
    let target = match known {
        Some(id) => id,
        None => {
            let (thread, _guard) = jit_thread_mut()?;
            let pin = thread.native_pin_roots.len();
            thread.native_pin_roots.push(*obj_ref);
            // Through the entry's record (round 15 wave 5, lane classsite2).
            let resolved = crate::runtime::interpreter::constants::resolve_class_entry_loader_aware(
                vm,
                thread,
                (holder, cp_index),
                class_name,
                fill_as_of,
            );
            *obj_ref = thread.native_pin_roots[pin];
            thread.native_pin_roots.truncate(pin);
            match resolved {
                Ok(id) => {
                    // JVMS §5.4.3: a failure another thread recorded for the
                    // entry meanwhile is its outcome, as in the interpreter's
                    // `resolve_cast_target` (interpreter round i1 wave 38,
                    // lane L5; `--jdk-only`). Pinned as above: building the
                    // throwable can collect.
                    let pin = thread.native_pin_roots.len();
                    thread.native_pin_roots.push(*obj_ref);
                    let recorded =
                        crate::runtime::interpreter::recorded_resolution_failure_after_success(
                            vm, thread, holder, cp_index,
                        );
                    *obj_ref = thread.native_pin_roots[pin];
                    thread.native_pin_roots.truncate(pin);
                    if let Some(failure) = recorded {
                        if let crate::error::MethodCallFailed::ExceptionThrown(exc) = failure {
                            set_jit_pending_exception(thread, exc);
                            JIT_TYPECHECK_RAISE
                                .with(|d| *d.borrow_mut() = Some(TypecheckRaise::Pending));
                        }
                        return None;
                    }
                    id
                }
                Err(e) => {
                    let converted = crate::runtime::exceptions::convert_class_not_found_for(
                        vm,
                        thread,
                        Some(holder),
                        class_name,
                        e,
                    );
                    // Recorded (a `LinkageError` only), so every later
                    // execution, compiled or interpreted, rethrows it without
                    // driving the loader again (i11-L2); a racing success
                    // recorded first is the entry's (t1 W14).
                    match crate::runtime::interpreter::constants::record_class_entry_failure_as_of(
                        vm, holder, cp_index, converted, fill_as_of,
                    ) {
                        Ok(winner) => winner,
                        Err(converted) => {
                            if let crate::error::MethodCallFailed::ExceptionThrown(exc) = converted {
                                set_jit_pending_exception(thread, exc);
                                JIT_TYPECHECK_RAISE
                                    .with(|d| *d.borrow_mut() = Some(TypecheckRaise::Pending));
                            }
                            return None;
                        }
                    }
                }
            }
        }
    };
    // The class-path package step of the interpreter's `checkcast` /
    // `instanceof` (`opcodes::resolve_cast_target` ->
    // `vm_util::check_class_path_package_unlinked_at_door`, interpreter round
    // i4 wave 94): an unlinked application class of a package sealed by
    // another JAR is HotSpot's `SecurityException` at its load. Raised as the
    // pending exception, neither recorded nor memoized (the site is recorded
    // only below), so the next execution asks again, as the interpreter's.
    // Pinned: the step runs Java. Interpreter round i4 wave 95, lane L5 (the
    // i92 page's second "Still open" item).
    if vm.classes.unlinked_doors_check_package {
        if let Some((thread, _guard)) = jit_thread_mut() {
            let pin = thread.native_pin_roots.len();
            thread.native_pin_roots.push(*obj_ref);
            let checked = crate::vm::check_class_path_package_unlinked_at_door(vm, thread, target);
            *obj_ref = thread.native_pin_roots[pin];
            thread.native_pin_roots.truncate(pin);
            if let Err(failure) = checked {
                if let crate::error::MethodCallFailed::ExceptionThrown(exc) = failure {
                    set_jit_pending_exception(thread, exc);
                    JIT_TYPECHECK_RAISE.with(|d| *d.borrow_mut() = Some(TypecheckRaise::Pending));
                }
                return None;
            }
        }
    }
    // JVMS §5.4.4 (i9-L2; every mode since i10-L2): the interpreter's `checkcast` /
    // `instanceof` refuse a class the holder may not access. The compiler
    // left such a site unbound (`jit_known_class`), so this is where compiled
    // code meets it: hand the refusal to `jit_typecheck_memoized` and never
    // record the site as resolved. The refusal itself is recorded against the
    // entry when it is raised (`jit_typecheck_raise_now`), as the interpreter's
    // is, so later executions take the recorded-failure arm above.
    if crate::runtime::interpreter::constants::class_constant_access_enforced(vm) {
        let denial = {
            let cm = vm.classes.class_manager.read();
            crate::runtime::interpreter::constants::class_constant_access_denial_in(
                vm,
                &cm,
                holder,
                class_name,
                Some(target),
            )
        };
        if let Some(message) = denial {
            // HotSpot's `unnamed module @0x<hash>` (lane L4's shared helper;
            // the guard above is released). Interpreter round i5 wave 100.
            let message = crate::vm::access_denial_with_unnamed_module_hash(vm, holder, message);
            JIT_TYPECHECK_RAISE.with(|d| {
                *d.borrow_mut() = Some(TypecheckRaise::AccessDenied {
                    message,
                    holder,
                    cp_index,
                    fill_as_of,
                })
            });
            return None;
        }
    }
    // The same validation the recorded-site branch applies before trusting an
    // id: it must name this site's class in this VM (a hidden class's own
    // class-file name included, wave 26 lane hiddenunload).
    let names_this_site = {
        let cm = vm.classes.class_manager.read();
        typecheck_site_names_class(&cm, target, class_name)
    };
    if names_this_site {
        cratonvm_jit::record_typecheck_target_for_site(class_name.as_ptr(), target.as_u32());
    }
    Some(target)
}

/// `CRATONVM_JIT_TYPECHECK_HIDDEN_SELF_SITE` (correctness, default on; `=0`
/// restores the stored-name-only check). JIT round 21 wave 26, lane
/// hiddenunload (`R21Fz9HiddenUnload1`, `docs/feature-designs/jdk-only-mode.md`).
///
/// Read only when a site's id names a HIDDEN class whose class-file name is
/// the site's name ([`typecheck_site_names_class`]), i.e. once per compiled
/// self cast before the answer memos and the site record take over; no
/// cached copy (the vm crate's static ratchet).
fn jit_typecheck_hidden_self_site_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TYPECHECK_HIDDEN_SELF_SITE")
}

/// Does class `id` name the class of the type-check site spelled
/// `class_name`? The validation every compiled `checkcast` / `instanceof`
/// applies to a site's id before answering by identity.
///
/// The stored name, or, for a HIDDEN class, its own class-file name
/// (`ClassManager::is_hidden_self_name`, the hidden self-name rule). A hidden
/// class is stored as `<this_class>/0x<hex>` while its constant pool still
/// spells `<this_class>`, so `(Self) o` in its own body (JVMS §5.4.3.1: the
/// entry resolves to the hidden class itself) never matched by stored name.
/// The id was then distrusted, and the by-name fallback asked the loader for
/// the bare name: it found nothing, LOADED the class path's same-named
/// ordinary class, and refused the hidden receiver with
/// `ClassCastException: class R21Fz9HU/0x.. cannot be cast to class
/// R21Fz9HU` (interpreted: no exception). A hidden class is in no loader's
/// namespace, so the only resolution that can hand a site such an id is the
/// self rule itself.
fn typecheck_site_names_class(
    cm: &crate::classloading::ClassManager,
    id: ClassId,
    class_name: &str,
) -> bool {
    typecheck_site_names_class_by(
        cm.get_class(id).map(|c| &*c.name),
        || cm.is_hidden_self_name(id, class_name),
        jit_typecheck_hidden_self_site_enabled,
        class_name,
    )
}

/// [`typecheck_site_names_class`] from its facts: the class's stored name
/// (`None`: not a class of this VM), whether the site's name is its hidden
/// self name, and the switch, the last two asked only on a stored-name miss.
fn typecheck_site_names_class_by(
    stored_name: Option<&str>,
    hidden_self_name: impl FnOnce() -> bool,
    switch_on: impl FnOnce() -> bool,
    class_name: &str,
) -> bool {
    match stored_name {
        Some(stored) if stored == class_name => true,
        Some(_) => hidden_self_name() && switch_on(),
        None => false,
    }
}

/// JIT round 21 wave 26, lane hiddenunload: a compiled self cast in a hidden
/// class trusts the site's id ([`typecheck_site_names_class`]).
#[cfg(test)]
mod r21w26_hiddenunload_typecheck_site_tests {
    use super::*;

    #[test]
    fn a_hidden_self_name_names_the_site_under_the_switch_only() {
        let site = "R21Fz9HU";
        let hidden = Some("R21Fz9HU/0x0000000000000004");
        assert!(typecheck_site_names_class_by(hidden, || true, || true, site));
        assert!(
            !typecheck_site_names_class_by(hidden, || true, || false, site),
            "=0 restores the stored-name-only check"
        );
        assert!(
            !typecheck_site_names_class_by(hidden, || false, || true, site),
            "a hidden class whose class-file name is another name does not name the site"
        );
    }

    #[test]
    fn a_stored_name_match_never_asks_the_hidden_rule_or_the_switch() {
        assert!(typecheck_site_names_class_by(
            Some("p/A"),
            || panic!("hidden rule asked on a stored-name match"),
            || panic!("switch read on a stored-name match"),
            "p/A",
        ));
        assert!(!typecheck_site_names_class_by(
            None,
            || panic!("hidden rule asked for an unknown id"),
            || panic!("switch read for an unknown id"),
            "p/A",
        ));
    }

    /// Every id validation in the compiled type check goes through the one
    /// predicate; a stored-name-only copy is what refused the hidden self
    /// cast (`R21Fz9HiddenUnload1`).
    #[test]
    fn the_type_check_validations_use_the_one_predicate() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let src = src.as_str();
        let start = src
            .find("\nunsafe fn jit_typecheck_holder_site_target(")
            .expect("holder site target");
        let end = src[start..]
            .find("\n/// JIT round 21 wave 26, lane hiddenunload: a compiled self cast")
            .map(|e| start + e)
            .expect("end of the predicate block");
        let resolve = src
            .find("\nunsafe fn jit_typecheck_resolve(")
            .expect("jit_typecheck_resolve");
        let resolve_end = src[resolve..]
            .find("// Fast path: target already loaded.")
            .map(|e| resolve + e)
            .expect("by-name tail");
        let stale = concat!("c.name == ", "class_name)");
        for body in [&src[start..end], &src[resolve..resolve_end]] {
            assert!(!body.contains(stale), "a stored-name-only site validation is back");
        }
        assert_eq!(
            src[resolve..resolve_end]
                .matches("typecheck_site_names_class(&cm, ")
                .count(),
            2,
            "the array branch and the recorded-site branch"
        );
    }
}

unsafe fn jit_typecheck_resolve(
    vm: &SharedVm,
    obj_class_id: ClassId,
    // GC-SAFETY (FMT-JIT-CCE): taken `&mut` so the slow path below can
    // refresh the caller's copy after a GC-triggering class load — see the
    // comment on that branch. Every read of `*obj_ref` in this function
    // after that point observes the refreshed address; `jit_checkcast`'s
    // caller reads it back too so its own return value isn't stale.
    obj_ref: &mut ObjectRef,
    class_name: &str,
    // `true` for `checkcast`, `false` for `instanceof`. Since i10-L2 both
    // answer one array rule (the lenient `Object[]` -> `T[]` carve-out is
    // gone); the flag still labels the trace and keys the answer memos.
    lenient: bool,
    // Set to `true` ONLY when this returns `false` from the recorded-site
    // (compile-time-resolved target) branch for a receiver whose answer is a
    // function of its class alone — see [`JIT_TYPECHECK_NEGATIVE_CACHE`] for
    // what that excludes and why. Left untouched on every other return.
    stable_negative: &mut bool,
    // Set to `true` when this returns `true` on the strength of the INSTANCE
    // rather than its class (annotation proxy, `Proxy$Instance` interface
    // slot, `Unmodifiable*` display class), so the caller does not memoize
    // it per class id. Only under [`jit_typecheck_instance_rules`].
    instance_admitted: &mut bool,
) -> bool {
    // KC26 array.clone() bug — descriptor-based array assignability.
    //
    // When the receiver is an array, falling through to the class-hierarchy
    // `is_subclass_of` path misses every legitimate case: primitive arrays
    // carry `class_id == 0` (no class entry), and reference arrays store
    // their *component* class id in the header (which is never a subclass
    // of the array class). The interpreter's `Checkcast` handler
    // (`runtime/interpreter.rs::6876`) computes the array's descriptor
    // and runs `array_is_assignable_to` — mirror that here so JIT-compiled
    // checkcast/instanceof on arrays returns the same result.
    //
    // Reproducer: `() -> SRC.clone()` on an `int[]` field returns null in
    // the JIT'd lambda body because `checkcast [I` after the clone() return
    // hit the false branch below and zeroed the result. With this branch
    // in place, the cast succeeds and the array round-trips correctly.
    // The array branch answers every array receiver and returns; nothing
    // after it sees one. A relocation does not change what kind of object it
    // is, so this is read once.
    let recv_is_array = vm.mem.heap.kind_of(*obj_ref) == cratonvm_types::ObjectKind::Array;
    if recv_is_array {
        // i11-L2: JVMS §6.5 resolves the target whatever the receiver, and
        // the interpreter's two opcodes now do. A site the compiler bound
        // resolved then (its id is validated as below); an unbound one
        // resolves here through its holder, and a failure or an access
        // refusal is raised rather than answered.
        let site_target = match cratonvm_jit::typecheck_target_for_site(class_name.as_ptr()) {
            Some(raw) => {
                let id = ClassId::new(raw);
                let names_this_site = {
                    let cm = vm.classes.class_manager.read();
                    typecheck_site_names_class(&cm, id, class_name)
                };
                names_this_site.then_some(id)
            }
            None => jit_typecheck_holder_site_target(vm, class_name, obj_ref),
        };
        if jit_typecheck_raise_stashed() {
            return false;
        }
        // The interpreter's verdict, whole: the identity answer when the
        // resolved target allows one, then the descriptor-free shapes, then
        // the descriptor path, which can load a component class — so the
        // receiver is pinned across it (it was not, before i11-L2, and
        // `jit_checkcast` returns the reference it holds afterwards).
        //
        // AUTHORITATIVE for an array receiver: never fall through to the
        // class-hierarchy path below. BUG-JIT-ARRAY-INSTANCEOF-20260726: a
        // reference array's header holds its COMPONENT's class id, so that
        // path answered `String[] instanceof String` (and `CharSequence`)
        // `true`; H2's `ObjectDataType.getTypeId` then classified a `String[]`
        // as a string once compiled. The verdict below handles `Object` /
        // `Serializable` / `Cloneable`, refuses every other non-array target
        // and recurses on components, so there is nothing to fall through for.
        let pinned = jit_thread_mut().map(|(thread, _guard)| {
            let idx = thread.native_pin_roots.len();
            thread.native_pin_roots.push(*obj_ref);
            idx
        });
        let verdict = crate::runtime::interpreter::array_receiver_cast_verdict(
            vm,
            *obj_ref,
            class_name,
            site_target,
        );
        if let Some(idx) = pinned {
            if let Some((thread, _guard)) = jit_thread_mut() {
                if let Some(&moved) = thread.native_pin_roots.get(idx) {
                    *obj_ref = moved;
                }
                thread.native_pin_roots.truncate(idx);
            }
        }
        return verdict;
    }

    // Loader-faithful target, resolved at COMPILE time.
    //
    // The compiler resolves this site's `CONSTANT_Class` entry through the
    // compiling class's own defining loader — the same resolution the
    // interpreter performs — and interns the site's name under the resulting
    // `ClassId` (`cratonvm_jit::intern_typecheck_target`). When that id is
    // available, the cast is an identity question and this function answers it
    // without ever consulting a name: no `find_unique_class_by_name`, and
    // critically no `is_assignable_to_name` fallback, so a same-named class
    // from a DIFFERENT loader is refused, which is the whole point of a
    // `(ClassLoaderId, name)`-keyed dictionary.
    //
    // The recorded id is validated first. `ClassId`s are per-VM and the intern
    // table is process-wide, so in a two-VM process an id recorded by one VM
    // could name something else in the other; and a redefinition can retire an
    // id outright. Confirming the id still names this site's class in THIS VM
    // costs one `&str` comparison on a class we had to look up anyway, and a
    // failed check simply falls through to the paths below.
    //
    // An array receiver never reaches here: the branch above answers it
    // authoritatively (i11-L2: including the site's resolution).
    //
    // A site the compiler could NOT resolve through that loader is interned
    // under its holder instead (`cratonvm_jit::intern_typecheck_unresolved_in`),
    // and is resolved HERE, through the holder's own loader, on its first
    // execution -- the resolution the interpreter's `checkcast`/`instanceof`
    // performs at the same program point -- and recorded, so it is a recorded
    // site from then on.
    jit_typecheck_trace(vm, obj_class_id, class_name, lenient, "enter", None);
    let site_target = cratonvm_jit::typecheck_target_for_site(class_name.as_ptr())
        .map(ClassId::new)
        .or_else(|| jit_typecheck_holder_site_target(vm, class_name, obj_ref));
    // A resolution failure or an access refusal at an unbound site is
    // raised, not answered: the by-name paths below would guess (and can
    // load a class, i.e. collect, for an answer nobody reads).
    if jit_typecheck_raise_stashed() {
        return false;
    }
    if let Some(target_class_id) = site_target {
        // A hidden class's own class-file name names it too (wave 26, lane
        // hiddenunload; [`typecheck_site_names_class`]).
        let names_this_site = {
            let cm = vm.classes.class_manager.read();
            typecheck_site_names_class(&cm, target_class_id, class_name)
        };
        jit_typecheck_trace(
            vm,
            obj_class_id,
            class_name,
            lenient,
            if names_this_site {
                "site-recorded"
            } else {
                "site-recorded-names-OTHER-class"
            },
            Some(target_class_id),
        );
        if names_this_site {
            if obj_class_id == target_class_id {
                return true;
            }
            if jit_is_subclass_of_cached(vm, obj_class_id, target_class_id) {
                return true;
            }
            if crate::runtime::interpreter::lambda_proxy_satisfies_public(
                vm,
                obj_class_id,
                target_class_id,
            ) {
                return true;
            }
            // The receiver's class genuinely is not this target. Synthetic and
            // annotation-proxy admission still apply — those encode their
            // relationships outside the class hierarchy, so an id comparison
            // cannot see them — but the loader-blind name walk does not.
            if crate::runtime::interpreter::synthetic_implements_public(
                vm,
                obj_class_id,
                class_name,
            ) {
                return true;
            }
            if crate::runtime::interpreter::annotation_proxy_satisfies_target(
                vm, *obj_ref, class_name,
            ) {
                *instance_admitted = jit_typecheck_instance_rules();
                return true;
            }
            // A `Proxy$Instance` object (the degrade / failed-generation
            // fallback) implements what its slot-1 `Class[]` names: the
            // interpreter's `op_checkcast` / `op_instanceof` ask this, and a
            // compiled site refused it -- then memoized the refusal for the
            // shim's one class id (round 13 wave 7, lane predicates). A
            // generated `$ProxyN` and a user subclass of `Proxy` are answered
            // by their class above; this screen declines them.
            if jit_typecheck_instance_rules()
                && crate::runtime::interpreter::proxy_instance_satisfies_resolved(
                    vm,
                    *obj_ref,
                    class_name,
                    target_class_id,
                )
            {
                *instance_admitted = true;
                return true;
            }
            // Display-class admission for the eleven `cratonvm/internal/
            // Unmodifiable*` stamps (`Map.of`/`List.of`/`Set.of`/`copyOf`/
            // `Collections.unmodifiable*`) — the interpreter's
            // `op_instanceof`/`op_checkcast` have consulted this since H18-1;
            // this JIT-compiled path did not, so `x instanceof RandomAccess`
            // on `Collections.unmodifiableList(arrayList)` answered `true`
            // interpreted and `false` once `Collections.binarySearch` (the
            // method that branches on it) tiered up — H18-3 §2 measured the
            // interpreted-mode cost of taking the wrong branch at 886x-911x
            // on a 60k-element search. Thread-free twin because this
            // function has no `&mut JvmThread` to pin `obj_ref` against a
            // class load; see its doc for the resulting (rare) weaker case.
            if crate::runtime::interpreter::display_class_satisfies_name_public(
                vm,
                *obj_ref,
                obj_class_id,
                class_name,
            ) {
                *instance_admitted = jit_typecheck_instance_rules();
                return true;
            }
            // Loader-duplication fallback -- the TWIN of the one at the bottom
            // of this function, which this branch's early `return false` was
            // hiding from every site that had an id to record.
            //
            // The interpreter accepts a same-named class from another loader:
            // `op_checkcast`/`op_instanceof` both end their assignability
            // chain in `loader_aware_name_assignable`, and
            // `CRATONVM_LOADER_AWARE_RESOLUTION` defaults ON. So refusing here
            // is not "stricter", it is a JIT/interpreter DIVERGENCE -- the same
            // one the by-name fallback below was already written to close for
            // `SpringBootContextLoaderAotTests` (Residual 6). That fix landed
            // on the by-name path only, and a site whose target class WAS
            // loaded at compile time never reaches it.
            //
            // Spring Boot's forked-classpath tests are where the gap shows.
            //
            // Not under `--jdk-only`, where the interpreter's rule is off too
            // (`op_checkcast`/`op_instanceof`): a class of
            // another loader is not the target, however it is spelled, and
            // the compile-time resolvers no longer bake an ancestor's copy of
            // a name the compiling class's loader has not initiated, which is
            // what this rule used to hide
            // (`spring-jdkonly-bytecode-cast-name-rule-jit-class-resolution-FIXED-20260923.md`).
            // `ModifiedClassPathClassLoader` re-defines a framework jar's
            // classes in a child loader, and CratonVM's flat store can hand
            // one CONSTANT_Class reference the app copy while the compiler
            // resolved this site to the child copy through the compiling
            // class's own loader. Under `-Jit on` that surfaced as
            //
            //   ClassCastException: class org.apache.hc.client5.http.psl.PublicSuffixList
            //     cannot be cast to class org.apache.hc.client5.http.psl.PublicSuffixList
            //
            // out of `PublicSuffixMatcher.<init>`, in four Spring Boot classes
            // that all pass under `--nojit`. `[DBG_TYPECHECK]` named it
            // exactly: `recv=(id=3125 loader=Application)` against
            // `resolved_target=(id=3126 loader=UserDefined(3))`, same binary
            // name on both sides.
            // With the row on under `--compatible`, the by-name answer it
            // overrides is counted (wave 52, lane L4; the walk the row-off
            // route pays here anyway).
            let named_row = cratonvm_types::HotSpotAlignment::NamespacedClassTypes;
            let row_on = vm.classes.aligned(named_row);
            if (!row_on || !vm.config.is_jdk_only())
                && vm
                    .classes
                    .class_manager
                    .read()
                    .is_assignable_to_name(obj_class_id, class_name)
            {
                if row_on {
                    vm.classes.note_alignment_divergence(named_row);
                } else {
                    jit_typecheck_trace(
                        vm,
                        obj_class_id,
                        class_name,
                        lenient,
                        "recorded-site-loader-duplication-by-name",
                        Some(target_class_id),
                    );
                    return true;
                }
            }
            jit_typecheck_trace(
                vm,
                obj_class_id,
                class_name,
                lenient,
                "REFUSED-by-recorded-site",
                Some(target_class_id),
            );
            // Every check above is a function of `(obj_class_id, target id,
            // the loaded class set)` EXCEPT two: an annotation proxy's answer
            // depends on the instance (its annotation type is a heap field),
            // and a lambda proxy's goes through a by-name load. Neither may be
            // memoized per class id. Nor may a `Proxy$Instance` (its slot-1
            // `Class[]`) or an `Unmodifiable*` stamp (its backing collection):
            // the interpreter's own negative-memo screen,
            // `receiver_class_determines_cast_verdict`, excludes exactly the
            // receivers whose inputs above are not their class, so both tiers
            // ask one predicate (round 13 wave 7, lane predicates).
            *stable_negative = if jit_typecheck_instance_rules() {
                crate::runtime::interpreter::receiver_class_determines_cast_verdict(
                    vm,
                    obj_class_id,
                )
            } else {
                obj_class_id.as_u32() < 0x8000_0000
                    && !vm
                        .classes
                        .class_manager
                        .read()
                        .get_class(obj_class_id)
                        .is_some_and(|c| &*c.name == "java/lang/annotation/AnnotationProxy")
            };
            return false;
        }
    }

    // Fast path: target already loaded. Most call sites hit this.
    //
    // IMPORTANT: bind the result to a local so the `RwLockReadGuard` temporary
    // from `.read()` is dropped at the semicolon. Using `if let Some(x) =
    // rwlock.read().method()` would extend the guard's lifetime to the entire
    // `if let` block (including the `else` branch), deadlocking any path that
    // later calls `load_class_concurrent` (which needs a write lock).
    let cache_key = (
        vm.vm_identity,
        class_name.as_ptr() as usize,
        class_name.len(),
    );
    // Read the epoch BEFORE the resolve, and require it unmoved afterwards
    // before recording: what is cached is a name -> `ClassId` mapping, and a
    // definition or unload that raced this lock acquisition must not be
    // memoized away. See [`JIT_TYPECHECK_TARGET_CACHE`]. This VM's store's
    // epoch ([`typecheck_memo_epoch`]).
    let epoch = typecheck_memo_epoch(vm);
    let cached_target = jit_typecheck_target_cached(cache_key, epoch);
    let target_class_id_opt = if cached_target.is_some() {
        cached_target
    } else {
        let resolved = vm
            .classes
            .class_manager
            .read()
            .find_unique_class_by_name(class_name);
        if let Some(target) = resolved {
            if typecheck_memo_epoch(vm) == epoch {
                jit_typecheck_target_cache_put(cache_key, target, epoch);
            }
        }
        resolved
    };
    jit_typecheck_trace(
        vm,
        obj_class_id,
        class_name,
        lenient,
        "by-name-fallback",
        target_class_id_opt,
    );
    // The target this tail resolved (already loaded, or loaded below), for
    // the proxy screen at the end: it must not load again by name, with
    // `*obj_ref` unpinned.
    let mut tail_target = target_class_id_opt;
    if let Some(target_class_id) = target_class_id_opt {
        if obj_class_id == target_class_id {
            return true;
        }
        if jit_is_subclass_of_cached(vm, obj_class_id, target_class_id) {
            return true;
        }
        // Lambda proxy fallback uses the *already-resolved* target id.
        if crate::runtime::interpreter::lambda_proxy_satisfies_public(
            vm,
            obj_class_id,
            target_class_id,
        ) {
            return true;
        }
    } else {
        // Slow path: target not yet loaded. Load it on demand using the
        // concurrent loader so we don't deadlock if another thread is racing
        // the same load. Failure is silently treated as "not assignable",
        // matching what HotSpot does for unresolvable targets in instanceof
        // (instanceof on an unresolvable target returns false; checkcast
        // would have been linked earlier and is a different failure mode).
        //
        // GC-SAFETY (FMT-JIT-CCE): `load_class_concurrent` parses/defines the
        // target class — allocating its `Class` mirror, constant-pool
        // strings, and method/field metadata, and potentially running a
        // user classloader's `loadClass`/`findClass` bytecode — any of which
        // can trigger a moving GC that relocates `*obj_ref`. Unlike the
        // interpreter's `Checkcast` handler (which pins the receiver in
        // `thread.native_pin_roots` around this exact class-resolution call
        // — see `runtime/interpreter.rs`), this JIT helper used to carry
        // `obj_ref` across the call unpinned and unrefreshed, so a GC landing
        // here left every later use of `*obj_ref` — the fallback checks
        // below, and critically `jit_checkcast`'s own returned pointer —
        // pointing at memory the collector had already reused. Observed live
        // as `ClassCastException: java.lang.Object cannot be cast to
        // org.jboss.as.controller.AttributeDefinition` (and other targets)
        // during WildFly's highly concurrent `parallel-extension-add` boot
        // step, where ~30 extension threads allocate/classload
        // simultaneously and a target interface like `AttributeDefinition`
        // is commonly resolved here for the first time under heavy GC
        // pressure. Pin `*obj_ref` in the current thread's
        // `native_pin_roots` (same mechanism, same pattern) across the call
        // and refresh it from the pin afterward.
        let current_thread = jit_get_current_thread();
        // Explicit `&mut *current_thread` reborrows (rather than letting
        // `.push()`/indexing implicitly autoref the raw pointer) — each is
        // scoped to a single statement and dropped well before
        // `load_class_concurrent` runs, so there's no borrow held across
        // that call (which may itself reborrow the same TLS thread pointer,
        // e.g. while running a classloader's bytecode).
        let pin_idx = if !current_thread.is_null() {
            let thread_ref: &mut JvmThread = &mut *current_thread;
            let idx = thread_ref.native_pin_roots.len();
            thread_ref.native_pin_roots.push(*obj_ref);
            Some(idx)
        } else {
            None
        };
        let load_result = vm.load_class_concurrent(class_name);
        if let Some(idx) = pin_idx {
            let thread_ref: &mut JvmThread = &mut *current_thread;
            *obj_ref = thread_ref.native_pin_roots[idx];
            thread_ref.native_pin_roots.truncate(idx);
        }
        if let Ok(target_class_id) = load_result {
            tail_target = Some(target_class_id);
            // NOT cached, deliberately. `jit_typecheck_target_cache_put` requires an
            // epoch that was current BOTH before and after the resolution, and the
            // resolution on this path IS a definition: `load_class_concurrent` moves
            // `class_definition_epoch` itself. The pre-load epoch would be stamped on
            // an entry the next lookup discards; the post-load epoch would claim a
            // mapping mutation that raced the load had not happened. Nothing is lost —
            // this arm runs once per site per load, and the next execution of the same
            // site finds the class loaded, takes the fast path above, and memoizes
            // there under an epoch that did hold still.
            if obj_class_id == target_class_id {
                return true;
            }
            if jit_is_subclass_of_cached(vm, obj_class_id, target_class_id) {
                return true;
            }
            if crate::runtime::interpreter::lambda_proxy_satisfies_public(
                vm,
                obj_class_id,
                target_class_id,
            ) {
                return true;
            }
        }
    }

    // Loader-duplication fallback (Residual 6,
    // `SpringBootContextLoaderAotTests`), now reachable ONLY for a site whose
    // target class was not loaded when the method was compiled.
    //
    // For every other site the block near the top of this function already
    // answered by identity, against the `ClassId` the compiler resolved through
    // the compiling class's own loader. This path is what is left when there
    // was no id to record: the helper has to resolve a bare NAME against a
    // dictionary keyed by `(ClassLoaderId, name)`, which returns one winner
    // even when two loaders defined the name (e.g. Spring's AOT-processing
    // child loader re-defining Groovy's `ClassInfo`). The receiver's `ClassId`
    // then never equals the resolved target's and the id-based checks above
    // wrongly refuse a cast the interpreter's loader-faithful CP resolution
    // would pass — under `checkcast` that surfaced as a SILENT null (see
    // `jit_checkcast`), observed live as `ClassInfo.getClassInfo()` returning
    // null only under `-Jit on`. Fall back to a name-based hierarchy walk
    // (supers + interfaces), mirroring the accepted `is_subclass_of_by_name`
    // tradeoff used for exception catch_type resolution. Off under
    // `--jdk-only`, like its twin in the recorded-site branch above (and
    // counted like it when the row overrides it under `--compatible`).
    let named_row = cratonvm_types::HotSpotAlignment::NamespacedClassTypes;
    let row_on = vm.classes.aligned(named_row);
    if (!row_on || !vm.config.is_jdk_only())
        && vm
            .classes
            .class_manager
            .read()
            .is_assignable_to_name(obj_class_id, class_name)
    {
        if !row_on {
            return true;
        }
        vm.classes.note_alignment_divergence(named_row);
    }

    // Name-based fallback for synthetic classes whose interface relationships
    // are encoded in `synthetic_implements` rather than in the class hierarchy.
    if crate::runtime::interpreter::synthetic_implements_public(vm, obj_class_id, class_name) {
        return true;
    }
    // Instance-aware annotation-proxy admission (the proxy's real annotation
    // type lives on the heap object, not its shared ClassId). Reads
    // `*obj_ref`, which by this point reflects the refresh above if the slow
    // path ran.
    if crate::runtime::interpreter::annotation_proxy_satisfies_target(vm, *obj_ref, class_name) {
        *instance_admitted = jit_typecheck_instance_rules();
        return true;
    }
    // `Proxy$Instance` admission -- the twin of the recorded-site arm above.
    if jit_typecheck_instance_rules() {
        if let Some(target_class_id) = tail_target {
            if crate::runtime::interpreter::proxy_instance_satisfies_resolved(
                vm,
                *obj_ref,
                class_name,
                target_class_id,
            ) {
                *instance_admitted = true;
                return true;
            }
        }
    }
    // Display-class admission — see the identical arm above in the
    // site-recorded branch for why. This is the twin for a site whose target
    // had no compile-time id to record, so it reaches the by-name walk
    // instead.
    if crate::runtime::interpreter::display_class_satisfies_name_public(
        vm,
        *obj_ref,
        obj_class_id,
        class_name,
    ) {
        *instance_admitted = jit_typecheck_instance_rules();
        return true;
    }

    false
}

/// `CRATONVM_PROXY_USER_SUBCLASS_ORDINARY` (default on; its reader is cached
/// once per process) also selects the compiled type check's instance rules
/// (round 13 wave 7, lane predicates): a `Proxy$Instance` object is admitted
/// from its interface slot as the interpreter admits it, a `true` that came
/// from an instance is not memoized per class id, and the negative memo
/// excludes what the interpreter's negative memo excludes. `0` restores the
/// compiled path as it was.
#[inline]
fn jit_typecheck_instance_rules() -> bool {
    cratonvm_native_builtins::reflect_annotations::proxy_user_subclass_is_ordinary()
}

/// Round 13 wave 7, lane predicates: the compiled type check admits a bare
/// `Proxy$Instance` from its interface slot, as the interpreter does, and does
/// not memoize that per-instance `true` (or any refusal) for the shim's shared
/// class id.
#[cfg(test)]
mod r13w7_predicates_proxy_shim_typecheck_tests {
    use super::*;
    use crate::classloading::{Class, ClassLoaderId, ClassState};
    use crate::runtime::proxy::PROXY_FIELD_INTERFACES;
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use std::sync::Arc;

    const IFACE: &str = "cratonvm/r13w7/PredicatesIface";
    const OTHER: &str = "cratonvm/r13w7/PredicatesOther";

    fn define(
        shared: &SharedVm,
        name: &str,
        access_flags: ClassAccessFlags,
        num_total_fields: usize,
        register: bool,
    ) -> ClassId {
        let mut cm = shared.classes.class_manager.write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Initialized,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        });
        if register {
            cm.register_class_name(ClassLoaderId::Application, name, id);
        }
        id
    }

    /// `(shim class, a Proxy$Instance whose slot-1 Class[] is {IFACE})`, or
    /// `None` when this heap cannot mint the fixture.
    fn fixture(shared: &SharedVm) -> Option<(ClassId, ObjectRef)> {
        let iface =
            ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT;
        let iface_id = define(shared, IFACE, iface, 0, true);
        define(shared, OTHER, iface, 0, true);
        let shim = define(
            shared,
            "java/lang/reflect/Proxy$Instance",
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            3,
            false,
        );
        let mirror = shared.mem.heap.alloc_object(shim, 0);
        shared
            .classes
            .class_mirrors_reverse
            .write()
            .insert(mirror, iface_id);
        let interfaces =
            shared
                .mem
                .heap
                .alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
        shared
            .mem
            .heap
            .set_array_element(interfaces, 0, Value::Object(Some(mirror)))
            .ok()?;
        let proxy = shared.mem.heap.alloc_object(shim, 3);
        if shared.mem.heap.class_id_of(proxy) != shim {
            return None;
        }
        shared.mem.heap.set_field(
            proxy,
            PROXY_FIELD_INTERFACES,
            Value::Object(Some(interfaces)),
        );
        Some((shim, proxy))
    }

    #[test]
    fn a_bare_proxy_instance_is_admitted_and_not_memoized() {
        if !jit_typecheck_instance_rules() {
            return; // CRATONVM_PROXY_USER_SUBCLASS_ORDINARY=0 in this environment
        }
        let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
        let Some((shim, proxy)) = fixture(&shared) else {
            return;
        };
        // Fresh `String`s: pointers no compiled site interned, so the check
        // takes the by-name tail.
        let iface = String::from(IFACE);
        let other = String::from(OTHER);
        let mut obj = proxy;
        let mut stable_negative = false;
        let mut instance_admitted = false;
        // SAFETY: `obj` is a live object of `shared`'s own heap and no
        // compiled code or JIT thread is involved.
        let admitted = unsafe {
            jit_typecheck_resolve(
                &shared,
                shim,
                &mut obj,
                &iface,
                false,
                &mut stable_negative,
                &mut instance_admitted,
            )
        };
        assert!(admitted, "Proxy$Instance for {{IFACE}} instanceof IFACE");
        assert!(
            instance_admitted,
            "a slot-1 answer must not be memoized per class id"
        );
        let mut instance_admitted = false;
        // SAFETY: as above.
        let refused = !unsafe {
            jit_typecheck_resolve(
                &shared,
                shim,
                &mut obj,
                &other,
                false,
                &mut stable_negative,
                &mut instance_admitted,
            )
        };
        assert!(refused, "Proxy$Instance for {{IFACE}} is not an OTHER");
        // Every such proxy shares the shim's id: its refusal is not a fact
        // about the class, in either tier's negative memo.
        assert!(
            !crate::runtime::interpreter::receiver_class_determines_cast_verdict(&shared, shim)
        );
    }
}

/// Raise what [`jit_typecheck_memoized`] handed back instead of an answer:
/// `i64::MIN` with the Java throwable pending (the sentinel every backend's
/// `checkcast` and `instanceof` lowering drains), or the historical fail-soft
/// `0` when no throwable can be built.
///
/// # Safety
/// Must run on the JIT thread whose type check produced `raise`.
unsafe fn jit_typecheck_raise_now(vm: &SharedVm, raise: TypecheckRaise) -> i64 {
    match raise {
        TypecheckRaise::Pending => i64::MIN,
        TypecheckRaise::AccessDenied {
            message,
            holder,
            cp_index,
            fill_as_of,
        } => {
            if let Some((thread, _jit_thread_guard)) = jit_thread_mut() {
                if let Ok(exc) = crate::runtime::exceptions::create_exception_object(
                    vm,
                    thread,
                    "java/lang/IllegalAccessError",
                    Some(&message),
                ) {
                    // JVMS §5.4.3, as the interpreter's
                    // `check_class_constant_access_as_of` records it.
                    let _ = crate::runtime::interpreter::constants::record_resolution_failure_as_of(
                        vm,
                        holder,
                        cp_index,
                        crate::error::MethodCallFailed::ExceptionThrown(exc),
                        fill_as_of,
                    );
                    set_jit_pending_exception(thread, exc);
                    return i64::MIN;
                }
            }
            0
        }
    }
}

/// One `checkcast` [`jit_checkcast`] passed: this OBJECT at this site
/// (netty compression wall, 2026-10-14; `CRATONVM_JIT_CHECKCAST_OBJECT_MEMO`).
///
/// A cast's verdict for one object at one site cannot change: the object's
/// class never does, a site's `CONSTANT_Class` resolves once, and the wider
/// answers the helper gives (instance-admitted proxies, stamps) are facts
/// about the instance, which this key names. Keyed like
/// [`DirectReceiverMemo`] -- `vm`, the address, `address_epoch` -- so a
/// collection that may move the object or reuse its address retires the
/// entry, plus the site's interned name pointer.
///
/// What it serves is the site the compile door could give no target id: a
/// `CONSTANT_Class` not loaded yet when the method compiled, which then calls
/// the helper -- a heap-membership walk and a memo scan -- on every
/// execution for the life of the body. The JDK's `Buffer.session()` is the
/// measured one: hot from the first `ByteBuffer` a JUnit run touches, long
/// before any FFM arena loads `AbstractMemorySegmentImpl`, then executed once
/// per `DirectByteBuffer` access with the same few segment objects.
#[derive(Clone, Copy)]
struct CheckcastPassMemo {
    vm: usize,
    site: usize,
    obj: i64,
    epoch: u64,
}

const CHECKCAST_PASS_MEMO_WAYS: usize = 4;

thread_local! {
    static CHECKCAST_PASS_MEMO: [Cell<Option<CheckcastPassMemo>>; CHECKCAST_PASS_MEMO_WAYS] =
        const { [const { Cell::new(None) }; CHECKCAST_PASS_MEMO_WAYS] };
    static CHECKCAST_PASS_MEMO_NEXT: Cell<usize> = const { Cell::new(0) };
}

/// `CRATONVM_JIT_CHECKCAST_OBJECT_MEMO` (default ON, `=0` asks the full
/// check every call). Read once per process: both arms answer the same.
fn checkcast_object_memo_enabled() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_CHECKCAST_OBJECT_MEMO")
    })
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// obj_ptr is 0 (null) or a valid heap pointer. class_name_ptr/class_name_len form a
// valid UTF-8 slice pointing into the JIT-compiled code's string table (or are null/<=0
// for an unresolved site, which fails closed). Returns obj_ptr on success, 0 on failure.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_checkcast(
    vm_ptr: i64,
    obj_ptr: i64,
    class_name_ptr: *const u8,
    class_name_len: i64,
) -> i64 {
    let f = || jit_checkcast_body(vm_ptr, obj_ptr, class_name_ptr, class_name_len);
    contain("jit_checkcast", OnPanic::Throw { vm_ptr }, i64::MIN, f)
}

/// Body of [`jit_checkcast`], run under its panic guard.
unsafe fn jit_checkcast_body(
    vm_ptr: i64,
    obj_ptr: i64,
    class_name_ptr: *const u8,
    class_name_len: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Null reference is always a valid cast (matches JVMS §6.5.checkcast).
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        if obj_ptr != 0 && cv_trace_enabled() {
            eprintln!(
                "[cv-checkcast-fail] implausible obj {:#x} -> silent null",
                obj_ptr
            );
        }
        return 0;
    }
    // Defensive: an unresolved typecheck site (no class_name attached) must
    // not silently allow the cast. Return 0 so the JIT-compiled code observes
    // a "failed cast" and falls back to the interpreter exception path.
    if class_name_len <= 0 || class_name_ptr.is_null() {
        if cv_trace_enabled() {
            eprintln!(
                "[cv-checkcast-fail] unresolved site, obj {:#x} -> silent null",
                obj_ptr
            );
        }
        return 0;
    }
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    let memo = checkcast_object_memo_enabled();
    let epoch = if memo { vm.mem.heap.address_epoch() } else { 0 };
    if memo
        && CHECKCAST_PASS_MEMO.with(|ways| {
            ways.iter().any(|cell| {
                cell.get().is_some_and(|m| {
                    m.obj == obj_ptr
                        && m.site == class_name_ptr as usize
                        && m.epoch == epoch
                        && m.vm == vm.vm_identity
                })
            })
        })
    {
        return obj_ptr;
    }
    crate::jit::helpers::note_membership_walk(4);
    let mut obj_ref = match vm.mem.heap.is_object_address(obj_ptr as usize) {
        Some(r) => r,
        None => {
            if cv_trace_enabled() {
                eprintln!(
                    "[cv-checkcast-fail] obj {:#x} FAILED is_object_address -> silent null",
                    obj_ptr
                );
            }
            return 0;
        }
    };
    let obj_class_id = vm.mem.heap.class_id_of(obj_ref);
    // j27 wave 3 (lane CLONE): the census reason, before the publish below.
    let census = jit_typecheck_census_reason(vm, obj_ref, obj_class_id);
    // JIT r21 wave 1 (lane typecheck, TYPECHECK21-4,
    // `CRATONVM_JIT_CHECKCAST_ENSURE_DISPLAY`, opt-in): a plain receiver whose
    // class has no supers display yet gets one, as `jit_instanceof` does, so
    // both tiers' checkcast display arm answers its next execution inline.
    // Lock-free once published; no lock is held here.
    if vm.jit.supers_displays.publish_on_checkcast()
        && vm.mem.heap.kind_of(obj_ref) != cratonvm_types::ObjectKind::Array
    {
        vh_const_ensure_display(vm, obj_class_id.as_u32());
    }
    // checkcast (`lenient = true` labels the site; one array rule since i10-L2).
    //
    // GC-SAFETY (FMT-JIT-CCE): `jit_typecheck_resolve` can trigger a moving
    // GC (via `load_class_concurrent` on a not-yet-loaded target) that
    // relocates `obj_ref`; it refreshes our local `obj_ref` in place through
    // the `&mut` when that happens. Return the (possibly refreshed)
    // `obj_ref.as_ptr()` on success, NOT the original `obj_ptr` argument —
    // returning the stale `obj_ptr` would hand the JIT-compiled caller a
    // dangling pointer into memory the collector already reused, which is
    // exactly what surfaced as `ClassCastException: java.lang.Object cannot
    // be cast to org.jboss.as.controller.AttributeDefinition` (and other
    // targets) during WildFly's concurrent `parallel-extension-add` boot.
    //
    // `None` = the site's class name is not valid UTF-8: keep the old
    // fail-soft `0` the `str::from_utf8` guard used to return here.
    let mut raise = None;
    let answer = jit_typecheck_memoized(
        vm,
        obj_class_id,
        &mut obj_ref,
        class_name_ptr,
        class_name_len as usize,
        true,
        &mut raise,
    );
    if let Some(reason) = census {
        let shown = jit_typecheck_census_answer(answer, raise.is_some());
        let name = (class_name_ptr, class_name_len as usize);
        jit_typecheck_census_line(vm, "checkcast", name, obj_class_id, reason, shown);
    }
    let Some(assignable) = answer else {
        return 0;
    };
    // JVMS §5.4.4 (i9-L2; every mode since i10-L2) and §6.5 (i11-L2): the
    // interpreter's `checkcast` throws IllegalAccessError / the resolution
    // error here whatever the receiver, so this does too, through the same
    // pending-exception sentinel as the CCE below.
    if let Some(raise) = raise {
        return jit_typecheck_raise_now(vm, raise);
    }
    if assignable {
        let passed = obj_ref.as_ptr() as i64;
        // Only an answer about the address the call was given, in the epoch
        // it was read in: a check that loaded a class may have moved it.
        if memo && passed == obj_ptr && vm.mem.heap.address_epoch() == epoch {
            let entry = CheckcastPassMemo {
                vm: vm.vm_identity,
                site: class_name_ptr as usize,
                obj: obj_ptr,
                epoch,
            };
            let victim = CHECKCAST_PASS_MEMO_NEXT.with(|c| {
                let v = c.get();
                c.set((v + 1) % CHECKCAST_PASS_MEMO_WAYS);
                v
            });
            CHECKCAST_PASS_MEMO.with(|ways| ways[victim].set(Some(entry)));
        }
        passed
    } else {
        // SAFETY: `class_name_ptr` is non-null with `class_name_len > 0`
        // (checked above) and points into the JIT string table; validity was
        // already proven by `jit_typecheck_memoized` returning `Some`.
        let class_name = std::str::from_utf8(std::slice::from_raw_parts(
            class_name_ptr,
            class_name_len as usize,
        ))
        .unwrap_or("<invalid-utf8>");
        if cv_trace_enabled() {
            // The receiver's OBJECT KIND disambiguates this failure: for a
            // reference array the header stores the *component* class id, and
            // the message below renders it by the component name — so a
            // genuine `T[]` → `T` refusal prints as "X cannot be cast to X"
            // and reads like a class-identity split. Print kind + array
            // descriptor so the two are separable (see SPRING-CRHM.1 in
            // `vm/src/jit/skip_list.rs`, where exactly that cost hours).
            let kind = vm.mem.heap.kind_of(obj_ref);
            let arr_desc = crate::runtime::interpreter::array_descriptor_of(vm, obj_ref);
            let cm = vm.classes.class_manager.read();
            let obj_cls_name = cm
                .get_class(obj_class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| "<none>".into());
            let target_cid = cm.find_unique_class_by_name(class_name);
            // `target_cid` is the LOADER-BLIND unique-by-name answer, which is
            // `None` the moment two loaders define the name -- so on exactly
            // the class-identity failures this trace exists to explain it
            // reports nothing. `site_target` is the identity the decision was
            // actually made against: the `ClassId` this site's
            // `CONSTANT_Class` resolved to through the COMPILING class's own
            // loader (`intern_typecheck_target`). Printed with the loaders of
            // both sides, because "obj_cid != site_target while both names
            // match" and "the target was never resolved" are opposite repairs.
            let site_target = cratonvm_jit::typecheck_target_for_site(class_name_ptr);
            let loader_of = |id: cratonvm_types::ClassId| {
                cm.get_class(id).map(|c| format!("{:?}", c.loader_id))
            };
            eprintln!(
                "[cv-checkcast-fail] typecheck REFUSED: obj={:#x} kind={:?} arr_desc={:?} obj_cid={} obj_loader={:?} obj_cls={} target_name={} target_cid={:?} site_target={:?} site_target_loader={:?}",
                obj_ptr,
                kind,
                arr_desc,
                obj_class_id.as_u32(),
                loader_of(obj_class_id),
                obj_cls_name,
                class_name,
                target_cid.map(|c| c.as_u32()),
                site_target,
                site_target.and_then(|t| loader_of(cratonvm_types::ClassId::new(t))),
            );
        }
        // A definitive refusal — the object's class is known and provably not
        // assignable — must throw ClassCastException per JVMS §6.5.checkcast,
        // NOT silently hand the compiled caller a null (the old behavior,
        // which converted every genuine type error into downstream data
        // corruption — and is what kept Residual 6 invisible for seven
        // sessions). Stash the CCE and return the i64::MIN sentinel; the
        // codegen's post-helper check (`emit_post_invoke_exception_check`)
        // routes it through the standard pending-exception drain. The
        // stale/implausible-pointer and unresolved-site branches above keep
        // the old fail-soft `0` — there the object's type is UNKNOWABLE, and
        // throwing would turn tolerated stale-reference reads into new
        // failures.
        // H2-CID0 (2026-08-01): same flag-free reclaimed-memory verdict the
        // interpreter's `checkcast` reporter emits. A compiled `checkcast` is
        // where a hot accessor like `MVStore`'s `readPageFromCache` actually
        // fails, and this path used to report nothing at all, so the run that
        // reproduces produced no evidence. `java.lang.Object` is `ClassId(0)`,
        // which is also the all-zero header the collector leaves over a
        // reclaimed span; free-list membership tells the two apart.
        // See old-sweep-liveness.md section 7.
        //
        // 2026-08-02: moved into `memory::reclaim_guard` so the three faces of
        // this defect — interpreted `checkcast`, compiled `checkcast`, and an
        // `invoke` dispatch miss — report the same verdict. This copy and the
        // interpreter's had already drifted (only the interpreter's consulted
        // the young-sweep ring), which is what a third copy for the dispatch
        // face would have compounded. The concurrent H2-CID0 follow-up that
        // landed here — ask the reclamation ring on EVERY failing cast, not only
        // on `ClassId(0)`, because a freed block only reads as
        // `java.lang.Object` while it stays on the free list and afterwards
        // shows a valid object of an unrelated class (`java.util.BitSet cannot
        // be cast to org.h2.mvstore.Chunk`) — is folded into that function, so
        // the dispatch face inherits it too. It is passed `actual_class_id`
        // and takes the heap locks only for the `ClassId(0)` branch.
        //
        // Round 9 wave 8 (vmrt8): gated. Its ring scans walk ~34 MB and were
        // the whole of a ~5 ms `ClassCastException` (HotSpot: 3 us); see
        // `cast_refusal_forensics_admitted` for the measurement and for why a
        // zero header and the first sighting of each (class, target) pair
        // still get the full report.
        if crate::runtime::exceptions::cast_refusal_forensics_admitted(
            obj_class_id.as_u32(),
            class_name,
        ) {
            crate::memory::reclaim_guard::report_reclaimed_receiver(
                vm,
                obj_ref.as_ptr() as usize,
                "JIT checkcast",
                class_name,
                obj_class_id.as_u32(),
            );
        }
        if let Some((thread, _jit_thread_guard)) = jit_thread_mut() {
            // r9w9 exc9: `OmitStackTraceInFastThrow` (opt-in, default off) —
            // a hot cast site throws a stackless CCE and skips the message.
            // The cast helper is called FROM the compiled body, whose frames
            // are all live: the site comes from a walk (only while fast throw
            // is on).
            let site = crate::runtime::exceptions::fast_throw_site_of_live_frames(vm);
            if let Some(exc) = crate::runtime::exceptions::try_fast_throw(
                vm,
                thread,
                crate::runtime::exceptions::FastThrowKind::ClassCast,
                site.as_ref(),
            ) {
                set_jit_pending_exception(thread, exc);
                return i64::MIN;
            }
            // Render an array receiver by its own descriptor. The header of a
            // reference array carries the COMPONENT class id, so the plain
            // `get_class(obj_class_id).name` lookup reports `java.lang.String`
            // for a `String[]` -- the self-cast text
            // `java.lang.String cannot be cast to java.lang.String` that cost
            // a session of misdiagnosis on TestObjectDataType. HotSpot prints
            // `[Ljava.lang.String;`.
            //
            // JIT round 21 wave 25, lane hexname (item 5 of
            // `r21-review-w23-findings-FIXED-20261020.md`,
            // `CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`): an array receiver by its
            // BOTTOM class's hidden flag (the header's component id walked
            // down `array_info`), as the interpreter's `checkcast` renders it:
            // `[Lp/0x2A;` of an ordinary class is `[Lp.0x2A;`, of a hidden one
            // `[Lp/0x2A;` (the bare `replace` dotted a hidden tail too, and
            // stays the fallback and the `=0` text).
            let obj_cls_name = match crate::runtime::interpreter::array_descriptor_of(vm, obj_ref) {
                Some(desc) => crate::runtime::exceptions::external_name_by_bottom_class_or(
                    &desc,
                    crate::runtime::exceptions::bottom_class_hidden(vm, obj_class_id, &desc),
                    |d| d.replace('/', "."),
                ),
                None => {
                    let stored = vm
                        .classes
                        .class_manager
                        .read()
                        .get_class(obj_class_id)
                        // A hidden class keeps its `/0x<hex>` tail, as the
                        // interpreter's `checkcast` (interpreter round i5
                        // wave 104, lane L7).
                        .map(|c| {
                            crate::runtime::exceptions::external_name_keeping_hidden_tail(
                                &c.name, c.hidden,
                            )
                        });
                    // A lambda proxy has no class in the store: named as the
                    // interpreter's `checkcast` names it (`Host$$Lambda/0x...`),
                    // where this said `class <unknown>` (interpreter round i4
                    // wave 98, lane L9; `L9W98VmErrorTexts` row `cast-lambda`).
                    stored
                        .or_else(|| {
                            crate::runtime::interpreter::lambda_proxy_display_name(vm, obj_class_id)
                        })
                        .unwrap_or_else(|| "<unknown>".into())
                }
            };
            // Wave 25, lane hexname (`CRATONVM_HIDDEN_NAME_TEXT_BY_CLASS`):
            // the target by its resolved class's BOTTOM hidden flag, so a
            // hidden target (`(Hid) o` in a hidden class's own body) keeps its
            // `/0x<hex>` tail as `Klass::external_name()` does; an unresolved
            // or mismatched id keeps the bare `replace` (the `=0` text).
            //
            // Wave 26, lane hexname (`CRATONVM_CAST_TEXT_BY_RESOLVED_CLASS`):
            // a hidden class's SELF cast resolves the pool's `Hd` to the
            // hidden class, whose name (`Hd/0x..`) is what HotSpot prints;
            // the name match above cannot see it (the stored name is not the
            // site's), so the resolved class is asked first.
            let site_target = cratonvm_jit::typecheck_target_for_site(class_name_ptr)
                .map(cratonvm_types::ClassId::new);
            let target_display = crate::runtime::exceptions::cast_target_display(
                vm,
                site_target,
                class_name,
                |class_name| {
                    crate::runtime::exceptions::external_name_by_bottom_class_or(
                        class_name,
                        site_target.and_then(|target| {
                            crate::runtime::exceptions::bottom_class_hidden(vm, target, class_name)
                        }),
                        |name| name.replace('/', "."),
                    )
                },
            );
            let msg = format!(
                "class {} cannot be cast to class {}",
                obj_cls_name, target_display
            );
            // W7-37 -- and this one is a correction to that record, which
            // listed "the JIT cast helper's `class ... cannot be cast to
            // class ...`" among the paths its funnel rewrite newly covers. It
            // did not: this site builds the throwable itself and so never
            // reaches `throw_runtime_error`, which is where the message gains
            // HotSpot's module/loader parenthetical
            // (`(java.lang.String and java.lang.Integer are in module
            // java.base of loader 'bootstrap')`). The interpreter's `checkcast`
            // raises a `RuntimeError` and does get it, so the two execution
            // modes printed different text for the same refusal.
            //
            // `msg` is already in the two-operand shape `split_cast_operands`
            // requires, and the rewrite fails open: when either operand's
            // module/loader cannot be named -- a user-defined loader, most
            // of all -- the funnel returns `msg` byte-identical.
            //
            // Round 15 wave 7 (lane throw2, TH1-2): and it names them from
            // the two class ids noted here (the receiver's and the site's
            // resolved target), as the interpreter's `checkcast` notes them,
            // not by NAME -- one class-manager read each instead of a
            // definition-index probe, the right class when two loaders define
            // the name, and the key of the funnel's per-thread message memo.
            // The funnel checks each id's name against its operand, so an
            // array receiver (whose header holds the component's id) falls
            // back to the name. `CRATONVM_CCE_MESSAGE_BY_ID=0` notes nothing.
            if crate::runtime::exceptions::cast_message_by_id_enabled() {
                if let Some(target) = cratonvm_jit::typecheck_target_for_site(class_name_ptr) {
                    crate::runtime::exceptions::note_cast_operands(
                        thread,
                        obj_class_id,
                        cratonvm_types::ClassId::new(target),
                    );
                }
            }
            use crate::error::MethodCallFailed;
            if let MethodCallFailed::ExceptionThrown(exc) =
                crate::runtime::exceptions::throw_runtime_error(
                    vm,
                    thread,
                    crate::error::RuntimeError::ClassCastException { message: msg },
                )
            {
                set_jit_pending_exception(thread, exc);
                return i64::MIN;
            }
        }
        0
    }
}

// SAFETY: Called from JIT-compiled code. vm_ptr must be a valid SharedVm pointer.
// obj_ptr is 0 (null) or a valid heap pointer. class_name_ptr/class_name_len form a
// valid UTF-8 slice pointing into the JIT string table (or are null/<=0 for unresolved,
// which returns 0). Returns 1 if obj is an instance, 0 otherwise.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub unsafe extern "C" fn jit_instanceof(
    vm_ptr: i64,
    obj_ptr: i64,
    class_name_ptr: *const u8,
    class_name_len: i64,
) -> i64 {
    let f = || jit_instanceof_body(vm_ptr, obj_ptr, class_name_ptr, class_name_len);
    contain("jit_instanceof", OnPanic::Throw { vm_ptr }, 0, f)
}

/// Body of [`jit_instanceof`], run under its panic guard.
unsafe fn jit_instanceof_body(
    vm_ptr: i64,
    obj_ptr: i64,
    class_name_ptr: *const u8,
    class_name_len: i64,
) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Null reference is never an instance of anything (JVMS §6.5.instanceof).
    // Treat a null OR implausible (stale/garbage, unaligned/>47-bit) receiver
    // identically: take the existing null path instead of dereferencing it (the
    // field/class/header read below would SIGSEGV). `plausible_heap_pointer(0)`
    // is already false, so this also covers the original null check. Valid
    // objects always pass (8-aligned, ≤47-bit); zero false positives. This is a
    // cheap, vm-independent pre-filter — kept ahead of the `vm_ptr` dereference
    // below so it alone guards the existing unit tests, which pass `vm_ptr = 0`.
    if !cratonvm_types::plausible_heap_pointer(obj_ptr as u64) {
        return 0;
    }
    if class_name_len <= 0 || class_name_ptr.is_null() {
        jit_typecheck_null_name_trace("instanceof");
        return 0;
    }
    // SAFETY: vm_ptr originates from JIT code that received it from the interpreter's SharedVm reference.
    let vm = &*(vm_ptr as *const SharedVm);
    // Defense-in-depth beyond the bit-pattern-only `plausible_heap_pointer`
    // check above: that check has no view of the heap's actual mapped
    // extent, only whether the bits LOOK like a pointer (aligned,
    // in-range). A stale `ObjectRef` into memory the heap has since
    // reclaimed/reused past a GC root-coverage gap still passes it, and
    // the unchecked `ObjectRef::from_raw` + `class_id_of` this used to do
    // next would then read through a dangling pointer — observed live as
    // a SIGSEGV inside this function under concurrent executor load
    // (WildFly `EEConcurrencyExecutorShutdownTestCase`, see
    // wildfly-domain-heap-corrupt-value-timeout-RESOLVED.md).
    // `is_object_address` additionally validates the address falls inside
    // a live heap region (and looks like a real header) before ever
    // dereferencing it, degrading a dangling reference to "not an
    // instance" instead of crashing — the same fallback every other stale-
    // reference guard in this codebase uses.
    crate::jit::helpers::note_membership_walk(5);
    let mut obj_ref = match vm.mem.heap.is_object_address(obj_ptr as usize) {
        Some(r) => r,
        None => return 0,
    };
    let obj_class_id = vm.mem.heap.class_id_of(obj_ref);
    // j27 wave 3 (lane CLONE): the census reason, before the publish below.
    let census = jit_typecheck_census_reason(vm, obj_ref, obj_class_id);
    // JIT r20 wave 17 (lane pAlloc2): a plain receiver whose class has no
    // supers display yet gets one, so the optimizing tier's display arm
    // (`CRATONVM_JIT_TYPECHECK_DISPLAY_MISS`) answers its next hit or miss
    // inline. Lock-free once published; no lock is held here.
    if vm.mem.heap.kind_of(obj_ref) != cratonvm_types::ObjectKind::Array {
        vh_const_ensure_display(vm, obj_class_id.as_u32());
    }
    // instanceof: strict (SBR-03). `obj_ref` is passed `&mut` — see the
    // GC-SAFETY comment in `jit_checkcast` — so any GC triggered by
    // resolving a not-yet-loaded target class inside `jit_typecheck_resolve`
    // doesn't leave the fallback checks in that function reading through a
    // stale pointer. `instanceof` returns a bool, not a pointer, so there is
    // no analogous stale-return-value fix needed here.
    //
    // `None` (class name not valid UTF-8) keeps the old fail-soft `0`.
    //
    // A class-access refusal or a resolution failure at an unbound site
    // (i11-L2) is raised as `checkcast` raises it: the Java throwable pending
    // and the `i64::MIN` sentinel returned, which every lowering of this
    // helper drains — the x64 single-pass `0xc1` arm's post-call exception
    // check and aarch64's `emit_bail_on_sentinel`. The optimizing tier never
    // sees one: it admits an `instanceof` only with a compile-time-resolved
    // target, and only an unbound site can raise (see
    // `jit_typecheck_holder_site_target`).
    let mut raise = None;
    let answer = jit_typecheck_memoized(
        vm,
        obj_class_id,
        &mut obj_ref,
        class_name_ptr,
        class_name_len as usize,
        false,
        &mut raise,
    );
    if let Some(reason) = census {
        let shown = jit_typecheck_census_answer(answer, raise.is_some());
        let name = (class_name_ptr, class_name_len as usize);
        jit_typecheck_census_line(vm, "instanceof", name, obj_class_id, reason, shown);
    }
    if let Some(raise) = raise {
        return jit_typecheck_raise_now(vm, raise);
    }
    match answer {
        Some(true) => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod i11_l2_unbound_typecheck_raises {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::{JvmThread, ThreadId};
    use std::sync::Arc;

    /// i11-L2: a type-check site the compiler could not bind (interned under
    /// its holder) whose class cannot be resolved raises from BOTH helpers —
    /// the resolution error pending and the `i64::MIN` sentinel — for a plain
    /// and an ARRAY receiver alike, as the interpreter's opcodes throw at the
    /// same site. It used to answer `false` (a silent `0`, or a
    /// `ClassCastException` naming the wrong cause).
    #[test]
    fn an_unbound_site_whose_class_is_missing_raises_from_both_helpers() {
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let holder = {
            let mut cm = shared.classes.class_manager.write();
            match cm.try_ensure_synthetic_class("cratonvm/test/i11l2/JitCastHolder", 0) {
                Ok(id) => id,
                Err(_) => return, // --jdk-only policy in the environment
            }
        };
        let vm_ptr = Arc::as_ptr(&shared) as i64;
        let mut thread = Box::new(JvmThread::new(ThreadId(11), "i11l2-jit-typecheck"));
        let plain = shared.mem.heap.alloc_object(holder, 0);
        let ints = shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Int, 1);
        for (cp_index, name) in [
            (1u16, "cratonvm/test/i11l2/JitMissing"),
            (2u16, "[Lcratonvm/test/i11l2/JitMissing;"),
        ] {
            let (ptr, len) =
                cratonvm_jit::intern_typecheck_unresolved_in(name, holder.as_u32(), cp_index);
            for (receiver, shape) in [(plain, "object"), (ints, "int[]")] {
                for checkcast in [true, false] {
                    thread.jit_pending_exception = None;
                    let scope = set_jit_thread(&mut thread);
                    // SAFETY: `vm_ptr` names the live VM above, `receiver` is a
                    // live heap object of it, the site pointer is an interned
                    // name, and the JIT thread pointer is installed for this
                    // OS thread — the contract compiled code satisfies.
                    let got = unsafe {
                        if checkcast {
                            jit_checkcast(vm_ptr, receiver.as_ptr() as i64, ptr, len as i64)
                        } else {
                            jit_instanceof(vm_ptr, receiver.as_ptr() as i64, ptr, len as i64)
                        }
                    };
                    restore_jit_thread(scope);
                    let pending = thread.jit_pending_exception.take();
                    let label = format!(
                        "{} of a {shape} against {name}",
                        if checkcast { "checkcast" } else { "instanceof" }
                    );
                    match pending {
                        Some(exc) => {
                            assert_eq!(got, i64::MIN, "{label}: raised, so the sentinel");
                            let cid = shared.mem.heap.class_id_of(exc);
                            let exc_name = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(cid)
                                .map(|c| c.name.to_string());
                            assert_eq!(
                                exc_name.as_deref(),
                                Some("java/lang/NoClassDefFoundError"),
                                "{label}"
                            );
                        }
                        // The stripped unit VM could not build the throwable:
                        // the helper must then fail soft, never hand out the
                        // sentinel with nothing pending.
                        None => assert_ne!(got, i64::MIN, "{label}: sentinel with nothing pending"),
                    }
                }
            }
        }
    }

    /// i11-L2 (wave 12): JVMS §5.4.3 at a compiled unbound type-check site.
    /// The first execution's resolution failure is recorded against the
    /// site's `(holder, cp index)`; once the class has become resolvable, the
    /// second execution must still raise the RECORDED error rather than
    /// resolve (or ask a loader) again. Before wave 12 the site carried no cp
    /// index, recorded nothing, and answered `0` the second time.
    #[test]
    fn a_recorded_failure_is_rethrown_at_an_unbound_site_after_the_class_appears() {
        const LATE: &str = "cratonvm/test/i12l2/JitLateClass";
        const CP_INDEX: u16 = 3;
        let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
        let holder = {
            let mut cm = shared.classes.class_manager.write();
            match cm.try_ensure_synthetic_class("cratonvm/test/i12l2/JitRecordHolder", 0) {
                Ok(id) => id,
                Err(_) => return, // --jdk-only policy in the environment
            }
        };
        let vm_ptr = Arc::as_ptr(&shared) as i64;
        let mut thread = Box::new(JvmThread::new(ThreadId(12), "i12l2-jit-typecheck"));
        let receiver = shared.mem.heap.alloc_object(holder, 0);
        let (ptr, len) =
            cratonvm_jit::intern_typecheck_unresolved_in(LATE, holder.as_u32(), CP_INDEX);
        let run = |thread: &mut Box<JvmThread>| {
            thread.jit_pending_exception = None;
            let scope = set_jit_thread(thread);
            // SAFETY: `vm_ptr` names the live VM above, `receiver` is a live
            // heap object of it, the site pointer is an interned name, and the
            // JIT thread pointer is installed for this OS thread.
            let got = unsafe { jit_instanceof(vm_ptr, receiver.as_ptr() as i64, ptr, len as i64) };
            restore_jit_thread(scope);
            (got, thread.jit_pending_exception.take())
        };

        let (first, first_exc) = run(&mut thread);
        let resolver = crate::runtime::resolve::MemberResolver::new(&shared);
        let recorded = resolver.probe_resolution_failure(resolver.scope(holder), CP_INDEX);
        let (Some(_), Some(record)) = (first_exc, recorded) else {
            // The stripped unit VM could not build (or classify) the
            // `LinkageError`: nothing was recorded, so there is nothing to
            // rethrow.
            return;
        };
        assert_eq!(first, i64::MIN, "the first failure is raised");
        assert_eq!(&*record.error_class, "java/lang/NoClassDefFoundError");

        // The class becomes resolvable through the holder's loader.
        let appeared = {
            let mut cm = shared.classes.class_manager.write();
            cm.try_ensure_synthetic_class(LATE, 0).is_ok()
        };
        let resolvable = appeared && {
            let cm = shared.classes.class_manager.read();
            crate::runtime::interpreter::class_resolved_without_loading(&shared, &cm, holder, LATE)
                .is_some()
        };
        if !resolvable {
            return;
        }

        let (second, second_exc) = run(&mut thread);
        assert_eq!(
            second,
            i64::MIN,
            "the recorded failure is rethrown, not answered by a fresh resolution"
        );
        let Some(exc) = second_exc else {
            return; // no throwable could be built this time: fail-soft is fine
        };
        let exc_name = shared
            .classes
            .class_manager
            .read()
            .get_class(shared.mem.heap.class_id_of(exc))
            .map(|c| c.name.to_string());
        assert_eq!(exc_name.as_deref(), Some("java/lang/NoClassDefFoundError"));
    }
}

// ---------------------------------------------------------------------------
// Bounds check helper
// ---------------------------------------------------------------------------

/// JIT bounds-check helper — sets a pending AIOOBE flag and returns `i64::MIN`
/// (the deopt sentinel) to signal the interpreter that a bounds check failed.
///
/// On Windows, JIT frames have no SEH unwind tables, so panicking here would
/// terminate the process instead of unwinding to the `catch_unwind` in the
/// interpreter.  Using a thread-local flag sidesteps this platform limitation.
// SAFETY: Called from JIT-compiled code when an array bounds check fails.
// Only stores two i64 values in a thread-local. The raw pointer is read solely
// by the explicitly-enabled diagnostic block below.
pub unsafe extern "C" fn jit_throw_aioobe(
    index: i64,
    length: i64,
    array_ptr: i64,
    bytecode_pc: i64,
) -> i64 {
    contain("jit_throw_aioobe", OnPanic::Deopt, i64::MIN, || {
        jit_throw_aioobe_body(index, length, array_ptr, bytecode_pc)
    })
}

/// Body of [`jit_throw_aioobe`]. Panic-guarded via [`contain`] with [`OnPanic::Deopt`].
unsafe fn jit_throw_aioobe_body(index: i64, length: i64, array_ptr: i64, bytecode_pc: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // nio21 wave 17 L5: a single-pass pad whose local-handler probe follows
    // sets the defer bit in `bytecode_pc`. Stripped before anything reads it.
    // Interpreter round i5 wave 102, lane L2: and the method token above it
    // (`split_throw_stamp_word`), first.
    let (bytecode_pc, stamp_owner) = split_throw_stamp_word(bytecode_pc);
    let (bytecode_pc, defer_snapshot) =
        cratonvm_jit::implicit_null::split_trap_snapshot_defer_bit(bytecode_pc);
    // TEMP DIAGNOSTIC (BigInteger.smallToString AIOOBE investigation,
    // 2026-07-17, `CRATONVM_DBG_AIOOBE3`): dump the raw ObjectHeader at the
    // pointer the JIT's bounds check actually compared against, to
    // distinguish "array genuinely under-allocated" (header's own
    // `array_length` field matches the reported `length`, and the mark word is
    // not FORWARDED) from "stale/forwarded pointer" (a FORWARDED mark word, or
    // a header that doesn't look like a live long[] at all — the classic
    // unrooted-local-across-allocating-call signature this codebase has hit
    // repeatedly elsewhere). Best-effort raw read: the pointer came from a
    // live JIT register moments ago, so even if stale it should still point
    // at mapped (recycled, not unmapped) heap memory.
    //
    // Decoded through the 8-byte-header accessors (round 11 wave 16, lane
    // tier; page `r11w15-hdr-helpers-header-diagnostics-patch`, item 2): the
    // kind and element-type tags live in the mark word, and an array is a
    // long-header object whose hash word is `IDENTITY_HASH_OFFSET` (offset 8
    // is its LENGTH). The old raw reads at 4/5/8 labelled the mark word's low
    // bytes and the length as kind, element type and hash.
    if aioobe3_dbg() {
        if array_ptr != 0 {
            let base = array_ptr as usize as *const u8;
            let class_id = std::ptr::read_unaligned(base as *const u32);
            let kind = cratonvm_types::kind_tag_at(base);
            let elem_ty = cratonvm_types::element_type_tag_at(base);
            let ident_hash = std::ptr::read_unaligned(
                base.add(cratonvm_types::IDENTITY_HASH_OFFSET) as *const i32,
            );
            let arr_len_hdr = std::ptr::read_unaligned(
                base.add(cratonvm_types::ARRAY_LENGTH_OFFSET) as *const u32,
            );
            // The same word as `arr_len_hdr` for an array; a legacy
            // instance's field count if the pointer is not one.
            let num_slots =
                std::ptr::read_unaligned(base.add(cratonvm_types::NUM_SLOTS_OFFSET) as *const u32);
            let gc_age = cratonvm_types::ObjectHeader::gc_age_of(std::ptr::read_unaligned(
                base.add(cratonvm_types::MARK_WORD_OFFSET) as *const u32,
            ));
            let gc_flags = std::ptr::read_unaligned(
                base.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) as *const u8
            );
            // Forwarding folded into the mark word (32 -> 24 shrink), so this
            // diagnostic reports the whole word: its 2-bit state distinguishes
            // NEUTRAL / THIN_LOCKED / INFLATED / FORWARDED, which is strictly
            // more than the old field could say.
            let mark_word =
                std::ptr::read_unaligned(base.add(cratonvm_types::MARK_WORD_OFFSET) as *const u32);
            eprintln!(
                "[AIOOBE3-DIAG] bci={bytecode_pc} jit-reported index={index} length={length} array_ptr={array_ptr:#x} \
header: class_id={class_id} kind={kind} elem_ty={elem_ty} ident_hash={ident_hash} \
array_length_field={arr_len_hdr} num_slots={num_slots} gc_age={gc_age} gc_flags={gc_flags} \
mark_word={mark_word:#x}"
            );
        } else {
            eprintln!(
                "[AIOOBE3-DIAG] bci={bytecode_pc} jit-reported index={index} length={length} array_ptr=NULL"
            );
        }
    }
    JIT_SIGNALS.with(|s| s.aioobe.set(Some((index, length))));
    // This direct-throw helper receives the trapping instruction's bci from
    // its per-site cold pad. Replace any stale callee stamp before the drain
    // range-checks this method's exception table.
    JIT_SIGNALS.with(|s| {
        s.athrow_bci.set(bytecode_pc);
        s.athrow_owner.set(stamp_owner);
    });
    // The frames this bounds check fired in are about to leave: the helper
    // returns and the compiled body runs its epilogue, so the
    // `ArrayIndexOutOfBoundsException` is built later, from the interpreter,
    // on a stack that no longer has them. Snapshot here, the one moment the
    // signal and the frames both exist -- the same rung the NPE and
    // div-by-zero setters stand on. The frame walk does NOT have this bci (a
    // bounds check publishes no safepoint id; the frame that raised read `-1`
    // or its last call's line), so the pad's own `bytecode_pc` places it.
    // nio21 wave 17 L5: unless the pad's probe follows, which takes it while
    // the frames are still live (or not at all, for a fast throw).
    if defer_snapshot {
        defer_trap_snapshot(DeferredTrapSnapshot::AtBci(bytecode_pc));
    } else {
        snapshot_trap_frames_at_bci(bytecode_pc);
    }
    // Out-of-band deopt signal: this `i64::MIN` IS the bounds-check stub's
    // method return value, so flag it as a genuine deopt so the interpreter
    // doesn't mistake a method legitimately returning `Long.MIN_VALUE` for one.
    set_jit_deopt_pending();
    i64::MIN // deopt sentinel — interpreter will detect and throw AIOOBE
}

/// Cached `CRATONVM_DBG_AIOOBE3` gate: temp diagnostic companion to
/// [`jit_throw_aioobe`] for the BigInteger.smallToString AIOOBE
/// investigation (2026-07-17) — see that function's doc comment.
#[inline]
fn aioobe3_dbg() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_AIOOBE3"))
}

/// RBC.6 — trace exception routing through `route_implicit_exc_through_callee`
/// / `route_jit_exception_through_method` (which entry each takes, resolved
/// handler pc). Cached read-once like the other `*_dbg()` gates in this file.
#[inline]
pub(crate) fn rbc6_dbg() -> bool {
    static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_RBC6"))
}

/// Direct-throw for `ArithmeticException` ("/ by zero") — the div-by-zero
/// sibling of [`jit_throw_aioobe`]. The x64 `idiv`/`irem`/`ldiv`/`lrem`
/// zero-divisor guard jumps to a stub that calls this and immediately runs the
/// method epilogue. We set the pending-arithmetic flag and the out-of-band deopt
/// signal, then return the `i64::MIN` sentinel; the interpreter's JIT-return
/// drain throws a real `ArithmeticException` through the method's exception table
/// WITHOUT re-running the method from entry. Re-running (the previous
/// `uncommon_trap` path) double-executed any side effect that preceded the trap,
/// diverging from HotSpot.
///
/// Same Windows platform rationale as [`jit_throw_aioobe`]: JIT frames have no
/// SEH unwind tables, so a Rust panic/unwind here would terminate the process;
/// thread-local stashing sidesteps that.
// SAFETY: Called from JIT-compiled code at a div-by-zero guard. Sets two
// thread-locals and returns a sentinel; no pointer dereferences.
pub unsafe extern "C" fn jit_throw_arithmetic() -> i64 {
    contain("jit_throw_arithmetic", OnPanic::Deopt, i64::MIN, || {
        jit_throw_arithmetic_body()
    })
}

/// Body of [`jit_throw_arithmetic`]. Panic-guarded via [`contain`] with [`OnPanic::Deopt`].
unsafe fn jit_throw_arithmetic_body() -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    JIT_SIGNALS.with(|s| s.arithmetic.set(true));
    // Same reason as the NPE setters, and it was missing here: the
    // `ArithmeticException` is not constructed at the trap. The stub flags the
    // signal, returns the deopt sentinel and runs the method EPILOGUE, so the
    // `fillInStackTrace` that eventually runs walks a stack the compiled frames
    // have already left — an empty trace for a throwable that came from four
    // frames of compiled code. This is the only moment both the signal and the
    // frames exist, so it is the only place the snapshot can be taken.
    //
    // The frame walk does NOT have this bci, whatever this comment used to
    // say: a zero-divisor guard publishes no safepoint id either, so the frame
    // that raised read the prologue's unset sentinel (`-1` -- every JIT arm of
    // `R11RtFibTraceLines` printed `lines -1 27 25 ...`) or the id of its last
    // call (a wrong line). Every x64 stub that calls this stamps the division's
    // own interpreter bci through `jit_set_throw_bci` immediately before the
    // call, so the value read here is that stamp; `-1` (nothing stamped)
    // leaves the walk's answer alone.
    //
    // nio21 wave 17 L5: a stamp with the defer bit comes from a pad whose
    // local-handler probe follows; the bit is stripped from the stamp here and
    // the snapshot is left to that probe.
    let (stamped_bci, defer_snapshot) = cratonvm_jit::implicit_null::split_trap_snapshot_defer_bit(
        JIT_SIGNALS.with(|s| s.athrow_bci.get()),
    );
    if defer_snapshot {
        JIT_SIGNALS.with(|s| s.athrow_bci.set(stamped_bci));
        defer_trap_snapshot(DeferredTrapSnapshot::AtBci(stamped_bci));
    } else {
        snapshot_trap_frames_at_bci(stamped_bci);
    }
    set_jit_deopt_pending();
    i64::MIN // deopt sentinel — interpreter will detect and throw ArithmeticException
}

/// RBC.6 (athrow codegen) — stash the thrown exception object as the
/// pending JIT exception and return the `i64::MIN` deopt sentinel. The
/// x64 `athrow` arm calls this and immediately runs the method epilogue;
/// the interpreter's JIT-return drains (`take_jit_pending_exception` on
/// every dispatch-aware return path) route the exception through the
/// caller's handling. `exc_ptr == 0` (athrow on a null reference) sets
/// the pending-NPE flag instead, per JVMS athrow semantics.
///
/// `bci` is the bytecode pc of this `athrow` instruction — a compile-time
/// immediate the x64 codegen bakes into the call site (RBC.6 correctness
/// fix, see `JitSignals::athrow_bci`). Stashed alongside the exception so
/// `execute_jit_call` can give `route_jit_exception_through_method` a real
/// `throw_pc` instead of `usize::MAX`, which — for a method with 2+
/// exception-table entries whose catch types are in a subtype relationship —
/// can match the WRONG entry (declaration-order, type-only) regardless of
/// which try-region actually threw. `exc_ptr == 0` still routes to the NPE
/// flag (no bci needed there — `jit_pending_npe` has no such ambiguity path
/// yet).
///
/// Same platform rationale as [`jit_throw_aioobe`]: JIT frames have no
/// SEH unwind tables on Windows, so a Rust panic/unwind here would
/// terminate the process; thread-local stashing sidesteps that.
// SAFETY: Called from JIT-compiled code at an athrow site. `exc_ptr` is
// either 0 or the heap pointer the JIT popped from the operand stack;
// no dereference happens here — it is only wrapped and stored in a TLS.
// `bci` is a bare immediate (no pointer semantics).
/// Stamp the *currently executing compiled method's own* throw-site bci onto
/// the pending-exception signal.
///
/// Called from the cold side of `emit_post_invoke_exception_check`, i.e. when
/// a dispatched callee has thrown and this method is about to return the
/// `i64::MIN` sentinel. Without it, `JitSignals::athrow_bci` still holds the
/// bci that the CALLEE's own compiled `athrow` lowering stashed — a pc in a
/// different method entirely. `execute_jit_call` then hands that foreign pc to
/// `route_jit_exception_through_method` as this method's throw site, and the
/// `[start_pc, end_pc)` range check rejects the method's own handler.
///
/// The observable symptom is a silently skipped `finally`: a catch-all entry
/// is the one handler shape that cannot be rescued by the type check, so it is
/// dropped whenever the callee's bci happens to fall outside the protected
/// region (and spuriously honoured when it happens to fall inside). Witness:
/// `docs/known-issues/repros/jitban-remaining-20260726/FinallyBalanceProbe.java`
/// — `try { n++; thrower(); } finally { n--; }` leaked one count per throw
/// under JIT and zero under `--nojit` / HotSpot.
///
/// Setting the bci unconditionally is safe: the sentinel is also returned for
/// plain deopts with no exception pending, and `take_all_jit_signals` resets
/// the field to `-1` on every drain, while `set_jit_pending_exception` resets
/// it whenever a *new* exception is stashed.
#[no_mangle]
pub unsafe extern "C" fn jit_set_throw_bci(bci: i64) {
    // Interpreter round i5 wave 102, lane L2: the exit packs its method's
    // stamp token beside the stamp (`CRATONVM_JIT_THROW_STAMP_OWNER`). A bare
    // bci has none.
    let (bci, owner) = split_throw_stamp_word(bci);
    JIT_SIGNALS.with(|s| {
        s.athrow_bci.set(bci);
        s.athrow_owner.set(owner);
        // Interpreter round i5 wave 106, lane L2: a compiled exit stamping
        // its own throw site now owns the throwable; an escape record written
        // before it (`note_dispatch_error_escape`, `note_redispatch_escape`)
        // spoke for a callee that has returned, or for an inner activation of
        // a self-recursive method, and must not let this activation's caller
        // skip ITS table. Every reader already refused a record beside a stamp
        // at the doors; the call-site services did not ask.
        s.redispatch_escape.set(0);
    });
}

pub unsafe extern "C" fn jit_throw_exception(exc_ptr: i64, bci: i64) -> i64 {
    contain("jit_throw_exception", OnPanic::Deopt, i64::MIN, || {
        jit_throw_exception_body(exc_ptr, bci)
    })
}

/// Body of [`jit_throw_exception`]. Panic-guarded via [`contain`] with [`OnPanic::Deopt`].
unsafe fn jit_throw_exception_body(exc_ptr: i64, bci: i64) -> i64 {
    // WS1: Rust<->JIT boundary — invalidate the per-thread JIT-scan cache
    // (see conservative_roots::note_jit_boundary).
    crate::jit::conservative_roots::note_jit_boundary();
    // Interpreter round i5 wave 102, lane L2: the stamp word may carry the
    // method's stamp token (`split_throw_stamp_word`).
    let (bci, owner) = split_throw_stamp_word(bci);
    if exc_ptr == 0 {
        // JIT round 20 wave 7 (lane fuzzC): the `athrow`'s own bci and action,
        // so the NPE carries HotSpot's JEP 358 message and line.
        set_jit_pending_npe_athrow(bci);
    } else if let Some((thread, _guard)) = jit_thread_mut() {
        // The throwable is stashed on the `JvmThread` so the collector can
        // both keep it alive and relocate it before the interpreter's drain
        // reads it back (`jit-signals-root-gap.md`).
        let exc = ObjectRef::from_raw(exc_ptr as usize as *mut u8);
        set_jit_pending_exception_with_stamp(thread, exc, bci, owner);
        // Interpreter round i4 wave 92 (lane L1): while some VM listens for
        // JVMTI `Exception`, leave this throw's site for the report (one
        // union load otherwise; `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED`).
        // Wave 93 (lane L1): or some VM serves a JDWP `Exception` request
        // from compiled code (one more load).
        if crate::runtime::interpreter::compiled_throw_sites_wanted() {
            crate::runtime::interpreter::note_compiled_throw_site(exc, bci);
        }
    } else {
        // Unreachable in a compiled method: `emitted_athrow` forces
        // `has_dispatch` (`jit/src/x64.rs`), and `has_dispatch` is exactly what
        // makes `execute_jit_call` install `JIT_THREAD` before entering. If
        // that ever stops holding, the throwable has nowhere GC-visible to go
        // and the fast entry path would not drain it either — so say so loudly
        // in debug rather than silently dropping an exception.
        debug_assert!(
            false,
            "jit_throw_exception with no JIT thread installed: `emitted_athrow` \
             must force has_dispatch (jit/src/x64.rs)"
        );
        // Still record the bci, and let the sentinel propagate exactly as
        // before; the interpreter treats a sentinel with no signal as a plain
        // deopt.
        JIT_SIGNALS.with(|s| s.athrow_bci.set(bci));
    }
    i64::MIN // deopt sentinel — interpreter drains the pending exception
}

/// Which of THIS compiled method's own `catch` blocks takes the pending
/// throwable — the runtime half of the compiled-local-handler feature.
///
/// Called from a local-handler stub the moment a fallible site returned the
/// `i64::MIN` sentinel, with `site_ptr` naming the throwing bci's candidate
/// handlers (already filtered at compile time to the exception-table entries
/// whose `[start_pc, end_pc)` covers it, in table order).
///
/// Returns the index of the first matching candidate, having *taken* the
/// pending exception and stored its raw address into `*out_exc` — the frame
/// slot the handler's operand stack starts at, so the stub only has to jump.
/// Returns `-1` for "this frame does not catch it", leaving every signal
/// exactly as it found them, so the stub falls through to the reason-9 /
/// shared-sentinel edge that ran before this feature existed.
///
/// # What it deliberately refuses
///
/// Only an already-constructed throwable stashed in `jit_pending_exception` can
/// be entered locally. A pending NPE / AIOOBE / `ArithmeticException` signal is
/// a *request* to build a throwable that the interpreter's drain fulfils, and a
/// bare deopt (or a stashed IR-deopt frame) is not an exception at all. Each of
/// those answers `-1` and takes the old route unchanged — which is what makes
/// this addition unable to change any program's observable behaviour except by
/// being faster.
///
/// # Matching
///
/// The rule is `find_jit_exception_handler`'s, restricted to one bci: table
/// order, catch-all (`catch_type == 0`, an empty name here) matches anything, a
/// typed entry matches when the throwable's class is assignable to the catch
/// class resolved through the compiled method's own declaring class. The
/// class-manager work sits behind a monomorphic cache on the site, because a
/// throwing loop throws the same class every time and the read lock plus the
/// name resolution is most of the cost otherwise.
///
/// SAFETY: `site_ptr` points to a `JitLocalHandlerSite` owned by the running
/// `CompiledMethod` (kept alive by `_jit_local_handler_sites` for as long as
/// its machine code is reachable); `out_exc` points into the live JIT frame.
/// Both are produced by the emitter and never by Java data.
pub unsafe extern "C" fn jit_local_handler_lookup(
    vm_ptr: i64,
    site_ptr: i64,
    out_exc: *mut i64,
) -> i64 {
    // nio21 wave 17 L5: the snapshot the trap's setter left to this probe
    // (`DeferredTrapSnapshot`). The implicit arm consumes it on a hit (taking
    // it, or not for a fast throw); every other answer is the propagate path,
    // which gets the snapshot the setter would have taken, taken here while
    // the pad that called us still holds the same frames.
    let deferred = std::cell::Cell::new(take_deferred_trap_snapshot());
    // JIT round 20 (lane throw): the deferred snapshot is walked below with
    // this VM's bound (`trap_snapshot_walk`).
    if vm_ptr != 0 && deferred.get() != DeferredTrapSnapshot::None {
        // SAFETY: `vm_ptr` is the hidden context argument the compiled method
        // was entered with, i.e. the `SharedVm` the interpreter handed it.
        note_trap_snapshot_bound(&*(vm_ptr as *const SharedVm));
    }
    let f = || jit_local_handler_lookup_body(vm_ptr, site_ptr, out_exc, &deferred);
    let answer = contain("jit_local_handler_lookup", OnPanic::Throw { vm_ptr }, -1, f);
    if answer < 0 {
        take_deferred_trap_snapshot_now(deferred.replace(DeferredTrapSnapshot::None));
    } else if vm_ptr != 0 && site_ptr != 0 {
        // JIT r24 (CompletableFuture page, item 1): a spliced site answers
        // even while a catch client may be armed; its handler resumes in the
        // interpreter through a chain, whose push posts the catch
        // (`interpreter::report_spliced_catch_if_pending`).
        // SAFETY: as in the body: compiler-emitted site, the entering VM.
        let site = &*(site_ptr as *const cratonvm_jit::JitLocalHandlerSite);
        if site.must_answer() {
            crate::runtime::interpreter::note_spliced_catch_needs_report(
                crate::runtime::interpreter::compiled_catch_events_armed(
                    &*(vm_ptr as *const SharedVm),
                ),
            );
        }
    }
    answer
}

/// Body of [`jit_local_handler_lookup`], run under its panic guard.
/// `deferred`: see that function; only the implicit arm reads it.
unsafe fn jit_local_handler_lookup_body(
    vm_ptr: i64,
    site_ptr: i64,
    out_exc: *mut i64,
    deferred: &std::cell::Cell<DeferredTrapSnapshot>,
) -> i64 {
    if vm_ptr == 0 || site_ptr == 0 || out_exc.is_null() {
        return -1;
    }
    // While an `Exception` or `ExceptionCatch` client may be armed for this
    // VM (interpreter round i1 wave 21, lane L2), compiled code does not catch
    // its own throws: `-1` is the ordinary propagate path with every signal
    // left as found, so the throw reaches a VM door that resumes the handler
    // in an interpreter frame and posts the events there
    // (`interpreter::report_exception_caught_by_compiled_door`). Asked before
    // either arm, so it covers `local_handler_enter_implicit`, which is
    // reached only from here.
    // SAFETY: emitted by the compiler from `CompiledMethod`-owned data.
    let site = &*(site_ptr as *const cratonvm_jit::JitLocalHandlerSite);
    // Round nio21 wave 4 (lane L2): except a site inside a SPLICED callee
    // body (`JitLocalHandlerSite::SPLICED_SCOPE_BIT`), whose `-1` exit leaves
    // through the caller's `invoke` and would skip the callee's own `catch`;
    // its handler resumes in the interpreter at once. Such a site exists only
    // under `CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS` (default OFF).
    // SAFETY: `vm_ptr` is the hidden context argument the compiled method was
    // entered with, i.e. the `SharedVm` the interpreter handed it.
    if !site.must_answer()
        && crate::runtime::interpreter::compiled_catch_events_armed(&*(vm_ptr as *const SharedVm))
    {
        return -1;
    }
    // Crossing back out of JIT code to touch the class manager and (possibly)
    // load a catch class — the same boundary note every other helper makes.
    crate::jit::conservative_roots::note_jit_boundary();
    // An IR-tier deopt frame is not an exception at all, whatever else is
    // set: it describes a compiled frame's fate and only `deoptimize` may
    // consume it. Asked first, for both arms.
    if cratonvm_jit::deopt::has_last_deopt() {
        return -1;
    }
    let (sig_npe, sig_aioobe, sig_arithmetic, sig_deopt) = JIT_SIGNALS.with(|s| {
        (
            s.npe.get(),
            s.aioobe.get(),
            s.arithmetic.get(),
            s.deopt.get(),
        )
    });
    let implicit = implicit_signal_of(sig_aioobe, sig_npe, sig_arithmetic);
    if !matches!(implicit, ImplicitSignal::None) {
        // JIT round 19 wave 5 (lane handler5, SPLICE4-1): an optimizing body's
        // compiled-handler probe runs where no safepoint map describes its
        // frame, so it never builds a throwable (an allocation): propagate,
        // with every signal as found, and the exit it always took drains it.
        if site.no_collect {
            return -1;
        }
        // The IMPLICIT arm: a bounds check, an inline null check or a
        // zero-divisor guard is asking for a throwable this frame may be able
        // to catch. See `local_handler_enter_implicit`.
        return local_handler_enter_implicit(vm_ptr, site, out_exc, implicit, deferred);
    }
    // No implicit signal. A bare deopt is not an exception either, so the
    // dispatched-throwable arm below keeps the guard it always had.
    if sig_deopt {
        return -1;
    }
    // Read the pending throwable WITHOUT taking a `&mut JvmThread`, and hold no
    // thread borrow across the resolution below.
    //
    // `local_handler_index_slow` can run `load_class_concurrent` for a catch
    // type this VM has not seen. That is the thread-less load (no agent
    // transformer, no user loader, no `<clinit>`), so it runs no Java and no
    // nested throw can overwrite `jit_pending_exception` meanwhile; but it
    // allocates (the class's mirror), therefore a collection that walks this
    // very thread's roots is possible (JIT round 19 wave 2, lane throws2,
    // THROWS19-9: this said "arbitrary Java", which it is not). Holding a
    // `&mut JvmThread` across that is the aliasing shape `jit_thread_mut`'s own
    // debug guard exists to catch. Nothing here needs one: the class id is all
    // the resolution consumes, and the throwable is re-read from the thread
    // afterwards — a moving collector rewrites `jit_pending_exception` in
    // place, so a copy taken before the resolution would be stale anyway.
    let thread_ptr = current_jit_thread_ptr();
    if thread_ptr.is_null() {
        return -1;
    }
    // SAFETY: this OS thread's own `JvmThread`, installed by `set_jit_thread`
    // and alive for the whole JIT call. A shared-reference-shaped read of an
    // `Option`, creating no aliasing `&mut` — the same argument
    // `jit_pending_exception_is_set` makes for the same field.
    let Some(exc) = (*thread_ptr).jit_pending_exception else {
        if local_handler_lookup_dbg() {
            eprintln!(
                "[local-handler-oom-debug] pending=false candidates={:?}",
                site.candidates
            );
        }
        return -1;
    };
    // SAFETY: `vm_ptr` is the hidden context argument the compiled method was
    // entered with, i.e. the `SharedVm` the interpreter handed it.
    let vm = &*(vm_ptr as *const SharedVm);
    let cached = cratonvm_jit::JitLocalHandlerSite::decode_cache(
        site.cache.load(std::sync::atomic::Ordering::Relaxed),
    );
    // JIT round 19 proposal wave (lane throws19, TP16-15;
    // `CRATONVM_JIT_LOCAL_HANDLER_LEAN`, default ON since the round-19 proposal round, `=0` turns it off): a cache HIT is
    // answered on the class id read without `class_id_of`'s conservative
    // header validation (13.5% of `R16Throw2Door pre-local`'s samples in round
    // 16), and the debug switch below is latched per VM instead of read
    // through the flag table on every catch.
    //
    // Why the unvalidated read is sound here: `jit_pending_exception` is a
    // PRECISE root (`memory::roots` section 10 pushes it unconditionally, the
    // remap rewrites it in place), so every collection already dereferences it
    // as an object; its setters store either an oop compiled code threw
    // (`jit_throw_exception`, the operand compiled code itself dereferences) or
    // a throwable the VM just allocated. A value that is not an object start
    // would already be fatal at the next collection's mark. And only a HIT is
    // answered this way: the cached class id was produced by a validated read
    // of an earlier throwable (the miss arm below), so the answer is the one a
    // validated read of the same object returns. A miss -- a different class,
    // a zero id, an empty cache -- falls through to the validated read and the
    // slow resolution exactly as before.
    let lean = vm
        .threads
        .throwable_layout_cache
        .local_handler_lean_enabled();
    let lean_hit = if lean {
        cached.and_then(|(cached_class, cached_index)| {
            (cached_class != 0 && cached_class == vm.mem.heap.class_id_of_validated(exc).as_u32())
                .then_some((cached_class, cached_index))
        })
    } else {
        None
    };
    let (exc_class_u32, index) = match lean_hit {
        Some(hit) => hit,
        None => {
            let exc_class_id = vm.mem.heap.class_id_of(exc);
            let index = match cached {
                Some((cached_class, cached_index)) if cached_class == exc_class_id.as_u32() => {
                    cached_index
                }
                _ => {
                    let resolved = local_handler_index_slow(vm, site, exc_class_id);
                    // A decline is not an answer about this class: not cached.
                    if resolved != LOCAL_HANDLER_DECLINED {
                        site.cache.store(
                            cratonvm_jit::JitLocalHandlerSite::encode_cache(
                                exc_class_id.as_u32(),
                                resolved,
                            ),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }
                    resolved
                }
            };
            (exc_class_id.as_u32(), index)
        }
    };
    // Lean arm: dev's per-VM latch (TP16-15); otherwise the once-read switch
    // (nio21 wave 17: no declared-flag read per lookup).
    let dbg_lookup = if lean {
        vm.threads
            .throwable_layout_cache
            .dbg_local_handler_lookup_enabled()
    } else {
        local_handler_lookup_dbg()
    };
    if dbg_lookup {
        eprintln!(
            "[local-handler-oom-debug] exc={:#x} class={} candidates={:?} result={index}",
            exc.as_ptr() as usize,
            exc_class_u32,
            site.candidates
        );
    }
    if index < 0 {
        note_local_handler_at_catch(vm, cratonvm_jit::metrics::LOCAL_HANDLER_PROPAGATED);
        return -1;
    }
    // Committed: take the throwable and publish it where the handler's operand
    // stack expects it. Re-read through the thread rather than reusing `exc`,
    // which the resolution above may have left stale. Nothing between here and
    // the handler's first instruction can safepoint, and the emitter marked
    // that slot as an oop, so the first safepoint inside the handler sees it.
    let Some((thread, _guard)) = jit_thread_mut() else {
        // Unreachable: `current_jit_thread_ptr` was non-null above and this is
        // the same thread. Refuse rather than assume — a `-1` here is the
        // ordinary propagate path, which is always correct.
        note_local_handler_at_catch(vm, cratonvm_jit::metrics::LOCAL_HANDLER_PROPAGATED);
        return -1;
    };
    let taken = take_jit_pending_exception(thread);
    debug_assert!(taken.is_some(), "the peek above proved one was pending");
    // gen r4w5/oomjit5: the compiled-catch twin of the interpreter's
    // `settle_thrown_exception` drain. `taken` is the only copy the handler
    // needs, and it reaches the handler's oop slot below.
    let _ = drain_native_return_at_compiled_catch(thread, "compiled local handler");
    // gen r4w6/oomjit6 (opt-in): the frames this throw's callees published on
    // their way out to this catch have no consumer left.
    let _ =
        drop_orphaned_exceptional_frames_at_compiled_catch(vm, thread, "compiled local handler");
    note_local_handler_at_catch(vm, cratonvm_jit::metrics::LOCAL_HANDLER_ENTERED);
    JIT_SIGNALS.with(|s| {
        s.athrow_bci.set(-1);
        s.athrow_owner.set(0);
        // Interpreter round i5 wave 106, lane L2: the throwable is caught, so
        // no escape record (`note_dispatch_error_escape`) speaks for it now.
        s.redispatch_escape.set(0);
    });
    // Cast: an object reference is a raw address in a JIT frame slot, exactly
    // as every allocating helper returns one.
    *out_exc = taken.map_or(0, |e| e.as_ptr() as i64);
    // Widening: a candidate index, single digits.
    index as i64
}

/// The IMPLICIT arm of [`jit_local_handler_lookup`]: a bounds check, an inline
/// null check or a zero-divisor guard has signalled, and this frame may be
/// able to catch what the signal is asking for.
///
/// Until 2026-09-20 every one of those answered `-1` here, on the ground that
/// a pending signal is a *request* for a throwable and nothing in compiled
/// code could grant one. It can now: [`materialize_implicit_signal`] is the
/// same body the interpreter's drain and the callee door already use, so the
/// throwable, its message and its trace cannot depend on which door built it.
///
/// That matters because the implicit family is exactly the one the
/// performance page is about (`docs/feature-designs/jdk-only-mode.md
/// perf-implicit-exceptions-in-compiled-code-cost-microseconds-each-FIXED-20260920.md`):
/// the round trip it measures -- an exception-exit TRANSFER, an interpreted
/// handler, and a fresh OSR entry at the next back edge -- is what a `-1` here
/// costs, about 1.4 us of a 3.0-3.3 us total against HotSpot 0.4-0.7.
///
/// # Order of operations, and why it is this order
///
/// The candidate index is resolved FIRST, from the signal kind alone, and only
/// then is the throwable built. A site whose `catch` does not take this kind
/// therefore allocates nothing and consumes nothing: it answers `-1` with
/// every flag exactly as it found it, and the pad it was called from runs the
/// epilogue it always ran. Building first and discarding on a miss -- what
/// [`route_implicit_exc_through_callee`] has to do, because it is committed to
/// a callee frame either way -- would put an allocation on the propagate path
/// of every protected bci in the method.
///
/// The exception class is a function of the signal kind, so the answer is
/// cached per kind on the site (`JitLocalHandlerSite::implicit_cache`) rather
/// than in the monomorphic class-id slot the dispatched-throwable arm uses.
/// A site reached by both would otherwise thrash one slot between them.
///
/// # What it consumes on a hit
///
/// [`take_all_jit_signals`] -- all of it. The deopt flag above all: the pad
/// raised it so the interpreter would read the `i64::MIN` the pad returns as a
/// deopt rather than as a value, and this frame is now not going to return it.
/// Leaving it set makes the method's eventual ordinary return read as a deopt.
/// The trap-frame snapshot and the JEP-358 action code go to the throwable
/// being built, and `athrow_bci` is cleared because no throw is in flight any
/// more.
///
/// The one miss AFTER that drain is `materialize_implicit_signal` returning
/// `None`, i.e. the throwable class will not load. Everything drained is put
/// back there, so that path is the propagate path too.
///
/// A frame that has BOTH a real pending throwable and an implicit signal is
/// the superseded-signal shape `drain_superseded_implicit_signals` describes.
/// It answers `-1` here, exactly as it did before this arm existed: the real
/// throwable is the one in flight and the interpreter drain owns the choice.
#[cold]
#[inline(never)]
unsafe fn local_handler_enter_implicit(
    vm_ptr: i64,
    site: &cratonvm_jit::JitLocalHandlerSite,
    out_exc: *mut i64,
    implicit: ImplicitSignal,
    deferred: &std::cell::Cell<DeferredTrapSnapshot>,
) -> i64 {
    if !cratonvm_jit::implicit_local_handlers_enabled() {
        return -1;
    }
    let (kind_slot, exc_class_name) = match implicit {
        ImplicitSignal::Aioobe { .. } => (
            cratonvm_jit::JitLocalHandlerSite::IMPLICIT_KIND_AIOOBE,
            "java/lang/ArrayIndexOutOfBoundsException",
        ),
        ImplicitSignal::Npe => (
            cratonvm_jit::JitLocalHandlerSite::IMPLICIT_KIND_NPE,
            "java/lang/NullPointerException",
        ),
        ImplicitSignal::Arithmetic => (
            cratonvm_jit::JitLocalHandlerSite::IMPLICIT_KIND_ARITHMETIC,
            "java/lang/ArithmeticException",
        ),
        // Unreachable: the caller classified a non-`None` signal.
        ImplicitSignal::None => return -1,
    };
    let thread_ptr = current_jit_thread_ptr();
    if thread_ptr.is_null() {
        return -1;
    }
    // SAFETY: this OS thread's own `JvmThread`, installed by `set_jit_thread`
    // and alive for the whole JIT call. A shared-reference-shaped read of an
    // `Option`, creating no aliasing `&mut` -- the same argument
    // `jit_pending_exception_is_set` makes for the same field.
    if (*thread_ptr).jit_pending_exception.is_some() {
        return -1;
    }
    // SAFETY: `vm_ptr` is the hidden context argument the compiled method was
    // entered with, i.e. the `SharedVm` the interpreter handed it.
    let vm = &*(vm_ptr as *const SharedVm);
    let cached = site.implicit_cache[kind_slot].load(std::sync::atomic::Ordering::Relaxed);
    let index = if cached == cratonvm_jit::JitLocalHandlerSite::IMPLICIT_UNRESOLVED {
        // First throw of this kind at this site. Resolving the throwable class
        // may load it (`load_class_concurrent`: no Java runs, but it
        // allocates, so a GC can) -- the same window
        // `local_handler_index_slow`'s caller documents, and the reason the
        // read guard is scoped to the probe rather than held across the load.
        let found = {
            let cm = vm.classes.class_manager.read();
            cm.find_cp_class_for_class(exc_class_name, ClassId::new(site.declaring_class_id))
        };
        let exc_class_id = match found {
            Some(id) => Some(id),
            None => vm.load_class_concurrent(exc_class_name).ok(),
        };
        let Some(exc_class_id) = exc_class_id else {
            // The throwable class itself will not load. Nothing here can catch
            // it, and the drain has a better place to fail. Deliberately NOT
            // cached: a later attempt may find the class loaded.
            return -1;
        };
        let resolved = local_handler_index_slow(vm, site, exc_class_id);
        if resolved != LOCAL_HANDLER_DECLINED {
            site.implicit_cache[kind_slot].store(resolved, std::sync::atomic::Ordering::Relaxed);
        }
        resolved
    } else {
        cached
    };
    if index < 0 {
        note_local_handler_at_catch(vm, cratonvm_jit::metrics::LOCAL_HANDLER_PROPAGATED);
        return -1;
    }
    let Some((thread, _guard)) = jit_thread_mut() else {
        // Unreachable inside a compiled frame, which is entered with
        // `JIT_THREAD` installed. Refuse rather than assume: `-1` is the
        // ordinary propagate path, which is always correct.
        note_local_handler_at_catch(vm, cratonvm_jit::metrics::LOCAL_HANDLER_PROPAGATED);
        return -1;
    };
    let drained = take_all_jit_signals(thread);
    // Proved by the `jit_pending_exception.is_some()` refusal above, which
    // nothing between there and here can have changed. Stated because the
    // drain TAKES the throwable: a future edit that removed that refusal
    // would drop a real exception on the floor here, silently.
    debug_assert!(
        drained.exception.is_none(),
        "the implicit arm drained a real pending throwable"
    );
    // nio21 wave 17 L5 (proposal L5W16-1): the setter left the compiled-frame
    // snapshot to this probe. A throw the fast throw serves never reads it, so
    // it is not taken; any other throw takes it now, from the frames the setter
    // saw (the pad that called us is still running), before anything below
    // allocates. `fast_throw_will_fire` is a peek with the key the
    // materialisation's `try_fast_throw` counts under, and nothing runs on
    // this thread between the two.
    let mut trap_frames = drained.trap_frames;
    let deferred_snapshot = deferred.replace(DeferredTrapSnapshot::None);
    if deferred_snapshot != DeferredTrapSnapshot::None {
        let kind = match implicit {
            ImplicitSignal::Aioobe { .. } => {
                Some(crate::runtime::exceptions::FastThrowKind::ArrayIndexOutOfBounds)
            }
            ImplicitSignal::Npe => Some(crate::runtime::exceptions::FastThrowKind::NullPointer),
            ImplicitSignal::Arithmetic => {
                Some(crate::runtime::exceptions::FastThrowKind::Arithmetic)
            }
            ImplicitSignal::None => None,
        };
        let fast = kind
            .is_some_and(|kind| crate::runtime::exceptions::fast_throw_will_fire(vm, thread, kind));
        if !fast {
            take_deferred_trap_snapshot_now(deferred_snapshot);
            trap_frames = JIT_SIGNALS.with(|s| s.trap_frames.borrow_mut().take());
        }
    }
    // The snapshot comes back only on the miss, where it was never attached;
    // it used to be cloned up front, on every catch, to cover that one path.
    let (exc, unused_frames) = match materialize_implicit_signal_or_frames(
        vm,
        thread,
        implicit,
        trap_frames,
        drained.npe_action,
        // nio21 wave 18 L1: built inside the trap pad of the frame that will
        // catch it, so every frame the snapshot names is still live.
        true,
    ) {
        Ok(exc) => (Some(exc), None),
        Err(frames) => (None, frames),
    };
    let Some(exc) = exc else {
        // Put the drain back exactly as it was found -- flag, JEP-358 action,
        // snapshot, throw bci and the deopt flag -- so the pad epilogue and
        // the interpreter drain see the state they would have seen had this
        // function never run. `restash_jit_pending_trap_frames` ASSIGNS, so
        // each arm restores the snapshot exactly once.
        //
        // nio21 wave 17 L5: a deferred snapshot this arm did not take (the
        // fast throw was predicted and then could not be handed out) goes back
        // to the caller, `jit_local_handler_lookup`, which takes it on this
        // `-1` after the restash below, while the frames are still live.
        if unused_frames.is_none() && deferred_snapshot != DeferredTrapSnapshot::None {
            deferred.set(deferred_snapshot);
        }
        match implicit {
            ImplicitSignal::Aioobe { index, length } => {
                stash_jit_pending_aioobe(index, length);
                restash_jit_pending_trap_frames(unused_frames);
            }
            ImplicitSignal::Npe => restash_jit_pending_npe(drained.npe_action, unused_frames),
            ImplicitSignal::Arithmetic => {
                stash_jit_pending_arithmetic();
                restash_jit_pending_trap_frames(unused_frames);
            }
            // Unreachable: rejected at the top of this function.
            ImplicitSignal::None => {}
        }
        JIT_SIGNALS.with(|s| {
            s.athrow_bci.set(drained.athrow_bci);
            s.athrow_owner.set(drained.athrow_owner);
            // The NPE arm restored this with its flag; setting it again to the
            // same value is a no-op, and setting it for the other two restores
            // a cell the drain cleared rather than leaving a zero behind.
            s.npe_action.set(drained.npe_action);
            if drained.deopt {
                s.deopt.set(true);
            }
        });
        note_local_handler_at_catch(vm, cratonvm_jit::metrics::LOCAL_HANDLER_PROPAGATED);
        return -1;
    };
    // gen r4w5/oomjit5: as on the dispatched arm — the handler is entered and
    // `exc` is its only needed reference. No allocation follows before the
    // handler's first instruction, so dropping the handoff root cannot race.
    let _ = drain_native_return_at_compiled_catch(thread, "compiled local handler (implicit)");
    let _ = drop_orphaned_exceptional_frames_at_compiled_catch(
        vm,
        thread,
        "compiled local handler (implicit)",
    );
    note_local_handler_at_catch(vm, cratonvm_jit::metrics::LOCAL_HANDLER_ENTERED);
    note_local_handler_at_catch(vm, cratonvm_jit::metrics::LOCAL_HANDLER_IMPLICIT_ENTERED);
    // Cast: an object reference is a raw address in a JIT frame slot, exactly
    // as every allocating helper returns one. Nothing between here and the
    // handler's first instruction can safepoint, and the emitter marked that
    // slot as an oop, so the first safepoint inside the handler sees it.
    *out_exc = exc.as_ptr() as i64;
    // Widening: a candidate index, single digits.
    index as i64
}

/// [`local_handler_index_slow`]'s "propagate, and do not cache this": a catch
/// type that only the interpreter-side route can resolve. Negative, so every
/// caller's `index < 0` test reads it as propagate.
const LOCAL_HANDLER_DECLINED: i32 = -2;

/// The uncached half of [`jit_local_handler_lookup`]: walk this site's
/// candidates in exception-table order and answer with the first whose catch
/// type is assignable from `exc_class_id`, or `-1`.
///
/// **A catch type that will not load answers `-1`** — "propagate", the
/// ordinary route out of the compiled frame — instead of being skipped. That
/// row is a JVMS §5.4.3 resolution point: the interpreter unwinder and the
/// interpreter-side compiled-frame routes (`catch_row_verdict` in
/// `runtime/interpreter/exception_dispatch.rs`) resolve it loader-faithfully,
/// REPLACE the in-flight throwable with the resolution error, and restart the
/// search at the row's handler pc. This lookup's ABI (a candidate index plus
/// `out_exc`) cannot hand back a replacement throwable, and the loader-blind
/// `load_class_concurrent` here can fail for a type only a user loader sees,
/// so it declines and lets that route redo the whole search. Skipping the row
/// (the behaviour until round i1 wave 4) entered a LATER candidate with the
/// original exception, which the interpreter never does.
///
/// A name that is not loaded for the declaring class and that the global path
/// would answer by fabricating a synthetic stub is matched by name only, as
/// `catch_row_verdict` does (the Keycloak/Quarkus `PreventFurtherStepsException`
/// guard): loading it here would mint the stub.
///
/// **A declaring class with its own loader namespace** (a user-defined or
/// registered defining loader) resolves an unloaded catch type through THAT
/// loader, which only the interpreter-side route can do: the loader-blind
/// `load_class_concurrent` could load a same-named class from the application
/// class path, which `is_subclass_of` then refuses, and the walk would go on
/// to a later candidate — the skip the paragraph above rules out. Such a row
/// answers [`LOCAL_HANDLER_DECLINED`]: propagate, and do not cache the answer,
/// because the interpreter-side resolution loads the class and the next throw
/// finds it with `find_class_by_name_for_class`.
#[cold]
#[inline(never)]
fn local_handler_index_slow(
    vm: &SharedVm,
    site: &cratonvm_jit::JitLocalHandlerSite,
    exc_class_id: ClassId,
) -> i32 {
    let declaring = ClassId::new(site.declaring_class_id);
    let mut cm = vm.classes.class_manager.read();
    for (i, (catch_name, _handler_bci)) in site.candidates.iter().enumerate() {
        if catch_name.is_empty() {
            // `catch_type == 0` — a `finally` / catch-all. Matches anything.
            // Cast: candidate counts are single digits.
            return i as i32;
        }
        let catch_class_id = match cm.find_cp_class_for_class(catch_name, declaring) {
            Some(id) => id,
            None if cm.would_fabricate_synthetic_stub(catch_name) => {
                if cm.is_subclass_of_by_name(exc_class_id, catch_name) {
                    // Cast: candidate counts are single digits.
                    return i as i32;
                }
                continue;
            }
            None => {
                // JIT round 19 wave 5 (lane handler5, SPLICE4-1): loading the
                // catch type allocates (its mirror), which an optimizing
                // body's probe must not reach (see `JitLocalHandlerSite::
                // no_collect`). Decline, uncached: the exit it always took
                // resolves the row, and a later throw finds the class loaded.
                // A throwable of that type cannot exist before the type is
                // loaded, so only the resolution error is at stake, and the
                // interpreter-side route owns it.
                if site.no_collect {
                    return LOCAL_HANDLER_DECLINED;
                }
                let user_loader = matches!(
                    cm.get_loader_id(declaring),
                    Some(cratonvm_types::ClassLoaderId::UserDefined(_))
                );
                drop(cm);
                // Asked with the class-manager guard released, as
                // `referencing_class_has_loader_namespace` asks it.
                if user_loader
                    || cratonvm_native_builtins::classloader::defining_loader_for(
                        &vm.loader_identity,
                        site.declaring_class_id,
                    )
                    .is_some()
                {
                    return LOCAL_HANDLER_DECLINED;
                }
                let loaded = vm.load_class_concurrent(catch_name);
                cm = vm.classes.class_manager.read();
                match loaded {
                    Ok(id) => id,
                    // Unresolvable here: decline, so the interpreter-side
                    // route resolves this row and throws its error (see the
                    // doc comment). Never a skip to a later candidate.
                    Err(_) => return -1,
                }
            }
        };
        // The loader-blind name rule is `--compatible`-only for a resolved
        // row, exactly as `catch_row_verdict` applies it; the two must agree or
        // the same frame catches differently compiled and interpreted.
        if cm.is_subclass_of(exc_class_id, catch_class_id)
            || (!vm.config.is_jdk_only() && cm.is_subclass_of_by_name(exc_class_id, catch_name))
        {
            // Cast: candidate counts are single digits.
            return i as i32;
        }
    }
    -1
}

/// One runtime row (`entered` / `propagated` / `implicit-entered`) of the
/// compiled-local-handler census, counted at a compiled catch.
///
/// JIT round 21 wave 2, lane throws (THROWS21-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`;
/// `CRATONVM_JIT_THROW_CENSUS_STATS_ONLY`, opt-in): the census is a process-wide
/// array of atomics (`cratonvm_jit::metrics::note_local_handler`), so every
/// caught throw paid one or two `lock xadd`s on cache lines every throwing
/// thread shares -- true sharing on a multi-threaded catch loop. Its only reader
/// is the `[cratonvm] local handlers:` line, printed only under
/// `CRATONVM_DBG_JIT_METHOD_STATS`, so with the switch on and the stats off the
/// row is not counted. Nothing but that line changes.
#[inline]
fn note_local_handler_at_catch(vm: &SharedVm, row: usize) {
    if vm.threads.throwable_layout_cache.throw_census_skipped() {
        return;
    }
    cratonvm_jit::metrics::note_local_handler(row);
}

// ---------------------------------------------------------------------------
// JDK-only policy for the JIT's own by-name native fast paths
// (`docs/book/src/user-guide/jdk-only-mode.md` §1, §7, §10)
//
// The JIT crate cannot call the shared resolver: `jit/Cargo.toml` depends on
// `types`/`reader`/`jit-api` but NOT on `cratonvm-native-api` or `cratonvm-vm`,
// and it cannot, because `vm` depends on `jit`. So every path from compiled
// machine code into a native callback runs through a VM-implemented function
// pointer whose body lives in THIS file. That makes these helpers the JIT's
// half of the §7 contract.
//
// # Where the policy check lives, and why it is not on the hot path
//
// Almost every by-name fast path in this file is **resolved once per call
// site** and then cached (`OBJECT_NATIVE_DISPATCH_CACHE`,
// `INTEGER_NATIVE_DISPATCH_CACHE`). The policy is fixed for a VM's lifetime,
// so the whole decision is hoisted to that cold cache-fill edge:
//
//   * **Compatible** — [`admit_jit_fast_native`] returns the caller's own
//     callback unchanged. It never consults the kind, never asks the class
//     manager anything, and never builds a violation. The only thing it adds
//     is one `resolve_id` (a triple hash) at fill time, paid once per call
//     site, in exchange for a census handle. Dispatch is bit-for-bit what it
//     was, plus the one relaxed increment below.
//
//   * **JdkOnly** — the strict arm is `#[cold] #[inline(never)]`. When policy
//     refuses, the fast path simply **does not become a cache entry**, so the
//     hot path never grows a strict-mode branch at all: the call falls through
//     to the generic `crate::vm::invoke_or_native` tail, which agent E already
//     routed through `resolve_native_dispatch_wave1` and which raises the
//     structured `VmError::JdkOnly(..)`. The refusal is therefore never a
//     silent fallback to the stub and never a wild jump — it is a decision to
//     stop optimizing and let the single authoritative resolver adjudicate.
//     That is the wave-1-safe answer §10 asks for, and it is the same shape
//     the JIT crate itself already uses for its thin direct-call helpers.
//
// Refusing here can only ever be *more* conservative than dispatching here:
// the generic path resolves the same triple with strictly more information
// (a real `&Class` and `&Method`), so it can never be wrong where this fast
// path would have been right.
//
// The two exceptions to "cached, so pay at fill time" are documented at their
// own call sites, because each needed a different hoist:
//
//   * `jit_invoke_virtual_mic`'s exact-receiver **Matcher** leaf re-decides on
//     every dispatch by design, so it gets a single
//     `dispatch_policy(vm).is_jdk_only()` gate rather than per-call admission.
//   * the **`ClassLoader.getResource*`-with-null** intercept in the same
//     function also resolves per call, but only a `null` resource name reaches
//     it — the JVM-mandated NPE path — so it can afford full admission inline.
//
// # Why a kind-discarding `find` is the actual bypass
//
// The pre-existing code called `NativeMethodRegistry::find`, which throws the
// `NativeKind` away. Policy is *defined* on the kind — `SyntheticStub` is the
// one kind §1.3 forbids invoking — so a lookup that discards it cannot enforce
// anything, whatever it does afterwards. Every site below now resolves through
// `resolve_id` + `callback_of`/`kind_of_id`, which keeps the kind AND yields
// the `NativeMethodId` the §4 census needs.
// ---------------------------------------------------------------------------

// Structured JDK-only violations recorded by the JIT runtime helpers, their
// drop counter and the refusal-event counter: since t1 wave 3 (lane Y) the
// `jit_helper` sink of THIS VM's `cratonvm_jit::jdk_only_sink::JdkOnlyReportSinks`
// (`JitCache::jdk_only_sinks`). They were three process statics
// (`JDK_ONLY_HELPER_VIOLATIONS`, `JDK_ONLY_HELPER_VIOLATIONS_DROPPED`,
// `JDK_ONLY_FASTPATH_REFUSALS`), so every `--jdk-only` VM in the process wrote
// one report. Bounded and deduplicated exactly as before.

/// Maximum number of distinct structured violations these helpers retain, by
/// default.
///
/// Read through [`jdk_only_jit_helper_violation_cap`], never directly — an
/// operator can raise it, and a site reading this constant would report the
/// default while the sink obeyed something else. Same shape, same default and
/// the same override knob as the interpreter's sink
/// (`crate::vm::JDK_ONLY_NATIVE_SHADOW_CAP`), deliberately: both feed one
/// `--jdk-only-report`, and two caps a reader has to remember separately is how
/// one of them ends up quoted for the other.
pub const JDK_ONLY_HELPER_VIOLATION_CAP: usize = 4096;

/// Ceiling on the operator override. Mirrors the interpreter sink's, for the
/// same reason: a mistyped value must not turn a diagnostic into a memory leak.
const JDK_ONLY_HELPER_VIOLATION_CAP_MAX: usize = 65_536;

fn jdk_only_helper_sink(vm: &SharedVm) -> &cratonvm_jit::jdk_only_sink::JdkOnlyViolationSink {
    &vm.jit.jit_cache.jdk_only_sinks().jit_helper
}

/// Snapshot of the structured JDK-only violations the JIT runtime helpers
/// recorded this run. Read when building `--jdk-only-report`; never hot.
///
/// This is the runtime-helper counterpart to
/// `cratonvm_jit::jdk_only_jit_violations()` (compile-time refusals). The two
/// lists are disjoint and both belong in the report.
pub fn jdk_only_jit_helper_violations(vm: &SharedVm) -> Vec<cratonvm_types::error::JdkOnlyViolation> {
    jdk_only_helper_sink(vm).rows()
}

/// Number of JIT by-name native fast-path admissions refused under `JdkOnly`
/// in this VM. Includes both outright `Reject`s and §7-step-3 yields to
/// bytecode.
pub fn jdk_only_jit_fastpath_refusals(vm: &SharedVm) -> u64 {
    jdk_only_helper_sink(vm).events()
}

/// The cap this process is actually using for the helper sink.
///
/// Reads the SAME declared knob as the interpreter's sink,
/// `CRATONVM_NATIVE_SHADOW_SINK_CAP` (`flag_groups::INVENTORY`,
/// `CRATONVM_DBG=native-shadow-sink-cap`). One knob for both on purpose: a
/// census run raises "the observation sink" once and gets both of the report's
/// bounded collections, instead of raising one and quietly reading a truncated
/// other. Reusing the declared name is also what keeps this read out of
/// `types/tests/flag_declaration_guard.rs` — an undeclared `getenv` is served
/// from a different source than the latched snapshot every other knob uses, and
/// is a defect in this tree rather than untidiness.
///
/// A value of `0`, an empty or a non-numeric value leaves the default in place:
/// a diagnostic must never be the thing that fails, and a cap of zero would
/// report an empty population as a complete one. A value ABOVE
/// [`JDK_ONLY_HELPER_VIOLATION_CAP_MAX`] is CLAMPED to it rather than dropped to
/// the default, and every adjustment prints one `[cratonvm]` line — see
/// [`cratonvm_types::flags::resolve_capped_usize`] and the WORKER-5 correction
/// on `crate::vm::jdk_only_native_shadow_cap`. Sharing that resolver is also
/// what keeps the two sinks' answers to ONE knob from drifting apart.
pub fn jdk_only_jit_helper_violation_cap() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        cratonvm_types::flags::resolve_capped_usize(
            "CRATONVM_NATIVE_SHADOW_SINK_CAP",
            "JIT fast-path violation sink",
            JDK_ONLY_HELPER_VIOLATION_CAP,
            JDK_ONLY_HELPER_VIOLATION_CAP_MAX,
        )
    })
}

/// How many distinct violations this sink is holding right now.
pub fn jdk_only_jit_helper_sink_len(vm: &SharedVm) -> usize {
    jdk_only_helper_sink(vm).len()
}

/// How many violations this sink had no room for.
///
/// **An EVENT count, and deliberately not dressed up as a distinct-row count.**
/// This sink dedups with `Vec::contains` over the rows it RETAINED; a row it
/// dropped was never pushed, so a second refusal naming the same triple is not
/// recognised as a repeat and is counted again. The number is therefore an
/// upper bound on the distinct rows missing from `violations[]`, and a
/// perfectly good answer to the only question that has to be answered without
/// a rebuild — *is this list the population, or is it short?*
///
/// Making it exact costs a second dedup structure that would itself need a cap,
/// i.e. the same problem one level down. Raising
/// `CRATONVM_NATIVE_SHADOW_SINK_CAP` until this reads zero is the way to get an
/// exact list, and it needs no code change at all.
///
/// The interpreter's counterpart, `crate::vm::jdk_only_native_shadow_sink_dropped`,
/// gets closer to distinct-row counting because that sink has a 512-slot
/// filter that survives the drop. Neither is exact; both are honest about which
/// way they err, and both err upward.
pub fn jdk_only_jit_helper_sink_dropped(vm: &SharedVm) -> u64 {
    jdk_only_helper_sink(vm).dropped()
}

/// Was at least one violation dropped for want of room?
///
/// **Not `len() == cap`.** A run whose last distinct violation exactly fills the
/// sink dropped nothing and must not be reported truncated — the same
/// distinction `crate::vm::jdk_only_native_shadow_sink_saturated` documents, and
/// the reason this is derived from the drop counter rather than from the length.
pub fn jdk_only_jit_helper_sink_saturated(vm: &SharedVm) -> bool {
    jdk_only_jit_helper_sink_dropped(vm) > 0
}

/// Record one refused fast-path admission, with its structured violation when
/// the resolver produced one.
///
/// `#[cold] #[inline(never)]`, mirroring `vm_exec.rs`'s reject helpers: the
/// `String` allocations a `JdkOnlyViolation` owns live only on this path, and
/// this path is unreachable in `Compatible` mode.
#[cold]
#[inline(never)]
fn record_jdk_only_fastpath_refusal(
    vm: &SharedVm,
    violation: Option<cratonvm_types::error::JdkOnlyViolation>,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    Option<cratonvm_native_api::NativeMethodId>,
)> {
    // The event count, the dedup-before-cap test (a full sink tells "a
    // repeat" from "no room" and counts only the latter as a drop) and the
    // drop counter are `JdkOnlyViolationSink::record`'s.
    jdk_only_helper_sink(vm).record(violation, jdk_only_jit_helper_violation_cap());
    None
}

/// Whether concrete bytecode exists for this triple — the raw hierarchy probe.
///
/// Deliberately shaped like `invoke_or_native`'s own `has_real` probe in
/// `vm/src/vm/vm_exec.rs` so the two sites cannot drift into disagreeing about
/// what "has bytecode" means. Only ever called from the strict arm below, and
/// only under the same `SyntheticStub`-on-an-allow-listed-class condition
/// `invoke_or_native` computes it under, so the class-manager read lock and
/// the hierarchy walk are off the hot path and off every `Compatible` path.
///
/// **On its own this is NOT §7 step 3's input at this door.** See
/// [`jit_fast_native_bytecode_terms`], which is.
#[cold]
#[inline(never)]
fn jit_fast_native_has_bytecode(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    let cm = vm.classes.class_manager.read();
    cm.get_loaded_class_id(class_name)
        .and_then(|cid| {
            cm.get_class(cid).and_then(|cls| {
                if cls.origin.is_compatibility_stub() {
                    None
                } else {
                    crate::classloading::find_method_recursive(
                        cid,
                        method_name,
                        descriptor,
                        &cm.class_store,
                    )
                    .map(|(m, _)| !m.is_native() && m.code().is_some())
                }
            })
        })
        .unwrap_or(false)
}

/// The two §7 inputs **at this door**, as `(has_real, bytecode_available)` —
/// the same pair `invoke_or_native` computes, so the caller can hand
/// `resolve_native_dispatch_wave1` `(!has_real, bytecode_available)` exactly as
/// that function hands them over.
///
/// # Why this is not just `jit_fast_native_has_bytecode`
///
/// A refused fast path does not stop a call happening; it routes it to the
/// generic tail, which for a compiled site is `vm_exec::invoke_or_native`. So
/// a refusal only means something if the tail refuses too. It did not.
///
/// `invoke_or_native` computes its own `bytecode_available` as
/// `has_real || dial`, and `has_real` is computed **only** when
/// `real_protected_stub` holds — a `SyntheticStub` on
/// `real_protected_stub_class`. For a `Bridge` it is therefore always `false`,
/// which is `H17-3` §6's finding in that function's own words: "for a `Bridge`
/// it is always `false` -- which is why arming a class never changed anything
/// that arrived HERE". This door, by contrast, ran the hierarchy probe
/// unconditionally, so a `Bridge` standing in front of concrete bytecode was
/// refused here and **run by the tail anyway**.
///
/// MEASURED 2026-09-22, `probes/HugeByteLoopProbe.java` (the inner byte loop of
/// netty `AbstractIntegrationTest#testHugeDecompress`), one host, one binary,
/// 1 MiB of `MessageDigest.update(byte)`:
///
/// ```text
///   --compatible   347 ms   find_with_kind = 14 598   invokes(general) = 2 197
///   --jdk-only     698 ms   find_with_kind = 1 049 006  invokes(general) = 1 034 351
/// ```
///
/// One `find_with_kind` per byte, and `--dump-native-registry` with
/// `CRATONVM_CENSUS_EXACT_INVOCATIONS=1` recorded
/// `java/security/MessageDigest.update(B)V` — a `bridge` — dispatched
/// **1 048 576 times**, i.e. exactly once per call. The refusal bought no
/// policy: the same native ran, through the slowest route in the VM.
///
/// So this door asks `invoke_or_native`'s question, in `invoke_or_native`'s
/// terms, and §7's promise that "the answer cannot depend on which door the
/// call came through" holds for the `Bridge` case as well. What the dial arms
/// is still honoured — it is the term that actually makes the tail yield.
///
/// `resolve_native_site` refuses a `SyntheticStub` under `JdkOnly` before it
/// ever reaches here, so the `has_real` half is dead on that path today; it is
/// written out anyway because [`admit_jit_fast_native`] has other callers and a
/// predicate that mirrors a sibling must mirror all of it.
///
/// **Stale since 2026-10-08 (`0dac905fcf`), kept as written** (JIT round 21
/// wave 4, lane compat, COMPAT21-1): `invoke_or_native` no longer computes
/// `has_real` this way; it asks `registered_native_yields_to_loaded_bytecode`,
/// which is `resolve_dispatch` on the loaded declaration, so a `Bridge` in
/// front of concrete bytecode yields there in BOTH modes. Neither caller relies
/// on this pair alone for that case any more: [`admit_jit_fast_native_resolved`]
/// asks the resolver before its `--jdk-only` arm reaches here, and
/// [`direct_helper_refusal`] asks it first through
/// `thin_helper_tail_yields_to_bytecode`. What this pair still contributes is
/// the dial, which can only refuse.
fn jit_fast_native_bytecode_terms(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    kind: cratonvm_native_api::NativeKind,
) -> (bool, bool) {
    let real_protected_stub = kind == cratonvm_native_api::NativeKind::SyntheticStub
        && crate::runtime::interpreter::real_protected_stub_class(class_name);
    let has_real = real_protected_stub
        && jit_fast_native_has_bytecode(vm, class_name, method_name, descriptor);
    let dial = crate::runtime::interpreter::jdk_only_dial_yields_to_bytecode(
        vm,
        class_name,
        method_name,
        descriptor,
        kind,
    );
    (has_real, has_real || dial)
}

/// The `JdkOnly` arm of [`admit_jit_fast_native`]. Cold by construction.
#[cold]
#[inline(never)]
fn jdk_only_admit_jit_fast_native(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    callback: cratonvm_native_api::NativeCallback,
    id: Option<cratonvm_native_api::NativeMethodId>,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    Option<cratonvm_native_api::NativeMethodId>,
)> {
    let registry = &vm.natives.native_methods;
    // The resolver is handed the FAST-PATH callback tagged with the REGISTERED
    // kind. Policy is a property of the registration; the callback is only the
    // thing that runs if policy admits it. Pairing them this way is what lets
    // an admitted decision keep the fast path's specialized callback (e.g. the
    // real-layout `Matcher.find` intrinsic) instead of silently swapping in a
    // differently-shaped one.
    //
    // No registration at all (`id == None`) yields `native == None`, the
    // resolver answers `None`, and the fast path is refused. That is the right
    // answer: a VM-side reimplementation the registry has never heard of
    // cannot be policy-audited, so it must not stand in front of real bytes.
    let native = id
        .and_then(|id| registry.kind_of_id(id))
        .map(|kind| (callback, kind));
    // Both §7 inputs are `invoke_or_native`'s, spelled its way: `has_real` is
    // the only thing that can veto the native there, and `has_real || dial` is
    // the only thing that can make step 3 fire. See
    // [`jit_fast_native_bytecode_terms`] for the measurement that forced
    // this door to stop asking a stricter question than the tail it falls
    // through to.
    let (has_real, yields) = match native {
        Some((_, kind)) => {
            jit_fast_native_bytecode_terms(vm, class_name, method_name, descriptor, kind)
        }
        // No registration: `resolve_native_dispatch_wave1` answers `None` on
        // its first line, so neither term can be read. Say so rather than run
        // a hierarchy walk for an answer nothing consults.
        None => (false, false),
    };
    match crate::vm::resolve_native_dispatch_wave1_in(
        &vm.natives.native_shadow_sink,
        crate::vm::DispatchDoor::JitFastNative,
        crate::vm::dispatch_policy(vm),
        class_name,
        method_name,
        descriptor,
        native,
        // `compat_native_wins` — this site's pre-existing compatibility
        // verdict, now stated the way the generic tail states it. Reaching here
        // at all *is* that verdict (the caller matched the name triple against
        // its fast-path table and guarded the exact receiver class), minus the
        // one thing that overrides it at `invoke_or_native`: a `SyntheticStub`
        // on an allow-listed class whose real bytecode is loaded.
        !has_real,
        yields,
    ) {
        // §1.3 — a fake may not be invoked under `--jdk-only`.
        Some(crate::vm::DispatchDecision::Reject(violation)) => {
            record_jdk_only_fastpath_refusal(vm, Some(violation))
        }
        Some(decision) => match decision.native_callback() {
            Some(cb) => Some((cb, id)),
            // `Bytecode` is unreachable from the name-only adapter (it has no
            // `&Method` to borrow), but refusing rather than asserting keeps a
            // future resolver change from turning into a JIT miscompile.
            None => record_jdk_only_fastpath_refusal(vm, None),
        },
        // "No opinion": §7 step 3 (concrete bytecode beats this bridge), or
        // nothing registered. Refuse the optimization; the generic tail runs
        // the real bytes.
        None => record_jdk_only_fastpath_refusal(vm, None),
    }
}

/// Policy admission + census handle for one of the JIT's by-name native fast
/// paths. Called at **cache-fill** time only — see the banner above.
///
/// Returns the callback to install and the `NativeMethodId` the dispatch edge
/// should count against, or `None` to decline the fast path entirely (the
/// caller then falls through to generic, policy-checked dispatch).
#[inline]
fn admit_jit_fast_native(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    callback: cratonvm_native_api::NativeCallback,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    Option<cratonvm_native_api::NativeMethodId>,
)> {
    // One triple hash, once per call site, to obtain the census handle. This is
    // the `resolve_id` + `callback_of`/`kind_of_id` shape the §7 routing note
    // in `vm_exec.rs` names as the replacement for a kind-discarding `find`:
    // the id is what makes the dispatch-edge increment a single relaxed add
    // rather than a second full lookup.
    let id = vm
        .natives
        .native_methods
        .resolve_id(class_name, method_name, descriptor);
    admit_jit_fast_native_resolved(vm, class_name, method_name, descriptor, callback, id)
}

/// [`admit_jit_fast_native`] for callers that already hold the
/// `NativeMethodId` (because their fast-path callback *came* from the
/// registry), so the triple is hashed once rather than twice at fill time.
#[inline]
fn admit_jit_fast_native_resolved(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    callback: cratonvm_native_api::NativeCallback,
    id: Option<cratonvm_native_api::NativeMethodId>,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    Option<cratonvm_native_api::NativeMethodId>,
)> {
    admit_jit_fast_native_on_owner(vm, None, class_name, method_name, descriptor, callback, id)
}

/// [`admit_jit_fast_native_resolved`] with the class, by id, whose loaded
/// declaration decides native vs bytecode (zt t29/jd: the native site cache's
/// loader-faithful owner, [`native_site_admission_owner`]); `None` decides on
/// the loader-blind name table's class named `class_name`, as before.
#[inline]
fn admit_jit_fast_native_on_owner(
    vm: &SharedVm,
    owner: Option<ClassId>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    callback: cratonvm_native_api::NativeCallback,
    id: Option<cratonvm_native_api::NativeMethodId>,
) -> Option<(
    cratonvm_native_api::NativeCallback,
    Option<cratonvm_native_api::NativeMethodId>,
)> {
    if id
        .and_then(|id| vm.natives.native_methods.kind_of_id(id))
        .is_some_and(|kind| {
            crate::vm::registered_native_yields_to_bytecode_on_owner(
                vm,
                owner,
                class_name,
                method_name,
                descriptor,
                callback,
                kind,
            )
        })
    {
        return None;
    }
    if crate::vm::dispatch_policy(vm).is_jdk_only() {
        return jdk_only_admit_jit_fast_native(
            vm,
            class_name,
            method_name,
            descriptor,
            callback,
            id,
        );
    }
    Some((callback, id))
}

/// Leaf-native fast-path hits from compiled code, reported on shutdown by
/// `CRATONVM_DBG=intrinsic-stats` beside the interpreter's own counter.
///
/// This exists for the same reason that one does, and the reason is worth
/// restating: **timings cannot tell "the fast path was never installed" from
/// "it was installed and is no faster."** Two earlier attempts at the
/// interpreter's `Thread.currentThread` bypass were inert, and only a counter
/// showed it. A JIT-side path has one extra way to be silently inert — the
/// site cache can resolve to `None` and cache the refusal forever — so the
/// number this prints is the acceptance criterion, not the ns/op.
/// Leaf-native dispatches served from compiled code this run.
pub fn leaf_native_hit_count() -> u64 {
    jit_counter_total(|b| {
        b.leaf_native_hits
            .load(std::sync::atomic::Ordering::Relaxed)
    })
}

/// This thread's +1 for a leaf dispatch. See [`JitCounterBlock`].
#[inline]
fn note_leaf_native_hit() {
    JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.leaf_native_hits));
}

/// Site-cached NON-leaf native dispatches from compiled code — the ones that
/// skipped `invoke_or_native` but still entered the full funnel.
///
/// Reported separately from the leaf count because they answer different
/// questions and have different expected magnitudes: this is every registered
/// native a compiled method calls, whereas the leaf count is only the audited
/// accessor set.
/// This thread's +1 for a non-leaf site-cached dispatch. See
/// [`JitCounterBlock`].
#[inline]
fn note_site_cached_native_hit() {
    JIT_COUNTERS.with(|b| JitCounterBlock::bump(&b.site_cached_native_hits));
}

/// Is the per-call-site native fast path for compiled code enabled?
///
/// **Default ON.** `CRATONVM_JIT=-native-site-cache` is the kill switch, and it
/// exists because this path spent 2026-08-05 as the prime suspect for the
/// Spring Boot corruption family with no way to take it out of a run short of a
/// ten-minute rebuild.
///
/// # What the switch is for
///
/// The path serves a registered native at the TOP of `jit_invoke_dispatch` /
/// `jit_invoke_virtual_mic`, ahead of the inline cache and the compile probes,
/// and it reads `NATIVE_SITE_CACHE` — one of the memos keyed on a
/// `JitInvokeInfo` ADDRESS. While those addresses were recyclable (fixed in
/// `383e7f5cf`, "a recycled JitInvokeInfo address let one call site serve
/// another's dispatch") this cache was the loudest way that hazard surfaced: a
/// site would call the PREVIOUS site's native and hand back whatever it
/// returned.
///
/// Measured on `module/spring-boot-batch-data-mongodb`'s
/// `BatchDataMongoAutoConfigurationTests` (13 tests; `--nojit` green; HotSpot
/// green), one fixture, one host, the path switched at runtime:
///
/// | tree | site cache | runs | runs with >=1 failure |
/// |---|---|---:|---:|
/// | before `383e7f5cf` | off | 14 | **0** |
/// | before `383e7f5cf` | leaves only | 12 | 3 |
/// | before `383e7f5cf` | every registered native | 8 | **8** |
/// | with `383e7f5cf` | every registered native | 14 | **0** |
///
/// The cache was the amplifier, not the defect. The last row is why it is still
/// on by default; the row above it is why the switch is worth its two lines — a
/// path whose failure mode is "call some other call site's native" should be
/// removable from a run in one flag.
///
/// See
/// `batch-data-mongodb-mongocustomconversions-noclassdeffounderror-RESOLVED-20260805.md`.
pub(crate) fn native_site_cache_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || !cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_NO_NATIVE_SITE_CACHE"))
}

/// Non-leaf natives dispatched from a resolved call site this run.
pub fn site_cached_native_hit_count() -> u64 {
    jit_counter_total(|b| {
        b.site_cached_native_hits
            .load(std::sync::atomic::Ordering::Relaxed)
    })
}

/// Call sites `resolve_native_site` declined, and why.
///
/// A hit count of zero has two very different explanations — "nothing on this
/// workload is a leaf" and "every site was refused for a reason I did not
/// intend" — and only the second is a bug. Rather than rebuild the VM to find
/// out (a release build here is ~15 minutes), the refusal reason is recorded at
/// fill time, which runs once per site and never on the dispatch path.
///
/// `CRATONVM_DBG=intrinsic-stats` prints the tally. The strings are `&'static`
/// reason tags, not formatted messages, so nothing allocates unless the flag
/// asked for the dump.
mod site_refusal {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// One counter per refusal reason, in the order they are tested.
    pub(super) static COUNTS: [AtomicU64; 13] = [
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
        AtomicU64::new(0),
    ];

    pub(super) const REASONS: [&str; 13] = [
        "invoke-kind not virtual/interface/static, or receiver not a heap object",
        "method name is special-cased by invoke_or_native",
        "receiver class unavailable",
        "capability-classified triple",
        "no native for the triple on the receiver or its supers (bytecode wins)",
        // Slot 5 meant "registered, but does not claim leaf". It was retired
        // when non-leaf natives started getting entries too, and is LIVE AGAIN
        // under `CRATONVM_JIT_SITE_CACHE=leaf`, which restores the leaf-only
        // cache this path shipped with before 836631dcc. Under the default
        // `all` nothing reports it, exactly as during its retirement.
        "registered, but does not claim leaf (mode=leaf)",
        // Slot 6 used to be every `SyntheticStub` plus every policy refusal in
        // one number, which made the two indistinguishable — and they have
        // opposite remedies. A stub that YIELDS (slot 8) must be refused: the
        // bytecode is what runs, and 2026-08-22 relaxed the compile gates so
        // the JIT compiles it. A stub that WINS is an ordinary registered
        // native that this cache simply declined to resolve, and refusing it
        // cost every call `invoke_or_native`'s full cascade — 4.77 calls per
        // expired task on `HashedWheelTimerTest` alone
        // (`AbstractOwnableSynchronizer.setExclusiveOwnerThread`).
        //
        // In `Compatible` mode slot 6 is now reachable only through the
        // `CRATONVM_JIT_SITE_CACHE_STUBS=0` lever, so a non-zero value there
        // with the lever unset is a defect. Under `--jdk-only` it is the
        // ordinary §1.3 refusal and a large number is expected.
        "SyntheticStub refused without asking the arbitration (--jdk-only, or the lever)",
        "site cache disabled (mode=off)",
        "SyntheticStub yields to real bytecode (the bytecode runs; correct to refuse)",
        "policy refused (--jdk-only §1.3), or no callback for the slot",
        // Slot 10 meant "the native answers an `ACC_SYNCHRONIZED` method while
        // `native_sync_enabled`, and a cached entry would not hold its
        // monitor". RETIRED in interpreter round i1 wave 11 (lane L5): the
        // entry carries the fact (`NativeSiteCache::sync`) and the dispatch
        // holds the monitor, so nothing reports it; kept so the indices of
        // the reasons above stay put.
        "synchronized method under native-sync (retired: the entry holds the monitor)",
        // Slot 11 (interpreter round i1 wave 12, lane L5): a static site whose
        // declaring class is not initialized yet. Declined without caching;
        // the funnel tail runs `<clinit>` and the next call fills the entry.
        // A count that keeps climbing names a class whose initializer failed.
        "static site whose declaring class is not initialized (not cached; the tail initializes)",
        // Slot 12 (round 15 wave 5, lane walk4; F4 of
        // `r15w4-walk3-review-findings-FIXED-20260930.md`): a JVMTI-redefined owner.
        // The ancestor walk stopped at a redefined ancestor whose native the
        // slow path keeps for some receivers only, or the owner scope refused
        // a woven owner. These used to be counted in slot 4, so a Mockito
        // run's refusals read as missing registrations.
        "redefined owner: the woven body or the per-receiver slow path decides",
    ];

    #[inline]
    pub(super) fn note(reason: usize) -> Option<super::NativeSiteCache> {
        COUNTS[reason].fetch_add(1, Ordering::Relaxed);
        None
    }

    /// [`note`] for the dispatch-side bails, which return "not served here"
    /// rather than "no cache entry".
    #[inline]
    pub(super) fn note_and_decline(reason: usize) -> Option<i64> {
        COUNTS[reason].fetch_add(1, Ordering::Relaxed);
        None
    }

    pub(super) fn report() -> Vec<(&'static str, u64)> {
        REASONS
            .iter()
            .zip(COUNTS.iter())
            .map(|(r, c)| (*r, c.load(Ordering::Relaxed)))
            .filter(|(_, n)| *n > 0)
            .collect()
    }
}

/// Per-reason tally of JIT call sites the leaf fast path declined. Cold; read
/// once on shutdown by `CRATONVM_DBG=intrinsic-stats`.
pub fn leaf_native_refusals() -> Vec<(&'static str, u64)> {
    site_refusal::report()
}

/// How much of the per-call-site native cache is in effect. **Default `All`.**
///
/// ## This lever once carried a wrong conclusion. Read the correction first.
///
/// The cache resolves a JIT call site's native ONCE and dispatches it directly,
/// skipping `invoke_or_native`'s hand-written gate cascade. 836631dcc widened it
/// from leaf natives to every registered native, and `CacheAutoConfigurationTests`
/// then failed 53 of 59 with the cache on and 0 with it leaf-only — on one
/// binary, with this mode as the only variable. That measurement was real. The
/// conclusion drawn from it, that the widening was the defect, was **wrong**.
///
/// `dev` landed `383e7f5cf` — "a recycled `JitInvokeInfo` address let one call
/// site serve another's dispatch" — during the same investigation, and that is
/// the actual defect. Every memo here is keyed on `(vm_identity, JitInvokeInfo
/// pointer)`; those boxes are freed with the `CompiledMethod` that owns them, so
/// the allocator can hand the same address to the next compile and the key then
/// names a DIFFERENT call site while the memo still holds the old site's answer.
/// A cache holding a resolved native is the loudest form of that — the reused
/// site CALLs the previous site's native and returns what it returns — which is
/// exactly why widening this cache *amplified* the recycled-key defect until it
/// looked like its cause.
///
/// Re-measured on current `dev` (with `383e7f5cf` in), same class, same fixture:
///
/// | mode | failures | when |
/// |---|---:|---|
/// | `all` | 53 | before `383e7f5cf` |
/// | `leaf` | 0 | before `383e7f5cf` |
/// | **`all`** | **0, twice** | **after `383e7f5cf` — the default** |
///
/// The faces were all "a call returned the wrong object": `Function$Identity`
/// (whose native returns its first argument) dispatched 5 870 times under the
/// JIT and 0 times under `--nojit`, `StreamSupport.stream(spliterator, false)`
/// handing back the spliterator, `Proxy$Dispatch.invokeProxy` reached with a
/// null `Method`. See
/// `cacheautoconfigurationtests-configclass-parse-nosuchmethod-FIXED.md`.
///
/// So `all` is restored. Gating it would cost 836631dcc's win to work around a
/// defect that no longer exists. **A perf change that makes a latent defect
/// reproducible is an amplifier, not the cause — merge `dev` and re-measure
/// before gating one.**
///
/// The lever survives its wrong conclusion because the question it answers is
/// permanent: a bisect over VM binaries costs ~15 minutes a step, and a bisect
/// over this costs one run. `native_site_cache_enabled()` is the coarse
/// on/off kill switch beside it; this is the shape selector.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SiteCacheMode {
    /// Every registered native the resolver can reach, including via the
    /// superclass walk — the 836631dcc behaviour, and **the default**.
    All,
    /// As `All`, but a native is only served when it is registered on the
    /// dispatch class ITSELF — the rule `invoke_or_native` actually applies
    /// (`find_with_kind(effective_class, …)`).
    NoSuperWalk,
    /// Leaf natives only — the behaviour this path shipped with before
    /// 836631dcc. The leaf claim is made per registration by someone who
    /// checked that the native may skip the funnel, so this is the narrowest
    /// mode that still has a fast path at all. No longer the default: see the
    /// correction on [`SiteCacheMode`].
    Leaf,
    /// No site caching at all; every call runs the generic dispatcher.
    Off,
}

impl SiteCacheMode {
    #[inline]
    fn walks_supers(self) -> bool {
        matches!(self, SiteCacheMode::All | SiteCacheMode::Leaf)
    }

    #[inline]
    fn admits_non_leaf(self) -> bool {
        matches!(self, SiteCacheMode::All | SiteCacheMode::NoSuperWalk)
    }
}

/// `CRATONVM_JIT_SITE_CACHE` — `leaf` (default), `all`, `no-super-walk`, `off`.
///
/// An unrecognised value is a hard error rather than a silent fall-back to the
/// default: a bisect lever that reads as "no effect" when it was misspelled is
/// worse than no lever, because "no effect" is exactly the answer it exists to
/// produce and the reader cannot tell the two apart.
///
/// Read per site resolution (cold: once per `(vm, call site)` and again only
/// when the registry generation moves), not latched: t1 wave 3 (lane Y)
/// removed the process `OnceLock`, so a per-VM or per-thread flag override is
/// honoured after the first resolution in the process.
fn site_cache_mode() -> SiteCacheMode {
    site_cache_mode_from(
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_SITE_CACHE")
            .ok()
            .as_deref(),
    )
}

/// The parse, split out from the `OnceLock` so it is testable: a mode latched
/// once per process cannot be exercised across values from a unit test.
fn site_cache_mode_from(raw: Option<&str>) -> SiteCacheMode {
    match raw {
        None => SiteCacheMode::All,
        Some(v) => match v.trim() {
            "" | "all" => SiteCacheMode::All,
            "leaf" => SiteCacheMode::Leaf,
            "no-super-walk" => SiteCacheMode::NoSuperWalk,
            "off" => SiteCacheMode::Off,
            other => panic!(
                "CRATONVM_JIT_SITE_CACHE={other:?} is not one of \
                 all / leaf / no-super-walk / off"
            ),
        },
    }
}

/// `CRATONVM_JIT_SITE_CACHE_STUBS=0` — refuse every `SyntheticStub` triple at
/// the site cache, as this path did before 2026-08-27, instead of arbitrating
/// it the way `invoke_or_native` does.
///
/// **Default ON** (arbitrate). The switch exists for the same reason every
/// other lever in this file does: a release build here is ~16 minutes, so the
/// only rigorous A/B is one binary against itself. Comparing against a
/// separately built branch confounds this change with everything else that
/// landed in between.
///
/// It is also the bisect lever for the one hazard the change carries. The
/// arbitration `resolve_native_site` now performs is asked ONCE per call site
/// and cached; `invoke_or_native` asks it on every dispatch. Both terms are
/// monotone in the safe direction — a class becomes loaded, a `Code` attribute
/// becomes decoded, never the reverse — so a cached "the native wins" cannot
/// go stale into "the bytecode should have won" while the process runs, and a
/// JVMTI redefine invalidates the entry through the `any_class_redefined`
/// check on the dispatch side. If a workload ever disagrees, this switch names
/// the change without a rebuild.
fn synthetic_stub_site_cache_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, 
        || match cratonvm_types::flags::runtime_var("CRATONVM_JIT_SITE_CACHE_STUBS") {
            Ok(v) => {
                let v = v.trim();
                !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            }
            Err(_) => true,
        },
    )
}

/// Does `invoke_or_native` special-case this method name BEFORE it reaches its
/// generic "find the registered native" step?
///
/// `vm_exec::invoke_or_native` opens with a cascade of hand-written gates —
/// `MethodHandle.type()`, the `invoke`/`invokeExact`/`invokeBasic` family, the
/// `AnnotationProxy` retarget, `ClassLoader`'s resource + `loadClass` bridges,
/// `setDefaultAssertionStatus`, the Spring Boot loader's jar shims, the
/// `ToIntFunction.apply` SAM bridges — each of which can route a call somewhere
/// other than the registry slot for its own triple. The leaf fast path skips
/// that entire cascade, so it must not be installed for a site the cascade
/// would have claimed.
///
/// The list is keyed on the **method name** alone, deliberately over-broad: it
/// is consulted once per call site at cache-fill time, and refusing one extra
/// name costs a fast path nobody has asked for while a missed name would be a
/// miscompile. Nothing in the leaf set (field reads, field writes, atomics,
/// `Math`, `System.nanoTime`, `Thread.currentThread`) is anywhere near it.
///
/// Pinned by `leaf_native_sites_avoid_invoke_or_native_special_cases` below: if
/// a future registration marks one of these names leaf, that test fails rather
/// than the VM silently taking a different call.
fn site_name_is_special_cased(method_name: &str) -> bool {
    matches!(
        method_name,
        "type"
            | "invoke"
            | "invokeExact"
            | "invokeBasic"
            | "apply"
            | "loadClass"
            | "setDefaultAssertionStatus"
            | "getResource"
            | "getResources"
            | "getResourceAsStream"
            | "isMultiRelease"
            | "getJarEntry"
            | "getRealName"
            | "getCertificates"
            | "getCodeSigners"
            | "getEntry"
            | "bufferEndsWithSignatureSuffix"
            | "<init>"
            | "<clinit>"
    )
}

/// `CRATONVM_JIT_SITE_CACHE_SPECIAL=0` — keep `invokespecial` sites out of the
/// native site cache, restoring the pre-r9w4 refusal. Default ON.
///
/// With it on, an `invokespecial` whose constant-pool triple has a registered
/// native — the `native_exc_init_*` shadows of `Throwable.<init>` and its JDK
/// subclasses' constructors are the hot case: every user exception's
/// `super(msg)` — is resolved once per site and served by the same site-cached
/// funnel call virtual sites use, instead of `jit_invoke_dispatch`'s whole
/// tail ending in `invoke_special_shared`'s by-name registry hash. The native
/// that runs is the one `invoke_special_shared` would have run; `--jdk-only`
/// admission still goes through `admit_jit_fast_native_resolved`. See
/// `docs/known-issues/jit/exception-subclass-constructor-super-call-is-never-bound-20260918.md`.
fn site_cache_special_enabled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_SITE_CACHE_SPECIAL").map_or(true, |v| {
            let v = v.trim();
            !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
        })
    })
}

/// Invoke kinds the native site cache resolves: virtual (0), interface (2) and
/// static (3) always; special (1) unless `CRATONVM_JIT_SITE_CACHE_SPECIAL=0`.
#[inline]
fn site_cache_admits_invoke_kind(invoke_kind: u8) -> bool {
    match invoke_kind {
        0 | 2 | 3 => true,
        1 => site_cache_special_enabled(),
        _ => false,
    }
}

/// Resolve a JIT call site to a leaf native, or refuse.
///
/// Cold: called once per `(vm, call site)` and again only when the native
/// registry's generation moves. Everything expensive — the registry hash, the
/// policy round trip, the class-manager read for a virtual receiver's name —
/// lives here and not on the dispatch path.
///
/// Refuses (returns `None`, cached as a negative) when any of these hold, and
/// each refusal is a semantic obligation the fast path could not otherwise
/// discharge:
///
/// * the invoke kind is `invokespecial` and `CRATONVM_JIT_SITE_CACHE_SPECIAL=0`.
///   With the switch on (the default) an `invokespecial` site resolves ONLY to
///   the native registered on the EXACT constant-pool triple — which is the
///   one native `invoke_special_shared` runs ahead of its declaring-class walk
///   (its "Native override always wins" step) — and to nothing reached by a
///   walk. The walk only ever ends in bytecode, which this cache never serves;
/// * the method name is one `invoke_or_native` special-cases (see above) — for
///   the virtual/static kinds only, since `invoke_special_shared` has no such
///   cascade;
/// * no native is registered for the triple, or its slot does not claim leaf;
/// * the slot is a `SyntheticStub` — those are subject to the
///   `real_protected_stub_class` / `has_real` yield-to-bytecode arbitration in
///   `invoke_or_native`, which this path does not reproduce;
/// * a capability policy is installed — `check_native_dispatch_capability` runs
///   at the funnel's dispatch site, and skipping the funnel would skip the gate;
/// * `admit_jit_fast_native_resolved` declines (`--jdk-only` §1.3).
fn resolve_native_site(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: Option<ClassId>,
) -> Option<NativeSiteCache> {
    let mode = site_cache_mode();
    if mode == SiteCacheMode::Off {
        return site_refusal::note(7);
    }
    if !site_cache_admits_invoke_kind(info.invoke_kind) {
        return site_refusal::note(0);
    }
    let special = info.invoke_kind == 1;
    // Round 13 wave 10 (lane callcost4): `MethodHandle.invoke` / `invokeExact`
    // on the VM's own carrier class are the one special-cased family this cache
    // serves (`method_handle_carrier_site`), decided below once the receiver's
    // class is known.
    // SBR3 wave 2 (lane mhperf): `invokeBasic` too, for a REAL handle receiver only
    // (the spun `LambdaForm` classes call it on every handle they run).
    let basic_candidate = info.invoke_kind == 0 && info.method_name == "invokeBasic";
    let carrier_candidate = basic_candidate
        || (info.invoke_kind == 0 && matches!(info.method_name, "invoke" | "invokeExact"));
    if !special && !carrier_candidate && site_name_is_special_cased(info.method_name) {
        return site_refusal::note(1);
    }
    // Static sites resolve on the constant-pool owner; virtual/interface sites
    // resolve on the receiver's runtime class, which is the only authority on
    // which override actually runs. An `invokespecial` site is statically bound
    // to the constant-pool owner too — `invoke_special_shared` probes the
    // native registry under `class_name` and never re-targets onto the
    // receiver — so it carries no receiver guard either.
    let (lookup_class, guard) = match info.invoke_kind {
        1 | 3 => (info.class_name.to_string(), None),
        _ => {
            let Some(cid) = receiver_class_id else {
                return site_refusal::note(2);
            };
            let Some(name) = vm
                .classes
                .class_manager
                .try_read()
                .and_then(|cm| cm.get_class(cid).map(|class| class.name.to_string()))
            else {
                return site_refusal::note(2);
            };
            (name, Some(cid.as_u32()))
        }
    };
    let mh_carrier = carrier_candidate
        && ((!basic_candidate && method_handle_carrier_site(&lookup_class))
            || real_method_handle_site(vm, &lookup_class, receiver_class_id));
    if carrier_candidate && !mh_carrier {
        return site_refusal::note(1);
    }
    // The capability gate (`vm_exec::check_native_dispatch_capability`) runs at
    // the funnel's dispatch site, so a path that skips the funnel must not skip
    // a gate that could have said no — or, in Permissive mode, could have
    // recorded an audit entry.
    //
    // The condition is `classify_native`, NOT "is a policy installed": a policy
    // is ALWAYS installed (`vm_init` calls `set_capabilities` unconditionally,
    // defaulting to Permissive, so that `capability_audit` can report), and an
    // earlier cut of this function gated on its mere presence — which made the
    // whole fast path inert in every configuration. That was caught by
    // `LEAF_NATIVE_HITS` reading 0 on a run whose ns/op had not moved, which is
    // exactly the failure mode that counter exists for; the timings alone said
    // "no faster", not "never ran".
    //
    // `classify_native` is a pure function of `(class, method)` and is the FIRST
    // thing the dispatch-site gate consults: when it answers `None`, that gate
    // returns `Ok(())` in every mode without touching the policy or the audit
    // log.
    //
    // THE REFUSAL BELOW WAS RIGHT WHEN IT WAS WRITTEN AND ITS REASON EXPIRED.
    //
    // It closed with "no leaf native is one — the classified set is process
    // spawn, library load, `Unsafe`, Panama, file and socket I/O, none of which
    // could satisfy the leaf contract in the first place". True while the cache
    // was leaf-only. 836631dcc widened it to EVERY registered native, and
    // `jdk/internal/misc/Unsafe` is `CapabilityKind::RawMemory`, so from that
    // commit the sentence stopped describing the code and the refusal became a
    // blanket exclusion of the hottest native on any CAS-driven workload.
    //
    // Measured on `HibfixComposeProbe2`, 40 000 chains,
    // `CRATONVM_DBG=dispatch-tally`: **99 428 of the 100 805 calls reaching
    // `invoke_or_native` are `Unsafe.compareAndSetInt` arriving from
    // `jit_invoke_virtual_mic`'s tail** — 2.49 per composition chain, each one
    // the full ~27-gate cascade and the three-string registry hash this cache
    // exists to remove. The site refusal census reported it as three refused
    // SITES, which is a number that cannot be read as a per-call cost, which is
    // why it sat there. See
    // `docs/feature-designs/jdk-only-mode.md`.
    //
    // The gate is not skipped: it is MOVED to the dispatch side, which is where
    // the funnel runs it too (`invoke_or_native`'s "CAPABILITY GATE, dispatch
    // site 1 of 3" — deliberately at the last point before the native runs, not
    // at resolution). The entry carries one `bool`, so every unclassified
    // native — the overwhelming majority — pays a not-taken branch, and a
    // classified one pays exactly what it pays through the funnel.
    //
    // `CRATONVM_JIT_SITE_CACHE_CAPABILITY=0` restores the refusal, so the
    // change can be priced and, if a capability question is ever raised
    // against it, switched off without a rebuild.
    // The KIND, not a bool, and derived from `lookup_class` — the receiver's
    // runtime class, which is what this site actually dispatches on. Carrying
    // the answer forward is the point: re-deriving it at dispatch would have to
    // use `info.class_name`, the constant-pool owner, and an unclassified owner
    // with a classified receiver (`InputStream` declared, `FileInputStream`
    // received) would silently lose the gate.
    let capability_kind =
        cratonvm_native_api::capability::classify_native(&lookup_class, info.method_name);
    if capability_kind.is_some() && !site_cache_capability_gate_enabled() {
        return site_refusal::note(3);
    }
    // The triple may be registered on the receiver's class OR inherited from a
    // superclass — `AbstractOwnableSynchronizer.setExclusiveOwnerThread` is
    // reached with a `ReentrantLock$NonfairSync` receiver, two levels down —
    // and `invoke_or_native` has a specific rule for that walk which has to be
    // reproduced, not approximated. See `resolve_native_owner_for_receiver`.
    //
    // `invokespecial` (r9w4 vm4, exception-subclass-constructor page option 1):
    // exactly the probe `invoke_special_shared_impl` makes before anything
    // else — `find(class_name, method, descriptor)`, the exact triple, no
    // superclass walk (a constructor is never inherited, and a `super.m()`
    // that is not registered on its CP owner ends in bytecode). `resolve_id`
    // and `find` share the prefilter, the exact slot and the descriptor-quirk
    // fallback, so the slot is the one `find` would have returned.
    // Round 15 wave 4 (lane walk2): the VM's redefine epoch BEFORE the walk
    // reads the owner's generation, and the ANCESTOR that owns the native with
    // that generation, so the hit can tell a later redefinition of the owner
    // (`native_site_owner_still_current`).
    let fill_redefine_epoch = vm.jit.jit_cache.redefine_epoch();
    let mut native_owner: Option<(ClassId, u32)> = None;
    let mut redefined_stop = false;
    let resolved_owner = if special {
        vm.natives
            .native_methods
            .resolve_id(&lookup_class, info.method_name, info.descriptor)
            .map(|id| (lookup_class.clone(), id, None))
    } else if mh_carrier {
        resolve_method_handle_carrier_native(vm, info)
    } else {
        resolve_native_owner_for_receiver(
            vm,
            &lookup_class,
            receiver_class_id,
            info,
            mode.walks_supers(),
            &mut native_owner,
            &mut redefined_stop,
        )
    };
    let Some((owner_class, id, poly_descriptor)) = resolved_owner else {
        if redefined_stop {
            site_refusal::note(12);
            return receiver_keyed_owner_refusal(vm, guard, info, fill_redefine_epoch);
        }
        return site_refusal::note(4);
    };
    // `invoke_special_shared_impl` lets a registered native yield to real
    // bytecode on exactly one condition, and asks it for EVERY kind of
    // registration (not only `SyntheticStub`): an object built by a stub
    // `<init>` while the rest of the class runs real bytecode is half
    // initialised. Asked once here, with the same arguments, so a site whose
    // native yields is never cached and keeps today's route.
    if special
        && crate::runtime::interpreter::synthetic_stub_should_yield_to_real_bytecode(
            vm,
            &owner_class,
            info.method_name,
            info.descriptor,
        )
    {
        return site_refusal::note(8);
    }
    // A signature-polymorphic entry was resolved under the REGISTRATION
    // descriptor, not the site's own, so every downstream question about the
    // registration (`--jdk-only` admission, and the `Debug`/census identity)
    // has to be asked with that one. `info.descriptor` stays the authority for
    // argument decode and for the return unboxing, which are call-site
    // properties.
    let registered_descriptor: &str = poly_descriptor.unwrap_or(info.descriptor);
    // `Thread.currentThread()` is served from the thread mirror instead of the
    // registered body — see `LeafNativeKind::ThreadCurrentThread` — so it is
    // recognised here rather than claimed at registration. Everything else
    // about the resolution is identical, deliberately: the site is only
    // installed if the native is registered AND policy admits it, so
    // `--jdk-only` and the capability gate above still decide.
    //
    // Everything that is NOT a leaf still gets an entry. It skips only
    // `invoke_or_native`, and still enters the full funnel through
    // `safe_native_call_prevalidated_objects` — see the `leaf` field's doc for
    // why that split is where the money is.
    let (kind, leaf) = if info.invoke_kind == 3
        && owner_class == "java/lang/Thread"
        && info.method_name == "currentThread"
        && info.descriptor == "()Ljava/lang/Thread;"
    {
        (LeafNativeKind::ThreadCurrentThread, true)
    } else {
        (
            LeafNativeKind::Callback,
            vm.natives.native_methods.is_leaf_id(id),
        )
    };
    if !leaf && !mode.admits_non_leaf() {
        return site_refusal::note(5);
    }
    // A `SyntheticStub` is not automatically a refusal — it is a QUESTION, and
    // this path used to refuse rather than ask it.
    //
    // `invoke_or_native` arbitrates a stub against the real bytecode with two
    // terms: `real_protected_stub_class(class)`, a twelve-entry allow-list, and
    // `has_real`, "the loaded class declares concrete non-native bytecode for
    // this triple". Only when BOTH hold does the bytecode win. For every other
    // stub the native wins and runs on every call — which is what the exact
    // census configuration says (`--nojit CRATONVM_DISABLE_INTRINSICS=1`):
    // `AbstractOwnableSynchronizer.setExclusiveOwnerThread` dispatches 399,999
    // times for 400,000 calls.
    //
    // Refusing here did not stop that native running. It only stopped it being
    // RESOLVED ONCE, so every call from compiled code fell to
    // `invoke_or_native`'s ~27-gate string cascade and three-string registry
    // hash — and that route, by its own comment, "is NOT counted for the §4
    // census", which is why `--dump-native-registry` reported 947 of those
    // 12,000,000 calls and the cost had no name in any census.
    //
    // MEASURED on this branch, `probes/OwnerCallProbe.java`, JIT arm, against
    // the identical two-level call shape on a class with no registration:
    //
    //     plain field store through two calls        17 ns/call
    //     setExclusiveOwnerThread                   458-898 ns/call
    //
    // The arbitration is a pure function of `(class, method, descriptor)` — no
    // per-call term — so asking it HERE, once per site at fill time, is exactly
    // equivalent to asking it on every dispatch, and the answer is cached with
    // the entry. `synthetic_stub_should_yield_to_real_bytecode` is the single
    // centralised predicate both dispatch paths already use; this is a third
    // caller of it, deliberately not a fourth copy of its five terms.
    //
    // `--jdk-only` is untouched: `admit_jit_fast_native_resolved` immediately
    // below routes every admission through `resolve_native_dispatch_wave1`,
    // whose §1.3 arm rejects a `SyntheticStub` outright. The refusal that
    // matters for policy is still there; what is gone is a refusal that only
    // ever cost throughput.
    //
    // `CRATONVM_JIT_SITE_CACHE_STUBS=0` restores the blanket refusal so one
    // binary can be A/B'd against its own pre-change behaviour.
    if vm.natives.native_methods.kind_of_id(id)
        == Some(cratonvm_native_api::NativeKind::SyntheticStub)
    {
        // Strict mode refuses here, exactly as it did before this change, and
        // deliberately WITHOUT asking the arbitration. §1.3 forbids invoking a
        // fake at all, so the yield question is moot — and asking it anyway
        // would route the site into `admit_jit_fast_native_resolved`'s strict
        // arm, which calls `record_jdk_only_fastpath_refusal` and grows the
        // bounded violation list `--jdk-only-report` prints. This early exit is
        // scoped to that one report: it keeps the fast-path refusal record from
        // gaining an entry for a decision that has not changed. It is NOT a
        // claim about the whole strict-mode census — `native-shadows-bytecode`
        // is recorded by the INTERPRETER's resolver (`resolve_dispatch_in`
        // step 3, bytecode won over a registered native), which this path does
        // not reach in either direction. (The "bridge ran over bytecode"
        // recorder this comment used to cite lost its last caller with
        // bytecode-first dispatch, `3d31008561`, and was removed in t1 wave 6,
        // lane Y3.)
        if crate::vm::dispatch_policy(vm).is_jdk_only() {
            return site_refusal::note(6);
        }
        if !synthetic_stub_site_cache_enabled() {
            return site_refusal::note(6);
        }
        if crate::runtime::interpreter::synthetic_stub_should_yield_to_real_bytecode(
            vm,
            &owner_class,
            info.method_name,
            registered_descriptor,
        ) {
            return site_refusal::note(8);
        }
    }
    let Some(callback) = vm.natives.native_methods.callback_of(id) else {
        return site_refusal::note(9);
    };
    // zt t29/jd (`CRATONVM_JIT_NATIVE_SITE_OWNER_BY_ID`): asked on the
    // site's loader-faithful owner, as the interpreter asks it on the class
    // the call resolved (`native_site_admission_owner`).
    let admission_owner = if mh_carrier || poly_descriptor.is_some() {
        None
    } else {
        native_site_admission_owner(
            vm,
            info,
            receiver_class_id,
            native_owner.map(|(owner, _)| owner),
            &owner_class,
        )
    };
    let Some((callback, native_id)) = admit_jit_fast_native_on_owner(
        vm,
        admission_owner,
        &owner_class,
        info.method_name,
        registered_descriptor,
        callback,
        Some(id),
    ) else {
        return site_refusal::note(9);
    };
    // JVMS §2.11.10: a native answering an `ACC_SYNCHRONIZED` method runs under
    // that method's monitor (`native_sync_enabled`). The entry carries the
    // fact — the per-slot memo the funnel itself reads — and the dispatch holds
    // the monitor around the call (interpreter round i1 wave 11, lane L5).
    // Until then such a site was refused (slot 10) and paid the funnel's
    // cascade on every call. Fill time only.
    let sync = crate::runtime::interpreter::native_sync_for_id(vm, id);
    // Round 15 wave 4 (lane walk3): name the one class whose redefinition can
    // stale this entry and judge the receiver's own native (or a static
    // site's) by the redefine rule now, so the hit need not decline forever
    // once that class, or the constant-pool class, was ever redefined. Not for
    // a carrier or the signature-polymorphic tail. Fill time only.
    //
    // Round 15 wave 5 (lane walk4; W3-4 of `jit-r15w4-walk3-proposals-RETIRED-20261003.md`):
    // `invokespecial` too. Its native is the exact constant-pool triple and
    // its owner the constant-pool class (`receiver_class_id` is `None` for
    // it, so the scope resolves that class as it does a static site's). The
    // entry is kept only where the rule keeps the native (never redefined,
    // bodyless, cache-path immune), and the compiled tail keeps it there too
    // (`special_site_runs_woven_body` drops it only for a woven owner outside
    // the forced immunity, a superset of the cache-path set). A refused site
    // takes that tail, as every declined hit did before.
    let owner_scoped = if !mh_carrier
        && poly_descriptor.is_none()
        && vm.natives.native_methods.jit_site_cache_owner_redefine()
    {
        match site_native_owner_scope(vm, info, receiver_class_id, &mut native_owner) {
            Some(scoped) => scoped,
            // The woven body wins, or the slow path keeps the native for some
            // receivers only: bytecode (or the funnel) decides (slot 12), for
            // this receiver only (round 16, lane redefine).
            None => {
                site_refusal::note(12);
                return receiver_keyed_owner_refusal(vm, guard, info, fill_redefine_epoch);
            }
        }
    } else {
        false
    };
    // Classified against the ADMITTED callback, after every policy gate above
    // has already said yes to running this native from here.
    let kind = if kind == LeafNativeKind::Callback {
        collection_leaf_kind(info, callback, poly_descriptor.is_some())
    } else {
        kind
    };
    Some(NativeSiteCache {
        kind,
        leaf,
        callback,
        native_id,
        receiver_class_id: guard,
        capability_kind,
        poly: poly_descriptor.is_some(),
        // Never for a `MethodHandle` carrier entry: its native takes the
        // call-site descriptor instead (armed at dispatch), and a `VarHandle`
        // value cell left armed across the Java its target runs would be read
        // by the next store-mode access as its own.
        // Round 15 wave 3 (lane vh3; VH1-3): the whole parameter-site word,
        // so an access-mode native can judge the COORDINATES too; its low
        // half is the old value-site cell.
        // Wave 57 (lane L4): with the site's primitive return in the top
        // byte, which the access natives judge the variable's type against.
        vh_value_site: if poly_descriptor.is_some() && !mh_carrier {
            cratonvm_native_api::poly_call_site::vh_param_site_word(info.descriptor)
        } else {
            0
        },
        // Decoded HERE, on the one cold path that builds an entry, so the
        // dispatch never parses this descriptor again. See
        // `NativeSiteDescriptor`.
        desc: NativeSiteDescriptor::decode(info.descriptor),
        sync,
        native_owner,
        fill_redefine_epoch,
        owner_scoped,
        refused_for_receiver: false,
    })
}

/// A slot-12 refusal (a redefined owner) of `resolve_native_site`, already
/// counted: keyed by the receiver class `guard` of a virtual/interface site
/// (`NativeSiteCache::refused_for_receiver`) under
/// `NativeMethodRegistry::jit_site_cache_owner_redefine`; otherwise (a static
/// or `invokespecial` site, whose refusal is receiver-independent, or the
/// switch off) the site-wide refusal, as before. Round 16, lane redefine
/// (backlog sweepB S4). Fill time only.
fn receiver_keyed_owner_refusal(
    vm: &SharedVm,
    guard: Option<u32>,
    info: &JitInvokeInfo,
    fill_redefine_epoch: u32,
) -> Option<NativeSiteCache> {
    let guard = guard.filter(|_| vm.natives.native_methods.jit_site_cache_owner_redefine())?;
    Some(NativeSiteCache::refusal_for_receiver(
        guard,
        info,
        fill_redefine_epoch,
    ))
}

/// JIT round 19 wave 6 (lane mega6; FUNNEL3-19-1,
/// `CRATONVM_JIT_NATIVE_SITE_REFUSAL_BY_RECEIVER`, default OFF): what the
/// site-cache door caches for a fresh resolution `resolved` of a site whose
/// receiver class is `receiver_class_id`.
///
/// A refusal (`None`) of a virtual/interface site is otherwise site-wide, and
/// the door answers it BEFORE it knows the receiver: a site whose first
/// receiver was a bytecode class (`String.equals`) never served a later
/// receiver whose method is a registered native (`Integer.equals`). Under the
/// switch the refusal is cached as the receiver-keyed placeholder the
/// redefined-owner rule already uses (`NativeSiteCache::refusal_for_receiver`),
/// so the hit machinery that exists for that rule applies unchanged:
///
/// * the refusing class declines on every call (`refused_for_receiver`);
/// * the first call with another class re-resolves the site once
///   (`native_site_refill_for_receiver`), and caches that class's answer;
/// * a second refusal becomes the site-wide refusal
///   (`native_site_refill_outcome`), so a polymorphic all-bytecode site costs
///   at most one extra resolution per registry generation.
///
/// With `CRATONVM_JIT_NATIVE_SITE_DOOR_TWO_WAY` the first (refusing) class
/// then reaches the second-receiver memo after the native class took the
/// site's entry; without it that class declines on the receiver guard, the
/// same answer.
///
/// The cost is on the refusing class: the door no longer answers it before
/// the receiver validation (one membership walk and a class-id read per
/// helper call, ~3-5 ns on a ~100+ ns helper path), until a second class makes
/// the refusal site-wide. A positive entry, a static or `invokespecial` site
/// and a missing receiver class keep their answer. Fill time only.
fn native_site_refusal_keyed_by_receiver(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: Option<ClassId>,
    resolved: Option<NativeSiteCache>,
) -> Option<NativeSiteCache> {
    if resolved.is_some()
        || !matches!(info.invoke_kind, 0 | 2)
        || !vm
            .natives
            .native_methods
            .jit_native_site_refusal_by_receiver()
    {
        return resolved;
    }
    let guard = receiver_class_id?.as_u32();
    Some(NativeSiteCache::refusal_for_receiver(
        guard,
        info,
        vm.jit.jit_cache.redefine_epoch(),
    ))
}

/// JIT round 19 wave 6, lane mega6: FUNNEL3-19-1 and FUNNEL3-19-2.
#[cfg(test)]
mod r19_mega6_native_door_tests {
    use super::*;

    fn info(kind: u8) -> JitInvokeInfo {
        JitInvokeInfo {
            class_name: "java/lang/Object",
            method_name: "equals",
            descriptor: "(Ljava/lang/Object;)Z",
            num_jit_args: 2,
            return_type: b'Z',
            invoke_kind: kind,
            declaring_class_id: 0,
            owner_class_id: 0,
        }
    }

    /// FUNNEL3-19-1: the keyed refusal is exactly the redefined-owner rule's
    /// placeholder for the refusing class, made only for a virtual/interface
    /// refusal with a receiver class and only under the switch; everything
    /// else is cached as resolved.
    #[test]
    fn a_refusal_is_receiver_keyed_only_under_the_switch() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let on = shared
            .natives
            .native_methods
            .jit_native_site_refusal_by_receiver();
        let cid = Some(ClassId::new(41));
        for kind in [0u8, 2] {
            let keyed = native_site_refusal_keyed_by_receiver(&shared, &info(kind), cid, None);
            assert_eq!(keyed.is_some(), on, "kind {kind}");
            if let Some(entry) = keyed {
                assert!(entry.refused_for_receiver);
                assert_eq!(entry.receiver_class_id, Some(41));
                // The hit machinery: the refusing class declines, another
                // class re-resolves once, a second refusal is site-wide.
                assert!(!native_site_refill_for_receiver(Some(&entry), Some(41)));
                assert!(native_site_refill_for_receiver(Some(&entry), Some(42)));
                assert!(native_site_refill_outcome(Some(entry)).is_none());
            }
            // No receiver class: nothing to key on.
            assert!(
                native_site_refusal_keyed_by_receiver(&shared, &info(kind), None, None).is_none()
            );
        }
        // Static and `invokespecial` refusals stay site-wide in both arms.
        for kind in [1u8, 3] {
            assert!(
                native_site_refusal_keyed_by_receiver(&shared, &info(kind), cid, None).is_none()
            );
        }
        // A positive resolution is never replaced.
        let positive = NativeSiteCache {
            refused_for_receiver: false,
            ..NativeSiteCache::refusal_for_receiver(41, &info(0), 0)
        };
        let kept = native_site_refusal_keyed_by_receiver(&shared, &info(0), cid, Some(positive));
        assert!(kept.is_some_and(|e| !e.refused_for_receiver));
    }

    /// FUNNEL3-19-2: the site-cache door hands a validated argument only on a
    /// warm call, for a reference-taking kind, from a set mask bit, and only
    /// under the switch.
    #[test]
    fn the_site_door_hands_a_validated_argument_only_when_it_may() {
        let shared = SharedVm::new(crate::config::VmConfig::default());
        let on = shared
            .natives
            .native_methods
            .jit_leaf_probe_validated_args();
        let args = [0x1000_0000_i64, 0x2000_0000_i64];
        let chm = LeafNativeKind::ChmGet;
        assert_eq!(
            site_leaf_probe_validated_arg(&shared, chm, true, 0b10, &args).is_some(),
            on
        );
        assert!(site_leaf_probe_validated_arg(&shared, chm, false, 0b10, &args).is_none());
        assert!(site_leaf_probe_validated_arg(&shared, chm, true, 0b01, &args).is_none());
        assert!(site_leaf_probe_validated_arg(&shared, chm, true, 0, &args).is_none());
        assert!(site_leaf_probe_validated_arg(&shared, chm, true, 0b10, &args[..1]).is_none());
        assert!(site_leaf_probe_validated_arg(
            &shared,
            LeafNativeKind::ArrayListGet,
            true,
            0b10,
            &args
        )
        .is_none());
    }
}

/// Which class's registered native `invoke_or_native` would actually dispatch
/// for this site, or `None` if it would run bytecode instead.
///
/// This reproduces the tail of `vm_exec::invoke_or_native` — the part after its
/// hand-written gate cascade — and it must stay faithful to it, because the
/// site cache's whole point is not to run that function per call:
///
///  1. a native registered on the dispatch class itself wins outright;
///  2. otherwise, if the dispatch class declares its OWN bytecode for the
///     method, that bytecode wins and there is no fast path (the S107
///     collection-`toString` rule);
///  3. otherwise walk the superclass chain. The first parent that declares the
///     method in bytecode ends the walk — **unless that same parent also has a
///     registered native, in which case the native wins** (the round-19
///     LinkedHashMap-overlay rule). A parent with a native and no bytecode also
///     wins.
///
/// Rule 3's exception is the one that matters here:
/// `AbstractOwnableSynchronizer` has both a real `setExclusiveOwnerThread` body
/// and a registered native, and the native is the one that runs — which is why
/// resolving on the receiver class alone (`ReentrantLock$NonfairSync`) found
/// nothing and refused the site.
///
/// # The walk must start from the receiver's `ClassId`, never from its name
///
/// `dispatch_class` is only a NAME, and a name does not identify a class once
/// more than one loader has defined it — `get_loaded_class_id(name)` then
/// answers with whichever one the global table happens to hold.
/// `invoke_or_native` says this in as many words at its own tail ("A virtual
/// call's receiver IS the authoritative answer"), and this test class is the
/// everyday case: `FilteredClassLoader` gives `autoconfigurationBacksOffEntirely
/// IfSpringMongoDbAbsent` a second, child-first definition of classes the other
/// twelve tests already loaded through the app loader. Resolving the walk
/// against the other loader's copy reads ANOTHER class's method table, so rules
/// 2 and 3 answer about a class the receiver is not an instance of — and the
/// entry that installs is then guarded by the REAL receiver's class id, so it
/// keeps firing. `receiver_class_id` is passed in for exactly this reason and
/// the name is used only for the registry lookups, which are name-keyed by
/// construction.
///
/// # Rule 4 — the signature-polymorphic tail
///
/// Rules 1-3 all look the native up under the CALL SITE's descriptor. A
/// `VarHandle` accessor never has one: `VH.get(holder)` is emitted as
/// `invokevirtual java/lang/invoke/VarHandle.get:(LHolder;)I`, while the
/// native is registered under `([Ljava/lang/Object;)Ljava/lang/Object;`. So
/// every one of those sites used to end here as refusal reason 4 ("no native
/// for the triple") and pay `invoke_or_native`'s full cascade on every call —
/// which is what made `VarHandle.get` cost **2.03 µs** against a 3 ns plain
/// field read, and, through `AbstractByteBuf.ensureAccessible()` ->
/// `RefCnt.isLiveNonVolatile` -> `VH.get`, made every netty `ByteBuf`
/// accessor cost ~2.6 µs. See
/// `varhandle-signature-polymorphic-dispatch-FIXED-20260817.md`.
///
/// [`vm_exec::invoke_on_class_shared_inner`] already handles the shape, in the
/// `None` arm of its hierarchy resolution — i.e. exactly where rules 1-3
/// arrive with nothing. This reproduces that arm's lookup order (base class
/// first, then the exact receiver class) using the same shared descriptor
/// list, and reports `poly = true` so the dispatch side runs
/// `unbox_poly_return_checked` rather than `coerce_native_return`: the erased
/// `Object` return has to be unboxed against the call site's own descriptor.
///
/// `MethodHandle.invoke`/`invokeExact`/`invokeBasic` do NOT reach here —
/// `site_name_is_special_cased` refuses them one level up, and this path
/// deliberately does not widen that.
///
/// Cold: fill time only.
///
/// `ancestor_owner` receives the ANCESTOR the walk found the native on, with
/// its redefine generation read under the walk's guard (round 15 wave 4, lane
/// walk2), for `NativeSiteCache::native_owner`; it stays `None` for rule 1
/// and rule 4. `redefined_stop` is set when the shared walk answered
/// `Stopped` (a redefined ancestor), so the caller counts the refusal in
/// `site_refusal` slot 12 rather than slot 4.
fn resolve_native_owner_for_receiver(
    vm: &SharedVm,
    dispatch_class: &str,
    receiver_class_id: Option<ClassId>,
    info: &JitInvokeInfo,
    walk_supers: bool,
    ancestor_owner: &mut Option<(ClassId, u32)>,
    redefined_stop: &mut bool,
) -> Option<(
    String,
    cratonvm_native_api::NativeMethodId,
    Option<&'static str>,
)> {
    let registry = &vm.natives.native_methods;
    if let Some(id) = registry.resolve_id(dispatch_class, info.method_name, info.descriptor) {
        return Some((dispatch_class.to_string(), id, None));
    }
    if !walk_supers {
        // `invoke_or_native` looks the native up on `effective_class` and
        // nothing else — rule 1 above and no more. The walk below is an
        // *approximation* of what the rest of the dispatch pipeline does, so
        // `CRATONVM_JIT_SITE_CACHE=no-super-walk` restricts this resolver to
        // the part that is a literal reproduction.
        return None;
    }
    // `<init>` is never inherited; the walk below would be wrong for it. It is
    // already refused by `site_name_is_special_cased`, but state it here
    // too so this function is correct in isolation.
    if info.method_name == "<init>" {
        return None;
    }
    let cm = vm.classes.class_manager.try_read()?;
    let mut cid = match receiver_class_id {
        Some(cid) => cid,
        None => cm.get_loaded_class_id(dispatch_class)?,
    };
    if cm
        .get_class(cid)
        .is_some_and(|c| c.find_method(info.method_name, info.descriptor).is_some())
    {
        // Rule 2: the dispatch class's own bytecode wins.
        return None;
    }
    let walk_start = cid;
    // SH14-1 (round 15 wave 2, lane jdkonly): the shared walk, with no
    // acceptance term -- rules 2 and 3 and the mask, as below. The redefine
    // guard is the one rule (round 15 wave 3, lane walk:
    // `redefined_ancestor_guard`; the copy below has none). A redefined
    // ancestor whose native the slow path keeps for some receivers only ends
    // the walk (`Stopped`): the site is refused and the funnel decides per
    // call. `CRATONVM_ANCESTOR_WALK_HELPER=0` runs the copy.
    if registry.ancestor_walk_helper() {
        use crate::runtime::interpreter::AncestorNative;
        match crate::runtime::interpreter::ancestor_native(
            vm,
            &cm,
            (walk_start, None),
            info.method_name,
            info.descriptor,
            |parent_id, parent| {
                jit_site_cache_redefine_guard(vm, &cm, parent_id, &parent.name, info)
            },
            |_, _, _| true,
        ) {
            AncestorNative::Native {
                owner,
                owner_name,
                id,
                ..
            } => {
                *ancestor_owner = Some((owner, cm.class_redefine_generation(owner)));
                return Some((owner_name.to_string(), id, None));
            }
            AncestorNative::Bytecode | AncestorNative::Unresolved => return None,
            // Counted in its own slot (12) by the caller: a redefined
            // ancestor, not a missing registration (round 15 wave 5, walk4).
            AncestorNative::Stopped => {
                *redefined_stop = true;
                return None;
            }
            AncestorNative::ChainEnd => {}
        }
        drop(cm);
        return resolve_signature_polymorphic_native_site(vm, dispatch_class, info);
    }
    while let Some(parent_id) = cm.get_class(cid).and_then(|c| c.superclass) {
        let parent = cm.get_class(parent_id)?;
        // `--jdk-only`: a `Bridge` above a retired row is masked, as in every
        // interpreter walk (round 14 wave 2, lane shadow).
        let parent_native = registry
            .resolve_id(&parent.name, info.method_name, info.descriptor)
            .filter(|id| {
                !crate::runtime::interpreter::retired_row_masks_ancestor_bridge(
                    vm,
                    &cm,
                    (walk_start, None),
                    parent_id,
                    info.method_name,
                    info.descriptor,
                    registry
                        .kind_of_id(*id)
                        .unwrap_or(cratonvm_native_api::NativeKind::Bridge),
                )
            });
        if parent
            .find_method(info.method_name, info.descriptor)
            .is_some()
        {
            // Rule 3: bytecode here ends the walk, and only a native declared
            // on THIS parent may override it.
            if parent_native.is_some() {
                *ancestor_owner = Some((parent_id, cm.class_redefine_generation(parent_id)));
            }
            return parent_native.map(|id| (parent.name.to_string(), id, None));
        }
        if let Some(id) = parent_native {
            *ancestor_owner = Some((parent_id, cm.class_redefine_generation(parent_id)));
            return Some((parent.name.to_string(), id, None));
        }
        cid = parent_id;
    }
    drop(cm);
    // Rule 4 — the signature-polymorphic tail (see this function's doc).
    resolve_signature_polymorphic_native_site(vm, dispatch_class, info)
}

/// The class a statically bound (`invokestatic` / `invokespecial`) compiled
/// site means by its constant-pool owner, by the CALLER's loader, or `None`
/// when the caller's loader has no recorded answer (the caller then falls
/// back to the loader-blind name table). The caller itself when it names its
/// own class. Pure lookup, never loads.
///
/// Round 15 wave 5 (lane walk4): the class the CALLER's loader resolves the
/// name to, as the compiled tails resolve it (`resolve_class_loader_aware`),
/// before the loader-blind name table, which answers another loader's class
/// of that name first (a mocked class in an isolated test loader). Split out
/// of [`site_native_owner_scope`] in zt t29/jd (item 3 of
/// `zgc-zt27jd1-compiled-static-call-decides-on-an-unloaded-owner-FIXED-20261024.md`)
/// so the fill's native-vs-bytecode question asks the same class.
fn static_site_owner_by_caller(
    cm: &crate::classloading::ClassManager,
    info: &JitInvokeInfo,
) -> Option<ClassId> {
    let candidate = ClassId::new(info.declaring_class_id);
    if cm
        .get_class(candidate)
        .is_some_and(|class| class.name.as_ref() == info.class_name)
    {
        return Some(candidate);
    }
    (info.declaring_class_id != 0)
        .then(|| cm.find_cp_class_for_class(info.class_name, candidate))
        .flatten()
}

/// zt t29/jd (item 3 of
/// `zgc-zt27jd1-compiled-static-call-decides-on-an-unloaded-owner-FIXED-20261024.md`,
/// `CRATONVM_JIT_NATIVE_SITE_OWNER_BY_ID`, default ON, `=0` kill; read once
/// per VM, `SharedVm::jit_native_site_owner_by_id`): the class, by id, on
/// which the native site cache's fill asks whether the native registered as
/// `owner_class` yields to bytecode -- the site's loader-faithful owner, not
/// the loader-blind name table's answer (`registered_native_yields_to_loaded_bytecode`
/// starts at `get_loaded_class_id`). With two loaders defining `owner_class`
/// the name table can answer the other loader's copy: a copy whose method is
/// `native` keeps the registered native where the interpreter, deciding on
/// the class the call resolved, runs the caller's copy's Java body; and the
/// cached entry keeps serving it.
///
/// * a static or special site: the caller's loader's answer for the
///   constant-pool owner ([`static_site_owner_by_caller`]);
/// * a virtual or interface site: the ancestor the walk found the native on
///   (`ancestor`), else the receiver's own class when the native is
///   registered on its name (rule 1).
///
/// Only an id whose class is named `owner_class` is returned, so `None`
/// (by name, as before) whenever the two cannot be the same registration:
/// a carrier or signature-polymorphic entry, a hidden caller's self
/// reference, a caller whose loader has no recorded answer. Fill time only.
fn native_site_admission_owner(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: Option<ClassId>,
    ancestor: Option<ClassId>,
    owner_class: &str,
) -> Option<ClassId> {
    if !vm.jit_native_site_owner_by_id {
        return None;
    }
    let cm = vm.classes.class_manager.try_read()?;
    let owner = match info.invoke_kind {
        1 | 3 => static_site_owner_by_caller(&cm, info)?,
        _ => ancestor.or(receiver_class_id)?,
    };
    native_site_owner_named(&cm, owner, owner_class)
}

/// `owner` when its class is named `owner_class`, else `None`: the check that
/// keeps [`native_site_admission_owner`] from deciding on a class the
/// registration does not name.
fn native_site_owner_named(
    cm: &crate::classloading::ClassManager,
    owner: ClassId,
    owner_class: &str,
) -> Option<ClassId> {
    cm.get_class(owner)
        .is_some_and(|class| class.name.as_ref() == owner_class)
        .then_some(owner)
}

/// zt t29/jd (item 3 of
/// `zgc-zt27jd1-compiled-static-call-decides-on-an-unloaded-owner-FIXED-20261024.md`):
/// the native site cache's fill decides native vs bytecode on the site's
/// loader-faithful owner, by id.
#[cfg(test)]
mod zt29_jd_native_site_owner_tests {
    use super::*;
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    use cratonvm_reader::method::ClassFileMethod;
    use cratonvm_types::ClassLoaderId;
    use std::sync::Arc;

    const OWNER: &str = "cratonvm/zt29jd/SplitNativeOwner";
    const FORK: ClassLoaderId = ClassLoaderId::UserDefined(29_029);

    fn noop(
        _: &mut dyn crate::native::registry::NativeContext,
        _: &[Value],
    ) -> crate::error::MethodCallResult {
        Ok(None)
    }

    /// `static native m()V`: no `Code`, as the JDK's own declaration.
    fn native_m() -> ClassFileMethod {
        ClassFileMethod {
            name: Arc::from("m"),
            descriptor: Arc::from("()V"),
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            attributes: vec![],
        }
    }

    /// `static m()V { return; }`: the isolated loader's copy's Java body.
    fn bytecode_m() -> ClassFileMethod {
        ClassFileMethod {
            name: Arc::from("m"),
            descriptor: Arc::from("()V"),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(vec![0xb1]),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }
    }

    fn info(kind: u8, caller: ClassId) -> JitInvokeInfo {
        JitInvokeInfo {
            class_name: OWNER,
            method_name: "m",
            descriptor: "()V",
            num_jit_args: 0,
            return_type: b'V',
            invoke_kind: kind,
            declaring_class_id: caller.as_u32(),
            owner_class_id: 0,
        }
    }

    /// A `Bridge` on `OWNER.m()V`; the application loader's `OWNER` (what
    /// the loader-blind name table answers) declares `m` native, an isolated
    /// loader's `OWNER` gives it a Java body. Built as
    /// `a_compiled_invokestatic_resolves_its_owner_in_the_callers_own_loader`
    /// builds its two copies. Returns `(vm, app_copy, fork_copy, fork_caller,
    /// app_caller)`.
    fn two_copies() -> (SharedVm, ClassId, ClassId, ClassId, ClassId) {
        let mut vm = SharedVm::new(crate::config::VmConfig::default());
        vm.natives.native_methods.register_with_kind(
            OWNER,
            "m",
            "()V",
            noop,
            cratonvm_native_api::NativeKind::Bridge,
        );
        let ids = {
            let mut cm = vm.classes.class_manager.write();
            let app_copy = cm
                .try_ensure_synthetic_class(OWNER, 0)
                .expect("Compatible mode fabricates");
            cm.class_store
                .get_mut(app_copy)
                .expect("fabricated")
                .methods
                .push(native_m());
            let fork_copy = cm
                .try_ensure_synthetic_class("cratonvm/zt29jd/SplitNativeOwner$Fork", 0)
                .expect("Compatible mode fabricates");
            {
                let c = cm.class_store.get_mut(fork_copy).expect("fabricated");
                c.name = cratonvm_types::intern_arc(OWNER);
                c.loader_id = FORK;
                c.methods.push(bytecode_m());
            }
            cm.register_class_name(FORK, OWNER, fork_copy);
            let fork_caller = cm
                .try_ensure_synthetic_class("cratonvm/zt29jd/ForkCaller", 0)
                .expect("Compatible mode fabricates");
            cm.class_store
                .get_mut(fork_caller)
                .expect("fabricated")
                .loader_id = FORK;
            cm.register_class_name(FORK, "cratonvm/zt29jd/ForkCaller", fork_caller);
            let app_caller = cm
                .try_ensure_synthetic_class("cratonvm/zt29jd/AppCaller", 0)
                .expect("Compatible mode fabricates");
            (app_copy, fork_copy, fork_caller, app_caller)
        };
        assert_eq!(
            vm.classes.class_manager.read().get_loaded_class_id(OWNER),
            Some(ids.0),
            "the name table must answer the application copy, or this measures nothing"
        );
        (vm, ids.0, ids.1, ids.2, ids.3)
    }

    fn admitted(vm: &SharedVm, owner: Option<ClassId>) -> bool {
        let id = vm.natives.native_methods.resolve_id(OWNER, "m", "()V");
        assert!(id.is_some(), "the Bridge is registered");
        admit_jit_fast_native_on_owner(vm, owner, OWNER, "m", "()V", noop, id).is_some()
    }

    /// The resolver question itself: on the isolated copy (Java body) the
    /// `Bridge` yields; by name (the application copy, `native`) it runs.
    #[test]
    fn the_bridge_is_decided_on_the_owner_it_is_given() {
        let (vm, app_copy, fork_copy, _, _) = two_copies();
        assert!(!admitted(&vm, Some(fork_copy)), "the fork's Java body wins");
        assert!(
            admitted(&vm, Some(app_copy)),
            "the native declaration keeps it"
        );
        assert!(admitted(&vm, None), "by name: the application copy");
    }

    /// The owner a static or special site asks on: the caller's loader's
    /// copy, so a site inside the isolated loader yields where the
    /// interpreter runs the copy's body; a caller of the application loader
    /// decides as by name.
    #[test]
    fn a_static_site_asks_on_the_callers_loaders_copy() {
        let (vm, app_copy, fork_copy, fork_caller, app_caller) = two_copies();
        for kind in [1u8, 3] {
            let owner =
                native_site_admission_owner(&vm, &info(kind, fork_caller), None, None, OWNER);
            assert_eq!(owner, Some(fork_copy), "kind {kind}");
            assert!(!admitted(&vm, owner), "kind {kind}");
            let owner =
                native_site_admission_owner(&vm, &info(kind, app_caller), None, None, OWNER);
            assert_ne!(owner, Some(fork_copy), "kind {kind}");
            assert!(owner.is_none() || owner == Some(app_copy), "kind {kind}");
            assert!(admitted(&vm, owner), "kind {kind}");
        }
    }

    /// A virtual site: the walk's ancestor, else the receiver's own class; an
    /// id whose class is not named by the registration is never asked on.
    #[test]
    fn a_virtual_site_asks_on_its_receiver_or_ancestor_named_by_the_registration() {
        let (vm, _, fork_copy, fork_caller, app_caller) = two_copies();
        let site = info(0, app_caller);
        assert_eq!(
            native_site_admission_owner(&vm, &site, Some(fork_copy), None, OWNER),
            Some(fork_copy)
        );
        assert_eq!(
            native_site_admission_owner(&vm, &site, Some(app_caller), Some(fork_copy), OWNER),
            Some(fork_copy)
        );
        assert_eq!(
            native_site_admission_owner(&vm, &site, Some(fork_caller), None, OWNER),
            None,
            "a receiver of another name decides by name"
        );
        assert_eq!(
            native_site_admission_owner(&vm, &site, None, None, OWNER),
            None
        );
    }

    /// `CRATONVM_JIT_NATIVE_SITE_OWNER_BY_ID=0`: no owner, by name as before
    /// (the fork's site keeps the application copy's native).
    #[test]
    fn the_kill_switch_decides_by_name() {
        let (mut vm, _, _, fork_caller, _) = two_copies();
        vm.jit_native_site_owner_by_id = false;
        let owner = native_site_admission_owner(&vm, &info(3, fork_caller), None, None, OWNER);
        assert_eq!(owner, None);
        assert!(admitted(&vm, owner));
    }

    /// Text pin: the fill passes the owner to the admission.
    #[test]
    fn the_fill_admits_on_the_site_owner() {
        let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
        let fill = src
            .find(concat!("\nfn resolve_native", "_site("))
            .expect("the fill");
        let body = &src[fill..];
        let body = &body[..body.find("\n\u{7d}\n").expect("its end")];
        assert!(body.contains(concat!("native_site_admission", "_owner(")));
        assert!(body.contains(concat!("admit_jit_fast_native", "_on_owner(")));
    }
}

/// Round 15 wave 4 (lane walk3;
/// `r15w4-walk2-jit-site-cache-declines-forever-after-a-redefinition-FIXED-20261001.md`,
/// W2-1/W2-2 of `jit-r15w4-walk2-proposals-RETIRED-20261003.md`): may the native site-cache
/// entry being filled be judged by ONE class's redefinitions
/// (`NativeSiteCache::owner_scoped`)? `Some(true)`: yes, and `native_owner`
/// names that class with its generation; `Some(false)`: the hit keeps its
/// permanent tests; `None`: refuse the site (the owner's woven body wins, or
/// the slow path keeps the native for some receivers only).
///
/// The owner is the class the funnel's redefine probe reads
/// (`invoke_or_native`: "Resolved ONCE, against the exact declaring class"):
/// where `find_method_recursive` lands from the receiver's class, or for a
/// static or (round 15 wave 5, lane walk4) `invokespecial` site from the
/// constant-pool class, chosen as `site_class_was_redefined` chooses it; for
/// `invokespecial` that is also the class whose redefinition
/// `special_site_runs_woven_body` asks about. A redefinition adds no method
/// (`RedefinitionRefusal::MethodAdded`), so the classes below the landing
/// keep declaring nothing and only the landing's redefinition can change what
/// the funnel runs.
///
/// * An ancestor's native (`native_owner` already set by the walk): scoped
///   when the probe lands on that ancestor (the walk applied the rule to it),
///   or on nothing (no bytecode to weave). Since round 16 (lane redefine) a
///   landing elsewhere (a declaration the registering ancestor inherits, or
///   an interface default method) is judged like the next case; an abstract
///   one keeps the permanent tests.
/// * The start class's own native (rule 1): scoped iff the probe lands on a
///   non-abstract declaration (the start class's own, or since round 16 one
///   it inherits). The owner is the LANDING, judged with the registering
///   class's immunity, as the funnel's probe judges it, and the one redefine rule
///   (`redefined_ancestor_guard`, door `JitSiteCache`) is applied to it now:
///   kept when it was never redefined, the declaration is bodyless or the
///   native is in the cache-path immunity (each a case in which the funnel
///   keeps the native too: its drop needs a body, a generation and no
///   forced immunity, a superset of the cache-path set); refused otherwise,
///   as the populate refuses it (`receiver_redefined`). Before, the entry was
///   kept and every hit declined, so the funnel decided per call.
///
/// Cold: fill time only. Takes the class manager with `try_read`; a writer
/// answers `Some(false)` (the permanent tests, as before).
fn site_native_owner_scope(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: Option<ClassId>,
    native_owner: &mut Option<(ClassId, u32)>,
) -> Option<bool> {
    let Some(cm) = vm.classes.class_manager.try_read() else {
        return Some(false);
    };
    let start = match receiver_class_id {
        Some(cid) => cid,
        None => match static_site_owner_by_caller(&cm, info)
            .or_else(|| cm.get_loaded_class_id(info.class_name))
        {
            Some(cid) => cid,
            None => return Some(false),
        },
    };
    // The funnel probe's lenient walk (`find_method_recursive` from `start`),
    // asked through `MemberResolver` as the metadata-table guard requires
    // (`runtime::resolve::guard`; round 15 wave 5, lane walk4): the same
    // answer, memoized per VM in its `LinkResolver`. A redefinition cannot
    // move it (it adds no method and changes no modifier).
    let resolver = crate::runtime::resolve::MemberResolver::new(vm);
    let landing = resolver
        .declared_method(
            &cm,
            resolver.scope(start),
            info.method_name,
            info.descriptor,
        )
        .and_then(|scoped| resolver.adopt(scoped))
        .ok()
        .and_then(|(declaring, index)| {
            let method = cm
                .class_store()
                .get(declaring)?
                .methods
                .get(index as usize)?;
            Some((declaring, method.is_abstract()))
        });
    // The class whose registration serves the site: the walk's ancestor, or
    // the start class (rule 1).
    let registering = native_owner.map_or(start, |(owner, _)| owner);
    let Some((declaring, is_abstract)) = landing else {
        // Declared nowhere: nothing to weave. An ancestor's native keeps its
        // owner; the start class's own native keeps the permanent tests.
        return Some(native_owner.is_some());
    };
    if native_owner.is_some_and(|(owner, _)| owner == declaring) {
        // The walk applied the rule to that owner.
        return Some(true);
    }
    if is_abstract {
        return Some(false);
    }
    // Round 16 (lane redefine): the owner is the LANDING, not the class that
    // registers the native. A native registered on `R` (the start class, or
    // the walk's ancestor) for a method `R` inherits from `A` is judged by the
    // funnel's probe on `A`'s generation and body (the HotSpot reading, S1 of
    // `r15w6-sweepB-review-findings-CLOSED-20260930.md`): a weave of `A.m`
    // runs `A`'s woven body for every `R` receiver. Such an entry used to keep
    // the permanent tests, which ask the receiver and the constant-pool class
    // and never `A`, so a compiled site typed by an unwoven interface kept
    // serving `R`'s native over `A`'s advice (probe `R16RedefineWoven`,
    // `genSecure*` rows). Now `A` is the owner: the rule is applied to it at
    // fill (`A`'s generation and body, `R`'s immunity, as the probe reads
    // them) and the hit re-checks its generation once per redefine epoch.
    let Some(registering_class) = cm.get_class(registering) else {
        return Some(false);
    };
    use crate::runtime::interpreter::AncestorGuard;
    match jit_site_cache_redefine_guard(vm, &cm, declaring, &registering_class.name, info) {
        AncestorGuard::Look => {
            *native_owner = Some((declaring, cm.class_redefine_generation(declaring)));
            Some(true)
        }
        AncestorGuard::SkipNative | AncestorGuard::EndWalk => None,
    }
}

/// The JIT native site cache's reading of the one redefine rule
/// (`redefined_ancestor_guard`, door `JitSiteCache`: the publishing reading)
/// for `owner`, which registers the site's native: the fill's ancestor walk
/// asks it of each ancestor, the owner scope of the start class. One call site
/// for the door, as `every_ancestor_walk_door_asks_the_one_redefine_rule`
/// pins (round 15 wave 5, lane walk4). Takes the caller's class-manager guard.
fn jit_site_cache_redefine_guard(
    vm: &SharedVm,
    cm: &crate::classloading::ClassManager,
    owner: ClassId,
    owner_name: &str,
    info: &JitInvokeInfo,
) -> crate::runtime::interpreter::AncestorGuard {
    crate::runtime::interpreter::redefined_ancestor_guard(
        vm,
        cm,
        owner,
        owner_name,
        info.method_name,
        info.descriptor,
        crate::runtime::interpreter::AncestorWalkDoor::JitSiteCache,
        None,
    )
}

/// Resolve a `VarHandle` access-mode call site to the erased native the
/// registry actually holds, mirroring `invoke_on_class_shared_inner`'s
/// signature-polymorphic arm: base class first, then the exact receiver class.
///
/// `None` for anything that is not a `VarHandle` receiver carrying a
/// polymorphic method name, which keeps every other site on the behaviour it
/// had. `prefers_exact_signature_polymorphic_receiver` names only
/// `java/lang/foreign/DowncallHandle`, a `MethodHandle` receiver, so the
/// exact-first ordering that predicate selects cannot apply here.
fn resolve_signature_polymorphic_native_site(
    vm: &SharedVm,
    dispatch_class: &str,
    info: &JitInvokeInfo,
) -> Option<(
    String,
    cratonvm_native_api::NativeMethodId,
    Option<&'static str>,
)> {
    if !crate::vm::vm_exec::is_var_handle_signature_polymorphic_receiver(dispatch_class)
        || !crate::vm::vm_exec::is_signature_polymorphic_method_name(info.method_name)
    {
        return None;
    }
    let registry = &vm.natives.native_methods;
    const BASE: &str = "java/lang/invoke/VarHandle";
    for owner in [BASE, dispatch_class] {
        for poly_desc in crate::vm::vm_exec::SIGNATURE_POLYMORPHIC_NATIVE_DESCRIPTORS {
            if let Some(id) = registry.resolve_id(owner, info.method_name, poly_desc) {
                return Some((owner.to_string(), id, Some(poly_desc)));
            }
        }
    }
    None
}

/// Round 13 wave 10 (lane callcost4; the "compiled caller's path into the
/// door" of `r12w7-mh5-invoke-door-rows-time-is-mostly-throws-CLOSED-20261006.md`):
/// may this cache serve an `invokevirtual MethodHandle.invoke` /
/// `invokeExact` site whose receiver's class is `lookup_class`?
///
/// Only for the VM's own carrier, whose class IS `java/lang/invoke/MethodHandle`
/// (`lang_invoke::alloc_method_handle`, and `panama`'s downcall carrier, which
/// the door itself routes). For such a receiver every arm `invoke_or_native` and
/// `invoke_on_class_shared_inner` ask before their signature-polymorphic block
/// declines -- the `DowncallHandle` arms test another class name, the carrier is
/// not a lambda or annotation proxy, `MethodHandle` declares no method of the
/// site's concrete descriptor and inherits no native under it -- so the call
/// always ends in that block: the call-site descriptor armed
/// (`arms_poly_call_site`), `MethodHandle`'s erased native run through the
/// funnel, its result through `unbox_poly_return_checked`. This entry is that
/// ending, and the receiver-class guard every virtual entry carries keeps a
/// real JDK `MethodHandle` subclass (a `DirectMethodHandle`, a `Species_L`) on
/// the by-name route. `CRATONVM_JIT_SITE_CACHE_MH_INVOKE=0` refuses the family
/// again, as before (read on the cold fill only).
fn method_handle_carrier_site(lookup_class: &str) -> bool {
    lookup_class == "java/lang/invoke/MethodHandle"
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SITE_CACHE_MH_INVOKE")
}

/// SBR3 wave 2 (lane mhperf): the same site cache for a REAL JDK `MethodHandle`
/// receiver (`DirectMethodHandle`, a `BoundMethodHandle` species, an adapter): a
/// subclass of `java/lang/invoke/MethodHandle` in `java.lang.invoke`. `invoke` /
/// `invokeExact` are `final` natives of `MethodHandle` that no subclass can declare
/// or override, so for such a receiver too every arm `invoke_or_native` asks before
/// its signature-polymorphic block declines, and the call ends in that block; the
/// entry is that ending, guarded on the receiver's class like every virtual entry.
/// `CRATONVM_JIT_SITE_CACHE_MH_REAL=0` sends these receivers by name again. Read on
/// the cold fill only.
fn real_method_handle_site(
    vm: &SharedVm,
    lookup_class: &str,
    receiver_class_id: Option<ClassId>,
) -> bool {
    let Some(cid) = receiver_class_id else {
        return false;
    };
    lookup_class.starts_with("java/lang/invoke/")
        && lookup_class != "java/lang/invoke/MethodHandle"
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_SITE_CACHE_MH_REAL")
        && vm
            .classes
            .class_manager
            .try_read()
            .is_some_and(|cm| cm.is_subclass_of_by_name(cid, "java/lang/invoke/MethodHandle"))
}

/// The erased native a [`method_handle_carrier_site`] resolves to: the
/// signature-polymorphic block's own lookup (`SIGNATURE_POLYMORPHIC_NATIVE_DESCRIPTORS`
/// in order, on `MethodHandle`, which for the carrier is also the exact
/// receiver class). Cold: fill time only.
fn resolve_method_handle_carrier_native(
    vm: &SharedVm,
    info: &JitInvokeInfo,
) -> Option<(
    String,
    cratonvm_native_api::NativeMethodId,
    Option<&'static str>,
)> {
    let registry = &vm.natives.native_methods;
    const BASE: &str = "java/lang/invoke/MethodHandle";
    for poly_desc in crate::vm::vm_exec::SIGNATURE_POLYMORPHIC_NATIVE_DESCRIPTORS {
        if let Some(id) = registry.resolve_id(BASE, info.method_name, poly_desc) {
            return Some((BASE.to_string(), id, Some(poly_desc)));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Constant `MethodHandle`s in compiled code (JIT round 16, lane mh; MH15-5 /
// C14-2 of `jit-r15-mh-proposals-RETIRED-20261003.md`)
// ---------------------------------------------------------------------------
//
