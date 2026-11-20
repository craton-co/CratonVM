#[allow(unused_imports)]
use super::*;

impl<'a> ReadCursor<'a> {
    /// A cursor with no trusted state: every read through it takes the
    /// ordinary checked path. The constructor an implementor uses when it has
    /// nothing cheaper, and the one the default [`NativeHeapAccess::read_cursor`]
    /// returns.
    #[inline]
    pub fn checked(obj: ObjectRef) -> Self {
        Self {
            obj,
            trusted: 0,
            region: std::marker::PhantomData,
        }
    }

    /// A cursor carrying an implementation-private `trusted` token.
    ///
    /// # Safety contract (not `unsafe`, because misuse cannot be UB *here*)
    ///
    /// Only the implementor that reads `trusted` may construct one, and only
    /// after validating `obj`. A token minted by one heap and read by another
    /// is a bug in that pairing; the token is opaque and carries no meaning
    /// across implementors.
    #[inline]
    pub fn trusted(obj: ObjectRef, trusted: usize) -> Self {
        Self {
            obj,
            trusted,
            region: std::marker::PhantomData,
        }
    }

    /// The validated reference. Safe to pass to any checked accessor.
    #[inline]
    pub fn object(&self) -> ObjectRef {
        self.obj
    }

    /// The implementation-private token, or `0`.
    #[inline]
    pub fn token(&self) -> usize {
        self.trusted
    }
}

/// The debug-build check of "an allocation is not a GC point" (the rule on
/// [`NativeHeapAccess`]), for an implementor's allocation doors (JIT round
/// 17 wave 6, lane eintr2; page
/// `r17-native-alloc-gc-point-rule-differs-between-api-and-moving-mock-20261005`).
///
/// A door takes it first, with the implementor's pause epoch -- in the VM,
/// `GcBarrier::gc_generation`, which the barrier bumps once at the end of
/// every stop-the-world pause (`memory::roots::jit_memo_epoch_now`) -- and the
/// guard compares the epoch again when it drops at the door's return:
///
/// ```ignore
/// fn alloc_object(&mut self, class_id: ClassId, num_fields: usize) -> ObjectRef {
///     let shared = self.shared;
///     let _no_gc = NativeAllocNoGcPoint::enter("alloc_object", &shared.mem.gc_barrier.gc_generation);
///     // ... the door's body, unchanged ...
/// }
/// ```
///
/// A pause that COMPLETED while the door ran means the door had a GC point:
/// it collected on the allocating thread, or it parked in a blocking region a
/// pause ran under. Then every native holding an unpinned reference across
/// that door -- about 2,000 sites rely on the rule -- holds a stale one, so
/// the guard panics, naming the door and both epochs. A door that unwinds
/// (the catchable `OutOfMemoryError` of an infallible door) is not checked:
/// the guard sees the thread panicking and stays quiet.
///
/// The pause can only complete under a door whose thread the pause did not
/// wait for, so the check is exact for a counted mutator (every registered
/// Java thread). A VM-internal thread outside the barrier that runs a native
/// would see other threads' pauses; such a door must not take the guard.
///
/// Release builds (`debug_assertions` off): a zero-sized value, no load, no
/// `Drop` -- the door compiles to what it was.
#[must_use = "the check runs when the guard drops at the door's return"]
pub struct NativeAllocNoGcPoint<'a> {
    #[cfg(debug_assertions)]
    pub(in crate::registry) epoch: &'a std::sync::atomic::AtomicU64,
    #[cfg(debug_assertions)]
    pub(in crate::registry) before: u64,
    #[cfg(debug_assertions)]
    pub(in crate::registry) door: &'static str,
    pub(in crate::registry) _lifetime: std::marker::PhantomData<&'a std::sync::atomic::AtomicU64>,
}

impl<'a> NativeAllocNoGcPoint<'a> {
    /// Open the check for `door` against the pause epoch `epoch`.
    #[inline(always)]
    pub fn enter(door: &'static str, epoch: &'a std::sync::atomic::AtomicU64) -> Self {
        #[cfg(not(debug_assertions))]
        let _ = (door, epoch);
        Self {
            #[cfg(debug_assertions)]
            epoch,
            #[cfg(debug_assertions)]
            before: epoch.load(std::sync::atomic::Ordering::Acquire),
            #[cfg(debug_assertions)]
            door,
            _lifetime: std::marker::PhantomData,
        }
    }

    /// Did a pause complete since [`Self::enter`]? Always `false` in a
    /// release build.
    #[inline(always)]
    pub fn pause_completed(&self) -> bool {
        #[cfg(debug_assertions)]
        {
            self.epoch.load(std::sync::atomic::Ordering::Acquire) != self.before
        }
        #[cfg(not(debug_assertions))]
        {
            false
        }
    }
}

#[cfg(debug_assertions)]
impl Drop for NativeAllocNoGcPoint<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let after = self.epoch.load(std::sync::atomic::Ordering::Acquire);
        assert!(
            after == self.before,
            concat!(
                "native allocation door `{}` had a GC point: a stop-the-world pause ",
                "completed inside it (pause epoch {} -> {}); an allocation must not ",
                "collect (NativeHeapAccess, GC points inside a native: the one rule)"
            ),
            self.door,
            self.before,
            after
        );
    }
}


/// # GC points inside a native: the one rule
///
/// (JIT round 17 wave 2, lane funnel; page
/// `docs/feature-designs/native-dispatch-architecture.md`.)
///
/// While a native callback runs, the heap can move only at these points:
///
/// * a **Java call** -- every `invoke*` door of `NativeInvokeAccess`, and
///   every door that runs Java on the native's behalf (a constructor through
///   `new_object_initialized`, a class initializer a door such as
///   `new_object` triggers, a `toString` / `hashCode` upcall);
/// * an explicit **reclaim** -- `reclaim_before_alloc_retry` and its staged
///   twins, which exist to collect, and the doors whose contract says the
///   caller has pinned everything so they may collect:
///   `create_string_uninterned_gc_safe`, `create_string_from_units_gc_safe`,
///   `monitor_enter_gc_safe` (but NOT `try_alloc_object_gc_safe`, whose
///   name is historical: it is `alloc_object`);
/// * a **blocking region** -- `begin_blocking_region` / `park` / a contended
///   monitor enter or a monitor wait: a pause may run while the thread is
///   parked, and the native's references are stale afterwards unless pinned
///   (`end_blocking_region_refs` re-reads the ones it is handed).
///
/// **An allocation is not a GC point.** `alloc_object`, `try_alloc_object`,
/// `try_alloc_object_gc_safe`, `new_array`, `try_new_array`,
/// `new_ref_array`, `try_new_ref_array`, `create_string` and the other
/// allocating doors never collect: on exhaustion the infallible ones unwind
/// to the funnel (a catchable `OutOfMemoryError`) and the `try_*` ones answer
/// `None`. About 2,000 call sites hold bare references across them, and the
/// VM's heaps implement the doors on their no-GC paths
/// (`VmHeap::try_alloc_object_full` / `try_alloc_array_full`). A native may
/// therefore keep an unpinned `ObjectRef` across an allocation, and must pin
/// it across any of the three points above. An implementor's allocation
/// door can assert the rule in debug builds with [`NativeAllocNoGcPoint`]
/// (no pause completes inside the door), at no release cost.
///
/// The test mock models exactly this rule in its VM-rule moving mode
/// (`MockNativeContext::set_moving_heap_vm_rule`); its older strict moving
/// mode (`set_moving_heap`) also relocates at every allocation, which is
/// stricter than the VM and is kept as an optional extra check for natives
/// that pin defensively.
pub trait NativeHeapAccess: NativeInvokeAccess {
    /// Capability boundary: Allocation, roots, object fields, arrays, and strings.

    /// Create a new object of the given class.
    /// Returns an ObjectRef wrapped as `Value::Object(Some(ref))`.
    fn new_object(&mut self, class_name: &str) -> MethodCallResult;

    /// Allocate an object of `class_name` and run its `<init>` constructor
    /// (`init_desc`, with `init_args` as the post-`this` arguments), returning
    /// the freshly-constructed object.
    ///
    /// **GC-safety:** unlike `new_object` followed by a separate `invoke(...,
    /// "<init>", ...)`, this keeps the new object pinned as a GC root across the
    /// `<init>` call and returns the *forwarded* reference. Under the moving
    /// collector, a heavy constructor (e.g. BouncyCastle provider setup that
    /// allocates enough to trigger a collection) relocates the object; a native
    /// that held the pre-`<init>` raw `ObjectRef` would otherwise return a stale
    /// pointer that resolves to a reused `java.lang.Object`. Reflective
    /// construction paths (`Constructor.newInstance`, `Class.newInstance`,
    /// `Provider$Service.newInstance`) must use this instead of `new_object` +
    /// `invoke`.
    ///
    /// The default implementation is the non-GC-safe `new_object` + `invoke`
    /// pair (sufficient for test mocks with no moving GC); the real VM overrides
    /// it with the pinned variant.
    fn new_object_initialized(
        &mut self,
        class_name: &str,
        init_desc: &str,
        init_args: &[Value],
    ) -> MethodCallResult {
        let obj_val = self.new_object(class_name)?;
        if let Some(Value::Object(Some(obj))) = obj_val {
            let mut full = Vec::with_capacity(init_args.len() + 1);
            full.push(Value::Object(Some(obj)));
            full.extend_from_slice(init_args);
            self.invoke(class_name, "<init>", init_desc, &full)?;
        }
        Ok(obj_val)
    }

    /// Allocate an object of the EXACT `class_id` and run its `<init>`
    /// (`init_desc`, with `init_args` as the post-`this` arguments),
    /// returning the freshly-constructed (GC-forwarded) object.
    ///
    /// Unlike [`Self::new_object_initialized`], which resolves the class by
    /// *name* (and therefore collapses to whichever loader defined that name
    /// first — the global Application/bootstrap copy), this honours per-loader
    /// class identity per JVMS §5.3: `(defining loader, name)`. Reflective
    /// construction (`Constructor.newInstance`, `Class.newInstance`) must use
    /// this when it already holds the declaring class's mirror, so a class a
    /// custom loader defined (load-time weaving, webapp/OSGi isolation) is
    /// instantiated as ITSELF rather than as a same-named class some other
    /// loader happens to have defined first.
    ///
    /// The default implementation resolves the class id back to a name and
    /// delegates to the name-based path — sufficient for test mocks and any
    /// context with a single (global) loader namespace.
    fn new_object_initialized_with_class_id(
        &mut self,
        class_id: ClassId,
        init_desc: &str,
        init_args: &[Value],
    ) -> MethodCallResult {
        match self.class_name_of_id(class_id) {
            Some(name) => self.new_object_initialized(&name, init_desc, init_args),
            None => Ok(Some(Value::Object(None))),
        }
    }

    /// Build a natively thrown exception, `new class_name(init_args..)`, and
    /// say which of three things happened (round 15 wave 4, lane jdkonly4;
    /// proposal JO3-2 of `docs/feature-designs/jit-ir-optimizations.md`):
    ///
    /// * `Ok(Some(exc))`: constructed; `exc` is the forwarded reference;
    /// * `Ok(None)`: it could not be built (the class does not resolve, it has
    ///   no such constructor, e.g. a synthetic stub) -- the caller raises its
    ///   own fallback (a mint, or a flat error);
    /// * `Err(thrown)`: the class resolved and its constructor THREW (an
    ///   `OutOfMemoryError` / `StackOverflowError` in `fillInStackTrace`, an
    ///   application override that throws). HotSpot's `new X(..)` in the JDK's
    ///   throw path propagates that throwable, so the caller must too.
    ///
    /// [`Self::new_object_initialized`] answers `Err` for both "no class" and
    /// "constructor threw", so every builder treated a throwing constructor as
    /// "cannot construct" and threw its fallback instead
    /// (`r15w3-jdkonly3-throwable-builder-review-findings-CLOSED-20260930.md` F2).
    ///
    /// The default cannot see the resolution step: it answers a Java
    /// throwable out of [`Self::new_object_initialized`] as the constructor's
    /// and every other failure as `Ok(None)` (a mock's failed lookup is an
    /// internal error, not a Java throwable). The VM overrides it with the
    /// split made at class resolution.
    fn construct_throwable(
        &mut self,
        class_name: &str,
        init_desc: &str,
        init_args: &[Value],
    ) -> Result<Option<ObjectRef>, MethodCallFailed> {
        match self.new_object_initialized(class_name, init_desc, init_args) {
            Ok(Some(Value::Object(Some(exc)))) => Ok(Some(exc)),
            Err(MethodCallFailed::ExceptionThrown(thrown)) => {
                Err(MethodCallFailed::ExceptionThrown(thrown))
            }
            _ => Ok(None),
        }
    }

    /// Pin a heap object as a GC root and return an opaque handle index.
    ///
    /// Native code that holds an `ObjectRef` across a re-entrant call
    /// (`invoke` / `new_object_initialized` / any operation that can allocate)
    /// MUST pin it first: under the moving collector the object may relocate,
    /// leaving the raw `ObjectRef` stale (it then resolves to a reused, usually
    /// `java.lang.Object`, slot). After the call, read the forwarded reference
    /// back with [`read_native_pin`] before using it. Unpin the whole batch with
    /// [`unpin_native_roots`] passing the index returned by the *first* pin.
    ///
    /// Forgot to pin somewhere? Run with `CRATONVM_DBG_STALE_OBJREF=1` (the
    /// `Generational` GC backend only) to turn a stale read into an immediate,
    /// deterministic panic instead of silent corruption — see
    /// `gc/src/stale_objref_debug.rs` and
    /// wildfly-parallel-boot-stale-objectref-residual.md.
    ///
    /// # A funnel that allocates takes its receiver by `&mut ObjectRef`
    ///
    /// Pinning protects the code that does it. It does NOT protect a CALLER
    /// that passed the receiver in by value: the callee's refreshed reference
    /// dies at its closing brace and the caller keeps the pre-move address.
    /// That is a whole defect family rather than a one-off —
    /// `WORKER-5-NOTE-10` traced `TreeMap.size()` answering **0** on a view
    /// nothing had mutated to exactly this, and `WORKER-5-NOTE-13` converted
    /// 23 more funnels across all seven native crates.
    ///
    /// So the convention is:
    ///
    /// > **A helper that (a) takes a receiver `ObjectRef`, (b) can allocate,
    /// > and (c) returns no reference MUST take that receiver as
    /// > `&mut ObjectRef` and write the refreshed value back.**
    ///
    /// ```ignore
    /// fn refresh_the_thing(ctx: &mut dyn NativeContext, this: &mut ObjectRef) {
    ///     let pin = ctx.pin_native_root(*this);
    ///     let out = refresh_the_thing_body(ctx, *this);
    ///     *this = ctx.read_native_pin(pin, *this);   // hand the caller the new address
    ///     ctx.unpin_native_roots(pin);
    ///     out
    /// }
    /// ```
    ///
    /// `&mut` is the point, not the pin: it turns every unconverted caller
    /// into a COMPILE ERROR. Returning the reference instead is equally sound
    /// but weaker — a plain `-> ObjectRef` can be dropped in silence. Pinning
    /// at each call site is weakest of all: it is what the next audit has to
    /// find again, and of the call sites `WORKER-5-NOTE-12`/`-13` examined,
    /// the ones that had been protected by hand were consistently outnumbered
    /// by the ones beside them that had not.
    ///
    /// `scripts/stale-receiver-audit.py` is the gate; its baseline is empty.
    ///
    /// Default impl is a no-op (handle 0) for test mocks with no moving GC.
    fn pin_native_root(&mut self, _obj: ObjectRef) -> usize {
        0
    }

    /// Read back a pinned object by handle, returning its current (post-GC,
    /// possibly forwarded) reference. `fallback` is returned when the handle is
    /// out of range. Default impl returns `fallback` (mocks don't relocate).
    fn read_native_pin(&self, _handle: usize, fallback: ObjectRef) -> ObjectRef {
        fallback
    }

    /// Release all native pin roots from `base` (a handle returned by
    /// [`pin_native_root`]) onward. Default impl is a no-op.
    fn unpin_native_roots(&mut self, _base: usize) {}

    // ---- rooted handle scope (arch/handles) ----
    //
    // `pin_native_root`/`read_native_pin`/`unpin_native_roots` above fix
    // staleness but not the discipline: a native method still holds the
    // SAME `ObjectRef` type before and after pinning, so reading the
    // pre-pin local by mistake type-checks fine and silently reintroduces
    // the bug. The handle-scope quartet below is `cratonvm_types::handle`'s
    // rooted-handle design (`RootedHandle`/`HandleStorage`) adapted to this
    // trait-object boundary: a handle is an opaque `u32` slot, not an
    // `ObjectRef`, so there is no raw pointer left to accidentally read.
    //
    // **Discipline — READ BEFORE holding any `ObjectRef` across a call that
    // can allocate:** any native code that holds a reference across
    // `invoke`/`new_object_initialized`/`new_array`/anything that can run
    // Java (and therefore can trigger a GC) MUST root it with
    // [`handle_root`] first and read it back with [`handle_get`] afterward
    // — never keep using the pre-call local. The usual shape:
    //
    // ```ignore
    // let mut scope = NativeHandleScope::new(ctx);
    // let this_h = scope.root(this);
    // let arr = scope.new_array(ArrayElementType::Char, len); // may GC-move `this`
    // let this = scope.get(&this_h);                          // current address
    // ```
    //
    // `NativeHandleScope` closes itself on normal, early-return, and unwind
    // paths. Scopes nest and each guard releases only its own handles. See
    // `docs/feature-designs/native-handle-discipline.md` for the full
    // design and `native_builtins::lang_string` for worked examples
    // (`native_sb_init_default`, `native_sb_init_string`,
    // `native_sb_init_charsequence`, `native_sb_init_capacity`).
    //
    // Default impls below mirror the no-op/pass-through convention already
    // used by `pin_native_root` & co. for mock/test contexts with no moving
    // GC: pushing/popping a scope is a no-op, and `handle_root` delegates to
    // the already-default-implemented `pin_native_root` (truncated to
    // `u32`) as the closest existing stand-in. `handle_get`'s default
    // returns `None` rather than trying to read back through that
    // delegation — `pin_native_root`'s own default doesn't retain anything
    // to read, so there is nothing genuine to hand back. The VM's
    // `NativeContextImpl` overrides all four with a real per-thread slot
    // table (`vm/src/vm/vm_exec.rs`, "handle scope support").

    /// Push a new handle scope. Every [`handle_root`] call until the
    /// matching [`handle_scope_pop`] is released together when that pop
    /// runs. Default impl is a no-op.
    fn handle_scope_push(&mut self) {}

    /// Pop the current handle scope, releasing every handle rooted since the
    /// matching [`handle_scope_push`]. Default impl is a no-op.
    fn handle_scope_pop(&mut self) {}

    /// Root `r` in the current handle scope and return its slot id. Reading
    /// through the slot (via [`handle_get`]) always returns `r`'s current,
    /// possibly-GC-forwarded address — a handle can never go stale while its
    /// scope is open, unlike a raw `ObjectRef` copy.
    ///
    /// Default impl delegates to [`pin_native_root`] (see the block doc
    /// above for why); real GC-safety comes from the VM's override.
    fn handle_root(&mut self, r: ObjectRef) -> u32 {
        self.pin_native_root(r) as u32
    }

    /// Read back the current reference for `slot` (from [`handle_root`]), or
    /// `None` if `slot` is out of range or its scope already popped. Default
    /// impl returns `None`.
    fn handle_get(&self, _slot: u32) -> Option<ObjectRef> {
        None
    }

    /// Create a *persistent* global GC root for `obj`, returning an opaque handle.
    ///
    /// Unlike [`pin_native_root`] (which is per-thread and unwound when the
    /// current native call returns), a global root survives across calls and
    /// across threads, and is remapped by the moving collector. It lives until
    /// [`remove_global_root`] is called. Use it to hold an `ObjectRef` that a
    /// *different* thread will consume later — e.g. an asynchronous-I/O
    /// `CompletionHandler` / attachment / target `ByteBuffer` parked while a
    /// worker thread performs a blocking read, then delivered on a dispatcher
    /// thread. Backed by the same table as JNI `NewGlobalRef`.
    ///
    /// Default impl returns `0` (no-op) for mock contexts with no moving GC.
    fn add_global_root(&mut self, _obj: ObjectRef) -> usize {
        0
    }

    /// Resolve a global root handle (from [`add_global_root`]) to its current
    /// (post-GC, possibly relocated) reference. Returns `None` for handle `0` or
    /// an unknown handle. Default impl returns `None`.
    fn resolve_global_root(&self, _handle: usize) -> Option<ObjectRef> {
        None
    }

    /// Release a global root created by [`add_global_root`]. Returns `true` if the
    /// handle was found and removed. Default impl returns `false`.
    fn remove_global_root(&mut self, _handle: usize) -> bool {
        false
    }

    /// Create a WEAK global root for `obj`, returning an opaque handle, or `0`
    /// when this context has none (the default: a mock context has no
    /// collector, so there is nothing to be weak against). gc-common w24-b
    /// (`common-w23a-awt-rows-outlive-their-java-objects-within-a-vm`).
    ///
    /// The handle is NOT a GC root: it keeps nothing alive. While `obj` is
    /// alive, [`resolve_global_root`] answers its current (remapped)
    /// reference. Once a collection has found `obj` unreachable, it answers
    /// `None`, from that collection's weak-global sweep on: a stop-the-world
    /// collection clears (or remaps) the handle inside its pause, and a
    /// concurrent cycle clears it at its remark, before the memory is
    /// reclaimed. Strength is the JNI weak global's (JDK 9+: phantom): a
    /// finalizable `obj` is kept until it is really gone. The handle stays
    /// valid, answering `None`, until [`remove_global_root`] releases it; that
    /// call releases weak and strong handles alike, and every weak handle must
    /// be released through it (a cleared one included) or its slot leaks.
    /// Backed by the table of JNI `NewWeakGlobalRef`.
    ///
    /// Resolving a weak handle is NOT a keep-alive. A caller that stores the
    /// resolved reference into the heap, or returns it to Java, must first
    /// call [`gc_reference_keep_alive`] on it, as `Reference.get()` does: a
    /// concurrent mark may not have reached it. Comparing it with a reference
    /// the caller already holds needs nothing.
    ///
    /// Default impl returns `0`.
    fn add_weak_global_root(&mut self, _obj: ObjectRef) -> usize {
        0
    }

    /// Look up a bounded, GC-rooted exact-HashMap cache using a Java String
    /// object directly. VM implementations can compare compact payloads without
    /// allocating a host String; the default leaves mock contexts unchanged.
    fn hashmap_string_node_cache_get_object(
        &mut self,
        _map: ObjectRef,
        _key: ObjectRef,
    ) -> Option<Value> {
        None
    }

    /// Look up a bounded, GC-rooted exact-HashMap String node cache. Native
    /// implementations may use this to avoid rediscovering immutable keys;
    /// the default keeps lightweight test contexts independent of VM layout.
    fn hashmap_string_node_cache_get(&mut self, _map: ObjectRef, _key: &str) -> Option<Value> {
        None
    }

    /// Publish an exact HashMap node for [`Self::hashmap_string_node_cache_get`].
    /// Implementations must preserve normal map mutation semantics.
    fn hashmap_string_node_cache_put(
        &mut self,
        _map: ObjectRef,
        _key_object: ObjectRef,
        _key: &str,
        _node: ObjectRef,
    ) {
    }

    /// Look up a ConcurrentHashMap segment node whose mutation generation is
    /// still current. The segment layout has no Java `modCount` field, so it
    /// cannot share the exact-HashMap validity predicate above.
    fn chm_string_node_cache_get_object(
        &mut self,
        _map: ObjectRef,
        _key: ObjectRef,
    ) -> Option<(i32, u64, Value)> {
        None
    }

    /// Publish a ConcurrentHashMap segment node guarded by the caller's
    /// seqlock-style mutation generation.
    fn chm_string_node_cache_put(
        &mut self,
        _map: ObjectRef,
        _segment_id: i32,
        _key_object: ObjectRef,
        _key: &str,
        _node: ObjectRef,
        _generation: u64,
    ) {
    }

    /// Get the identity hash code of an ObjectRef.
    fn identity_hash_code(&self, obj: ObjectRef) -> i32;

    /// [`Self::identity_hash_code`] when the answer is the hash the object's
    /// header holds or mints in place (the heap's lazy CAS), so the call takes
    /// no monitor and allocates nothing: what a LEAF caller
    /// ([`NativeMethodRegistry::set_leaf`]) may do. `None` -- nothing done --
    /// when only the full answer can give it (a thin-locked or inflated object
    /// never hashed: the hash is displaced into its monitor). JIT round 21,
    /// lane native (`CRATONVM_JIT_IDENTITY_HASH_LEAF`). The default answers
    /// `None`, so mocks and out-of-tree contexts take the full answer.
    fn identity_hash_code_installed(&self, _obj: ObjectRef) -> Option<i32> {
        None
    }

    /// B-J: register a `java.lang.invoke.VarHandle` as a permanent GC root.
    /// VarHandles live in `static final` fields and are used for lock-free CAS;
    /// without an explicit root a moving GC reclaimed them and left their static
    /// holder slots stale (all-zero header → misdispatch). Default no-op for
    /// non-VM contexts (tests/mocks); the interpreter overrides it to insert
    /// into `SharedVm::var_handle_roots`.
    ///
    /// The registry is keyed by identity hash, and two live objects of one VM
    /// can share a hash. Since gc-common w29-d a same-hash registration is
    /// APPENDED to the key's bucket (both objects stay rooted); it used to
    /// overwrite the first object's root. Re-registering one object is a
    /// no-op. The root is permanent: there is no release.
    fn register_var_handle_root(&mut self, _vh: ObjectRef) {}

    /// Read back the CURRENT address of a persistent native root previously
    /// registered with [`register_var_handle_root`], keyed by the value
    /// [`identity_hash_code`] returned for it at registration time.
    ///
    /// **Ambiguous under an identity-hash collision.** When several objects
    /// were registered under one hash, this answers the FIRST of them, for
    /// every caller holding that key (gc-common w29-d). A native that needs
    /// the current address of ONE specific object should hold a JNI global
    /// handle instead ([`add_global_root`] / [`resolve_global_root`]), which is
    /// unique per object and releasable; or keep its copy in a side table that
    /// a `VM_ROOT_SOURCES` row roots and remaps. New code should not add
    /// readers of this method
    /// (`docs/feature-designs/native-dispatch-architecture.md`,
    /// rank 2).
    ///
    /// Rationale: `register_var_handle_root` keeps the object alive and the
    /// GC remaps the registry entry after a move — but it cannot rewrite raw
    /// `ObjectRef` copies cached in native `static`s (`ASYNC_POOL`,
    /// `SYSTEM_CL`, `SECURITY_MANAGER`). Such long-lived-native-singleton
    /// caches must store the identity key alongside the raw ref and re-read
    /// through this method at every use; using only the cached raw ref is a
    /// use-after-move once a moving young GC or a promotion relocates the
    /// object. Default `None` for non-VM contexts (callers fall back to the
    /// cached ref, matching the mock heaps that never move objects).
    fn read_var_handle_root(&self, _identity_key: i32) -> Option<ObjectRef> {
        None
    }

    // -- Heap access methods (for native method implementations) --
    //
    // # Security contract (M4a — unvalidated slot indices)
    //
    // Every accessor in this section (and the volatile / CAS / static
    // variants further down) takes a bare `usize` slot `index` with no type
    // carried bound and returns an infallible `Value`. The `index` is NOT
    // validated by the trait: it is the *caller's* obligation to pass a slot
    // that is in range for `obj`'s class layout (for fields) or for the
    // array's length (for elements). Callers typically derive the index from
    // trusted reflection metadata (`FieldMetadata::slot_index`,
    // `resolve_field_index`, `array_length`) and must NOT pass an index
    // sourced from untrusted Java/native input without first bounds-checking
    // it against `array_length` / the resolved field count.
    //
    // Implementations are the enforcement point: an implementation MUST
    // bounds-check `index` and MUST NOT read or write memory outside the
    // object's field block / the array's element range. On an out-of-range
    // index an implementation must fail safe (e.g. return
    // `Value::Object(None)` / a default for reads, no-op for writes, or
    // raise a VM error) — it must NEVER perform an out-of-bounds heap access.
    // The production `NativeContextImpl` in the `vm` crate performs this
    // validation; mock/test impls that elide it must only ever be fed
    // trusted indices.

    /// Read an object field by slot index.
    ///
    /// `index` is an absolute heap field slot (see [`FieldMetadata::slot_index`]).
    /// The caller must ensure it is in range for `obj`'s class; the
    /// implementation MUST bounds-check and MUST NOT read out of range (M4a).
    fn get_field(&self, obj: ObjectRef, index: usize) -> Value;

    /// [`get_field`](Self::get_field) for a caller that already knows the
    /// field's declared descriptor byte (`I`, `J`, `Z`, `L`, ...).
    ///
    /// `get_field` has to ask the class metadata what the slot's declared type
    /// is before it can decode the raw storage — a per-read lookup that a
    /// native which resolved its field indices once already has the answer to.
    /// Passing the descriptor in skips that lookup; passing a WRONG one decodes
    /// the slot as the wrong type, exactly as if the class had declared it that
    /// way, so only use it with a descriptor read out of the same class
    /// metadata the index came from.
    ///
    /// The default implementation ignores the hint and delegates, so an
    /// implementor with no cheaper path needs to do nothing.
    fn get_field_typed(&self, obj: ObjectRef, index: usize, descriptor: u8) -> Value {
        let _ = descriptor;
        self.get_field(obj, index)
    }

    /// [`get_field_typed`](Self::get_field_typed) for several slots of ONE
    /// object at once.
    ///
    /// Each element of `slots` is `(slot index, descriptor byte)`; results land
    /// in `out` at the matching position, and the shorter of the two bounds the
    /// batch.
    ///
    /// Every single-field read has to canonicalise the reference first (an
    /// object may have been evacuated since the caller obtained it). A native
    /// that needs three or four fields of the same receiver per call — which is
    /// what the per-element `java.nio.DirectByteBuffer` accessors do, once per
    /// byte moved — pays that, and the dynamic dispatch, once instead of once
    /// per field.
    fn get_fields_typed(&self, obj: ObjectRef, slots: &[(usize, u8)], out: &mut [Value]) {
        for (slot, dst) in slots.iter().zip(out.iter_mut()) {
            *dst = self.get_field_typed(obj, slot.0, slot.1);
        }
    }

    /// The WRITE twin of [`get_fields_typed`](Self::get_fields_typed): several
    /// slots of ONE object, with the descriptor supplied per slot.
    ///
    /// Each element of `slots` is `(slot index, descriptor byte)` and the value
    /// at the matching position of `values` is stored there; the shorter of the
    /// two bounds the batch. The descriptor must be the one this class actually
    /// declares for that slot -- read it out of the same class metadata the
    /// index came from -- because it selects the coercion the store performs.
    ///
    /// MEASURED, and the reason this exists: `SHA256Digest.processBlock`
    /// (`native-builtins/src/phases_late/bouncycastle.rs`) writes nine fields
    /// per 64-byte block. On the single-slot door each of those pays its own
    /// canonicalisation, corrupt-cell watch and descriptor resolution, and a
    /// block measured **5 893 ns** against HotSpot's 334 -- with the whole
    /// compression function already in Rust and the sixteen `processWord`
    /// decodes costing only 384 ns of it. The crossing, not the kernel and not
    /// the bytecode around it, was the wall.
    ///
    /// The default implementation loops over [`set_field`](Self::set_field), so
    /// an implementor with no cheaper route needs to do nothing.
    fn set_fields_typed(&self, obj: ObjectRef, slots: &[(usize, u8)], values: &[Value]) {
        for (slot, value) in slots.iter().zip(values.iter()) {
            let _ = slot.1;
            self.set_field(obj, slot.0, *value);
        }
    }

    /// Enter a read-only region on `obj`, validating it ONCE — see
    /// [`ReadCursor`] for the measurement that motivates it and for why the
    /// no-safepoint claim is a borrow-checker obligation rather than a comment.
    ///
    /// `None` when `obj` is not a live object on this heap, which is the same
    /// answer the checked accessors reach by returning defaults — a caller that
    /// gets `None` must fall back or bail, never dereference.
    ///
    /// The default implementation validates nothing extra and hands back a
    /// [`ReadCursor::checked`], so every accessor below is exactly today's
    /// behaviour for an implementor that does not override this.
    fn read_cursor(&self, obj: ObjectRef) -> Option<ReadCursor<'_>> {
        Some(ReadCursor::checked(obj))
    }

    /// A reference read THROUGH a cursor, promoted to a cursor of its own.
    ///
    /// This is the arm that removes most of a native's membership probes: a
    /// reference slot of a live object holds a live object, and inside a region
    /// no safepoint has intervened since the region's entry was validated, so
    /// the probe is re-answering a settled question.
    ///
    /// `value` MUST have been read through `parent` (one of the cursor
    /// accessors on this trait). Passing a reference from anywhere else — an
    /// argument, a side table, a previous call — defeats the premise, because
    /// only a read through the region carries its no-safepoint guarantee.
    ///
    /// `None` for a null, a non-reference `Value`, or a reference this
    /// implementation declines to trust.
    ///
    /// The default implementation validates through [`Self::read_cursor`], so
    /// it is exactly as safe as the checked path.
    fn reached(&self, parent: &ReadCursor<'_>, value: Value) -> Option<ReadCursor<'_>> {
        let _ = parent;
        match value {
            Value::Object(Some(obj)) => self.read_cursor(obj),
            _ => None,
        }
    }

    /// [`Self::get_field_typed`] through a cursor.
    ///
    /// Default: the checked accessor on the cursor's object.
    fn cursor_field_typed(&self, cursor: &ReadCursor<'_>, index: usize, descriptor: u8) -> Value {
        self.get_field_typed(cursor.object(), index, descriptor)
    }

    /// [`Self::get_fields_typed`] through a cursor.
    fn cursor_fields_typed(
        &self,
        cursor: &ReadCursor<'_>,
        slots: &[(usize, u8)],
        out: &mut [Value],
    ) {
        self.get_fields_typed(cursor.object(), slots, out);
    }

    /// [`Self::get_field_volatile`] through a cursor.
    ///
    /// Kept distinct from [`Self::cursor_field_typed`] because the ORDERING is
    /// not the region's to elide: a region proves nothing can move, not that a
    /// concurrent writer cannot publish. Only the membership probe and the
    /// forwarding barrier are skipped.
    fn cursor_field_volatile(&self, cursor: &ReadCursor<'_>, index: usize) -> Value {
        self.get_field_volatile(cursor.object(), index)
    }

    // --- read-and-promote, fused ---------------------------------------
    //
    // A reference read through a region is almost always read IN ORDER TO
    // walk it, so the natural spelling is `reached(&c, cursor_field(&c, i))`
    // — and that is two dynamic calls through `&dyn NativeContext` where the
    // checked accessor it replaces was one. MEASURED: the un-fused spelling
    // roughly doubled the walk's dynamic-call count and gave back most of
    // what the region saved — `ChmProbe get` was 13 % faster with the region
    // ON than OFF in the same binary, and FLAT against the binary without
    // the region at all. The three below are the same work in one call, and
    // they are the spelling a native should use.

    /// [`Self::cursor_field_typed`] of a REFERENCE slot, promoted to a cursor.
    ///
    /// `None` for null, a non-reference value, or a reference this
    /// implementation declines to trust.
    fn cursor_field_reference(
        &self,
        cursor: &ReadCursor<'_>,
        index: usize,
    ) -> Option<ReadCursor<'_>> {
        let value = self.cursor_field_typed(cursor, index, b'L');
        self.reached(cursor, value)
    }

    /// [`Self::cursor_field_volatile`] of a REFERENCE slot, promoted to a
    /// cursor.
    fn cursor_field_volatile_reference(
        &self,
        cursor: &ReadCursor<'_>,
        index: usize,
    ) -> Option<ReadCursor<'_>> {
        let value = self.cursor_field_volatile(cursor, index);
        self.reached(cursor, value)
    }

    /// [`Self::cursor_array_element`] of a REFERENCE array, promoted to a
    /// cursor.
    fn cursor_array_element_reference(
        &self,
        cursor: &ReadCursor<'_>,
        index: usize,
    ) -> Option<ReadCursor<'_>> {
        let value = self.cursor_array_element(cursor, index);
        self.reached(cursor, value)
    }

    /// [`Self::array_shape`] through a cursor.
    fn cursor_array_shape(&self, cursor: &ReadCursor<'_>) -> Option<(ArrayElementType, usize)> {
        self.array_shape(cursor.object())
    }

    /// [`Self::get_array_element`] through a cursor.
    fn cursor_array_element(&self, cursor: &ReadCursor<'_>, index: usize) -> Value {
        self.get_array_element(cursor.object(), index)
    }

    /// [`Self::class_id_of_object`] through a cursor — free once the region has
    /// the header.
    fn cursor_class_id(&self, cursor: &ReadCursor<'_>) -> crate::ClassId {
        self.class_id_of_object(cursor.object())
    }

    /// Write an object field by slot index.
    ///
    /// `index` is an absolute heap field slot (see [`FieldMetadata::slot_index`]).
    /// The caller must ensure it is in range for `obj`'s class; the
    /// implementation MUST bounds-check and MUST NOT write out of range (M4a).
    fn set_field(&self, obj: ObjectRef, index: usize, value: Value);

    /// Read an object field by slot index **without descriptor coercion** —
    /// the `Value` exactly as it is stored, tag included.
    ///
    /// # Why this exists (G52-1 NOMINATION 1, G56-1)
    ///
    /// Every other slot-indexed accessor on this trait resolves the field's
    /// declared descriptor and routes the slot through
    /// `gc::heap::coerce_field_value_for_slot`: [`get_field`](Self::get_field),
    /// [`get_field_volatile`](Self::get_field_volatile),
    /// [`compare_and_swap_field`](Self::compare_and_swap_field), and
    /// [`get_field_typed`](Self::get_field_typed) with a real descriptor.
    /// The only non-coercing pair was
    /// [`get_field_by_name`](Self::get_field_by_name) /
    /// [`set_field_by_name`](Self::set_field_by_name), which is **name**-keyed:
    /// it cannot be driven from a slot index at all, and it resolves a shadowed
    /// field name to the wrong slot. So a native that holds a slot index and
    /// wants the stored bits — `Object.clone()`, which the JVM specifies as a
    /// verbatim field copy, and any reader that must tell "never written" from
    /// "explicitly null" — had no way to ask for them. `Object.clone()` did not
    /// opt into coercion; the API moved underneath it.
    ///
    /// # Contract
    ///
    /// An implementation MUST return the slot's stored `Value` unchanged, and
    /// MUST still bounds-check `index` and fail safe exactly as
    /// [`get_field`](Self::get_field) does (M4a). "Raw" licenses skipping the
    /// descriptor decode; it never licenses an out-of-range access. The
    /// reference must still be canonicalised (forwarded) before the read — a
    /// raw read of a stale address is not a raw read of the object.
    ///
    /// # The default implementation is already raw, on both impls that exist
    ///
    /// It routes through [`get_field_typed`](Self::get_field_typed) with
    /// [`RAW_SLOT_DESCRIPTOR`], which is not a JVM field-descriptor first byte.
    ///
    /// * Production (`NativeContextImpl` in the `vm` crate) overrides
    ///   `get_field_typed` as `heap.get_field_as(obj, index, descriptor)`, and
    ///   `coerce_field_value_for_slot` dispatches on the descriptor byte with a
    ///   final `_ => value` arm. A byte outside `J D F I B C S Z L [` therefore
    ///   returns the slot verbatim and — this is the half that matters for the
    ///   G30 instrument — fires **no** coercion-loss event. It is also cheaper
    ///   than [`get_field`](Self::get_field): the descriptor is supplied, so
    ///   `resolve_field_descriptor_byte_cached` is skipped entirely.
    /// * Mocks that do not override `get_field_typed` get this trait's default,
    ///   which ignores the byte and calls `get_field` — already raw there.
    ///
    /// An implementor with a cheaper direct route should override this; one
    /// with none needs to do nothing.
    fn get_field_raw(&self, obj: ObjectRef, index: usize) -> Value {
        self.get_field_typed(obj, index, RAW_SLOT_DESCRIPTOR)
    }

    /// Write an object field by slot index **without descriptor coercion** —
    /// the `Value` is stored with the tag the caller handed over.
    ///
    /// The store half of [`get_field_raw`](Self::get_field_raw). See that
    /// method for why the pair exists; the contract is the same, and an
    /// implementation MUST still bounds-check `index` and MUST NOT write out
    /// of range (M4a).
    ///
    /// # THIS DEFAULT IS NOT RAW. Read before pairing it with a raw read.
    ///
    /// There is no typed setter on this trait to lean on the way
    /// [`get_field_raw`](Self::get_field_raw) leans on
    /// [`get_field_typed`](Self::get_field_typed), and the only non-coercing
    /// setter reachable from here — [`set_field_by_name`](Self::set_field_by_name)
    /// — is name-keyed and cannot be driven from a slot index. The default
    /// therefore delegates to [`set_field`](Self::set_field), which in the
    /// production `NativeContextImpl` resolves the descriptor and coerces.
    /// Overriding it there is a one-line body — `heap.set_field(obj, index,
    /// value)` after the usual `load_and_forward` — and until that lands this
    /// method is raw only on impls whose `set_field` was already raw (the test
    /// mocks).
    ///
    /// **Do not pair a raw read with this default in a copy loop.** That
    /// combination is strictly worse for the G30 instrument than coercing
    /// both halves: a `read` of `Int(0)` at an `L` slot currently answers
    /// `Object(None)` and is counted in the benign `read` column, whereas a
    /// raw read followed by a coercing store hands the `Int(0)` to the setter
    /// and re-reports it as a **`store`** — the column `gc/src/collector.rs`
    /// reserves for real defects ("a read that coerces is usually the slot
    /// repairing a never-initialised tag, a store that coerces has destroyed
    /// something a writer meant"). G52-1 §1.5 measured that migration at ~24
    /// events for `native_object_clone` alone. The verbatim-copy callers must
    /// wait for both halves to be genuinely raw and then switch together.
    fn set_field_raw(&self, obj: ObjectRef, index: usize, value: Value) {
        self.set_field(obj, index, value);
    }

    /// `Object.clone()`'s copy: slots `0..num_fields` of `src` into `dst`,
    /// verbatim, where `dst` is an object of `src`'s class the caller has just
    /// allocated and nothing has read or written since.
    ///
    /// The default is the slot-by-slot raw pair
    /// ([`get_field_raw`](Self::get_field_raw) /
    /// [`set_field_raw`](Self::set_field_raw)), which canonicalises both
    /// objects and resolves the slot's layout on every read and every write.
    /// An implementor that can validate once and copy the whole body should
    /// override this; the copy it makes must be the one the default makes.
    fn copy_fields_into_clone(&self, src: ObjectRef, dst: ObjectRef, num_fields: usize) {
        for i in 0..num_fields {
            let val = self.get_field_raw(src, i);
            self.set_field_raw(dst, i, val);
        }
    }

    /// Read an object field by name. Resolves the field name to a slot index
    /// by searching the object's class hierarchy. Returns `Value::Object(None)`
    /// if the field is not found.
    fn get_field_by_name(&self, obj: ObjectRef, field_name: &str) -> Value;

    /// Write an object field by name. Resolves the field name to a slot index
    /// by searching the object's class hierarchy. No-op if the field is not found.
    fn set_field_by_name(&self, obj: ObjectRef, field_name: &str, value: Value);

    /// [`get_field_by_name`](Self::get_field_by_name) for several fields of ONE
    /// object at once; `out[i]` receives the value of `names[i]` (the shorter
    /// of the two bounds the batch, and a name that does not resolve reads as
    /// `Value::Object(None)`, exactly as the single-field read does).
    ///
    /// A native that reads a dozen fields of the same receiver per call pays
    /// the reference canonicalisation and the class lookup once instead of once
    /// per field. The default delegates to the single-field read, so an
    /// implementor with no cheaper path needs to do nothing.
    ///
    /// The names are `'static` on purpose: a string literal's address and
    /// length identify its content for the life of the process, which lets an
    /// implementation memoize `(class, name) -> slot` on the address alone,
    /// without hashing or comparing the characters.
    fn get_fields_by_name(&self, obj: ObjectRef, names: &[&'static str], out: &mut [Value]) {
        for (name, dst) in names.iter().zip(out.iter_mut()) {
            *dst = self.get_field_by_name(obj, name);
        }
    }

    /// [`set_field_by_name`](Self::set_field_by_name) for several fields of ONE
    /// object at once, applied in order. A name that does not resolve is
    /// dropped exactly as the single-field write drops it.
    ///
    /// The store half of [`get_fields_by_name`](Self::get_fields_by_name); the
    /// default delegates to the single-field write.
    fn set_fields_by_name(&self, obj: ObjectRef, fields: &[(&'static str, Value)]) {
        for (name, value) in fields {
            self.set_field_by_name(obj, name, *value);
        }
    }

    /// Resolve a field name to its slot index for a given class.
    /// Returns `None` if the field is not found in the class hierarchy.
    fn resolve_field_index(&self, class_name: &str, field_name: &str) -> Option<usize>;

    /// Resolve a field name to its slot index by `ClassId` directly --
    /// no class-name round-trip. Returns `None` if the field is not found
    /// in the class hierarchy.
    ///
    /// Prefer this over `resolve_field_index` whenever the caller already
    /// holds the object (and so its exact `ClassId` via
    /// `class_id_of_object`): `resolve_field_index`'s name-based lookup
    /// re-resolves the class GLOBALLY by name, which returns `None`
    /// whenever 2+ distinct loaders each define their own class under the
    /// same simple name (a legitimate "ambiguous" answer for a bare name,
    /// but a needless loss when the caller already holds the exact,
    /// unambiguous `ClassId` -- e.g. a native shim reading a field off a
    /// third-party object whose class gets redefined under a fresh loader
    /// each time, such as ByteBuddy classes under
    /// `@CompileWithForkedClassLoader`).
    fn resolve_field_index_by_class_id(&self, class_id: ClassId, field_name: &str)
        -> Option<usize>;

    /// Resolve the slot declared by `java/lang/Throwable` for `field_name`.
    ///
    /// The default deliberately keeps this a normal by-name resolution. A VM
    /// whose Throwable layout is stable may override it with a **per-VM**
    /// cache; native code must never retain a slot index in a process global,
    /// because another VM in the same process can have a different bootstrap
    /// class layout.
    fn throwable_field_index(&self, field_name: &str) -> Option<usize> {
        self.resolve_field_index("java/lang/Throwable", field_name)
    }

    /// JIT round 16 (lane throwpath, TP16-2): finish a throwable's fill in one
    /// call -- store `backtrace = throwable` and `depth` in the slots
    /// `java/lang/Throwable` declares, and answer whether its
    /// `suppressedExceptions` slot already holds a reference. `None` means
    /// "not done, nothing written" (this default, the VM's switch off, or a
    /// receiver whose layout is not the real `Throwable`'s): the caller takes
    /// its per-field path. Done as three `throwable_field_index` /
    /// `object_num_fields` / `set_field` round trips, the fill's stores were
    /// ~17% of the `fillInStackTrace` native (sampled).
    fn finish_throwable_fill(&mut self, _throwable: ObjectRef, _depth: i32) -> Option<bool> {
        None
    }

    /// Round 15 wave 4 (lane threadvm; TR4W3-1): where the field
    /// `field_name` that `java.lang.Thread` declares lives on the thread
    /// `thread`, answered from a **per-VM** `Thread` layout memo without the
    /// class-manager lock. `None` means "no memo here" (this default, a
    /// mock, the memo switched off, or a name the memo does not keep): the
    /// caller walks the class chain itself. A slot must never be kept in a
    /// process global (see [`Self::throwable_field_index`]).
    fn thread_declared_field_slot(
        &self,
        _thread: ObjectRef,
        _field_name: &str,
    ) -> Option<ThreadDeclaredField> {
        None
    }

    /// Allocate a primitive array (element_type: Boolean=4..Long=11).
    fn new_array(&mut self, element_type: ArrayElementType, length: usize) -> ObjectRef;

    /// Allocate a reference array for the given component class.
    fn new_ref_array(&mut self, class_id: ClassId, length: usize) -> ObjectRef;

    /// Fallible reference-array allocator: returns `None` when the request
    /// cannot be satisfied (the backing array is too large for the heap) so the
    /// caller can throw a *catchable* `OutOfMemoryError` instead of the VM
    /// hard-aborting in the infallible [`new_ref_array`](Self::new_ref_array).
    /// The default delegates to `new_ref_array` so non-VM contexts (mocks)
    /// compile unchanged; the VM impl overrides it with the no-GC
    /// young→old-gen spill path that reports OOM rather than aborting.
    fn try_new_ref_array(&mut self, class_id: ClassId, length: usize) -> Option<ObjectRef> {
        Some(self.new_ref_array(class_id, length))
    }

    /// Fallible primitive-array allocator — the `new_array` counterpart of
    /// [`try_new_ref_array`](Self::try_new_ref_array). Returns `None` when the
    /// request is too large for the heap so a native (e.g. `StringBuilder(int)`)
    /// can raise a catchable `OutOfMemoryError` instead of aborting. Default
    /// delegates to the infallible `new_array`.
    fn try_new_array(
        &mut self,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        Some(self.new_array(element_type, length))
    }

    /// Reclaim the heap and report whether a retry is worth making, for a
    /// native whose `try_new_array` / `try_new_ref_array` / `alloc_object`
    /// just returned `None`.
    ///
    /// # Why this is not simply done inside the allocators
    ///
    /// The fallible native allocators deliberately do NOT collect. A native
    /// holds raw `ObjectRef`s in Rust locals, and those are in no GC root set:
    /// a collection triggered underneath one would relocate them (dangling the
    /// locals) or sweep them (freeing live objects). See `runtime::native_oom`,
    /// whose whole design follows from that rule. So the allocators get exactly
    /// one attempt and then report failure — which is correct for them and, on
    /// its own, wrong for the program.
    ///
    /// What the rule costs, measured: the interpreter's `gc_alloc_array` and
    /// the JIT's `jit_newarray` both run a LADDER on a failed allocation —
    /// retire the TLAB, force a collection, retry, `last_ditch_reclaim`, retry
    /// again — and only then throw. A native allocating the same array gets no
    /// ladder at all, so it reports `OutOfMemoryError` on the first refusal.
    /// On H2 `TestBenchmark` (`-Xmx1g`, ZGC) that surfaced as a 10 MiB
    /// `ByteBuffer.allocate` failing with the heap **97% free**: the arena had
    /// no hole that big at that instant, the collection that would have opened
    /// one had not been asked for, and repeating the identical allocation one
    /// Java statement later succeeded immediately.
    ///
    /// # The precondition, which the CALLER owns
    ///
    /// Only call this when **this native holds no unpinned `ObjectRef` in a
    /// Rust local** — i.e. at an allocation performed before the native has
    /// acquired any heap reference, or with everything it holds pinned through
    /// [`pin_native_root`](Self::pin_native_root). At that point the collection
    /// is exactly as safe as the one the interpreter runs between two
    /// bytecodes. A native that has already stashed a bare `ObjectRef` must NOT
    /// call this; it must report OOM as before.
    ///
    /// That is why this is a separate call rather than a retry folded into the
    /// allocators: the allocators cannot see their caller's locals, and the
    /// caller can.
    ///
    /// Returns `false` when no reclamation was attempted or the heap is
    /// GC-thrashing past the overhead limit — in which case the caller should
    /// surface `OutOfMemoryError` without a retry, rather than spin. The
    /// default is `false` so mock/non-VM contexts keep their current
    /// single-attempt behaviour.
    fn reclaim_before_alloc_retry(&mut self) -> bool {
        false
    }

    /// gcg g3/o (opt-in `CRATONVM_GC_NATIVE_FACTORY_ATTEMPT_LADDER`): the first
    /// rung of a staged reclaim, the young collection only. `true`: attempt the
    /// allocation now and, only if that fails, call
    /// [`reclaim_before_alloc_retry_after_young`](Self::reclaim_before_alloc_retry_after_young)
    /// and attempt once more. `false` (the default, and the VM with the switch
    /// off): nothing ran; use [`reclaim_before_alloc_retry`](Self::reclaim_before_alloc_retry).
    /// The same rooting precondition as `reclaim_before_alloc_retry`.
    fn reclaim_young_before_alloc_retry(&mut self) -> bool {
        false
    }

    /// gcg g3/o: the rest of the staged reclaim. Default: the whole door.
    fn reclaim_before_alloc_retry_after_young(&mut self) -> bool {
        self.reclaim_before_alloc_retry()
    }

    /// gci i13/oa item 4: [`reclaim_before_alloc_retry`](Self::reclaim_before_alloc_retry)
    /// for a refused allocation of `bytes` (header plus data; `0` = unknown),
    /// so the VM can run the collection a G1 humongous request asks for under
    /// HotSpot's `G1 Humongous Allocation` cause. Same precondition and answer.
    /// Default: the unsized door (mock contexts, and any context that ignores
    /// the size).
    fn reclaim_before_alloc_retry_for(&mut self, bytes: usize) -> bool {
        let _ = bytes;
        self.reclaim_before_alloc_retry()
    }

    /// gci i13/oa item 4: [`reclaim_young_before_alloc_retry`](Self::reclaim_young_before_alloc_retry)
    /// for a refused allocation of `bytes` (see
    /// [`reclaim_before_alloc_retry_for`](Self::reclaim_before_alloc_retry_for)).
    /// Default: the unsized stage.
    fn reclaim_young_before_alloc_retry_for(&mut self, bytes: usize) -> bool {
        let _ = bytes;
        self.reclaim_young_before_alloc_retry()
    }

    /// Run the one extra, allocation-failure-only reclaim step after a first
    /// reclaim/retry has itself exposed G1 humongous fragmentation.  This is
    /// deliberately a separate hook: the ordinary two-attempt ladder remains
    /// the contract for every other collector and failure shape.  The VM
    /// answers `true` only when the second refusal proved that total Free
    /// capacity exists but its longest run is too short, then performs the
    /// single target-window compaction needed to make that capacity usable.
    ///
    /// The same rooting precondition as [`reclaim_before_alloc_retry`](Self::reclaim_before_alloc_retry)
    /// applies.  The default keeps non-VM contexts and all non-G1 collectors
    /// on their existing bounded two-attempt path.
    fn reclaim_after_fragmented_humongous_retry(&mut self) -> bool {
        false
    }

    /// zr z8/nr: the retry after [`reclaim_before_alloc_retry`](Self::reclaim_before_alloc_retry)
    /// (or the staged door) has just FAILED. On ZGC, when that ladder skipped
    /// its clear-soft rung because its collection had made room for the
    /// request (the room was then taken by another thread, or by this
    /// native's own earlier allocation), the VM runs the skipped rung now and
    /// answers `true`: retry once more before raising `OutOfMemoryError`.
    /// `false` (nothing ran) everywhere else, so HotSpot's rule holds: no
    /// `OutOfMemoryError` without a clear-soft collection after the refusal.
    /// The same rooting precondition as `reclaim_before_alloc_retry`.
    fn reclaim_after_failed_alloc_retry(&mut self) -> bool {
        false
    }

    /// zr z8/nr: should a native whose allocation door is otherwise
    /// one-attempt (`new_ref_array`, `instantiate`) take the fallible door and
    /// the reclaim ladder instead, because it can prove it holds no unpinned
    /// reference? `true` on every collector of the VM's heap (Generational,
    /// G1 and ZGC since gci i48/zl2) with `CRATONVM_ZGC_NATIVE_DOORS_COLLECT`
    /// on (default ON; the name is historical); `=0` and this trait default
    /// keep every such native on its historical one-attempt door.
    fn native_doors_collect_on_refusal(&self) -> bool {
        false
    }

    /// zt t1/nd: is the VM's heap the ZGC backend? For a native whose
    /// behaviour differs by collector only under a ZGC kill switch (the FFM
    /// memo `ADDRESS` return, `CRATONVM_ZGC_FFM_MINT_COLLECT`). Default
    /// `false` (a context with no VM heap).
    fn heap_is_zgc(&self) -> bool {
        false
    }

    /// zt t27/zc10: is a ZGC concurrent mark in flight on the VM's heap? For
    /// the inherited-`ThreadLocal` take, which keeps a started child's values
    /// pinned to it until the cycle's closing pause
    /// (`CRATONVM_ZGC_ITL_DEFER_IN_CYCLE`). Default `false` (a context with no
    /// VM heap, every non-ZGC heap).
    fn zgc_concurrent_mark_active(&self) -> bool {
        false
    }

    /// zt t6/tc: ZGC's `-XX:SoftMaxHeapSize` for the manageable JMX flag row:
    /// `(soft maximum bytes, -Xmx bytes, origin kind)` with the origin kind
    /// 0 = ergonomic default, 1 = command line, 2 = management. Default
    /// `None` (no ZGC heap behind this context).
    fn zgc_soft_max_heap_flag(&self) -> Option<(u64, u64, u8)> {
        None
    }

    /// zt t6/tc: `HotSpotDiagnosticMXBean.setVMOption("SoftMaxHeapSize", ..)`
    /// on a ZGC heap (the caller has range-checked `bytes` against `-Xmx`).
    /// `false` when there is no ZGC heap behind this context (the default).
    fn zgc_set_soft_max_heap_flag(&mut self, bytes: u64) -> bool {
        let _ = bytes;
        false
    }

    /// zt t12/tg5 (`zgc-zt11tg4-*` item 4): the exit natives' director stop
    /// (`System.exit`, `Runtime.exit`, `Shutdown.halt0`), as HotSpot's
    /// `before_exit` stops its ZGC director: stop the director tick and wait,
    /// GC-safe on this context's thread and bounded, for a tick pause that was
    /// already past its last stop check, so its `GC(n)` lines print before
    /// the `gc,exit` summary the pre-exit hook writes. Called before the
    /// pre-exit hook, outside any blocking region. Default: nothing (no ZGC
    /// heap behind this context).
    fn zgc_stop_director_tick_before_exit(&mut self) {}

    /// zr z8/nr: [`instantiate`](Self::instantiate) with
    /// `NativeConstruct::NoConstructor` whose heap refusal answers `Ok(None)`
    /// instead of raising, for a caller that then reclaims and retries.
    /// `Ok(None)` also means the class is unknown, as for `instantiate`. The
    /// default is `instantiate` itself (no fallible door).
    fn try_instantiate_no_constructor(
        &mut self,
        class_id: ClassId,
    ) -> Result<Option<ObjectRef>, MethodCallFailed> {
        self.instantiate(class_id, NativeConstruct::NoConstructor)
    }

    /// Component (element) class id of an array class `class_id`, or `None` if
    /// it is not an array class. Lets natives allocate a typed array matching a
    /// given array `Class` — e.g. `Arrays.copyOf(T[], n, a.getClass())` /
    /// `Collection.toArray(T[])`, where the result must be `a.getClass()` (e.g.
    /// `String[][]`), not a bare `Object[]`. The result is the `component_class_id`
    /// the caller feeds back into `new_ref_array`. Default `None` (callers fall
    /// back to `Object[]`).
    fn array_component_class_id(&self, _class_id: ClassId) -> Option<ClassId> {
        None
    }

    /// Get the length of an array object.
    fn array_length(&self, obj: ObjectRef) -> usize;

    /// Whether `obj` is an array object (as opposed to an ordinary instance).
    ///
    /// This is a heap object-kind check — it does NOT go through
    /// `class_id_of_object`/`class_name_of_id`, which for a heap-allocated
    /// reference array report the *component* class (arrays store their element
    /// class id + an array kind flag rather than a distinct `[L…;` class id), so
    /// a class-name prefix test cannot reliably detect arrays. Default `false`
    /// (mock contexts without a heap); the VM overrides it.
    fn object_is_array(&self, _obj: ObjectRef) -> bool {
        false
    }

    /// Read an array element by index.
    ///
    /// `index` must be in `0..array_length(obj)`. The trait does NOT validate
    /// it (M4a): the caller is responsible for range-checking against
    /// [`array_length`](Self::array_length), and the implementation MUST
    /// bounds-check and MUST NOT read out of range (fail safe — e.g. default
    /// value or VM error — never an out-of-bounds heap read).
    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value;

    /// Write an array element by index.
    ///
    /// `index` must be in `0..array_length(obj)`. The trait does NOT validate
    /// it (M4a): the caller is responsible for range-checking against
    /// [`array_length`](Self::array_length), and the implementation MUST
    /// bounds-check and MUST NOT write out of range (fail safe — no-op or VM
    /// error — never an out-of-bounds heap write).
    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value);

    // -- Bulk primitive-array intrinsics (perf path) -----------------------
    //
    // These default to the per-element loop so existing mock contexts compile
    // unchanged. The VM override (`NativeContextImpl`) replaces the loop with
    // a single `ptr::copy_nonoverlapping` against the compact array payload,
    // eliminating ~N virtual dispatches + Value boxes per byte/char copy.
    //
    // Callers in native-io / native-builtins / native-collections (e.g.
    // ZipFile entry reads, FileInputStream/OutputStream, String construction
    // from char[]) currently loop element-by-element through
    // `set_array_element` / `get_array_element`; migrating those callers to
    // these intrinsics is the round-2 perf win.

    /// Bulk copy from a host byte slice into a Java `byte[]` array at the given offset.
    /// Returns `true` on success, `false` on bounds error / wrong array kind.
    ///
    /// The default impl loops via `set_array_element` (correct but slow). The
    /// VM override uses `ptr::copy_nonoverlapping` against the array's raw
    /// payload, which is ~50-100x faster for multi-KB copies.
    fn write_byte_array_from(&mut self, arr: ObjectRef, dst_off: usize, src: &[u8]) -> bool {
        // CRIT fix: previously the per-element fallback wrote unconditionally,
        // so a too-large `src` would silently overflow past the array end
        // (or panic on the underlying `set_array_element`, depending on impl)
        // while the function reported `true` to the caller. Guard the entry
        // with the same bounds check the VM override performs.
        let dst_len = self.array_length(arr);
        if dst_off
            .checked_add(src.len())
            .map_or(true, |end| end > dst_len)
        {
            return false;
        }
        for (i, b) in src.iter().enumerate() {
            self.set_array_element(arr, dst_off + i, Value::Int(*b as i8 as i32));
        }
        true
    }

    /// Bulk read from a Java `byte[]` array into a host buffer at the given offset.
    /// Returns the number of bytes actually copied (0 on bounds error / wrong array kind).
    ///
    /// The default impl loops via `get_array_element`; the VM override
    /// `memcpy`s from the array's raw payload.
    fn read_byte_array_into(&self, arr: ObjectRef, src_off: usize, dst: &mut [u8]) -> usize {
        // CRIT fix: clamp to the array length up front so we never call
        // `get_array_element` out of bounds (which can panic in real
        // contexts) and so the return value honours the documented
        // "0 on bounds error" contract when `src_off` itself is past end.
        let src_len = self.array_length(arr);
        if src_off > src_len {
            return 0;
        }
        let available = src_len - src_off;
        let n = available.min(dst.len());
        for i in 0..n {
            match self.get_array_element(arr, src_off + i) {
                Value::Int(v) => dst[i] = v as u8,
                _ => return i,
            }
        }
        n
    }

    /// Bulk read from a Java `char[]` array into a host `u16` buffer.
    ///
    /// Default: per-element loop. VM override: single `copy_nonoverlapping`
    /// of `len * 2` bytes from the compact char-array payload (chars are
    /// stored as little-endian `u16` matching host order on supported
    /// targets — same convention `read_char_array_bulk` in `vm_heap`).
    fn read_char_array_into(&self, arr: ObjectRef, src_off: usize, dst: &mut [u16]) -> usize {
        // CRIT fix: same out-of-bounds guard as `read_byte_array_into`.
        let src_len = self.array_length(arr);
        if src_off > src_len {
            return 0;
        }
        let available = src_len - src_off;
        let n = available.min(dst.len());
        for i in 0..n {
            match self.get_array_element(arr, src_off + i) {
                Value::Int(v) => dst[i] = v as u16,
                _ => return i,
            }
        }
        n
    }

    /// Bulk read from a Java `int[]` array into a host `i32` buffer.
    ///
    /// Default: per-element loop. VM override: one `copy_nonoverlapping` of
    /// `n * 4` bytes from the compact int-array payload. Added for the
    /// BouncyCastle digest kernels, whose whole reason for existing as a
    /// native is that the per-element `get_array_element` path costs a virtual
    /// dispatch plus a `Value` box per word — 16 of them per compression
    /// block, which is most of what the intrinsic was meant to remove.
    fn read_int_array_into(&self, arr: ObjectRef, src_off: usize, dst: &mut [i32]) -> usize {
        // Same out-of-bounds guard as `read_byte_array_into`.
        let src_len = self.array_length(arr);
        if src_off > src_len {
            return 0;
        }
        let available = src_len - src_off;
        let n = available.min(dst.len());
        for i in 0..n {
            match self.get_array_element(arr, src_off + i) {
                Value::Int(v) => dst[i] = v,
                _ => return i,
            }
        }
        n
    }

    /// Bulk write from a host `i32` buffer into a Java `int[]` array at the
    /// given destination offset. Returns `true` on success, `false` on bounds
    /// error / wrong array kind. Symmetric to [`read_int_array_into`](Self::read_int_array_into).
    fn write_int_array_from(&mut self, arr: ObjectRef, dst_off: usize, src: &[i32]) -> bool {
        // Bounds-check the destination before any writes so we never overflow
        // past the array end while reporting `true` (the CRIT fix the byte and
        // char twins above carry).
        let dst_len = self.array_length(arr);
        if dst_off
            .checked_add(src.len())
            .map_or(true, |end| end > dst_len)
        {
            return false;
        }
        for (i, v) in src.iter().enumerate() {
            self.set_array_element(arr, dst_off + i, Value::Int(*v));
        }
        true
    }

    /// Bulk write from a host `u16` buffer into a Java `char[]` array at
    /// the given destination offset. Returns `true` on success, `false` on
    /// bounds error / wrong array kind.
    ///
    /// Copy `src` into a primitive array's payload as raw bytes.
    ///
    /// Returns `false` on a bounds error, a non-array receiver, a
    /// reference array, or a context with no bulk path.
    ///
    /// # Why a byte-level API rather than one per element type
    ///
    /// The typed helpers above cover `byte`, `char` and `int`, which is
    /// where they were needed. Adding `float`, `double`, `long` and
    /// `short` would be four more near-identical methods here and four
    /// more overrides in the VM, all doing the same `copy_nonoverlapping`
    /// once the width is known.
    ///
    /// The caller that needs them — GPU array marshalling — already holds
    /// its data as a flat `Vec<u8>` in native-endian order, because that
    /// is what a device buffer is. So the byte form is not a workaround
    /// for it; it is the shape it actually wants, and one pair of methods
    /// serves every primitive width.
    ///
    /// Native-endian, matching the `to_ne_bytes` the callers already use:
    /// host and heap are the same machine, so this is a copy and not a
    /// conversion.
    ///
    /// Reference arrays are rejected outright. Writing raw bytes over
    /// object references would hand the collector pointers it never
    /// issued.
    fn write_primitive_array_bytes(
        &mut self,
        _arr: ObjectRef,
        _byte_off: usize,
        _src: &[u8],
    ) -> bool {
        false
    }

    /// Read a primitive array's payload as raw bytes into `dst`.
    ///
    /// Returns the number of bytes copied, or `0` on any of the
    /// conditions [`write_primitive_array_bytes`](Self::write_primitive_array_bytes)
    /// rejects. Zero is unambiguous here: a caller asking for zero bytes
    /// has nothing to do either way.
    fn read_primitive_array_bytes(
        &self,
        _arr: ObjectRef,
        _byte_off: usize,
        _dst: &mut [u8],
    ) -> usize {
        0
    }

    /// A hardware-atomic read-modify-write of `width` (4 or 8) bytes at byte
    /// `byte_off` of a primitive array's payload (round nio20 wave 2, lane L4):
    /// the `Unsafe` int/long CAS / getAndAdd / getAndSet / volatile access over
    /// an array of ANOTHER element type (`compareAndSetFloat` on a `float[]`, a
    /// byte-array view `VarHandle`). `f(current)` answers the replacement, or
    /// `None` to leave the bytes (a plain atomic load); the call answers
    /// `Some((witnessed, wrote))`. The value is the native-endian (= little-
    /// endian on every supported host) integer the bytes hold, zero-extended.
    ///
    /// It is a `compare_exchange` loop on an `AtomicU32` / `AtomicU64` at the
    /// payload address, so it is atomic against compiled code's `lock cmpxchg`
    /// / `lock xadd` on the same memory, not only against other interpreter
    /// calls. `None` when the receiver is not a primitive array, the range is out
    /// of bounds, the address is not naturally aligned for `width`, or the
    /// payload has no flat address (the default, a G1 humongous array): the
    /// caller then falls back to its locked path.
    fn primitive_array_atomic_update(
        &mut self,
        _arr: ObjectRef,
        _byte_off: usize,
        _width: usize,
        _f: &dyn Fn(u64) -> Option<u64>,
    ) -> Option<(u64, bool)> {
        None
    }

    /// The misaligned arm of [`primitive_array_atomic_update`](Self::primitive_array_atomic_update)
    /// for OFF-HEAP memory (an `Arena` / `allocateMemory` block): a
    /// read-modify-write of `width` (2, 4 or 8) bytes at the misaligned `addr`
    /// as x86-64 `LOCK CMPXCHG`, HotSpot's instruction for the same `Unsafe`
    /// call, so it is atomic against plain stores into those bytes (JIT round 21
    /// proposal ATOMICS21-4). Same switches as the array arm
    /// (`CRATONVM_UNSAFE_MISALIGNED_HW_CAS`, and `CRATONVM_UNSAFE_SPLIT_LOCK_CAS`
    /// for an access crossing a cache line). `None`: the caller keeps its
    /// process lock -- the default, another architecture, a switch off.
    ///
    /// # Safety
    /// `addr .. addr + width` must be live, writable memory for the call.
    unsafe fn misaligned_raw_atomic_update(
        &self,
        _addr: *mut u8,
        _width: usize,
        _f: &dyn Fn(u64) -> Option<u64>,
    ) -> Option<(u64, bool)> {
        None
    }

    /// AUDIT 2026-05-17: symmetric to `read_char_array_into`. Used by
    /// `stream_decoder::refill` to populate the read-ahead char buffer
    /// in one shot instead of N `set_array_element` round-trips. The
    /// VM override `memcpy`s into the compact char-array payload.
    fn write_char_array_from(&mut self, arr: ObjectRef, dst_off: usize, src: &[u16]) -> bool {
        // CRIT fix: bounds-check the destination before any writes so we
        // never silently overflow past the array end while reporting `true`.
        let dst_len = self.array_length(arr);
        if dst_off
            .checked_add(src.len())
            .map_or(true, |end| end > dst_len)
        {
            return false;
        }
        for (i, c) in src.iter().enumerate() {
            self.set_array_element(arr, dst_off + i, Value::Int(*c as i32));
        }
        true
    }

    /// Bulk copy of array elements (primitive arrays only — for ref arrays
    /// the caller must do per-element typecheck). `src` and `dst` may alias
    /// (the VM override uses `copy_within` for same-array overlap, falling
    /// back to `copy_nonoverlapping` for distinct backings).
    ///
    /// Returns `true` on success, `false` on bounds / element-type mismatch.
    /// Even a zero-length copy only succeeds after the array kind, primitive
    /// element type, and offset bounds have been validated.
    fn bulk_array_copy(
        &mut self,
        src: ObjectRef,
        src_off: usize,
        dst: ObjectRef,
        dst_off: usize,
        len: usize,
    ) -> bool {
        if self.heap_kind_of(src) != ObjectKind::Array
            || self.heap_kind_of(dst) != ObjectKind::Array
        {
            return false;
        }
        let src_type = self.heap_element_type_of(src);
        let dst_type = self.heap_element_type_of(dst);
        if src_type != dst_type || src_type == ArrayElementType::Reference {
            return false;
        }
        let src_end = match src_off.checked_add(len) {
            Some(end) => end,
            None => return false,
        };
        let dst_end = match dst_off.checked_add(len) {
            Some(end) => end,
            None => return false,
        };
        if src_end > self.array_length(src) || dst_end > self.array_length(dst) {
            return false;
        }
        if len == 0 {
            return true;
        }

        for i in 0..len {
            if !value_matches_primitive_array(src_type, self.get_array_element(src, src_off + i)) {
                return false;
            }
        }

        let same_array = src.as_ptr() == dst.as_ptr();
        if same_array && dst_off > src_off {
            for i in (0..len).rev() {
                let v = self.get_array_element(src, src_off + i);
                self.set_array_element(dst, dst_off + i, v);
            }
        } else {
            for i in 0..len {
                let v = self.get_array_element(src, src_off + i);
                self.set_array_element(dst, dst_off + i, v);
            }
        }
        true
    }

    /// Bulk copy between two REFERENCE arrays, for a caller that has already
    /// proven every element of `src` assignable to `dst`'s component type
    /// (`System.arraycopy`'s identical-component-class case: every
    /// `ArrayList` grow, `Arrays.copyOf(Object[], int)`, and the
    /// `ArrayList.add(int, E)` / `remove(int)` shifts).
    ///
    /// Memmove semantics: `src` and `dst` may be the same array and the
    /// ranges may overlap. Every element store carries the collector's
    /// barriers exactly as [`set_array_element`](Self::set_array_element)
    /// would.
    ///
    /// Returns `true` only when the WHOLE range was copied. `false` means
    /// nothing was written and the caller must take its own per-element path:
    /// a context with no bulk path (this default), a non-array or primitive
    /// array, or an out-of-range offset. An implementation may not return
    /// `false` after it has written anything, because the caller's fallback
    /// would then re-copy an overlapping range from an already-overwritten
    /// source.
    ///
    /// Round 16 lane `arraylist`: the per-element loop through
    /// `get_array_element` / `set_array_element` validated and forwarded both
    /// receivers on every element, ~300 ns per element, which made a
    /// reference `System.arraycopy` hundreds of times HotSpot's
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    fn copy_reference_array_elements(
        &mut self,
        _src: ObjectRef,
        _src_off: usize,
        _dst: ObjectRef,
        _dst_off: usize,
        _len: usize,
    ) -> bool {
        false
    }

    /// JIT round 23 wave 2e (`jit-r23-w2-proxy-proposals.md` item 2): the
    /// CHECKED twin of [`copy_reference_array_elements`](Self::copy_reference_array_elements),
    /// for `System.arraycopy` between reference arrays whose components differ
    /// (`Object[]` into `T[]`: `ArrayList.toArray(T[])`, `Arrays.copyOf(U[],
    /// int, Class)`), copying FORWARD from `src_off` while each element is
    /// provably assignable to `dst_component` by a cheap test (null, or a
    /// plain object whose class is `dst_component` or a subclass of it).
    ///
    /// Answers `Some(n)`: elements `[0, n)` were copied with every barrier a
    /// per-element store carries, and element `n` (if `n < len`) is the first
    /// the cheap test did not prove -- the caller resumes its own full
    /// per-element check-then-write at `n`, so an `ArrayStoreException` and its
    /// partial commit are exactly the caller's. `None`: nothing was written
    /// (no fast path in this context -- this default --, a collector state
    /// that needs the per-element accessors, an overlapping backward copy, or
    /// a range the caller must reject itself).
    fn copy_reference_array_elements_checked(
        &mut self,
        _src: ObjectRef,
        _src_off: usize,
        _dst: ObjectRef,
        _dst_off: usize,
        _len: usize,
        _dst_component: ClassId,
    ) -> Option<usize> {
        None
    }

    /// Get the ObjectKind (Object or Array) of a heap object.
    fn heap_kind_of(&self, obj: ObjectRef) -> ObjectKind;

    /// Get the ArrayElementType of an array object.
    /// Returns `ArrayElementType::Reference` for non-array objects or reference arrays.
    fn heap_element_type_of(&self, obj: ObjectRef) -> ArrayElementType;

    /// An array's element type and length together, or `None` when `obj` is
    /// not a live array.
    ///
    /// The default composes the three single-answer accessors, so every
    /// implementation keeps working unchanged. A real heap should override it:
    /// `object_is_array`, `heap_element_type_of` and `array_length` each
    /// repeat the SAME membership walk and forwarding barrier, and callers
    /// that want all three answers (`StringBuilder`'s payload view asks for
    /// them on every `append`) pay that walk three times for one header.
    fn array_shape(&self, obj: ObjectRef) -> Option<(ArrayElementType, usize)> {
        if !self.object_is_array(obj) {
            return None;
        }
        Some((self.heap_element_type_of(obj), self.array_length(obj)))
    }

    /// Create a Java String object from a Rust &str. Returns the ObjectRef.
    ///
    /// Consults and populates the VM's interned-string pool: equal text yields
    /// the *same* ObjectRef. Use only for content that should behave like a
    /// string literal. For dynamically produced strings — `StringBuilder
    /// .toString()`, `substring`, etc. — use [`create_string_uninterned`]
    /// (Self::create_string_uninterned) so `==` reports them as distinct.
    ///
    /// On the VM that pool is weak on Generational by default (dead entries
    /// dropped at each pause, `vm/src/memory/string_table.rs`); it is a STRONG
    /// GC root that is never pruned on G1 / ZGC or with
    /// `CRATONVM_GC_WEAK_STRING_TABLE=0` (`vm/src/memory/roots.rs` section 5):
    /// there every distinct text passed here lives, and is re-scanned by every
    /// collection, for the life of the VM.
    /// Text computed from arguments (number formatting, `toString`, messages
    /// carrying a value) passed here is an unbounded leak, not only an
    /// identity divergence. The Java semantics that DO require the pool are
    /// `String.intern()` and names HotSpot interns (class, method and field
    /// names, stack-trace elements). See
    /// `docs/feature-designs/native-dispatch-architecture.md`.
    fn create_string(&mut self, text: &str) -> ObjectRef;

    /// The canonical pooled String for `text`: the one `String.intern()` and an
    /// equal literal answer. For callers whose Java semantics REQUIRE identity
    /// with literals (`String.intern()`, and the names HotSpot interns: class,
    /// method and field names, stack-trace elements).
    ///
    /// Today this is exactly [`create_string`](Self::create_string) on every
    /// implementation. It exists so those callers say so: the structural fix of
    /// `r13w8-bigdec-dynamic-strings-interned-into-a-never-pruned-root-FIXED-20260929.md`
    /// (design A) makes `create_string` answer a pool hit or a FRESH string, and
    /// this the only path that inserts. Round 13 wave 10 (lane misc10).
    fn intern_string(&mut self, text: &str) -> ObjectRef {
        self.create_string(text)
    }

    /// `String.intern()` on `receiver`, whose exact text is `text`, when the
    /// implementation pools the RECEIVER on a miss (JLS: "this String object is
    /// added to the pool and a reference to this String object is returned"):
    /// the pooled String equal to it, or `receiver` itself, now pooled.
    ///
    /// `None` means this implementation does not (the default, and the VM
    /// under `CRATONVM_STRING_INTERN_RETURNS_RECEIVER=0`): the caller then
    /// takes [`intern_string`](Self::intern_string), which pools a NEW String on
    /// a miss. gcg g2/w (`common-w9c-proposal-weak-string-intern-table` step 3).
    fn intern_string_receiver(&mut self, receiver: ObjectRef, text: &str) -> Option<ObjectRef> {
        let _ = (receiver, text);
        None
    }

    /// Does this VM's String intern pool hold its entries WEAKLY (an interned
    /// String nothing else references is collected, as in HotSpot)? The VM
    /// answers `memory::string_table::weak_table_engaged` (Generational with
    /// `CRATONVM_GC_WEAK_STRING_TABLE` on, the default). The surrogate pool
    /// (`lang_string::intern_unrepresentable`) follows it with JNI weak global
    /// handles. gci i4/wr (`gcg-g2w-string-table-review-findings` item 2).
    ///
    /// Default `false`: a mock context keeps strong handles.
    fn string_pool_is_weak(&self) -> bool {
        false
    }

    /// Create a Java String object from a Rust &str **without** interning.
    ///
    /// Always allocates a fresh, distinct String object — the correct
    /// constructor for dynamically produced strings, matching the JVM spec
    /// requirement that only literals and `String.intern()` participate in
    /// the constant pool. Defaults to [`create_string`](Self::create_string)
    /// for mock/test contexts.
    fn create_string_uninterned(&mut self, text: &str) -> ObjectRef {
        self.create_string(text)
    }

    /// Create a dynamic String at a native-call safepoint when the caller has
    /// no unpinned Java references.  The VM implementation may collect before
    /// allocating; the default keeps mock contexts and legacy implementations
    /// on the ordinary uninterned path.
    fn create_string_uninterned_gc_safe(&mut self, text: &str) -> ObjectRef {
        self.create_string_uninterned(text)
    }

    /// Probe a per-thread cache for an ASCII case-conversion result. The
    /// cache alternates two immutable values so consecutive calls stay
    /// observably distinct.
    fn get_ascii_case_string_cached(
        &mut self,
        _source: ObjectRef,
        _locale: Option<ObjectRef>,
        _upper: bool,
    ) -> Option<ObjectRef> {
        None
    }

    /// Create and retain the alternating pair for an ASCII case conversion.
    fn create_ascii_case_string_cached(
        &mut self,
        _source: ObjectRef,
        _locale: Option<ObjectRef>,
        text: &str,
        _upper: bool,
    ) -> ObjectRef {
        self.create_string_uninterned_gc_safe(text)
    }

    /// Populate an *already-allocated* `java/lang/String` object's backing
    /// fields directly from raw UTF-16 code `units`, using the same
    /// Latin1-fits-in-a-byte bulk scan + little-endian compact-string layout
    /// as [`create_string`](Self::create_string). Unlike `create_string`,
    /// this does NOT allocate the `String` object itself and does NOT touch
    /// the intern pool — it exists for native `<init>` overrides
    /// (`String(char[])`, `String(char[], int, int)`) that intercept
    /// construction *after* `new` has already allocated `this`: a
    /// constructor native must mutate `this` in place, not return a
    /// different object identity.
    ///
    /// Preserves raw code units byte-for-byte (including unpaired
    /// surrogates), unlike routing through a Rust `&str`, which cannot
    /// represent those. Returns `false` only on backing-array allocation
    /// failure (heap exhaustion) — the caller should surface a catchable
    /// `OutOfMemoryError`.
    ///
    /// Default impl (mock/test contexts, which treat strings as opaque
    /// objects): stores the units in a plain `char[]` at field 0 via the
    /// generic array/field primitives. The VM override replaces this with
    /// the exact compact-string layout used by every other String natively.
    ///
    /// # This IS the lossless String writer — there is no missing primitive
    ///
    /// `create_string`/`create_string_uninterned`/`create_string_uninterned_gc_safe`
    /// all take a `&str`, and a Rust `str` cannot hold an unpaired UTF-16
    /// surrogate — so a native that reads a Java `String` losslessly and then
    /// hands the units to any of those has thrown the surrogate away at the
    /// LAST step. The recurring conclusion from that is "`NativeContext` needs
    /// a `create_string_from_utf16`". It does not: `new_object("java/lang/
    /// String")` followed by this method is that constructor, it is what the
    /// `String(char[])` / `String(char[], int, int)` overrides already use, and
    /// on the VM it lands in the same `populate_java_string_fields` that
    /// `create_java_string_from_units` uses — the one place that decides a
    /// compact `String`'s coder and byte order.
    ///
    /// `native-builtins`' `lang_string::sb_string_from_units` packages the pair
    /// (plus the GC pin and the well-formed fast path) into a single call, and
    /// is what every `String`-returning native in that file should end in.
    ///
    /// A third spelling of the same construction would be a second encoding of
    /// one concept, reconciled only where the bytes are consumed. Extend or
    /// re-route this one instead.
    fn init_string_from_units(&mut self, this: ObjectRef, units: &[u16]) -> bool {
        // gc-common w19-g: `this` is pinned across the backing array's
        // allocation and re-read for the store (it was held raw). Only a
        // context that keeps this default AND moves objects needs it -- the
        // VM overrides the method -- but the default is the documented shape.
        let this_pin = self.pin_native_root(this);
        let arr = self.new_array(ArrayElementType::Char, units.len());
        for (i, &u) in units.iter().enumerate() {
            self.set_array_element(arr, i, Value::Int(u as i32));
        }
        let this_now = self.read_native_pin(this_pin, this);
        self.unpin_native_roots(this_pin);
        self.set_field(this_now, 0, Value::Object(Some(arr)));
        true
    }

    /// Read a Java String object back to a Rust String.
    ///
    /// LOSSY BY CONSTRUCTION for one input: a Rust `String` cannot hold an
    /// unpaired UTF-16 surrogate, so any text containing one comes back with
    /// U+FFFD substituted. That is fine wherever the result is only inspected
    /// (a class name, a charset name, a flag) and wrong wherever it is handed
    /// back to Java. Use [`read_string_units`] there.
    fn read_string(&self, obj: ObjectRef) -> Option<String>;

    /// Copy an entire ASCII String into caller-owned storage without Java
    /// allocation, safepoints, or locks held across safepoints. None declines
    /// unsupported storage, an insufficient buffer, or an unresolved layout.
    /// Only dst[..returned_length] is meaningful on success.
    fn read_ascii_string_into(&self, _obj: ObjectRef, _dst: &mut [u8]) -> Option<usize> {
        None
    }

    /// Read a Java String object as UTF-16 code units, losing nothing.
    ///
    /// The units-exact counterpart of [`read_string`], and the fourth member of
    /// the units-exact family beside [`read_char_array_into`],
    /// [`write_char_array_from`] and [`init_string_from_units`]. `G55-1` N3
    /// nominated it because `native-collections` cannot decode a String itself
    /// -- it does not depend on `native-builtins`, and a local decoder would be
    /// a second encoding of the compact-string layout.
    ///
    /// The default is deliberately LOSSY -- it is `read_string` widened -- so
    /// that no implementation regresses by not overriding it, and so the
    /// surrogate hazard stays exactly where it already was for anyone who does
    /// not. The VM overrides it with the real reader.
    fn read_string_units(&self, obj: ObjectRef) -> Option<Vec<u16>> {
        self.read_string(obj).map(|s| s.encode_utf16().collect())
    }

    /// Read UTF-16 code units into caller-provided storage. Returns `None` if
    /// the object cannot be read as a string or does not fit. Implementations
    /// with direct heap access can avoid allocating the intermediate `Vec`.
    fn read_string_units_into(&self, obj: ObjectRef, out: &mut [u16]) -> Option<usize> {
        let units = self.read_string_units(obj)?;
        if units.len() > out.len() {
            return None;
        }
        out[..units.len()].copy_from_slice(&units);
        Some(units.len())
    }

    /// Append the UTF-16 code units of `obj` -- which the CALLER knows is a
    /// `java.lang.String` (a parameter or receiver the verifier types as one;
    /// `String` is final) -- to `out`, losing nothing; `false` (with `out`
    /// unchanged) when this context cannot read it that way, and the caller
    /// then takes its own reader. JIT r23 wave 5, lane L5
    /// (`CRATONVM_STRING_REGEX_KNOWN_STRING_READ`): the String regex natives'
    /// per-call reads. The default is the units reader above; the VM reads the
    /// heap directly, without the per-field layout resolution or the class
    /// identity lookup that a speculative caller needs.
    fn append_known_string_units(&self, obj: ObjectRef, out: &mut Vec<u16>) -> bool {
        match self.read_string_units(obj) {
            Some(units) => {
                out.extend_from_slice(&units);
                true
            }
            None => false,
        }
    }

    /// Build a fresh Java String from UTF-16 code units, losing nothing.
    ///
    /// The write half of [`read_string_units`], and the reason a units-exact
    /// READER alone would not have been enough: a value read without loss and
    /// then written back through [`create_string`] is lossy again at the last
    /// step. NOT [`init_string_from_units`], which fills an ALREADY-ALLOCATED
    /// receiver and assumes the `char[]` layout -- a real JDK `String` is
    /// `byte[]` plus a coder, so that default is wrong for the mode this
    /// matters most in.
    ///
    /// Default is lossy for the same reason as above.
    fn create_string_from_units(&mut self, units: &[u16]) -> ObjectRef {
        let text = String::from_utf16_lossy(units);
        self.create_string(&text)
    }

    /// Create a lossless dynamic String at a native-call safepoint when the
    /// caller has no unpinned Java references. The VM may collect while
    /// allocating; the default preserves legacy and mock behavior.
    fn create_string_from_units_gc_safe(&mut self, units: &[u16]) -> ObjectRef {
        self.create_string_from_units(units)
    }

    /// Return the raw Java `String.hashCode()` for a confirmed String object.
    /// `None` means "no hash was computed": `obj` is not a String, or an
    /// overriding implementation could not read its character storage.
    /// Implementations may override this to inspect compact storage without
    /// allocating a host String.
    fn java_string_hash_code(&self, obj: ObjectRef) -> Option<i32> {
        self.read_string(obj).map(|text| {
            text.encode_utf16().fold(0i32, |hash, unit| {
                hash.wrapping_mul(31).wrapping_add(unit as i32)
            })
        })
    }

    /// Compare two confirmed Java Strings without routing through Java
    /// dispatch.
    ///
    /// `Some(_)` is an answer. `None` means **the comparison was not made** —
    /// an operand is not a String, or an overriding implementation could not
    /// read one operand's character storage — and the caller must fall back to
    /// dispatching `String.equals`. An implementation must never report `false`
    /// for a pair it did not actually read: doing so silently turned every
    /// `ConcurrentHashMap.get` on a String key into a miss (see
    /// `chm-get-misses-stored-key-in-process-RETIRED-20260804.md`).
    fn java_strings_equal(&self, a: ObjectRef, b: ObjectRef) -> Option<bool> {
        Some(self.read_string(a)? == self.read_string(b)?)
    }

    /// Get or create the java.lang.Class mirror for the given ClassId.
    fn get_class_mirror(&mut self, class_id: ClassId) -> ObjectRef;

    /// [`get_class_mirror`](Self::get_class_mirror) for a native that can
    /// collect and retry: `None` when the heap refused a mirror that did not
    /// exist yet, WITHOUT the heap-exhaustion unwind. The caller then follows
    /// [`reclaim_before_alloc_retry`](Self::reclaim_before_alloc_retry)'s
    /// rooting precondition. Default: the infallible door (so a context that
    /// does not override it never answers `None`). The VM overrides it (ZGC
    /// since zr z4/ab; Generational and G1 since gci i22/oo), where a lazily
    /// created mirror on a full heap used to
    /// raise `OutOfMemoryError` from `Object.getClass()` with the garbage that
    /// would have made room still uncollected -- HotSpot made the mirror when
    /// it loaded the class, so its `getClass()` never allocates.
    fn try_get_class_mirror(&mut self, class_id: ClassId) -> Option<ObjectRef> {
        Some(self.get_class_mirror(class_id))
    }

    /// zs s7/nd6: reserve the `java.lang.Class` mirror object of a class
    /// about to be defined, through the fallible door (no collection here;
    /// the caller runs the reclaim ladder on [`MirrorReservation::Refused`]).
    /// `pooled_name` (internal form; a named, non-hidden define) also interns
    /// the mirror's name. The caller pins a [`MirrorReservation::Reserved`]
    /// object across the define and hands it to
    /// [`adopt_reserved_class_mirror`](Self::adopt_reserved_class_mirror).
    /// Default: [`MirrorReservation::Unsupported`] (the define mints its
    /// mirror afterwards, as before). The VM overrides it on ZGC.
    fn try_reserve_class_mirror(&mut self, _pooled_name: Option<&str>) -> MirrorReservation {
        MirrorReservation::Unsupported
    }

    /// zs s7/nd6: make a reserved mirror object (current address) the mirror
    /// of the just-defined `class_id`; a mirror that already exists is
    /// answered instead (the reservation is then garbage). `None`: not
    /// adopted; the caller mints as before.
    fn adopt_reserved_class_mirror(
        &mut self,
        _class_id: ClassId,
        _reserved: ObjectRef,
    ) -> Option<ObjectRef> {
        None
    }

    /// Allocate an object with the given class_id and number of fields,
    /// without loading a class (for synthetic objects).
    fn alloc_object(&mut self, class_id: ClassId, num_fields: usize) -> ObjectRef;

    /// Historical "fallible" twin of [`alloc_object`](Self::alloc_object).
    ///
    /// **It is not fallible, and nothing overrides it** (pinned by
    /// `native-api/tests/layout_alias_coverage.rs`): the default below
    /// forwards to `alloc_object`, which inside a native callback UNWINDS on
    /// exhaustion (the VM's `NativeAllocOom` channel, converted to a
    /// catchable `OutOfMemoryError` by the native funnel) and outside one
    /// keeps the VM's historical infallible entry. So `None` is never seen
    /// from the VM. About 2,000 call sites reach it through
    /// `try_alloc_concurrent_synthetic` while holding bare references, so it
    /// must never be made to collect either.
    ///
    /// A native that wants to SEE a refusal (and, having pinned everything,
    /// reclaim and retry) calls [`try_alloc_object`](Self::try_alloc_object).
    fn try_alloc_object_gc_safe(
        &mut self,
        class_id: ClassId,
        num_fields: usize,
    ) -> Option<ObjectRef> {
        Some(self.alloc_object(class_id, num_fields))
    }

    /// Fallible object allocation that never collects and never unwinds:
    /// `None` when the heap refuses.
    ///
    /// The object twin of [`try_new_array`](Self::try_new_array) /
    /// [`try_new_ref_array`](Self::try_new_ref_array), with the same layout
    /// rules as [`alloc_object`](Self::alloc_object): the VM override runs the
    /// same layout-alias observation and the same clamp of `num_fields` up to
    /// the class's real field count, then tries the same no-GC arms, and
    /// answers `None` where `alloc_object` would unwind (or abort).
    ///
    /// The caller decides what a refusal means. It may call
    /// [`reclaim_before_alloc_retry`](Self::reclaim_before_alloc_retry) and
    /// try once more **only** when it holds no unpinned `ObjectRef` in a Rust
    /// local (see that method); otherwise it reports `OutOfMemoryError`.
    /// `native-builtins`' `try_alloc_concurrent_synthetic_reclaiming` is that
    /// ladder for a synthetic object.
    ///
    /// Default: `Some(self.alloc_object(..))`, so mock and non-VM contexts
    /// keep their behaviour; the VM overrides it (gc-common w8-b,
    /// `common-w3c-native-callback-allocations-cannot-collect`).
    fn try_alloc_object(&mut self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        Some(self.alloc_object(class_id, num_fields))
    }

    /// Get the number of fields (slots) of a heap object.
    fn object_num_fields(&self, obj: ObjectRef) -> usize;

    /// The bytes `obj` occupies on the heap, header included, as the
    /// allocator charged them: the COMPACT body for an object whose backend
    /// planned one (`GC_FLAG_COMPACT`), `HEADER_SIZE + num_slots * SLOT_SIZE`
    /// for a legacy object, and the natural-width, 8-aligned data area for an
    /// array. `None` when this context cannot read a heap header (mocks), or
    /// `obj` is not a sizable object; callers fall back to their own
    /// estimate.
    ///
    /// gc-common w5-e (`common-w4e-object-size-reports-legacy-footprint`):
    /// `object_num_fields` is the FIELD COUNT on a compact object, so
    /// `fields * SLOT_SIZE` over-sizes it by up to 16x per field. This is
    /// what `Instrumentation.getObjectSize` answers with.
    fn heap_object_footprint(&self, obj: ObjectRef) -> Option<usize> {
        let _ = obj;
        None
    }

    /// Bytes of the Java heap that are **currently occupied by live objects**
    /// — the "used" half of `Runtime.freeMemory()`, `MemoryUsage.getUsed()`
    /// and every other heap-occupancy report.
    ///
    /// Occupancy, not a high-water mark. That distinction is the whole content
    /// of this doc comment, because a bump-pointer arena makes the two easy to
    /// confuse and the VM answered with the wrong one until 2026-09-08: the
    /// generational young sweep reclaims in place, into a free list, WITHOUT
    /// retreating the arena cursor, so the raw cursor stays pinned at its
    /// high-water mark for the rest of the process. Programs that size caches
    /// or buffers from the free heap then see a heap that never empties: H2's
    /// `TestValueMemory` is `totalMemory() - freeMemory()` around a pair of
    /// `System.gc()` calls, and on the generational arm it read 6715 KB against
    /// a 2928 KB threshold where occupancy is 2228.
    fn heap_allocated_bytes(&self) -> usize;

    /// Cumulative bytes the **calling** thread has allocated since it started,
    /// or `None` when the VM cannot account for it.
    ///
    /// This is the source for `com.sun.management.ThreadMXBean
    /// .getCurrentThreadAllocatedBytes` / `getThreadAllocatedBytes(long)`.
    /// `None` is the honest answer a mock or a thread-less context gives, and
    /// the bean turns it into the JMM's documented `-1` plus
    /// `isThreadAllocatedMemorySupported() == false` — a "not supported" that
    /// callers already handle, rather than a fabricated number.
    ///
    /// The VM implementation reads `Tlab::thread_allocated_bytes`, which is
    /// live-cursor based and therefore sees compiled code's inline allocation
    /// as well as the interpreter's.
    fn current_thread_allocated_bytes(&self) -> Option<u64> {
        None
    }

    /// Cumulative bytes allocated by ALL threads over the life of the process,
    /// for `com.sun.management.ThreadMXBean.getTotalThreadAllocatedBytes`.
    ///
    /// Must never decrease. Deliberately NOT
    /// [`Self::heap_allocated_bytes`], which is an occupancy gauge and falls at
    /// every collection — a caller measuring a window containing a GC would get
    /// the difference of two occupancies rather than what it allocated.
    fn total_allocated_bytes(&self) -> Option<u64> {
        None
    }

    /// Bytes currently COMMITTED for the Java heap — backing storage the VM
    /// holds whether or not anything lives in it. `Runtime.totalMemory()`, the
    /// JMX heap `MemoryUsage.getCommitted()`, and (minus
    /// [`Self::heap_allocated_bytes`]) `Runtime.freeMemory()`.
    ///
    /// Capacity, never occupancy: `totalMemory()` is expected to move only when
    /// the heap grows or shrinks. See `gc::vm_heap::VmHeap::committed_bytes`.
    ///
    /// The default is the historical `Runtime.totalMemory()` stub, kept for
    /// mock/test contexts that have no heap; the VM overrides it.
    fn committed_heap_bytes(&self) -> usize {
        64 * 1024 * 1024
    }

    // -- ObjectStreamClass descriptor cache (WP0.2) --
    //
    // Backs `java.io.ObjectStreamClass.lookup(Class)`. See
    // `vm::runtime::serialization::oscache` for the rationale.

    /// Look up a previously-built `ObjectStreamClass` descriptor for
    /// `class_id`. Returns `None` if `lookup` has never been called for
    /// this class yet (the native then allocates a fresh descriptor and
    /// installs it via `osc_cache_put`).
    ///
    /// Default implementation returns `None` — mock contexts and other
    /// simple impls just never cache.
    fn osc_cache_get(&self, _class_id: ClassId) -> Option<ObjectRef> {
        None
    }

    /// Install a freshly-built `ObjectStreamClass` descriptor in the
    /// cache. Returns the `ObjectRef` that ends up cached (either
    /// `desc` on a successful insert, or the pre-existing entry if
    /// another thread raced us). Callers MUST use the returned ref as
    /// the result of `lookup` — discarding it would break the
    /// identity contract.
    ///
    /// Default implementation ignores the descriptor and returns `desc`
    /// unchanged — non-caching contexts behave as if every lookup
    /// builds a fresh descriptor.
    fn osc_cache_put(&self, _class_id: ClassId, desc: ObjectRef) -> ObjectRef {
        desc
    }

    // -- Bound `MethodType` table (JIT round 16, lane mh; MH3-2) --
    //
    // Backs `MethodHandle.bindTo`'s bound `type`; see
    // `crate::method_type_table` for the contract (per-VM, rooted by the VM's
    // `bound-method-types` native-roots row, java.*-only rows).

    /// The `MethodType` this VM's bound-type table holds for the method
    /// descriptor `desc`, or `None`. Default: `None` (a context with no table
    /// builds every bound type afresh, the pre-table behaviour).
    fn bound_method_type_get(&self, _desc: &str) -> Option<ObjectRef> {
        None
    }

    /// Offer `mt` (just built for `desc`) to this VM's bound-type table and
    /// answer the object the caller must use: the table's existing row, `mt`
    /// once stored, or `mt` when the table is full. The caller has already
    /// decided the row is shareable. Default: `mt` (nothing stored).
    fn bound_method_type_put(&self, _desc: &str, mt: ObjectRef) -> ObjectRef {
        mt
    }

    /// The JDK signature-polymorphic linker's rooted `Object[2]`
    /// (`MemberName`, MethodType appendix) for a class-keyed `linker:` key.
    /// The VM scans/remaps the pair and removes it when the caller unloads.
    /// A mock without a table declines, so every call resolves its linker.
    fn method_handle_linker_get(&self, _key: &str) -> Option<ObjectRef> {
        None
    }

    /// Publish a linker pair under its caller-class key, returning the earlier
    /// pair on a race or this pair if the bounded table refuses the insert.
    fn method_handle_linker_put(&self, _key: &str, pair: ObjectRef) -> ObjectRef {
        pair
    }

    // -- Volatile field access (for sun.misc.Unsafe / Atomics) --

    /// Read an object field with volatile (sequentially consistent) semantics.
    ///
    /// Same slot-index contract as [`get_field`](Self::get_field): `index`
    /// must be in range for `obj`'s class, the trait does NOT validate it
    /// (M4a), and the implementation MUST bounds-check and MUST NOT read out
    /// of range. Note `Unsafe` callers may pass an index derived from a
    /// Java-supplied field *offset* — implementations must treat such input as
    /// untrusted and validate it.
    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value;

    /// Write an object field with volatile (sequentially consistent) semantics.
    ///
    /// Same slot-index contract as [`set_field`](Self::set_field): `index`
    /// must be in range for `obj`'s class, the trait does NOT validate it
    /// (M4a), and the implementation MUST bounds-check and MUST NOT write out
    /// of range. `Unsafe`-sourced offsets are untrusted and must be validated
    /// by the implementation.
    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value);

    // -- Compare-and-swap --

    /// Compare-and-swap on an object field. Returns true if field contained
    /// `expected` and was updated to `new_val`.
    ///
    /// Same slot-index contract as [`set_field`](Self::set_field): `index`
    /// must be in range for `obj`'s class, the trait does NOT validate it
    /// (M4a), and the implementation MUST bounds-check and MUST NOT
    /// read/write out of range. On an out-of-range index the implementation
    /// must fail safe (return `false`), never touch out-of-bounds memory.
    fn compare_and_swap_field(
        &mut self,
        obj: ObjectRef,
        index: usize,
        expected: Value,
        new_val: Value,
    ) -> bool;

    /// audit-round5 fix #9 (HIGH): atomic `fetch_add` on an `int` instance
    /// field. Returns the *previous* value (matching `AtomicInteger.getAndAdd`
    /// / `AtomicI32::fetch_add` semantics).
    ///
    /// Default implementation is a `compare_and_swap_field` retry loop. The
    /// VM override should map this to a single `LOCK XADD` (one trait
    /// dispatch, no CAS spin under contention).
    ///
    /// On a field-type mismatch this returns `Err(MethodCallFailed)` wrapping
    /// an `IllegalArgumentException` rather than panicking: a panic crossing
    /// the native/Java boundary is unsound (it may unwind through JIT-compiled
    /// frames that are not unwind-safe).
    fn atomic_fetch_add_int(
        &mut self,
        obj: ObjectRef,
        index: usize,
        delta: i32,
    ) -> Result<i32, MethodCallFailed> {
        loop {
            let current = self.get_field_volatile(obj, index);
            // Bug 3 (CRIT type corruption): the previous default impl
            // silently fell back to old=0 for non-Int slots, then
            // CAS-wrote `Value::Int(delta)` over the existing slot —
            // corrupting both the numeric value and the field's type
            // tag (a Long field would become Int). Surface the
            // caller's mis-dispatch as a catchable Java exception
            // instead of panicking across the native boundary. The
            // Long → atomic_fetch_add_long delegation is intentionally
            // NOT done here because the int-variant's i32 return type
            // cannot losslessly carry a Long previous value; callers
            // must route via the correct accessor.
            let old = match current {
                Value::Int(v) => v,
                other => {
                    return Err(MethodCallFailed::InternalError(
                        RuntimeError::IllegalArgumentException {
                            message: format!(
                                "atomic_fetch_add_int: field {} on object is not Int: {:?}",
                                index, other
                            ),
                        }
                        .into(),
                    ));
                }
            };
            let new_val = Value::Int(old.wrapping_add(delta));
            if self.compare_and_swap_field(obj, index, current, new_val) {
                return Ok(old);
            }
        }
    }

    /// audit-round5 fix #9 (HIGH): atomic `fetch_add` on a `long` instance
    /// field — `AtomicLong.getAndAdd` / `AtomicI64::fetch_add` analogue.
    /// See `atomic_fetch_add_int` for the default-impl rationale.
    ///
    /// As with `atomic_fetch_add_int`, a field-type mismatch yields
    /// `Err(MethodCallFailed)` (`IllegalArgumentException`) instead of a
    /// panic that could unwind through JIT frames.
    fn atomic_fetch_add_long(
        &mut self,
        obj: ObjectRef,
        index: usize,
        delta: i64,
    ) -> Result<i64, MethodCallFailed> {
        loop {
            let current = self.get_field_volatile(obj, index);
            // Bug 3 (CRIT type corruption): refuse to silently rewrite a
            // non-Long slot. The previous default impl's `_ => 0` arm
            // turned a wrong-typed field (Int, Reference, …) into
            // `Value::Long(delta)`, permanently corrupting the slot's
            // type tag. A `panic!` here would unwind across the native
            // boundary and abort the VM, so instead `debug_assert!`
            // (loud in debug builds) and return the current value
            // unchanged in release — a type mismatch indicates a native
            // dispatch bug (caller used the wrong accessor).
            let old = match current {
                Value::Long(v) => v,
                other => {
                    return Err(MethodCallFailed::InternalError(
                        RuntimeError::IllegalArgumentException {
                            message: format!(
                                "atomic_fetch_add_long: field {} on object is not Long: {:?}",
                                index, other
                            ),
                        }
                        .into(),
                    ));
                }
            };
            let new_val = Value::Long(old.wrapping_add(delta));
            if self.compare_and_swap_field(obj, index, current, new_val) {
                return Ok(old);
            }
        }
    }

    // -- Object allocation without constructor --

    /// Allocate an uninitialized object instance by NAME, without running
    /// `<clinit>`. Returns None if the class cannot be loaded. A native that
    /// holds a `Class` mirror wants [`Self::instantiate`] (exact class, class
    /// initialized first).
    fn allocate_instance(&mut self, class_name: &str) -> Option<ObjectRef>;

    /// The one native "instantiate" funnel (round 15 wave 4, lane jdkonly6;
    /// JO5-1 of `docs/feature-designs/jit-ir-optimizations.md`):
    /// HotSpot's `InstanceKlass::allocate_instance(java_class)` /
    /// JNI `alloc_object` order -- initialize the EXACT class `class_id`
    /// (JVMS §5.5: a `<clinit>` failure is the answer and nothing is
    /// allocated), then allocate it with typed field defaults, then
    /// `construct`:
    ///
    /// * [`NativeConstruct::NoConstructor`] (`Unsafe.allocateInstance`, JNI
    ///   `AllocObject`): nothing runs; not registered for finalization
    ///   (HotSpot registers when `Object.<init>` returns, and none runs);
    /// * [`NativeConstruct::AncestorInit`] (the serialization constructor):
    ///   the ancestor's nullary `<init>` runs on the instance with
    ///   `invokespecial` semantics (so `Object.<init>` runs: a finalizable
    ///   instance is registered), and its throwable is the answer.
    ///
    /// The caller has made the instantiability checks it owes
    /// (abstract / interface / array), which differ per entry point.
    /// `Ok(None)`: `class_id` (or the ancestor) names no class.
    ///
    /// The default is built from the other trait methods, so a forwarding
    /// context stays correct; the VM overrides it (by id, never by name).
    fn instantiate(
        &mut self,
        class_id: ClassId,
        construct: NativeConstruct,
    ) -> Result<Option<ObjectRef>, MethodCallFailed> {
        let ancestor = match construct {
            NativeConstruct::NoConstructor => None,
            NativeConstruct::AncestorInit(ancestor) => match self.class_name_of_id(ancestor) {
                Some(name) => Some((ancestor, name)),
                None => return Ok(None),
            },
        };
        let Some(name) = self.class_name_of_id(class_id) else {
            return Ok(None);
        };
        self.initialize_class(class_id)?;
        let Some(obj) = self.allocate_instance(&name) else {
            return Ok(None);
        };
        let Some((ancestor, ancestor_name)) = ancestor else {
            return Ok(Some(obj));
        };
        let pin = self.pin_native_root(obj);
        let init = self.invoke_special_by_class_id(
            ancestor,
            &ancestor_name,
            "<init>",
            "()V",
            &[Value::Object(Some(obj))],
        );
        let obj = self.read_native_pin(pin, obj);
        self.unpin_native_roots(pin);
        init?;
        Ok(Some(obj))
    }

    /// Register a discovered weak/soft/phantom reference with the GC's ReferenceProcessor.
    /// `ref_type`: 0=Weak, 1=Soft, 2=Phantom
    /// `reference_obj`: the Reference object itself
    /// `referent`: the referred-to object
    /// `queue`: optional ReferenceQueue object
    fn discover_reference(
        &mut self,
        ref_type: u8,
        reference_obj: ObjectRef,
        referent: ObjectRef,
        queue: Option<ObjectRef>,
    );

    /// Notify the GC's reference processor that a `SoftReference.get()` just
    /// observed its referent, refreshing the LRU timestamp used by
    /// soft-reference clearing heuristics on the next major GC.
    ///
    /// Round-5 fix (HIGH): without this hook, the LRU index sees
    /// `last_access_time_ms == 0` forever and every SoftReference looks
    /// infinitely stale — clearing on the first low-memory cycle and
    /// defeating soft-ref-backed caches. The VM overrides this with a
    /// call into `ReferenceProcessor::touch_soft_reference`. The default
    /// no-op keeps mock/test contexts compiling.
    fn touch_soft_reference(&mut self, _reference_obj: ObjectRef) {}

    /// INT-8: GC keep-alive for a referent a `Reference.get()` just handed to
    /// the mutator — the HotSpot `G1ReferenceGet` intrinsic barrier
    /// equivalent. While a G1 concurrent mark cycle is active, the marker
    /// deliberately does NOT trace through referent slots (referent-slot
    /// hiding); a mutator that reads a referent and stores it into an
    /// already-scanned (black) object would create the only strong path via
    /// an edge the snapshot cannot see, and the remark-time reference
    /// processor could then clear the weak ref and free the referent while
    /// strongly reachable (use-after-free). The VM overrides this with the
    /// heap's standalone SATB barrier (`VmHeap::satb_barrier`), which logs the
    /// value as a mark root when marking is active and is a no-op otherwise.
    /// `refersTo` intentionally does NOT call this — its JDK contract is to
    /// test the referent WITHOUT keeping it alive. The default no-op keeps
    /// mock/test contexts compiling.
    fn gc_reference_keep_alive(&mut self, _referent: ObjectRef) {}

    /// zt t23/zb8: read reference field `index` of `obj` WITHOUT keeping its
    /// target alive -- HotSpot's `AS_NO_KEEPALIVE` / phantom load, which
    /// `Reference.refersTo0` / `PhantomReference.refersTo0` use to compare a
    /// referent without resurrecting it. Only for an identity comparison: the
    /// answer must not be stored anywhere or handed to Java (a value read this
    /// way may be one the running mark judges dead). The VM overrides it with
    /// ZGC's non-marking `Weak` load (`VmHeap::get_field_weak`), which differs
    /// from [`get_field`](Self::get_field) only while ZGC's read barrier is
    /// armed (`CRATONVM_ZGC_REFERS_TO_NO_KEEPALIVE`, default ON, `=0` kill).
    /// The default is `get_field`, so mock/test contexts need nothing.
    fn get_field_no_keepalive(&self, obj: ObjectRef, index: usize) -> Value {
        self.get_field(obj, index)
    }

    /// Keep alive a reference returned by an unknown-strength instance-field
    /// read (Unsafe, reflection or a handle). Unlike `Reference.get`, these
    /// doors must first identify `Reference.referent` on the receiver. The VM
    /// overrides this with its per-VM bootstrap field identity and subtype
    /// cache. A context without a hierarchy conservatively keeps slot zero
    /// alive; the VM's override screens its actual bootstrap layout precisely.
    fn gc_referent_read_keep_alive(&mut self, receiver: ObjectRef, slot: usize, value: Value) {
        if slot == 0 && !self.object_is_array(receiver) {
            if let Value::Object(Some(referent)) = value {
                let class = self.class_id_of_object(receiver);
                if self.class_assignable_to_name(class, "java/lang/ref/Reference") != Some(false) {
                    self.gc_reference_keep_alive(referent);
                }
            }
        }
    }

    /// gcj j1/rf (`g1-and-generational-referent-reads-through-unsafe-and-jni-have-no-keep-alive-20261019`,
    /// the Generational half): the keep-alive for a reflective, handle or
    /// `Unsafe` read of instance slot `slot` of `receiver` that handed
    /// `referent` to Java, on a heap that hides `Reference.referent` from its
    /// concurrent mark with no blanket slot-0 rule of its own. The VM's
    /// Generational-specific callers log `referent` through the SATB barrier exactly when
    /// a concurrent old-generation mark is open, `slot` is 0 (the slot the
    /// mark hides) and `receiver`'s class is a `java.lang.ref.Reference`
    /// subclass -- HotSpot's `java_lang_ref_Reference::is_referent_field`
    /// test -- so a slot-0 read of any other object adds no floating garbage.
    /// G1 and ZGC: this optional hook does nothing; general unknown-strength
    /// reads use `gc_referent_read_keep_alive`. The default no-op keeps
    /// mock/test contexts compiling.
    fn gc_referent_slot_read_keep_alive(
        &mut self,
        _receiver: ObjectRef,
        _slot: usize,
        _referent: ObjectRef,
    ) {
    }

    /// Notify the GC's reference processor that a `Reference.enqueue()` call
    /// just enqueued this reference itself (the application's own explicit
    /// enqueue, as opposed to the GC discovering the referent dead).
    ///
    /// PGJDBC-PHANTOM-GHOST (2026-08-07): without this, a `Reference` that the
    /// application manually retires while its referent is STILL reachable
    /// (e.g. pgjdbc's `SimpleQuery.unprepare()`/`setCleanupRef()`, which
    /// `clear()`s and `enqueue()`s the *previous* `PhantomReference` when a
    /// long-lived, reused `SimpleQuery` gets re-prepared) leaves a stale
    /// `enqueued: false` bookkeeping entry in the GC's registry. If that same
    /// referent is later shared with a NEW Reference (as in the pgjdbc
    /// pattern) and eventually dies for real, the GC's own weak/phantom
    /// processing rediscovers the stale entry and delivers it a SECOND time —
    /// a ghost the application already fully drained and forgot, so its own
    /// removal bookkeeping (e.g. a `HashMap.remove(ref)`) returns null. See
    /// `ReferenceProcessor::mark_manually_enqueued`'s doc for the full
    /// mechanism. The VM overrides this to retire the registry entry so it is
    /// never rediscovered; the default no-op keeps mock/test contexts
    /// compiling.
    fn mark_reference_manually_enqueued(&mut self, _reference_obj: ObjectRef) {}

    /// t1 wave 7, lane G6 (`CRATONVM_REF_NOTES_PER_THREAD`): the program has
    /// just nulled `reference_obj`'s referent (`clear0`, `clear()`,
    /// `enqueue()`), but [`Self::mark_reference_manually_enqueued`] is not
    /// called, because the reference has no live queue (`NULL_QUEUE`,
    /// `ENQUEUED`, or none). A queue-less `clear()` is `ThreadLocal.remove()`'s,
    /// the hottest clear of all, which must stay off the reference processor's
    /// mutex. Drop the reference's allocation note when it still waits,
    /// unabsorbed, among the calling thread's recent notes (lock-free of that
    /// mutex); otherwise do nothing, since the pre-collection bind drops a
    /// row whose referent reads null anyway. Returns whether a note was
    /// dropped. Never takes the processor's lock. The default (no stripes,
    /// mock and test contexts) does nothing.
    fn cancel_reference_allocation_note(&mut self, _reference_obj: ObjectRef) -> bool {
        false
    }

    /// GC-discovered references awaiting the genuine JDK Reference Handler.
    /// The returned head links through Reference.discovered; both operations
    /// are scoped to this VM, with pending objects rooted until the take.
    fn take_reference_pending_list(&mut self) -> Option<ObjectRef> {
        None
    }

    fn has_reference_pending_list(&self) -> bool {
        false
    }
}

pub trait NativeThreadAccess: NativeHeapAccess {
    // Capability boundary: Threads, monitors, parking, blocking, and scoped values.

    // -- Threading methods --

    /// Get the current thread's ThreadId (as a u64).
    fn thread_id(&self) -> u64;

    /// Acquire the monitor (synchronized) on the given object.
    fn monitor_enter(&mut self, obj: ObjectRef);

    /// GC-safe variant of [`monitor_enter`], for the rare native whose
    /// contended wait needs to be excused from an in-flight STW barrier
    /// pause instead of leaving the calling thread counted in its `expected`
    /// set for the whole wait (see
    /// `wildfly-standalone-boot-stw-jit-takeover-hang-FIXED.md`).
    ///
    /// Deliberately NARROW: `monitor_enter` itself stays on its original,
    /// non-GC-blocked path for the other ~80 native call sites that use
    /// it (Semaphore/Phaser/Exchanger/blocking-queue/ConcurrentHashMap/
    /// ReentrantLock/Condition/etc.) — a from-scratch audit of every one of
    /// those (2026-07-13) found the overwhelming majority keep reading
    /// fields off the SAME `obj`/`this` after the call without any
    /// pin-and-refresh, so blanket-switching `monitor_enter`'s contended
    /// path to span a completing (possibly moving) GC pause would expose
    /// all of them to the stale-`ObjectRef`-across-GC bug class this
    /// codebase has repeatedly hit (see
    /// `wildfly-parallel-boot-stale-objectref-residual.md`)
    /// — an unaudited-at-scale regression risk far worse than the original
    /// hang. This method exists so the ONE call site with live-gdb-confirmed
    /// evidence of the deadlock (`CountDownLatch`'s `native_cdl_await` /
    /// `native_cdl_await_timeout` / `native_cdl_count_down` polling loop,
    /// contending a shared handshake latch under WildFly's
    /// `parallel-extension-add`) can opt in individually, and MUST use the
    /// returned reference for anything after the call — the object may have
    /// moved if the wait spanned a GC. Default implementation is a no-op
    /// pass-through to `monitor_enter` (correct for every mock/test context
    /// in this workspace, none of which move objects mid-wait).
    fn monitor_enter_gc_safe(&mut self, obj: ObjectRef) -> ObjectRef {
        self.monitor_enter(obj);
        obj
    }

    /// gci i11/ja: run `f` with THIS context's thread installed as the
    /// calling OS thread's JNI binding (the VM's `JNI_THREAD` /
    /// `JNI_SHARED_VM`), derived from the context's own exclusive borrow,
    /// and the previous binding put back afterwards (also on unwind).
    ///
    /// For a JNIEnv function that C code calls with no binding while a Rust
    /// frame up the stack -- an FFM downcall, whose context the vm reaches
    /// through `panama_libffi::with_active_context` -- holds the thread: the
    /// function then borrows the thread from the binding (a reborrow of that
    /// frame's borrow) instead of re-deriving it from the thread registry
    /// (a second exclusive borrow of the same thread). `f` must not use the
    /// context. Default: run `f` with nothing installed (mock and test
    /// contexts have no JNI binding to offer).
    fn run_with_jni_binding(&mut self, f: &mut dyn FnMut()) {
        f();
    }

    /// Release the monitor (synchronized) on the given object.
    fn monitor_exit(&mut self, obj: ObjectRef);

    /// Perform Object.wait() on the given object's monitor.
    fn monitor_wait(
        &mut self,
        obj: ObjectRef,
        timeout_ms: Option<u64>,
    ) -> cratonvm_types::error::MethodCallResult;

    /// `Object.wait(timeout)` itself: as [`Self::monitor_wait`], and a
    /// positive `timeout_ms` makes `Thread.getState()` report TIMED_WAITING
    /// for the wait, as HotSpot does. Plain `monitor_wait` reports WAITING:
    /// most natives that pass a timeout poll in bounded slices on behalf of
    /// an untimed Java wait.
    fn monitor_wait_reporting_timeout(
        &mut self,
        obj: ObjectRef,
        timeout_ms: Option<u64>,
    ) -> cratonvm_types::error::MethodCallResult {
        self.monitor_wait(obj, timeout_ms)
    }

    /// An `Object.wait` on `obj` (the caller owns its monitor) by a served
    /// stand-in that says what JDK code the wait stands for, so the waiting
    /// thread reads the state HotSpot reports for that code (interpreter
    /// round i2 wave 65, lane L1, `CRATONVM_SERVED_WAIT_KINDS`;
    /// `docs/feature-designs/native-dispatch-architecture.md`).
    ///
    /// `kind` is the JDK body's: [`BlockKind::Parked`] / [`BlockKind::TimedParked`]
    /// for an AQS acquire (the `--compatible` `CountDownLatch.await`, which
    /// polls its count in bounded monitor-wait slices), whose thread reads
    /// WAITING / TIMED_WAITING and JVMTI `PARKED`; [`BlockKind::TimedWaiting`]
    /// / [`BlockKind::TimedObjectWait`] report TIMED_WAITING for a positive
    /// `timeout_ms`, as [`Self::monitor_wait_reporting_timeout`]; every other
    /// kind is a plain [`Self::monitor_wait`]. A timed kind needs a positive
    /// `timeout_ms` to read TIMED_WAITING (the slice, not the Java deadline).
    ///
    /// The default maps the timed kinds onto
    /// [`Self::monitor_wait_reporting_timeout`] and the others onto
    /// [`Self::monitor_wait`], so out-of-tree implementors and mocks need not
    /// change.
    fn monitor_wait_as(
        &mut self,
        obj: ObjectRef,
        timeout_ms: Option<u64>,
        kind: BlockKind,
    ) -> cratonvm_types::error::MethodCallResult {
        match kind {
            BlockKind::TimedWaiting
            | BlockKind::Sleeping
            | BlockKind::TimedObjectWait
            | BlockKind::TimedParked => self.monitor_wait_reporting_timeout(obj, timeout_ms),
            BlockKind::Waiting
            | BlockKind::InNative
            | BlockKind::ObjectWait
            | BlockKind::Parked => self.monitor_wait(obj, timeout_ms),
        }
    }

    /// Perform Object.notify() on the given object's monitor.
    fn monitor_notify(&mut self, obj: ObjectRef) -> cratonvm_types::error::MethodCallResult;

    /// Perform Object.notifyAll() on the given object's monitor.
    fn monitor_notify_all(&mut self, obj: ObjectRef) -> cratonvm_types::error::MethodCallResult;

    /// Spawn a new OS thread to run Thread.run() on the given Java Thread object.
    fn thread_start(&mut self, thread_obj: ObjectRef) -> cratonvm_types::error::MethodCallResult;

    /// Block until the target thread (identified by Java Thread object) finishes.
    fn thread_join(&mut self, thread_obj: ObjectRef) -> cratonvm_types::error::MethodCallResult;

    /// Check if the target thread (identified by Java Thread object) is alive.
    fn thread_is_alive(&self, thread_obj: ObjectRef) -> bool;

    /// The VM thread-registry id of the target thread (identified by its Java
    /// Thread object; reads the heap), for [`Self::thread_registry_id_is_alive`].
    /// `None` when the thread was never started, and in the default.
    ///
    /// Interpreter round i1 wave 50, lane L1: a served join resolves the id
    /// once and then polls liveness without touching the heap, so it can stay
    /// in ONE blocking region for the whole join (it read `RUNNABLE` between
    /// its 2 ms slices).
    fn thread_registry_id(&self, _thread_obj: ObjectRef) -> Option<u64> {
        None
    }

    /// Is the VM thread with registry id `id` ([`Self::thread_registry_id`])
    /// alive? Reads no heap, so it may be asked inside a blocking region.
    /// `false` in the default.
    fn thread_registry_id_is_alive(&self, _id: u64) -> bool {
        false
    }

    /// Coarse run-state of the target thread, derived from the VM thread
    /// registry (the authoritative liveness source). Returns:
    ///   * `0` — NEW: the thread was never started (no registry entry).
    ///   * `1` — RUNNABLE: started and still alive.
    ///   * `2` — TERMINATED: started and has since finished.
    ///
    /// Value `3` represents an alive thread parked in a blocking region
    /// (`WAITING`) and value `4` an alive thread acquiring a contended
    /// monitor (`BLOCKED`); the default returns `0`.
    ///
    /// Used to back `Thread.getState()` in real-JDK mode, where the JDK
    /// bytecode reads `holder.threadStatus` — a field the VM does not keep
    /// updated, so `getState()` would otherwise always report `NEW` (even for
    /// finished threads), tripping strict thread-leak detectors. The default
    /// returns `0`.
    fn thread_run_state(&self, _thread_obj: ObjectRef) -> u8 {
        0
    }

    /// The Java call stack of the target thread (identified by its Thread
    /// object), innermost frame first. For the *current* thread this is the live
    /// stack; for another thread it is the snapshot published at its last
    /// blocking deposit point (so for a parked thread it shows where it is
    /// stuck). Empty if unavailable. Backs cross-thread `Thread.getStackTrace()`
    /// / `Thread.dumpThreads()`. The default returns empty.
    fn thread_stack_trace(&self, _thread_obj: ObjectRef) -> Vec<StackTraceEntry> {
        Vec::new()
    }

    /// [`Self::thread_stack_trace`] for a thread DUMP (`Thread.dumpThreads`,
    /// behind `Thread.getAllStackTraces`), which lists the hidden frames
    /// between `Thread.run` and the task -- `Thread.runWith` and a lambda
    /// proxy's `run` -- as HotSpot's thread dump does, where
    /// `Thread.getStackTrace` hides them (JIT round 19 wave 11, lane gc11,
    /// `CRATONVM_JMX_THREAD_INFO_HIDDEN_RUN_FRAMES`). The default is
    /// [`Self::thread_stack_trace`].
    fn thread_dump_stack_trace(&self, thread_obj: ObjectRef) -> Vec<StackTraceEntry> {
        self.thread_stack_trace(thread_obj)
    }

    /// Atomically snapshot the thread state and lock relationships needed by
    /// `ThreadMXBean`. The default leaves lightweight/mock contexts source
    /// compatible; production VMs must return GC-safe registry-backed refs.
    fn thread_jmx_snapshot(&self, _thread_obj: ObjectRef) -> Option<ThreadJmxSnapshot> {
        None
    }

    /// [`Self::thread_jmx_snapshot`] for a `ThreadInfo` door that reports no
    /// frames and no locked monitors (`maxDepth` 0; interpreter round i2 wave
    /// 61, lane L6): an implementation may leave the stack out, and so not
    /// stop the target to capture it. The caller cuts whatever stack comes
    /// back. The default is the full snapshot.
    fn thread_jmx_snapshot_frameless(&self, thread_obj: ObjectRef) -> Option<ThreadJmxSnapshot> {
        self.thread_jmx_snapshot(thread_obj)
    }

    /// A dump of the stacks of `threads` starts (interpreter round i2 wave
    /// 61, lane L6): an implementation may stop the world once, so that
    /// every running target publishes its stack of the same instant, and
    /// serve each [`Self::thread_stack_trace`] / [`Self::thread_jmx_snapshot`]
    /// of the dump from what they published, until [`Self::end_thread_dump`].
    /// The default does nothing.
    fn begin_thread_dump(&mut self, _threads: &[ObjectRef]) {}

    /// [`Self::begin_thread_dump`] for a dump of `ThreadInfo`s (JIT round 25
    /// W10-7 review): an implementation may also read every target's
    /// [`Self::thread_jmx_snapshot`] state in the dump's own pause(s), as
    /// HotSpot's `VM_ThreadDump` reads them all at one safepoint, instead of
    /// stopping the world again per target. The default is
    /// [`Self::begin_thread_dump`]; [`Self::end_thread_dump`] ends both.
    fn begin_thread_info_dump(&mut self, threads: &[ObjectRef]) {
        self.begin_thread_dump(threads)
    }

    /// The dump [`Self::begin_thread_dump`] started is over. The default does
    /// nothing.
    fn end_thread_dump(&mut self) {}

    /// Record the current ownership of an `AbstractOwnableSynchronizer`.
    /// Implementations retain/remap the synchronizer while it is owned so a
    /// later JMX dump can report `lockedSynchronizers` without heap walking.
    fn record_jmx_owned_synchronizer(
        &mut self,
        _synchronizer: ObjectRef,
        _owner: Option<ObjectRef>,
    ) {
    }

    /// nio21 wave 3, lane L4: HotSpot's answer to `lockedSynchronizers`
    /// (`ConcurrentLocksDump::dump_at_safepoint`): one walk of the heap, in a
    /// pause, for every `java.util.concurrent.locks.AbstractOwnableSynchronizer`
    /// whose `exclusiveOwnerThread` is set, as `(owner Thread mirror,
    /// synchronizer)` pairs in heap order. The references are valid until the
    /// caller's next allocation or safepoint, so a caller pins (or reads) them
    /// before it allocates. `None`: this context cannot walk its heap (the
    /// default, and a VM whose pause could not be had) -- the caller then keeps
    /// whatever [`Self::thread_jmx_snapshot`] reported.
    fn owned_synchronizer_census(&mut self) -> Option<Vec<(ObjectRef, ObjectRef)>> {
        None
    }

    /// Get the Java Thread object for the current thread.
    fn current_thread_object(&mut self) -> ObjectRef;

    /// Round 17 wave 2 (lane ffm2): the current thread's Java `Thread` mirror
    /// WITHOUT minting one. `None` when the thread has no mirror yet, where
    /// `current_thread_object` would allocate it (a GC point). For callers
    /// that hold raw references and only compare identities (an FFM confined
    /// session's owner check: a thread with no mirror cannot own a session).
    /// The default delegates, which is right for contexts whose
    /// `current_thread_object` never allocates.
    fn current_thread_object_if_any(&mut self) -> Option<ObjectRef> {
        Some(self.current_thread_object())
    }

    /// Interrupt the target thread (identified by Java Thread object).
    fn thread_interrupt(&mut self, thread_obj: ObjectRef);

    // (`thread_post_async_exception`, the T1.5.1 async-exception post, was
    // removed in interpreter round i1 wave 4: it had no caller in any crate
    // and nothing ever raised what it posted. See
    // docs/feature-designs/profile-guided-inlining.md.)

    /// Check and optionally clear the current thread's interrupted status.
    fn is_interrupted(&self, clear: bool) -> bool;

    /// Windows (interpreter round i6 wave 115, lane L1,
    /// `CRATONVM_PROCESS_WAIT_INTERRUPT_EVENT`; page
    /// `i114-L1-proposal-windows-process-wait-wakes-on-interrupt`): announce
    /// the current thread as a waiter on an event that `Thread.interrupt`
    /// raises, for a native that waits on OS handles and must end on an
    /// interrupt at once -- HotSpot's `JVM_GetThreadInterruptEvent()`, the
    /// second handle of the JDK's Windows `ProcessImpl` waits. `Some(h)`: a
    /// raw Windows event `HANDLE` (as `isize`) to add to the caller's
    /// `WaitForMultipleObjects`. An interrupt raised after this call signals
    /// it; one raised before it set the interrupt status, so the caller tests
    /// [`Self::is_interrupted`] AFTER this call and before it waits. The event
    /// may also be raised by an `unpark` of the thread: re-test and wait
    /// again. Pair every `Some` with exactly one
    /// [`Self::leave_interrupt_wake_event`]. Takes one short lock; legal
    /// inside a blocking region. `None` (the default; every target but
    /// Windows; a VM-scheduled virtual thread): no event, wait as before.
    fn enter_interrupt_wake_event(&mut self) -> Option<isize> {
        None
    }

    /// Leave a wait announced by [`Self::enter_interrupt_wake_event`];
    /// `consumed`: the caller's OS wait returned on that event. The default
    /// does nothing.
    fn leave_interrupt_wake_event(&mut self, consumed: bool) {
        let _ = consumed;
    }

    /// Round 14 wave 6 (lane trace5; TR5-3): tell the NEXT throwable capture
    /// on this thread that the served `Thread.join(long)` it stands at is
    /// about to raise `InterruptedException` having run with a positive
    /// timeout (`true`) or none (`false`), so the capture can rebuild the
    /// JDK frames from the right `wait` call. The capture consumes it. The
    /// default (mock contexts) drops it.
    fn set_served_timed_join_hint(&mut self, _timed: bool) {}

    /// Round 15 wave 2 (lane trace2): tell the NEXT throwable capture on this
    /// thread that the `ClassNotFoundException` about to be built is the miss
    /// of the served base `ClassLoader.loadClass(String)` for a built-in
    /// loader of class `loader_class` (`Some`), so the capture can rebuild the
    /// JDK loader frames that native never pushed; `None` withdraws a hint
    /// no capture took (the exception was built without one). The capture
    /// consumes it. The default (mock contexts) drops it.
    fn set_served_loader_miss_hint(&mut self, _loader_class: Option<ClassId>) {}

    /// JIT round 16 wave 13 (lane wmte): tell the NEXT throwable capture on
    /// this thread that the `WrongMethodTypeException` about to be built is
    /// the refusal a `MethodHandle` door raises natively in place of the JDK
    /// method `raise` names (`Some`), so the capture can list that method's
    /// frames, which the door never pushed; `None` withdraws a hint no capture
    /// took. Only a capture of a `WrongMethodTypeException` consumes it. The
    /// default (mock contexts) drops it.
    fn set_mh_wmte_raise_hint(&mut self, _raise: Option<MhWmteRaise>) {}

    /// Interpreter round i1 wave 48, lane L1: a `Thread.sleep` native about
    /// to raise `InterruptedException` asks whether to raise it in the shape
    /// of HotSpot's DEFAULT virtual threads -- no message, as
    /// `VirtualThread.sleepNanos`'s `new InterruptedException()` -- and, when
    /// it answers `true`, tells the NEXT throwable capture on this thread
    /// which of that method's two throws it stands for: `during` = after the
    /// park began (the second), else before sleeping (the first). `true`
    /// only on a virtual thread of a VM that asks for that shape. The default
    /// (mock contexts, platform threads) answers `false`.
    fn note_virtual_sleep_interrupt(&mut self, _during: bool) -> bool {
        false
    }

    /// Take the throwable a debugger's JDWP `ThreadReference.Stop` left for
    /// the current thread (interpreter round i1 wave 25). The stop interrupts
    /// the thread to wake it; a blocking call woken that way throws this in
    /// place of its `InterruptedException`, as HotSpot's asynchronous
    /// exception replaces the pending one. `None` — the default, and
    /// whenever no stop is pending — otherwise.
    fn take_debugger_stop(&mut self) -> Option<ObjectRef> {
        None
    }

    /// Check the interrupted status of the thread identified by `thread_obj`
    /// — which may be a thread *other* than the current one (e.g.
    /// `ThreadPoolExecutor.interruptIdleWorkers` calls `worker.isInterrupted()`
    /// from the pool-management thread). Never clears the flag. The default
    /// impl falls back to the current thread's status for mock contexts that
    /// don't track per-thread state.
    fn thread_is_interrupted(&self, _thread_obj: ObjectRef) -> bool {
        self.is_interrupted(false)
    }

    // -- Virtual-thread / Loom (JEP 444/491) --
    //
    // Default implementations make these no-ops so platform native code (and
    // test mocks) don't need to implement them. The VM overrides them in
    // `vm_exec.rs` to drive the carrier-thread semaphore and pin tracking.

    /// Returns `true` if the current thread is a virtual thread.
    fn is_current_virtual(&self) -> bool {
        false
    }

    // -- JDK-driven continuations (the VM's `vm::jdk_continuation`) --
    //
    // Served only where `NativeMethodRegistry::jdk_continuations` is on; the
    // defaults answer for a context that runs no continuation.

    /// `jdk.internal.vm.Continuation.enterSpecial(c, isContinue,
    /// isVirtualThread)`, called by `Continuation.run` on the carrier: run
    /// continuation `cont` (its first run, or its continue after a yield)
    /// until it yields or completes, and return to `run` either way. A
    /// throwable escaping the continuation's `Continuation.enter` is
    /// `ExceptionThrown`, as from any call.
    fn continuation_enter_special(
        &mut self,
        _cont: ObjectRef,
        _is_continue: bool,
        _is_virtual_thread: bool,
    ) -> Result<(), MethodCallFailed> {
        Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Internal {
                message: "Continuation.enterSpecial: this VM runs no JDK continuation".to_string(),
            },
        ))
    }

    /// `Continuation.doYield()`: unmount the running continuation (the
    /// native's result is then a `VmError::ContinuationYield` the carrier's
    /// `enterSpecial` receives, and the remount answers `0`), or, when it
    /// cannot be unmounted, `Ok(reason)`: HotSpot's freeze results, `2`
    /// critical section (`Continuation.pin()`), `3` native frame, `4` monitor.
    fn continuation_do_yield(&mut self) -> Result<i32, MethodCallFailed> {
        Ok(3)
    }

    /// `Continuation.isPinned0(scope)`: the reason [`Self::continuation_do_yield`]
    /// would answer now, `0` when it would unmount.
    fn continuation_pinned_reason(&self) -> i32 {
        0
    }

    /// `Object.wait0` of a mounted JDK virtual thread under
    /// `CRATONVM_VTHREAD_MONITOR_UNMOUNT` (interpreter round i4 wave 89, lane
    /// L6): `Some(r)` when the VM took the wait -- `Err(ContinuationYield)`
    /// (the continuation unmounts; the remount re-acquires the monitor) or
    /// `IllegalMonitorStateException`; `None` to wait as before. Only the
    /// `Object.wait` natives call it. Default: `None`.
    fn vt_monitor_wait_unmount(
        &mut self,
        _obj: ObjectRef,
        _timeout_ms: Option<u64>,
    ) -> Option<MethodCallResult> {
        None
    }

    /// `Thread.setCurrentThread(thread)`: re-point what `Thread.currentThread()`
    /// answers on this thread (a carrier mounting or unmounting a virtual
    /// thread, `VirtualThread.mount` / `unmount` / `switchToCarrierThread`).
    fn set_current_thread_object(&mut self, _thread: ObjectRef) {}

    /// `Thread.currentCarrierThread()`: the platform thread this code runs on
    /// (a mounted virtual thread's carrier), the current thread otherwise.
    fn current_carrier_thread_object(&mut self) -> ObjectRef {
        self.current_thread_object()
    }

    /// The frames of unmounted continuation `cont`, OUTERMOST first (the order
    /// of [`Self::capture_stack_walk_trace`]), for a `StackWalker` built on it
    /// (`Continuation.getStackTrace`, which `VirtualThread.getStackTrace` asks
    /// of an unmounted virtual thread); `None` when it has none to give.
    fn continuation_stack_trace(&self, _cont: ObjectRef) -> Option<Vec<StackTraceEntry>> {
        None
    }

    /// Is this a mounted JDK virtual thread (a JDK-driven continuation's
    /// thread), whose VM-level blocking -- a pinned `Object.wait` or park --
    /// blocks its carrier? See [`Self::begin_carrier_compensation`].
    fn blocks_jdk_carrier(&self) -> bool {
        false
    }

    /// Is this `Unsafe.park` of a mounted JDK virtual thread a pinned park
    /// whose pin HotSpot would not have (a compiled frame beneath, or a VM
    /// door's continuation pin), under the learned compiled-entry switch? The
    /// park then takes the carrier compensation
    /// ([`Self::begin_carrier_compensation`]) that HotSpot's unmount makes
    /// unnecessary (interpreter round i5 wave 101, lane L2,
    /// `CRATONVM_VTHREAD_COMPILED_LEARNED`). Default: `false`.
    fn vm_pinned_park_compensates(&self) -> bool {
        false
    }

    /// A mounted JDK virtual thread is about to block its carrier in the VM:
    /// let the scheduler compensate with another carrier, as the JDK's own
    /// blocking operations do (`jdk.internal.misc.Blocker.begin()`, JDK 22+),
    /// so carriers pinned by waits cannot starve the threads that would end
    /// them. Answers whether compensation was attempted; hand that to
    /// [`Self::end_carrier_compensation`] once the block is over. Runs Java:
    /// references held across it must be pinned.
    fn begin_carrier_compensation(&mut self) -> bool {
        false
    }

    /// The end of a block [`Self::begin_carrier_compensation`] began
    /// (`Blocker.end(attempted)`). Runs Java.
    fn end_carrier_compensation(&mut self, _attempted: bool) {}

    /// A JDK virtual thread's JVMTI transition (`VirtualThread.notifyJvmti*`):
    /// `0` start, `1` end, `2` mount, `3` unmount.
    fn post_virtual_thread_transition(&mut self, _kind: u8) {}

    /// `VirtualThread.takeVirtualThreadListToUnblock()`: wait (RUNNABLE, in
    /// the VM) until virtual threads blocked on a monitor can continue, and
    /// answer the head of their list (linked through `VirtualThread.next`),
    /// or `None` after a bounded wait with nothing to hand out.
    fn take_virtual_thread_list_to_unblock(&mut self) -> Option<ObjectRef> {
        self.begin_vm_wait_region();
        std::thread::sleep(std::time::Duration::from_millis(50));
        self.end_blocking_region();
        None
    }

    /// Return the current thread's pin depth (0 = not pinned).
    fn vt_pin_count(&self) -> u32 {
        0
    }

    /// Increment the current thread's pin count with the given reason.
    /// Called from `monitor_enter` / JNI entry. No-op for platform threads.
    fn vt_pin(&mut self, _reason: &'static str) {}

    /// Decrement the current thread's pin count.
    /// No-op for platform threads or when pin_count is already zero.
    fn vt_unpin(&mut self) {}

    /// Release the carrier-thread permit so another virtual thread can run.
    /// Called before a blocking syscall (sleep, park, NIO wait) in a VT.
    /// No-op for platform threads.
    fn vt_release_carrier(&mut self) {}

    /// Reacquire a carrier-thread permit after a blocking operation completes.
    /// Must be paired with `vt_release_carrier`. No-op for platform threads.
    fn vt_acquire_carrier(&mut self) {}

    /// Request a continuation-backed timed park. Returns `true` only for an
    /// unpinned virtual thread whose interpreter frames can be frozen by the
    /// VM. The native must then return `ContinuationYield` without blocking.
    fn vt_park_for(&mut self, _duration: std::time::Duration) -> bool {
        false
    }

    /// [`Self::vt_park_for`] for a `Thread.sleep` native. Same answer; the VM
    /// additionally records that the continuation yielded in a SLEEP, so its
    /// remount can end the sleep the way HotSpot's `JVM_SleepNanos` does: an
    /// interrupt throws `InterruptedException("sleep interrupted")` in the
    /// sleeping frame, and a wake-up that is neither the deadline nor an
    /// interrupt (a stray `LockSupport.unpark`) sleeps out the rest.
    /// (Interpreter round i1 wave 24, lane L7.)
    fn vt_sleep_for(&mut self, duration: std::time::Duration) -> bool {
        self.vt_park_for(duration)
    }

    /// Register the current unpinned virtual thread as an asynchronous waiter
    /// on a VM-local stable key. The native must recheck its condition after
    /// registration and return `ContinuationYield` only while it remains false.
    fn vt_wait_on_key(&mut self, _key: u64) -> bool {
        false
    }

    /// Cancel a waiter registration made by [`Self::vt_wait_on_key`].
    fn vt_cancel_wait_on_key(&mut self, _key: u64) {}

    /// Wake and resubmit all virtual threads waiting on a stable key.
    fn vt_wake_waiters(&mut self, _key: u64) {}

    /// The index the next Java frame pushed on this thread will take (its
    /// Java frame count), for [`Self::note_continuation_return`]; `usize::MAX`
    /// from a context that keeps no frames (interpreter round i1 wave 28, lane
    /// L4).
    fn continuation_frame_mark(&self) -> usize {
        usize::MAX
    }

    /// A native's nested Java call has just returned a continuation yield
    /// (`VmError::ContinuationYield`), through a native frame that declared
    /// itself transparent for it (`continuation_pin::transparent`) but owes the
    /// call's return a conversion: record that the frame at `frame_index` --
    /// the first one the call pushed, [`Self::continuation_frame_mark`] taken
    /// before it -- must convert the value it returns after the remount to the
    /// descriptor token `caller_return` (`I`, `J`, `Ljava/lang/Object;`, ...)
    /// before its caller receives it, as the native would have. The
    /// method-handle doors' boxing and widening of a primitive leaf result
    /// (interpreter round i1 wave 28, lane L4). A no-op by default.
    fn note_continuation_return(&mut self, _frame_index: usize, _caller_return: &str) {}

    /// [`Self::note_continuation_return`] for a `void` leaf behind a
    /// method-handle door whose caller receives a reference: the door owes its
    /// caller `null` (interpreter round i6 wave 115, lane L4,
    /// `CRATONVM_MH_VOID_TAIL_UNMOUNT`). With a Java frame at `frame_index`
    /// (the leaf's), its `void` return pushes the `null` after the remount;
    /// with none (a native leaf yielded itself, so the caller resumes past its
    /// invoke as if the native had returned) the `null` goes onto the caller
    /// frame's operand stack now. Only reached through a context whose
    /// [`Self::continuation_frame_mark`] is not `usize::MAX`. A no-op by
    /// default.
    fn note_continuation_void_leaf_null(&mut self, _frame_index: usize) {}

    /// Get the number of alive threads in the VM.
    fn active_thread_count(&self) -> i32;

    /// The JMM's platform-thread counters `(started, live, peak)`, HotSpot's
    /// `ThreadService::_total_threads_count`, `_atomic_threads_count` and
    /// `_peak_threads_count` (interpreter round i5 wave 100, lane L6): every
    /// platform thread start (`Thread.start`), the main thread and JNI
    /// attaches count, never a virtual thread; `started` never falls; `peak`
    /// is raised at every start, so a spike between two reads is seen.
    /// `None` when the VM keeps no such counters (callers then sample the
    /// live count, as before).
    fn jmm_thread_counters(&self) -> Option<(i64, i32, i32)> {
        None
    }

    /// `ThreadMXBean.resetPeakThreadCount`: the VM's JMM peak becomes its
    /// live count ([`Self::jmm_thread_counters`]). `false` when the VM keeps
    /// no such mark (the caller keeps its own).
    fn jmm_peak_threads_reset(&self) -> bool {
        false
    }

    /// The OS thread id backing a Java `Thread` mirror, or `None` if unknown.
    ///
    /// Enables ARBITRARY-thread CPU time (`ThreadMXBean.getThreadCpuTime(long)`
    /// and therefore `isThreadCpuTimeSupported()`), which reported `-1`/false
    /// only because `ThreadRegistry`'s per-thread `os_tid` — published since
    /// forever by `set_os_tid_current` — was not reachable from a native.
    /// Current-thread CPU time never needed this and was already real.
    fn thread_os_tid(&self, _thread_obj: ObjectRef) -> Option<u32> {
        None
    }

    /// Total JIT compilation time in milliseconds, or `None` when the VM keeps
    /// no such accounting.
    ///
    /// Backs BOTH `CompilationMXBean.getTotalCompilationTime()` and
    /// `isCompilationTimeMonitoringSupported()` — returning one value for both
    /// is deliberate, so they cannot drift into claiming support for a number
    /// that is not kept. The unit is already milliseconds in
    /// `jit::tiered::CompilationStats::total_compile_time_ms`, which is also
    /// the unit the JMM specifies.
    fn jit_total_compile_time_ms(&self) -> Option<u64> {
        None
    }

    /// gci i44/sp: this VM's safepoint accounting as `(count, total stopped
    /// ms, sync ms, application ms)` -- what HotSpot's `RuntimeService` answers
    /// `VMManagementImpl.getSafepointCount` / `getTotalSafepointTime` /
    /// `getSafepointSyncTime` / `getTotalApplicationNonStoppedTime` with
    /// (`docs/feature-designs/native-dispatch-architecture.md`).
    /// `None` when the VM keeps none (the natives then answer 0, as before).
    fn safepoint_stats_ms(&self) -> Option<(u64, u64, u64, u64)> {
        None
    }

    /// Virtual-thread scheduler counters as `(pool_size, mounted, queued)`,
    /// or `None` when no scheduler is running.
    ///
    /// Returned as ONE triple rather than three accessors so
    /// `VirtualThreadSchedulerMXBean`'s three getters cannot sample the
    /// scheduler at three different instants and report a mutually
    /// inconsistent snapshot.
    fn vt_scheduler_stats(&self) -> Option<(i32, i32, i64)> {
        None
    }

    /// Number of objects queued for finalization but not yet finalized —
    /// `MemoryMXBean.getObjectPendingFinalizationCount()`.
    ///
    /// The datum has always existed (`ReferenceProcessor::finalization_queue`,
    /// drained by a real `FinalizerThread`); it simply was not reachable from
    /// `native-builtins`, so the native returned a flat 0. Zero is a
    /// legitimate ANSWER but was not a legitimate CONSTANT: it reported "no
    /// finalization backlog" even while the queue was growing, which is
    /// exactly the condition an operator queries this bean to detect.
    ///
    /// Defaults to 0 for implementations with no reference processor.
    fn pending_finalization_count(&self) -> i32 {
        0
    }

    /// Get the Java Thread objects for all alive threads (up to `max` entries).
    /// Returns the number of thread objects written.
    fn enumerate_threads(&self, max: usize) -> Vec<ObjectRef>;

    /// T19.H1 — mark the start of a *blocking region* inside a native
    /// method (a spin/poll loop or an OS wait that may run for a long
    /// time, e.g. `ReferenceQueue.remove`, a selector `select`, a socket
    /// `accept`).
    ///
    /// While inside a blocking region the calling thread is treated as
    /// GC-safe: its frame roots are published to the registry snapshot
    /// and a concurrent stop-the-world collector will NOT wait for it to
    /// reach an interpreter safepoint. Every `begin_blocking_region` MUST
    /// be paired with exactly one `end_blocking_region`.
    ///
    /// The default impl is a no-op so out-of-tree `NativeContext`
    /// implementors (tests) need not change.
    ///
    /// `#[track_caller]` (interpreter round i2 wave 66, lane L1): the VM's
    /// `CRATONVM_DBG_UNRECORDED_WAIT_KINDS` census names the caller's source
    /// line for a region that records no wait kind.
    #[track_caller]
    fn begin_blocking_region(&mut self) {}

    /// Same GC-safety contract as `begin_blocking_region`, for a region with
    /// a bounded/known wait duration (`Thread.sleep`, a timed `Object.wait`,
    /// `LockSupport.parkNanos`, …). `Thread.getState()` reports
    /// `TIMED_WAITING` for a thread inside one of these vs. plain `WAITING`
    /// for an unbounded `begin_blocking_region` — real JDK's
    /// `Thread.State` makes exactly this distinction, and callers such as
    /// Spring Boot's `SpringApplicationShutdownHookTests` assert on it via
    /// `Awaitility.await().until(thread::getState, State.TIMED_WAITING::equals)`.
    ///
    /// The default impl just delegates to `begin_blocking_region` (reported
    /// as plain `WAITING`) so out-of-tree `NativeContext` implementors need
    /// not change; must still be paired with exactly one `end_blocking_region`.
    ///
    /// `#[track_caller]` (interpreter round i2 wave 66, lane L1): the VM's
    /// `CRATONVM_DBG_UNRECORDED_WAIT_KINDS` census names the caller's source
    /// line for a region that records no wait kind.
    #[track_caller]
    fn begin_timed_blocking_region(&mut self) {
        self.begin_blocking_region();
    }

    /// The GC-safety contract of [`Self::begin_blocking_region`], for a region
    /// that says what it stands for ([`BlockKind`]): the thread inside reads
    /// the state HotSpot reports for that Java code. Paired with exactly one
    /// `end_blocking_region` / `end_blocking_region_refs` (interpreter round
    /// i2 wave 59, lane L6).
    ///
    /// The default maps the kind onto the two older entries, so out-of-tree
    /// implementors and mocks count one ordinary region.
    ///
    /// `#[track_caller]` (interpreter round i2 wave 66, lane L1): the VM's
    /// `CRATONVM_DBG_UNRECORDED_WAIT_KINDS` census names the caller's source
    /// line for a region that records no wait kind.
    #[track_caller]
    fn begin_blocking_region_as(&mut self, kind: BlockKind) {
        match kind {
            BlockKind::TimedWaiting
            | BlockKind::Sleeping
            | BlockKind::TimedObjectWait
            | BlockKind::TimedParked => self.begin_timed_blocking_region(),
            BlockKind::Waiting
            | BlockKind::InNative
            | BlockKind::ObjectWait
            | BlockKind::Parked => self.begin_blocking_region(),
        }
    }

    /// [`Self::begin_blocking_region_as`] for a stand-in that knows the object
    /// the JDK code it stands for waits or parks on (interpreter round i2
    /// wave 68, lane L1, `CRATONVM_STAND_IN_WAIT_OBJECTS`;
    /// `docs/feature-designs/native-dispatch-architecture.md`).
    /// For the region's span the thread publishes `obj` as HotSpot's thread
    /// would: an [`BlockKind::ObjectWait`] / [`BlockKind::TimedObjectWait`]
    /// region as its waiting monitor (`obj` is the monitor the JDK body
    /// `wait`s on), a [`BlockKind::Parked`] / [`BlockKind::TimedParked`] one
    /// as its `Thread.parkBlocker` (`obj` is the blocker the JDK body hands
    /// `LockSupport.park`), so `ThreadInfo.getLockName()` names it. Other
    /// kinds publish nothing. Withdrawn by the region's
    /// `end_blocking_region` / `end_blocking_region_refs`, which pair with it
    /// exactly as with [`Self::begin_blocking_region_as`].
    ///
    /// `obj` must be live when this is called (the caller is still a running
    /// mutator); the VM keeps it rooted and forwarded for the region's span.
    ///
    /// The default ignores `obj`, so out-of-tree implementors and mocks count
    /// one ordinary region.
    #[track_caller]
    fn begin_blocking_region_as_on(&mut self, obj: ObjectRef, kind: BlockKind) {
        let _ = obj;
        self.begin_blocking_region_as(kind);
    }

    /// `CRATONVM_STAND_IN_WAIT_OBJECTS` for this VM (interpreter round i2
    /// wave 68, lane L1): do stand-ins publish the object their JDK code
    /// waits or parks on ([`Self::begin_blocking_region_as_on`]), and do the
    /// `LockSupport.park*(Object, ..)` bridges set `Thread.parkBlocker` as the
    /// JDK's own bodies do? A native asks before it does work only that
    /// answer needs (allocating a stand-in's condition object). The default
    /// answers `false`.
    fn stand_in_wait_objects(&self) -> bool {
        false
    }

    /// For a LEAF native ([`NativeMethodRegistry::set_leaf`]) whose call turns
    /// out to need what a leaf may not do -- typically a blocking region
    /// around a system call that can wait on a peer (a `RandomAccessFile`
    /// read of a FIFO): `true` when the call came through the leaf path,
    /// which then re-runs it on the full native-call funnel once the native
    /// returns. The native must decline BEFORE any side effect and return
    /// at once (its answer is discarded). `false` everywhere else (the full
    /// funnel, a direct call): the native goes on and does what it needs.
    ///
    /// The default answers `false`, so out-of-tree implementors and mocks run
    /// the full body.
    fn decline_leaf_call(&mut self) -> bool {
        false
    }

    /// The GC-safety contract of [`Self::begin_blocking_region`] for a thread
    /// that runs foreign code (an FFM downcall): a pause does not wait for it,
    /// but `Thread.getState()` stays `RUNNABLE`, as HotSpot reports a thread in
    /// `_thread_in_native`. Paired with exactly one `end_blocking_region`
    /// (round 14 wave 1, lane ffm; `r14w1-ffm-downcall-in-native-state-patch`).
    ///
    /// The default delegates to `begin_blocking_region` (reported `WAITING`),
    /// so out-of-tree implementors need not change.
    fn begin_native_region(&mut self) {
        self.begin_blocking_region();
    }

    /// The GC-safety contract of [`Self::begin_blocking_region`] for a wait
    /// that HotSpot performs inside the VM (`_thread_in_vm`), not in a Java
    /// `wait`/`park`: a pause does not wait for the thread, and
    /// `Thread.getState()` stays `RUNNABLE`. Unlike
    /// [`Self::begin_native_region`] the thread does not run foreign code, so
    /// it is not exempt from a pause's signal. Paired with exactly one
    /// `end_blocking_region` / `end_blocking_region_refs` (JIT round 17 wave
    /// 2, lane ffm2: a shared arena's close waiting out in-flight accesses).
    ///
    /// The default delegates to `begin_blocking_region` (reported `WAITING`),
    /// so out-of-tree implementors need not change.
    fn begin_vm_wait_region(&mut self) {
        self.begin_blocking_region();
    }

    /// [`Self::begin_native_region`] the cheap way, for an FFM downcall (JIT
    /// round 16 wave 9, lane ffm;
    /// `r13w5-ffm3-a-blocked-downcall-stalls-every-stop-the-world`): the same
    /// GC-safety contract and the same `RUNNABLE` state, but no blocked-stack
    /// summary for code reclamation, and an answer that lets
    /// [`Self::end_native_region_quiet`] skip the full wake (the fixups and a
    /// second root deposit) when no pause completed and nothing was relocated
    /// into this thread while it was in native. The JNI in-native bracket's
    /// transition (the vm's `native::jni` `native_call_enter_native` /
    /// `native_call_leave_native`), offered to foreign code.
    ///
    /// The answer is opaque: hand it, unchanged, to exactly one
    /// [`Self::end_native_region_quiet`]. The default delegates to
    /// [`Self::begin_native_region`] (and its end to
    /// [`Self::end_blocking_region`]), so out-of-tree implementors and mocks
    /// count one ordinary region.
    fn begin_native_region_quiet(&mut self) -> (u64, u64) {
        self.begin_native_region();
        (u64::MAX, u64::MAX)
    }

    /// Leave a region opened by [`Self::begin_native_region_quiet`]; `sync`
    /// is what that call answered. Waits out a pause in progress, as
    /// [`Self::end_blocking_region`] does, and takes the full wake unless the
    /// region was quiet. Never pair it with any other `begin_*`, nor that
    /// entry with any other end.
    fn end_native_region_quiet(&mut self, sync: (u64, u64)) {
        let _ = sync;
        self.end_blocking_region();
    }

    /// HotSpot's `_thread_in_native` for an FFM downcall, without a root
    /// deposit (JIT round 16 wave 14, lane ffm3;
    /// `CRATONVM_FFM_DOWNCALL_IN_NATIVE_LAZY`): one release store of the
    /// thread's state. The thread stays a counted mutator and reports
    /// `RUNNABLE`; a collection that finds it in C claims it, scans the stack
    /// band `[band_lo, band_hi)` conservatively (with its interpreter frames
    /// and thread fields) and excuses it, and its
    /// [`Self::end_native_region_lazy`] waits until that pause is done.
    ///
    /// `band_lo` must lie below every frame the caller keeps live across the
    /// C call, with the caller's callee-saved registers spilled inside the
    /// band before this call; `band_hi` is the thread's stack top. `true`:
    /// entered, pair it with exactly one [`Self::end_native_region_lazy`].
    /// `false`: nothing done (the default; out-of-tree implementors and
    /// mocks, a target or heap the pause cannot claim on, a band it cannot
    /// scan) -- the caller keeps the thread counted or opens another region.
    fn begin_native_region_lazy(&mut self, band_lo: usize, band_hi: usize) -> bool {
        let _ = (band_lo, band_hi);
        false
    }

    /// [`Self::begin_native_region_lazy`] for a JNI native call that may hold
    /// local references in this thread's `JNI_LOCAL_FRAMES`. Unlike the FFM
    /// downcall path, this does not decline when local frames are non-empty;
    /// instead it marks `locals_empty = false` so a claiming pause uses the
    /// non-precise pinned claim and reads the published local-frame table.
    fn begin_native_region_lazy_with_locals(&mut self, band_lo: usize, band_hi: usize) -> bool {
        let _ = (band_lo, band_hi);
        false
    }

    /// Leave a region [`Self::begin_native_region_lazy`] entered: one
    /// compare-exchange, plus a wait while a pause that claimed the thread
    /// is still running. Never pair it with any other `begin_*`.
    fn end_native_region_lazy(&mut self) {}

    /// Run `wait` -- ONE OS-level wait that touches no Java object and holds
    /// no Java reference in its closure (a process wait: the process tables
    /// and the OS only) -- in HotSpot's `_thread_in_native` without a root
    /// deposit: the lazy in-native state of [`Self::begin_native_region_lazy`]
    /// over the caller's stack band, captured by the implementation itself
    /// (interpreter round i7 wave 139, lane L1b; item 2 of
    /// `i135-L1b-proposal-process-wait-leftovers-of-i102`,
    /// `CRATONVM_PROCESS_WAIT_LAZY_NATIVE`, performance, default OFF).
    /// `true`: `wait` ran inside the lazy state. `false`: nothing ran (the
    /// default; a target or heap the lazy state cannot serve) -- the caller
    /// opens its ordinary region and runs `wait` itself.
    fn run_in_native_wait_lazy(&mut self, wait: &mut dyn FnMut()) -> bool {
        let _ = wait;
        false
    }

    /// Run `wait` -- ONE OS-level wait (a sleep, a condvar wait) made INSIDE a
    /// blocking region, holding no Java reference in its closure -- so the
    /// collector can read the waiting thread's stack band without signalling
    /// it (JIT round 18 wave 10, lane ffmeintr, EINTR2-1;
    /// `CRATONVM_XT_HELPER_UNPARKED_READ`, opt-in). The default just calls
    /// `wait` (mocks, out-of-tree contexts, the switch off).
    fn run_blocked_wait(&mut self, wait: &mut dyn FnMut()) {
        wait()
    }

    /// One `Thread.sleep` wait of at most `max`, made INSIDE the sleep's
    /// blocking region, that an interrupt of this thread ends at once
    /// (interpreter round i3 wave 75, lane L6, `CRATONVM_SLEEP_NO_POLL`):
    /// `true` when the VM waited (for `max`, or less if the thread was
    /// interrupted -- the caller re-reads the clock and the flag either way);
    /// `false` when it did nothing (the switch off, a virtual thread, the
    /// default for mocks and out-of-tree contexts) and the caller sleeps its
    /// own slice through [`Self::run_blocked_wait`].
    fn sleep_wait_unpolled(&mut self, max: std::time::Duration) -> bool {
        let _ = max;
        false
    }

    /// T19.H1 — end a blocking region opened by `begin_blocking_region`.
    /// Re-syncs the thread with any GC that ran while it was blocked.
    fn end_blocking_region(&mut self) {}

    /// End a blocking region AND re-sync caller-held raw `Value` refs.
    ///
    /// A native poll loop captures its arguments as raw `Value`s before
    /// entering the region; a moving GC that completes while the thread is
    /// blocked relocates the referenced objects, and the thread-side
    /// re-sync (`end_blocking_region`) only repairs the *frames* — the
    /// native-local copies would keep their stale pre-GC addresses (the
    /// `ReferenceQueue.remove` stale-receiver writer). Pass those locals
    /// here so they are rewritten through the same accumulated GC fixup.
    ///
    /// The default impl ends the region without touching `refs` (matches
    /// VMs/tests whose collector never moves objects under natives).
    fn end_blocking_region_refs(&mut self, refs: &mut [Value]) {
        let _ = &refs;
        self.end_blocking_region();
    }

    // -- Park/Unpark (LockSupport) --

    /// Park the current thread (block until unparked or timeout).
    fn park(&mut self, timeout: Option<std::time::Duration>);

    /// `LockSupport.parkNanos` / `parkUntil` / `Unsafe.park` themselves: as
    /// [`Self::park`], and a timeout makes `Thread.getState()` report
    /// TIMED_WAITING, as HotSpot does. Plain `park` reports WAITING (natives
    /// park in slices on behalf of untimed waits).
    fn park_reporting_timeout(&mut self, timeout: Option<std::time::Duration>) {
        self.park(timeout)
    }

    /// An absolute Java-level park (`LockSupport.parkUntil`,
    /// `Unsafe.park(true, time)`): as [`Self::park_reporting_timeout`] with
    /// `timeout` the time left, and `until_ms` the caller's deadline (epoch
    /// milliseconds), which the `jdk.ThreadPark` event reports as `until`
    /// with `timeout = Long.MIN_VALUE`, as HotSpot does (interpreter round i3
    /// wave 79, lane L6).
    fn park_until_reporting(&mut self, timeout: Option<std::time::Duration>, until_ms: i64) {
        let _ = until_ms;
        self.park_reporting_timeout(timeout)
    }

    /// A NEGATIVE relative Java-level park (`Unsafe.park(false, time)` with
    /// `time < 0`): as [`Self::park_reporting_timeout`] (it returns at once,
    /// `timeout` is `Some(ZERO)`), and the `jdk.ThreadPark` event reports the
    /// caller's raw `time_ns` as `timeout`, as HotSpot 25's `Unsafe_Park`
    /// posts `(timeout = time, until = Long.MIN_VALUE)` -- a `Duration`
    /// cannot carry the sign (gci i23/pk, finding 6 of
    /// `gci-i22rv-i21-default-path-review-findings-FIXED-20261016`).
    fn park_relative_reporting_raw(&mut self, timeout: Option<std::time::Duration>, time_ns: i64) {
        let _ = time_ns;
        self.park_reporting_timeout(timeout)
    }

    /// The untimed Java-level `LockSupport.park()` / `park(Object)` stand-ins:
    /// exactly [`Self::park`] (WAITING, no `jdk.ThreadPark` event, as before),
    /// but the VM may know the block is a Java-level park whose object
    /// arguments the caller pinned (JIT round 21 lane refs, REFS21-6, the
    /// opt-in helper-window park credit). VM-internal slice waits keep
    /// calling [`Self::park`].
    fn park_java_untimed(&mut self, timeout: Option<std::time::Duration>) {
        self.park(timeout)
    }

    /// Unpark a thread identified by its Java Thread object.
    fn unpark(&self, thread_obj: ObjectRef);

    // -- Scoped Values (JEP 446, Java 25) --

    /// Look up a scoped value binding by key_id on the current thread's stack.
    fn get_scoped_value(&self, key_id: u64) -> Option<Value>;

    /// Push a scoped value binding onto the current thread's stack.
    fn push_scoped_value(&mut self, key_id: u64, value: Value);

    /// Round-9 GC fix: push a scoped value binding AND remember the
    /// ScopedValue key object so the GC keeps it live for the duration of
    /// the binding. Default impl forwards to the legacy
    /// `push_scoped_value` for backward compatibility — callers that have
    /// the key ObjectRef (e.g. `Carrier.run`) should call this overload
    /// instead so the key cannot be reclaimed while bindings exist.
    fn push_scoped_value_with_key(
        &mut self,
        key_id: u64,
        _key_ref: Option<ObjectRef>,
        value: Value,
    ) {
        self.push_scoped_value(key_id, value);
    }

    /// Pop the most recent scoped value binding from the current thread's stack.
    fn pop_scoped_value(&mut self);

    /// Return the current depth (number of entries) of the scoped value binding stack.
    fn scoped_value_depth(&self) -> usize;
}

pub trait NativeExceptionAccess: NativeHeapAccess {
    /// Capability boundary: Throwable and stack-trace capture.

    /// zs s5/nd5: when `payload` (a panic payload a native caught with
    /// `std::panic::catch_unwind`) is the VM's heap-exhaustion unwind (the
    /// one-attempt allocation doors' `NativeAllocOom`), the
    /// `OutOfMemoryError` failure it stands for; any other payload, `None`.
    /// For a native whose stack has a frame the unwinder cannot cross between
    /// the allocation and the VM's own handler (an FFM upcall entered through
    /// libffi's closure trampoline): it must stop the unwind itself. The
    /// default (mocks: no such unwind) answers `None`.
    fn native_alloc_oom_failure(
        &self,
        _payload: &(dyn std::any::Any + Send),
    ) -> Option<MethodCallFailed> {
        None
    }

    /// Capture the current Java call stack without retaining it. Used by
    /// StackWalker and caller-sensitive helpers. Frames are OUTERMOST-first.
    ///
    /// It took an ignored `i32` until 2026-09-24, a leftover of the removed
    /// hash-keyed trace registry.
    fn capture_stack_trace(&mut self) -> Vec<StackTraceEntry>;

    /// [`Self::capture_stack_trace`] for a `StackWalker` walk: an entry of an
    /// activation begun before a redefinition of its class (an old version
    /// of its method, obsolete or EMCP) has no line (`-1`), as HotSpot's
    /// `StackFrameInfo` of such a frame has none, while a `Throwable` of the
    /// same activation keeps the old body's line. Interpreter round i1 wave
    /// 39, lane L3. The default is the plain capture.
    fn capture_stack_walk_trace(&mut self) -> Vec<StackTraceEntry> {
        self.capture_stack_trace()
    }

    /// JIT round 19 wave 2 (lane throws2, WALK17-3 step 1;
    /// `CRATONVM_STACKWALK_LEAN_CAPTURE=1`, default OFF until measured): may
    /// the `StackWalker` natives capture with
    /// [`Self::capture_stack_walk_trace_deferred`] and resolve each entry's
    /// line only when it is written ([`Self::resolve_deferred_stack_walk_line`])?
    /// The default is no (the eager capture).
    fn stack_walk_lean_capture(&self) -> bool {
        false
    }

    /// [`Self::capture_stack_walk_trace`] with the line numbers DEFERRED: an
    /// entry whose line the eager capture would read from its method's
    /// `LineNumberTable` carries [`LINE_NUMBER_DEFERRED`] or
    /// [`LINE_NUMBER_DEFERRED_AT_BCI_ZERO`] instead, and every other field is
    /// the eager capture's. A caller must pass each entry through
    /// [`Self::resolve_deferred_stack_walk_line`] before any reader sees its
    /// line. The default is the eager capture (no deferred entry).
    fn capture_stack_walk_trace_deferred(&mut self) -> Vec<StackTraceEntry> {
        self.capture_stack_walk_trace()
    }

    /// Resolve one entry of [`Self::capture_stack_walk_trace_deferred`] in
    /// place: the line the eager capture would have given it. A no-op on an
    /// entry with no deferred line (the default resolves nothing, matching the
    /// default capture).
    fn resolve_deferred_stack_walk_line(&self, entry: &mut StackTraceEntry) {
        let _ = entry;
    }

    /// JIT round 21 wave 3, lane walk (WALK21-9 / WALK21-10): the INNERMOST
    /// `want` (or more) entries of [`Self::capture_stack_walk_trace`], same
    /// order (OUTERMOST-first) and same entries, lines included, without
    /// walking the rest of the stack -- or `None` when this context cannot
    /// prove the window is exactly that suffix (the stack is shorter than the
    /// window, a capture rule that inserts or splices frames could apply, a
    /// class was ever redefined) or has no such capture. The caller then takes
    /// the whole capture. The default answers `None`.
    fn capture_stack_walk_trace_tail(&mut self, want: usize) -> Option<Vec<StackTraceEntry>> {
        let _ = want;
        None
    }

    /// WALK21-10: the calling thread's stack-walk anchor now -- its
    /// interpreter frame count and its JIT entry chain length -- for a later
    /// [`Self::capture_stack_walk_trace_anchored`] from deeper in the same
    /// call. `None` (the default) when the context has no such capture.
    fn stack_walk_anchor(&self) -> Option<(usize, u32)> {
        None
    }

    /// WALK21-10: [`Self::capture_stack_walk_trace`] of the frames below
    /// `anchor` ([`Self::stack_walk_anchor`], taken earlier in a call that is
    /// still running): the interpreter frames under its count and the compiled
    /// activations of the chain entries under its length, with the capture's
    /// pair rules and without the reflective / served splices (the caller took
    /// the anchor only when the thread had none). `None` (the default) when
    /// the context has no such capture.
    fn capture_stack_walk_trace_anchored(
        &mut self,
        anchor: (usize, u32),
    ) -> Option<Vec<StackTraceEntry>> {
        let _ = anchor;
        None
    }

    /// The innermost `want` entries of [`Self::capture_stack_trace`] (same
    /// OUTERMOST-first order) and whether the returned vector is the WHOLE
    /// trace. For a reader that needs only the frames nearest the call
    /// (`Reflection.getCallerClass`): the VM answers without walking the rest
    /// of the stack. The entries' line numbers may be unresolved. The default
    /// returns the whole capture, which satisfies the contract.
    fn capture_stack_trace_tail(&mut self, want: usize) -> (Vec<StackTraceEntry>, bool) {
        let _ = want;
        (self.capture_stack_trace(), true)
    }

    /// For each of `entries`, is it a frame HotSpot's stack walk leaves out
    /// unless `StackWalker.Option.SHOW_HIDDEN_FRAMES` is set
    /// (`Method::is_hidden`: a method of a hidden class, or one annotated
    /// `@jdk.internal.vm.annotation.Hidden`)? Interpreter round i1 wave 35.
    /// The default (and `--compatible`) hides nothing.
    fn hidden_stack_entries(&self, entries: &[StackTraceEntry]) -> Vec<bool> {
        vec![false; entries.len()]
    }

    /// For each of `entries`, is its method HIDDEN as a property of the method
    /// (`Method::is_hidden`), whatever `-XX:+ShowHiddenFrames` says? The
    /// answer HotSpot gives the readers that do not list frames but skip them
    /// by kind: a `ClassFrameInfo`'s `MN_HIDDEN_MEMBER` bit (which
    /// `StackStreamFactory.CallerClassFinder` skips on), and the
    /// `getCallerClass` rules. [`Self::hidden_stack_entries`] is the LISTING
    /// answer, which `-XX:+ShowHiddenFrames` empties of every real method
    /// (interpreter round i4 wave 84); without the flag the two agree.
    /// Interpreter round i4 wave 92, lane L2. The default is the listing
    /// answer.
    fn hidden_member_entries(&self, entries: &[StackTraceEntry]) -> Vec<bool> {
        self.hidden_stack_entries(entries)
    }

    /// Does this VM run under `-XX:+ShowHiddenFrames` (`VmConfig::
    /// show_hidden_frames`)? HotSpot's `StackWalk::fill_in_frames` then skips
    /// no hidden frame in any walk mode (`!ShowHiddenFrames &&
    /// (skip_hidden_frames(mode) || get_caller_class(mode))`), as if every
    /// walker had `SHOW_HIDDEN_FRAMES`; `getCallerClass` still skips them, in
    /// Java, by their member bit ([`Self::hidden_member_entries`]).
    /// Interpreter round i4 wave 92, lane L2. The default answers `false`.
    fn show_hidden_frames_flag(&self) -> bool {
        false
    }

    /// This VM's `-XX:MaxJavaStackTraceDepth` (`VmConfig::
    /// max_java_stack_trace_depth`; `0` means unlimited): the most frames
    /// another thread's `Thread.getStackTrace()` lists, as HotSpot's
    /// `GetStackTraceHandshakeClosure` caps it (interpreter round i4 wave 84,
    /// lane L2). The default is HotSpot's 1024.
    fn max_java_stack_trace_depth(&self) -> usize {
        1024
    }

    /// For each of `entries`, is its method `@jdk.internal.reflect.CallerSensitive`
    /// (HotSpot's `Method::is_caller_sensitive`, honoured in privileged JDK
    /// classes only)? A `DROP_METHOD_INFO` walk's `ClassFrameInfo` carries it
    /// as `MemberName`'s `MN_CALLER_SENSITIVE`, which its `toString()` tags
    /// ` caller sensitive`. Interpreter round i2 wave 55, lane L3. The default
    /// answers `false` for every entry.
    fn caller_sensitive_stack_entries(&self, entries: &[StackTraceEntry]) -> Vec<bool> {
        vec![false; entries.len()]
    }

    /// Does this VM list the hidden JDK frames a native stands for in a
    /// `SHOW_HIDDEN_FRAMES` walk ([`Self::hidden_standin_frames`])? A per-VM
    /// switch (`CRATONVM_STANDIN_HIDDEN_INVOKE_FRAMES`), asked before the
    /// walker's options are read, so a VM that lists none reads no option for
    /// it. Interpreter round i1 wave 48, lane L3. The default lists none.
    fn lists_hidden_standin_frames(&self) -> bool {
        false
    }

    /// Does `StackWalker.getCallerClass()` answer by HotSpot's rule (JDK 25
    /// `StackStreamFactory.CallerClassFinder`: skip hidden frames, reflection
    /// frames and every `java.lang.invoke` class; `IllegalCallerException`
    /// when no caller frame is left) rather than the older name list? A
    /// per-VM switch (`CRATONVM_CALLER_CLASS_FINDER`). Interpreter round i1
    /// wave 50, lane L3. The default answers by the name list.
    fn caller_class_finder_rule(&self) -> bool {
        false
    }

    /// Does `jdk.internal.reflect.Reflection.getCallerClass()` answer by
    /// HotSpot's rule (JDK 25 `JVM_GetCallerClass` over the walkers' capture:
    /// the frame after the asking one that `Method::is_ignored_by_security_stack_walk`
    /// does not skip, `null` when none) rather than the bare frame list's
    /// second frame with its stream-plumbing skip? A per-VM switch
    /// (`CRATONVM_REFLECTION_CALLER_RULE`). Interpreter round i1 wave 51,
    /// lane L3. The default answers by the older rule.
    fn reflection_caller_class_rule(&self) -> bool {
        false
    }

    /// The class `Reflection.getCallerClass()` answers by
    /// [`Self::reflection_caller_class_rule`]'s rule, decided from the two
    /// innermost frames without the walkers' capture, or `None` when the VM
    /// cannot decide it that way (the switch off, a reflective or served
    /// `Thread.run` call in force on the thread, or a frame the security walk
    /// may skip): the caller then runs the whole rule. A per-VM switch
    /// (`CRATONVM_REFLECTION_CALLER_FAST`). Interpreter round i2 wave 60, lane
    /// L3: the walkers' capture per `Method.invoke` /
    /// `Constructor.newInstance` was the larger half of the JDK accessor's
    /// cost (`CRATONVM_JDK_REFLECTIVE_INVOKE`). The default never decides.
    fn reflection_caller_class_fast(&self) -> Option<ClassId> {
        None
    }

    /// Do the reflective door natives (`Method.invoke`, `Field.get`/`set`,
    /// `setAccessible`, `Class.getResource`, ...) find their caller by
    /// HotSpot's `JVM_GetCallerClass` rule over the walkers' capture
    /// (`stack_walker::reflective_door_caller_class_id`) rather than the name
    /// list over the bare frame window? A per-VM switch
    /// (`CRATONVM_REFLECTION_CS_CALLER_RULE`). Interpreter round i4 wave 94,
    /// lane L3 (stage 3 of the i49-L3 proposal). The default answers by the
    /// name list.
    fn reflection_cs_caller_rule(&self) -> bool {
        false
    }

    /// The lambda proxy SAM frames a caller-class question must see between
    /// `frames` (a capture's frames, INNERMOST first), as `(index, entries)`
    /// insertions before `frames[index]` (ascending): HotSpot's
    /// `JVM_GetCallerClass` does not skip a hidden proxy frame, so a method
    /// reference to a `@CallerSensitive` method (`Field::get`) makes the
    /// proxy the caller, and this VM's lambda dispatch pushes no proxy frame.
    /// `served(class, name)` names the caller-sensitive methods a NATIVE is
    /// serving right now (no frame of its own: the innermost frame then stands
    /// at the proxy's `invokeinterface`). A per-VM switch
    /// (`CRATONVM_CALLER_LAMBDA_PROXY_FRAMES`, default on). Interpreter round
    /// i4 wave 94, lane L3. The default inserts nothing.
    fn caller_lambda_proxy_insertions(
        &mut self,
        frames: &[StackTraceEntry],
        served: &dyn Fn(&str, &str) -> bool,
    ) -> Vec<(usize, Vec<StackTraceEntry>)> {
        let _ = (frames, served);
        Vec::new()
    }

    /// For a native serving a `@CallerSensitive` method `served` accepts:
    /// the lambda proxy whose SAM the innermost frame is calling, when exactly
    /// one known proxy of that SAM implements such a method -- that proxy is
    /// the caller ([`Self::caller_lambda_proxy_insertions`]'s second shape).
    /// Answers `(proxy, host)`: the proxy's synthetic class id (for the
    /// caller's NAME, `Host$$Lambda/0x..`) and its host class when known (a
    /// nestmate of the proxy in the same loader, package and module: the
    /// class access decisions ask). Interpreter round i4 wave 94, lane L3.
    /// The default answers `None`.
    fn served_lambda_proxy_caller(
        &mut self,
        served: &dyn Fn(&str, &str) -> bool,
    ) -> Option<(ClassId, Option<ClassId>)> {
        let _ = served;
        None
    }

    /// Do the served `Class.getMethod` / `getMethods` take JDK 25's public
    /// method walk (`Class.getMethodsRecursive` with
    /// `PublicMethods.MethodList.merge` / `getMostSpecific`, and
    /// `privateGetPublicMethods` with `PublicMethods.merge`) rather than their
    /// two older walks? A per-VM switch (`CRATONVM_PUBLIC_METHOD_JDK_WALK`).
    /// Interpreter round i4 wave 94, lane L3 (the i52-L3 one-walk proposal).
    /// The default keeps the older walks.
    fn public_method_jdk_walk(&self) -> bool {
        false
    }

    /// Does a JDK stack walk's `StackFrameInfo` leave its `StackTraceElement`
    /// to its first read (`StackTraceElement.of` -> `initStackTraceElement`),
    /// so that read can settle the frame against a redefinition of its class
    /// since the walk, as HotSpot builds the element then? A per-VM switch
    /// (`CRATONVM_SFI_LAZY_ELEMENT`). Interpreter round i2 wave 54, lane L3.
    /// The default pre-caches the element at the walk, as before.
    fn sfi_lazy_element(&self) -> bool {
        false
    }

    /// Does `--compatible`'s native `StackWalker.walk` / `forEach` hand out
    /// the JDK's own frame classes (`java.lang.StackFrameInfo`, or
    /// `ClassFrameInfo` for a `DROP_METHOD_INFO` walker), as `--real-jdk`'s
    /// does and HotSpot does, rather than the VM's carrier? A per-VM switch
    /// (`CRATONVM_COMPATIBLE_JDK_WALK_FRAMES`). Interpreter round i2 wave 55,
    /// lane L3. The default keeps the carrier.
    fn compatible_jdk_walk_frames(&self) -> bool {
        false
    }

    /// Does a native walk's lazy `StackFrameInfo` ([`Self::sfi_lazy_element`])
    /// keep, instead of a pre-built `StackTraceElement`, its entry in ONE
    /// `int[]` per walk (the walk's class-redefinition count, then each
    /// frame's line and no-file bit), from which its first read builds the
    /// element? A per-VM switch (`CRATONVM_SFI_WALK_LINES`). Interpreter round
    /// i2 wave 55, lane L3. The trait default pre-builds the element (the VM's
    /// switch is on by default).
    fn sfi_walk_lines(&self) -> bool {
        false
    }

    /// Do `--compatible`'s generic-signature natives build the JDK's own
    /// `sun.reflect.generics.reflectiveObjects.TypeVariableImpl` /
    /// `GenericArrayTypeImpl` for a type variable / generic array type, as
    /// `--real-jdk`'s do and HotSpot does, rather than the VM's carriers
    /// (instances of the `java.lang.reflect` interfaces)? A per-VM switch
    /// (`CRATONVM_COMPATIBLE_REFLECT_TYPE_IMPLS`). Interpreter round i2 wave
    /// 56, lane L3. The default keeps the carriers.
    fn compatible_reflect_type_impls(&self) -> bool {
        false
    }

    /// Is the VM's `StackWalker$StackFrame` carrier retired
    /// ([`NativeMethodRegistry::walk_carrier_retired`]): does a native walk
    /// always hand out the JDK's frame classes? Interpreter round i2 wave 56,
    /// lane L3. The default keeps the carrier.
    fn walk_carrier_retired(&self) -> bool {
        false
    }

    /// Does the JDK's own annotation parser build every `AnnotatedType`
    /// ([`NativeMethodRegistry::jdk_type_annotations`]), so that
    /// `Class.getRawAnnotations` answers the class file's bytes under
    /// `--compatible` too (`AnnotationType`'s `@Retention` lookup reads them)?
    /// Interpreter round i2 wave 57, lane L3. The trait default answers no.
    fn jdk_type_annotations(&self) -> bool {
        false
    }

    /// Does a reflective listing (`getDeclaredMethods0`,
    /// `getDeclaredConstructors0`, `getDeclaredFields0`) resolve every listed
    /// member's signature types through the holder's loader first, as
    /// HotSpot's `Reflection::new_method` / `new_constructor` / `new_field`
    /// do (a type the loader cannot find fails the listing with
    /// `NoClassDefFoundError`)? A per-VM switch
    /// (`CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES`). Interpreter round i2
    /// wave 54, lane L3. The default resolves nothing up front.
    fn reflection_loads_signature_types(&self) -> bool {
        false
    }

    /// [`NativeMethodRegistry::jdk_declaration_annotations`]: under
    /// `--compatible`, do the member mirrors carry their annotation bytes for
    /// the JDK's `AnnotationParser`? Interpreter round i2 wave 58, lane L3.
    /// The default answers no.
    fn jdk_declaration_annotations(&self) -> bool {
        false
    }

    /// A native about to return an unbuilt `RuntimeError` whose throwable
    /// HotSpot raises two or more JDK frames below the method the native
    /// serves notes that chain here, OUTERMOST first (the served method
    /// first), for the funnel's mark of the raise to carry
    /// ([`ServedRaiseChain`]). Only the next marked raise of the chain's
    /// throwable class takes it. `true` when it was noted: a native whose
    /// raise the one-hop rule would misplace without the chain builds its
    /// throwable itself otherwise. Interpreter round i2 wave 54, lane L3. The
    /// default notes nothing.
    fn note_served_raise_chain(&mut self, _chain: ServedRaiseChain) -> bool {
        false
    }

    /// A native serving a JDK method is about to BUILD (not return unbuilt)
    /// a throwable of `chain.throwable_class` that HotSpot raises `chain`'s
    /// hops below that method: mark the raise now, as the funnel marks an
    /// unbuilt error, so the capture of the throwable built next lists the
    /// chain's frames; [`Self::withdraw_served_raise`] after the build. `true`
    /// when marked. The `java.lang.Class` listing doors' missing signature
    /// type (item 3 of `interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007`,
    /// 2026-10-07), as `NativeClassAccess::initialize_class_served` marks a
    /// class initialization's failure. The default marks nothing.
    fn mark_served_raise(&mut self, _chain: ServedRaiseChain) -> bool {
        false
    }

    /// Withdraw a [`Self::mark_served_raise`] mark the build did not take.
    /// The default does nothing.
    fn withdraw_served_raise(&mut self) {}

    /// The hidden JDK frames HotSpot runs between the frames of `frames` (a
    /// walk's frames, INNERMOST first) where the VM ran a native instead: the
    /// method-handle frames of a `MethodHandle.invokeExact` / `invoke` call and
    /// of a `Method.invoke` call (`DirectMethodHandle$Holder.invokeStatic`, ...)
    /// that only a `StackWalker.Option.SHOW_HIDDEN_FRAMES` walk lists. Each
    /// item is `(index, entries)`: insert `entries` (INNERMOST first) before
    /// `frames[index]`; indices ascend. Asked only for a walk that shows
    /// hidden frames. May load the JDK classes the frames name. Interpreter
    /// round i1 wave 48, lane L3. The default lists none.
    fn hidden_standin_frames(
        &mut self,
        _frames: &[StackTraceEntry],
    ) -> Vec<(usize, Vec<StackTraceEntry>)> {
        Vec::new()
    }

    /// The walk frame of the method `name descriptor` of the spun
    /// `java.lang.invoke.LambdaForm$MH` class `class_id` (a class the JDK's
    /// `InvokerBytecodeGenerator` defined for a `LambdaForm`), standing at its
    /// one `MethodHandle.invokeBasic` call, as a real activation of it would
    /// be listed. `None` when the class is not such a class, has no such
    /// method, or the method does not make exactly one `invokeBasic` call.
    /// Interpreter round i6 wave 115, lane L3 (`CRATONVM_STANDIN_SPUN_IWA_FRAMES`,
    /// page `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`). Since
    /// interpreter round i7 wave 137 (lane L3) also a spun
    /// `LambdaForm$DMH` class (a customized direct handle's form), at its one
    /// `MethodHandle.linkTo*` call. The default answers `None`.
    fn spun_lambda_form_frame(
        &self,
        _class_id: ClassId,
        _name: &str,
        _descriptor: &str,
    ) -> Option<StackTraceEntry> {
        None
    }

    /// [`Self::spun_lambda_form_frame`] for a spun `LambdaForm$MH` method that
    /// makes SEVERAL `invokeBasic` calls: the frame at the `ordinal`-th one
    /// (0-based, code order), only when it makes exactly `count`. Interpreter
    /// round i7 wave 139, lane L3 (an adapter with bound filter handles). The
    /// default answers `None`.
    fn spun_lambda_form_frame_at(
        &self,
        _class_id: ClassId,
        _name: &str,
        _descriptor: &str,
        _ordinal: usize,
        _count: usize,
    ) -> Option<StackTraceEntry> {
        None
    }

    /// The type descriptor of the `invokevirtual MethodHandle.invokeExact`
    /// the walk frame `caller` stands at (its class, method and bci, read
    /// from the class bytes), or `None` when it stands at no such call.
    /// Interpreter round i6 wave 121, lane L3 (`CRATONVM_STANDIN_SPUN_LINKER_FRAMES`,
    /// page `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`). The
    /// default answers `None`.
    fn method_handle_invoke_exact_site(&self, _caller: &StackTraceEntry) -> Option<String> {
        None
    }

    /// [`Self::method_handle_invoke_exact_site`] for a generic
    /// `invokevirtual MethodHandle.invoke` call site. Interpreter round i7
    /// wave 142, lane L3. The default answers `None`.
    fn method_handle_generic_invoke_site(&self, _caller: &StackTraceEntry) -> Option<String> {
        None
    }

    /// Record that the current thread enters the reflective call `kind`
    /// (`Method.invoke` / `Constructor.newInstance`), which the VM serves as a
    /// native and which therefore pushes none of the JDK frames HotSpot shows
    /// for it (`Method.invoke` and `DirectMethodHandleAccessor.invoke`, and the
    /// three constructor frames). A stack capture made inside the call lists
    /// them between the caller and the target, as HotSpot does (interpreter
    /// round i1 wave 43, lane L3). Since wave 49 also the
    /// `AccessController.doPrivileged` bridges
    /// ([`ReflectiveCallKind::is_privileged`]), whose one JDK frame every
    /// capture lists. Returns the mark to hand to
    /// [`Self::leave_reflective_call`] on every exit path. The default records
    /// nothing.
    fn enter_reflective_call(&mut self, _kind: ReflectiveCallKind) -> usize {
        0
    }

    /// End the reflective call [`Self::enter_reflective_call`] returned `mark`
    /// for (and any it left open inside it).
    fn leave_reflective_call(&mut self, _mark: usize) {}

    /// Interpreter round i7 wave 136, lane L4 (page
    /// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`, residuals 1
    /// and 2): note on the current thread's innermost reflective row -- only
    /// when it is a served `invokeWithArguments`
    /// ([`ReflectiveCallKind::is_invoke_with_arguments`]) that has pushed
    /// nothing yet and noted no adapter -- the native pin slot
    /// ([`Self::pin_native_root`]) holding the adapter the served call is about
    /// to run (`asType(genericMethodType(n))`: the `asTypeCache` entry, or the
    /// handle itself). `noted == false` withdraws the note of that same slot
    /// (and nothing else); the caller withdraws it BEFORE it unpins the slot.
    /// Rooted by the pin, so the walk reads the adapter where it is now. The
    /// default records nothing.
    fn note_invoke_with_arguments_adapter(&mut self, _pin: usize, _noted: bool) {}

    /// The adapters [`Self::note_invoke_with_arguments_adapter`] noted on the
    /// current thread's served `invokeWithArguments` rows, OUTERMOST first,
    /// one entry per such row (`None`: that row has no adapter noted: the
    /// served lanes declined, `CRATONVM_MH_IWA_SERVED_CUSTOMIZATION=0`, or a
    /// mock VM). For the thread's own walks (`native_call_stack_walk_of`,
    /// `walk_frames_screened`): the walk takes `adapter.form.vmentry`
    /// (customized or not) instead of rebuilding the adapter from the frame's
    /// target. A row a capture does not list (nothing pushed above it) still
    /// has its entry here. The default answers none.
    fn invoke_with_arguments_adapters(&self) -> Vec<Option<ObjectRef>> {
        Vec::new()
    }

    /// Interpreter round i3 wave 79, lane L3: a reflective door about to
    /// re-enter Java (`Method.invoke`, `Constructor.newInstance`) re-anchors
    /// the thread's compiled-recursion stack budget, so its own native frames
    /// are not charged to the JIT's `-Xss` analogue. Only a call already near
    /// the budget's end moves it; a shallow one is a compare. `stack_bounds`:
    /// this OS thread's `(low, high)` native stack, as the caller caches it
    /// (`None`: unknown). Returns the value to hand to
    /// [`Self::restore_compiled_stack_budget`] on every exit path. The default
    /// (no JIT) does nothing.
    fn rebase_compiled_stack_budget(&mut self, _stack_bounds: Option<(usize, usize)>) -> usize {
        usize::MAX
    }

    /// Undo [`Self::rebase_compiled_stack_budget`] with the value it returned.
    fn restore_compiled_stack_budget(&mut self, _saved: usize) {}

    /// Round 15 wave 3 (lane trace4; TR2W2-2): the served
    /// `java.lang.Thread.run()` is about to run its task (`task.run()`), a
    /// call HotSpot shows as a `java.lang.Thread.run` frame right below the
    /// first frame the task pushes. A stack capture made inside the call lists
    /// that frame there. Returns the mark to hand to
    /// [`Self::leave_served_thread_run`] on every exit path. The default
    /// records nothing.
    fn enter_served_thread_run(&mut self) -> usize {
        0
    }

    /// End the served `Thread.run` call [`Self::enter_served_thread_run`]
    /// returned `mark` for (and any left open inside it).
    fn leave_served_thread_run(&mut self, _mark: usize) {}

    /// [`Self::leave_served_thread_run`] for a call whose task returned a
    /// continuation yield: the task's frames come back at the remount without
    /// the call, so the thread's records no longer list every `Thread.run` it
    /// is inside (round 15 wave 6, lane trace6). The default is the plain
    /// leave.
    fn leave_served_thread_run_yielded(&mut self, mark: usize) {
        self.leave_served_thread_run(mark)
    }

    /// Note on the innermost reflective call this thread is inside which of
    /// its argument checks failed, right before the native builds the
    /// check's throwable by its real constructor inside the call: a capture
    /// of that throwable then lists the JDK frames HotSpot lists for the
    /// check (the accessor's `checkArgumentCount` / `checkReceiver`, its
    /// `new IllegalArgumentException` of the arm, or `Method.invoke`'s own
    /// access check). Interpreter round i1 wave 46, lane L3. The default
    /// notes nothing; outside a reflective call it is a no-op.
    fn note_reflective_check(&mut self, _check: ReflectiveCheck) {}

    /// Note on the innermost reflective call this thread is inside the
    /// throwable its target threw, right before the native wraps it in an
    /// `InvocationTargetException`: a capture of the wrapper lists the
    /// accessor at the `catch` arm HotSpot's accessor takes for that class
    /// (`NullPointerException`, `ClassCastException`, anything else). The
    /// capture reads it from the wrapper's constructor frame when one exists;
    /// this is for a wrapper built without one (a stub `<init>`, `--compatible`).
    /// Interpreter round i1 wave 48, lane L3. The default notes nothing;
    /// outside a reflective call it is a no-op.
    fn note_reflective_raise_target(&mut self, _target: ObjectRef) {}

    /// Note on the innermost reflective call this thread is inside the
    /// static method `(class_id, name, descriptor)` its by-name door is about
    /// to enter (`Some`), or withdraw the note right after the door returns
    /// (`None`): a throwable a callee builds while a native serves that
    /// method (the served loader's `ClassNotFoundException` under the served
    /// `Class.forName(String, boolean, ClassLoader)`) lists the method's JDK
    /// frames above the call's. Interpreter round i2 wave 62, lane L3. The
    /// default notes nothing; outside a reflective call it is a no-op. Since
    /// wave 64 the VM keeps the note on the thread's door record (the one
    /// every static JDK door writes), the record outside the call saved on
    /// the call and restored by the `None`: every `Some` must be followed by
    /// a `None` before the call is left.
    fn note_reflective_target(&mut self, _target: Option<(ClassId, &str, &str)>) {}

    /// HotSpot's message for a `checkcast` of an instance of `from` to `to`
    /// that fails (`class A cannot be cast to class B (A is in ... of loader
    /// ...; ...)`, `SharedRuntime::generate_class_cast_message`), for a
    /// `ClassCastException` a native raises where HotSpot's JDK code runs a
    /// checkcast. `from_display` is the instance's `Class.getName()`: for an
    /// ARRAY the heap reports the component's class id, so the array's own
    /// name (`[Ljava.lang.Integer;`, `[I`) comes from the caller and `from`
    /// is only a hint. Interpreter round i1 wave 47, lane L3. The default, and
    /// a class the VM cannot name the module and loader of, is `None`.
    fn class_cast_message(
        &self,
        _from: ClassId,
        _from_display: &str,
        _to: ClassId,
    ) -> Option<String> {
        None
    }

    /// Capture and retain a stack trace for `Throwable.fillInStackTrace`, and
    /// return its depth (the `Throwable.depth` the caller records).
    ///
    /// Only the depth comes back: the VM retains the frames in a compact form
    /// and builds `StackTraceEntry`s only when Java reads the trace
    /// ([`Self::get_throwable_stack_trace`] and siblings), so handing the
    /// entries back here would pay for exactly the per-frame work the compact
    /// form avoids. The default keeps lightweight/mock contexts
    /// source-compatible; the VM implementation overrides it so retained frames
    /// are owned by the VM, rather than by the Java thread that happened to
    /// construct the throwable.
    fn capture_throwable_stack_trace(&mut self, _throwable: ObjectRef) -> usize {
        self.capture_stack_trace().len()
    }

    /// Return the VM-local cached `StackTraceElement` class id when available.
    /// A default `None` keeps lightweight mock contexts source-compatible.
    fn cached_stack_trace_element_class_id(&self) -> Option<ClassId> {
        None
    }

    /// Record a resolved `StackTraceElement` id in VM-local state. The default
    /// is intentionally a no-op for contexts that do not own a VM.
    fn cache_stack_trace_element_class_id(&self, _class_id: ClassId) {}

    /// Return `Throwable.SUPPRESSED_SENTINEL` through VM-local cached metadata
    /// when available. The object itself is never cached here: it is movable
    /// heap state and must continue to be read from the static field.
    fn throwable_suppressed_sentinel(&self) -> Option<Value> {
        None
    }

    /// Retrieve the stack trace captured for `throwable` (or its `backtrace`
    /// marker object) as an owned snapshot.
    ///
    /// An owned value deliberately avoids lending a reference through a
    /// VM-shared lock while another Java thread may replace or discard a trace.
    /// The VM keeps retained traces keyed by address, so its override is one
    /// shard probe. There is deliberately no identity-hash-keyed variant any
    /// more: an identity hash is not an identity (two live throwables can
    /// share one), and the old hash-keyed door scanned every retained trace
    /// and MINTED a hash on each one it compared. The default (no trace) keeps
    /// lightweight/mock contexts source-compatible.
    fn get_throwable_stack_trace(&self, _throwable: ObjectRef) -> Option<Vec<StackTraceEntry>> {
        None
    }

    /// Replace the trace retained for `throwable` with `entries`
    /// (OUTERMOST-first, as [`Self::get_throwable_stack_trace`] answers it),
    /// stored under `throwable`'s current address. `false` when this context
    /// keeps no traces (the default). Known gap
    /// `gap-showhiddenframes-throwable-lacks-spun-forms`: the capture's
    /// `-XX:+ShowHiddenFrames` step (`lang_misc::add_spun_frames_to_capture`)
    /// stores the trace with the spun `LambdaForm` frames it asked the JDK for.
    fn replace_throwable_stack_trace(
        &mut self,
        _throwable: ObjectRef,
        _entries: Vec<StackTraceEntry>,
    ) -> bool {
        false
    }

    /// The length of [`Self::get_throwable_stack_trace`]'s answer, without
    /// copying the trace or resolving its line numbers.
    ///
    /// `Throwable.getStackTraceDepth()` needs only this, and a
    /// `getOurStackTrace()` that loops `getStackTraceElement(i)` asks it once
    /// and then asks for each element — through the whole-trace door that was
    /// a copy of up to `MaxJavaStackTraceDepth` entries plus a line resolution
    /// of every one of them, per call: quadratic in the depth.
    fn throwable_stack_trace_depth(&self, throwable: ObjectRef) -> Option<usize> {
        self.get_throwable_stack_trace(throwable).map(|t| t.len())
    }

    /// Interpreter round i3 wave 82, lane L2
    /// (`CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1`, default OFF): does
    /// [`Self::capture_throwable_stack_trace`] store `Throwable.backtrace`
    /// itself (a throwable-owned holder, or the registry's self-reference
    /// marker)? Then the fill that follows writes only `depth`. The default
    /// `false` keeps the fill's own `backtrace = this` store.
    fn throwable_backtrace_in_object(&self) -> bool {
        false
    }

    /// Entry `index` of [`Self::get_throwable_stack_trace`]'s answer
    /// (OUTERMOST-first indexing, as the stored trace is), line resolved, or
    /// `None` when there is no trace or no such entry. The VM override copies
    /// and resolves that one entry only.
    fn throwable_stack_trace_entry(
        &self,
        throwable: ObjectRef,
        index: usize,
    ) -> Option<StackTraceEntry> {
        self.get_throwable_stack_trace(throwable)
            .and_then(|mut t| (index < t.len()).then(|| t.swap_remove(index)))
    }

    /// The JEP 358 message of a `NullPointerException` whose message the VM
    /// deferred to `getExtendedNPEMessage()`, as HotSpot computes it there:
    /// `Some(answer)` when `throwable` is such an NPE (`answer` `None` for "no
    /// message"), `None` when it is not and the native answers as before.
    /// Round 15 wave 7, lane throw2 (TH1-5). The default defers nothing.
    fn deferred_npe_message(&mut self, _throwable: ObjectRef) -> Option<Option<String>> {
        None
    }

    /// The JEP 358 message HotSpot prints when the JDK method `class.method
    /// descriptor` (internal class name) is entered with a null `param`
    /// (0-based, `this` not counted) and reaches its first dereference of it,
    /// read from that method's own bytecode and `LocalVariableTable` -- or
    /// `None` when the dereference is not on the method's straight-line
    /// prefix (a native that refuses a null argument then keeps its
    /// transcribed line). JIT round 16, lane compatdrain, proposal C5W4-1.
    /// The default answers nothing.
    fn null_arg_message(
        &self,
        _class: &str,
        _method: &str,
        _descriptor: &str,
        _param: usize,
    ) -> Option<String> {
        None
    }

    /// The exact `ClassId` each live frame is currently executing in,
    /// innermost (most recent call) first.
    ///
    /// Unlike `capture_stack_trace`'s `StackTraceEntry`s (which carry only a
    /// display `class_name: Arc<str>`, for `Throwable`/`StackWalker` output),
    /// this exposes each frame's precise, already-resolved `ClassId` — no
    /// re-resolution by name needed. That distinction matters whenever two
    /// *different* classes share one name (a common shape for custom
    /// classloaders, e.g. Hibernate bytecode-enhancement's per-test-class
    /// `EnhancingClassLoader`, or a class reloaded under a fresh loader
    /// between JUnit tests sharing one process): re-resolving a frame's name
    /// via `class_id_by_name` collapses to whichever definition the global
    /// class table associates with that name (typically the first one ever
    /// registered), which is not necessarily the one actually executing on
    /// that frame. Used by `latest_user_defined_loader_class` to correctly
    /// mirror `jdk.internal.misc.VM.latestUserDefinedLoader()`.
    fn frame_class_ids(&self) -> Vec<ClassId> {
        Vec::new()
    }

    /// The innermost `want` entries of [`Self::frame_class_ids`] (same
    /// INNERMOST-first order) and whether the vector is the WHOLE answer. For a
    /// reader that stops at the first frame it accepts
    /// ([`find_in_caller_frames`]); the VM answers without walking the rest of
    /// the stack. The default returns the whole answer, which satisfies the
    /// contract.
    fn frame_class_ids_tail(&self, want: usize) -> (Vec<ClassId>, bool) {
        let _ = want;
        (self.frame_class_ids(), true)
    }

    /// Is the innermost entry of [`Self::frame_class_ids_tail`] the class of
    /// the code actually running there? `false` when a compiled activation
    /// that inlined other methods may have reported its own (outer) class in
    /// place of an inlined callee's. The default `false` is the conservative
    /// answer: a reader that would act on a boot-loader caller falls back.
    fn innermost_frame_class_exact(&self) -> bool {
        false
    }

    /// `Object.clone()` of a throwable (interpreter round i3; page
    /// `i82-L2-a-cloned-throwable-does-not-keep-its-trace`): `clone` has just
    /// copied every field of `original`, so its `backtrace` names the
    /// ORIGINAL's retained trace. HotSpot's clone holds the original's
    /// backtrace object, which a later `fillInStackTrace()` on the original
    /// does not touch; the VM gives the clone its own copy of the trace. The
    /// default does nothing.
    fn clone_throwable_backtrace(&mut self, original: ObjectRef, clone: ObjectRef) {
        let _ = (original, clone);
    }
}

/// Walk the caller frames of the current Java stack innermost-first, asking `f`
/// about each until it answers [`std::ops::ControlFlow::Break`], without
/// materialising frames beyond the ones `f` looks at (the window grows until
/// `f` breaks or the stack ends). `None` when `f` never breaks. The order and
/// the frames are exactly `for id in ctx.frame_class_ids()`'s.
pub fn find_in_caller_frames<R>(
    ctx: &dyn NativeContext,
    mut f: impl FnMut(ClassId) -> std::ops::ControlFlow<R>,
) -> Option<R> {
    let mut want = 4usize;
    let mut seen = 0usize;
    loop {
        let (ids, whole) = ctx.frame_class_ids_tail(want);
        for &id in ids.iter().skip(seen) {
            if let std::ops::ControlFlow::Break(r) = f(id) {
                return Some(r);
            }
        }
        if whole {
            return None;
        }
        seen = ids.len();
        want = want.saturating_mul(4);
    }
}

